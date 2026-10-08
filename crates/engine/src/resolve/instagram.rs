//! Instagram posts, reels and IGTV, through the web app's GraphQL query as a visitor or a
//! logged-in session, and through the embed page when the query is walled off, which it
//! is for visitors from most networks: asked for the way a browser navigates to it, the
//! embed page carries the post's GraphQL record in its context JSON. The embed withholds
//! the video of a post that uses licensed music, so such a post is read from its own page,
//! which renders the media record into its script data. A post carrying several videos
//! becomes a playlist of them.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};
use url::Url;

use super::page::{Page, json_after, unescape_json_string};
use super::util::{find_key, parse_duration};
use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionCheck, SessionSupport, Tag, Variant, VariantKind, clean_title, fetch_ok,
    navigation_headers,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "instagram";
const APP_ID: &str = "936619743392459";
const GRAPHQL: &str = "https://www.instagram.com/graphql/query";
/// The `PolarisPostActionLoadPostQueryQuery` the web app registers.
const DOC_ID: &str = "8845758582119845";
const CURRENT_USER: &str = "https://www.instagram.com/api/v1/accounts/current_user/?edit=true";

static RE_SHORTCODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(?:[^/]+/)?(?:p|reel|reels|tv)/([A-Za-z0-9_-]{5,})").unwrap());
static RE_VIDEO_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\\?"video_url\\?":\\?"((?:[^"\\]|\\.)+?)\\?""#).unwrap());
static RE_USERNAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\\?"username\\?":\\?"([A-Za-z0-9_.]+)\\?""#).unwrap());
/// The embed page's context, a JSON document held as a string in the page's script data.
static RE_CONTEXT_JSON: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""contextJSON":"((?:[^"\\]|\\.)*)""#).unwrap());
static RE_MPD_DURATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"mediaPresentationDuration="([^"]+)""#).unwrap());
const POST_PAGE_MARKERS: [&str; 2] = [
    "xig_polaris_media",
    "xdt_api__v1__media__shortcode__web_info",
];

pub fn shortcode(url: &Url) -> Option<String> {
    RE_SHORTCODE.captures(url.path()).map(|c| c[1].to_string())
}

/// One video of a post.
#[derive(Debug, Clone, PartialEq)]
struct Video {
    /// Position among the post's items, counted from 1 as the site's img_index does
    index: usize,
    url: Url,
    width: Option<u32>,
    height: Option<u32>,
    duration: Option<Duration>,
    thumbnail: Option<Url>,
}

/// A post as either source describes it.
#[derive(Debug, Clone, PartialEq, Default)]
struct Post {
    videos: Vec<Video>,
    /// How many items the post carries, videos or not.
    items: usize,
    caption: Option<String>,
    username: Option<String>,
    full_name: Option<String>,
    taken_at: Option<jiff::Timestamp>,
}

fn video_of(node: &Value, index: usize) -> Option<Video> {
    if node["is_video"].as_bool() != Some(true) {
        return None;
    }
    let url = Url::parse(node["video_url"].as_str()?).ok()?;
    Some(Video {
        index,
        url,
        width: node["dimensions"]["width"].as_u64().map(|w| w as u32),
        height: node["dimensions"]["height"].as_u64().map(|h| h as u32),
        duration: node["video_duration"]
            .as_f64()
            .filter(|d| *d > 0.0)
            .map(Duration::from_secs_f64),
        thumbnail: node["display_url"]
            .as_str()
            .and_then(|u| Url::parse(u).ok()),
    })
}

/// The post a `shortcode_media` node describes, whichever of its shapes it comes in.
fn post_of(media: &Value) -> Post {
    let children: Vec<&Value> = media["edge_sidecar_to_children"]["edges"]
        .as_array()
        .map(|edges| edges.iter().map(|e| &e["node"]).collect())
        .unwrap_or_default();
    let (videos, items) = if children.is_empty() {
        (video_of(media, 1).into_iter().collect(), 1)
    } else {
        (
            children
                .iter()
                .enumerate()
                .filter_map(|(i, c)| video_of(c, i + 1))
                .collect(),
            children.len(),
        )
    };
    Post {
        videos,
        items,
        caption: media
            .pointer("/edge_media_to_caption/edges/0/node/text")
            .and_then(Value::as_str)
            .and_then(clean_title),
        username: media["owner"]["username"].as_str().map(String::from),
        full_name: media["owner"]["full_name"].as_str().and_then(clean_title),
        taken_at: media["taken_at_timestamp"]
            .as_i64()
            .and_then(|t| jiff::Timestamp::from_second(t).ok()),
    }
}

/// The post's GraphQL record from the embed page's context JSON.
fn context_media(html: &str) -> Option<Value> {
    let raw = RE_CONTEXT_JSON.captures(html)?;
    let text: String = serde_json::from_str(&format!("\"{}\"", &raw[1])).ok()?;
    let context: Value = serde_json::from_str(&text).ok()?;
    let media = context.pointer("/gql_data/shortcode_media")?;
    media.is_object().then(|| media.clone())
}

/// What the post page renders for the post, or whether it withholds it from visitors
enum PageRecord {
    Item(Value),
    Gated,
    Missing,
}

/// The media record in the post page's script data, in its visitor or logged-in shape
fn page_record(html: &str, url: &Url) -> PageRecord {
    let page = Page::parse(html, url);
    let selector =
        scraper::Selector::parse("script[type='application/json'][data-sjs]").expect("valid");
    let mut gated = false;
    for script in page.document().select(&selector) {
        let text: String = script.text().collect();
        if !POST_PAGE_MARKERS.iter().any(|m| text.contains(m)) {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        if let Some(media) = find_key(&value, "xig_polaris_media") {
            let item = &media["if_not_gated_logged_out"];
            if item.is_object() {
                return PageRecord::Item(item.clone());
            }
            gated = true;
        }
        if let Some(item) = find_key(&value, "xdt_api__v1__media__shortcode__web_info")
            .and_then(|info| info["items"][0].as_object())
        {
            return PageRecord::Item(Value::Object(item.clone()));
        }
    }
    if gated {
        PageRecord::Gated
    } else {
        PageRecord::Missing
    }
}

/// The best of a media item's video versions, type 101 first, then the tallest
fn item_video(item: &Value, index: usize) -> Option<Video> {
    let versions = item["video_versions"].as_array()?;
    let version = versions
        .iter()
        .find(|v| v["type"].as_u64() == Some(101))
        .or_else(|| {
            versions
                .iter()
                .max_by_key(|v| v["height"].as_u64().unwrap_or(0))
        })?;
    let url = Url::parse(version["url"].as_str()?).ok()?;
    let duration = item["video_duration"]
        .as_f64()
        .filter(|d| *d > 0.0)
        .map(Duration::from_secs_f64)
        .or_else(|| {
            let manifest = item["video_dash_manifest"].as_str()?;
            parse_duration(&RE_MPD_DURATION.captures(manifest)?[1])
        });
    Some(Video {
        index,
        url,
        width: version["width"]
            .as_u64()
            .or_else(|| item["original_width"].as_u64())
            .map(|w| w as u32),
        height: version["height"]
            .as_u64()
            .or_else(|| item["original_height"].as_u64())
            .map(|h| h as u32),
        duration,
        thumbnail: item
            .pointer("/image_versions2/candidates/0/url")
            .and_then(Value::as_str)
            .and_then(|u| Url::parse(u).ok()),
    })
}

/// The post a page's media item describes, carousel items included
fn post_of_item(item: &Value) -> Post {
    let children: Vec<&Value> = item["carousel_media"]
        .as_array()
        .map(|c| c.iter().collect())
        .unwrap_or_default();
    let (videos, items) = if children.is_empty() {
        (item_video(item, 1).into_iter().collect(), 1)
    } else {
        (
            children
                .iter()
                .enumerate()
                .filter_map(|(i, c)| item_video(c, i + 1))
                .collect(),
            children.len(),
        )
    };
    Post {
        videos,
        items,
        caption: item["caption"]["text"].as_str().and_then(clean_title),
        username: item["user"]["username"].as_str().map(String::from),
        full_name: item["user"]["full_name"].as_str().and_then(clean_title),
        taken_at: item["taken_at"]
            .as_i64()
            .and_then(|t| jiff::Timestamp::from_second(t).ok()),
    }
}

pub struct InstagramResolver {
    http: Http,
}

impl InstagramResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    fn csrf(&self) -> Option<String> {
        self.http
            .jar(PLATFORM)
            .get("csrftoken")
            .map(|c| c.value.clone())
    }

    /// The post through the GraphQL query.
    async fn graphql(&self, code: &str, origin: &Url) -> Result<Post, ResolveError> {
        let variables = json!({
            "shortcode": code,
            "fetch_tagged_user_count": null,
            "hoisted_comment_id": null,
            "hoisted_reply_id": null
        })
        .to_string();
        let mut request = self
            .http
            .post(Url::parse(GRAPHQL).expect("valid"))
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("x-ig-app-id", APP_ID)
            .header("x-fb-lsd", "AVqbxe3J_YA")
            .header("x-asbd-id", "129477")
            .header("referer", &format!("https://www.instagram.com/p/{code}/"))
            .form(&[
                ("doc_id", DOC_ID),
                ("variables", variables.as_str()),
                ("lsd", "AVqbxe3J_YA"),
                ("server_timestamps", "true"),
            ]);
        if let Some(csrf) = self.csrf() {
            request = request.header("x-csrftoken", &csrf);
        }
        let response = request.send().await?;
        let status = response.status;
        if status.as_u16() == 404 {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        if status.as_u16() == 429 {
            return Err(ResolveError::RateLimited(origin.clone()));
        }
        if !status.is_success() {
            return Err(ResolveError::unavailable(
                origin,
                format!("the query answered HTTP {status}"),
            ));
        }
        let value: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, format!("query JSON: {e}")))?;
        match value.pointer("/data/xdt_shortcode_media") {
            Some(media) if media.is_object() => Ok(post_of(media)),
            Some(Value::Null) => Err(ResolveError::NotFound(origin.clone())),
            _ => {
                let message = value["message"]
                    .as_str()
                    .or_else(|| value["errors"][0]["message"].as_str())
                    .unwrap_or("the query answered without the post");
                if value["require_login"].as_bool() == Some(true)
                    || message.to_lowercase().contains("login")
                {
                    Err(ResolveError::login_required(origin, PLATFORM, message))
                } else {
                    Err(ResolveError::unavailable(origin, message))
                }
            }
        }
    }

    /// The post through its embed page, which visitors reach when the query is walled.
    async fn embed(&self, code: &str, origin: &Url) -> Result<Post, ResolveError> {
        let embed_url = Url::parse(&format!(
            "https://www.instagram.com/p/{code}/embed/captioned/"
        ))
        .expect("valid");
        let fetched = fetch_ok(
            &self.http,
            &embed_url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        let html = fetched.text();
        if let Some(media) = context_media(&html) {
            return Ok(post_of(&media));
        }
        if let Some(extra) = json_after(&html, "__additionalDataLoaded('extra',")
            .and_then(|v| v.get("shortcode_media").cloned())
        {
            return Ok(post_of(&extra));
        }
        let page = Page::parse(&html, &embed_url);
        let Some(url) = RE_VIDEO_URL
            .captures(&html)
            .map(|c| unescape_json_string(&unescape_json_string(&c[1])))
            .and_then(|u| Url::parse(&u).ok())
        else {
            if html.contains("/accounts/login/") && !html.contains("video_url") {
                return Err(ResolveError::login_required(
                    origin,
                    PLATFORM,
                    "the embed page asks to log in",
                ));
            }
            return Err(ResolveError::unavailable(
                origin,
                "the embed page shows no video",
            ));
        };
        Ok(Post {
            videos: vec![Video {
                index: 1,
                url,
                width: None,
                height: None,
                duration: None,
                thumbnail: page.meta("og:image").and_then(|u| Url::parse(&u).ok()),
            }],
            items: 1,
            caption: page.title(),
            username: RE_USERNAME.captures(&html).map(|c| c[1].to_string()),
            full_name: None,
            taken_at: None,
        })
    }

    /// The post through its own page, which carries the video the embed withholds
    async fn page(&self, code: &str, origin: &Url) -> Result<Post, ResolveError> {
        let page_url = Url::parse(&format!("https://www.instagram.com/p/{code}/")).expect("valid");
        let fetched = fetch_ok(
            &self.http,
            &page_url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        if fetched.url.path().starts_with("/accounts/login") {
            return Err(ResolveError::login_required(
                origin,
                PLATFORM,
                "the post page asks to log in",
            ));
        }
        match page_record(&fetched.text(), &page_url) {
            PageRecord::Item(item) => Ok(post_of_item(&item)),
            PageRecord::Gated => Err(ResolveError::login_required(
                origin,
                PLATFORM,
                "the post is shown to logged-in viewers only",
            )),
            PageRecord::Missing => Err(ResolveError::unavailable(
                origin,
                "the post page carries no media record",
            )),
        }
    }

    fn resolved(&self, post: &Post, video: &Video, code: &str, index: Option<usize>) -> Resolved {
        let handle = post.username.as_deref().unwrap_or("");
        let mut variant = Variant::new(video.url.clone(), VariantKind::File);
        variant.container = Some(Container::Mp4);
        variant.video = Some(VideoCodec::H264);
        variant.audio = Some(AudioCodec::Aac);
        variant.width = video.width;
        variant.height = video.height;
        variant.duration = video.duration;
        variant.headers = vec![(
            "referer".to_string(),
            "https://www.instagram.com/".to_string(),
        )];
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(match index {
            Some(i) => format!("{code}_{i}"),
            None => code.to_string(),
        });
        resolved.title = post
            .caption
            .clone()
            .or_else(|| clean_title(&format!("Instagram post by @{handle}")));
        resolved.description = post.caption.clone();
        resolved.uploader = post
            .full_name
            .clone()
            .or_else(|| (!handle.is_empty()).then(|| format!("@{handle}")));
        resolved.uploader_url = (!handle.is_empty())
            .then(|| Url::parse(&format!("https://www.instagram.com/{handle}/")).ok())
            .flatten();
        resolved.uploaded_at = post.taken_at;
        resolved.duration = video.duration;
        resolved.thumbnail = video.thumbnail.clone();
        resolved.webpage_url = Url::parse(&format!("https://www.instagram.com/p/{code}/")).ok();
        resolved.variants = vec![variant];
        resolved
    }
}

#[async_trait]
impl Resolver for InstagramResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Instagram",
            hosts: &["instagram.com"],
            features: &["posts", "reels", "igtv", "carousels"],
            formats: &["mp4"],
            media: &[MediaKind::Video],
            tags: &[Tag::Basic, Tag::Social, Tag::Video],
            session: SessionSupport::Optional,
            on_by_default: true,
            examples: &[
                "https://www.instagram.com/p/aye83DjauH/",
                "https://www.instagram.com/reel/DeMzMqyRgYT/",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        matches!(url.scheme(), "http" | "https")
            && url
                .host_str()
                .is_some_and(|h| h == "instagram.com" || h.ends_with(".instagram.com"))
            && shortcode(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let code = shortcode(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let index: Option<usize> = url
            .query_pairs()
            .find(|(k, _)| k == "img_index")
            .and_then(|(_, v)| v.parse().ok())
            .filter(|i| *i >= 1);
        let post = match self.graphql(&code, url).await {
            Ok(post) => post,
            Err(error @ (ResolveError::NotFound(_) | ResolveError::RateLimited(_))) => {
                return Err(error);
            }
            Err(error) => {
                tracing::debug!(%url, "Instagram query failed: {error}. Trying the embed page.");
                match self.embed(&code, url).await {
                    Ok(post) if !post.videos.is_empty() => post,
                    Ok(_) => {
                        tracing::debug!(%url, "Instagram embed shows no video. Trying the post page.");
                        self.page(&code, url).await?
                    }
                    Err(error) => {
                        tracing::debug!(%url, "Instagram embed failed: {error}. Trying the post page.");
                        self.page(&code, url).await?
                    }
                }
            }
        };
        if post.videos.is_empty() {
            return Err(ResolveError::unavailable(url, "the post has no video"));
        }
        match index {
            Some(i) if post.items > 1 => {
                let video = post.videos.iter().find(|v| v.index == i).ok_or_else(|| {
                    ResolveError::unavailable(url, format!("item {i} of the post is not a video"))
                })?;
                Ok(Resolution::from(self.resolved(
                    &post,
                    video,
                    &code,
                    Some(i),
                )))
            }
            _ if post.videos.len() > 1 => {
                let handle = post.username.as_deref().unwrap_or("");
                Ok(Resolution::Playlist(Playlist {
                    resolver: PLATFORM.to_string(),
                    id: Some(code.clone()),
                    title: post
                        .caption
                        .clone()
                        .or_else(|| clean_title(&format!("Instagram post by @{handle}"))),
                    total: None,
                    entries: post
                        .videos
                        .iter()
                        .enumerate()
                        .filter_map(|(i, video)| {
                            Some(PlaylistEntry {
                                url: Url::parse(&format!(
                                    "https://www.instagram.com/p/{code}/?img_index={}",
                                    video.index
                                ))
                                .ok()?,
                                title: post
                                    .caption
                                    .clone()
                                    .map(|c| format!("{c} ({}/{})", i + 1, post.videos.len())),
                                duration: video.duration,
                            })
                        })
                        .collect(),
                }))
            }
            _ => Ok(Resolution::from(self.resolved(
                &post,
                &post.videos[0],
                &code,
                None,
            ))),
        }
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        if self.http.jar(PLATFORM).get("sessionid").is_none() {
            return Ok(SessionCheck::LoggedOut);
        }
        let me = Url::parse(CURRENT_USER).expect("valid");
        let mut request = self
            .http
            .get(me.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("x-ig-app-id", APP_ID)
            .header("x-requested-with", "XMLHttpRequest");
        if let Some(csrf) = self.csrf() {
            request = request.header("x-csrftoken", &csrf);
        }
        let response = request.send().await?;
        if !response.is_success() {
            return Ok(SessionCheck::LoggedOut);
        }
        let value: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(&me, e.to_string()))?;
        Ok(match value["user"]["username"].as_str() {
            Some(name) if !name.is_empty() => SessionCheck::LoggedIn {
                account: format!("@{name}"),
            },
            _ => SessionCheck::LoggedOut,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

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
                headers: vec![("content-type".into(), content_type.into())],
                body: RecordedBody::Text(body),
                truncated: false,
            },
        }
    }

    fn video_node(url: &str) -> Value {
        json!({"is_video": true, "video_url": url, "dimensions": {"width": 1080, "height": 1920}, "video_duration": 8.5, "display_url": "https://scontent.cdninstagram.com/thumb.jpg"})
    }

    #[tokio::test]
    async fn a_reel_resolves_through_the_query() {
        let mut fixture = Fixture::new("instagram", None);
        let mut media = video_node("https://scontent.cdninstagram.com/v.mp4");
        media["owner"] = json!({"username": "instagram", "full_name": "Instagram"});
        media["edge_media_to_caption"] = json!({"edges": [{"node": {"text": "Hello reel"}}]});
        media["taken_at_timestamp"] = json!(1_700_000_000);
        fixture.exchanges.push(exchange(
            "POST",
            GRAPHQL,
            200,
            "application/json",
            json!({"data": {"xdt_shortcode_media": media}, "status": "ok"}).to_string(),
        ));
        let resolver = InstagramResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.instagram.com/reel/aye83DjauH/?utm_source=x").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.title.as_deref(), Some("Hello reel"));
        assert_eq!(resolved.uploader.as_deref(), Some("Instagram"));
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(8.5)));
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].height, Some(1920));
        assert!(resolved.uploaded_at.is_some());
    }

    #[tokio::test]
    async fn carousels_become_playlists_and_items_are_picked() {
        let sidecar = |caption: &str| {
            let mut media = json!({"is_video": false, "owner": {"username": "someone"}});
            media["edge_media_to_caption"] = json!({"edges": [{"node": {"text": caption}}]});
            media["edge_sidecar_to_children"] = json!({"edges": [
                {"node": {"is_video": false, "display_url": "https://scontent.cdninstagram.com/photo.jpg"}},
                {"node": video_node("https://scontent.cdninstagram.com/one.mp4")},
                {"node": video_node("https://scontent.cdninstagram.com/two.mp4")}
            ]});
            json!({"data": {"xdt_shortcode_media": media}, "status": "ok"}).to_string()
        };
        let mut fixture = Fixture::new("instagram", None);
        for _ in 0..3 {
            fixture.exchanges.push(exchange(
                "POST",
                GRAPHQL,
                200,
                "application/json",
                sidecar("Three things"),
            ));
        }
        let resolver = InstagramResolver::new(Http::replay(fixture));
        let playlist = match resolver
            .resolve(&Url::parse("https://www.instagram.com/someone/p/Cabcdefgh/").unwrap())
            .await
            .unwrap()
        {
            Resolution::Playlist(playlist) => playlist,
            Resolution::Media(_) => panic!("two videos make a playlist"),
        };
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[1].url.as_str(),
            "https://www.instagram.com/p/Cabcdefgh/?img_index=3"
        );
        assert_eq!(
            playlist.entries[1].title.as_deref(),
            Some("Three things (2/2)")
        );
        let second = resolver
            .resolve(&playlist.entries[1].url)
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(
            second.variants[0].url.as_str(),
            "https://scontent.cdninstagram.com/two.mp4"
        );
        assert_eq!(second.id.as_deref(), Some("Cabcdefgh_3"));
        let photo = resolver
            .resolve(&Url::parse("https://www.instagram.com/p/Cabcdefgh/?img_index=1").unwrap())
            .await
            .unwrap_err();
        assert!(
            photo
                .to_string()
                .contains("item 1 of the post is not a video"),
            "{photo}"
        );
    }

    #[tokio::test]
    async fn a_walled_query_falls_back_to_the_embed_page() {
        let mut fixture = Fixture::new("instagram", None);
        fixture.exchanges.push(exchange("POST", GRAPHQL, 200, "application/json", json!({"data": null, "status": "fail", "message": "Please wait a few minutes before you try again."}).to_string()));
        fixture.exchanges.push(exchange(
            "GET",
            "https://www.instagram.com/p/aye83DjauH/embed/captioned/",
            200,
            "text/html",
            r#"<html><head><title>Instagram post</title><meta property="og:image" content="https://scontent.cdninstagram.com/t.jpg"></head><body><script>window.__additionalDataLoaded('extra',{"shortcode_media":{"is_video":true,"video_url":"https://scontent.cdninstagram.com/embed.mp4","owner":{"username":"instagram"},"video_duration":3.0}});</script></body></html>"#.to_string(),
        ));
        let resolver = InstagramResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://www.instagram.com/p/aye83DjauH/").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(
            resolved.variants[0].url.as_str(),
            "https://scontent.cdninstagram.com/embed.mp4"
        );
        assert_eq!(resolved.uploader.as_deref(), Some("@instagram"));

        // The embed page as it is served today: the post's record in its context JSON.
        let mut fixture = Fixture::new("instagram", None);
        fixture.exchanges.push(exchange(
            "POST",
            GRAPHQL,
            401,
            "application/json",
            json!({"message": "Please wait a few minutes before you try again.", "require_login": true, "status": "fail"}).to_string(),
        ));
        let context = json!({
            "context": {"type": "GraphVideo", "shortcode": "aye83DjauH", "copyright_blocked": false},
            "gql_data": {"shortcode_media": {
                "__typename": "GraphVideo", "shortcode": "aye83DjauH", "is_video": true,
                "video_url": "https://scontent.cdninstagram.com/o1/v/context.mp4?efg=1&_nc_ht=x",
                "video_duration": 8.742, "dimensions": {"height": 612, "width": 612},
                "display_url": "https://scontent.cdninstagram.com/v/t51/thumb.jpg",
                "edge_media_to_caption": {"edges": [{"node": {"text": "If Abel was a booger"}}]},
                "owner": {"id": "2815873", "username": "naomipq", "is_verified": false}
            }}
        })
        .to_string();
        let script = json!({"define": [], "require": [["PolarisEmbedSimple", "init", [], [{"contextJSON": context}]]]}).to_string();
        fixture.exchanges.push(exchange(
            "GET",
            "https://www.instagram.com/p/aye83DjauH/embed/captioned/",
            200,
            "text/html",
            format!(r#"<html><head><title>Instagram</title></head><body><script>requireLazy(["ServerJS"],function(ServerJS){{var s=(new ServerJS());s.handle({script});}});</script></body></html>"#),
        ));
        let resolver = InstagramResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://www.instagram.com/reel/aye83DjauH/").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(
            resolved.variants[0].url.as_str(),
            "https://scontent.cdninstagram.com/o1/v/context.mp4?efg=1&_nc_ht=x"
        );
        assert_eq!(resolved.title.as_deref(), Some("If Abel was a booger"));
        assert_eq!(resolved.uploader.as_deref(), Some("@naomipq"));
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(8.742)));
        assert_eq!(resolved.variants[0].width, Some(612));

        // An embed page carrying the video only in its script text still yields it.
        let mut fixture = Fixture::new("instagram", None);
        fixture.exchanges.push(exchange(
            "POST",
            GRAPHQL,
            500,
            "application/json",
            "{}".into(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://www.instagram.com/p/aye83DjauH/embed/captioned/",
            200,
            "text/html",
            r#"<html><head><title>Instagram post</title></head><body><script>var s = "{\"video_url\":\"https:\\/\\/scontent.cdninstagram.com\\/raw.mp4?a=1\\u0026b=2\",\"username\":\"someone\"}";</script></body></html>"#.to_string(),
        ));
        let resolver = InstagramResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://www.instagram.com/p/aye83DjauH/").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(
            resolved.variants[0].url.as_str(),
            "https://scontent.cdninstagram.com/raw.mp4?a=1&b=2"
        );
        assert_eq!(resolved.uploader.as_deref(), Some("@someone"));

        let mut fixture = Fixture::new("instagram", None);
        fixture.exchanges.push(exchange(
            "POST",
            GRAPHQL,
            200,
            "application/json",
            json!({"data": {"xdt_shortcode_media": null}}).to_string(),
        ));
        let resolver = InstagramResolver::new(Http::replay(fixture));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.instagram.com/p/aye83DjauH/").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    /// The post page's script data as the site renders it for a visitor
    fn visitor_page(media: Value) -> String {
        let data = json!({"require": [["ScheduledServerJS", "handle", null, [{"__bbox": {"require": [["RelayPrefetchedStreamCache", "next", [], ["adp_PolarisLoggedOutDesktopWWWPostRootContentQueryRelayPreloader_6ac7232d0df3e7e34796021", {"__bbox": {"complete": true, "result": {"data": {"xig_polaris_media": media}, "extensions": {"is_final": true}}}}]]]}}]]]});
        format!(
            r#"<html><head><script type="application/json" data-content-len="1" data-sjs>{{"define":[]}}</script><script type="application/json" data-sjs>{data}</script></head><body></body></html>"#
        )
    }

    fn reel_item() -> Value {
        json!({
            "__typename": "XDTMediaDict", "code": "DeMzMqyRgYT", "pk": "4002799339771921939",
            "product_type": "clips", "media_type": 2, "taken_at": 1791390938, "has_audio": true,
            "original_width": 1080, "original_height": 1920,
            "caption": {"text": "Flour is a neutral. I heard it goes with everything\n\n#fyp"},
            "user": {"username": "creme.de.la.crumb", "full_name": "Crème de la Crumb", "is_private": false},
            "image_versions2": {"candidates": [{"url": "https://scontent.cdninstagram.com/v/t51/reel.jpg?a=1", "width": 1080, "height": 1920}]},
            "video_dash_manifest": "<?xml version=\"1.0\"?><MPD type=\"static\" mediaPresentationDuration=\"PT8.334513S\"></MPD>",
            "video_versions": [
                {"type": 103, "url": "https://scontent.cdninstagram.com/o1/v/low.mp4", "width": null, "height": null},
                {"type": 101, "url": "https://scontent.cdninstagram.com/o1/v/best.mp4?efg=x", "width": null, "height": null},
                {"type": 102, "url": "https://scontent.cdninstagram.com/o1/v/mid.mp4", "width": null, "height": null}
            ]
        })
    }

    /// The embed page of a post with licensed music, carrying the record without its video
    fn blocked_embed(code: &str) -> String {
        let context = json!({
            "context": {"type": "GraphVideo", "shortcode": code, "copyright_blocked": true},
            "gql_data": {"shortcode_media": {
                "__typename": "GraphVideo", "shortcode": code, "is_video": true, "product_type": "clips",
                "video_duration": 8.334, "dimensions": {"height": 1136, "width": 640},
                "display_url": "https://scontent.cdninstagram.com/v/t51/thumb.jpg",
                "owner": {"id": "72767094298", "username": "creme.de.la.crumb"}
            }}
        })
        .to_string();
        let script = json!({"define": [], "require": [["PolarisEmbedSimple", "init", [], [{"contextJSON": context}]]]}).to_string();
        format!(
            r#"<html><head><title>Instagram</title></head><body><script>requireLazy(["ServerJS"],function(ServerJS){{var s=(new ServerJS());s.handle({script});}});</script></body></html>"#
        )
    }

    #[tokio::test]
    async fn a_copyright_blocked_embed_falls_back_to_the_post_page() {
        let mut fixture = Fixture::new("instagram", None);
        fixture.exchanges.push(exchange(
            "POST",
            GRAPHQL,
            403,
            "text/html; charset=utf-8",
            "<html><title>Page Not Found</title></html>".into(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://www.instagram.com/p/DeMzMqyRgYT/embed/captioned/",
            200,
            "text/html",
            blocked_embed("DeMzMqyRgYT"),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://www.instagram.com/p/DeMzMqyRgYT/",
            200,
            "text/html",
            visitor_page(json!({"__typename": "XDTMediaDict", "pk": "4002799339771921939", "code": "DeMzMqyRgYT", "if_not_gated_logged_out": reel_item(), "gating_ruling": null, "id": "4002799339771921939_72767094298"})),
        ));
        let resolver = InstagramResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.instagram.com/reel/DeMzMqyRgYT/?stkn=cXlpaGEzdGJ6ODln")
            .unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(
            resolved.variants[0].url.as_str(),
            "https://scontent.cdninstagram.com/o1/v/best.mp4?efg=x"
        );
        assert_eq!(resolved.variants[0].width, Some(1080));
        assert_eq!(resolved.variants[0].height, Some(1920));
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(8.334513)));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Flour is a neutral. I heard it goes with everything #fyp")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("Crème de la Crumb"));
        assert_eq!(
            resolved.uploader_url.as_ref().map(Url::as_str),
            Some("https://www.instagram.com/creme.de.la.crumb/")
        );
        assert_eq!(
            resolved.thumbnail.as_ref().map(Url::as_str),
            Some("https://scontent.cdninstagram.com/v/t51/reel.jpg?a=1")
        );
        assert!(resolved.uploaded_at.is_some());
    }

    #[tokio::test]
    async fn a_logged_in_post_page_yields_its_carousel() {
        let page = || {
            let item = json!({
                "code": "Cabcdefgh", "media_type": 8, "taken_at": 1_700_000_000,
                "caption": {"text": "Three things"}, "user": {"username": "someone", "full_name": "Some One"},
                "carousel_media": [
                    {"media_type": 1, "image_versions2": {"candidates": [{"url": "https://scontent.cdninstagram.com/photo.jpg"}]}},
                    {"media_type": 2, "video_duration": 4.5, "original_width": 720, "original_height": 1280, "video_versions": [{"type": 101, "url": "https://scontent.cdninstagram.com/one.mp4", "width": 720, "height": 1280}]},
                    {"media_type": 2, "video_duration": 6.0, "video_versions": [{"type": 102, "url": "https://scontent.cdninstagram.com/two-mid.mp4", "width": 480, "height": 854}, {"type": 101, "url": "https://scontent.cdninstagram.com/two.mp4", "width": 720, "height": 1280}]}
                ]
            });
            let data = json!({"require": [["ScheduledServerJS", "handle", null, [{"__bbox": {"require": [["RelayPrefetchedStreamCache", "next", [], ["PolarisPostRootQueryRelayPreloader_1", {"__bbox": {"result": {"data": {"xdt_api__v1__media__shortcode__web_info": {"items": [item]}}}}}]]]}}]]]});
            format!(
                r#"<html><head><script type="application/json" data-sjs>{data}</script></head><body></body></html>"#
            )
        };
        let mut fixture = Fixture::new("instagram", None);
        for _ in 0..2 {
            fixture.exchanges.push(exchange("POST", GRAPHQL, 200, "application/json", json!({"data": null, "status": "fail", "message": "Please wait a few minutes before you try again."}).to_string()));
            fixture.exchanges.push(exchange(
                "GET",
                "https://www.instagram.com/p/Cabcdefgh/embed/captioned/",
                200,
                "text/html",
                "<html><head><title>Instagram</title></head><body></body></html>".into(),
            ));
            fixture.exchanges.push(exchange(
                "GET",
                "https://www.instagram.com/p/Cabcdefgh/",
                200,
                "text/html",
                page(),
            ));
        }
        let resolver = InstagramResolver::new(Http::replay(fixture));
        let playlist = match resolver
            .resolve(&Url::parse("https://www.instagram.com/p/Cabcdefgh/").unwrap())
            .await
            .unwrap()
        {
            Resolution::Playlist(playlist) => playlist,
            Resolution::Media(_) => panic!("two videos make a playlist"),
        };
        assert_eq!(playlist.title.as_deref(), Some("Three things"));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[1].duration,
            Some(Duration::from_secs_f64(6.0))
        );
        let second = resolver
            .resolve(&playlist.entries[1].url)
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(
            second.variants[0].url.as_str(),
            "https://scontent.cdninstagram.com/two.mp4"
        );
        assert_eq!(second.variants[0].height, Some(1280));
        assert_eq!(second.id.as_deref(), Some("Cabcdefgh_3"));
        assert_eq!(second.uploader.as_deref(), Some("Some One"));
    }

    #[tokio::test]
    async fn a_gated_post_page_asks_for_a_session() {
        let mut fixture = Fixture::new("instagram", None);
        fixture.exchanges.push(exchange(
            "POST",
            GRAPHQL,
            403,
            "text/html",
            "<html></html>".into(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://www.instagram.com/p/DeMzMqyRgYT/embed/captioned/",
            200,
            "text/html",
            blocked_embed("DeMzMqyRgYT"),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://www.instagram.com/p/DeMzMqyRgYT/",
            200,
            "text/html",
            visitor_page(json!({"__typename": "XDTMediaDict", "code": "DeMzMqyRgYT", "if_not_gated_logged_out": null, "gating_ruling": "sensitive"})),
        ));
        let resolver = InstagramResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://www.instagram.com/reel/DeMzMqyRgYT/").unwrap())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("logged-in viewers"), "{error}");
    }

    /// Every example link resolves live to a playable video, the embed-withheld reel included
    #[ignore = "reaches the live site: cargo test -- --ignored"]
    #[tokio::test]
    async fn live_examples_resolve() {
        let resolver = InstagramResolver::new(Http::new(crate::http::HttpConfig::default()));
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
            let resolved = tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&url))
                .await
                .expect("resolution timed out")
                .unwrap_or_else(|e| panic!("{link}: {e}"))
                .media()
                .unwrap_or_else(|| panic!("{link}: a single video, not a playlist"));
            assert_eq!(resolved.media, MediaKind::Video, "{link}");
            assert!(
                resolved.variants.iter().any(|v| v.is_playable()),
                "{link}: no playable variant"
            );
            assert!(resolved.duration.is_some(), "{link}: no duration");
            println!(
                "{link}: {:?} by {:?}, {:?}, {}",
                resolved.title, resolved.uploader, resolved.duration, resolved.variants[0].url
            );
        }
    }
}
