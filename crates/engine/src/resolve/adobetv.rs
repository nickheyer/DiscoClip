//! Adobe TV (video.tv.adobe.com): every video answers a JSON description of itself at
//! `/v/{id}?format=json`, listing its HLS renditions, or the H.264 files of an older
//! video, and its caption tracks.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport,
    SubtitleFormat, SubtitleTrack, Tag, Variant, VariantKind, clean_title, fetch, manifests,
    path_extension, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "adobetv";
const SITE: &str = "https://video.tv.adobe.com/v/";

/// `/v/{id}` with an optional slug.
static RE_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/v/(\d+)(?:/|$)").unwrap());
/// Players embedded in other Adobe pages.
static RE_EMBED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:https?:)?//video\.tv\.adobe\.com/v/(\d+)(?:[/?#][^"'\s<>]*)?"#).unwrap()
});

pub fn video_id(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    if !url.host_str()?.eq_ignore_ascii_case("video.tv.adobe.com") {
        return None;
    }
    RE_PATH.captures(url.path()).map(|caps| caps[1].to_string())
}

/// A link the site writes without a scheme.
fn site_url(value: &Value) -> Option<Url> {
    let text = value.as_str()?.trim();
    let text = if text.starts_with("//") {
        format!("https:{text}")
    } else {
        text.to_string()
    };
    Url::parse(&text).ok()
}

