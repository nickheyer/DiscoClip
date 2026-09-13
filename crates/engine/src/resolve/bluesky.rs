//! Bluesky posts with video, through the public AT Protocol API the app reads: the post
//! thread names the video's HLS playlist and thumbnail.

use std::sync::LazyLock;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    ClipRange, MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport,
    Variant, clean_title, fetch, hls, timestamp_hint,
};
use crate::http::{BROWSER_UA, Http};

pub const PLATFORM: &str = "bluesky";
const SITE: &str = "https://bsky.app/";
const API: &str = "https://public.api.bsky.app/xrpc/";

static RE_RKEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z2-7]{13}$").unwrap());

/// A post, by the account that wrote it (a handle or a DID) and its record key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostRef {
    pub actor: String,
    pub rkey: String,
}

impl PostRef {
    fn at_uri(&self) -> String {
        format!("at://{}/app.bsky.feed.post/{}", self.actor, self.rkey)
    }
}

pub fn parse_link(url: &Url) -> Option<PostRef> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let bluesky = host == "bsky.app"
        || host.ends_with(".bsky.app")
        || host == "bsky.social"
        || host.ends_with(".bsky.social");
    if !bluesky {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    match segments.as_slice() {
        ["profile", actor, "post", rkey] if RE_RKEY.is_match(rkey) && !actor.is_empty() => {
            Some(PostRef {
                actor: actor.to_string(),
                rkey: rkey.to_string(),
            })
        }
        _ => None,
    }
}

pub struct BlueskyResolver {
    http: Http,
}

impl BlueskyResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    async fn thread(&self, post: &PostRef, origin: &Url) -> Result<Value, ResolveError> {
        let mut url = Url::parse(&format!("{API}app.bsky.feed.getPostThread")).expect("valid");
        url.query_pairs_mut()
            .append_pair("uri", &post.at_uri())
            .append_pair("depth", "0")
            .append_pair("parentHeight", "0");
        let fetched = fetch(&self.http, &url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        let answer = fetched.json(origin)?;
        if !fetched.status.is_success() {
            let error = answer["error"].as_str().unwrap_or("");
            let message = answer["message"].as_str().unwrap_or("the API refused the post");
            return Err(match (fetched.status.as_u16(), error) {
                (_, "NotFound") | (404, _) => ResolveError::NotFound(origin.clone()),
                (429, _) => ResolveError::RateLimited(origin.clone()),
                (_, "InvalidRequest") if message.contains("resolve") => {
                    ResolveError::NotFound(origin.clone())
                }
                _ => ResolveError::unavailable(origin, message.to_string()),
            });
        }
        let thread = &answer["thread"];
        match thread["$type"].as_str() {
            Some("app.bsky.feed.defs#notFoundPost") => Err(ResolveError::NotFound(origin.clone())),
            Some("app.bsky.feed.defs#blockedPost") => Err(ResolveError::unavailable(
                origin,
                "the post's author blocks viewers of this kind",
            )),
            _ => Ok(thread["post"].clone()),
        }
    }
}

/// The video embed of a post, whether the post carries it alone or with a quote.
fn video_embed(post: &Value) -> Option<&Value> {
    let embed = &post["embed"];
    match embed["$type"].as_str() {
        Some("app.bsky.embed.video#view") => Some(embed),
        Some("app.bsky.embed.recordWithMedia#view")
            if embed["media"]["$type"].as_str() == Some("app.bsky.embed.video#view") =>
        {
            Some(&embed["media"])
        }
        _ => None,
    }
}

