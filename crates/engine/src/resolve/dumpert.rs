//! Dumpert (dumpert.nl) items: the site's mobile API describes an item with one media
//! entry per video, each listing its renditions. The site's edge answers anything but a
//! browser's TLS fingerprint with a bot check, so the API is read as Chrome.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use url::Url;

use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Tag, Variant,
    clean_title, fetch_as_browser, hls, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "dumpert";
const API: &str = "https://api-live.dumpert.nl/mobile_api/json/info/";

/// `/item/{id}_{hash}`, `/embed/{id}_{hash}`, `/mediabase/{id}/{hash}` on the legacy
/// site, or any page with `?selectedId={id}_{hash}`.
static RE_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^/(?:item|embed)/(\d+)[_/]([0-9a-f]+)|^/mediabase/(\d+)/([0-9a-f]+)").unwrap()
});
static RE_SELECTED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d+)_([0-9a-f]+)$").unwrap());

pub fn item_id(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !matches!(
        host.as_str(),
        "dumpert.nl" | "www.dumpert.nl" | "legacy.dumpert.nl"
    ) {
        return None;
    }
    if let Some(caps) = RE_PATH.captures(url.path()) {
        let (number, hash) = match (caps.get(1), caps.get(3)) {
            (Some(n), _) => (n.as_str(), caps.get(2)?.as_str()),
            (None, Some(n)) => (n.as_str(), caps.get(4)?.as_str()),
            _ => return None,
        };
        return Some(format!("{number}_{hash}"));
    }
    let selected = util::query_param(url, "selectedId")?;
    RE_SELECTED
        .captures(&selected)
        .map(|caps| format!("{}_{}", &caps[1], &caps[2]))
}

/// The order the site's renditions rank in.
fn quality_rank(version: &str) -> u8 {
    match version {
        "1080p" => 5,
        "720p" => 4,
        "tablet" => 3,
        "mobile" => 2,
        "flv" => 1,
        _ => 0,
    }
}

pub struct DumpertResolver {
    http: Http,
}

