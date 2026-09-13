//! Rumble videos, embeds and live streams, through the embed data the player loads: the
//! video page names its embed, and the embed's JSON lists the MP4 files by height, the
//! HLS stream of a broadcast, the subtitles, the author and the time. Channel and user
//! pages list their videos when the site renders the listing into the page.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use scraper::Selector;
use serde_json::Value;
use url::Url;

use super::page::{Page, ld_objects_of_type, leading_json};
use super::web::parse_iso_duration;
use super::{
    ClipRange, MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionCheck, SessionSupport, SubtitleFormat, SubtitleTrack, Variant, VariantKind,
    clean_title, fetch, parse_time_stamp, timestamp_hint,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "rumble";
const SITE: &str = "https://rumble.com/";
const EMBED_API: &str = "https://rumble.com/embedJS/u3/";
/// The cookie a logged-in rumble.com session carries.
const SESSION_COOKIE: &str = "u_s";
const MAX_PAGES: usize = 5;

static RE_VIDEO_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^v[a-z0-9]{4,}(?:-|\.html$)").unwrap());
static RE_EMBED_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^v[a-z0-9]{4,}$").unwrap());

/// What a link names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A video page such as `/v6rrcbh-channel-update-vid.html`.
    Video(String),
    /// An embed, by the player's own id.
    Embed(String),
    Channel(String),
    User(String),
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !(host == "rumble.com" || host.ends_with(".rumble.com")) {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    match segments.as_slice() {
        ["embed", id, ..] if RE_EMBED_ID.is_match(id) => Some(Link::Embed(id.to_string())),
        ["embedJS", ..] => url
            .query_pairs()
            .find(|(k, _)| k == "v")
            .map(|(_, v)| v.into_owned())
            .filter(|v| RE_EMBED_ID.is_match(v))
            .map(Link::Embed),
        ["c", channel, ..] if !channel.is_empty() => Some(Link::Channel(channel.to_string())),
        ["user", user, ..] if !user.is_empty() => Some(Link::User(user.to_string())),
        [page] if RE_VIDEO_PATH.is_match(page) => Some(Link::Video(page.to_string())),
        _ => None,
    }
}

/// The embed id a video page names.
pub fn embed_id_in(page: &Page) -> Option<String> {
    let from_ld = ld_objects_of_type(&page.ld_json(), "VideoObject")
        .into_iter()
        .find_map(|v| v["embedUrl"].as_str().map(String::from));
    let embed_url = from_ld
        .or_else(|| page.meta("og:video").or_else(|| page.meta("og:video:url")))
        .or_else(|| page.between("\"embedUrl\":\"", "\"").map(|s| s.replace("\\/", "/")))?;
    let url = Url::parse(&embed_url).ok()?;
    match parse_link(&url) {
        Some(Link::Embed(id)) => Some(id),
        _ => None,
    }
}

/// What a video page says about its video. Read whole before the embed is fetched, so the
/// page's document, which lives on one thread, is gone before the future crosses threads.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageDetails {
    pub title: Option<String>,
    pub description: Option<String>,
    pub duration: Option<Duration>,
}

impl PageDetails {
    pub fn of(page: &Page) -> Self {
        let ld = page.ld_json();
        let video = ld_objects_of_type(&ld, "VideoObject");
        Self {
            title: page.title(),
            description: video
                .iter()
                .find_map(|v| v["description"].as_str().and_then(clean_title))
                .or_else(|| page.meta("description").and_then(|d| clean_title(&d))),
            duration: video
                .iter()
                .find_map(|v| v["duration"].as_str().and_then(parse_iso_duration)),
        }
    }
}

