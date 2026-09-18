//! 9GAG posts, through the API the site's own pages call: an animated post's MP4 and WebM
//! files, its title, section and time, with a video post hosted on YouTube handed to the
//! YouTube resolver and a photo post reported as one.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Tag, Variant,
    VariantKind, clean_title, fetch,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "9gag";
const SITE: &str = "https://9gag.com/";
const POST_API: &str = "https://9gag.com/v1/post";

static RE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9]{5,12}$").unwrap());

pub fn post_id(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "9gag.com" && host != "www.9gag.com" && host != "m.9gag.com" {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    match segments.as_slice() {
        ["gag", id, ..] if RE_ID.is_match(id) => Some(id.to_string()),
        _ => None,
    }
}

/// The video files of an animated post, by codec.
pub fn variants_of(post: &Value) -> Vec<Variant> {
    let source = &post["images"]["image460sv"];
    let width = source["width"].as_u64().map(|w| w as u32);
    let height = source["height"].as_u64().map(|h| h as u32);
    let duration = source["duration"]
        .as_f64()
        .filter(|d| *d > 0.0)
        .map(Duration::from_secs_f64);
    let has_audio =
        source["hasAudio"].as_i64().unwrap_or(0) != 0 || source["hasAudio"].as_bool() == Some(true);
    let mut variants = Vec::new();
    for (key, container, video, audio) in [
        ("url", Container::Mp4, VideoCodec::H264, AudioCodec::Aac),
        ("h265Url", Container::Mp4, VideoCodec::H265, AudioCodec::Aac),
        ("vp9Url", Container::Webm, VideoCodec::Vp9, AudioCodec::Opus),
        (
            "vp8Url",
            Container::Webm,
            VideoCodec::Vp8,
            AudioCodec::Vorbis,
        ),
    ] {
        let Some(url) = source[key].as_str().and_then(|u| Url::parse(u).ok()) else {
            continue;
        };
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(container);
        v.video = Some(video);
        v.audio = has_audio.then_some(audio);
        v.width = width;
        v.height = height;
        v.duration = duration;
        v.format_id = Some(key.trim_end_matches("Url").to_string());
        v.label = Some(match key {
            "url" => "h264".to_string(),
            other => other.trim_end_matches("Url").to_string(),
        });
        v.headers = vec![("referer".to_string(), SITE.to_string())];
        variants.push(v);
    }
    variants
}

pub struct NinegagResolver {
    http: Http,
}

impl NinegagResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for NinegagResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "9GAG",
            hosts: &["9gag.com"],
            features: &["posts", "animated posts", "youtube posts"],
            formats: &["mp4", "webm"],
            media: &[MediaKind::Video],
            tags: &[Tag::Social, Tag::Images],
            session: SessionSupport::None,
            examples: &["https://9gag.com/gag/ae5Ag7B"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        post_id(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let id = post_id(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let mut api = Url::parse(POST_API).expect("valid");
        api.query_pairs_mut().append_pair("id", &id);
        let headers = [
            ("accept".to_string(), "application/json".to_string()),
            ("referer".to_string(), format!("{SITE}gag/{id}")),
        ];
        let fetched = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &headers, MAX_PAGE).await?;
        match fetched.status.as_u16() {
            200..=299 => {}
            404 | 410 => return Err(ResolveError::NotFound(url.clone())),
            429 => return Err(ResolveError::RateLimited(url.clone())),
            403 => {
                return Err(ResolveError::unavailable(
                    url,
                    "the site's protection refused this client",
                ));
            }
            status => {
                return Err(ResolveError::unavailable(
                    url,
                    format!("the API answered HTTP {status}"),
                ));
            }
        }
        let answer = fetched.json(url)?;
        if answer["meta"]["status"]
            .as_str()
            .is_some_and(|s| s != "Success")
        {
            let message = answer["meta"]["errorMessage"]
                .as_str()
                .unwrap_or("the API reported a failure")
                .to_string();
            return Err(if message.to_ascii_lowercase().contains("not found") {
                ResolveError::NotFound(url.clone())
            } else {
                ResolveError::unavailable(url, message)
            });
        }
        let post = &answer["data"]["post"];
        if !post.is_object() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        if let Some(youtube) = post["youtubeVideoId"]
            .as_str()
            .filter(|v| !v.is_empty())
            .and_then(|v| Url::parse(&format!("https://www.youtube.com/watch?v={v}")).ok())
            .or_else(|| {
                post["videoExternalUrl"]
                    .as_str()
                    .or(post["sourceUrl"].as_str())
                    .filter(|u| u.contains("youtube.com") || u.contains("youtu.be"))
                    .and_then(|u| Url::parse(u).ok())
            })
        {
            return Err(ResolveError::Redirect(youtube));
        }
        let variants = variants_of(post);
        if variants.is_empty() {
            return Err(match post["type"].as_str() {
                Some("Photo") => ResolveError::unavailable(url, "the post is a photo"),
                Some(kind) => ResolveError::unavailable(
                    url,
                    format!("the post is a {kind} without a video file"),
                ),
                None => ResolveError::NotFound(url.clone()),
            });
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.clone());
        resolved.title = post["title"].as_str().and_then(clean_title);
        resolved.description = post["description"].as_str().and_then(clean_title);
        resolved.uploader = post["creator"]["username"]
            .as_str()
            .or(post["creator"]["fullName"].as_str())
            .and_then(clean_title);
        resolved.uploader_url = post["creator"]["profileUrl"]
            .as_str()
            .and_then(|u| Url::parse(u).ok());
        resolved.uploaded_at = post["creationTs"]
            .as_i64()
            .and_then(|t| Timestamp::from_second(t).ok());
        resolved.duration = variants.iter().find_map(|v| v.duration);
        resolved.thumbnail = post["images"]["image700"]["url"]
            .as_str()
            .or(post["images"]["image460"]["url"].as_str())
            .and_then(|u| Url::parse(u).ok());
        resolved.webpage_url = Url::parse(&format!("{SITE}gag/{id}")).ok();
        resolved.age_limit = (post["nsfw"].as_i64().unwrap_or(0) != 0).then_some(18);
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

    fn get(url: &str, status: u16, body: String) -> Exchange {
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
                headers: vec![("content-type".into(), "application/json".into())],
                body: RecordedBody::Text(body),
                truncated: false,
            },
        }
    }

    fn post() -> Value {
        json!({"meta": {"status": "Success"}, "data": {"post": {
            "id": "ae5Ag7B", "url": "http://9gag.com/gag/ae5Ag7B", "title": "Capybara Agility Training", "description": "",
            "type": "Animated", "nsfw": 0, "creationTs": 1573237208, "creator": null,
            "images": {
                "image700": {"width": 460, "height": 288, "url": "https://img-9gag-fun.9cache.com/photo/ae5Ag7B_460s.jpg"},
                "image460sv": {"width": 460, "height": 288, "url": "https://img-9gag-fun.9cache.com/photo/ae5Ag7B_460sv.mp4", "hasAudio": 1, "duration": 44,
                    "vp8Url": "https://img-9gag-fun.9cache.com/photo/ae5Ag7B_460svwm.webm", "h265Url": "https://img-9gag-fun.9cache.com/photo/ae5Ag7B_460svh265.mp4", "vp9Url": "https://img-9gag-fun.9cache.com/photo/ae5Ag7B_460svvp9.webm"}
            },
            "postSection": {"name": "9GAGGER"}, "tags": [{"key": "Awesome"}]
        }}})
    }

    #[test]
    fn links_are_read() {
        let id = |s: &str| post_id(&Url::parse(s).unwrap());
        assert_eq!(id("https://9gag.com/gag/ae5Ag7B"), Some("ae5Ag7B".into()));
        assert_eq!(
            id("https://9gag.com/gag/ae5Ag7B?ref=android"),
            Some("ae5Ag7B".into())
        );
        assert_eq!(id("https://9gag.com/tag/awesome"), None);
        assert_eq!(id("https://9gag.com/"), None);
    }

    #[tokio::test]
    async fn animated_posts_resolve_with_every_codec() {
        let mut fixture = Fixture::new("9gag", None);
        fixture.exchanges.push(get(
            "https://9gag.com/v1/post?id=ae5Ag7B",
            200,
            post().to_string(),
        ));
        let resolver = NinegagResolver::new(Http::replay(fixture));
        let url = Url::parse("https://9gag.com/gag/ae5Ag7B").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.title.as_deref(), Some("Capybara Agility Training"));
        assert_eq!(resolved.uploaded_at.unwrap().as_second(), 1573237208);
        assert_eq!(resolved.duration, Some(Duration::from_secs(44)));
        assert!(resolved.uploader.is_none());
        assert_eq!(resolved.variants.len(), 4);
        let h264 = &resolved.variants[0];
        assert_eq!(
            h264.url.as_str(),
            "https://img-9gag-fun.9cache.com/photo/ae5Ag7B_460sv.mp4"
        );
        assert_eq!(h264.video, Some(VideoCodec::H264));
        assert_eq!(h264.audio, Some(AudioCodec::Aac));
        assert_eq!((h264.width, h264.height), (Some(460), Some(288)));
        assert_eq!(h264.headers[0], ("referer".to_string(), SITE.to_string()));
        assert_eq!(resolved.variants[1].video, Some(VideoCodec::H265));
        assert_eq!(resolved.variants[2].container, Some(Container::Webm));
        assert_eq!(resolved.variants[2].video, Some(VideoCodec::Vp9));
    }

    #[tokio::test]
    async fn photos_youtube_posts_and_missing_posts_say_so() {
        let mut photo = post();
        photo["data"]["post"]["type"] = json!("Photo");
        photo["data"]["post"]["images"] = json!({"image700": {"url": "https://img.test/p.jpg"}});
        let mut youtube = post();
        youtube["data"]["post"]["type"] = json!("Video");
        youtube["data"]["post"]["youtubeVideoId"] = json!("dQw4w9WgXcQ");
        let mut fixture = Fixture::new("9gag", None);
        fixture.exchanges.push(get(
            "https://9gag.com/v1/post?id=aPhoto1",
            200,
            photo.to_string(),
        ));
        fixture.exchanges.push(get(
            "https://9gag.com/v1/post?id=aVideo1",
            200,
            youtube.to_string(),
        ));
        fixture.exchanges.push(get(
            "https://9gag.com/v1/post?id=aGone11",
            200,
            json!({"meta": {"status": "Failure", "errorMessage": "Post not found"}}).to_string(),
        ));
        let resolver = NinegagResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://9gag.com/gag/aPhoto1").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("photo")),
            "{error}"
        );
        let error = resolver
            .resolve(&Url::parse("https://9gag.com/gag/aVideo1").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Redirect(u) if u.as_str() == "https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            "{error}"
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://9gag.com/gag/aGone11").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
