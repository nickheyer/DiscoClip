//! Bloomberg videos and the articles that carry one, through the multimedia embed API the
//! site's player reads once the page names the video's id. The site answers anything but
//! a browser's TLS fingerprint with a bot check, so pages and the API are read as Chrome.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Tag,
    clean_title, fetch_as_browser, hls, navigation_headers, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::MediaKind;

pub const PLATFORM: &str = "bloomberg";
const EMBED_API: &str = "https://www.bloomberg.com/multimedia/api/embed?id=";

/// The id is the last path segment of any Bloomberg link.
static RE_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/(?:[^/]+/)*([^/?#]+)").unwrap());
/// Where pages name their video: `"bmmrId": "…"`, `videoId: "…"`, `data-bmmrid="…"`, and
/// in the React payload of today's video pages, `\"videoId\":\"…\"`.
static RE_IDS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r#"["']bmmrId["']\s*:\s*"([^"\n]+)""#,
        r#"["']bmmrId["']\s*:\s*'([^'\n]+)'"#,
        r#"\\?"videoId\\?"\s*:\s*\\?"([^"\\\n]+)\\?""#,
        r#"videoId\s*:\s*"([^"\n]+)""#,
        r#"videoId\s*:\s*'([^'\n]+)'"#,
        r#"data-bmmrid="([^"\n]+)""#,
        r#"data-bmmrid='([^'\n]+)'"#,
    ]
    .iter()
    .map(|p| Regex::new(p).unwrap())
    .collect()
});
/// Feature pages set the player up with `BPlayer(null, {…});` instead.
static RE_BPLAYER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"BPlayer\(null,\s*").unwrap());
/// What the site wraps titles in: `Watch …: Video`, `… - Bloomberg`.
static RE_VIDEO_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^Watch\s+|(?::\s*Video|\s+-\s+Bloomberg)$").unwrap());

/// The page name a link ends with.
pub fn page_name(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "bloomberg.com" && host != "www.bloomberg.com" {
        return None;
    }
    RE_NAME.captures(url.path()).map(|c| c[1].to_string())
}

/// The video id a page names, in the markup or in its `BPlayer` setup.
pub fn video_id(html: &str) -> Option<String> {
    if let Some(id) = RE_IDS.iter().find_map(|re| util::search(re, html)) {
        return Some(id);
    }
    let start = RE_BPLAYER.find(html)?.end();
    let rest = &html[start..];
    let end = util::balanced_js_end(rest)?;
    util::parse_js(&rest[..end]).and_then(|data| util::text(&data["id"]))
}

pub struct BloombergResolver {
    http: Http,
}

