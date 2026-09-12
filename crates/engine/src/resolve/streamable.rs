//! Streamable videos, through the API the player reads.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Variant,
    VariantKind, clean_title, fetch,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "streamable";
const API: &str = "https://ajax.streamable.com/videos/";

/// `/{code}`, `/e/{code}`, `/o/{code}`, `/t/{code}`, `/s/{code}/{token}`.
static RE_CODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(?:[eost]/)?([A-Za-z0-9]{3,})(?:/|$)").unwrap());

/// The short code a link names.
pub fn video_id(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "streamable.com" && host != "www.streamable.com" {
        return None;
    }
    RE_CODE
        .captures(url.path())
        .map(|c| c[1].to_ascii_lowercase())
}

/// Streamable hands out `//cdn…` links without a scheme.
fn absolute(raw: &str) -> Option<Url> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }
    if text.starts_with("//") {
        Url::parse(&format!("https:{text}")).ok()
    } else {
        Url::parse(text).ok()
    }
}

fn video_codec(name: &str) -> Option<VideoCodec> {
    match name.to_ascii_lowercase().as_str() {
        "" => None,
        "h264" | "avc" | "avc1" => Some(VideoCodec::H264),
        "hevc" | "h265" => Some(VideoCodec::H265),
        "vp8" => Some(VideoCodec::Vp8),
        "vp9" => Some(VideoCodec::Vp9),
        "av1" => Some(VideoCodec::Av1),
        other => Some(VideoCodec::Other(other.to_string())),
    }
}

fn audio_codec(name: &str) -> Option<AudioCodec> {
    match name.to_ascii_lowercase().as_str() {
        "" => None,
        "aac" => Some(AudioCodec::Aac),
        "opus" => Some(AudioCodec::Opus),
        "vorbis" => Some(AudioCodec::Vorbis),
        "mp3" => Some(AudioCodec::Mp3),
        other => Some(AudioCodec::Other(other.to_string())),
    }
}

pub struct StreamableResolver {
    http: Http,
}

impl StreamableResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

/// The variants a video's `files` make: `mp4`, `mp4-mobile` and whatever else has a link.
pub fn variants_of(video: &Value) -> Vec<Variant> {
    let fallback_duration = video["duration"]
        .as_f64()
        .filter(|d| *d > 0.0)
        .map(Duration::from_secs_f64);
    let mut variants = Vec::new();
    for (key, file) in video["files"].as_object().into_iter().flatten() {
        let Some(url) = file["url"].as_str().and_then(absolute) else {
            continue;
        };
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(Container::Mp4);
        v.video = file["input_metadata"]["video_codec_name"]
            .as_str()
            .and_then(video_codec)
            .or(Some(VideoCodec::H264));
        v.audio = file["input_metadata"]["audio_codec_name"]
            .as_str()
            .and_then(audio_codec)
            .or(Some(AudioCodec::Aac));
        v.width = file["width"].as_u64().filter(|w| *w > 0).map(|w| w as u32);
        v.height = file["height"].as_u64().filter(|h| *h > 0).map(|h| h as u32);
        v.size = file["size"].as_u64().filter(|s| *s > 0);
        v.fps = file["framerate"].as_f64().filter(|f| *f > 0.0);
        v.bitrate = file["bitrate"].as_u64().filter(|b| *b > 0);
        v.duration = file["duration"]
            .as_f64()
            .filter(|d| *d > 0.0)
            .map(Duration::from_secs_f64)
            .or(fallback_duration);
        v.format_id = Some(key.clone());
        v.label = v.height.map(|h| format!("{h}p"));
        variants.push(v);
    }
    variants
}