#[async_trait]
impl Resolver for BlueskyResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Bluesky",
            hosts: &["bsky.app"],
            features: &["posts", "quote posts", "at links"],
            formats: &["hls"],
            session: SessionSupport::None,
            examples: &["https://bsky.app/profile/bsky.app/post/3mk4lzkrnk22d"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let post_ref = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let post = self.thread(&post_ref, url).await?;
        let embed = video_embed(&post).ok_or_else(|| {
            if post["embed"]["$type"]
                .as_str()
                .is_some_and(|t| t.contains("images"))
            {
                ResolveError::unavailable(url, "the post carries images, not a video")
            } else {
                ResolveError::NotFound(url.clone())
            }
        })?;
        let playlist = embed["playlist"]
            .as_str()
            .and_then(|u| Url::parse(u).ok())
            .ok_or_else(|| ResolveError::malformed(url, "the video embed has no playlist"))?;
        let expanded = hls::expand(&self.http, &playlist, PLATFORM, BROWSER_UA, &[]).await?;
        let aspect = &embed["aspectRatio"];
        let (width, height) = (
            aspect["width"].as_u64().map(|w| w as u32),
            aspect["height"].as_u64().map(|h| h as u32),
        );
        let mut variants: Vec<Variant> = expanded.variants;
        for v in &mut variants {
            if v.width.is_none() && v.height.is_none() {
                v.width = width;
                v.height = height;
            }
        }
        let author = &post["author"];
        let handle = author["handle"].as_str().unwrap_or("");
        let text = post["record"]["text"].as_str().unwrap_or("");
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = post["uri"]
            .as_str()
            .and_then(|u| u.rsplit('/').next())
            .map(String::from)
            .or(Some(post_ref.rkey.clone()));
        resolved.title = clean_title(text)
            .or_else(|| embed["alt"].as_str().and_then(clean_title))
            .or_else(|| (!handle.is_empty()).then(|| format!("Video by @{handle}")));
        resolved.description = clean_title(text);
        resolved.uploader = author["displayName"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| (!handle.is_empty()).then(|| format!("@{handle}")));
        resolved.uploader_url = (!handle.is_empty())
            .then(|| Url::parse(&format!("{SITE}profile/{handle}")).ok())
            .flatten();
        resolved.uploaded_at = post["record"]["createdAt"]
            .as_str()
            .or_else(|| post["indexedAt"].as_str())
            .and_then(|t| t.parse::<Timestamp>().ok());
        resolved.duration = expanded.duration;
        resolved.thumbnail = embed["thumbnail"].as_str().and_then(|u| Url::parse(u).ok());
        resolved.webpage_url = (!handle.is_empty())
            .then(|| Url::parse(&format!("{SITE}profile/{handle}/post/{}", post_ref.rkey)).ok())
            .flatten();
        resolved.live = expanded.live;
        resolved.clip = timestamp_hint(url).map(|start| ClipRange { start, end: None });
        resolved.subtitles = expanded.subtitles;
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

    const THREAD_API: &str = "https://public.api.bsky.app/xrpc/app.bsky.feed.getPostThread";
    const MASTER: &str = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1200000,RESOLUTION=380x800,CODECS=\"avc1.64001f,mp4a.40.2\"\n720p/video.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=400000,RESOLUTION=170x360,CODECS=\"avc1.64001e,mp4a.40.2\"\n360p/video.m3u8\n";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:3\n#EXTINF:3.0,\n0.ts\n#EXTINF:2.5,\n1.ts\n#EXT-X-ENDLIST\n";

    fn post_json() -> Value {
        json!({"thread": {"$type": "app.bsky.feed.defs#threadViewPost", "post": {
            "uri": "at://did:plc:z72i7hdynmk6r22z27h6tvur/app.bsky.feed.post/3mk4lzkrnk22d",
            "author": {"did": "did:plc:z72i7hdynmk6r22z27h6tvur", "handle": "bsky.app", "displayName": "Bluesky"},
            "record": {"text": "v1.121 is live!\n\nWe've increased the quality of photos.", "createdAt": "2026-04-22T23:00:21.312Z",
                "embed": {"$type": "app.bsky.embed.video", "video": {"ref": {"$link": "bafkreifhuv"}, "mimeType": "video/mp4", "size": 956983}, "aspectRatio": {"height": 800, "width": 381}}},
            "embed": {"$type": "app.bsky.embed.video#view", "cid": "bafkreifhuv", "aspectRatio": {"height": 800, "width": 381},
                "playlist": "https://video.bsky.app/watch/did%3Aplc%3Az72i7hdynmk6r22z27h6tvur/bafkreifhuv/playlist.m3u8",
                "thumbnail": "https://video.bsky.app/watch/did%3Aplc%3Az72i7hdynmk6r22z27h6tvur/bafkreifhuv/thumbnail.jpg"}
        }}})
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://bsky.app/profile/bsky.app/post/3mk4lzkrnk22d"),
            Some(PostRef {
                actor: "bsky.app".into(),
                rkey: "3mk4lzkrnk22d".into()
            })
        );
        assert_eq!(
            link("https://bsky.app/profile/did:plc:z72i7hdynmk6r22z27h6tvur/post/3mk4lzkrnk22d"),
            Some(PostRef {
                actor: "did:plc:z72i7hdynmk6r22z27h6tvur".into(),
                rkey: "3mk4lzkrnk22d".into()
            })
        );
        assert_eq!(link("https://bsky.app/profile/bsky.app"), None);
        assert_eq!(link("https://bsky.app/profile/bsky.app/post/short"), None);
    }

    #[tokio::test]
    async fn posts_with_video_resolve_to_their_playlist() {
        let mut fixture = Fixture::new("bluesky", None);
        fixture.exchanges.push(get(THREAD_API, 200, "application/json", &post_json().to_string()));
        fixture.exchanges.push(get(
            "https://video.bsky.app/watch/did%3Aplc%3Az72i7hdynmk6r22z27h6tvur/bafkreifhuv/playlist.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MASTER,
        ));
        fixture.exchanges.push(get(
            "https://video.bsky.app/watch/did%3Aplc%3Az72i7hdynmk6r22z27h6tvur/bafkreifhuv/720p/video.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MEDIA,
        ));
        let resolver = BlueskyResolver::new(Http::replay(fixture));
        let url = Url::parse("https://bsky.app/profile/bsky.app/post/3mk4lzkrnk22d").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("3mk4lzkrnk22d"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("v1.121 is live! We've increased the quality of photos.")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("Bluesky"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://bsky.app/profile/bsky.app"
        );
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.duration, Some(std::time::Duration::from_secs_f64(5.5)));
        assert_eq!(resolved.variants.len(), 2);
        assert_eq!(resolved.variants[0].height, Some(800));
        assert!(resolved.thumbnail.is_some());
    }

    #[tokio::test]
    async fn posts_without_video_and_missing_posts() {
        let mut fixture = Fixture::new("bluesky", None);
        let mut images = post_json();
        images["thread"]["post"]["embed"] = json!({"$type": "app.bsky.embed.images#view", "images": []});
        fixture.exchanges.push(get(THREAD_API, 200, "application/json", &images.to_string()));
        fixture.exchanges.push(get(
            THREAD_API,
            400,
            "application/json",
            &json!({"error": "NotFound", "message": "Post not found: at://bsky.app/app.bsky.feed.post/3l6ovee00000"}).to_string(),
        ));
        fixture.exchanges.push(get(
            THREAD_API,
            200,
            "application/json",
            &json!({"thread": {"$type": "app.bsky.feed.defs#notFoundPost", "uri": "at://x", "notFound": true}}).to_string(),
        ));
        let resolver = BlueskyResolver::new(Http::replay(fixture));
        let url = Url::parse("https://bsky.app/profile/bsky.app/post/3mk4lzkrnk22d").unwrap();
        let error = resolver.resolve(&url).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("images")),
            "{error}"
        );
        assert!(matches!(
            resolver.resolve(&url).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
        assert!(matches!(
            resolver.resolve(&url).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
