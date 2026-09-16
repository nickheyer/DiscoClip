//! bTV Plus (btvplus.bg) programmes, series episodes and news broadcasts: the page names
//! its player page, whose video.js call lists the HLS sources. The site's Cloudflare edge
//! serves addresses outside Bulgaria a browser challenge instead of any page.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    Fetched, MAX_PAGE, Page, Platform, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, clean_title, essence, fetch, hls, is_hls_type, navigation_headers,
    status_error, util,
};
use crate::http::{BROWSER_UA, Http};

pub const PLATFORM: &str = "btvplus";
const SITE: &str = "https://btvplus.bg";
/// What a Cloudflare challenge in place of the page means: the site is served only to
/// Bulgarian addresses.
const GEO_BLOCKED: &str = "available only in BG: btvplus.bg answers this address with a Cloudflare browser challenge instead of the page";

/// `/produkt/{predavaniya|seriali|novini}/{id}…`
static RE_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/produkt/(?:predavaniya|seriali|novini)/(\d+)").unwrap());
/// `var videoUrl = '/player/…'` on the product page.
static RE_PLAYER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"var\s+videoUrl\s*=\s*['"]([^'"]+)['"]"#).unwrap());
/// `videojs('player', {…})` in the player configuration script.
static RE_VIDEOJS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"videojs\(["'][^"']+["'],"#).unwrap());

/// The numeric id a product link names.
pub fn video_id(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    if host != "btvplus.bg" {
        return None;
    }
    RE_LINK.captures(url.path()).map(|c| c[1].to_string())
}

/// The object the player configuration hands `videojs(…)`.
pub fn videojs_data(config: &str) -> Option<Value> {
    let found = RE_VIDEOJS.find(config)?;
    let after = config[found.end()..].trim_start();
    let end = util::balanced_js_end(after)?;
    util::parse_js(&after[..end])
}

/// Cloudflare's "Just a moment…" page, served with HTTP 403 in place of the page asked
/// for.
pub fn is_browser_challenge(html: &str) -> bool {
    html.contains("Just a moment") && html.contains("challenge")
}

pub struct BtvplusResolver {
    http: Http,
}

impl BtvplusResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// GETs a page of the site, telling the Cloudflare challenge the edge serves addresses
    /// outside Bulgaria apart from the site's own answers.
    async fn fetch_site(
        &self,
        url: &Url,
        origin: &Url,
        headers: &[(String, String)],
    ) -> Result<Fetched, ResolveError> {
        let fetched = fetch(&self.http, url, PLATFORM, BROWSER_UA, headers, MAX_PAGE).await?;
        if fetched.status.as_u16() == 403 && is_browser_challenge(&fetched.text()) {
            return Err(ResolveError::unavailable(origin, GEO_BLOCKED));
        }
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        Ok(fetched)
    }
}

#[async_trait]
impl Resolver for BtvplusResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "bTV Plus",
            hosts: &["btvplus.bg"],
            features: &["videos", "shows", "news"],
            formats: &["hls"],
            session: SessionSupport::None,
            examples: &[
                "https://btvplus.bg/produkt/predavaniya/67271/btv-reporterite/btv-reporterite-12-07-2025-g",
                "https://btvplus.bg/produkt/seriali/66942/sezon-2/plen-sezon-2-epizod-55",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        video_id(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let id = video_id(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let fetched = self.fetch_site(url, url, &navigation_headers()).await?;
        let html = fetched.text();
        let (title, thumbnail, description) = {
            let page = Page::parse(&html, &fetched.url);
            (
                page.meta("og:title")
                    .and_then(|t| clean_title(&t))
                    .or_else(|| {
                        util::element_by_class(&html, "product-title")
                            .map(|t| util::clean_html(&t))
                            .and_then(|t| clean_title(&t))
                    }),
                page.meta("og:image").and_then(|t| Url::parse(&t).ok()),
                page.meta("og:description").and_then(|d| clean_title(&d)),
            )
        };
        let player_path = util::search(&RE_PLAYER, &html)
            .ok_or_else(|| ResolveError::malformed(url, "the page names no player URL"))?;
        let site = Url::parse(SITE).expect("valid");
        let player_url = util::join_url(Some(&site), &player_path)
            .ok_or_else(|| ResolveError::malformed(url, format!("bad player URL {player_path}")))?;
        let player = self.fetch_site(&player_url, url, &[]).await?.json(url)?;
        let config = player["config"]
            .as_str()
            .ok_or_else(|| ResolveError::malformed(url, "the player answer has no config"))?;
        let data = videojs_data(config)
            .ok_or_else(|| ResolveError::malformed(url, "the player config has no videojs call"))?;

        let mut resolved = Resolved::new(PLATFORM);
        let mut failure = None;
        for source in data["sources"].as_array().into_iter().flatten() {
            let Some(src) = util::url_of(&source["src"], None) else {
                continue;
            };
            let kind = essence(source["type"].as_str());
            if !is_hls_type(&kind) {
                tracing::warn!(platform = PLATFORM, %src, "unknown format type {kind}");
                continue;
            }
            match hls::expand(&self.http, &src, PLATFORM, BROWSER_UA, &[]).await {
                Ok(expanded) => {
                    for mut variant in expanded.variants {
                        variant.format_id =
                            Some(format!("hls-{}", variant.label.clone().unwrap_or_default()));
                        resolved.variants.push(variant);
                    }
                    resolved.subtitles.extend(expanded.subtitles);
                    resolved.duration = resolved.duration.or(expanded.duration);
                    resolved.live |= expanded.live;
                }
                Err(error) => failure = Some(error),
            }
        }
        if resolved.variants.is_empty() {
            return Err(failure.unwrap_or_else(|| {
                ResolveError::unavailable(url, "the player lists no HLS source")
            }));
        }
        resolved.id = Some(id);
        resolved.title = title;
        resolved.thumbnail = thumbnail;
        resolved.description = description;
        resolved.webpage_url = Some(fetched.url.clone());
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

    #[test]
    fn links_are_read() {
        let id = |s: &str| video_id(&Url::parse(s).unwrap());
        assert_eq!(
            id(
                "https://btvplus.bg/produkt/predavaniya/67271/btv-reporterite/btv-reporterite-12-07-2025-g"
            ),
            Some("67271".into())
        );
        assert_eq!(
            id("https://btvplus.bg/produkt/seriali/66942/sezon-2/plen-sezon-2-epizod-55"),
            Some("66942".into())
        );
        assert_eq!(
            id("https://btvplus.bg/produkt/novini/67270/btv-novinite-centralna-emisija-12-07-2025"),
            Some("67270".into())
        );
        assert_eq!(
            id("https://www.btvplus.bg/produkt/novini/1"),
            Some("1".into())
        );
        assert_eq!(id("https://btvplus.bg/produkt/filmi/67270/x"), None);
        assert_eq!(id("https://btv.bg/produkt/novini/67270"), None);
    }

    #[test]
    fn videojs_calls_are_read() {
        let config = "var p = videojs('player', {sources: [{src: 'https://cdn.test/a.m3u8', type: 'application/x-mpegURL'}], autoplay: false});";
        let data = videojs_data(config).unwrap();
        assert_eq!(data["sources"][0]["src"], "https://cdn.test/a.m3u8");
        assert!(videojs_data("nothing here").is_none());
    }

    const PAGE: &str = r#"<html><head><meta property="og:title" content="bTV Репортерите - 12.07.2025 г. "/>
<meta property="og:image" content="https://cdn.btv.bg/media/images/940x529/Jul2025/2113606319.jpg"/>
<meta property="og:description" content="Разследвания"/></head>
<body><h1 class="product-title">bTV Репортерите</h1><script>var videoUrl = '/player/67271';</script></body></html>"#;

    /// The answer the site's edge gives every address outside Bulgaria, as recorded from
    /// the United States on 2026-09-15 (`cf-mitigated: challenge`, HTTP 403).
    const CHALLENGE: &str = r#"<!DOCTYPE html><html lang="en-US"><head><title>Just a moment...</title><meta http-equiv="Content-Type" content="text/html; charset=UTF-8"><meta name="robots" content="noindex,nofollow"></head><body><div class="main-wrapper" role="main"><div class="main-content"><noscript><div class="h2"><span id="challenge-error-text">Enable JavaScript and cookies to continue</span></div></noscript></div></div><script nonce="IckSw492ms3PhyGHaEJgEE">(function(){window._cf_chl_opt = {cFPWv: 'g',cType: 'interactive',cZone: 'btvplus.bg'};}());</script></body></html>"#;

    #[tokio::test]
    async fn videos_resolve_to_hls() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://btvplus.bg/produkt/predavaniya/67271/btv-reporterite/btv-reporterite-12-07-2025-g",
            200,
            "text/html",
            PAGE.into(),
        ));
        fixture.exchanges.push(get("https://btvplus.bg/player/67271", 200, "application/json", json!({
            "config": "videojs(\"player\", {sources: [{src: \"https://vod.btv.bg/x/master.m3u8\", type: \"application/x-mpegURL\"}, {src: \"https://vod.btv.bg/x/other.mpd\", type: \"application/dash+xml\"}]});"
        }).to_string()));
        fixture.exchanges.push(get(
            "https://vod.btv.bg/x/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720\n720.m3u8\n".into(),
        ));
        fixture.exchanges.push(get(
            "https://vod.btv.bg/x/720.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n0.ts\n#EXTINF:5.0,\n1.ts\n#EXT-X-ENDLIST\n".into(),
        ));
        let resolver = BtvplusResolver::new(Http::replay(fixture));
        let url = Url::parse(
            "https://btvplus.bg/produkt/predavaniya/67271/btv-reporterite/btv-reporterite-12-07-2025-g",
        )
        .unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("67271"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("bTV Репортерите - 12.07.2025 г.")
        );
        assert_eq!(resolved.description.as_deref(), Some("Разследвания"));
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://cdn.btv.bg/media/images/940x529/Jul2025/2113606319.jpg"
        );
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].height, Some(720));
        assert_eq!(resolved.variants[0].format_id.as_deref(), Some("hls-720p"));
        assert_eq!(resolved.duration, Some(std::time::Duration::from_secs(15)));
    }

    #[tokio::test]
    async fn addresses_outside_bulgaria_are_told_the_site_is_geo_blocked() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://btvplus.bg/produkt/seriali/66942/sezon-2/plen-sezon-2-epizod-55",
            403,
            "text/html; charset=UTF-8",
            CHALLENGE.into(),
        ));
        fixture.exchanges.push(get(
            "https://btvplus.bg/produkt/novini/3/page-ok-player-challenged",
            200,
            "text/html",
            PAGE.replace("/player/67271", "/player/3"),
        ));
        fixture.exchanges.push(get(
            "https://btvplus.bg/player/3",
            403,
            "text/html; charset=UTF-8",
            CHALLENGE.into(),
        ));
        fixture.exchanges.push(get(
            "https://btvplus.bg/produkt/novini/4/plain-forbidden",
            403,
            "text/html",
            "<html><body>Forbidden</body></html>".into(),
        ));
        let resolver = BtvplusResolver::new(Http::replay(fixture));
        for path in [
            "/produkt/seriali/66942/sezon-2/plen-sezon-2-epizod-55",
            "/produkt/novini/3/page-ok-player-challenged",
        ] {
            let url = Url::parse(&format!("https://btvplus.bg{path}")).unwrap();
            let error = resolver.resolve(&url).await.unwrap_err();
            assert!(
                matches!(&error, ResolveError::Unavailable { url: at, reason } if *at == url && reason.starts_with("available only in BG")),
                "{path}: {error}"
            );
        }
        let error = resolver
            .resolve(&Url::parse("https://btvplus.bg/produkt/novini/4/plain-forbidden").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "HTTP 403 Forbidden"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn missing_videos_and_playerless_pages_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://btvplus.bg/produkt/novini/1/gone",
            404,
            "text/html",
            "<html>404</html>".into(),
        ));
        fixture.exchanges.push(get(
            "https://btvplus.bg/produkt/novini/2/empty",
            200,
            "text/html",
            "<html><head><title>x</title></head><body>no player</body></html>".into(),
        ));
        let resolver = BtvplusResolver::new(Http::replay(fixture));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://btvplus.bg/produkt/novini/1/gone").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(&Url::parse("https://btvplus.bg/produkt/novini/2/empty").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Malformed { detail, .. } if detail.contains("player URL")),
            "{error}"
        );
    }
}