/// A source entry as a variant, an HLS rendition or an H.264 file, passing over the master playlist entry
fn source_variant(source: &Value) -> Option<Variant> {
    if source["format"].as_str() == Some("playlist") {
        return None;
    }
    let url = site_url(&source["src"])?;
    let mut variant = match path_extension(&url).as_deref() {
        Some("m3u8") => Variant::hls(url),
        Some("mp4") => Variant::file(url),
        _ => return None,
    };
    variant.container = Some(Container::Mp4);
    variant.video = Some(VideoCodec::H264);
    variant.audio = Some(AudioCodec::Aac);
    variant.width = util::u32_of(&source["width"]).filter(|w| *w > 0);
    variant.height = util::u32_of(&source["height"]).filter(|h| *h > 0);
    variant.bitrate = util::uint(&source["bitrate"])
        .filter(|b| *b > 0)
        .map(|kbps| kbps * 1000);
    variant.duration = util::millis(&source["duration"]);
    variant.size = util::uint(&source["kilobytes"]).map(|kb| kb * 1000);
    let label = source["label"].as_str().unwrap_or("").trim();
    variant.label = variant
        .height
        .map(|h| format!("{h}p"))
        .or_else(|| (!label.is_empty()).then(|| label.to_string()));
    variant.format_id = Some(
        [
            source["format"].as_str(),
            (!label.is_empty()).then_some(label),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("-"),
    );
    Some(variant)
}

pub struct AdobetvResolver {
    http: Http,
}

impl AdobetvResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for AdobetvResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Adobe TV",
            hosts: &["video.tv.adobe.com"],
            features: &["videos"],
            formats: &["hls", "mp4"],
            media: &[MediaKind::Video],
            tags: &[Tag::Video],
            session: SessionSupport::None,
            on_by_default: true,
            examples: &[
                "https://video.tv.adobe.com/v/3463980/adobe-acrobat",
                "https://video.tv.adobe.com/v/2456",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        video_id(url).is_some()
    }

    fn embeds_in(&self, page: &Page) -> Vec<Url> {
        let mut seen = std::collections::HashSet::new();
        RE_EMBED
            .captures_iter(page.html())
            .filter(|caps| seen.insert(caps[1].to_string()))
            .filter_map(|caps| Url::parse(&format!("{SITE}{}", &caps[1])).ok())
            .collect()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let id = video_id(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let api = Url::parse(&format!("{SITE}{id}?format=json")).expect("valid");
        let accept = [("accept".to_string(), "application/json".to_string())];
        let fetched = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &accept, MAX_PAGE).await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        let data = fetched.json(url)?;
        let mut variants: Vec<Variant> = data["sources"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(source_variant)
            .collect();
        // One family, the renditions the player streams, or the files an older video only has
        if variants.iter().any(|v| v.kind == VariantKind::Hls) {
            variants.retain(|v| v.kind == VariantKind::Hls);
        }
        if variants.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let mut subtitles: Vec<SubtitleTrack> = data["translations"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|translation| {
                let vtt = site_url(&translation["vttPath"])?;
                let language = translation["language_w3c"]
                    .as_str()
                    .map(str::to_string)
                    .or_else(|| {
                        translation["language_medium"]
                            .as_str()
                            .map(|code| util::iso639_short(code).unwrap_or(code).to_string())
                    })
                    .unwrap_or_else(|| "und".to_string());
                Some(SubtitleTrack {
                    url: vtt,
                    language,
                    name: translation["language_name"]
                        .as_str()
                        .or(translation["language"].as_str())
                        .map(str::to_string),
                    format: SubtitleFormat::Vtt,
                    auto: false,
                    headers: Vec::new(),
                })
            })
            .collect();
        let duration = variants.iter().find_map(|v| v.duration);
        let variants =
            manifests::expand_all(&self.http, PLATFORM, variants, &mut subtitles, duration).await;
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.clone());
        resolved.title = data["title"]
            .as_str()
            .map(util::clean_html)
            .and_then(|t| clean_title(&t));
        resolved.description = data["description"]
            .as_str()
            .map(util::clean_html)
            .and_then(|d| clean_title(&d));
        resolved.thumbnail = site_url(&data["video"]["poster"]);
        resolved.duration = duration.or_else(|| variants.iter().find_map(|v| v.duration));
        resolved.uploader = Some("Adobe".to_string());
        resolved.webpage_url = Url::parse(&format!("{SITE}{id}")).ok();
        resolved.subtitles = subtitles;
        resolved.variants = variants;
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
        let id = |s: &str| video_id(&Url::parse(s).unwrap());
        assert_eq!(id("https://video.tv.adobe.com/v/2456"), Some("2456".into()));
        assert_eq!(
            id("https://video.tv.adobe.com/v/3463980/adobe-acrobat"),
            Some("3463980".into())
        );
        assert_eq!(
            id("https://video.tv.adobe.com/v/3463980?quality=12"),
            Some("3463980".into())
        );
        assert_eq!(id("https://video.tv.adobe.com/"), None);
        assert_eq!(id("https://tv.adobe.com/v/2456"), None);
    }

    #[test]
    fn embedded_players_are_found() {
        let resolver = AdobetvResolver::new(Http::replay(Fixture::new(PLATFORM, None)));
        let page = Page::parse(
            r#"<iframe src="https://video.tv.adobe.com/v/3442499/?quality=12&learn=on"></iframe><a href="//video.tv.adobe.com/v/3442499">again</a><iframe src="https://video.tv.adobe.com/v/2456"></iframe>"#,
            &Url::parse("https://business.adobe.com/summit/2025/S335.html").unwrap(),
        );
        let embeds: Vec<String> = resolver
            .embeds_in(&page)
            .into_iter()
            .map(|u| u.to_string())
            .collect();
        assert_eq!(
            embeds,
            vec![
                "https://video.tv.adobe.com/v/3442499",
                "https://video.tv.adobe.com/v/2456"
            ]
        );
    }

    /// An older video with H.264 and VP8 files and no playlist, the H.264 files are the family
    #[tokio::test]
    async fn older_videos_resolve_to_their_files() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://video.tv.adobe.com/v/2456?format=json",
            200,
            "application/json",
            json!({
                "title": "New experience with Acrobat DC", "description": "",
                "video": {"poster": "//images-tv.adobe.com/avp/vr/a/poster.jpg"},
                "sources": [
                    {"format": "webm", "label": "HD", "width": 1280, "height": 720, "bitrate": 474, "kilobytes": 19779, "duration": 248457, "src": "https://images-tv.adobe.com/avp/vr/a/b.1280x720at2400_vp8.webm"},
                    {"format": "mpeg4", "label": "HD", "width": 1280, "height": 720, "bitrate": 1036, "kilobytes": 36235, "duration": 248522, "src": "https://images-tv.adobe.com/avp/vr/a/b.1280x720at2400_h264.mp4"},
                    {"format": "mpeg4", "label": "Low", "width": 640, "height": 360, "bitrate": 239, "kilobytes": 8926, "duration": 248667, "src": "https://images-tv.adobe.com/avp/vr/a/b.640x360at320_h264.mp4"}
                ],
                "translations": []
            })
            .to_string(),
        ));
        let resolver = AdobetvResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://video.tv.adobe.com/v/2456").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(
            resolved.title.as_deref(),
            Some("New experience with Acrobat DC")
        );
        assert_eq!(resolved.duration, Some(Duration::from_millis(248522)));
        crate::resolve::assert_one_family(&resolved.variants);
        assert_eq!(resolved.variants.len(), 2);
        let best = &resolved.variants[0];
        assert_eq!(best.kind, VariantKind::File);
        assert_eq!(best.height, Some(720));
        assert_eq!(best.size, Some(36_235_000));
        assert_eq!(best.format_id.as_deref(), Some("mpeg4-HD"));
        assert!(best.url.as_str().ends_with("_h264.mp4"));
        assert_eq!(resolved.variants[1].height, Some(360));
    }

    #[tokio::test]
    async fn videos_resolve_with_renditions_and_captions() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://video.tv.adobe.com/v/3463980?format=json",
            200,
            "application/json",
            json!({
                "viewBucketID": "3463980", "title": "Adobe Acrobat: How to Customize the <b>Toolbar</b>", "description": "Learn how.",
                "video": {"poster": "//images-tv.adobe.com/mpcv3/x/poster.jpg"},
                "sources": [
                    {"label": "1080p", "width": 1920, "height": 1080, "bitrate": 922, "format": "mpeg-ts", "duration": 97514, "kilobytes": 12693, "src": "https://images-tv.adobe.com/mpcv3/1059/a.1920x1080at3000_h264.m3u8"},
                    {"label": "HLS", "format": "playlist", "src": "https://images-tv.adobe.com/mpcv3/1059/playlist.m3u8"},
                    {"label": "Medium", "width": 854, "height": 480, "bitrate": 800, "format": "mpeg4", "duration": 97514, "kilobytes": 9000, "src": "//images-tv.adobe.com/mpcv3/1059/a.854x480at800_h264.mp4"},
                    {"label": "Medium", "width": 854, "height": 480, "bitrate": 800, "format": "mpeg-ts", "src": "https://images-tv.adobe.com/mpcv3/1059/a.854x480at800_h264.m3u8"},
                    {"label": "broken", "format": "mpeg4"}
                ],
                "translations": [
                    {"language": "English", "language_name": "English", "language_medium": "eng", "language_w3c": "en-US", "vttPath": "//video.tv.adobe.com/vc/x/eng.vtt"},
                    {"language": "German", "language_medium": "deu", "vttPath": "//video.tv.adobe.com/vc/x/deu.vtt"},
                    {"language": "None", "vttPath": ""}
                ]
            })
            .to_string(),
        ));
        fixture.exchanges.push(get(
            "https://video.tv.adobe.com/v/1?format=json",
            404,
            "text/html",
            "gone".into(),
        ));
        let resolver = AdobetvResolver::new(Http::replay(fixture));
        let url = Url::parse("https://video.tv.adobe.com/v/3463980/adobe-acrobat").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("3463980"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Adobe Acrobat: How to Customize the Toolbar")
        );
        assert_eq!(resolved.description.as_deref(), Some("Learn how."));
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://images-tv.adobe.com/mpcv3/x/poster.jpg"
        );
        assert_eq!(resolved.duration, Some(Duration::from_millis(97514)));
        crate::resolve::assert_one_family(&resolved.variants);
        assert_eq!(resolved.variants.len(), 2);
        let best = &resolved.variants[0];
        assert_eq!(best.kind, VariantKind::Hls);
        assert_eq!(best.height, Some(1080));
        assert_eq!(best.bitrate, Some(922_000));
        assert_eq!(best.size, Some(12_693_000));
        assert_eq!(best.format_id.as_deref(), Some("mpeg-ts-1080p"));
        assert_eq!(resolved.variants[1].kind, VariantKind::Hls);
        assert_eq!(resolved.variants[1].height, Some(480));
        assert_eq!(resolved.subtitles.len(), 2);
        assert_eq!(resolved.subtitles[0].language, "en-US");
        assert_eq!(resolved.subtitles[1].language, "de");
        assert_eq!(
            resolved.subtitles[1].url.as_str(),
            "https://video.tv.adobe.com/vc/x/deu.vtt"
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://video.tv.adobe.com/v/1").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
