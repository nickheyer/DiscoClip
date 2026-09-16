//! Bluesky posts with video, through the public AT Protocol API the app reads: the post
//! thread names the video's HLS playlist and thumbnail, the author's data server holds
//! the original upload and the caption files, a quoted post's video counts as the post's
//! own, and a link card hands the linked page on.

use std::sync::LazyLock;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    ClipRange, MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionSupport, SubtitleFormat, SubtitleTrack, Variant, clean_title, fetch, hls,
    timestamp_hint, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "bluesky";
const SITE: &str = "https://bsky.app/";
const API: &str = "https://public.api.bsky.app/xrpc/";
/// The data server every account falls back to.
const DEFAULT_PDS: &str = "https://bsky.social";

static RE_RKEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\w+$").unwrap());

/// Which of a post's videos a link names, when the post carries its own and a quoted
/// post's: the `#media=own` or `#media=quote` fragment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    Own,
    Quote,
}

/// A post, by the account that wrote it (a handle or a DID) and its record key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostRef {
    pub actor: String,
    pub rkey: String,
    pub pick: Option<Pick>,
}

impl PostRef {
    fn at_uri(&self) -> String {
        format!("at://{}/app.bsky.feed.post/{}", self.actor, self.rkey)
    }
}

pub fn parse_link(url: &Url) -> Option<PostRef> {
    let pick = match url.fragment() {
        Some("media=own") => Some(Pick::Own),
        Some("media=quote") => Some(Pick::Quote),
        _ => None,
    };
    if url.scheme() == "at" {
        // `at://{actor}/app.bsky.feed.post/{rkey}`
        let actor = url.host_str().or_else(|| url.authority().split('/').next())?;
        let actor = if actor.is_empty() {
            url.as_str().trim_start_matches("at://").split('/').next()?
        } else {
            actor
        };
        let rest = url.as_str().trim_start_matches("at://");
        let mut parts = rest.split('/');
        let actor = parts.next().unwrap_or(actor);
        if parts.next() != Some("app.bsky.feed.post") {
            return None;
        }
        let rkey = parts.next()?.split(|c| c == '?' || c == '#').next()?;
        return (RE_RKEY.is_match(rkey) && !actor.is_empty()).then(|| PostRef {
            actor: actor.to_string(),
            rkey: rkey.to_string(),
            pick,
        });
    }
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let bluesky = host == "bsky.app"
        || host.ends_with(".bsky.app")
        || host == "bsky.social"
        || host.ends_with(".bsky.social")
        || host == "main.bsky.dev";
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
                pick,
            })
        }
        _ => None,
    }
}

/// A video a post carries: the view the API renders (playlist, thumbnail, size) and the
/// record it was posted with (the blob, its captions, the text), with the post that
/// carries it.
struct PostVideo<'a> {
    post: &'a Value,
    view: &'a Value,
    record: &'a Value,
}

/// The videos a post carries, its own first: in its video embed, in the media half of a
/// record-with-media embed, and in the post it quotes. A link card is a video of its
/// own kind, handed on as the page it links.
enum Media<'a> {
    Video(PostVideo<'a>),
    External(Url),
}

