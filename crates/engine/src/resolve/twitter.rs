//! Twitter and X posts, read through the fxtwitter JSON API: the embed-friendly mirror
//! that needs no login. The native GraphQL resolver in `x.rs` takes precedence for hosts
//! it handles when it is registered before this one.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use url::Url;

use super::{
    Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Variant, VariantKind,
    check_status, clean_title,
};
use crate::http::{APP_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

const MAX_JSON: usize = 2 * 1024 * 1024;
const HOSTS: [&str; 10] = [
    "twitter.com",
    "www.twitter.com",
    "mobile.twitter.com",
    "x.com",
    "www.x.com",
    "mobile.x.com",
    "fxtwitter.com",
    "vxtwitter.com",
    "fixupx.com",
    "fixvx.com",
];

static RE_STATUS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/status(?:es)?/(\d{5,})").unwrap());
static RE_DIMS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/(\d{2,5})x(\d{2,5})/").unwrap());

pub struct TwitterResolver {
    http: Http,
}

impl TwitterResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

pub fn status_id(url: &Url) -> Option<String> {
    RE_STATUS.captures(url.path()).map(|c| c[1].to_string())
}

/// The `WxH` a video URL's path names.
pub fn dimensions(url: &Url) -> Option<(u32, u32)> {
    RE_DIMS
        .captures(url.path())
        .and_then(|c| Some((c[1].parse::<u32>().ok()?, c[2].parse::<u32>().ok()?)))
}

#[async_trait]
impl Resolver for TwitterResolver {
    fn id(&self) -> &'static str {
        "twitter"
    }

    fn platform(&self) -> Platform {
        Platform {
            id: "twitter",
            name: "Twitter / X (fxtwitter)",
            hosts: &[
                "twitter.com",
                "x.com",
                "fxtwitter.com",
                "vxtwitter.com",
                "fixupx.com",
                "fixvx.com",
            ],
            features: &["videos", "gifs"],
            formats: &["mp4", "hls"],
            session: SessionSupport::None,
            examples: &["https://x.com/SpaceX/status/1732824684683784516"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some_and(|h| HOSTS.contains(&h))
            && status_id(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let id = status_id(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let api = Url::parse(&format!("https://api.fxtwitter.com/status/{id}"))
            .map_err(|e| ResolveError::malformed(url, e.to_string()))?;
        let response = self
            .http
            .get(api)
            .platform("twitter")
            .user_agent(APP_UA)
            .send()
            .await?;
        let status = response.status;
        let body = response.bytes(MAX_JSON).await?;
        let value: serde_json::Value = serde_json::from_slice(&body)
            .map_err(|e| ResolveError::malformed(url, format!("API JSON: {e}")))?;
        let code = value
            .get("code")
            .and_then(|c| c.as_u64())
            .unwrap_or(status.as_u16() as u64);
        if code == 404 {
            return Err(ResolveError::NotFound(url.clone()));
        }
        if code != 200 {
            let message = value
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("API error")
                .to_string();
            return Err(ResolveError::unavailable(
                url,
                format!("{message} ({code})"),
            ));
        }
        if !status.is_success() {
            return Err(super::status_error(status, url).expect("non-success status"));
        }
        let tweet = value
            .get("tweet")
            .ok_or_else(|| ResolveError::malformed(url, "API JSON has no tweet"))?;
        let author = tweet
            .pointer("/author/name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let handle = tweet
            .pointer("/author/screen_name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let text = tweet.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let title = clean_title(&format!("{author} @{handle} {text}"));

        let videos = tweet
            .pointer("/media/videos")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let video = videos
            .iter()
            .find(|v| {
                v.get("variants")
                    .and_then(|x| x.as_array())
                    .is_some_and(|a| !a.is_empty())
            })
            .or_else(|| videos.first())
            .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let duration = video
            .get("duration")
            .and_then(|d| d.as_f64())
            .filter(|d| *d > 0.0)
            .map(Duration::from_secs_f64);
        let width = video
            .get("width")
            .and_then(|w| w.as_u64())
            .map(|w| w as u32);
        let height = video
            .get("height")
            .and_then(|h| h.as_u64())
            .map(|h| h as u32);

        let mut variants = Vec::new();
        for entry in video
            .get("variants")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
        {
            let Some(raw) = entry.get("url").and_then(|u| u.as_str()) else {
                continue;
            };
            let Ok(variant_url) = Url::parse(raw) else {
                continue;
            };
            let content_type = entry
                .get("content_type")
                .and_then(|c| c.as_str())
                .unwrap_or("");
            let bitrate = entry
                .get("bitrate")
                .and_then(|b| b.as_u64())
                .filter(|b| *b > 0);
            let dims = RE_DIMS
                .captures(variant_url.path())
                .and_then(|c| Some((c[1].parse::<u32>().ok()?, c[2].parse::<u32>().ok()?)));
            if content_type == "video/mp4" {
                let mut v = Variant::new(variant_url, VariantKind::File);
                v.container = Some(Container::Mp4);
                v.video = Some(VideoCodec::H264);
                v.audio = Some(AudioCodec::Aac);
                v.bitrate = bitrate;
                v.duration = duration;
                match dims {
                    Some((w, h)) => {
                        v.width = Some(w);
                        v.height = Some(h);
                    }
                    None => {
                        v.width = width;
                        v.height = height;
                    }
                }
                variants.push(v);
            } else if content_type.to_ascii_lowercase().contains("mpegurl") {
                let mut v = Variant::new(variant_url, VariantKind::Hls);
                v.duration = duration;
                variants.push(v);
            }
        }
        if variants.is_empty()
            && let Some(raw) = video.get("url").and_then(|u| u.as_str())
            && let Ok(direct) = Url::parse(raw)
        {
            let mut v = Variant::new(direct, VariantKind::File);
            v.container = Some(Container::Mp4);
            v.width = width;
            v.height = height;
            v.duration = duration;
            variants.push(v);
        }
        if variants.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let mut resolved = Resolved::new("twitter");
        resolved.id = Some(id);
        resolved.title = title;
        resolved.description = tweet
            .get("text")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        resolved.uploader = (!handle.is_empty()).then(|| format!("@{handle}"));
        resolved.uploader_url = (!handle.is_empty())
            .then(|| Url::parse(&format!("https://x.com/{handle}")).ok())
            .flatten();
        resolved.uploaded_at = tweet
            .get("created_timestamp")
            .and_then(|t| t.as_i64())
            .and_then(|t| jiff::Timestamp::from_second(t).ok());
        resolved.duration = duration;
        resolved.thumbnail = video
            .get("thumbnail_url")
            .and_then(|t| t.as_str())
            .and_then(|t| Url::parse(t).ok());
        resolved.webpage_url = tweet
            .get("url")
            .and_then(|u| u.as_str())
            .and_then(|u| Url::parse(u).ok());
        resolved.variants = variants;
        let _ = check_status;
        Ok(Resolution::from(resolved))
    }
}
