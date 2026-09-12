//! Facebook videos, reels and watch links, read from the data the page hands its player,
//! with short links unwrapped first.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use url::Url;

use super::page::{Page, unescape_json_string};
use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionCheck, SessionSupport,
    Variant, VariantKind, clean_title, fetch_ok,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "facebook";
const ME: &str = "https://www.facebook.com/me/";

static RE_VIDEO_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(?:[^/]+/videos/(?:[^/]+/)?|reel/|videos/)(\d{6,})").unwrap());
static RE_VIDEO_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""video_id":"(\d+)""#).unwrap());
static RE_OWNER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""owner":\{[^{}]*?"name":"((?:[^"\\]|\\.)+)""#).unwrap());
static RE_TITLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""title":\{"text":"((?:[^"\\]|\\.)+)"\}"#).unwrap());
static RE_PUBLISH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""publish_time":(\d+)"#).unwrap());
static RE_DURATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""playable_duration_in_ms":(\d+)"#).unwrap());
static RE_THUMB: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""preferred_thumbnail":\{"image":\{"uri":"((?:[^"\\]|\\.)+)""#).unwrap()
});

fn is_facebook_host(host: &str) -> bool {
    host == "facebook.com"
        || host.ends_with(".facebook.com")
        || host == "fb.com"
        || host == "www.fb.com"
}

fn is_short_link(url: &Url) -> bool {
    matches!(url.host_str(), Some("fb.watch")) || url.path().starts_with("/share/")
}

/// The video id a page link names, from its path or its `v` parameter.
pub fn video_id(url: &Url) -> Option<String> {
    if let Some(v) = url
        .query_pairs()
        .find(|(k, _)| k == "v")
        .map(|(_, v)| v.into_owned())
        && v.chars().all(|c| c.is_ascii_digit())
        && v.len() >= 6
    {
        return Some(v);
    }
    RE_VIDEO_PATH
        .captures(url.path())
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_string())
}

/// The first JSON string value named `key` in `html`, unescaped.
fn json_field(html: &str, key: &str) -> Option<String> {
    let re = Regex::new(&format!(r#""{}":"((?:[^"\\]|\\.)+)""#, regex::escape(key))).ok()?;
    re.captures(html)
        .map(|c| unescape_json_string(&c[1]))
        .filter(|v| !v.is_empty())
}

pub struct FacebookResolver {
    http: Http,
}

impl FacebookResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    fn logged_in(&self) -> Option<String> {
        let jar = self.http.jar(PLATFORM);
        jar.get("xs")?;
        jar.get("c_user").map(|c| c.value.clone())
    }
}

/// What a video page hands its player, as variants.
pub fn variants_of(html: &str) -> Vec<Variant> {
    let duration = RE_DURATION
        .captures(html)
        .and_then(|c| c[1].parse::<u64>().ok())
        .filter(|d| *d > 0)
        .map(Duration::from_millis);
    let mut variants = Vec::new();
    let mut file = |key: &str, label: &str, height: Option<u32>| {
        if let Some(url) = json_field(html, key).and_then(|u| Url::parse(&u).ok())
            && !variants.iter().any(|v: &Variant| v.url == url)
        {
            let mut v = Variant::new(url, VariantKind::File);
            v.container = Some(Container::Mp4);
            v.video = Some(VideoCodec::H264);
            v.audio = Some(AudioCodec::Aac);
            v.height = height;
            v.duration = duration;
            v.label = Some(label.to_string());
            v.format_id = Some(key.to_string());
            variants.push(v);
        }
    };
    file("browser_native_hd_url", "hd", Some(720));
    file("playable_url_quality_hd", "hd", Some(720));
    file("browser_native_sd_url", "sd", Some(360));
    file("playable_url", "sd", Some(360));
    if let Some(manifest) = json_field(html, "dash_manifest_url").and_then(|u| Url::parse(&u).ok())
    {
        let mut v = Variant::new(manifest, VariantKind::Dash);
        v.duration = duration;
        v.format_id = Some("dash".into());
        variants.push(v);
    }
    variants
}

#[async_trait]
impl Resolver for FacebookResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Facebook",
            hosts: &["facebook.com", "fb.watch", "fb.com"],
            features: &["videos", "reels", "watch links", "short links"],
            formats: &["mp4", "dash"],
            session: SessionSupport::Optional,
            examples: &["https://www.facebook.com/facebook/videos/10153231379946729/"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        if !matches!(url.scheme(), "http" | "https") {
            return false;
        }
        match url.host_str() {
            Some("fb.watch") => url.path().len() > 1,
            Some(host) if is_facebook_host(host) => is_short_link(url) || video_id(url).is_some(),
            _ => false,
        }
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let target = if is_short_link(url) {
            self.http
                .unwrap_redirects(url, Some(PLATFORM), BROWSER_UA)
                .await?
        } else {
            url.clone()
        };
        let id = video_id(&target).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let page_url =
            Url::parse(&format!("https://www.facebook.com/watch/?v={id}")).expect("valid");
        let headers = [("accept-language".to_string(), "en-US,en;q=0.9".to_string())];
        let fetched = fetch_ok(
            &self.http, &page_url, PLATFORM, BROWSER_UA, &headers, MAX_PAGE,
        )
        .await?;
        let html = fetched.text();
        let variants = variants_of(&html);
        if variants.is_empty() {
            let walled = html.contains("login_form") || html.contains("/login/?next=");
            return Err(if walled && self.logged_in().is_none() {
                ResolveError::login_required(url, PLATFORM, "the page asks to log in")
            } else if html.contains("This content isn't available")
                || html.contains("content isn&#039;t available")
            {
                ResolveError::NotFound(url.clone())
            } else {
                ResolveError::unavailable(url, "the page hands its player no video")
            });
        }
        let page = Page::parse(&html, &page_url);
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(
            RE_VIDEO_ID
                .captures(&html)
                .map(|c| c[1].to_string())
                .unwrap_or(id.clone()),
        );
        resolved.title = RE_TITLE
            .captures(&html)
            .and_then(|c| clean_title(&unescape_json_string(&c[1])))
            .or_else(|| page.title())
            .map(|t| t.trim_end_matches(" | Facebook").to_string());
        resolved.description = page.meta("og:description").and_then(|d| clean_title(&d));
        resolved.uploader = RE_OWNER
            .captures(&html)
            .and_then(|c| clean_title(&unescape_json_string(&c[1])));
        resolved.uploaded_at = RE_PUBLISH
            .captures(&html)
            .and_then(|c| c[1].parse::<i64>().ok())
            .and_then(|t| jiff::Timestamp::from_second(t).ok());
        resolved.duration = variants.iter().find_map(|v| v.duration);
        resolved.thumbnail = RE_THUMB
            .captures(&html)
            .map(|c| unescape_json_string(&c[1]))
            .or_else(|| page.meta("og:image"))
            .and_then(|u| Url::parse(&u).ok());
        resolved.webpage_url = Some(page_url);
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        let Some(user) = self.logged_in() else {
            return Ok(SessionCheck::LoggedOut);
        };
        let me = Url::parse(ME).expect("valid");
        let response = self
            .http
            .get(me.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .send()
            .await?;
        if !response.is_success() || response.url.path().contains("login") {
            return Ok(SessionCheck::LoggedOut);
        }
        let html = response.text(MAX_PAGE).await?;
        let page = Page::parse(&html, &me);
        let name = page
            .title()
            .map(|t| t.trim_end_matches(" | Facebook").trim().to_string())
            .filter(|t| !t.is_empty() && t != "Facebook");
        Ok(SessionCheck::LoggedIn {
            account: name.unwrap_or_else(|| format!("user {user}")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Cookie;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

    fn get(url: &str, status: u16, body: &str, headers: &[(&str, &str)]) -> Exchange {
        let mut all: Vec<(String, String)> =
            vec![("content-type".into(), "text/html; charset=utf-8".into())];
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

    const PAGE: &str = r#"<html><head><title>Watch | Facebook</title><meta property="og:description" content="A short film"><meta property="og:image" content="https://scontent.xx.fbcdn.net/og.jpg"></head><body>
<script type="application/json" data-content-len="1">{"video_id":"10153231379946729","owner":{"__typename":"User","name":"Meta & Friends","id":"1"},"title":{"text":"Our story"},"publish_time":1700000000,"playable_duration_in_ms":95000,"browser_native_hd_url":"https:\/\/video.xx.fbcdn.net\/hd.mp4?a=1&b=2","browser_native_sd_url":"https:\/\/video.xx.fbcdn.net\/sd.mp4","dash_manifest_url":"https:\/\/video.xx.fbcdn.net\/manifest.mpd","preferred_thumbnail":{"image":{"uri":"https:\/\/scontent.xx.fbcdn.net\/thumb.jpg"}}}</script>
</body></html>"#;

    #[tokio::test]
    async fn watch_pages_hand_over_their_files_and_manifest() {
        let mut fixture = Fixture::new("facebook", None);
        fixture.exchanges.push(get(
            "https://fb.watch/abc123/",
            302,
            "",
            &[(
                "location",
                "https://www.facebook.com/facebook/videos/10153231379946729/",
            )],
        ));
        fixture.exchanges.push(get(
            "https://www.facebook.com/facebook/videos/10153231379946729/",
            200,
            "",
            &[],
        ));
        fixture.exchanges.push(get(
            "https://www.facebook.com/watch/?v=10153231379946729",
            200,
            PAGE,
            &[],
        ));
        let resolver = FacebookResolver::new(Http::replay(fixture));
        let short = Url::parse("https://fb.watch/abc123/").unwrap();
        assert!(resolver.matches(&short));
        let resolved = resolver.resolve(&short).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("10153231379946729"));
        assert_eq!(resolved.title.as_deref(), Some("Our story"));
        assert_eq!(resolved.uploader.as_deref(), Some("Meta & Friends"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(95)));
        assert_eq!(
            resolved.thumbnail.unwrap().as_str(),
            "https://scontent.xx.fbcdn.net/thumb.jpg"
        );
        assert_eq!(resolved.variants.len(), 3);
        assert_eq!(
            resolved.variants[0].url.as_str(),
            "https://video.xx.fbcdn.net/hd.mp4?a=1&b=2"
        );
        assert_eq!(resolved.variants[0].label.as_deref(), Some("hd"));
        assert_eq!(resolved.variants[1].label.as_deref(), Some("sd"));
        assert_eq!(resolved.variants[2].kind, VariantKind::Dash);
        for link in [
            "https://www.facebook.com/watch/?v=10153231379946729",
            "https://www.facebook.com/facebook/videos/10153231379946729/",
            "https://www.facebook.com/facebook/videos/our-story/10153231379946729",
            "https://www.facebook.com/reel/10153231379946729",
            "https://m.facebook.com/video.php?v=10153231379946729",
            "https://www.facebook.com/share/v/abcDEF/",
        ] {
            assert!(resolver.matches(&Url::parse(link).unwrap()), "{link}");
        }
        assert!(!resolver.matches(&Url::parse("https://www.facebook.com/facebook").unwrap()));
    }

    #[tokio::test]
    async fn walls_and_missing_videos_are_told_apart_and_sessions_are_checked() {
        let mut fixture = Fixture::new("facebook", None);
        fixture.exchanges.push(get(
            "https://www.facebook.com/watch/?v=10153231379946729",
            200,
            r#"<html><body><form id="login_form"></form></body></html>"#,
            &[],
        ));
        let resolver = FacebookResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.facebook.com/watch/?v=10153231379946729").unwrap();
        assert!(matches!(
            resolver.resolve(&url).await.unwrap_err(),
            ResolveError::LoginRequired { .. }
        ));

        let mut fixture = Fixture::new("facebook", None);
        fixture.exchanges.push(get(
            "https://www.facebook.com/watch/?v=10153231379946729",
            200,
            "<html><body>This content isn't available right now</body></html>",
            &[],
        ));
        let resolver = FacebookResolver::new(Http::replay(fixture));
        assert!(matches!(
            resolver.resolve(&url).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));

        let mut fixture = Fixture::new("facebook", None);
        fixture.exchanges.push(get(
            ME,
            200,
            "<html><head><title>Nick Example | Facebook</title></head></html>",
            &[],
        ));
        let http = Http::replay(fixture);
        let resolver = FacebookResolver::new(http.clone());
        assert_eq!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedOut
        );
        http.with_jar(PLATFORM, |jar| {
            jar.insert(Cookie::new("c_user", "100", "facebook.com"));
            jar.insert(Cookie::new("xs", "abc", "facebook.com"));
        });
        assert_eq!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedIn {
                account: "Nick Example".into()
            }
        );
    }
}
