//! Tumblr posts with video, through the site's own API with the client token its web app
//! carries: a post's own video blocks, the blocks of the post it reblogs, and a video
//! hosted elsewhere handed on to that host's resolver. A blog's video posts become a
//! playlist.

use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::page::{Page, between};
use super::{
    ClipRange, MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionCheck, SessionSupport, Variant, VariantKind, clean_title, timestamp_hint,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "tumblr";
const SITE: &str = "https://www.tumblr.com/";
const API: &str = "https://www.tumblr.com/api/v2/";
/// The token the web app carries for its own API calls; the page's state names the
/// current one, and this is what it has been.
const WEB_TOKEN: &str = "aIcXSOoTtqrzR8L8YEIOmBeW94c3FmbSNSWAUbxsny9KKx5VFh";
/// The cookie a logged-in tumblr.com session carries.
const SESSION_COOKIE: &str = "logged_in";
const PAGE_SIZE: usize = 20;
const MAX_PAGES: usize = 10;

static RE_BLOG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9-]{0,31}$").unwrap());

/// What a link names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Post { blog: String, id: String },
    /// A blog's video posts.
    Blog(String),
}

fn digits(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_digit())
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    if host == "tumblr.com" || host == "www.tumblr.com" {
        return match segments.as_slice() {
            ["blog", "view", blog, id, ..] | [blog, id, ..] if digits(id) && RE_BLOG.is_match(&blog.to_ascii_lowercase()) => {
                Some(Link::Post {
                    blog: blog.to_ascii_lowercase(),
                    id: id.to_string(),
                })
            }
            ["blog", "view", blog] | [blog] if RE_BLOG.is_match(&blog.to_ascii_lowercase()) => {
                let reserved = ["dashboard", "explore", "search", "settings", "login", "register", "about", "apps", "policy", "tagged", "likes", "following", "inbox", "communities", "live", "tips", "shop", "help", "jobs", "privacy", "new", "blog"];
                (!reserved.contains(&blog.to_ascii_lowercase().as_str()))
                    .then(|| Link::Blog(blog.to_ascii_lowercase()))
            }
            _ => None,
        };
    }
    let blog = host.strip_suffix(".tumblr.com")?;
    let blog = blog.strip_prefix("www.").unwrap_or(blog);
    if !RE_BLOG.is_match(blog) || blog == "assets" || blog == "static" || blog == "api" {
        return None;
    }
    match segments.as_slice() {
        ["post", id, ..] | ["private", id, ..] if digits(id) => Some(Link::Post {
            blog: blog.to_string(),
            id: id.to_string(),
        }),
        ["image", id] if digits(id) => Some(Link::Post {
            blog: blog.to_string(),
            id: id.to_string(),
        }),
        [] | ["archive"] | ["videos"] | ["tagged", _] => Some(Link::Blog(blog.to_string())),
        _ => None,
    }
}

pub struct TumblrResolver {
    http: Http,
    token: Mutex<String>,
}

/// The web client token a page's state names.
pub fn token_in(html: &str) -> Option<String> {
    let state = between(html, "___INITIAL_STATE___\">", "</script>")?;
    let value: Value = serde_json::from_str(state.trim()).ok()?;
    value["apiFetchStore"]["API_TOKEN"]
        .as_str()
        .filter(|t| !t.is_empty())
        .map(String::from)
}

impl TumblrResolver {
    pub fn new(http: Http) -> Self {
        Self {
            http,
            token: Mutex::new(WEB_TOKEN.to_string()),
        }
    }

    fn logged_in(&self) -> bool {
        self.http.jar(PLATFORM).get(SESSION_COOKIE).is_some()
    }