fn media_of(post: &Value) -> Vec<Media<'_>> {
    let mut found = Vec::new();
    let embed = &post["embed"];
    let record = &post["record"]["embed"];
    match embed["$type"].as_str() {
        Some("app.bsky.embed.video#view") => found.push(Media::Video(PostVideo {
            post,
            view: embed,
            record,
        })),
        Some("app.bsky.embed.external#view") => {
            if let Some(link) = util::url_of(&embed["external"]["uri"], None) {
                found.push(Media::External(link));
            }
        }
        Some("app.bsky.embed.recordWithMedia#view") => {
            let media = &embed["media"];
            match media["$type"].as_str() {
                Some("app.bsky.embed.video#view") => found.push(Media::Video(PostVideo {
                    post,
                    view: media,
                    record: &record["media"],
                })),
                Some("app.bsky.embed.external#view") => {
                    if let Some(link) = util::url_of(&media["external"]["uri"], None) {
                        found.push(Media::External(link));
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
    // The quoted post, as the view carries it.
    let quoted = if embed["record"]["record"].is_object() {
        &embed["record"]["record"]
    } else {
        &embed["record"]
    };
    if quoted["$type"].as_str() == Some("app.bsky.embed.record#viewRecord")
        || quoted["embeds"].is_array()
    {
        let view = &quoted["embeds"][0];
        let record = &quoted["value"]["embed"];
        match view["$type"].as_str() {
            Some("app.bsky.embed.video#view") => found.push(Media::Video(PostVideo {
                post: quoted,
                view,
                record,
            })),
            Some("app.bsky.embed.external#view") => {
                if let Some(link) = util::url_of(&view["external"]["uri"], None) {
                    found.push(Media::External(link));
                }
            }
            Some("app.bsky.embed.recordWithMedia#view")
                if view["media"]["$type"].as_str() == Some("app.bsky.embed.video#view") =>
            {
                found.push(Media::Video(PostVideo {
                    post: quoted,
                    view: &view["media"],
                    record: &record["media"],
                }))
            }
            _ => {}
        }
    }
    found
}

/// The author's handle, from the view's `author` or the quoted record's.
fn handle_of(post: &Value) -> String {
    post["author"]["handle"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

/// The post's key, from its AT URI.
fn rkey_of(post: &Value) -> Option<String> {
    post["uri"]
        .as_str()
        .and_then(|u| u.rsplit('/').next())
        .map(String::from)
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

impl BlueskyResolver {
    /// The data server holding an account's blobs: named by the account's DID document
    /// (at plc.directory, or under the domain of a `did:web`), else the site's own.
    async fn data_server(&self, did: &str) -> Url {
        let document = if let Some(domain) = did.strip_prefix("did:web:") {
            format!("https://{domain}/.well-known/did.json")
        } else {
            format!("https://plc.directory/{did}")
        };
        let fallback = Url::parse(DEFAULT_PDS).expect("valid");
        let Ok(url) = Url::parse(&document) else {
            return fallback;
        };
        let answer = match fetch(&self.http, &url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await {
            Ok(fetched) if fetched.status.is_success() => fetched.json(&url).unwrap_or(Value::Null),
            Ok(fetched) => {
                tracing::debug!(%url, status = %fetched.status, "DID document not read");
                Value::Null
            }
            Err(error) => {
                tracing::debug!(%url, "DID document not read: {error}");
                Value::Null
            }
        };
        answer["service"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|service| service["type"].as_str() == Some("AtprotoPersonalDataServer"))
            .and_then(|service| util::url_of(&service["serviceEndpoint"], None))
            .unwrap_or(fallback)
    }

    /// A blob on `server`, by the account and its content id.
    fn blob_url(server: &Url, did: &str, cid: &str) -> Option<Url> {
        let mut url = server.join("/xrpc/com.atproto.sync.getBlob").ok()?;
        url.query_pairs_mut()
            .append_pair("did", did)
            .append_pair("cid", cid);
        Some(url)
    }

    /// One video of a post as media: the HLS renditions, the original upload from the
    /// author's data server, the caption files, and what the post says.
    async fn video(&self, video: &PostVideo<'_>, origin: &Url, webpage: Url) -> Result<Resolved, ResolveError> {
        let PostVideo { post, view, record } = video;
        let playlist = view["playlist"]
            .as_str()
            .and_then(|u| Url::parse(u).ok())
            .ok_or_else(|| ResolveError::malformed(origin, "the video embed has no playlist"))?;
        let expanded = hls::expand(&self.http, &playlist, PLATFORM, BROWSER_UA, &[]).await?;
        let aspect = &view["aspectRatio"];
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
        let mut subtitles = expanded.subtitles;
        let author = &post["author"];
        let did = author["did"].as_str().unwrap_or("");
        let cid = view["cid"]
            .as_str()
            .or_else(|| record["video"]["ref"]["$link"].as_str());
        if let (false, Some(cid)) = (did.is_empty(), cid) {
            let server = self.data_server(did).await;
            if let Some(blob) = Self::blob_url(&server, did, cid) {
                let mut original = Variant::file(blob);
                let mime = record["video"]["mimeType"].as_str().unwrap_or("video/mp4");
                original.container = Container::from_mime(mime).or(Some(Container::Mp4));
                original.video = Some(VideoCodec::H264);
                original.audio = Some(AudioCodec::Aac);
                original.width = width;
                original.height = height;
                original.size = record["video"]["size"].as_u64().filter(|s| *s > 0);
                original.duration = expanded.duration;
                original.format_id = Some("blob".to_string());
                original.label = Some("original".to_string());
                variants.push(original);
            }
            for caption in record["captions"].as_array().into_iter().flatten() {
                let Some(file_cid) = caption["file"]["ref"]["$link"].as_str() else {
                    continue;
                };
                let Some(url) = Self::blob_url(&server, did, file_cid) else {
                    continue;
                };
                let mime = caption["file"]["mimeType"].as_str().unwrap_or("text/vtt");
                subtitles.push(SubtitleTrack {
                    url,
                    language: caption["lang"].as_str().unwrap_or("und").to_string(),
                    name: None,
                    format: if mime.contains("srt") || mime.contains("subrip") {
                        SubtitleFormat::Srt
                    } else {
                        SubtitleFormat::Vtt
                    },
                    auto: false,
                    headers: Vec::new(),
                });
            }
        }
        let handle = handle_of(post);
        let text = post["record"]["text"]
            .as_str()
            .or(post["value"]["text"].as_str())
            .unwrap_or("");
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = rkey_of(post);
        resolved.title = clean_title(text)
            .or_else(|| view["alt"].as_str().and_then(clean_title))
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
            .or(post["value"]["createdAt"].as_str())
            .or_else(|| post["indexedAt"].as_str())
            .and_then(|t| t.parse::<Timestamp>().ok());
        resolved.duration = expanded.duration;
        resolved.thumbnail = view["thumbnail"].as_str().and_then(|u| Url::parse(u).ok());
        resolved.webpage_url = Some(webpage);
        resolved.live = expanded.live;
        resolved.clip = timestamp_hint(origin).map(|start| ClipRange { start, end: None });
        resolved.age_limit = post["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|label| {
                matches!(label["val"].as_str(), Some("sexual") | Some("porn") | Some("graphic-media"))
            })
            .then_some(18);
        resolved.subtitles = subtitles;
        resolved.variants = variants;
        Ok(resolved)
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
            hosts: &["bsky.app", "main.bsky.dev"],
            features: &["posts", "quote posts", "link cards", "at links", "original uploads", "captions"],
            formats: &["hls", "mp4"],
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
        let handle = handle_of(&post);
        let page = |post_handle: &str, rkey: &str, pick: Option<Pick>| {
            let mut page = Url::parse(&format!(
                "{SITE}profile/{}/post/{rkey}",
                if post_handle.is_empty() { post_ref.actor.as_str() } else { post_handle }
            ))
            .expect("valid");
            page.set_fragment(match pick {
                Some(Pick::Own) => Some("media=own"),
                Some(Pick::Quote) => Some("media=quote"),
                None => None,
            });
            page
        };
        let mut media = media_of(&post);
        if media.is_empty() {
            return Err(if post["embed"]["$type"]
                .as_str()
                .is_some_and(|t| t.contains("images"))
            {
                ResolveError::unavailable(url, "the post carries images, not a video")
            } else {
                ResolveError::NotFound(url.clone())
            });
        }
        // The post's own media comes first, the quoted post's second.
        let own_is_first = std::ptr::eq(
            match &media[0] {
                Media::Video(video) => video.post,
                Media::External(_) => &post,
            },
            &post,
        );
        let chosen = match (post_ref.pick, media.len()) {
            (Some(Pick::Own), _) if own_is_first => Some(0),
            (Some(Pick::Own), _) => return Err(ResolveError::NotFound(url.clone())),
            (Some(Pick::Quote), n) if n > 1 || !own_is_first => Some(media.len() - 1),
            (Some(Pick::Quote), _) => return Err(ResolveError::NotFound(url.clone())),
            (None, 1) => Some(0),
            (None, _) => None,
        };
        match chosen {
            Some(index) => match media.swap_remove(index) {
                Media::External(link) => Err(ResolveError::Redirect(link)),
                Media::Video(video) => {
                    let webpage = page(&handle_of(video.post), &rkey_of(video.post).unwrap_or_else(|| post_ref.rkey.clone()), None);
                    Ok(Resolution::from(self.video(&video, url, webpage).await?))
                }
            },
            None => {
                let entries: Vec<PlaylistEntry> = media
                    .iter()
                    .enumerate()
                    .map(|(index, item)| match item {
                        Media::External(link) => PlaylistEntry {
                            url: link.clone(),
                            title: None,
                            duration: None,
                        },
                        Media::Video(video) => PlaylistEntry {
                            url: page(
                                &handle,
                                &post_ref.rkey,
                                Some(if index == 0 && own_is_first { Pick::Own } else { Pick::Quote }),
                            ),
                            title: video.post["record"]["text"]
                                .as_str()
                                .or(video.post["value"]["text"].as_str())
                                .and_then(clean_title),
                            duration: None,
                        },
                    })
                    .collect();
                Ok(Resolution::Playlist(Playlist {
                    resolver: PLATFORM.into(),
                    id: Some(post_ref.rkey.clone()),
                    title: post["record"]["text"].as_str().and_then(clean_title),
                    total: Some(entries.len()),
                    entries,
                }))
            }
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
                pick: None,
                actor: "bsky.app".into(),
                rkey: "3mk4lzkrnk22d".into()
            })
        );
        assert_eq!(
            link("https://bsky.app/profile/did:plc:z72i7hdynmk6r22z27h6tvur/post/3mk4lzkrnk22d"),
            Some(PostRef {
                pick: None,
                actor: "did:plc:z72i7hdynmk6r22z27h6tvur".into(),
                rkey: "3mk4lzkrnk22d".into()
            })
        );
        assert_eq!(link("https://bsky.app/profile/bsky.app"), None);
        assert_eq!(link("https://bsky.app/profile/bsky.app/post/short").unwrap().rkey, "short");
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
        assert_eq!(resolved.variants.len(), 3, "two renditions and the original upload");
        assert_eq!(resolved.variants[0].height, Some(800));
        let original = resolved.variants.iter().find(|v| v.format_id.as_deref() == Some("blob")).unwrap();
        assert!(original.url.as_str().starts_with("https://bsky.social/xrpc/com.atproto.sync.getBlob?did="));
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

    #[test]
    fn at_uris_dev_hosts_and_media_picks_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("at://blu3blue.bsky.social/app.bsky.feed.post/3l4omssdl632g"),
            Some(PostRef {
                actor: "blu3blue.bsky.social".into(),
                rkey: "3l4omssdl632g".into(),
                pick: None
            })
        );
        assert_eq!(
            link("https://main.bsky.dev/profile/bsky.app/post/3mk4lzkrnk22d"),
            Some(PostRef {
                actor: "bsky.app".into(),
                rkey: "3mk4lzkrnk22d".into(),
                pick: None
            })
        );
        assert_eq!(
            link("https://bsky.app/profile/bsky.app/post/3mk4lzkrnk22d#media=quote").unwrap().pick,
            Some(Pick::Quote)
        );
        assert_eq!(
            link("https://bsky.app/profile/bsky.app/post/3mk4lzkrnk22d#media=own").unwrap().pick,
            Some(Pick::Own)
        );
        assert_eq!(link("at://bsky.app/app.bsky.feed.like/3mk4lzkrnk22d"), None);
    }
}