/// The player's JSON as the embed page inlines it.
pub fn embed_json_in(html: &str) -> Option<Value> {
    let at = html.find("\"ua\":")?;
    let start = html[..at].rfind('{')?;
    // Walk back to the object that holds `ua`, which starts the player's data.
    let mut depth = 0i32;
    let mut begin = None;
    for (index, ch) in html[..at].char_indices().rev() {
        match ch {
            '}' => depth += 1,
            '{' => {
                if depth == 0 {
                    begin = Some(index);
                    break;
                }
                depth -= 1;
            }
            _ => {}
        }
    }
    let begin = begin.unwrap_or(start);
    leading_json(&html[begin..]).map(|(v, _)| v).filter(|v| v["ua"].is_object() || v["u"].is_object())
}

pub struct RumbleResolver {
    http: Http,
}

impl RumbleResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    fn logged_in(&self) -> bool {
        self.http.jar(PLATFORM).get(SESSION_COOKIE).is_some()
    }

    /// The player's data for an embed: from the embed API, which wants the cookies a
    /// visit to the site hands out, and from the embed page inlined otherwise.
    async fn embed(&self, id: &str, referer: &Url, origin: &Url) -> Result<Value, ResolveError> {
        let mut api = Url::parse(EMBED_API).expect("valid");
        api.query_pairs_mut()
            .append_pair("request", "video")
            .append_pair("ver", "2")
            .append_pair("v", id);
        let headers = [
            ("referer".to_string(), referer.to_string()),
            ("accept".to_string(), "*/*".to_string()),
        ];
        let fetched = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &headers, MAX_PAGE).await?;
        if fetched.status.is_success()
            && let Ok(value) = serde_json::from_slice::<Value>(&fetched.body)
            && value.is_object()
        {
            return Ok(value);
        }
        if fetched.status.as_u16() == 429 {
            return Err(ResolveError::RateLimited(origin.clone()));
        }
        let embed_page = Url::parse(&format!("{SITE}embed/{id}/")).expect("valid");
        let fetched = fetch(&self.http, &embed_page, PLATFORM, BROWSER_UA, &headers, MAX_PAGE).await?;
        match fetched.status.as_u16() {
            200..=299 => {}
            404 | 410 => return Err(ResolveError::NotFound(origin.clone())),
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the embed answered HTTP {status}"),
                ));
            }
        }
        let html = fetched.text();
        if html.contains("Just a moment") && html.contains("challenge") {
            return Err(ResolveError::unavailable(
                origin,
                "the site's protection asks this client to solve a browser challenge",
            ));
        }
        embed_json_in(&html)
            .ok_or_else(|| ResolveError::malformed(origin, "the embed page carries no player data"))
    }

    async fn video_page(&self, path: &str, origin: &Url) -> Result<(Page, Url), ResolveError> {
        let page_url = Url::parse(&format!("{SITE}{path}")).expect("valid");
        let fetched = fetch(&self.http, &page_url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        match fetched.status.as_u16() {
            200..=299 => {}
            404 | 410 => return Err(ResolveError::NotFound(origin.clone())),
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the page answered HTTP {status}"),
                ));
            }
        }
        let html = fetched.text();
        if html.contains("Just a moment") && html.contains("challenge") {
            return Err(ResolveError::unavailable(
                origin,
                "the site's protection asks this client to solve a browser challenge",
            ));
        }
        let final_url = fetched.url.clone();
        Ok((Page::parse(&html, &final_url), final_url))
    }

    async fn video(&self, embed_id: &str, page: Option<&PageDetails>, page_url: &Url, origin: &Url) -> Result<Resolution, ResolveError> {
        let data = self.embed(embed_id, page_url, origin).await?;
        let live = matches!(data["live"].as_i64(), Some(1) | Some(2)) || data["live"].as_bool() == Some(true);
        let duration = data["duration"]
            .as_f64()
            .filter(|d| *d > 0.0)
            .map(Duration::from_secs_f64)
            .or_else(|| page.and_then(|p| p.duration));
        let mut variants = variants_of(&data, duration, live);
        if variants.is_empty() {
            if let Some(hls) = data["u"]["hls"]["url"]
                .as_str()
                .or_else(|| data["ua"]["hls"]["auto"]["url"].as_str())
                .and_then(|u| Url::parse(u).ok())
            {
                let expanded = super::hls::expand(&self.http, &hls, PLATFORM, BROWSER_UA, &[]).await?;
                variants = expanded.variants;
                for v in &mut variants {
                    v.live |= live;
                }
            }
        }
        if variants.is_empty() {
            return Err(if live {
                ResolveError::unavailable(origin, "the broadcast has not started")
            } else if data["restrict"].as_i64().is_some_and(|r| r != 0) {
                ResolveError::unavailable(origin, "the video is restricted")
            } else {
                ResolveError::NotFound(origin.clone())
            });
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(embed_id.to_string());
        resolved.title = data["title"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| page.and_then(|p| p.title.clone()));
        resolved.description = page.and_then(|p| p.description.clone());
        resolved.uploader = data["author"]["name"].as_str().and_then(clean_title);
        resolved.uploader_url = data["author"]["url"].as_str().and_then(|u| Url::parse(u).ok());
        resolved.uploaded_at = data["pubDate"]
            .as_str()
            .and_then(|t| t.parse::<Timestamp>().ok());
        resolved.duration = duration;
        resolved.thumbnail = data["i"].as_str().and_then(|u| Url::parse(u).ok());
        resolved.webpage_url = Some(page_url.clone());
        resolved.live = live;
        resolved.clip = timestamp_hint(origin).map(|start| ClipRange { start, end: None });
        resolved.subtitles = data["cc"]
            .as_object()
            .into_iter()
            .flat_map(|m| m.iter())
            .filter_map(|(language, track)| {
                let url = track["path"].as_str().and_then(|u| Url::parse(u).ok())?;
                Some(SubtitleTrack {
                    url,
                    auto: language.ends_with("-auto") || track["language"].as_str().is_some_and(|l| l.contains("auto")),
                    language: language.trim_end_matches("-auto").to_string(),
                    name: track["language"].as_str().map(String::from),
                    format: SubtitleFormat::Vtt,
                    headers: Vec::new(),
                })
            })
            .collect();
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    /// A channel's or user's videos, from the listing the page renders.
    async fn listing(&self, base: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let mut entries: Vec<PlaylistEntry> = Vec::new();
        let mut title = None;
        for page_number in 1..=MAX_PAGES {
            let mut url = Url::parse(&format!("{SITE}{base}/videos")).expect("valid");
            if page_number > 1 {
                url.query_pairs_mut().append_pair("page", &page_number.to_string());
            }
            let fetched = fetch(&self.http, &url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
            match fetched.status.as_u16() {
                200..=299 => {}
                404 | 410 if page_number == 1 => return Err(ResolveError::NotFound(origin.clone())),
                404 | 410 => break,
                429 => return Err(ResolveError::RateLimited(origin.clone())),
                status => {
                    return Err(ResolveError::unavailable(
                        origin,
                        format!("the page answered HTTP {status}"),
                    ));
                }
            }
            let html = fetched.text();
            let page = Page::parse(&html, &url);
            if title.is_none() {
                title = page
                    .document()
                    .select(&Selector::parse("h1").expect("valid"))
                    .next()
                    .and_then(|h| clean_title(&h.text().collect::<String>()))
                    .or_else(|| page.title());
            }
            let found = listed_videos(&page);
            if found.is_empty() {
                if page_number == 1 {
                    return Err(ResolveError::unavailable(
                        origin,
                        "the page lists its videos only to browsers running scripts; the videos' own links resolve",
                    ));
                }
                break;
            }
            let before = entries.len();
            for entry in found {
                if !entries.iter().any(|e| e.url == entry.url) {
                    entries.push(entry);
                }
            }
            if entries.len() == before {
                break;
            }
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.into(),
            id: Some(base.to_string()),
            title,
            total: Some(entries.len()),
            entries,
        }))
    }
}

