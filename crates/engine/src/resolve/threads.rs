//! Threads posts with video, from the data the page embeds for a browser that navigates
//! to it: the post's video versions, or each video of a carousel as a playlist item.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::page::Page;
use super::{
    ClipRange, MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionSupport, Variant, VariantKind, clean_title, timestamp_hint,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "threads";
const SITE: &str = "https://www.threads.net/";

static RE_CODE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]{8,}$").unwrap());

/// A post, by its short code, with the user the link names and the carousel item it
/// picks out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostRef {
    pub user: Option<String>,
    pub code: String,
    pub item: Option<usize>,
}

pub fn parse_link(url: &Url) -> Option<PostRef> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let threads = matches!(
        host.as_str(),
        "threads.net" | "www.threads.net" | "threads.com" | "www.threads.com"
    );
    if !threads {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let (user, code) = match segments.as_slice() {
        [user, "post", code, ..] if user.starts_with('@') => (Some(user[1..].to_string()), *code),
        ["t", code] | ["post", code] => (None, *code),
        _ => return None,
    };
    if !RE_CODE.is_match(code) {
        return None;
    }
    let item = url
        .fragment()
        .and_then(|f| f.strip_prefix("item-"))
        .and_then(|n| n.parse::<usize>().ok())
        .filter(|n| *n >= 1);
    Some(PostRef {
        user,
        code: code.to_string(),
        item,
    })
}

