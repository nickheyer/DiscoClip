//! Reddit posts and v.redd.it videos.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use url::Url;

use super::{
    Platform, Resolution, ResolveError, Resolved, Resolver, SessionCheck, SessionSupport, Variant,
    VariantKind, clean_title, fetch_ok, hls,
};
use crate::http::{EMBED_BOT_UA, Http};

const MAX_JSON: usize = 8 * 1024 * 1024;
const PLATFORM: &str = "reddit";

static RE_COMMENTS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/comments/([A-Za-z0-9]+)").unwrap());
static RE_SHARE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/r/[^/]+/s/[A-Za-z0-9]+").unwrap());
static RE_VIDEO_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(?:video/)?([A-Za-z0-9]{6,})").unwrap());

pub struct RedditResolver {
    http: Http,
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
        let expanded = hls::expand(&self.http, &master, PLATFORM, EMBED_BOT_UA, &[]).await?;
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
        Ok(Resolution::Media(resolved))
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
                .user_agent(EMBED_BOT_UA)
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
}

fn reddit_video(post: &serde_json::Value) -> Option<&serde_json::Value> {
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
            examples: &["https://www.reddit.com/r/aww/comments/1b3v6gc/"],
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
        let json_url = Url::parse(&format!(
            "https://old.reddit.com/comments/{post_id}/.json?raw_json=1"
        ))
        .map_err(|e| ResolveError::malformed(url, e.to_string()))?;
        let fetched =
            fetch_ok(&self.http, &json_url, PLATFORM, EMBED_BOT_UA, &[], MAX_JSON).await?;
        let value = fetched.json(url)?;
        let post = value
            .pointer("/0/data/children/0/data")
            .ok_or_else(|| ResolveError::malformed(url, "post JSON has no listing"))?;
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
                return self.resolve_video_id(&id, base, hint, url).await;
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
                self.resolve_video_id(&id, base, None, url).await
            }
            Some(dest)
                if matches!(dest.scheme(), "http" | "https")
                    && dest
                        .host_str()
                        .is_some_and(|h| !Self::is_reddit_host(h) && h != "v.redd.it") =>
            {
                Err(ResolveError::Redirect(dest))
            }
            _ => Err(ResolveError::NotFound(url.clone())),
        }
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
            .user_agent(EMBED_BOT_UA)
            .send()
            .await?;
        if !response.is_success() {
            return Ok(SessionCheck::LoggedOut);
        }
        let value: serde_json::Value = response.json(MAX_JSON).await?;
        Ok(match value.pointer("/data/name").and_then(|n| n.as_str()) {
            Some(name) => SessionCheck::LoggedIn {
                account: format!("u/{name}"),
            },
            None => SessionCheck::LoggedOut,
        })
    }
}
