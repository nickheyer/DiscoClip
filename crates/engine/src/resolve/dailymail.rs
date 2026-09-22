//! Daily Mail videos (dailymail.com, formerly dailymail.co.uk): the video page's player
//! options name the video and its sources feed, which lists every rendition, HLS and
//! MP4. The options also name the player's own MP4.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Tag, Variant,
    clean_title, fetch, hls, navigation_headers, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "dailymail";
const SITE: &str = "https://www.dailymail.com";

/// `/video/{section}/video-{id}/{slug}.html` or `/embed/video/{id}.html`.
static RE_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(?:video/[^/]+/video-|embed/video/)(\d+)").unwrap());
static RE_OPTS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"data-opts='(\{.+?\})'").unwrap());
/// `1024x576_MP4_…`: the size in a rendition's file name.
static RE_SIZE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/(\d+)x(\d+)_[A-Z0-9]+_").unwrap());

pub fn video_id(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !matches!(
        host.as_str(),
        "dailymail.co.uk" | "www.dailymail.co.uk" | "dailymail.com" | "www.dailymail.com"
    ) {
        return None;
    }
    RE_PATH.captures(url.path()).map(|caps| caps[1].to_string())
}

/// The player options a page carries.
pub fn player_options(html: &str) -> Option<Value> {
    let json = util::search(&RE_OPTS, html)?;
    serde_json::from_str(&util::html_unescape(&json)).ok()
}

/// The width and height a rendition's file name states.
fn size_in_name(url: &Url) -> Option<(u32, u32)> {
    let caps = RE_SIZE.captures(url.path())?;
    Some((caps[1].parse().ok()?, caps[2].parse().ok()?))
}

fn mp4_variant(url: Url, format_id: &str) -> Variant {
    let mut variant = Variant::file(url);
    variant.container = Some(Container::Mp4);
    variant.video = Some(VideoCodec::H264);
    variant.audio = Some(AudioCodec::Aac);
    if let Some((width, height)) = size_in_name(&variant.url) {
        variant.width = Some(width);
        variant.height = Some(height);
        variant.label = Some(format!("{height}p"));
    }
    variant.format_id = Some(format_id.to_string());
    variant
}

pub struct DailymailResolver {
    http: Http,
}

