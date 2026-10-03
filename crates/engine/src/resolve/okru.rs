//! OK.ru (Odnoklassniki) videos, live broadcasts, groups and embeds: a video page carries
//! the player's options with the movie's metadata, which names the MP4 files by quality,
//! the HLS playlist of a recording and the master playlist of a broadcast; a page whose
//! metadata the player fetches on its own is asked for the same way. A group's video tab
//! lists its videos as cards. A video that only embeds another site's player hands the
//! link on to that site, and the mobile site is read when the desktop page has no player.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionSupport, SubtitleFormat, SubtitleTrack, Tag, Variant, clean_title, essence,
    fetch, hls, navigation_headers, probe_file, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "okru";
const SITE: &str = "https://ok.ru";
const MOBILE_SITE: &str = "https://m.ok.ru";

static RE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9-]+$").unwrap());
/// `data-options="{…}"`: the player's options, HTML-escaped JSON.
static RE_DATA_OPTIONS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-options=(?:"([^"]+)"|'([^']+)')"#).unwrap());
/// The reason the page shows in place of a video, in the player's or the page's notice.
static RE_WITHHELD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:vp_video_stub_txt|stub-empty_t)[^>]*>\s*([^<]+?)\s*<"#).unwrap()
});
/// `data-video="{…}"`: the mobile player's data, HTML-escaped JSON.
static RE_DATA_VIDEO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-video="([^"]+)""#).unwrap());
/// The reason the mobile page shows in place of a video.
static RE_MOBILE_ERROR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?s)видео</a>\s*<div\s+class="empty">(.+?)</div>"#).unwrap());
/// `href="/video/123" class="video-card_lk"`: a video card's link on a group's video tab.
static RE_CARD: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"href="/video/(\d+)"[^>]*class="video-card_lk""#).unwrap());
static RE_CARD_DURATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"video-card_duration">\s*([0-9:]+)"#).unwrap());
static RE_CARD_TITLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"class="video-card_n[^"]*"[^>]*title="([^"]*)""#).unwrap());
/// A YouTube video id, as the metadata of a video that embeds one names it.
static RE_YOUTUBE_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]{11}$").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A video or a broadcast, as the site numbers them.
    Video { id: String, embed: bool },
    /// A group, whose video tab is listed.
    Group { id: String },
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let site = host
        .strip_prefix("www.")
        .or_else(|| host.strip_prefix("m."))
        .or_else(|| host.strip_prefix("mobile."))
        .unwrap_or(&host);
    if site != "ok.ru" && site != "odnoklassniki.ru" {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let is_id = |id: &str| RE_ID.is_match(id) && id.chars().any(|c| c.is_ascii_digit());
    match segments.as_slice() {
        ["video", id] | ["live", id] | ["web-api", "video", "moviePlayer", id] if is_id(id) => {
            Some(Link::Video {
                id: id.to_string(),
                embed: false,
            })
        }
        ["videoembed", id] if is_id(id) => Some(Link::Video {
            id: id.to_string(),
            embed: true,
        }),
        ["dk"] => {
            let id = util::query_param(url, "st.mvId").filter(|id| is_id(id))?;
            Some(Link::Video { id, embed: false })
        }
        ["group", id] | ["group", id, "video"] | ["group", id, "videos"] if is_id(id) => {
            Some(Link::Group { id: id.to_string() })
        }
        _ => None,
    }
}

/// The height a quality name stands for in the player's ladder.
fn quality_height(name: &str) -> Option<u32> {
    Some(match name {
        "mobile" => 144,
        "lowest" => 240,
        "low" => 360,
        "sd" => 480,
        "hd" => 720,
        "full" => 1080,
        "quad" => 1440,
        "ultra" => 2160,
        _ => return None,
    })
}

/// The height the `type` of a file link stands for, for files without a quality name.
fn type_height(url: &Url) -> Option<u32> {
    Some(match util::query_param(url, "type")?.as_str() {
        "4" => 144,
        "0" => 240,
        "1" => 360,
        "2" => 480,
        "3" => 720,
        "5" => 1080,
        "6" => 1440,
        "7" => 2160,
        _ => return None,
    })
}