/// The videos a listing page shows, in the markup the site has used for its listings.
pub fn listed_videos(page: &Page) -> Vec<PlaylistEntry> {
    let mut out: Vec<PlaylistEntry> = Vec::new();
    let items = Selector::parse(
        ".video-listing-entry, .videostream, .thumbnail__grid--item, .video-item, li.video-listing-entry",
    )
    .expect("valid");
    let link_selector = Selector::parse(
        "a.videostream__link, a.video-item--a, a.thumbnail__link, a[href*='.html']",
    )
    .expect("valid");
    let title_selector = Selector::parse(
        ".videostream__title, .thumbnail__title, .video-item--title, h3",
    )
    .expect("valid");
    let duration_selector = Selector::parse(
        ".videostream__status--duration, .video-item--duration, .thumbnail__duration, [data-value]",
    )
    .expect("valid");
    for item in page.document().select(&items) {
        let Some(link) = item
            .select(&link_selector)
            .find_map(|a| a.value().attr("href"))
            .and_then(|href| page.url().join(href).ok())
        else {
            continue;
        };
        if !matches!(parse_link(&link), Some(Link::Video(_))) {
            continue;
        }
        let mut clean = link.clone();
        clean.set_query(None);
        if out.iter().any(|e| e.url == clean) {
            continue;
        }
        let title = item
            .select(&title_selector)
            .next()
            .and_then(|t| {
                t.value()
                    .attr("title")
                    .map(String::from)
                    .or_else(|| Some(t.text().collect::<String>()))
            })
            .and_then(|t| clean_title(&t));
        let duration = item.select(&duration_selector).next().and_then(|d| {
            d.value()
                .attr("data-value")
                .map(String::from)
                .or_else(|| Some(d.text().collect::<String>()))
                .and_then(|t| parse_time_stamp(t.trim()))
        });
        out.push(PlaylistEntry {
            url: clean,
            title,
            duration,
        });
    }
    out
}