#[async_trait]
impl Resolver for StreamableResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Streamable",
            hosts: &["streamable.com"],
            features: &["videos", "embeds", "share links"],
            formats: &["mp4"],
            session: SessionSupport::None,
            examples: &[
                "https://streamable.com/moo",
                "https://streamable.com/e/dnd1",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        video_id(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let code = video_id(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let api = Url::parse(&format!("{API}{code}")).expect("valid");
        let fetched = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        match fetched.status.as_u16() {
            200..=299 => {}
            404 | 410 => return Err(ResolveError::NotFound(url.clone())),
            429 => return Err(ResolveError::RateLimited(url.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    url,
                    format!("the API answered HTTP {status}"),
                ));
            }
        }
        let video = fetched.json(url)?;
        match video["status"].as_i64() {
            Some(2) => {}
            Some(0) | Some(1) => {
                return Err(ResolveError::unavailable(
                    url,
                    "the video is still uploading or processing",
                ));
            }
            Some(3) => {
                return Err(ResolveError::unavailable(
                    url,
                    "the video failed processing",
                ));
            }
            other => {
                return Err(ResolveError::unavailable(
                    url,
                    format!("the video is not ready (status {other:?})"),
                ));
            }
        }
        let variants = variants_of(&video);
        if variants.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(code.clone());
        resolved.title = video["reddit_title"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| video["title"].as_str().and_then(clean_title));
        resolved.description = video["description"].as_str().and_then(clean_title);
        resolved.uploader = video["owner"]["user_name"].as_str().and_then(clean_title);
        resolved.uploaded_at = video["date_added"]
            .as_f64()
            .filter(|t| *t > 0.0)
            .and_then(|t| Timestamp::from_second(t as i64).ok());
        resolved.duration = video["duration"]
            .as_f64()
            .filter(|d| *d > 0.0)
            .map(Duration::from_secs_f64)
            .or_else(|| variants.iter().find_map(|v| v.duration));
        resolved.thumbnail = video["thumbnail_url"]
            .as_str()
            .and_then(absolute)
            .or_else(|| video["poster_url"].as_str().and_then(absolute));
        resolved.webpage_url = Url::parse(&format!("https://streamable.com/{code}")).ok();
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };
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
        assert_eq!(id("https://streamable.com/moo"), Some("moo".into()));
        assert_eq!(
            id("https://streamable.com/e/dnd1?loop=0"),
            Some("dnd1".into())
        );
        assert_eq!(
            id("https://streamable.com/s/AbC12/xyz"),
            Some("abc12".into())
        );
        assert_eq!(id("https://streamable.com/o/dnd1"), Some("dnd1".into()));
        assert_eq!(id("https://streamable.com/"), None);
        assert_eq!(id("https://example.com/moo"), None);
    }

    #[tokio::test]
    async fn ready_videos_resolve_with_every_file() {
        let mut fixture = Fixture::new("streamable", None);
        fixture.exchanges.push(get("https://ajax.streamable.com/videos/dnd1", 200, "application/json", json!({
            "status": 2, "title": "", "reddit_title": "me irl", "duration": 61.516, "date_added": 1454964157.351,
            "thumbnail_url": "//cdn-cf-west.streamable.com/image/dnd1.jpg?Expires=1", "owner": {"user_name": "someone"},
            "files": {
                "mp4": {"url": "//cdn-cf-west.streamable.com/video/mp4/dnd1.mp4?Expires=1", "width": 1280, "height": 720, "size": 19286494, "duration": 61.516, "framerate": 30, "bitrate": 2374146, "status": 2, "input_metadata": {"video_codec_name": "h264", "audio_codec_name": "aac"}},
                "mp4-mobile": {"url": "https://cdn-cf-west.streamable.com/video/mp4-mobile/dnd1.mp4?Expires=1", "width": 640, "height": 360, "size": 12171177, "duration": 61.5, "framerate": 30, "bitrate": 1512284, "status": 2},
                "original": {"url": "", "width": 1280, "height": 720, "size": 16993797, "status": null}
            }
        }).to_string()));
        let resolver = StreamableResolver::new(Http::replay(fixture));
        let url = Url::parse("https://streamable.com/e/dnd1").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("dnd1"));
        assert_eq!(resolved.title.as_deref(), Some("me irl"));
        assert_eq!(resolved.uploader.as_deref(), Some("someone"));
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(61.516)));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://cdn-cf-west.streamable.com/image/dnd1.jpg?Expires=1"
        );
        assert_eq!(resolved.variants.len(), 2);
        let best = &resolved.variants[0];
        assert_eq!(
            best.url.as_str(),
            "https://cdn-cf-west.streamable.com/video/mp4/dnd1.mp4?Expires=1"
        );
        assert_eq!(best.height, Some(720));
        assert_eq!(best.size, Some(19286494));
        assert_eq!(best.fps, Some(30.0));
        assert_eq!(best.video, Some(VideoCodec::H264));
        assert_eq!(best.format_id.as_deref(), Some("mp4"));
        assert_eq!(resolved.variants[1].label.as_deref(), Some("360p"));
    }

    #[tokio::test]
    async fn missing_and_unfinished_videos_say_so() {
        let mut fixture = Fixture::new("streamable", None);
        fixture.exchanges.push(get(
            "https://ajax.streamable.com/videos/gone",
            404,
            "text/plain",
            "Video does not exist".into(),
        ));
        fixture.exchanges.push(get(
            "https://ajax.streamable.com/videos/busy",
            200,
            "application/json",
            json!({"status": 1, "title": "soon", "files": {}}).to_string(),
        ));
        let resolver = StreamableResolver::new(Http::replay(fixture));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://streamable.com/gone").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(&Url::parse("https://streamable.com/busy").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("processing")),
            "{error}"
        );
    }
}