impl DailymailResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for DailymailResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Daily Mail",
            hosts: &["dailymail.com", "dailymail.co.uk"],
            features: &["videos"],
            formats: &["hls", "mp4"],
            media: &[MediaKind::Video],
            tags: &[Tag::News],
            session: SessionSupport::None,
            examples: &[
                "https://www.dailymail.com/video/royals/video-3692517/Video-Meghan-shares-playful-clips-45th-birthday.html",
                "https://www.dailymail.co.uk/video/royals/video-3692517/Video-Meghan-shares-playful-clips-45th-birthday.html",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        video_id(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let id = video_id(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let fetched = fetch(
            &self.http,
            url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        let html = fetched.text();
        let options = player_options(&html).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let sources_url = util::url_of(&options["plugins"]["sources"]["url"], None)
            .or_else(|| util::url_of(&options["sources"]["url"], None))
            .or_else(|| {
                let video_id = util::text(&options["videoId"]).unwrap_or_else(|| id.clone());
                Url::parse(&format!("{SITE}/api/player/{video_id}/video-sources.json")).ok()
            })
            .ok_or_else(|| ResolveError::malformed(url, "the player names no sources"))?;
        let mut resolved = Resolved::new(PLATFORM);
        let mut failure = None;
        let accept = [("accept".to_string(), "application/json".to_string())];
        let sources = fetch(
            &self.http,
            &sources_url,
            PLATFORM,
            BROWSER_UA,
            &accept,
            MAX_PAGE,
        )
        .await?;
        if sources.status.is_success() {
            let listed = sources.json(url)?;
            let renditions = listed["body"]["renditions"]
                .as_array()
                .or_else(|| listed["renditions"].as_array())
                .cloned()
                .unwrap_or_default();
            for rendition in &renditions {
                let Some(link) = util::url_of(&rendition["url"], None) else {
                    continue;
                };
                let bitrate = util::uint(&rendition["encodingRate"]).filter(|b| *b > 0);
                if rendition["videoContainer"].as_str() == Some("M2TS")
                    || link.path().ends_with(".m3u8")
                {
                    match hls::expand(&self.http, &link, PLATFORM, BROWSER_UA, &[]).await {
                        Ok(expanded) => {
                            for mut variant in expanded.variants {
                                if variant.bitrate.is_none() {
                                    variant.bitrate = bitrate;
                                }
                                if let Some((width, height)) = size_in_name(&link) {
                                    variant.width = variant.width.or(Some(width));
                                    variant.height = variant.height.or(Some(height));
                                    variant.label =
                                        variant.label.clone().or(Some(format!("{height}p")));
                                }
                                resolved.variants.push(variant);
                            }
                            resolved.duration = resolved.duration.or(expanded.duration);
                        }
                        Err(error) => failure = Some(error),
                    }
                } else {
                    let mut variant = mp4_variant(
                        link,
                        &format!("http-{}", bitrate.map(|b| b / 1000).unwrap_or(0)),
                    );
                    variant.bitrate = bitrate;
                    // The rendition record names its frame and codec for files whose
                    // names do not.
                    if variant.width.is_none() {
                        variant.width = util::u32_of(&rendition["frameWidth"]).filter(|w| *w > 0);
                    }
                    if variant.height.is_none() {
                        variant.height = util::u32_of(&rendition["frameHeight"]).filter(|h| *h > 0);
                        variant.label = variant.height.map(|h| format!("{h}p"));
                    }
                    if let Some(codec) = rendition["videoCodec"].as_str() {
                        variant.video = Some(match codec.to_ascii_uppercase().as_str() {
                            "H265" | "HEVC" => VideoCodec::H265,
                            "VP9" => VideoCodec::Vp9,
                            "AV1" => VideoCodec::Av1,
                            _ => VideoCodec::H264,
                        });
                    }
                    resolved.variants.push(variant);
                }
            }
        } else if let Some(error) = status_error(sources.status, url)
            && !matches!(sources.status.as_u16(), 404 | 410)
        {
            failure = Some(error);
        }
        if let Some(src) = util::url_of(&options["src"], None)
            && !resolved.variants.iter().any(|v| v.url == src)
        {
            resolved.variants.push(mp4_variant(src, "player"));
        }
        if resolved.variants.is_empty() {
            return Err(failure.unwrap_or_else(|| ResolveError::NotFound(url.clone())));
        }
        resolved
            .variants
            .sort_by_key(|v| std::cmp::Reverse((v.height.unwrap_or(0), v.bitrate.unwrap_or(0))));
        resolved.id = Some(id);
        resolved.title = options["title"]
            .as_str()
            .map(util::html_unescape)
            .and_then(|t| clean_title(&t));
        resolved.description = options["descr"]
            .as_str()
            .map(util::html_unescape)
            .and_then(|d| clean_title(&d));
        resolved.thumbnail = util::url_of(&options["poster"], None)
            .or_else(|| util::url_of(&options["thumbnail"], None));
        resolved.duration = util::millis(&options["duration"]).or(resolved.duration);
        resolved.uploader = options["source"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| Some("Daily Mail".to_string()));
        resolved.webpage_url =
            util::url_of(&options["linkBaseURL"], None).or_else(|| Some(url.clone()));
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};
    use crate::resolve::VariantKind;
    use serde_json::json;

    fn get(url: &str, status: u16, content_type: &str, body: String) -> Exchange {
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
                body: RecordedBody::Text(body),
                truncated: false,
            },
        }
    }

    const VIDEO: &str = "https://www.dailymail.com/video/royals/video-3692517/Video-Meghan-shares-playful-clips-45th-birthday.html";

    #[test]
    fn links_are_read() {
        let id = |s: &str| video_id(&Url::parse(s).unwrap());
        assert_eq!(id(VIDEO), Some("3692517".into()));
        assert_eq!(
            id(
                "http://www.dailymail.co.uk/video/tvshowbiz/video-1295863/The-Mountain-appears-sparkling-water-ad-Heavy-Bubbles.html"
            ),
            Some("1295863".into())
        );
        assert_eq!(
            id("http://www.dailymail.co.uk/embed/video/1295863.html"),
            Some("1295863".into())
        );
        assert_eq!(id("https://www.dailymail.com/news/article-1.html"), None);
        assert_eq!(
            id("https://example.com/video/royals/video-3692517/x.html"),
            None
        );
    }

    #[tokio::test]
    async fn videos_resolve_with_every_rendition() {
        let opts = json!({"linkBaseURL": VIDEO, "duration": 44000, "src": "https://video.dailymail.com/video/mol/2026/08/05/234/1024x576_MP4_234.mp4", "source": "Instagram",
            "title": "Meghan Markle posts clip of her dancing", "descr": "Meghan Markle has posted a new clip &amp; more.", "thumbnail": "https://i.dailymail.com/1s/2026/08/05/01/x.jpg",
            "plugins": {"sources": {"url": "https://www.dailymail.com/api/player/234/video-sources.json"}}, "videoId": "234"});
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            VIDEO,
            200,
            "text/html",
            format!(
                "<html><body><div data-opts='{}'></div></body></html>",
                opts.to_string().replace('\'', "&#39;")
            ),
        ));
        fixture.exchanges.push(get(
            "https://www.dailymail.com/api/player/234/video-sources.json",
            200,
            "application/json",
            json!({"renditions": [
                {"url": "https://video.dailymail.com/video/mol/2026/08/05/234/640x360_M2TS_234.m3u8", "duration": 44000, "frameWidth": 360, "frameHeight": 640, "videoContainer": "M2TS", "encodingRate": 679936, "videoCodec": "H264"},
                {"url": "https://video.dailymail.com/video/mol/2026/08/05/234/480x270_MP4_234.mp4", "duration": 44000, "videoContainer": "MP4", "encodingRate": 372736, "videoCodec": "H264"},
                {"url": "", "videoContainer": "MP4"}
            ]}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://video.dailymail.com/video/mol/2026/08/05/234/640x360_M2TS_234.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\n0.ts\n#EXT-X-ENDLIST\n".into(),
        ));
        let resolver = DailymailResolver::new(Http::replay(fixture));
        let url = Url::parse(VIDEO).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("3692517"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Meghan Markle posts clip of her dancing")
        );
        assert_eq!(
            resolved.description.as_deref(),
            Some("Meghan Markle has posted a new clip & more.")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("Instagram"));
        assert_eq!(resolved.duration, Some(Duration::from_millis(44000)));
        assert_eq!(resolved.variants.len(), 3);
        let best = &resolved.variants[0];
        assert_eq!(best.kind, VariantKind::File);
        assert_eq!((best.width, best.height), (Some(1024), Some(576)));
        assert_eq!(best.format_id.as_deref(), Some("player"));
        let playlist = resolved
            .variants
            .iter()
            .find(|v| v.kind == VariantKind::Hls)
            .unwrap();
        assert_eq!(playlist.height, Some(360));
        assert_eq!(playlist.bitrate, Some(679936));
        let small = resolved
            .variants
            .iter()
            .find(|v| v.format_id.as_deref() == Some("http-372"))
            .unwrap();
        assert_eq!(small.height, Some(270));
    }

    #[tokio::test]
    async fn pages_without_a_player_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.dailymail.com/embed/video/1.html",
            200,
            "text/html",
            "<html><body>no player</body></html>".into(),
        ));
        let resolver = DailymailResolver::new(Http::replay(fixture));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.dailymail.com/embed/video/1.html").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