impl BloombergResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for BloombergResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Bloomberg",
            hosts: &["bloomberg.com"],
            features: &["videos", "articles"],
            formats: &["hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::News],
            session: SessionSupport::None,
            examples: &[
                "https://www.bloomberg.com/news/videos/2021-09-14/apple-unveils-the-new-iphone-13-stock-doesn-t-move-much-video",
                "http://www.bloomberg.com/features/2016-hello-world-new-zealand/",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        page_name(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        page_name(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let fetched =
            fetch_as_browser(&self.http, url, PLATFORM, &navigation_headers(), MAX_PAGE).await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        let html = fetched.text();
        let id = video_id(&html).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let (title, description, thumbnail) = {
            let page = Page::parse(&html, url);
            (
                page.meta("og:title")
                    .map(|t| RE_VIDEO_SUFFIX.replace_all(&t, "").into_owned())
                    .and_then(|t| clean_title(&t))
                    .or_else(|| page.title()),
                page.meta("og:description").and_then(|d| clean_title(&d)),
                page.meta("og:image")
                    .and_then(|t| util::join_url(Some(url), &t)),
            )
        };
        // The bot check's cookies from the page visit get the API refused. A fresh browser
        // request without them is answered.
        let api = Url::parse(&format!("{EMBED_API}{id}")).expect("valid");
        let response = self
            .http
            .get(api.clone())
            .platform(PLATFORM)
            .impersonate()
            .no_cookies()
            .send()
            .await?;
        if let Some(error) = status_error(response.status, &api) {
            return Err(error);
        }
        let embed: Value = serde_json::from_slice(&response.bytes(MAX_PAGE).await?)
            .map_err(|e| ResolveError::malformed(url, format!("embed JSON: {e}")))?;
        let mut variants = Vec::new();
        let mut hds_offered = false;
        let mut failure = None;
        for stream in embed["streams"].as_array().into_iter().flatten() {
            let Some(stream_url) = util::url_of(&stream["url"], None) else {
                continue;
            };
            if stream["muxing_format"].as_str() == Some("TS") {
                match hls::expand(&self.http, &stream_url, PLATFORM, BROWSER_UA, &[]).await {
                    Ok(expanded) => variants.extend(expanded.variants),
                    Err(error) => failure = Some(error),
                }
            } else {
                hds_offered = true;
            }
        }
        if variants.is_empty() {
            if let Some(error) = failure {
                return Err(error);
            }
            if hds_offered {
                return Err(ResolveError::unavailable(
                    url,
                    "only an HDS (f4m) stream is offered",
                ));
            }
            return Err(ResolveError::NotFound(url.clone()));
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id);
        resolved.title = title;
        resolved.description = description;
        resolved.thumbnail = thumbnail;
        resolved.webpage_url = Some(url.clone());
        resolved.duration = variants.iter().find_map(|v| v.duration);
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};
    use serde_json::json;

    fn get(url: &str, status: u16, content_type: &str, body: String) -> Exchange {
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
                body: RecordedBody::Text(body),
                truncated: false,
            },
        }
    }

    const MASTER: &str = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1800000,RESOLUTION=1280x720\nhttps://cdn.bloomberg.test/v/720.m3u8\n";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\n0.ts\n#EXT-X-ENDLIST\n";

    #[test]
    fn links_are_read() {
        let name = |s: &str| page_name(&Url::parse(s).unwrap());
        assert_eq!(
            name(
                "https://www.bloomberg.com/news/videos/2021-09-14/apple-unveils-the-new-iphone-13-stock-doesn-t-move-much-video"
            ),
            Some("apple-unveils-the-new-iphone-13-stock-doesn-t-move-much-video".into())
        );
        assert_eq!(
            name("http://www.bloomberg.com/features/2016-hello-world-new-zealand/"),
            Some("2016-hello-world-new-zealand".into())
        );
        assert_eq!(
            name(
                "https://www.bloomberg.com/politics/articles/2017-02-08/le-pen-aide-briefed-french-central-banker-on-plan-to-print-money"
            ),
            Some("le-pen-aide-briefed-french-central-banker-on-plan-to-print-money".into())
        );
        assert_eq!(
            name(
                "http://www.bloomberg.com/politics/videos/2015-11-25/karl-rove-on-jeb-bush-s-struggles-stopping-trump?x=1"
            ),
            Some("karl-rove-on-jeb-bush-s-struggles-stopping-trump".into())
        );
        assert_eq!(name("https://www.bloomberg.com/"), None);
        assert_eq!(name("https://example.com/news/videos/x"), None);
    }

    #[test]
    fn video_ids_are_found_in_every_shape() {
        assert_eq!(
            video_id(r#"<script>{"bmmrId":"V8cFcYMxTHaMcEiiYVr39A"}</script>"#).as_deref(),
            Some("V8cFcYMxTHaMcEiiYVr39A")
        );
        assert_eq!(
            video_id("player({ videoId: 'abc-123' })").as_deref(),
            Some("abc-123")
        );
        assert_eq!(
            video_id(r#"<div data-bmmrid="xyz"></div>"#).as_deref(),
            Some("xyz")
        );
        assert_eq!(
            video_id(
                r#"BPlayer(null, {"id": "938c7e72-3f25-4ddb-8b85-a9be731baa74", "muted": true});"#
            )
            .as_deref(),
            Some("938c7e72-3f25-4ddb-8b85-a9be731baa74")
        );
        assert_eq!(
            video_id(r#"{\"children\":[\"$\",\"$L21\",null,{\"videoId\":\"57c70571-8331-4c76-8c70-48a2615af7f4\",\"durationMs\":247000}"#).as_deref(),
            Some("57c70571-8331-4c76-8c70-48a2615af7f4")
        );
        assert_eq!(video_id("<html>no player</html>"), None);
    }

    #[tokio::test]
    async fn videos_resolve_to_hls() {
        let page_url = "https://www.bloomberg.com/news/videos/2021-09-14/apple-unveils-video";
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(page_url, 200, "text/html", concat!(
            "<html><head><meta property=\"og:title\" content=\"Apple Unveils the New IPhone 13: Video\">",
            "<meta property=\"og:description\" content=\"Apple unveiled it.\">",
            "<meta property=\"og:image\" content=\"https://assets.bwbx.io/images/x.jpg\">",
            "</head><body><script>window.__PRELOADED_STATE__ = {\"video\": {\"bmmrId\": \"V8cFcYMxTHaMcEiiYVr39A\"}}</script></body></html>"
        ).into()));
        fixture.exchanges.push(get(
            &format!("{EMBED_API}V8cFcYMxTHaMcEiiYVr39A"),
            200,
            "application/json",
            json!({"streams": [
                {"url": "https://cdn.bloomberg.test/v/master.f4m", "muxing_format": "F4M"},
                {"url": "https://cdn.bloomberg.test/v/master.m3u8", "muxing_format": "TS"},
                {"url": null, "muxing_format": "TS"}
            ]})
            .to_string(),
        ));
        fixture.exchanges.push(get(
            "https://cdn.bloomberg.test/v/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MASTER.into(),
        ));
        fixture.exchanges.push(get(
            "https://cdn.bloomberg.test/v/720.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MEDIA.into(),
        ));
        let resolver = BloombergResolver::new(Http::replay(fixture));
        let url = Url::parse(page_url).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("V8cFcYMxTHaMcEiiYVr39A"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Apple Unveils the New IPhone 13")
        );
        assert_eq!(resolved.description.as_deref(), Some("Apple unveiled it."));
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://assets.bwbx.io/images/x.jpg"
        );
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].height, Some(720));
        assert_eq!(resolved.duration, Some(std::time::Duration::from_secs(6)));
    }

    #[tokio::test]
    async fn pages_without_a_playable_video_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.bloomberg.com/news/articles/2015-11-12/five-strange-things",
            200,
            "text/html",
            "<html><body><p>Just text.</p></body></html>".into(),
        ));
        fixture.exchanges.push(get(
            "https://www.bloomberg.com/features/2016-hello-world-new-zealand/",
            200,
            "text/html",
            "<html><script>BPlayer(null, {id: 'hds-only'});</script></html>".into(),
        ));
        fixture.exchanges.push(get(
            &format!("{EMBED_API}hds-only"),
            200,
            "application/json",
            json!({"streams": [{"url": "https://cdn.bloomberg.test/v/master.f4m", "muxing_format": "F4M"}]}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://www.bloomberg.com/news/videos/gone",
            404,
            "text/html",
            "<html>gone</html>".into(),
        ));
        let resolver = BloombergResolver::new(Http::replay(fixture));
        let resolve = |s: &str| {
            let url = Url::parse(s).unwrap();
            let resolver = &resolver;
            async move { resolver.resolve(&url).await.unwrap_err() }
        };
        assert!(matches!(
            resolve("https://www.bloomberg.com/news/articles/2015-11-12/five-strange-things").await,
            ResolveError::NotFound(_)
        ));
        let error =
            resolve("https://www.bloomberg.com/features/2016-hello-world-new-zealand/").await;
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("HDS")),
            "{error}"
        );
        assert!(matches!(
            resolve("https://www.bloomberg.com/news/videos/gone").await,
            ResolveError::NotFound(_)
        ));
    }
}