impl DumpertResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for DumpertResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Dumpert",
            hosts: &["dumpert.nl"],
            features: &["videos"],
            formats: &["mp4", "hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::Video],
            session: SessionSupport::None,
            examples: &[
                "https://www.dumpert.nl/item/100031688_b317a185",
                "https://www.dumpert.nl/toppers?selectedId=100031688_b317a185",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        item_id(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let id = item_id(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let api = Url::parse(&format!("{API}{id}")).expect("valid");
        let accept = [("accept".to_string(), "application/json".to_string())];
        let fetched = fetch_as_browser(&self.http, &api, PLATFORM, &accept, MAX_PAGE).await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        let answer = fetched.json(url)?;
        let Some(item) = answer["items"]
            .as_array()
            .and_then(|items| items.first())
            .cloned()
        else {
            return Err(ResolveError::NotFound(url.clone()));
        };
        let Some(media) = item["media"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|m| m["mediatype"].as_str() == Some("VIDEO"))
            .cloned()
        else {
            return Err(ResolveError::unavailable(url, "the item carries no video"));
        };
        let mut resolved = Resolved::new(PLATFORM);
        let mut failure = None;
        let mut seen = std::collections::HashSet::new();
        for rendition in media["variants"].as_array().into_iter().flatten() {
            let Some(link) = util::url_of(&rendition["uri"], None) else {
                continue;
            };
            // Stills, thumbnails and the thumbnail track are listed beside the video.
            if matches!(
                super::path_extension(&link).as_deref(),
                Some("png" | "jpg" | "jpeg" | "webp" | "gif" | "vtt")
            ) || !seen.insert(link.to_string())
            {
                continue;
            }
            let version = rendition["version"].as_str().unwrap_or("").to_string();
            let version_height: Option<u32> =
                version.strip_suffix('p').and_then(|h| h.parse().ok());
            if link.path().ends_with(".m3u8") {
                match hls::expand(&self.http, &link, PLATFORM, BROWSER_UA, &[]).await {
                    Ok(expanded) => {
                        for mut variant in expanded.variants {
                            if variant.height.is_none() {
                                variant.height = version_height;
                            }
                            if variant.label.is_none() {
                                variant.label = Some(version.clone());
                            }
                            variant.format_id = Some(format!("hls-{}", version));
                            resolved.variants.push(variant);
                        }
                        resolved.subtitles.extend(expanded.subtitles);
                        resolved.duration = resolved.duration.or(expanded.duration);
                    }
                    Err(error) => failure = Some(error),
                }
            } else {
                let mut variant = Variant::file(link);
                variant.container = Some(Container::Mp4);
                variant.video = Some(VideoCodec::H264);
                variant.audio = Some(AudioCodec::Aac);
                variant.height = version_height;
                variant.label = Some(version.clone());
                variant.format_id = Some(version.clone());
                resolved.variants.push(variant);
            }
        }
        // The master playlist's renditions are the per-version playlists again; keep
        // each once, as the master describes it.
        let mut kept: Vec<Variant> = Vec::new();
        for variant in resolved.variants.drain(..) {
            match kept.iter_mut().find(|k| k.url == variant.url) {
                Some(existing) => {
                    if existing.width.is_none() && variant.width.is_some() {
                        *existing = variant;
                    }
                }
                None => kept.push(variant),
            }
        }
        resolved.variants = kept;
        if resolved.variants.is_empty() {
            return Err(failure.unwrap_or_else(|| ResolveError::NotFound(url.clone())));
        }
        resolved.variants.sort_by_key(|v| {
            std::cmp::Reverse((
                v.height.unwrap_or(0),
                quality_rank(v.format_id.as_deref().unwrap_or("")),
            ))
        });
        resolved.id = Some(id.clone());
        resolved.title = item["title"].as_str().and_then(clean_title);
        resolved.description = item["description"]
            .as_str()
            .map(util::clean_html)
            .and_then(|d| clean_title(&d));
        resolved.thumbnail = [
            "still-large",
            "still-medium",
            "still",
            "thumb-large",
            "thumb-medium",
            "thumb",
        ]
        .iter()
        .find_map(|key| util::url_of(&item["stills"][*key], None));
        resolved.duration = util::seconds(&media["duration"]).or(resolved.duration);
        resolved.uploaded_at = util::time(&item["date"]);
        resolved.uploader = item["uploader"]["name"]
            .as_str()
            .or(item["uploader"].as_str())
            .and_then(clean_title);
        resolved.webpage_url = Url::parse(&format!("https://www.dumpert.nl/item/{id}")).ok();
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

    #[test]
    fn links_are_read() {
        let id = |s: &str| item_id(&Url::parse(s).unwrap());
        assert_eq!(
            id("https://www.dumpert.nl/item/6646981_951bc60f"),
            Some("6646981_951bc60f".into())
        );
        assert_eq!(
            id("https://www.dumpert.nl/embed/6675421_dc440fe7"),
            Some("6675421_dc440fe7".into())
        );
        assert_eq!(
            id("http://legacy.dumpert.nl/mediabase/6646981/951bc60f"),
            Some("6646981_951bc60f".into())
        );
        assert_eq!(
            id("http://legacy.dumpert.nl/embed/6675421/dc440fe7"),
            Some("6675421_dc440fe7".into())
        );
        assert_eq!(
            id("https://www.dumpert.nl/toppers?selectedId=100031688_b317a185"),
            Some("100031688_b317a185".into())
        );
        assert_eq!(
            id("https://www.dumpert.nl/?selectedId=100031688_b317a185"),
            Some("100031688_b317a185".into())
        );
        assert_eq!(id("https://www.dumpert.nl/toppers/dag"), None);
        assert_eq!(id("https://example.com/item/6646981_951bc60f"), None);
    }

    #[tokio::test]
    async fn items_resolve_to_their_renditions() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://api-live.dumpert.nl/mobile_api/json/info/100031688_b317a185",
            200,
            "application/json",
            json!({"items": [{"id": "100031688_b317a185", "title": "Die zag je niet", "description": "<p>Die zag je niet eh</p>", "date": "2022-05-26T21:12:59+02:00",
                "stills": {"still": "https://media.dumpert.nl/still.jpg", "still-large": "https://media.dumpert.nl/still-large.jpg"},
                "media": [{"mediatype": "VIDEO", "duration": 12, "variants": [
                    {"version": "720p", "uri": "https://media.dumpert.nl/dmp/media/video/c/720p.mp4"},
                    {"version": "mobile", "uri": "https://media.dumpert.nl/dmp/media/video/c/mobile.mp4"},
                    {"version": "hls", "uri": "https://media.dumpert.nl/dmp/media/video/c/master.m3u8"},
                    {"version": "still", "uri": "https://media.dumpert.nl/dmp/media/video/c/poster.jpg"},
                    {"version": "thumbrail", "uri": "https://media.dumpert.nl/dmp/media/video/c/thumbs.vtt"},
                    {"version": "720p", "uri": "https://media.dumpert.nl/dmp/media/video/c/720p.mp4"},
                    {"version": "broken"}
                ]}]}]}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://media.dumpert.nl/dmp/media/video/c/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1920x1080\n1080.m3u8\n".into(),
        ));
        fixture.exchanges.push(get(
            "https://media.dumpert.nl/dmp/media/video/c/1080.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\n0.ts\n#EXT-X-ENDLIST\n".into(),
        ));
        fixture.exchanges.push(get(
            "https://api-live.dumpert.nl/mobile_api/json/info/1_00",
            200,
            "application/json",
            json!({"items": []}).to_string(),
        ));
        let resolver = DumpertResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.dumpert.nl/item/100031688_b317a185").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("100031688_b317a185"));
        assert_eq!(resolved.title.as_deref(), Some("Die zag je niet"));
        assert_eq!(resolved.description.as_deref(), Some("Die zag je niet eh"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(12)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1653592379)
        );
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://media.dumpert.nl/still-large.jpg"
        );
        assert_eq!(resolved.variants.len(), 3);
        assert_eq!(resolved.variants[0].kind, VariantKind::Hls);
        assert_eq!(resolved.variants[0].height, Some(1080));
        assert_eq!(resolved.variants[1].format_id.as_deref(), Some("720p"));
        assert_eq!(resolved.variants[1].height, Some(720));
        assert_eq!(resolved.variants[2].format_id.as_deref(), Some("mobile"));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.dumpert.nl/item/1_00").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
