//! Reddit posts and v.redd.it videos.
//!
//! A post is read one of two ways. The JSON API, `www.reddit.com/comments/{id}/.json`, is
//! asked first: it names the post's video with its duration, the parents of a crosspost,
//! the over-18 flag and the thumbnail, and a stored `reddit_session` cookie makes it
//! answer on every network. For anonymous callers on many networks Reddit walls the API
//! off, answering a block page, a redirect to its login page or a 429; there the post's
//! Atom feed, `www.reddit.com/comments/{id}/.rss`, is read instead. Its post entry
//! carries the title, author, subreddit, permalink, published time, thumbnail and link,
//! and the link leads to the v.redd.it video, to media hosted elsewhere, or to the post a
//! crosspost was taken from, which is read through its own feed. The feed says nothing
//! about the over-18 flag, so before a video read this way is returned, the post's embed
//! page at `embed.reddit.com`, which states the flag, settles it and supplies the poster
//! of a post whose feed entry shows no thumbnail.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use scraper::Selector;
use serde_json::Value;
use url::Url;

use super::page::Page;
use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionCheck, SessionSupport,
    Variant, VariantKind, clean_title, essence, hls,
};
use crate::http::{BROWSER_UA, Cookie, Http};

const MAX_JSON: usize = 8 * 1024 * 1024;
const PLATFORM: &str = "reddit";
/// How many crossposts pointing at other posts are followed before giving up.
const MAX_CROSSPOST_HOPS: usize = 3;

static RE_COMMENTS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/comments/([A-Za-z0-9]+)").unwrap());
static RE_SHARE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/r/[^/]+/s/[A-Za-z0-9]+").unwrap());
static RE_VIDEO_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(?:video/)?([A-Za-z0-9]{6,})").unwrap());
static RE_SUBREDDIT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/r/([^/]+)/").unwrap());
static RE_ENTRY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<entry>(.*?)</entry>").unwrap());
static RE_ENTRY_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<id>\s*([A-Za-z0-9_]+)\s*</id>").unwrap());
static RE_ENTRY_TITLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<title>(.*?)</title>").unwrap());
static RE_ENTRY_AUTHOR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<author>\s*<name>(.*?)</name>").unwrap());
static RE_ENTRY_CATEGORY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<category\s+term="([^"]*)""#).unwrap());
static RE_ENTRY_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<link\s+href="([^"]*)""#).unwrap());
static RE_ENTRY_PUBLISHED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"<published>\s*([^<]*?)\s*</published>").unwrap());
static RE_ENTRY_THUMBNAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<media:thumbnail\s+url="([^"]*)""#).unwrap());
static RE_ENTRY_CONTENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)<content[^>]*>(.*?)</content>").unwrap());
static RE_ENTRY_POST_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<a href="([^"]+)">\[link\]</a>"#).unwrap());

pub struct RedditResolver {
    http: Http,
}

/// What the JSON API answered for a post.
enum ApiAnswer {
    Post(Value),
    /// The API refuses anonymous reading from this network.
    Walled(&'static str),
}

/// The post entry of a post's Atom feed.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FeedPost {
    title: Option<String>,
    author: Option<String>,
    subreddit: Option<String>,
    permalink: Option<Url>,
    published: Option<jiff::Timestamp>,
    thumbnail: Option<Url>,
    /// Where the post points: its video, media hosted elsewhere, another post, or itself.
    link: Option<Url>,
}

/// What a post's embed page states about it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EmbedPost {
    nsfw: bool,
    poster: Option<Url>,
}