/// The variants the player's data lists: MP4 files by height, WebM files, and the HLS
/// stream of a broadcast.
pub fn variants_of(data: &Value, duration: Option<Duration>, live: bool) -> Vec<Variant> {
    let headers = vec![("referer".to_string(), SITE.to_string())];
    let mut variants: Vec<Variant> = Vec::new();
    let ua = &data["ua"];
    for (format, container, codec) in [
        ("mp4", Container::Mp4, VideoCodec::H264),
        ("webm", Container::Webm, VideoCodec::Vp9),
    ] {
        let Some(qualities) = ua[format].as_object() else {
            continue;
        };
        for (quality, entry) in qualities {
            let Some(url) = entry["url"].as_str().and_then(|u| Url::parse(u).ok()) else {
                continue;
            };
            if variants.iter().any(|v| v.url == url) {
                continue;
            }
            let meta = &entry["meta"];
            let mut v = Variant::new(url, VariantKind::File);
            v.container = Some(container.clone());
            v.video = Some(codec.clone());
            v.audio = Some(if format == "webm" {
                AudioCodec::Opus
            } else {
                AudioCodec::Aac
            });
            v.width = meta["w"].as_u64().map(|w| w as u32);
            v.height = meta["h"]
                .as_u64()
                .map(|h| h as u32)
                .or_else(|| quality.parse().ok());
            v.bitrate = meta["bitrate"].as_u64().filter(|b| *b > 0).map(|kbps| kbps * 1000);
            v.size = meta["size"].as_u64().filter(|s| *s > 0);
            v.duration = duration;
            v.format_id = Some(format!("{format}-{quality}"));
            v.label = v.height.map(|h| format!("{h}p"));
            v.headers = headers.clone();
            variants.push(v);
        }
    }
    if let Some(qualities) = ua["hls"].as_object() {
        for (quality, entry) in qualities {
            let Some(url) = entry["url"].as_str().and_then(|u| Url::parse(u).ok()) else {
                continue;
            };
            if variants.iter().any(|v| v.url == url) {
                continue;
            }
            let meta = &entry["meta"];
            let mut v = Variant::new(url, VariantKind::Hls);
            v.width = meta["w"].as_u64().map(|w| w as u32);
            v.height = meta["h"].as_u64().map(|h| h as u32).or_else(|| quality.parse().ok());
            v.bitrate = meta["bitrate"].as_u64().filter(|b| *b > 0).map(|kbps| kbps * 1000);
            v.duration = duration;
            v.live = live;
            v.format_id = Some(format!("hls-{quality}"));
            v.headers = headers.clone();
            variants.push(v);
        }
    }
    if let Some(url) = data["u"]["mp4"]["url"].as_str().and_then(|u| Url::parse(u).ok())
        && !variants.iter().any(|v| v.url == url)
    {
        let meta = &data["u"]["mp4"]["meta"];
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(Container::Mp4);
        v.video = Some(VideoCodec::H264);
        v.audio = Some(AudioCodec::Aac);
        v.width = meta["w"].as_u64().map(|w| w as u32);
        v.height = meta["h"].as_u64().map(|h| h as u32);
        v.bitrate = meta["bitrate"].as_u64().filter(|b| *b > 0).map(|kbps| kbps * 1000);
        v.size = meta["size"].as_u64().filter(|s| *s > 0);
        v.duration = duration;
        v.format_id = Some("mp4".into());
        v.headers = headers.clone();
        variants.push(v);
    }
    variants
}