/// Every `thread_items` post the page's data blobs carry.
pub fn posts_in(page: &Page) -> Vec<Value> {
    fn walk(value: &Value, out: &mut Vec<Value>) {
        match value {
            Value::Object(map) => {
                if let Some(items) = map.get("thread_items").and_then(|i| i.as_array()) {
                    for item in items {
                        if item["post"].is_object() {
                            out.push(item["post"].clone());
                        }
                    }
                }
                for child in map.values() {
                    walk(child, out);
                }
            }
            Value::Array(items) => items.iter().for_each(|i| walk(i, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    let selector = scraper::Selector::parse("script[type='application/json'][data-sjs]")
        .expect("valid");
    for script in page.document().select(&selector) {
        let text: String = script.text().collect();
        if !text.contains("thread_items") {
            continue;
        }
        if let Ok(value) = serde_json::from_str::<Value>(&text) {
            walk(&value, &mut out);
        }
    }
    out
}

/// The variants a post's (or carousel item's) `video_versions` make.
pub fn variants_of(media: &Value, fallback_size: (Option<u32>, Option<u32>)) -> Vec<Variant> {
    let headers = vec![("referer".to_string(), SITE.to_string())];
    let mut variants: Vec<Variant> = Vec::new();
    for version in media["video_versions"].as_array().into_iter().flatten() {
        let Some(url) = version["url"].as_str().and_then(|u| Url::parse(u).ok()) else {
            continue;
        };
        if variants.iter().any(|v| v.url == url) {
            continue;
        }
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(Container::Mp4);
        v.video = Some(VideoCodec::H264);
        v.audio = (media["has_audio"].as_bool() != Some(false)).then_some(AudioCodec::Aac);
        v.width = version["width"].as_u64().map(|w| w as u32).or(fallback_size.0);
        v.height = version["height"].as_u64().map(|h| h as u32).or(fallback_size.1);
        v.format_id = version["type"].as_u64().map(|t| format!("type-{t}"));
        v.duration = media["video_duration"]
            .as_f64()
            .filter(|d| *d > 0.0)
            .map(Duration::from_secs_f64);
        v.headers = headers.clone();
        variants.push(v);
    }
    variants
}

/// The size a media record states for itself.
fn size_of(media: &Value) -> (Option<u32>, Option<u32>) {
    (
        media["original_width"].as_u64().map(|w| w as u32),
        media["original_height"].as_u64().map(|h| h as u32),
    )
}

fn thumbnail_of(media: &Value) -> Option<Url> {
    media["image_versions2"]["candidates"]
        .as_array()
        .and_then(|c| c.first())
        .and_then(|c| c["url"].as_str())
        .and_then(|u| Url::parse(u).ok())
}

pub struct ThreadsResolver {
    http: Http,
}

impl ThreadsResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// The post's record from its page, fetched the way a browser navigates to it, which
    /// is what makes the site render the post's data into the page.
    async fn post(&self, post_ref: &PostRef, origin: &Url) -> Result<Value, ResolveError> {
        let path = match &post_ref.user {
            Some(user) => format!("{SITE}@{user}/post/{}", post_ref.code),
            None => format!("{SITE}post/{}", post_ref.code),
        };
        let page_url = Url::parse(&path).expect("valid");
        let response = self
            .http
            .get(page_url.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
            .header("accept-language", "en-US,en;q=0.9")
            .header("sec-fetch-mode", "navigate")
            .header("sec-fetch-site", "none")
            .header("sec-fetch-dest", "document")
            .header("sec-fetch-user", "?1")
            .header("upgrade-insecure-requests", "1")
            .send()
            .await?;
        match response.status.as_u16() {
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
        let final_url = response.url.clone();
        if final_url.path().starts_with("/login") {
            return Err(ResolveError::unavailable(
                origin,
                "the site sent the request to its login page",
            ));
        }
        let html = response.text(MAX_PAGE).await?;
        let page = Page::parse(&html, &final_url);
        let posts = posts_in(&page);
        posts
            .into_iter()
            .find(|p| p["code"].as_str() == Some(post_ref.code.as_str()))
            .ok_or_else(|| {
                if html.contains("thread_items") {
                    ResolveError::NotFound(origin.clone())
                } else {
                    ResolveError::unavailable(
                        origin,
                        "the page carries no post data; the post may be private or gone",
                    )
                }
            })
    }
}

#[async_trait]
impl Resolver for ThreadsResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Threads",
            hosts: &["threads.net", "threads.com"],
            features: &["posts", "carousels"],
            formats: &["mp4"],
            session: SessionSupport::None,
            examples: &["https://www.threads.net/@instagram/post/DdHPeall2Bx"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let post_ref = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let post = self.post(&post_ref, url).await?;
        let user = &post["user"];
        let username = user["username"].as_str().unwrap_or("");
        let caption = post["caption"]["text"].as_str().and_then(clean_title);
        let page = Url::parse(&format!(
            "{SITE}@{}/post/{}",
            if username.is_empty() {
                post_ref.user.clone().unwrap_or_default()
            } else {
                username.to_string()
            },
            post_ref.code
        ))
        .ok();
        let mut base = Resolved::new(PLATFORM);
        base.id = Some(post_ref.code.clone());
        base.title = caption
            .clone()
            .or_else(|| (!username.is_empty()).then(|| format!("Thread by @{username}")));
        base.description = caption;
        base.uploader = user["full_name"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| (!username.is_empty()).then(|| format!("@{username}")));
        base.uploader_url = (!username.is_empty())
            .then(|| Url::parse(&format!("{SITE}@{username}")).ok())
            .flatten();
        base.uploaded_at = post["taken_at"]
            .as_i64()
            .or_else(|| post["taken_at"].as_str().and_then(|t| t.parse().ok()))
            .and_then(|t| Timestamp::from_second(t).ok());
        base.webpage_url = page.clone();
        base.clip = timestamp_hint(url).map(|start| ClipRange { start, end: None });

        let carousel: Vec<&Value> = post["carousel_media"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["video_versions"].as_array().is_some_and(|v| !v.is_empty()))
            .collect();
        if !carousel.is_empty() {
            if let Some(item) = post_ref.item {
                let media = carousel
                    .get(item - 1)
                    .copied()
                    .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
                let variants = variants_of(media, size_of(media));
                if variants.is_empty() {
                    return Err(ResolveError::NotFound(url.clone()));
                }
                let mut resolved = base;
                resolved.id = Some(format!("{}-{item}", post_ref.code));
                resolved.duration = variants.iter().find_map(|v| v.duration);
                resolved.thumbnail = thumbnail_of(media);
                resolved.variants = variants;
                return Ok(Resolution::from(resolved));
            }
            if carousel.len() == 1 {
                let media = carousel[0];
                let variants = variants_of(media, size_of(media));
                let mut resolved = base;
                resolved.duration = variants.iter().find_map(|v| v.duration);
                resolved.thumbnail = thumbnail_of(media);
                resolved.variants = variants;
                return Ok(Resolution::from(resolved));
            }
            let entries = carousel
                .iter()
                .enumerate()
                .filter_map(|(index, media)| {
                    let mut entry = page.clone()?;
                    entry.set_fragment(Some(&format!("item-{}", index + 1)));
                    Some(PlaylistEntry {
                        url: entry,
                        title: base.title.as_ref().map(|t| format!("{t} ({})", index + 1)),
                        duration: media["video_duration"]
                            .as_f64()
                            .filter(|d| *d > 0.0)
                            .map(Duration::from_secs_f64),
                    })
                })
                .collect::<Vec<_>>();
            return Ok(Resolution::Playlist(Playlist {
                resolver: PLATFORM.into(),
                id: base.id,
                title: base.title,
                total: Some(entries.len()),
                entries,
            }));
        }
        let variants = variants_of(&post, size_of(&post));
        if variants.is_empty() {
            return Err(if post["image_versions2"]["candidates"]
                .as_array()
                .is_some_and(|c| !c.is_empty())
            {
                ResolveError::unavailable(url, "the post carries images, not a video")
            } else {
                ResolveError::NotFound(url.clone())
            });
        }
        let mut resolved = base;
        resolved.duration = variants.iter().find_map(|v| v.duration);
        resolved.thumbnail = thumbnail_of(&post);
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

    fn get(url: &str, status: u16, body: &str) -> Exchange {
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
                headers: vec![("content-type".into(), "text/html".into())],
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    fn page(posts: Vec<Value>) -> String {
        let data = json!({"require": [["ScheduledServerJS", "handle", null, [{"__bbox": {"require": [["RelayPrefetchedStreamCache", "next", [], ["adp_BarcelonaPostPageQuery", {"__bbox": {"result": {"data": {"data": {"edges": [{"node": {"thread_items": posts.iter().map(|p| json!({"post": p})).collect::<Vec<_>>()}}]}}}}}]]]}}]]]});
        format!(
            r#"<html><head><script type="application/json" data-content-len="1" data-sjs>{{"define":[]}}</script><script type="application/json" data-sjs>{data}</script></head><body></body></html>"#
        )
    }

    fn video_post() -> Value {
        json!({
            "code": "DdKHPefAvpb", "pk": "3984028670212897371", "taken_at": 1789153249, "media_type": 2,
            "original_width": 1920, "original_height": 1080, "has_audio": true, "video_duration": 30.5,
            "caption": {"text": "The OGs vs Gen Z!\n\nA DIFFERENT WORLD premieres September 24"},
            "user": {"username": "netflix", "full_name": "Netflix", "pk": "1"},
            "video_versions": [
                {"type": 101, "url": "https://scontent.cdninstagram.com/o1/v/t16/f2/m84/a.mp4?_nc_cat=108"},
                {"type": 102, "url": "https://scontent.cdninstagram.com/o1/v/t16/f2/m84/b.mp4?_nc_cat=108"},
                {"type": 103, "url": "https://scontent.cdninstagram.com/o1/v/t16/f2/m84/c.mp4?_nc_cat=108"}
            ],
            "image_versions2": {"candidates": [{"height": 360, "url": "https://scontent.cdninstagram.com/v/t51/thumb.jpg", "width": 640}]},
            "carousel_media": null
        })
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://www.threads.net/@netflix/post/DdKHPefAvpb"),
            Some(PostRef {
                user: Some("netflix".into()),
                code: "DdKHPefAvpb".into(),
                item: None
            })
        );
        assert_eq!(
            link("https://www.threads.com/@netflix/post/DdKHPefAvpb?xmt=abc#item-2"),
            Some(PostRef {
                user: Some("netflix".into()),
                code: "DdKHPefAvpb".into(),
                item: Some(2)
            })
        );
        assert_eq!(
            link("https://www.threads.net/t/DdKHPefAvpb"),
            Some(PostRef {
                user: None,
                code: "DdKHPefAvpb".into(),
                item: None
            })
        );
        assert_eq!(link("https://www.threads.net/@netflix"), None);
        assert_eq!(link("https://www.threads.net/@netflix/post/x"), None);
    }

    #[tokio::test]
    async fn video_posts_resolve_from_the_page_data() {
        let mut fixture = Fixture::new("threads", None);
        fixture.exchanges.push(get(
            "https://www.threads.net/@netflix/post/DdKHPefAvpb",
            200,
            &page(vec![video_post()]),
        ));
        let resolver = ThreadsResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.threads.net/@netflix/post/DdKHPefAvpb").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("DdKHPefAvpb"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("The OGs vs Gen Z! A DIFFERENT WORLD premieres September 24")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("Netflix"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://www.threads.net/@netflix"
        );
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(30.5)));
        assert_eq!(resolved.variants.len(), 3);
        assert_eq!(resolved.variants[0].width, Some(1920));
        assert_eq!(resolved.variants[0].format_id.as_deref(), Some("type-101"));
        assert!(resolved.thumbnail.is_some());
    }

    #[tokio::test]
    async fn carousels_list_their_videos_and_images_are_refused() {
        let mut carousel = video_post();
        carousel["code"] = json!("DcULMjPEsV3");
        carousel["media_type"] = json!(8);
        carousel["video_versions"] = Value::Null;
        carousel["carousel_media"] = json!([
            {"video_versions": [{"type": 101, "url": "https://scontent.cdninstagram.com/one.mp4"}], "original_width": 720, "original_height": 1280, "has_audio": true, "video_duration": 5.0},
            {"image_versions2": {"candidates": []}, "original_width": 720, "original_height": 1280},
            {"video_versions": [{"type": 101, "url": "https://scontent.cdninstagram.com/two.mp4"}], "original_width": 720, "original_height": 1280, "video_duration": 7.0}
        ]);
        let mut images = video_post();
        images["code"] = json!("Dimages1234");
        images["video_versions"] = Value::Null;
        let mut fixture = Fixture::new("threads", None);
        fixture.exchanges.push(get(
            "https://www.threads.net/@mosseri/post/DcULMjPEsV3",
            200,
            &page(vec![carousel.clone()]),
        ));
        fixture.exchanges.push(get(
            "https://www.threads.net/@mosseri/post/DcULMjPEsV3",
            200,
            &page(vec![carousel]),
        ));
        fixture.exchanges.push(get(
            "https://www.threads.net/@netflix/post/Dimages1234",
            200,
            &page(vec![images]),
        ));
        fixture.exchanges.push(get(
            "https://www.threads.net/@netflix/post/Dgone123456",
            200,
            "<html><head></head><body>login shell</body></html>",
        ));
        let resolver = ThreadsResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.threads.net/@mosseri/post/DcULMjPEsV3").unwrap();
        let playlist = match resolver.resolve(&url).await.unwrap() {
            Resolution::Playlist(playlist) => playlist,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[1].url.as_str(),
            "https://www.threads.net/@netflix/post/DcULMjPEsV3#item-2"
        );
        assert_eq!(playlist.entries[1].duration, Some(Duration::from_secs(7)));
        let second = resolver
            .resolve(&Url::parse("https://www.threads.net/@mosseri/post/DcULMjPEsV3#item-2").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(second.id.as_deref(), Some("DcULMjPEsV3-2"));
        assert_eq!(second.variants[0].url.as_str(), "https://scontent.cdninstagram.com/two.mp4");
        assert_eq!(second.variants[0].height, Some(1280));
        let error = resolver
            .resolve(&Url::parse("https://www.threads.net/@netflix/post/Dimages1234").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("images")),
            "{error}"
        );
        let error = resolver
            .resolve(&Url::parse("https://www.threads.net/@netflix/post/Dgone123456").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("no post data")),
            "{error}"
        );
    }
}