impl RedditResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    fn is_reddit_host(host: &str) -> bool {
        host == "reddit.com" || host.ends_with(".reddit.com") || host == "redd.it"
    }

    async fn resolve_video_id(
        &self,
        id: &str,
        base: Resolved,
        hint: Option<Duration>,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let master = Url::parse(&format!("https://v.redd.it/{id}/HLSPlaylist.m3u8"))
            .map_err(|e| ResolveError::malformed(origin, e.to_string()))?;
        let expanded = hls::expand(&self.http, &master, PLATFORM, BROWSER_UA, &[]).await?;
        let duration = expanded.duration.or(hint);
        let mut variants = expanded.variants;
        for v in &mut variants {
            v.duration = duration;
        }
        let dash = Url::parse(&format!("https://v.redd.it/{id}/DASHPlaylist.mpd"))
            .map_err(|e| ResolveError::malformed(origin, e.to_string()))?;
        let mut fallback = Variant::new(dash, VariantKind::Dash);
        fallback.duration = duration;
        variants.push(fallback);
        let mut resolved = base;
        resolved.id = Some(id.to_string());
        resolved.duration = duration;
        resolved.subtitles = expanded.subtitles;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn canonical_post_id(&self, url: &Url) -> Result<Option<String>, ResolveError> {
        if let Some(caps) = RE_COMMENTS.captures(url.path()) {
            return Ok(Some(caps[1].to_string()));
        }
        if url.host_str() == Some("redd.it") {
            let id = url.path().trim_matches('/');
            if !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()) {
                return Ok(Some(id.to_string()));
            }
        }
        if RE_SHARE.is_match(url.path()) {
            let mut share = url.clone();
            share
                .set_host(Some("www.reddit.com"))
                .map_err(|e| ResolveError::malformed(url, e.to_string()))?;
            let response = self
                .http
                .get(share)
                .platform(PLATFORM)
                .user_agent(BROWSER_UA)
                .follow_redirects(false)
                .send()
                .await?;
            if let Some(location) = response.header("location")
                && let Ok(target) = url.join(location)
                && let Some(caps) = RE_COMMENTS.captures(target.path())
            {
                return Ok(Some(caps[1].to_string()));
            }
        }
        Ok(None)
    }

    /// The post through the JSON API, or word that the API is walled off here.
    async fn post_json(&self, post_id: &str, origin: &Url) -> Result<ApiAnswer, ResolveError> {
        let json_url = Url::parse(&format!(
            "https://www.reddit.com/comments/{post_id}/.json?raw_json=1"
        ))
        .map_err(|e| ResolveError::malformed(origin, e.to_string()))?;
        let response = self
            .http
            .get(json_url)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .send()
            .await?;
        if response.url.path().starts_with("/login") {
            return Ok(ApiAnswer::Walled(
                "the API sent the request to the login page",
            ));
        }
        match response.status.as_u16() {
            200..=299 => {}
            403 => return Ok(ApiAnswer::Walled("the API answered HTTP 403")),
            429 => return Ok(ApiAnswer::Walled("the API answered HTTP 429")),
            404 | 410 => return Err(ResolveError::NotFound(origin.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the API answered HTTP {status}"),
                ));
            }
        }
        let (body, _) = response.bytes_up_to(MAX_JSON).await?;
        let listing: Value = serde_json::from_slice(&body)
            .map_err(|e| ResolveError::malformed(origin, format!("JSON: {e}")))?;
        let post = listing
            .pointer("/0/data/children/0/data")
            .ok_or_else(|| ResolveError::malformed(origin, "post JSON has no listing"))?;
        Ok(ApiAnswer::Post(post.clone()))
    }

    /// A post read through the JSON API, or through its feed where the API is walled off.
    async fn resolve_post(&self, post_id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        match self.post_json(post_id, origin).await? {
            ApiAnswer::Post(post) => self.resolve_api_post(&post, origin).await,
            ApiAnswer::Walled(reason) => {
                tracing::debug!(
                    post = post_id,
                    reason,
                    "the Reddit API is walled off here; reading the post's feed"
                );
                self.resolve_feed_post(post_id, origin, 0).await
            }
        }
    }

    async fn resolve_api_post(
        &self,
        post: &Value,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let mut base = Resolved::new(PLATFORM);
        base.title = post
            .get("title")
            .and_then(|t| t.as_str())
            .and_then(clean_title);
        base.uploader = post
            .get("author")
            .and_then(|a| a.as_str())
            .map(|a| format!("u/{a}"));
        base.uploaded_at = post
            .get("created_utc")
            .and_then(|t| t.as_f64())
            .and_then(|t| jiff::Timestamp::from_second(t as i64).ok());
        base.webpage_url = post
            .get("permalink")
            .and_then(|p| p.as_str())
            .and_then(|p| Url::parse(&format!("https://www.reddit.com{p}")).ok());
        base.thumbnail = post
            .get("thumbnail")
            .and_then(|t| t.as_str())
            .and_then(|t| Url::parse(t).ok());
        base.age_limit = post
            .get("over_18")
            .and_then(|o| o.as_bool())
            .filter(|o| *o)
            .map(|_| 18);

        if let Some(video) = reddit_video(post) {
            let hint = video
                .get("duration")
                .and_then(|d| d.as_f64())
                .filter(|d| *d > 0.0)
                .map(Duration::from_secs_f64);
            let source = video
                .get("hls_url")
                .or_else(|| video.get("dash_url"))
                .or_else(|| video.get("fallback_url"))
                .and_then(|u| u.as_str())
                .and_then(|u| Url::parse(u).ok());
            if let Some(id) = source.as_ref().and_then(video_id_from_url) {
                return self.resolve_video_id(&id, base, hint, origin).await;
            }
        }

        let destination = post
            .get("url_overridden_by_dest")
            .or_else(|| post.get("url"))
            .and_then(|u| u.as_str())
            .and_then(|u| Url::parse(u).ok());
        match destination {
            Some(dest) if video_id_from_url(&dest).is_some() => {
                let id = video_id_from_url(&dest).unwrap_or_default();
                self.resolve_video_id(&id, base, None, origin).await
            }
            Some(dest)
                if matches!(dest.scheme(), "http" | "https")
                    && dest
                        .host_str()
                        .is_some_and(|h| !Self::is_reddit_host(h) && h != "v.redd.it") =>
            {
                Err(ResolveError::Redirect(dest))
            }
            _ => Err(ResolveError::NotFound(origin.clone())),
        }
    }

    /// The post entry of the post's Atom feed.
    async fn feed_post(&self, post_id: &str, origin: &Url) -> Result<FeedPost, ResolveError> {
        let feed_url = Url::parse(&format!("https://www.reddit.com/comments/{post_id}/.rss"))
            .map_err(|e| ResolveError::malformed(origin, e.to_string()))?;
        let response = self
            .http
            .get(feed_url)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .send()
            .await?;
        let atom = essence(response.content_type()).ends_with("xml");
        match response.status.as_u16() {
            200..=299 => {}
            // A removed or private post's feed is an empty feed with a 403.
            403 if atom => {}
            404 | 410 => return Err(ResolveError::NotFound(origin.clone())),
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the feed answered HTTP {status}"),
                ));
            }
        }
        let body = response.text(MAX_PAGE).await?;
        parse_feed_post(&body, post_id).ok_or_else(|| ResolveError::NotFound(origin.clone()))
    }

    /// What the post's embed page states about it.
    async fn embed_post(
        &self,
        subreddit: &str,
        post_id: &str,
        origin: &Url,
    ) -> Result<EmbedPost, ResolveError> {
        let embed_url = Url::parse(&format!(
            "https://embed.reddit.com/r/{subreddit}/comments/{post_id}/?embed=true"
        ))
        .map_err(|e| ResolveError::malformed(origin, e.to_string()))?;
        let response = self
            .http
            .get(embed_url.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header(
                "accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
            )
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
                    format!("the embed page answered HTTP {status}"),
                ));
            }
        }
        let html = response.text(MAX_PAGE).await?;
        parse_embed_post(&html, &embed_url)
            .ok_or_else(|| ResolveError::malformed(origin, "the embed page carries no post data"))
    }

    /// Settles the post's over-18 flag from its embed page, and takes the embed's poster
    /// where the feed entry showed no thumbnail.
    async fn flag_from_embed(
        &self,
        post: &FeedPost,
        post_id: &str,
        base: &mut Resolved,
        origin: &Url,
    ) -> Result<(), ResolveError> {
        let subreddit = post
            .subreddit
            .as_deref()
            .ok_or_else(|| ResolveError::malformed(origin, "the feed entry names no subreddit"))?;
        let embed = self.embed_post(subreddit, post_id, origin).await?;
        base.age_limit = embed.nsfw.then_some(18);
        if base.thumbnail.is_none() {
            base.thumbnail = embed.poster;
        }
        Ok(())
    }

    /// A post read through its feed; `hops` counts the crossposts followed to reach it.
    async fn resolve_feed_post(
        &self,
        post_id: &str,
        origin: &Url,
        hops: usize,
    ) -> Result<Resolution, ResolveError> {
        let post = self.feed_post(post_id, origin).await?;
        let mut base = Resolved::new(PLATFORM);
        base.title = post.title.clone();
        base.uploader = post.author.clone();
        base.uploaded_at = post.published;
        base.webpage_url = post.permalink.clone();
        base.thumbnail = post.thumbnail.clone();
        let Some(link) = post.link.clone() else {
            return Err(ResolveError::unavailable(
                origin,
                "the post's feed entry names no link",
            ));
        };
        if let Some(video_id) = video_id_from_url(&link) {
            self.flag_from_embed(&post, post_id, &mut base, origin)
                .await?;
            return self.resolve_video_id(&video_id, base, None, origin).await;
        }
        let host = link.host_str().unwrap_or("");
        if matches!(link.scheme(), "http" | "https")
            && !Self::is_reddit_host(host)
            && host != "v.redd.it"
        {
            return Err(ResolveError::Redirect(link));
        }
        let linked_post = RE_COMMENTS
            .captures(link.path())
            .map(|caps| caps[1].to_string());
        match linked_post {
            Some(parent_id) if !parent_id.eq_ignore_ascii_case(post_id) => {
                if hops >= MAX_CROSSPOST_HOPS {
                    return Err(ResolveError::unavailable(
                        origin,
                        format!("the post is a chain of more than {MAX_CROSSPOST_HOPS} crossposts"),
                    ));
                }
                self.flag_from_embed(&post, post_id, &mut base, origin)
                    .await?;
                let parent = Box::pin(self.resolve_feed_post(&parent_id, origin, hops + 1)).await?;
                Ok(match parent {
                    Resolution::Media(mut media) => {
                        keep_crosspost_details(&base, &mut media);
                        Resolution::Media(media)
                    }
                    Resolution::Playlist(list) => Resolution::Playlist(list),
                })
            }
            Some(_) => Err(ResolveError::unavailable(origin, "the post is a text post")),
            None if link.path().starts_with("/gallery/") => Err(ResolveError::unavailable(
                origin,
                "the post is an image gallery",
            )),
            None => Err(ResolveError::NotFound(origin.clone())),
        }
    }
}