/// The player's options on a video page: the `data-options` JSON that carries the player
/// of `id`.
pub fn player_options(html: &str, id: &str) -> Option<Value> {
    RE_DATA_OPTIONS.captures_iter(html).find_map(|caps| {
        let raw = caps.get(1).or_else(|| caps.get(2))?.as_str();
        let unescaped = util::html_unescape(raw);
        if !unescaped.contains("flashvars") || !unescaped.contains(id) {
            return None;
        }
        serde_json::from_str::<Value>(&unescaped).ok()
    })
}

/// The reason a page shows in place of its video.
pub fn withheld_reason(html: &str) -> Option<String> {
    util::search(&RE_WITHHELD, html).and_then(|reason| clean_title(&reason))
}

/// The MP4 files the metadata lists, one per quality, with their heights from the
/// ladder, capped at the movie's own height.
pub fn mp4_variants(metadata: &Value) -> Vec<Variant> {
    let movie = &metadata["movie"];
    let movie_width = util::u32_of(&movie["width"]).filter(|w| *w > 0);
    let movie_height = util::u32_of(&movie["height"]).filter(|h| *h > 0);
    let mut variants = Vec::new();
    for file in metadata["videos"].as_array().into_iter().flatten() {
        let Some(url) = util::url_of(&file["url"], None) else {
            continue;
        };
        if util::boolean(&file["disallowed"]).unwrap_or(false) {
            continue;
        }
        let name = util::text(&file["name"]);
        let mut variant = Variant::file(url);
        variant.container = Some(Container::Mp4);
        variant.video = Some(VideoCodec::H264);
        variant.audio = Some(AudioCodec::Aac);
        let ladder = name
            .as_deref()
            .and_then(quality_height)
            .or_else(|| type_height(&variant.url));
        variant.height = match (ladder, movie_height) {
            (Some(h), Some(max)) => Some(h.min(max)),
            (Some(h), None) => Some(h),
            (None, max) => max,
        };
        variant.width = match (variant.height, movie_width, movie_height) {
            (Some(h), Some(mw), Some(mh)) if mh > 0 => Some((h * mw / mh) & !1),
            _ => None,
        };
        variant.format_id = name.clone();
        variant.label = variant.height.map(|h| format!("{h}p"));
        variants.push(variant);
    }
    variants
}

/// The subtitle tracks the movie carries.
fn subtitles_of(metadata: &Value) -> Vec<SubtitleTrack> {
    metadata["movie"]["subtitleTracks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|track| {
            Some(SubtitleTrack {
                url: util::url_of(&track["url"], None)?,
                language: util::text(&track["language"]).unwrap_or_else(|| "en".to_string()),
                name: util::text(&track["title"]),
                format: SubtitleFormat::Vtt,
                auto: false,
                headers: Vec::new(),
            })
        })
        .collect()
}

/// The link another site's player is embedded from, when the video is one.
fn external_link(player: &Value, metadata: &Value) -> Option<Url> {
    if util::boolean(&player["isExternalPlayer"]).unwrap_or(false)
        && let Some(url) = util::url_of(&player["url"], None)
    {
        return Some(url);
    }
    if metadata["provider"].as_str() == Some("USER_YOUTUBE") {
        let content = util::text(&metadata["movie"]["contentId"])?;
        if let Ok(url) = Url::parse(&content) {
            return Some(url);
        }
        if RE_YOUTUBE_ID.is_match(&content) {
            return Url::parse(&format!("https://www.youtube.com/watch?v={content}")).ok();
        }
    }
    None
}

/// The group's name from its page title, without the site's suffixes.
fn group_title(page: &Page) -> Option<String> {
    let title = page.meta("og:title").or_else(|| page.title())?;
    let title = title
        .trim_end_matches("| OK.RU")
        .trim_end_matches("| OK")
        .trim_end();
    let title = title.trim_end_matches("— Видео").trim_end();
    clean_title(title)
}

/// The videos a group's video tab lists, as cards.
pub fn group_videos(html: &str) -> Vec<PlaylistEntry> {
    let cards: Vec<(usize, usize, String)> = RE_CARD
        .captures_iter(html)
        .map(|caps| {
            let whole = caps.get(0).expect("match");
            (whole.start(), whole.end(), caps[1].to_string())
        })
        .collect();
    let mut entries: Vec<PlaylistEntry> = Vec::new();
    for (index, (_, end, id)) in cards.iter().enumerate() {
        let next = cards
            .get(index + 1)
            .map(|(start, _, _)| *start)
            .unwrap_or(html.len())
            .min(end + 6000);
        let chunk = &html[*end..next];
        let Ok(url) = Url::parse(&format!("{SITE}/video/{id}")) else {
            continue;
        };
        if entries.iter().any(|e| e.url == url) {
            continue;
        }
        entries.push(PlaylistEntry {
            url,
            title: util::search(&RE_CARD_TITLE, chunk)
                .map(|t| util::html_unescape(&t))
                .and_then(|t| clean_title(&t)),
            duration: util::search(&RE_CARD_DURATION, chunk)
                .and_then(|d| util::parse_duration(&d))
                .filter(|d| !d.is_zero()),
        });
    }
    entries
}

