//! BitChute videos, channels and playlists: the site's beta API answers a video's media
//! link and its details. Channel and playlist pages are paged through the old site's
//! `extend` endpoint, which returns the next cards as HTML.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::page::Page;
use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, Tag, Variant, clean_title, fetch, hls, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "bitchute";
const SITE: &str = "https://www.bitchute.com/";
const OLD_SITE: &str = "https://old.bitchute.com/";
const API: &str = "https://api.bitchute.com/api/beta/";
/// The CSRF token the old site's `extend` endpoint accepts.
const OLD_SITE_TOKEN: &str = "zyG6tQcGPE5swyAEFLqKUwMuMMuF6IO2DZ6ZDQjGfsL0e4dcTLwqkTTul05Jdve7";
const PAGE_SIZE: usize = 25;
/// How many cards a listing is read up to.
const MAX_ENTRIES: usize = 500;

/// `/video/{id}`, `/embed/{id}` or `/torrent/{x}/{id}.webtorrent`.
static RE_VIDEO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(?:video|embed|torrent/[^/?#]+)/([^/?#&.]+)").unwrap());
/// `/channel/{id}` or `/playlist/{id}`.
static RE_LISTING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(channel|playlist)/([^/?#&]+)").unwrap());
static RE_CARD_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<a\s[^>]*\bhref=["']/video/([^"'/]+)"#).unwrap());
static RE_COUNT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<span>(\d+)\s+videos?</span>").unwrap());
/// `seed122.bitchute.com`: the seed host a media link names, one of many that serve the
/// same file.
static RE_SEED_HOST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^seed[a-z0-9]+\.bitchute\.com$").unwrap());
/// The seed hosts a file is served from when the one its link names refuses it.
const SEED_HOSTS: &[&str] = &[
    "seed122",
    "seed125",
    "seed126",
    "seed128",
    "seed132",
    "seed150",
    "seed151",
    "seed152",
    "seed153",
    "seed167",
    "seed171",
    "seed177",
    "seed305",
    "seed307",
    "seedp29xb",
    "zb10-7gsop1v78",
];
/// `<script src="https://www.bitchute.com/embed/{id}/">` or an iframe of the same.
static RE_EMBED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<(?:script|iframe)\b[^>]*\bsrc=["'](https?://(?:www\.|old\.)?bitchute\.com/(?:video|embed|torrent/[^"'/]+)/[^"'/?#]+/?)["']"#)
        .unwrap()
});

/// The links a media file may be fetched from: the one the API names, then the same
/// file on every other seed host, in the order the site's player tries them.
pub fn seed_candidates(media_url: &Url) -> Vec<Url> {
    let mut candidates = vec![media_url.clone()];
    if let Some(host) = media_url.host_str()
        && RE_SEED_HOST.is_match(host)
    {
        for seed in SEED_HOSTS {
            let mut candidate = media_url.clone();
            if candidate
                .set_host(Some(&format!("{seed}.bitchute.com")))
                .is_ok()
                && !candidates.contains(&candidate)
            {
                candidates.push(candidate);
            }
        }
    }
    candidates
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Video { id: String },
    Listing { kind: String, id: String },
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !matches!(
        host.as_str(),
        "bitchute.com" | "www.bitchute.com" | "old.bitchute.com"
    ) {
        return None;
    }
    let path = url.path();
    if let Some(caps) = RE_VIDEO.captures(path) {
        return Some(Link::Video {
            id: caps[1].to_string(),
        });
    }
    RE_LISTING.captures(path).map(|caps| Link::Listing {
        kind: caps[1].to_string(),
        id: caps[2].to_string(),
    })
}

/// The class names the old site's cards use, per listing kind.
fn card_classes(kind: &str) -> (&'static str, &'static str, &'static str) {
    match kind {
        "playlist" => ("playlist-video", "title", "description"),
        _ => (
            "channel-videos-container",
            "channel-videos-title",
            "channel-videos-text",
        ),
    }
}

/// The videos a page of cards links, with each card's title.
pub fn cards(html: &str, kind: &str) -> Vec<(String, Option<String>)> {
    let (container, title_class, _) = card_classes(kind);
    let opener = Regex::new(&format!(
        r#"<div\b[^>]*\bclass=["'][^"']*\b{}\b[^"']*["'][^>]*>"#,
        regex::escape(container)
    ))
    .expect("class names are plain");
    let starts: Vec<usize> = opener.find_iter(html).map(|m| m.start()).collect();
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (index, start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(html.len());
        let block = &html[*start..end];
        let Some(id) = util::search(&RE_CARD_LINK, block) else {
            continue;
        };
        if !seen.insert(id.clone()) {
            continue;
        }
        let title = util::element_by_class(block, title_class)
            .map(|t| util::clean_html(&t))
            .and_then(|t| clean_title(&t));
        found.push((id, title));
    }
    found
}

pub struct BitchuteResolver {
    http: Http,
}

impl BitchuteResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// One call of the beta API. A refusal names the caller's location when that is why.
    async fn api(&self, endpoint: &str, body: Value, origin: &Url) -> Result<Value, ResolveError> {
        let api = Url::parse(&format!("{API}{endpoint}")).expect("valid");
        let response = self
            .http
            .post(api)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "application/json")
            .header("origin", SITE.trim_end_matches('/'))
            .header("referer", SITE)
            .json(&body)
            .send()
            .await?;
        let status = response.status;
        let text = response.text(MAX_PAGE).await?;
        if status.as_u16() == 403 {
            let refusal: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let reasons: Vec<String> = refusal["errors"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|e| e["context"].as_str() == Some("reason"))
                .filter_map(|e| util::text(&e["message"]))
                .collect();
            let reason = reasons.join(". ");
            if reason.contains("location") {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("refused for this location: {reason}"),
                ));
            }
            if !reason.is_empty() {
                return Err(ResolveError::unavailable(origin, reason));
            }
        }
        if status.as_u16() == 401 || status.as_u16() == 403 {
            let refusal: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let needs_account = refusal["errors"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|e| e["context"].as_str() == Some("AUTH"));
            if needs_account {
                let detail = refusal["errors"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|e| e["context"].as_str() != Some("AUTH"))
                    .filter_map(|e| util::text(&e["message"]))
                    .collect::<Vec<_>>()
                    .join(". ");
                return Err(ResolveError::login_required(
                    origin,
                    PLATFORM,
                    if detail.is_empty() {
                        "the API answers this to an account only".to_string()
                    } else {
                        detail
                    },
                ));
            }
        }
        if let Some(error) = status_error(status, origin) {
            return Err(error);
        }
        serde_json::from_str(&text)
            .map_err(|e| ResolveError::malformed(origin, format!("API JSON: {e}")))
    }

    async fn resolve_video(&self, id: &str, url: &Url) -> Result<Resolution, ResolveError> {
        let media = self
            .api("video/media", serde_json::json!({"video_id": id}), url)
            .await?;
        let media_url = util::url_of(&media["media_url"], None)
            .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let details = self
            .api("video", serde_json::json!({"video_id": id}), url)
            .await
            .unwrap_or(Value::Null);
        let mut resolved = Resolved::new(PLATFORM);
        if media_url.path().ends_with(".m3u8") {
            let expanded = hls::expand(&self.http, &media_url, PLATFORM, BROWSER_UA, &[]).await?;
            resolved.variants = expanded.variants;
            resolved.subtitles = expanded.subtitles;
            resolved.duration = expanded.duration;
            resolved.live = expanded.live;
        } else {
            // The seed host the API names often refuses the file while another seed
            // serves it. The first that answers is the one to fetch.
            let mut served = None;
            let mut refusal = None;
            for candidate in seed_candidates(&media_url) {
                match super::probe_file(&self.http, &candidate, PLATFORM, BROWSER_UA, &[]).await {
                    Ok(probe) if probe.status.is_success() => {
                        served = Some((candidate, probe.size));
                        break;
                    }
                    Ok(probe) => {
                        tracing::debug!(url = %candidate, status = %probe.status, "BitChute seed refused the file");
                        refusal = status_error(probe.status, url);
                    }
                    Err(error) => {
                        tracing::debug!(url = %candidate, "BitChute seed not reached: {error}");
                        refusal = Some(error);
                    }
                }
            }
            let Some((served_url, size)) = served else {
                return Err(refusal.unwrap_or_else(|| {
                    ResolveError::unavailable(url, "no seed host serves the video file")
                }));
            };
            let mut variant = Variant::file(served_url);
            variant.size = size;
            variant.container = Some(Container::Mp4);
            variant.video = Some(VideoCodec::H264);
            variant.audio = Some(AudioCodec::Aac);
            variant.format_id = Some("mp4".to_string());
            resolved.variants = vec![variant];
        }
        resolved.id = Some(id.to_string());
        resolved.title = details["video_name"].as_str().and_then(clean_title);
        resolved.description = details["description"].as_str().and_then(clean_title);
        resolved.thumbnail = util::url_of(&details["thumbnail_url"], None);
        resolved.uploaded_at = util::time(&details["date_published"]);
        resolved.duration = details["duration"]
            .as_str()
            .and_then(super::parse_time_stamp)
            .or(resolved.duration);
        resolved.live = resolved.live || details["state_id"].as_str() == Some("live");
        // The channel's record names the profile that owns it, the video's uploader.
        let channel = match util::text(&details["channel"]["channel_id"]) {
            Some(channel_id) => self
                .api(
                    "channel",
                    serde_json::json!({"channel_id": channel_id}),
                    url,
                )
                .await
                .unwrap_or(Value::Null),
            None => Value::Null,
        };
        resolved.uploader = channel["profile_name"]
            .as_str()
            .or(details["channel"]["channel_name"].as_str())
            .and_then(clean_title);
        resolved.uploader_url = util::text(&channel["profile_id"])
            .or_else(|| util::text(&details["profile_id"]))
            .and_then(|id| Url::parse(&format!("{SITE}profile/{id}/")).ok())
            .or_else(|| {
                details["channel"]["channel_url"]
                    .as_str()
                    .and_then(|href| Url::parse(SITE).ok()?.join(href).ok())
            })
            .or_else(|| {
                util::text(&details["channel"]["channel_id"])
                    .and_then(|id| Url::parse(&format!("{SITE}channel/{id}/")).ok())
            });
        resolved.webpage_url = Url::parse(&format!("{SITE}video/{id}/")).ok();
        Ok(Resolution::from(resolved))
    }

    /// A playlist's videos, through the beta API, which answers them only to an account.
    async fn resolve_playlist(&self, id: &str, url: &Url) -> Result<Resolution, ResolveError> {
        let mut entries: Vec<PlaylistEntry> = Vec::new();
        let mut offset = 0usize;
        loop {
            let page = self
                .api(
                    "playlist/videos",
                    serde_json::json!({"playlist_id": id, "offset": offset, "limit": PAGE_SIZE}),
                    url,
                )
                .await?;
            let videos = page["videos"].as_array().cloned().unwrap_or_default();
            if videos.is_empty() {
                break;
            }
            let count = videos.len();
            for video in &videos {
                let Some(video_id) = util::text(&video["video_id"]) else {
                    continue;
                };
                entries.push(PlaylistEntry {
                    url: Url::parse(&format!("{SITE}video/{video_id}/")).expect("ids are plain"),
                    title: video["video_name"].as_str().and_then(clean_title),
                    duration: video["duration"].as_str().and_then(super::parse_time_stamp),
                });
            }
            offset += PAGE_SIZE;
            if count < PAGE_SIZE || entries.len() >= MAX_ENTRIES {
                break;
            }
        }
        if entries.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let details = self
            .api("playlist", serde_json::json!({"playlist_id": id}), url)
            .await
            .unwrap_or(Value::Null);
        let total = entries.len();
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(id.to_string()),
            title: details["playlist_name"]
                .as_str()
                .or(details["name"].as_str())
                .and_then(clean_title),
            entries,
            total: util::uint(&details["video_count"])
                .map(|n| n as usize)
                .or(Some(total)),
        }))
    }

    async fn resolve_listing(
        &self,
        kind: &str,
        id: &str,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        if kind == "playlist" {
            return self.resolve_playlist(id, url).await;
        }
        let listing_url = Url::parse(&format!("{OLD_SITE}{kind}/{id}/")).expect("valid");
        let page = fetch(
            &self.http,
            &listing_url,
            PLATFORM,
            BROWSER_UA,
            &[],
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(page.status, url) {
            return Err(error);
        }
        let html = page.text();
        let (title, total) = {
            let page = super::Page::parse(&html, &listing_url);
            (
                page.meta("og:title")
                    .and_then(|t| clean_title(&t))
                    .or_else(|| page.title()),
                util::search(&RE_COUNT, &html).and_then(|n| n.parse::<usize>().ok()),
            )
        };
        let extend = Url::parse(&format!("{OLD_SITE}{kind}/{id}/extend/")).expect("valid");
        let mut entries: Vec<PlaylistEntry> = Vec::new();
        let mut offset = 0usize;
        loop {
            let response = self
                .http
                .post(extend.clone())
                .platform(PLATFORM)
                .user_agent(BROWSER_UA)
                .header("accept", "application/json")
                .header("x-requested-with", "XMLHttpRequest")
                .header("referer", listing_url.as_str())
                .header("cookie", &format!("csrftoken={OLD_SITE_TOKEN}"))
                .form(&[
                    ("csrfmiddlewaretoken", OLD_SITE_TOKEN),
                    ("name", ""),
                    ("offset", &offset.to_string()),
                ])
                .send()
                .await?;
            if let Some(error) = status_error(response.status, url) {
                return Err(error);
            }
            let answer: Value = response
                .json(MAX_PAGE)
                .await
                .map_err(|e| ResolveError::malformed(url, format!("listing JSON: {e}")))?;
            if answer["success"].as_bool() != Some(true) {
                break;
            }
            let found = cards(answer["html"].as_str().unwrap_or(""), kind);
            if found.is_empty() {
                break;
            }
            let count = found.len();
            for (video_id, title) in found {
                if entries
                    .iter()
                    .any(|e| e.url.path().ends_with(&format!("/video/{video_id}/")))
                {
                    continue;
                }
                entries.push(PlaylistEntry {
                    url: Url::parse(&format!("{SITE}video/{video_id}/")).expect("ids are plain"),
                    title,
                    duration: None,
                });
            }
            offset += PAGE_SIZE;
            if count < PAGE_SIZE || entries.len() >= MAX_ENTRIES {
                break;
            }
        }
        if entries.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(id.to_string()),
            title,
            total: total.or(Some(entries.len())),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for BitchuteResolver {
    /// The site's player embedded in a page: script and iframe tags loading a video,
    /// embed or torrent link.
    fn embeds_in(&self, page: &Page) -> Vec<Url> {
        let mut found: Vec<Url> = Vec::new();
        for caps in RE_EMBED.captures_iter(page.html()) {
            if let Ok(link) = Url::parse(&util::html_unescape(&caps[1]))
                && parse_link(&link).is_some()
                && !found.contains(&link)
            {
                found.push(link);
            }
        }
        found
    }

    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "BitChute",
            hosts: &["bitchute.com"],
            features: &["videos", "live", "channels", "playlists"],
            formats: &["mp4", "hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::Video],
            session: SessionSupport::None,
            examples: &[
                "https://www.bitchute.com/video/UGlrF9o9b-Q/",
                "https://www.bitchute.com/embed/UGlrF9o9b-Q/",
                "https://www.bitchute.com/channel/bitchute/",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Video { id } => self.resolve_video(&id, url).await,
            Link::Listing { kind, id } => self.resolve_listing(&kind, &id, url).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};
    use serde_json::json;

    fn exchange(
        method: &str,
        url: &str,
        status: u16,
        content_type: &str,
        body: String,
    ) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: method.into(),
                url: url.into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status,
                url: url.into(),
                headers: vec![
                    ("content-type".into(), content_type.into()),
                    ("content-length".into(), "12345".into()),
                ],
                body: RecordedBody::Text(body),
                truncated: false,
            },
        }
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://www.bitchute.com/video/UGlrF9o9b-Q/"),
            Some(Link::Video {
                id: "UGlrF9o9b-Q".into()
            })
        );
        assert_eq!(
            link("https://www.bitchute.com/embed/lbb5G1hjPhw/"),
            Some(Link::Video {
                id: "lbb5G1hjPhw".into()
            })
        );
        assert_eq!(
            link("https://www.bitchute.com/torrent/Zee5BE49045h/szoMrox2JEI.webtorrent"),
            Some(Link::Video {
                id: "szoMrox2JEI".into()
            })
        );
        assert_eq!(
            link("https://old.bitchute.com/video/UGlrF9o9b-Q/"),
            Some(Link::Video {
                id: "UGlrF9o9b-Q".into()
            })
        );
        assert_eq!(
            link("https://www.bitchute.com/channel/bitchute/"),
            Some(Link::Listing {
                kind: "channel".into(),
                id: "bitchute".into()
            })
        );
        assert_eq!(
            link("https://old.bitchute.com/playlist/wV9Imujxasw9/"),
            Some(Link::Listing {
                kind: "playlist".into(),
                id: "wV9Imujxasw9".into()
            })
        );
        assert_eq!(link("https://www.bitchute.com/"), None);
        assert_eq!(link("https://example.com/video/UGlrF9o9b-Q/"), None);
    }

    #[test]
    fn cards_are_read_from_listing_html() {
        let html = r#"<div class="channel-videos-container"><div class="channel-videos-title"><a href="/video/abc123/">First &amp; best</a></div><div class="channel-videos-text">desc</div></div>
            <div class="channel-videos-container"><a href="/video/abc123/">dup</a></div>
            <div class="channel-videos-container"><div class="channel-videos-title"><a href="/video/def456/">Second</a></div></div>"#;
        assert_eq!(
            cards(html, "channel"),
            vec![
                ("abc123".to_string(), Some("First & best".to_string())),
                ("def456".to_string(), Some("Second".to_string()))
            ]
        );
        assert!(cards("<div>no cards</div>", "channel").is_empty());
    }

    #[tokio::test]
    async fn videos_resolve_through_the_api() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "POST",
            "https://api.bitchute.com/api/beta/video/media",
            200,
            "application/json",
            json!({"video_id": "UGlrF9o9b-Q", "media_type": "MPEG-4", "media_url": "https://zbbb278hfll091.bitchute.com/1VBwRfyNcKdX/UGlrF9o9b-Q.mp4"}).to_string(),
        ));
        fixture.exchanges.push(exchange(
            "POST",
            "https://api.bitchute.com/api/beta/video",
            200,
            "application/json",
            json!({"video_id": "UGlrF9o9b-Q", "video_name": "This is the first video on #BitChute !", "description": "Sample.", "date_published": "2017-01-03T06:37:23Z",
                   "duration": "0:16", "thumbnail_url": "https://static-3.bitchute.com/live/cover_images/1VBwRfyNcKdX/UGlrF9o9b-Q_640x360.jpg", "state_id": "published",
                   "channel": {"channel_id": "1VBwRfyNcKdX", "channel_name": "BitChute", "channel_url": "/channel/bitchute/"}}).to_string(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://zbbb278hfll091.bitchute.com/1VBwRfyNcKdX/UGlrF9o9b-Q.mp4",
            200,
            "video/mp4",
            String::new(),
        ));
        fixture.exchanges.push(exchange(
            "POST",
            "https://api.bitchute.com/api/beta/video/media",
            403,
            "application/json",
            json!({"errors": [{"context": "reason", "message": "Video not available in your location"}]}).to_string(),
        ));
        let resolver = BitchuteResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.bitchute.com/video/UGlrF9o9b-Q/").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("UGlrF9o9b-Q"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("This is the first video on #BitChute !")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("BitChute"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://www.bitchute.com/channel/bitchute/"
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(16)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1483425443)
        );
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].size, Some(12345));
        let error = resolver
            .resolve(&Url::parse("https://www.bitchute.com/video/blocked/").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("location")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn private_playlists_need_an_account() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "POST",
            "https://api.bitchute.com/api/beta/playlist/videos",
            403,
            "application/json",
            json!({"errors": [{"context": "AUTH", "message": "Unauthorized - Please authenticate before attempting to perform this action"}, {"context": "wV9Imujxasw9", "message": "private playlist"}]}).to_string(),
        ));
        let resolver = BitchuteResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://www.bitchute.com/playlist/wV9Imujxasw9/").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::LoginRequired { reason, .. } if reason.contains("private playlist")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn public_playlists_list_their_videos() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "POST",
            "https://api.bitchute.com/api/beta/playlist/videos",
            200,
            "application/json",
            json!({"videos": [{"video_id": "UGlrF9o9b-Q", "video_name": "First", "duration": "0:16"}, {"video_id": "Yti_j9A-UZ4", "video_name": "Second", "duration": "1:02:03"}]}).to_string(),
        ));
        fixture.exchanges.push(exchange(
            "POST",
            "https://api.bitchute.com/api/beta/playlist",
            200,
            "application/json",
            json!({"playlist_id": "abc", "playlist_name": "Favourites", "video_count": 2})
                .to_string(),
        ));
        let resolver = BitchuteResolver::new(Http::replay(fixture));
        let Resolution::Playlist(playlist) = resolver
            .resolve(&Url::parse("https://www.bitchute.com/playlist/abc/").unwrap())
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(playlist.title.as_deref(), Some("Favourites"));
        assert_eq!(playlist.total, Some(2));
        assert_eq!(
            playlist.entries[1].duration,
            Some(Duration::from_secs(3723))
        );
        assert_eq!(
            playlist.entries[1].url.as_str(),
            "https://www.bitchute.com/video/Yti_j9A-UZ4/"
        );
    }

    #[tokio::test]
    async fn channels_page_through_their_cards() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "GET",
            "https://old.bitchute.com/channel/bitchute/",
            200,
            "text/html",
            r#"<html><head><meta property="og:title" content="BitChute"></head><body><span>2 videos</span></body></html>"#.into(),
        ));
        fixture.exchanges.push(exchange(
            "POST",
            "https://old.bitchute.com/channel/bitchute/extend/",
            200,
            "application/json",
            json!({"success": true, "html": r#"<div class="channel-videos-container"><div class="channel-videos-title"><a href="/video/UGlrF9o9b-Q/">First</a></div></div><div class="channel-videos-container"><div class="channel-videos-title"><a href="/video/Yti_j9A-UZ4/">Second</a></div></div>"#}).to_string(),
        ));
        let resolver = BitchuteResolver::new(Http::replay(fixture));
        let Resolution::Playlist(playlist) = resolver
            .resolve(&Url::parse("https://www.bitchute.com/channel/bitchute/").unwrap())
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(playlist.title.as_deref(), Some("BitChute"));
        assert_eq!(playlist.total, Some(2));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[1].url.as_str(),
            "https://www.bitchute.com/video/Yti_j9A-UZ4/"
        );
        assert_eq!(playlist.entries[0].title.as_deref(), Some("First"));
    }

    #[tokio::test]
    async fn refused_seeds_are_retried_and_profiles_name_the_uploader() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "POST",
            "https://api.bitchute.com/api/beta/video/media",
            200,
            "application/json",
            json!({"video_id": "abc", "media_type": "MPEG-4", "media_url": "https://seed167.bitchute.com/x/abc.mp4"}).to_string(),
        ));
        fixture.exchanges.push(exchange(
            "POST",
            "https://api.bitchute.com/api/beta/video",
            200,
            "application/json",
            json!({"video_id": "abc", "video_name": "Retried", "channel": {"channel_id": "chan1", "channel_name": "The Channel", "channel_url": "/channel/the-channel/"}}).to_string(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://seed167.bitchute.com/x/abc.mp4",
            404,
            "text/html",
            String::new(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://seed122.bitchute.com/x/abc.mp4",
            403,
            "text/html",
            String::new(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://seed125.bitchute.com/x/abc.mp4",
            200,
            "video/mp4",
            String::new(),
        ));
        fixture.exchanges.push(exchange(
            "POST",
            "https://api.bitchute.com/api/beta/channel",
            200,
            "application/json",
            json!({"channel_id": "chan1", "channel_name": "The Channel", "url_slug": "the-channel", "profile_id": "prof9", "profile_name": "Owner Person"}).to_string(),
        ));
        let resolver = BitchuteResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://www.bitchute.com/video/abc/").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(
            resolved.variants[0].url.as_str(),
            "https://seed125.bitchute.com/x/abc.mp4"
        );
        assert_eq!(resolved.uploader.as_deref(), Some("Owner Person"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://www.bitchute.com/profile/prof9/"
        );
        let candidates =
            seed_candidates(&Url::parse("https://seed167.bitchute.com/x/abc.mp4").unwrap());
        assert_eq!(
            candidates.len(),
            SEED_HOSTS.len(),
            "the named seed is one of the list"
        );
        assert_eq!(candidates[0].host_str(), Some("seed167.bitchute.com"));
        assert_eq!(candidates[1].host_str(), Some("seed122.bitchute.com"));
        assert_eq!(
            seed_candidates(&Url::parse("https://zbbb278hfll091.bitchute.com/x/abc.mp4").unwrap())
                .len(),
            1,
            "only seed hosts have twins"
        );
        let page = Page::parse(
            r#"<html><script src="https://www.bitchute.com/embed/UGlrF9o9b-Q/"></script><iframe src="https://old.bitchute.com/video/abc/"></iframe><iframe src="https://example.com/other"></iframe></html>"#,
            &Url::parse("https://example.com/post").unwrap(),
        );
        assert_eq!(
            resolver
                .embeds_in(&page)
                .iter()
                .map(|u| u.as_str())
                .collect::<Vec<_>>(),
            vec![
                "https://www.bitchute.com/embed/UGlrF9o9b-Q/",
                "https://old.bitchute.com/video/abc/"
            ]
        );
    }
}