/// A crosspost keeps its own title, author, time, page, thumbnail and age limit over its
/// parent's, the way the JSON API reports it.
fn keep_crosspost_details(child: &Resolved, parent: &mut Resolved) {
    if child.title.is_some() {
        parent.title = child.title.clone();
    }
    if child.uploader.is_some() {
        parent.uploader = child.uploader.clone();
    }
    if child.uploaded_at.is_some() {
        parent.uploaded_at = child.uploaded_at;
    }
    if child.webpage_url.is_some() {
        parent.webpage_url = child.webpage_url.clone();
    }
    if child.thumbnail.is_some() {
        parent.thumbnail = child.thumbnail.clone();
    }
    parent.age_limit = match (child.age_limit, parent.age_limit) {
        (Some(child), Some(parent)) => Some(child.max(parent)),
        (child, parent) => child.or(parent),
    };
}

fn reddit_video(post: &Value) -> Option<&Value> {
    let direct = post
        .pointer("/secure_media/reddit_video")
        .or_else(|| post.pointer("/media/reddit_video"));
    if direct.is_some() {
        return direct;
    }
    post.get("crosspost_parent_list")?
        .as_array()?
        .iter()
        .find_map(|parent| {
            parent
                .pointer("/secure_media/reddit_video")
                .or_else(|| parent.pointer("/media/reddit_video"))
        })
}

fn video_id_from_url(url: &Url) -> Option<String> {
    if url.host_str() != Some("v.redd.it") {
        return None;
    }
    RE_VIDEO_ID.captures(url.path()).map(|c| c[1].to_string())
}