#[async_trait]
impl Resolver for RumbleResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Rumble",
            hosts: &["rumble.com"],
            features: &["videos", "embeds", "live", "subtitles", "channels", "user pages"],
            formats: &["mp4", "webm", "hls"],
            session: SessionSupport::Optional,
            examples: &[
                "https://rumble.com/v6rrcbh-channel-update-vid.html",
                "https://rumble.com/embed/v6pkg2n/",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Video(path) => {
                let (details, embed_id, page_url) = {
                    let (page, page_url) = self.video_page(&path, url).await?;
                    let embed_id = embed_id_in(&page).ok_or_else(|| {
                        if page.html().contains("Video not found") {
                            ResolveError::NotFound(url.clone())
                        } else {
                            ResolveError::malformed(url, "the page names no embed")
                        }
                    })?;
                    (PageDetails::of(&page), embed_id, page_url)
                };
                self.video(&embed_id, Some(&details), &page_url, url).await
            }
            Link::Embed(id) => {
                let page_url = Url::parse(&format!("{SITE}embed/{id}/")).expect("valid");
                self.video(&id, None, &page_url, url).await
            }
            Link::Channel(channel) => self.listing(&format!("c/{channel}"), url).await,
            Link::User(user) => self.listing(&format!("user/{user}"), url).await,
        }
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        if !self.logged_in() {
            return Ok(SessionCheck::LoggedOut);
        }
        let account = Url::parse(&format!("{SITE}account/")).expect("valid");
        let response = self
            .http
            .get(account)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .follow_redirects(false)
            .send()
            .await?;
        if !response.is_success() {
            return Ok(SessionCheck::LoggedOut);
        }
        let html = response.text(MAX_PAGE).await?;
        let page = Page::parse(&html, &Url::parse(SITE).expect("valid"));
        let name = page
            .document()
            .select(&Selector::parse("[data-js='user_name'], .header-user-name, .main-menu-item-label-user").expect("valid"))
            .next()
            .and_then(|n| clean_title(&n.text().collect::<String>()))
            .or_else(|| page.between("\"username\":\"", "\"").and_then(clean_title));
        Ok(match name {
            Some(name) => SessionCheck::LoggedIn { account: name },
            None => SessionCheck::LoggedOut,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };
    use serde_json::json;

    fn get(url: &str, status: u16, content_type: &str, body: &str) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: "GET".into(),
                url: url.into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status,
                url: url.into(),
                headers: vec![("content-type".into(), content_type.into())],
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    const VIDEO_PAGE: &str = r#"<html><head><title>Channel Update vid</title>
      <meta name="description" content="Just a quick update about the channel.">
      <script type="application/ld+json">[{"@context":"http://schema.org","@type":"VideoObject","name":"Channel Update vid","description":"Just a quick update about the channel, new changes, and what to expect for a while.","thumbnailUrl":"https://hugh.cdn.rumble.cloud/video/thumb.jpg","uploadDate":"2025-04-07T02:38:42+00:00","duration":"PT00H01M23S","embedUrl":"https://rumble.com/embed/v6pkg2n/"}]</script>
      </head><body></body></html>"#;

    fn embed_json() -> Value {
        json!({
            "fps": 60, "w": 1920, "h": 1080, "title": "Channel Update vid", "author": {"name": "WolfBlitzerTTV", "url": "https://rumble.com/user/WolfBlitzerTTV"},
            "duration": 83, "pubDate": "2025-04-07T02:38:42+00:00", "live": 0, "i": "https://hugh.cdn.rumble.cloud/video/fww1/small.jpg",
            "u": {"mp4": {"url": "https://hugh.cdn.rumble.cloud/video/fww1/DO0zy.caa.mp4", "meta": {"bitrate": 1013, "size": 10551412, "w": 854, "h": 480}}},
            "ua": {"mp4": {
                "360": {"url": "https://hugh.cdn.rumble.cloud/video/fww1/DO0zy.baa.mp4", "meta": {"bitrate": 641, "size": 6677690, "w": 640, "h": 360}},
                "480": {"url": "https://hugh.cdn.rumble.cloud/video/fww1/DO0zy.caa.mp4", "meta": {"bitrate": 1013, "size": 10551412, "w": 854, "h": 480}},
                "1080": {"url": "https://hugh.cdn.rumble.cloud/video/fww1/DO0zy.gaa.mp4", "meta": {"bitrate": 4133, "size": 43000000, "w": 1920, "h": 1080}}
            }},
            "cc": {"en-auto": {"language": "English (auto)", "path": "https://hugh.cdn.rumble.cloud/video/fww1/DO0zy.fn8Si.vtt"}}
        })
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://rumble.com/v6rrcbh-channel-update-vid.html?e9s=src"),
            Some(Link::Video("v6rrcbh-channel-update-vid.html".into()))
        );
        assert_eq!(link("https://rumble.com/embed/v6pkg2n/"), Some(Link::Embed("v6pkg2n".into())));
        assert_eq!(
            link("https://rumble.com/embedJS/u3/?request=video&ver=2&v=v6pkg2n"),
            Some(Link::Embed("v6pkg2n".into()))
        );
        assert_eq!(link("https://rumble.com/c/Rumble"), Some(Link::Channel("Rumble".into())));
        assert_eq!(link("https://rumble.com/user/WolfBlitzerTTV/videos"), Some(Link::User("WolfBlitzerTTV".into())));
        assert_eq!(link("https://rumble.com/videos?date=today"), None);
        assert_eq!(link("https://rumble.com/"), None);
    }

    #[tokio::test]
    async fn videos_resolve_through_the_embed_api() {
        let mut fixture = Fixture::new("rumble", None);
        fixture.exchanges.push(get("https://rumble.com/v6rrcbh-channel-update-vid.html", 200, "text/html", VIDEO_PAGE));
        fixture.exchanges.push(get(EMBED_API, 200, "application/json", &embed_json().to_string()));
        let resolver = RumbleResolver::new(Http::replay(fixture));
        let url = Url::parse("https://rumble.com/v6rrcbh-channel-update-vid.html?t=30").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("v6pkg2n"));
        assert_eq!(resolved.title.as_deref(), Some("Channel Update vid"));
        assert_eq!(
            resolved.description.as_deref(),
            Some("Just a quick update about the channel, new changes, and what to expect for a while.")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("WolfBlitzerTTV"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://rumble.com/user/WolfBlitzerTTV"
        );
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.duration, Some(Duration::from_secs(83)));
        assert_eq!(resolved.clip.unwrap().start, Duration::from_secs(30));
        assert!(!resolved.live);
        assert_eq!(resolved.variants.len(), 3);
        let tallest = resolved.variants.iter().find(|v| v.height == Some(1080)).unwrap();
        assert_eq!(tallest.size, Some(43000000));
        assert_eq!(tallest.bitrate, Some(4_133_000));
        assert_eq!(tallest.label.as_deref(), Some("1080p"));
        assert_eq!(resolved.subtitles.len(), 1);
        assert!(resolved.subtitles[0].auto);
        assert_eq!(resolved.subtitles[0].language, "en");
    }

    #[tokio::test]
    async fn embeds_fall_back_to_the_inlined_page_data_and_broadcasts_are_live() {
        let mut live = embed_json();
        live["live"] = json!(1);
        live["ua"] = json!({"hls": {"auto": {"url": "https://hugh.cdn.rumble.cloud/live/abc/playlist.m3u8", "meta": {"bitrate": 0}}}});
        live["u"] = json!({});
        let challenge = "<!DOCTYPE html><html><head><title>Just a moment...</title></head><body>challenge</body></html>";
        let embed_page = format!(
            r#"<html><head><title>Channel Update vid - Rumble</title></head><body><script>window.RumblePlayer = {{}}; var player = {}; </script></body></html>"#,
            live
        );
        let mut fixture = Fixture::new("rumble", None);
        fixture.exchanges.push(get(EMBED_API, 200, "text/html", challenge));
        fixture.exchanges.push(get("https://rumble.com/embed/v6pkg2n/", 200, "text/html", &embed_page));
        let resolver = RumbleResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://rumble.com/embed/v6pkg2n/").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert!(resolved.live);
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].kind, VariantKind::Hls);
        assert!(resolved.variants[0].live);
    }

    #[tokio::test]
    async fn listings_and_missing_pages() {
        let listing = r#"<html><head><title>Rumble</title></head><body><h1>Rumble</h1>
          <div class="channel-listing__container">
            <div class="videostream thumbnail__grid--item"><a class="videostream__link link" href="/v6rrcbh-channel-update-vid.html?e9s=src_v1_ucp"><h3 class="thumbnail__title" title="Channel Update vid">Channel Update vid</h3><div class="videostream__status--duration">1:23</div></a></div>
            <div class="videostream thumbnail__grid--item"><a class="videostream__link link" href="/v5abcde-second.html"><h3 class="thumbnail__title">Second</h3></a></div>
          </div></body></html>"#;
        let mut fixture = Fixture::new("rumble", None);
        fixture.exchanges.push(get("https://rumble.com/c/Rumble/videos", 200, "text/html", listing));
        fixture.exchanges.push(get("https://rumble.com/c/Rumble/videos?page=2", 404, "text/html", "<html>404</html>"));
        fixture.exchanges.push(get("https://rumble.com/c/Empty/videos", 200, "text/html", "<html><body><h1>Empty</h1><section class=\"channel-listing__container\"></section></body></html>"));
        fixture.exchanges.push(get("https://rumble.com/v5qvf5-this-is-rumble.html", 404, "text/html", "<html><title>404 Video not found</title></html>"));
        let resolver = RumbleResolver::new(Http::replay(fixture));
        let playlist = match resolver
            .resolve(&Url::parse("https://rumble.com/c/Rumble").unwrap())
            .await
            .unwrap()
        {
            Resolution::Playlist(playlist) => playlist,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(playlist.title.as_deref(), Some("Rumble"));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://rumble.com/v6rrcbh-channel-update-vid.html"
        );
        assert_eq!(playlist.entries[0].duration, Some(Duration::from_secs(83)));
        assert_eq!(playlist.entries[1].title.as_deref(), Some("Second"));
        let error = resolver
            .resolve(&Url::parse("https://rumble.com/c/Empty").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("scripts")),
            "{error}"
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://rumble.com/v5qvf5-this-is-rumble.html").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