    fn token(&self) -> String {
        self.token.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Reads the token the web app carries now, from a page of the site.
    async fn refresh_token(&self) -> Result<bool, ResolveError> {
        let url = Url::parse(&format!("{SITE}explore/trending")).expect("valid");
        let fetched = super::fetch(&self.http, &url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        match token_in(&fetched.text()) {
            Some(token) if token != self.token() => {
                *self.token.lock().unwrap_or_else(|e| e.into_inner()) = token;
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    /// GETs an API path, with a fresh token when the stored one is refused.
    async fn api(&self, path: &str, origin: &Url) -> Result<Value, ResolveError> {
        for attempt in 0..2 {
            let url = Url::parse(&format!("{API}{path}"))
                .map_err(|e| ResolveError::malformed(origin, e.to_string()))?;
            let response = self
                .http
                .get(url)
                .platform(PLATFORM)
                .user_agent(BROWSER_UA)
                .header("authorization", &format!("Bearer {}", self.token()))
                .header("accept", "application/json;format=camelcase")
                .header("referer", SITE)
                .send()
                .await?;
            let status = response.status.as_u16();
            let answer: Value = response
                .json(MAX_PAGE)
                .await
                .map_err(|e| ResolveError::malformed(origin, format!("API JSON: {e}")))?;
            let meta_status = answer["meta"]["status"].as_u64().unwrap_or(status as u64);
            match meta_status {
                200..=299 => return Ok(answer["response"].clone()),
                401 if attempt == 0 && self.refresh_token().await? => continue,
                401 | 403 => {
                    return Err(if self.logged_in() {
                        ResolveError::unavailable(origin, "the API refused the request")
                    } else {
                        ResolveError::login_required(origin, PLATFORM, "the post is not public")
                    });
                }
                404 => return Err(ResolveError::NotFound(origin.clone())),
                429 => return Err(ResolveError::RateLimited(origin.clone())),
                _ => {
                    let message = answer["errors"][0]["detail"]
                        .as_str()
                        .or_else(|| answer["meta"]["msg"].as_str())
                        .unwrap_or("the API answered with an error");
                    return Err(ResolveError::unavailable(
                        origin,
                        format!("{message} (HTTP {meta_status})"),
                    ));
                }
            }
        }
        Err(ResolveError::unavailable(origin, "the API refused the request"))
    }

    async fn post(&self, blog: &str, id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let response = self
            .api(
                &format!("blog/{blog}/posts/{id}/permalink?fields[blogs]=name,avatar,title,url&reblog_info=true"),
                origin,
            )
            .await?;
        let post = response["timeline"]["elements"]
            .as_array()
            .and_then(|e| e.first())
            .cloned()
            .ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
        let blocks = video_blocks(&post);
        if blocks.is_empty() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        let blog_name = post["blogName"]
            .as_str()
            .or_else(|| post["blog"]["name"].as_str())
            .unwrap_or(blog);
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.to_string());
        resolved.title = post["summary"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| text_block(&post))
            .or_else(|| Some(format!("Post by {blog_name}")));
        resolved.description = text_block(&post);
        resolved.uploader = clean_title(blog_name);
        resolved.uploader_url = Url::parse(&format!("https://{blog_name}.tumblr.com/")).ok();
        resolved.uploaded_at = post["timestamp"]
            .as_i64()
            .and_then(|t| Timestamp::from_second(t).ok());
        resolved.webpage_url = post["postUrl"]
            .as_str()
            .and_then(|u| Url::parse(u).ok())
            .or_else(|| Url::parse(&format!("{SITE}{blog_name}/{id}")).ok());
        resolved.clip = timestamp_hint(origin).map(|start| ClipRange { start, end: None });
        let block = blocks[0];
        if let Some(variant) = hosted_variant(block) {
            resolved.duration = variant.duration;
            resolved.thumbnail = block["poster"]
                .as_array()
                .and_then(|p| p.first())
                .and_then(|p| p["url"].as_str())
                .and_then(|u| Url::parse(u).ok());
            resolved.variants = vec![variant];
            return Ok(Resolution::from(resolved));
        }
        // A video hosted elsewhere: its own resolver takes it.
        let external = block["url"]
            .as_str()
            .or_else(|| block["embedUrl"].as_str())
            .or_else(|| block["embed_url"].as_str())
            .and_then(|u| Url::parse(u).ok())
            .ok_or_else(|| ResolveError::malformed(origin, "the video block names no URL"))?;
        Err(ResolveError::Redirect(external))
    }

    async fn blog(&self, blog: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let mut entries = Vec::new();
        let mut title = None;
        let mut total = None;
        for page in 0..MAX_PAGES {
            let response = self
                .api(
                    &format!(
                        "blog/{blog}/posts?type=video&npf=true&reblog_info=true&limit={PAGE_SIZE}&offset={}",
                        page * PAGE_SIZE
                    ),
                    origin,
                )
                .await?;
            if title.is_none() {
                title = response["blog"]["title"]
                    .as_str()
                    .and_then(clean_title)
                    .or_else(|| response["blog"]["name"].as_str().and_then(clean_title));
                total = response["totalPosts"]
                    .as_u64()
                    .or_else(|| response["total_posts"].as_u64())
                    .map(|t| t as usize);
            }
            let posts: Vec<&Value> = response["posts"].as_array().into_iter().flatten().collect();
            for post in &posts {
                let Some(url) = post["postUrl"]
                    .as_str()
                    .or_else(|| post["post_url"].as_str())
                    .and_then(|u| Url::parse(u).ok())
                else {
                    continue;
                };
                entries.push(PlaylistEntry {
                    url,
                    title: post["summary"].as_str().and_then(clean_title),
                    duration: None,
                });
            }
            if posts.len() < PAGE_SIZE || total.is_some_and(|t| entries.len() >= t) {
                break;
            }
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.into(),
            id: Some(blog.to_string()),
            title: title.map(|t| format!("{t} (videos)")),
            total: total.filter(|t| *t >= entries.len()).or(Some(entries.len())),
            entries,
        }))
    }
}

/// The video blocks of a post: its own, or the ones of the post it reblogs.
fn video_blocks(post: &Value) -> Vec<&Value> {
    let own: Vec<&Value> = post["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|b| b["type"].as_str() == Some("video"))
        .collect();
    if !own.is_empty() {
        return own;
    }
    post["trail"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|t| t["content"].as_array().into_iter().flatten())
        .filter(|b| b["type"].as_str() == Some("video"))
        .collect()
}

/// The first text block of a post, or of the post it reblogs.
fn text_block(post: &Value) -> Option<String> {
    post["content"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(
            post["trail"]
                .as_array()
                .into_iter()
                .flatten()
                .flat_map(|t| t["content"].as_array().into_iter().flatten()),
        )
        .filter(|b| b["type"].as_str() == Some("text"))
        .find_map(|b| b["text"].as_str().and_then(clean_title))
}

/// A video Tumblr hosts itself, as a variant.
fn hosted_variant(block: &Value) -> Option<Variant> {
    let media = &block["media"];
    let media = if media.is_array() { &media[0] } else { media };
    let url = media["url"].as_str().and_then(|u| Url::parse(u).ok())?;
    if block["provider"]
        .as_str()
        .is_some_and(|p| p != "tumblr")
        && !url.host_str().is_some_and(|h| h.ends_with("tumblr.com"))
    {
        return None;
    }
    let playlist = url.path().ends_with(".m3u8");
    let mut v = Variant::new(
        url,
        if playlist {
            VariantKind::Hls
        } else {
            VariantKind::File
        },
    );
    if !playlist {
        v.container = Some(match media["type"].as_str() {
            Some(mime) => Container::from_mime(mime).unwrap_or(Container::Mp4),
            None => Container::Mp4,
        });
        v.video = Some(VideoCodec::H264);
        v.audio = Some(AudioCodec::Aac);
    }
    v.width = media["width"].as_u64().map(|w| w as u32);
    v.height = media["height"].as_u64().map(|h| h as u32);
    v.duration = block["duration"]
        .as_f64()
        .or_else(|| media["duration"].as_f64())
        .filter(|d| *d > 0.0)
        .map(Duration::from_secs_f64);
    Some(v)
}

#[async_trait]
impl Resolver for TumblrResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Tumblr",
            hosts: &["tumblr.com"],
            features: &["posts", "reblogs", "blogs", "embedded players"],
            formats: &["mp4", "hls"],
            session: SessionSupport::Optional,
            examples: &[
                "https://www.tumblr.com/staff/802565427665502208",
                "https://staff.tumblr.com/post/802565427665502208",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Post { blog, id } => self.post(&blog, &id, url).await,
            Link::Blog(blog) => self.blog(&blog, url).await,
        }
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        if !self.logged_in() {
            return Ok(SessionCheck::LoggedOut);
        }
        let url = Url::parse(&format!("{SITE}dashboard")).expect("valid");
        let fetched = super::fetch(&self.http, &url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        let html = fetched.text();
        let page = Page::parse(&html, &url);
        let state = page
            .between("___INITIAL_STATE___\">", "</script>")
            .and_then(|s| serde_json::from_str::<Value>(s.trim()).ok());
        let Some(state) = state else {
            return Ok(SessionCheck::LoggedOut);
        };
        if state["isLoggedIn"]["isLoggedIn"].as_bool() != Some(true)
            && state["isLoggedIn"].as_bool() != Some(true)
        {
            return Ok(SessionCheck::LoggedOut);
        }
        let name = state["queries"]["queries"]
            .as_array()
            .into_iter()
            .flatten()
            .find_map(|q| q["state"]["data"]["user"]["name"].as_str())
            .or_else(|| state["user"]["name"].as_str())
            .map(String::from)
            .unwrap_or_else(|| "a Tumblr account".into());
        Ok(SessionCheck::LoggedIn { account: name })
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
                headers: vec![("content-type".into(), "application/json".into())],
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    fn permalink(elements: Vec<Value>) -> String {
        json!({"meta": {"status": 200, "msg": "OK"}, "response": {"timeline": {"elements": elements}}}).to_string()
    }

    fn reblog_post() -> Value {
        json!({
            "objectType": "post", "id": "802565427665502208", "blogName": "staff", "postUrl": "https://staff.tumblr.com/post/802565427665502208",
            "summary": "Recap of the recap? Check it out over on the other app 👁️👄👁️", "timestamp": 1765386035, "content": [],
            "trail": [{"blog": {"name": "tumblr"}, "content": [
                {"type": "video", "provider": "tumblr", "media": {"url": "https://va.media.tumblr.com/tumblr_t729kgZ4VU1zyvz82.mp4", "type": "video/mp4", "width": 720, "height": 1280},
                 "poster": [{"url": "https://64.media.tumblr.com/poster.jpg", "width": 720, "height": 1280}]},
                {"type": "text", "text": "Recap of the recap?"}
            ]}]
        })
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let post = Link::Post {
            blog: "staff".into(),
            id: "802565427665502208".into(),
        };
        assert_eq!(link("https://www.tumblr.com/staff/802565427665502208"), Some(post.clone()));
        assert_eq!(
            link("https://www.tumblr.com/staff/802565427665502208/recap-of-the-recap"),
            Some(post.clone())
        );
        assert_eq!(
            link("https://staff.tumblr.com/post/802565427665502208/recap"),
            Some(post.clone())
        );
        assert_eq!(
            link("https://www.tumblr.com/blog/view/staff/802565427665502208"),
            Some(post)
        );
        assert_eq!(link("https://staff.tumblr.com/"), Some(Link::Blog("staff".into())));
        assert_eq!(link("https://www.tumblr.com/staff"), Some(Link::Blog("staff".into())));
        assert_eq!(link("https://www.tumblr.com/dashboard"), None);
        assert_eq!(link("https://www.tumblr.com/"), None);
        assert_eq!(link("https://assets.tumblr.com/x.js"), None);
        let html = r#"<script id="___INITIAL_STATE___">{"apiFetchStore":{"API_TOKEN":"tok123","extraHeaders":"{}"}}</script>"#;
        assert_eq!(token_in(html).as_deref(), Some("tok123"));
    }

    #[tokio::test]
    async fn hosted_videos_resolve_through_the_api_including_reblogs() {
        let mut fixture = Fixture::new("tumblr", None);
        fixture.exchanges.push(get(
            "https://www.tumblr.com/api/v2/blog/staff/posts/802565427665502208/permalink?fields[blogs]=name,avatar,title,url&reblog_info=true",
            200,
            &permalink(vec![reblog_post()]),
        ));
        let resolver = TumblrResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.tumblr.com/staff/802565427665502208").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("802565427665502208"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Recap of the recap? Check it out over on the other app 👁️👄👁️")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("staff"));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(
            resolved.webpage_url.as_ref().unwrap().as_str(),
            "https://staff.tumblr.com/post/802565427665502208"
        );
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].height, Some(1280));
        assert_eq!(
            resolved.variants[0].url.as_str(),
            "https://va.media.tumblr.com/tumblr_t729kgZ4VU1zyvz82.mp4"
        );
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://64.media.tumblr.com/poster.jpg"
        );
    }

    #[tokio::test]
    async fn external_players_are_handed_on_and_refusals_refresh_the_token() {
        let mut external = reblog_post();
        external["trail"] = json!([]);
        external["content"] = json!([{"type": "video", "provider": "youtube", "url": "https://www.youtube.com/watch?v=abc123", "embedUrl": "https://www.youtube.com/embed/abc123"}]);
        let mut fixture = Fixture::new("tumblr", None);
        fixture.exchanges.push(get(
            "https://www.tumblr.com/api/v2/blog/staff/posts/802565427665502208/permalink?fields[blogs]=name,avatar,title,url&reblog_info=true",
            401,
            r#"{"meta":{"status":401,"msg":"Unauthorized"},"errors":[{"title":"Unauthorized","code":1016}],"response":[]}"#,
        ));
        fixture.exchanges.push(Exchange {
            request: RecordedRequest {
                method: "GET".into(),
                url: "https://www.tumblr.com/explore/trending".into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status: 200,
                url: "https://www.tumblr.com/explore/trending".into(),
                headers: vec![("content-type".into(), "text/html".into())],
                body: RecordedBody::Text(r#"<html><script id="___INITIAL_STATE___">{"apiFetchStore":{"API_TOKEN":"freshtoken"}}</script></html>"#.into()),
                truncated: false,
            },
        });
        fixture.exchanges.push(get(
            "https://www.tumblr.com/api/v2/blog/staff/posts/802565427665502208/permalink?fields[blogs]=name,avatar,title,url&reblog_info=true",
            200,
            &permalink(vec![external]),
        ));
        fixture.exchanges.push(get(
            "https://www.tumblr.com/api/v2/blog/staff/posts/1/permalink?fields[blogs]=name,avatar,title,url&reblog_info=true",
            404,
            r#"{"meta":{"status":404,"msg":"Not Found"},"errors":[],"response":[]}"#,
        ));
        let resolver = TumblrResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://staff.tumblr.com/post/802565427665502208").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Redirect(u) if u.as_str() == "https://www.youtube.com/watch?v=abc123"),
            "{error}"
        );
        assert_eq!(resolver.token(), "freshtoken");
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://staff.tumblr.com/post/1").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn blogs_list_their_video_posts() {
        let mut fixture = Fixture::new("tumblr", None);
        fixture.exchanges.push(get(
            "https://www.tumblr.com/api/v2/blog/staff/posts?type=video&npf=true&reblog_info=true&limit=20&offset=0",
            200,
            &json!({"meta": {"status": 200}, "response": {"blog": {"name": "staff", "title": "Tumblr Staff"}, "totalPosts": 2, "posts": [
                {"idString": "1", "postUrl": "https://staff.tumblr.com/post/1", "summary": "First"},
                {"idString": "2", "postUrl": "https://staff.tumblr.com/post/2", "summary": "Second"}
            ]}}).to_string(),
        ));
        let resolver = TumblrResolver::new(Http::replay(fixture));
        let playlist = match resolver
            .resolve(&Url::parse("https://staff.tumblr.com/").unwrap())
            .await
            .unwrap()
        {
            Resolution::Playlist(playlist) => playlist,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(playlist.title.as_deref(), Some("Tumblr Staff (videos)"));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(playlist.entries[1].title.as_deref(), Some("Second"));
    }
}