/// Decodes the named and numeric character references of XML and HTML text.
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let after = &rest[start..];
        let Some(end) = after.find(';').filter(|end| *end <= 10) else {
            out.push('&');
            rest = &after[1..];
            continue;
        };
        let name = &after[1..end];
        let decoded = match name {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => name
                .strip_prefix('#')
                .and_then(|number| match number.strip_prefix(['x', 'X']) {
                    Some(hex) => u32::from_str_radix(hex, 16).ok(),
                    None => number.parse().ok(),
                })
                .and_then(char::from_u32),
        };
        match decoded {
            Some(ch) => {
                out.push(ch);
                rest = &after[end + 1..];
            }
            None => {
                out.push('&');
                rest = &after[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The entry of post `post_id` in an Atom feed, which lists the post before its comments.
fn parse_feed_post(feed: &str, post_id: &str) -> Option<FeedPost> {
    let wanted = format!("t3_{post_id}");
    let entry = RE_ENTRY
        .captures_iter(feed)
        .filter_map(|caps| caps.get(1).map(|m| m.as_str()))
        .find(|entry| {
            RE_ENTRY_ID
                .captures(entry)
                .is_some_and(|id| id[1].eq_ignore_ascii_case(&wanted))
        })?;
    let text = |re: &Regex| {
        re.captures(entry)
            .map(|caps| decode_entities(caps[1].trim()))
            .filter(|value| !value.is_empty())
    };
    let permalink = text(&RE_ENTRY_LINK).and_then(|l| Url::parse(&l).ok());
    let subreddit = permalink
        .as_ref()
        .and_then(|p| RE_SUBREDDIT.captures(p.path()).map(|c| c[1].to_string()))
        .or_else(|| text(&RE_ENTRY_CATEGORY));
    let content = text(&RE_ENTRY_CONTENT).unwrap_or_default();
    let link = RE_ENTRY_POST_LINK
        .captures(&content)
        .and_then(|caps| Url::parse(&decode_entities(&caps[1])).ok());
    Some(FeedPost {
        title: text(&RE_ENTRY_TITLE).and_then(|t| clean_title(&t)),
        author: text(&RE_ENTRY_AUTHOR).map(|a| {
            let name = a.trim().trim_start_matches('/').trim_start_matches("u/");
            format!("u/{name}")
        }),
        subreddit,
        permalink,
        published: text(&RE_ENTRY_PUBLISHED).and_then(|p| p.parse().ok()),
        thumbnail: text(&RE_ENTRY_THUMBNAIL).and_then(|t| Url::parse(&t).ok()),
        link,
    })
}

/// The post's over-18 flag and poster from its embed page, whose `shreddit-screenview-data`
/// element carries the post's record.
fn parse_embed_post(html: &str, embed_url: &Url) -> Option<EmbedPost> {
    let page = Page::parse(html, embed_url);
    let screenview = Selector::parse("shreddit-screenview-data[data]").expect("valid");
    let data: Value = page
        .document()
        .select(&screenview)
        .filter_map(|element| element.value().attr("data"))
        .find_map(|data| serde_json::from_str(data).ok())?;
    let nsfw = data.pointer("/post/nsfw")?.as_bool()?;
    let player = Selector::parse("shreddit-player[poster]").expect("valid");
    let poster = page
        .document()
        .select(&player)
        .filter_map(|element| element.value().attr("poster"))
        .find_map(|poster| embed_url.join(poster).ok());
    Some(EmbedPost { nsfw, poster })
}

#[async_trait]
impl Resolver for RedditResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Reddit",
            hosts: &["reddit.com", "redd.it", "v.redd.it"],
            features: &["videos", "crossposts", "share links", "linked media"],
            formats: &["hls", "dash"],
            session: SessionSupport::Optional,
            examples: &[
                "https://www.reddit.com/r/videos/comments/6rrwyj/that_small_heart_attack/",
                "https://redd.it/6rrwyj",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        matches!(url.scheme(), "http" | "https")
            && url
                .host_str()
                .is_some_and(|h| Self::is_reddit_host(h) || h == "v.redd.it")
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        if let Some(id) = video_id_from_url(url) {
            return self
                .resolve_video_id(&id, Resolved::new(PLATFORM), None, url)
                .await;
        }
        let post_id = self
            .canonical_post_id(url)
            .await?
            .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        self.resolve_post(&post_id, url).await
    }

    /// Reddit hides posts marked over 18 behind a prompt this cookie answers.
    fn consent_cookies(&self) -> Vec<Cookie> {
        vec![Cookie::new("over18", "1", "reddit.com")]
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        if self.http.jar(PLATFORM).get("reddit_session").is_none() {
            return Ok(SessionCheck::LoggedOut);
        }
        let me = Url::parse("https://www.reddit.com/api/me.json").expect("valid");
        let response = self
            .http
            .get(me.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .send()
            .await?;
        if !response.is_success() {
            return Ok(SessionCheck::LoggedOut);
        }
        let value: Value = response.json(MAX_JSON).await?;
        Ok(match value.pointer("/data/name").and_then(|n| n.as_str()) {
            Some(name) => SessionCheck::LoggedIn {
                account: format!("u/{name}"),
            },
            None => SessionCheck::LoggedOut,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };
    use serde_json::json;

    const POST: &str = "https://www.reddit.com/r/videos/comments/6rrwyj/that_small_heart_attack/";
    const JSON: &str = "https://www.reddit.com/comments/6rrwyj/.json?raw_json=1";
    const FEED: &str = "https://www.reddit.com/comments/6rrwyj/.rss";
    const EMBED: &str = "https://embed.reddit.com/r/videos/comments/6rrwyj/?embed=true";
    const MASTER: &str = "https://v.redd.it/zv89llsvexdz/HLSPlaylist.m3u8";
    const MEDIA: &str = "https://v.redd.it/zv89llsvexdz/HLS_1_5_M_v4.m3u8";
    const BLOCK_PAGE: &str =
        r#"<html><body class="theme-beta"><h1>blocked by network security</h1></body></html>"#;
    const MASTER_BODY: &str = "#EXTM3U\n\
        #EXT-X-MEDIA:URI=\"HLS_AUDIO_160_K_v4.m3u8\",TYPE=AUDIO,GROUP-ID=\"audio-0\",NAME=\"Default\",DEFAULT=YES,AUTOSELECT=YES\n\
        #EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=1875000,RESOLUTION=352x640,CODECS=\"avc1.4d001f,mp4a.40.2\",AUDIO=\"audio-0\"\n\
        HLS_1_5_M_v4.m3u8\n\
        #EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=822000,RESOLUTION=176x320,CODECS=\"avc1.42001e,mp4a.40.2\",AUDIO=\"audio-0\"\n\
        HLS_600_K_v4.m3u8\n";
    const MEDIA_BODY: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\ns1.ts\n#EXTINF:2.0,\ns2.ts\n#EXT-X-ENDLIST\n";

    fn exchange(
        url: &str,
        status: u16,
        content_type: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> Exchange {
        let mut all: Vec<(String, String)> = vec![("content-type".into(), content_type.into())];
        all.extend(headers.iter().map(|(k, v)| (k.to_string(), v.to_string())));
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
                headers: all,
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    fn video_exchanges(fixture: &mut Fixture) {
        fixture.exchanges.push(exchange(
            MASTER,
            200,
            "application/vnd.apple.mpegurl",
            MASTER_BODY,
            &[],
        ));
        fixture.exchanges.push(exchange(
            MEDIA,
            200,
            "application/vnd.apple.mpegurl",
            MEDIA_BODY,
            &[],
        ));
    }

    fn walled(fixture: &mut Fixture) {
        fixture.exchanges.push(exchange(
            JSON,
            403,
            "text/html; charset=utf-8",
            BLOCK_PAGE,
            &[],
        ));
    }

    /// A post's feed the way Reddit writes it: the post entry, HTML-escaped content with
    /// the `[link]` anchor, then a comment entry.
    fn feed(post_id: &str, permalink: &str, link: &str, thumbnail: bool) -> String {
        let escaped_link = link.replace('&', "&amp;");
        let (table, media) = if thumbnail {
            (
                "&lt;table&gt; &lt;tr&gt;&lt;td&gt; &lt;a href=&quot;PERMALINK&quot;&gt; &lt;img src=&quot;https://external-preview.redd.it/cb9n.png?width=320&amp;amp;crop=smart&quot; alt=&quot;That small heart attack.&quot; /&gt; &lt;/a&gt; &lt;/td&gt;&lt;td&gt;".replace("PERMALINK", permalink),
                r#"<media:thumbnail url="https://external-preview.redd.it/cb9n.png?width=320&amp;crop=smart&amp;s=2586" />"#.to_string(),
            )
        } else {
            (String::new(), String::new())
        };
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><feed xmlns="http://www.w3.org/2005/Atom" xmlns:media="http://search.yahoo.com/mrss/"><category term=" reddit.com" label="r/ reddit.com"/><updated>2026-09-12T22:46:52+00:00</updated><id>/comments/{post_id}/.rss</id><link rel="self" href="https://www.reddit.com/comments/{post_id}/.rss" type="application/atom+xml" /><link rel="alternate" href="https://www.reddit.com/comments/{post_id}/" type="text/html" /><title>That small heart attack. : reddit.com</title><entry><author><name>/u/Antw87</name><uri>https://www.reddit.com/user/Antw87</uri></author><category term="videos" label="r/videos"/><content type="html">{table} &amp;#32; submitted by &amp;#32; &lt;a href=&quot;https://www.reddit.com/user/Antw87&quot;&gt; /u/Antw87 &lt;/a&gt; &amp;#32; to &amp;#32; &lt;a href=&quot;https://www.reddit.com/r/videos/&quot;&gt; r/videos &lt;/a&gt; &lt;br/&gt; &lt;span&gt;&lt;a href=&quot;{escaped_link}&quot;&gt;[link]&lt;/a&gt;&lt;/span&gt; &amp;#32; &lt;span&gt;&lt;a href=&quot;{permalink}&quot;&gt;[comments]&lt;/a&gt;&lt;/span&gt; &lt;/td&gt;&lt;/tr&gt;&lt;/table&gt;</content><id>t3_{post_id}</id>{media}<link href="{permalink}" /><updated>2017-08-05T14:05:39+00:00</updated><published>2017-08-05T14:05:39+00:00</published><title>That small heart &amp; attack.</title></entry><entry><author><name>/u/Stedmania</name><uri>https://www.reddit.com/user/Stedmania</uri></author><category term="videos" label="r/videos" /><content type="html">&lt;div class=&quot;md&quot;&gt;&lt;p&gt;&lt;a href=&quot;https://example.com/&quot;&gt;[link]&lt;/a&gt;&lt;/p&gt;&lt;/div&gt;</content><id>t1_dl7cki4</id><link href="{permalink}dl7cki4/"/><updated>2017-08-05T15:28:14+00:00</updated><title>/u/Stedmania on That small heart attack.</title></entry></feed>"#
        )
    }

    fn empty_feed(post_id: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?><feed xmlns="http://www.w3.org/2005/Atom" xmlns:media="http://search.yahoo.com/mrss/"><category term=" reddit.com" label="r/ reddit.com"/><updated>2026-09-12T23:03:56+00:00</updated><id>/comments/{post_id}/.rss</id><link rel="self" href="https://www.reddit.com/comments/{post_id}/.rss" type="application/atom+xml" /><title>reddit.com: forbidden (reddit.com)</title></feed>"#
        )
    }

    /// An embed page the way Reddit renders it: the post's record in the screenview
    /// element and the player with its poster.
    fn embed(post_id: &str, nsfw: bool, video: &str) -> String {
        format!(
            r#"<!DOCTYPE html><html lang="en-US" class="theme-beta"><head><title>Reddit - The heart of the internet</title></head><body><shreddit-screenview-data data="{{&quot;post&quot;:{{&quot;id&quot;:&quot;t3_{post_id}&quot;,&quot;url&quot;:&quot;https://v.redd.it/{video}&quot;,&quot;nsfw&quot;:{nsfw},&quot;created_timestamp&quot;:1501941939969,&quot;type&quot;:&quot;video&quot;}},&quot;subreddit&quot;:{{&quot;name&quot;:&quot;videos&quot;}}}}"></shreddit-screenview-data><h1 class="line-clamp-3">That small heart attack.</h1><shreddit-player src="https://v.redd.it/{video}/HLSPlaylist.m3u8?f=sd&amp;v=1" post-id="t3_{post_id}" poster="https://external-preview.redd.it/poster-{post_id}.png?format=pjpg&amp;auto=webp"></shreddit-player></body></html>"#
        )
    }

    fn resolver(fixture: Fixture) -> RedditResolver {
        RedditResolver::new(Http::replay(fixture))
    }

    #[tokio::test]
    async fn a_post_resolves_through_the_json_api() {
        let mut fixture = Fixture::new("reddit", None);
        fixture.exchanges.push(exchange(
            "https://www.reddit.com/r/videos/s/AbCdEf123",
            307,
            "text/html",
            "",
            &[("location", POST)],
        ));
        fixture.exchanges.push(exchange(
            JSON,
            200,
            "application/json",
            &json!([{"data": {"children": [{"data": {
                "title": "That small heart attack.", "author": "Antw87", "created_utc": 1501941939.0,
                "permalink": "/r/videos/comments/6rrwyj/that_small_heart_attack/", "thumbnail": "https://b.thumbs.redditmedia.com/t.jpg",
                "over_18": true, "url": "https://v.redd.it/zv89llsvexdz",
                "secure_media": {"reddit_video": {"hls_url": "https://v.redd.it/zv89llsvexdz/HLSPlaylist.m3u8?a=1", "duration": 12}}
            }}]}}]).to_string(),
            &[],
        ));
        video_exchanges(&mut fixture);
        let resolver = resolver(fixture);
        let share = Url::parse("https://reddit.com/r/videos/s/AbCdEf123").unwrap();
        assert!(resolver.matches(&share));
        let resolved = resolver.resolve(&share).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("zv89llsvexdz"));
        assert_eq!(resolved.title.as_deref(), Some("That small heart attack."));
        assert_eq!(resolved.uploader.as_deref(), Some("u/Antw87"));
        assert_eq!(resolved.webpage_url.as_ref().unwrap().as_str(), POST);
        assert_eq!(resolved.age_limit, Some(18));
        assert_eq!(
            resolved.uploaded_at,
            Some(jiff::Timestamp::from_second(1501941939).unwrap())
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(12)));
        assert_eq!(resolved.variants.len(), 3);
        assert_eq!(resolved.variants[0].url.as_str(), MEDIA);
        assert_eq!(resolved.variants[0].height, Some(640));
        assert_eq!(resolved.variants[2].kind, VariantKind::Dash);
        assert_eq!(
            resolved.variants[2].url.as_str(),
            "https://v.redd.it/zv89llsvexdz/DASHPlaylist.mpd"
        );
    }

    #[tokio::test]
    async fn a_v_redd_it_link_resolves_on_its_own() {
        let mut fixture = Fixture::new("reddit", None);
        video_exchanges(&mut fixture);
        let resolved = resolver(fixture)
            .resolve(&Url::parse("https://v.redd.it/zv89llsvexdz").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("zv89llsvexdz"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(12)));
        assert_eq!(resolved.variants.len(), 3);
    }

    #[tokio::test]
    async fn a_walled_api_reads_the_post_from_its_feed() {
        let mut fixture = Fixture::new("reddit", None);
        walled(&mut fixture);
        fixture.exchanges.push(exchange(
            FEED,
            200,
            "application/atom+xml; charset=UTF-8",
            &feed("6rrwyj", POST, "https://v.redd.it/zv89llsvexdz", true),
            &[],
        ));
        fixture.exchanges.push(exchange(
            EMBED,
            200,
            "text/html; charset=utf-8",
            &embed("6rrwyj", false, "zv89llsvexdz"),
            &[],
        ));
        video_exchanges(&mut fixture);
        let resolved = resolver(fixture)
            .resolve(&Url::parse("https://redd.it/6rrwyj").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("zv89llsvexdz"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("That small heart & attack.")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("u/Antw87"));
        assert_eq!(resolved.webpage_url.as_ref().unwrap().as_str(), POST);
        assert_eq!(
            resolved.uploaded_at,
            Some(jiff::Timestamp::from_second(1501941939).unwrap())
        );
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://external-preview.redd.it/cb9n.png?width=320&crop=smart&s=2586"
        );
        assert_eq!(resolved.age_limit, None);
        assert_eq!(resolved.duration, Some(Duration::from_secs(12)));
        assert_eq!(resolved.variants.len(), 3);
        assert_eq!(resolved.variants[0].url.as_str(), MEDIA);
    }

    #[tokio::test]
    async fn a_login_redirect_or_a_429_from_the_api_also_reads_the_feed() {
        let login = "https://www.reddit.com/login/?reason=lor2&dest=https%3A%2F%2Fwww.reddit.com%2Fcomments%2F6rrwyj%2F.json%3Fraw_json%3D1";
        let mut fixture = Fixture::new("reddit", None);
        fixture
            .exchanges
            .push(exchange(JSON, 302, "text/html", "", &[("location", login)]));
        fixture.exchanges.push(exchange(
            login,
            200,
            "text/html",
            "<html><body>log in</body></html>",
            &[],
        ));
        fixture.exchanges.push(exchange(
            FEED,
            200,
            "application/atom+xml; charset=UTF-8",
            &feed(
                "6rrwyj",
                POST,
                "https://www.youtube.com/watch?v=jNGZo5gn_tc&t=5",
                true,
            ),
            &[],
        ));
        let error = resolver(fixture)
            .resolve(&Url::parse(POST).unwrap())
            .await
            .unwrap_err();
        match error {
            ResolveError::Redirect(dest) => assert_eq!(
                dest.as_str(),
                "https://www.youtube.com/watch?v=jNGZo5gn_tc&t=5"
            ),
            other => panic!("an external link redirects, not {other}"),
        }

        let mut fixture = Fixture::new("reddit", None);
        fixture.exchanges.push(exchange(
            JSON,
            429,
            "text/html",
            "",
            &[("retry-after", "3600")],
        ));
        fixture.exchanges.push(exchange(
            FEED,
            200,
            "application/atom+xml; charset=UTF-8",
            &feed("6rrwyj", POST, "https://streamable.com/abc123", true),
            &[],
        ));
        assert!(matches!(
            resolver(fixture)
                .resolve(&Url::parse(POST).unwrap())
                .await
                .unwrap_err(),
            ResolveError::Redirect(dest) if dest.as_str() == "https://streamable.com/abc123"
        ));
    }

    #[tokio::test]
    async fn an_over_18_post_is_flagged_by_its_embed_page() {
        let mut fixture = Fixture::new("reddit", None);
        walled(&mut fixture);
        fixture.exchanges.push(exchange(
            FEED,
            200,
            "application/atom+xml; charset=UTF-8",
            &feed("6rrwyj", POST, "https://v.redd.it/zv89llsvexdz", false),
            &[],
        ));
        fixture.exchanges.push(exchange(
            EMBED,
            200,
            "text/html; charset=utf-8",
            &embed("6rrwyj", true, "zv89llsvexdz"),
            &[],
        ));
        video_exchanges(&mut fixture);
        let resolved = resolver(fixture)
            .resolve(&Url::parse(POST).unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.age_limit, Some(18));
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://external-preview.redd.it/poster-6rrwyj.png?format=pjpg&auto=webp"
        );

        // An embed page that hides the post's record leaves the flag unsettled, which is
        // reported rather than guessed.
        let mut fixture = Fixture::new("reddit", None);
        walled(&mut fixture);
        fixture.exchanges.push(exchange(
            FEED,
            200,
            "application/atom+xml; charset=UTF-8",
            &feed("6rrwyj", POST, "https://v.redd.it/zv89llsvexdz", true),
            &[],
        ));
        fixture.exchanges.push(exchange(
            EMBED,
            200,
            "text/html; charset=utf-8",
            BLOCK_PAGE,
            &[],
        ));
        assert!(matches!(
            resolver(fixture)
                .resolve(&Url::parse(POST).unwrap())
                .await
                .unwrap_err(),
            ResolveError::Malformed { .. }
        ));
    }

    #[tokio::test]
    async fn a_crosspost_linking_its_parent_is_followed() {
        let parent = "https://www.reddit.com/r/aww/comments/5abcde/original_title/";
        let mut fixture = Fixture::new("reddit", None);
        walled(&mut fixture);
        fixture.exchanges.push(exchange(
            FEED,
            200,
            "application/atom+xml; charset=UTF-8",
            &feed("6rrwyj", POST, parent, true),
            &[],
        ));
        fixture.exchanges.push(exchange(
            EMBED,
            200,
            "text/html; charset=utf-8",
            &embed("6rrwyj", false, "zv89llsvexdz"),
            &[],
        ));
        fixture.exchanges.push(exchange(
            "https://www.reddit.com/comments/5abcde/.rss",
            200,
            "application/atom+xml; charset=UTF-8",
            &feed("5abcde", parent, "https://v.redd.it/zv89llsvexdz", false)
                .replace("/u/Antw87", "/u/original")
                .replace("heart &amp; attack", "original"),
            &[],
        ));
        fixture.exchanges.push(exchange(
            "https://embed.reddit.com/r/aww/comments/5abcde/?embed=true",
            200,
            "text/html; charset=utf-8",
            &embed("5abcde", true, "zv89llsvexdz"),
            &[],
        ));
        video_exchanges(&mut fixture);
        let resolved = resolver(fixture)
            .resolve(&Url::parse(POST).unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("zv89llsvexdz"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("That small heart & attack.")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("u/Antw87"));
        assert_eq!(resolved.webpage_url.as_ref().unwrap().as_str(), POST);
        assert_eq!(resolved.age_limit, Some(18));
        assert_eq!(resolved.variants.len(), 3);

        // A chain of crossposts pointing at one another ends after a few hops.
        let mut fixture = Fixture::new("reddit", None);
        walled(&mut fixture);
        let ids = ["6rrwyj", "5abcde", "4abcde", "3abcde", "2abcde"];
        for pair in ids.windows(2) {
            let permalink = format!("https://www.reddit.com/r/videos/comments/{}/x/", pair[0]);
            let next = format!("https://www.reddit.com/r/videos/comments/{}/x/", pair[1]);
            fixture.exchanges.push(exchange(
                &format!("https://www.reddit.com/comments/{}/.rss", pair[0]),
                200,
                "application/atom+xml; charset=UTF-8",
                &feed(pair[0], &permalink, &next, true),
                &[],
            ));
            fixture.exchanges.push(exchange(
                &format!(
                    "https://embed.reddit.com/r/videos/comments/{}/?embed=true",
                    pair[0]
                ),
                200,
                "text/html; charset=utf-8",
                &embed(pair[0], false, "zv89llsvexdz"),
                &[],
            ));
        }
        assert!(matches!(
            resolver(fixture)
                .resolve(&Url::parse(POST).unwrap())
                .await
                .unwrap_err(),
            ResolveError::Unavailable { reason, .. } if reason.contains("chain")
        ));
    }

    #[tokio::test]
    async fn gone_text_and_gallery_posts_are_reported() {
        let mut fixture = Fixture::new("reddit", None);
        fixture
            .exchanges
            .push(exchange(JSON, 404, "application/json", "{}", &[]));
        assert!(matches!(
            resolver(fixture)
                .resolve(&Url::parse(POST).unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));

        let mut fixture = Fixture::new("reddit", None);
        walled(&mut fixture);
        fixture.exchanges.push(exchange(
            FEED,
            403,
            "application/atom+xml; charset=UTF-8",
            &empty_feed("6rrwyj"),
            &[],
        ));
        assert!(matches!(
            resolver(fixture)
                .resolve(&Url::parse(POST).unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));

        let mut fixture = Fixture::new("reddit", None);
        walled(&mut fixture);
        fixture.exchanges.push(exchange(
            FEED,
            200,
            "application/atom+xml; charset=UTF-8",
            &feed("6rrwyj", POST, POST, false),
            &[],
        ));
        assert!(matches!(
            resolver(fixture)
                .resolve(&Url::parse(POST).unwrap())
                .await
                .unwrap_err(),
            ResolveError::Unavailable { reason, .. } if reason == "the post is a text post"
        ));

        let mut fixture = Fixture::new("reddit", None);
        walled(&mut fixture);
        fixture.exchanges.push(exchange(
            FEED,
            200,
            "application/atom+xml; charset=UTF-8",
            &feed(
                "6rrwyj",
                POST,
                "https://www.reddit.com/gallery/6rrwyj",
                true,
            ),
            &[],
        ));
        assert!(matches!(
            resolver(fixture)
                .resolve(&Url::parse(POST).unwrap())
                .await
                .unwrap_err(),
            ResolveError::Unavailable { reason, .. } if reason == "the post is an image gallery"
        ));

        let mut fixture = Fixture::new("reddit", None);
        walled(&mut fixture);
        fixture.exchanges.push(exchange(
            FEED,
            200,
            "text/html; charset=utf-8",
            BLOCK_PAGE,
            &[],
        ));
        fixture.exchanges[1].response.status = 403;
        assert!(matches!(
            resolver(fixture)
                .resolve(&Url::parse(POST).unwrap())
                .await
                .unwrap_err(),
            ResolveError::Unavailable { reason, .. } if reason == "the feed answered HTTP 403"
        ));
    }

    #[test]
    fn feed_entries_and_entities_are_read() {
        let post = parse_feed_post(
            &feed(
                "6rrwyj",
                POST,
                "https://v.redd.it/zv89llsvexdz?x=1&y=2",
                true,
            ),
            "6RRWYJ",
        )
        .unwrap();
        assert_eq!(post.title.as_deref(), Some("That small heart & attack."));
        assert_eq!(post.author.as_deref(), Some("u/Antw87"));
        assert_eq!(post.subreddit.as_deref(), Some("videos"));
        assert_eq!(post.permalink.as_ref().unwrap().as_str(), POST);
        assert_eq!(
            post.link.as_ref().unwrap().as_str(),
            "https://v.redd.it/zv89llsvexdz?x=1&y=2"
        );
        assert_eq!(
            post.published,
            Some(jiff::Timestamp::from_second(1501941939).unwrap())
        );
        assert!(post.thumbnail.is_some());
        assert!(parse_feed_post(&feed("6rrwyj", POST, POST, true), "other1").is_none());
        assert!(parse_feed_post(&empty_feed("6rrwyj"), "6rrwyj").is_none());

        assert_eq!(
            decode_entities("a &amp;amp; b &#32;&#x41;&lt;&quot;&apos;&gt;"),
            "a &amp; b  A<\"'>"
        );
        assert_eq!(
            decode_entities("stray & ampersand &unknown; &#zz;"),
            "stray & ampersand &unknown; &#zz;"
        );

        let page = parse_embed_post(
            &embed("6rrwyj", true, "zv89llsvexdz"),
            &Url::parse(EMBED).unwrap(),
        )
        .unwrap();
        assert!(page.nsfw);
        assert_eq!(
            page.poster.unwrap().as_str(),
            "https://external-preview.redd.it/poster-6rrwyj.png?format=pjpg&auto=webp"
        );
        assert!(parse_embed_post(BLOCK_PAGE, &Url::parse(EMBED).unwrap()).is_none());
    }
}