pub struct OkruResolver {
    http: Http,
}

impl OkruResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// A page of the site, read as a browser navigating to it.
    async fn page(&self, url: &Url, origin: &Url) -> Result<String, ResolveError> {
        let fetched = fetch(
            &self.http,
            url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(fetched.status, origin) {
            return Err(error);
        }
        Ok(fetched.text())
    }

    /// The movie's metadata: in the player's options, or fetched from the link the
    /// options name, the way the player does.
    async fn metadata(&self, player: &Value, origin: &Url) -> Result<Value, ResolveError> {
        let flashvars = &player["flashvars"];
        match &flashvars["metadata"] {
            Value::Object(_) => return Ok(flashvars["metadata"].clone()),
            Value::String(text) => {
                return serde_json::from_str(text)
                    .map_err(|e| ResolveError::malformed(origin, format!("metadata JSON: {e}")));
            }
            _ => {}
        }
        let metadata_url = util::text(&flashvars["metadataUrl"])
            .map(|u| util::url_decode(&u))
            .and_then(|u| Url::parse(&u).ok())
            .ok_or_else(|| ResolveError::malformed(origin, "the player names no metadata"))?;
        let location = util::text(&flashvars["location"]).unwrap_or_default();
        let fields: Vec<(&str, &str)> = if location.is_empty() {
            Vec::new()
        } else {
            vec![("st.location", location.as_str())]
        };
        let response = self
            .http
            .post(metadata_url.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("referer", origin.as_str())
            .form(&fields)
            .send()
            .await?;
        if let Some(error) = status_error(response.status, origin) {
            return Err(error);
        }
        response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, format!("metadata JSON: {e}")))
    }

    /// The streams the metadata names: the MP4 files, the recording's playlist or the
    /// broadcast's master playlist.
    async fn variants_of(
        &self,
        metadata: &Value,
        live: bool,
    ) -> Result<Vec<Variant>, ResolveError> {
        let mut variants = mp4_variants(metadata);
        let mut playlists: Vec<(Url, bool)> = Vec::new();
        for key in ["hlsManifestUrl", "ondemandHls"] {
            if let Some(url) = util::url_of(&metadata[key], None)
                && !playlists.iter().any(|(u, _)| *u == url)
            {
                playlists.push((url, false));
            }
        }
        if let Some(url) = util::url_of(&metadata["hlsMasterPlaylistUrl"], None)
            && !playlists.iter().any(|(u, _)| *u == url)
        {
            playlists.push((url, true));
        }
        let mut failure = None;
        for (playlist, broadcast) in &playlists {
            match hls::expand(&self.http, playlist, PLATFORM, BROWSER_UA, &[]).await {
                Ok(expanded) => {
                    for mut variant in expanded.variants {
                        variant.live = live || *broadcast || expanded.live;
                        variant.format_id = Some(match &variant.label {
                            Some(label) => format!("hls-{label}"),
                            None => "hls".to_string(),
                        });
                        if !variants.iter().any(|v| v.url == variant.url) {
                            variants.push(variant);
                        }
                    }
                }
                Err(error) => failure = Some(error),
            }
        }
        if variants.is_empty()
            && let Some(error) = failure
        {
            return Err(error);
        }
        for variant in &mut variants {
            variant.live |= live;
        }
        Ok(variants)
    }

    async fn resolve_video(
        &self,
        id: &str,
        embed: bool,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let mode = if embed { "videoembed" } else { "video" };
        let page_url = Url::parse(&format!("{SITE}/{mode}/{id}")).expect("valid");
        let html = self.page(&page_url, origin).await?;
        if html.contains(">Access to this video is restricted</div>") {
            return Err(ResolveError::login_required(
                origin,
                PLATFORM,
                "access to the video is restricted to logged-in users",
            ));
        }
        let Some(player) = player_options(&html, id) else {
            if let Some(reason) = withheld_reason(&html) {
                let missing = [
                    "Видео не найдено",
                    "Video has not been found",
                    "Video not found",
                ];
                return Err(if missing.iter().any(|m| reason.starts_with(m)) {
                    ResolveError::NotFound(origin.clone())
                } else {
                    ResolveError::unavailable(origin, reason)
                });
            }
            return self.resolve_mobile(id, origin).await;
        };
        let metadata = self.metadata(&player, origin).await?;
        if let Some(external) = external_link(&player, &metadata) {
            return Err(ResolveError::Redirect(external));
        }
        let movie = &metadata["movie"];
        let live = util::boolean(&movie["isLive"]).unwrap_or(false)
            || metadata["hlsMasterPlaylistUrl"].as_str().is_some();
        let variants = self.variants_of(&metadata, live).await?;
        let page = Page::parse(&html, &page_url);
        if variants.is_empty() {
            return Err(ResolveError::unavailable(
                origin,
                if metadata["paymentInfo"].is_object() {
                    "the video is paid: a subscription unlocks it"
                } else {
                    "the player names no stream"
                },
            ));
        }
        let title = movie["title"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| page.meta("og:title").and_then(|t| clean_title(&t)));
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.to_string());
        resolved.description = page
            .meta("og:description")
            .and_then(|d| clean_title(&d))
            .filter(|d| Some(d) != title.as_ref());
        resolved.title = title;
        resolved.uploader = metadata["author"]["name"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| page.meta("ya:ovs:login").and_then(|n| clean_title(&n)));
        resolved.uploader_url = util::text(&metadata["author"]["id"])
            .and_then(|author| Url::parse(&format!("{SITE}/profile/{author}")).ok());
        resolved.uploaded_at = page
            .meta("ya:ovs:upload_date")
            .and_then(|d| util::parse_timestamp(&d));
        resolved.duration = if live {
            None
        } else {
            util::seconds(&movie["duration"]).filter(|d| !d.is_zero())
        };
        resolved.thumbnail = util::url_of(&movie["poster"], None)
            .or_else(|| page.meta("og:image").and_then(|i| Url::parse(&i).ok()));
        resolved.webpage_url = Url::parse(&format!("{SITE}/video/{id}")).ok();
        resolved.live = live;
        resolved.age_limit = page
            .meta("ya:ovs:adult")
            .filter(|adult| adult == "true")
            .map(|_| 18);
        resolved.subtitles = subtitles_of(&metadata);
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    /// The mobile site's player: one MP4 behind a redirect.
    async fn resolve_mobile(&self, id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let page_url = Url::parse(&format!("{MOBILE_SITE}/video/{id}")).expect("valid");
        let html = self.page(&page_url, origin).await?;
        if let Some(reason) = util::search(&RE_MOBILE_ERROR, &html) {
            return Err(ResolveError::unavailable(
                origin,
                clean_title(&util::clean_html(&reason))
                    .unwrap_or_else(|| "the video is withheld".to_string()),
            ));
        }
        let data: Value = util::search(&RE_DATA_VIDEO, &html)
            .and_then(|raw| serde_json::from_str(&util::html_unescape(&raw)).ok())
            .ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
        let source = util::url_of(&data["videoSrc"], None)
            .ok_or_else(|| ResolveError::malformed(origin, "the mobile player names no file"))?;
        let probed = probe_file(&self.http, &source, PLATFORM, BROWSER_UA, &[]).await?;
        if let Some(error) = status_error(probed.status, origin) {
            return Err(error);
        }
        if essence(probed.content_type.as_deref()) == "text/html" {
            return Err(ResolveError::unavailable(
                origin,
                "the mobile player's file link leads to a page",
            ));
        }
        let mut variant = Variant::file(probed.url);
        variant.container = Some(Container::Mp4);
        variant.video = Some(VideoCodec::H264);
        variant.audio = Some(AudioCodec::Aac);
        variant.size = probed.size;
        variant.format_id = Some("mobile".to_string());
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.to_string());
        resolved.title = data["videoName"].as_str().and_then(clean_title);
        resolved.duration = util::millis(&data["videoDuration"]).filter(|d| !d.is_zero());
        resolved.thumbnail = util::url_of(&data["videoPosterSrc"], None);
        resolved.webpage_url = Url::parse(&format!("{SITE}/video/{id}")).ok();
        resolved.variants = vec![variant];
        Ok(Resolution::from(resolved))
    }

    async fn resolve_group(&self, id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let page_url = Url::parse(&format!("{SITE}/group/{id}/video")).expect("valid");
        let html = self.page(&page_url, origin).await?;
        let entries = group_videos(&html);
        if entries.is_empty() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        let page = Page::parse(&html, &page_url);
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(id.to_string()),
            title: group_title(&page),
            total: Some(entries.len()),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for OkruResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "OK.ru",
            hosts: &["ok.ru", "odnoklassniki.ru"],
            features: &["videos", "live", "groups", "embeds"],
            formats: &["mp4", "hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::Social, Tag::Video, Tag::Live],
            session: SessionSupport::None,
            on_by_default: true,
            examples: &[
                "https://ok.ru/video/20079905452",
                "https://ok.ru/videoembed/20079905452",
                "https://ok.ru/live/1115050286838",
                "https://ok.ru/group/70000000411441/video",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Video { id, embed } => self.resolve_video(&id, embed, url).await,
            Link::Group { id } => self.resolve_group(&id, url).await,
        }
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

    /// JSON as a page writes it into an attribute.
    fn escape(json: &Value) -> String {
        json.to_string()
            .replace('&', "&amp;")
            .replace('"', "&quot;")
    }

    fn page_with(id: &str, metadata: Value, meta: &[(&str, &str)]) -> String {
        let player = json!({"playerId": "p", "isExternalPlayer": false, "flashvars": {"metadata": metadata, "location": "video"}});
        let head: String = meta
            .iter()
            .map(|(k, v)| format!(r#"<meta property="{k}" content="{v}">"#))
            .collect();
        format!(
            r#"<html><head>{head}</head><body><div id="vp_{id}" data-options="{}"></div></body></html>"#,
            escape(&player)
        )
    }

    fn file(name: &str, kind: u8) -> Value {
        json!({"name": name, "url": format!("https://vd560.okcdn.ru/?expires=1&type={kind}&ct=0&id=50132290220"), "seekSchema": 3, "disallowed": false})
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let video = |id: &str, embed: bool| {
            Some(Link::Video {
                id: id.into(),
                embed,
            })
        };
        assert_eq!(
            link("https://ok.ru/video/20079905452"),
            video("20079905452", false)
        );
        assert_eq!(
            link("http://www.ok.ru/video/20648036891"),
            video("20648036891", false)
        );
        assert_eq!(
            link("https://m.ok.ru/video/2361249957145"),
            video("2361249957145", false)
        );
        assert_eq!(
            link("http://mobile.ok.ru/video/20079905452"),
            video("20079905452", false)
        );
        assert_eq!(
            link("https://odnoklassniki.ru/video/63567059965189-0?fromTime=5"),
            video("63567059965189-0", false)
        );
        assert_eq!(
            link("https://ok.ru/live/1115050286838"),
            video("1115050286838", false)
        );
        assert_eq!(
            link("https://ok.ru/videoembed/2932705602075"),
            video("2932705602075", true)
        );
        assert_eq!(
            link("http://ok.ru/web-api/video/moviePlayer/20079905452"),
            video("20079905452", false)
        );
        assert_eq!(
            link(
                "https://m.ok.ru/dk?st.cmd=movieLayer&st.discId=863789452017&st.mvId=863789452017&_prevCmd=friendMovies&tkn=3648"
            ),
            video("863789452017", false)
        );
        assert_eq!(
            link("https://ok.ru/group/70000000411441/video"),
            Some(Link::Group {
                id: "70000000411441".into()
            })
        );
        assert_eq!(
            link("https://ok.ru/group/70000000411441"),
            Some(Link::Group {
                id: "70000000411441".into()
            })
        );
        assert_eq!(link("https://ok.ru/video/notanumber"), None);
        assert_eq!(link("https://ok.ru/profile/123"), None);
        assert_eq!(link("https://ok.ru/"), None);
        assert_eq!(link("https://example.com/video/20079905452"), None);
    }

    #[tokio::test]
    async fn videos_resolve_with_their_files_and_playlist() {
        let metadata = json!({
            "movie": {"id": "50132290220", "movieId": "20079905452", "contentId": "50132290220", "title": "Культура меняет нас (прекрасный ролик!))",
                "poster": "https://iv.okcdn.ru/videoPreview?id=50132290220&type=37", "duration": "100", "isLive": false, "width": 1280, "height": 720,
                "subtitleTracks": [{"url": "https://vd560.okcdn.ru/subs/en.vtt", "language": "en", "title": "English"}]},
            "provider": "UPLOADED_ODKL", "author": {"id": "330537914540", "name": "Виталий Добровольский"},
            "videos": [file("mobile", 4), file("lowest", 0), file("low", 1), file("sd", 2), file("hd", 3)],
            "hlsManifestUrl": "https://vd560.okcdn.ru/video.m3u8?cmd=videoPlayerCdn&id=50132290220",
            "likeCount": 678959
        });
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://ok.ru/video/20079905452",
            200,
            "text/html",
            &page_with(
                "20079905452",
                metadata,
                &[
                    ("og:title", "Культура меняет нас (прекрасный ролик!))"),
                    ("og:description", "Культура меняет нас (прекрасный ролик!))"),
                    ("ya:ovs:upload_date", "2014-12-07T16:18:08+03:00"),
                    ("ya:ovs:adult", "false"),
                ],
            ),
        ));
        fixture.exchanges.push(get(
            "https://vd560.okcdn.ru/video.m3u8?cmd=videoPlayerCdn&id=50132290220",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2538617,RESOLUTION=1280x720,CODECS=\"avc1.64001f,mp4a.40.2\"\nhttps://vd560.okcdn.ru/hls/50132290220_high/index.m3u8\n",
        ));
        fixture.exchanges.push(get(
            "https://vd560.okcdn.ru/hls/50132290220_high/index.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n0.ts\n#EXT-X-ENDLIST\n",
        ));
        let resolver = OkruResolver::new(Http::replay(fixture));
        let url = Url::parse("https://ok.ru/video/20079905452").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("20079905452"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Культура меняет нас (прекрасный ролик!))")
        );
        assert_eq!(
            resolved.description, None,
            "the description repeats the title"
        );
        assert_eq!(resolved.uploader.as_deref(), Some("Виталий Добровольский"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://ok.ru/profile/330537914540"
        );
        assert_eq!(resolved.duration, Some(std::time::Duration::from_secs(100)));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.age_limit, None);
        assert!(!resolved.live);
        assert_eq!(resolved.subtitles.len(), 1);
        assert_eq!(resolved.subtitles[0].language, "en");
        assert_eq!(
            resolved.variants.len(),
            6,
            "five files and one playlist rendition"
        );
        let hd = resolved
            .variants
            .iter()
            .find(|v| v.format_id.as_deref() == Some("hd"))
            .unwrap();
        assert_eq!(hd.height, Some(720));
        assert_eq!(hd.width, Some(1280));
        assert_eq!(hd.container, Some(Container::Mp4));
        assert_eq!(hd.label.as_deref(), Some("720p"));
        let mobile = resolved
            .variants
            .iter()
            .find(|v| v.format_id.as_deref() == Some("mobile"))
            .unwrap();
        assert_eq!(mobile.height, Some(144));
        let hls = resolved
            .variants
            .iter()
            .find(|v| v.format_id.as_deref() == Some("hls-720p"))
            .unwrap();
        assert_eq!(hls.height, Some(720));
        assert!(!hls.live);
    }

    #[tokio::test]
    async fn broadcasts_resolve_live_and_embedded_players_hand_the_link_on() {
        let live = json!({
            "movie": {"id": "528984312566", "movieId": "1115050286838", "contentId": "528984312566", "title": "Первый канал. Прямой эфир",
                "poster": "https://iv.okcdn.ru/i?r=x", "duration": "0", "isLive": true, "width": 1280, "height": 720},
            "provider": "LIVE_TV_ODKL", "author": {}, "videos": [],
            "hlsMasterPlaylistUrl": "https://vsd162.okcdn.ru/hls/528984312566.m3u8/sig/x/expires/1/urls/y/clientType/0/id/528984312566"
        });
        let youtube = json!({
            "movie": {"id": "1", "movieId": "3952212382174", "contentId": "5axVgHHDBvU", "title": "Youtube-dl 101", "duration": "1529"},
            "provider": "USER_YOUTUBE", "author": {}, "videos": [{"name": "hd", "url": "https://www.youtube.com/watch?v=5axVgHHDBvU"}]
        });
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://ok.ru/video/1115050286838",
            200,
            "text/html",
            &page_with(
                "1115050286838",
                live,
                &[
                    ("og:title", "Первый канал. Прямой эфир"),
                    ("ya:ovs:login", "Первый канал"),
                    ("ya:ovs:adult", "false"),
                ],
            ),
        ));
        fixture.exchanges.push(get(
            "https://vsd162.okcdn.ru/hls/528984312566.m3u8/sig/x/expires/1/urls/y/clientType/0/id/528984312566",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=2538617,RESOLUTION=1280x720,QUALITY=hd,FRAME-RATE=25\n528984312566_high/index.m3u8\n#EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=684247,RESOLUTION=640x360,QUALITY=low,FRAME-RATE=25\n528984312566_low/index.m3u8\n",
        ));
        for rendition in ["high", "low"] {
            fixture.exchanges.push(get(
                &format!("https://vsd162.okcdn.ru/hls/528984312566.m3u8/sig/x/expires/1/urls/y/clientType/0/id/528984312566_{rendition}/index.m3u8"),
                200,
                "application/vnd.apple.mpegurl",
                "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n0.ts\n",
            ));
        }
        fixture.exchanges.push(get(
            "https://ok.ru/video/3952212382174",
            200,
            "text/html",
            &page_with("3952212382174", youtube, &[("og:title", "Youtube-dl 101")]),
        ));
        let resolver = OkruResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://ok.ru/live/1115050286838").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert!(resolved.live);
        assert_eq!(resolved.duration, None);
        assert_eq!(resolved.uploader.as_deref(), Some("Первый канал"));
        assert_eq!(resolved.variants.len(), 2);
        assert!(resolved.variants.iter().all(|v| v.live));
        assert_eq!(resolved.variants[0].height, Some(720));
        assert_eq!(resolved.variants[0].format_id.as_deref(), Some("hls-720p"));
        let error = resolver
            .resolve(&Url::parse("https://ok.ru/video/3952212382174").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Redirect(to) if to.as_str() == "https://www.youtube.com/watch?v=5axVgHHDBvU"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn missing_videos_and_pages_without_a_player_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://ok.ru/video/1",
            200,
            "text/html",
            r#"<html><body><div class="stub-empty __video-modern unavailable-video-stub"><div class="stub-empty_t">Видео не найдено</div></div></body></html>"#,
        ));
        fixture.exchanges.push(get(
            "https://ok.ru/video/2",
            200,
            "text/html",
            r#"<html><body><div class="vp_video_stub_txt">The author of this video has not been found or is blocked</div></body></html>"#,
        ));
        fixture.exchanges.push(get(
            "https://ok.ru/video/3",
            200,
            "text/html",
            r#"<html><body>Access to this video is restricted</div></body></html>"#,
        ));
        fixture.exchanges.push(get(
            "https://ok.ru/video/2361249957145",
            200,
            "text/html",
            r#"<html><body><p>nothing here</p></body></html>"#,
        ));
        fixture.exchanges.push(get(
            "https://m.ok.ru/video/2361249957145",
            200,
            "text/html",
            r#"<html><body><div data-video="{&quot;videoPosterSrc&quot;:&quot;https://iv.okcdn.ru/videoPreview?id=1&quot;,&quot;videoSrc&quot;:&quot;https://m.ok.ru/dk?st.cmd=moviePlaybackRedirect&amp;st.sig=abc&quot;,&quot;movieId&quot;:&quot;2361249957145&quot;,&quot;videoDuration&quot;:&quot;3038181&quot;,&quot;videoName&quot;:&quot;Быковское крещение&quot;}"></div></body></html>"#,
        ));
        fixture.exchanges.push(Exchange {
            request: RecordedRequest {
                method: "GET".into(),
                url: "https://m.ok.ru/dk?st.cmd=moviePlaybackRedirect&st.sig=abc".into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status: 206,
                url: "https://vd123.okcdn.ru/?expires=1&type=3&id=1".into(),
                headers: vec![
                    ("content-type".into(), "video/mp4".into()),
                    ("content-range".into(), "bytes 0-0/123456789".into()),
                ],
                body: RecordedBody::Empty,
                truncated: false,
            },
        });
        let resolver = OkruResolver::new(Http::replay(fixture));
        let url = |s: &str| Url::parse(s).unwrap();
        assert!(matches!(
            resolver
                .resolve(&url("https://ok.ru/video/1"))
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(&url("https://ok.ru/video/2"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("blocked")),
            "{error}"
        );
        assert!(matches!(
            resolver
                .resolve(&url("https://ok.ru/video/3"))
                .await
                .unwrap_err(),
            ResolveError::LoginRequired { .. }
        ));
        let mobile = resolver
            .resolve(&url("https://ok.ru/video/2361249957145"))
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(mobile.title.as_deref(), Some("Быковское крещение"));
        assert_eq!(
            mobile.duration,
            Some(std::time::Duration::from_millis(3038181))
        );
        assert_eq!(mobile.variants.len(), 1);
        assert_eq!(
            mobile.variants[0].url.as_str(),
            "https://vd123.okcdn.ru/?expires=1&type=3&id=1"
        );
        assert_eq!(mobile.variants[0].size, Some(123456789));
    }

    #[tokio::test]
    async fn groups_list_their_video_tab() {
        let card = |id: &str, title: &str, duration: &str| {
            format!(
                r##"<div class="video-card"><a aria-label="Смотреть" tabindex="-1" href="/video/{id}" class="video-card_lk"><img alt="{title}"></a><div class="video-card_duration-w"><div class="video-card_duration">{duration}</div></div><div class="video-card_n-w"><a class="video-card_n ellip" href="#" onclick="OK.VideoPlayer.openMovie(&quot;{id}&quot;); return false;" title="{title}">{title}</a></div></div>"##
            )
        };
        let html = format!(
            r#"<html><head><meta property="og:title" content="Катя Студа готовит — Видео | OK.RU"></head><body>{}{}{}</body></html>"#,
            card("15917195528753", "Как вам такой лайфхак?", "00:28"),
            card(
                "15906589444657",
                "Ленивая пицца &laquo;пельменная&raquo; за 5 минут!",
                "1:02:20"
            ),
            card("15917195528753", "Как вам такой лайфхак?", "00:28"),
        );
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://ok.ru/group/70000000411441/video",
            200,
            "text/html",
            &html,
        ));
        fixture.exchanges.push(get(
            "https://ok.ru/group/1/video",
            404,
            "text/html",
            "<html>gone</html>",
        ));
        let resolver = OkruResolver::new(Http::replay(fixture));
        let Resolution::Playlist(group) = resolver
            .resolve(&Url::parse("https://ok.ru/group/70000000411441").unwrap())
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(group.title.as_deref(), Some("Катя Студа готовит"));
        assert_eq!(group.entries.len(), 2, "the repeated card is listed once");
        assert_eq!(
            group.entries[0].url.as_str(),
            "https://ok.ru/video/15917195528753"
        );
        assert_eq!(
            group.entries[0].title.as_deref(),
            Some("Как вам такой лайфхак?")
        );
        assert_eq!(
            group.entries[0].duration,
            Some(std::time::Duration::from_secs(28))
        );
        assert_eq!(
            group.entries[1].title.as_deref(),
            Some("Ленивая пицца «пельменная» за 5 минут!")
        );
        assert_eq!(
            group.entries[1].duration,
            Some(std::time::Duration::from_secs(3740))
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://ok.ru/group/1/video").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    /// Every example link resolves live: the video, the embed and the broadcast to
    /// media with playable streams, the group to entries.
    #[ignore = "reaches the live site: cargo test -- --ignored"]
    #[tokio::test]

    async fn live_examples_resolve() {
        use std::time::Duration;

        let resolver = OkruResolver::new(Http::new(crate::http::HttpConfig::default()));
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
            assert!(resolver.matches(&url), "{link}");
            let began = std::time::Instant::now();
            let resolution = tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&url))
                .await
                .expect("resolution timed out")
                .unwrap_or_else(|e| panic!("{link}: {e}"));
            let took = began.elapsed();
            match resolution {
                Resolution::Media(resolved) => {
                    assert!(
                        resolved.variants.iter().any(|v| v.is_playable()),
                        "{link}: no playable variant"
                    );
                    assert!(resolved.title.is_some(), "{link}: no title");
                    println!(
                        "{link}: {} variants, live {}, {:?} in {took:?}",
                        resolved.variants.len(),
                        resolved.live,
                        resolved.title
                    );
                }
                Resolution::Playlist(playlist) => {
                    assert!(!playlist.entries.is_empty(), "{link}: no entries");
                    println!(
                        "{link}: {} entries, {:?} in {took:?}",
                        playlist.entries.len(),
                        playlist.title
                    );
                }
            }
        }
    }
}
