//! Generic resolver for direct media files, HLS, DASH and Smooth Streaming manifests, and
//! web pages that advertise their video through standard metadata, `<video>` tags, or an
//! embedded player the registry knows.

use std::collections::HashSet;
use std::time::Duration;

use async_trait::async_trait;
use jiff::{Span, SpanRelativeTo};
use url::Url;

use super::page::{Page, ld_objects_of_type};
use super::{
    ClipRange, MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport,
    Variant, VariantKind, clean_title, essence, hls, is_dash_type, is_hls_type, is_ism_type,
    path_extension, status_error, timestamp_hint,
};
use crate::http::{BROWSER_UA, EMBED_BOT_UA, Http, WEB_PLATFORM};
use crate::media::Container;

const MAX_CANDIDATES: usize = 8;

pub struct WebResolver {
    http: Http,
}

impl WebResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[derive(Debug, PartialEq)]
pub enum Kind {
    File(Option<Container>),
    Hls,
    Dash,
    Ism,
    Html,
    Other,
}

/// What a URL points at, from its content type and path.
pub fn classify(url: &Url, content_type: Option<&str>) -> Kind {
    let ct = essence(content_type);
    let ext = path_extension(url);
    if is_hls_type(&ct) || matches!(ext.as_deref(), Some("m3u8") | Some("m3u")) {
        return Kind::Hls;
    }
    if is_dash_type(&ct) || ext.as_deref() == Some("mpd") {
        return Kind::Dash;
    }
    let ism_path = url.path().to_ascii_lowercase();
    if ism_path.ends_with("/manifest") && (ism_path.contains(".ism") || is_ism_type(&ct))
        || (ism_path.ends_with(".ism") || ism_path.ends_with(".isml"))
    {
        return Kind::Ism;
    }
    if ct.starts_with("video/") || ct.starts_with("audio/") {
        return Kind::File(
            Container::from_mime(&ct)
                .or_else(|| ext.as_deref().and_then(Container::from_extension)),
        );
    }
    if ct == "text/html" || ct == "application/xhtml+xml" {
        return Kind::Html;
    }
    if let Some(container) = ext.as_deref().and_then(Container::from_extension)
        && container != Container::Gif
        && (ct.is_empty() || ct == "application/octet-stream" || ct == "binary/octet-stream")
    {
        return Kind::File(Some(container));
    }
    if ct.is_empty() {
        return Kind::Html;
    }
    Kind::Other
}

pub fn file_title(url: &Url) -> Option<String> {
    let name = url.path().rsplit('/').next()?;
    let decoded = percent_encoding::percent_decode_str(name).decode_utf8_lossy();
    let stem = decoded
        .rsplit_once('.')
        .map(|(s, _)| s.to_string())
        .unwrap_or(decoded.to_string());
    clean_title(&stem)
}

struct Candidate {
    url: Url,
    width: Option<u32>,
    height: Option<u32>,
    declared_type: Option<String>,
}

pub struct PageMedia {
    pub title: Option<String>,
    pub description: Option<String>,
    pub duration: Option<Duration>,
    pub thumbnail: Option<Url>,
    pub uploader: Option<String>,
    candidates: Vec<Candidate>,
    pub iframes: Vec<Url>,
}

/// Everything a page says about its video.
pub fn extract(html: &str, base: &Url) -> PageMedia {
    let page = Page::parse(html, base);
    let mut urls: Vec<String> = Vec::new();
    for key in [
        "og:video",
        "og:video:url",
        "og:video:secure_url",
        "twitter:player:stream",
    ] {
        urls.extend(page.meta_all(key));
    }
    let declared_type = page
        .meta("og:video:type")
        .or_else(|| page.meta("twitter:player:stream:content_type"))
        .map(|t| t.to_ascii_lowercase());
    let width = page.meta("og:video:width").and_then(|w| w.parse().ok());
    let height = page.meta("og:video:height").and_then(|h| h.parse().ok());
    let mut duration = page
        .meta("og:video:duration")
        .or_else(|| page.meta("video:duration"))
        .and_then(|d| d.parse::<f64>().ok())
        .filter(|d| *d > 0.0)
        .map(Duration::from_secs_f64);
    let mut title = page.title();
    let mut uploader = None;
    let description = page
        .meta("og:description")
        .or_else(|| page.meta("description"))
        .and_then(|d| clean_title(&d));

    urls.extend(page.video_sources().iter().map(|u| u.to_string()));

    let ld = page.ld_json();
    for object in ld_objects_of_type(&ld, "VideoObject") {
        if let Some(u) = object.get("contentUrl").and_then(|v| v.as_str()) {
            urls.push(u.to_string());
        }
        if title.is_none()
            && let Some(n) = object.get("name").and_then(|v| v.as_str())
        {
            title = clean_title(n);
        }
        if duration.is_none()
            && let Some(d) = object.get("duration").and_then(|v| v.as_str())
        {
            duration = parse_iso_duration(d);
        }
        if uploader.is_none() {
            uploader = object
                .pointer("/author/name")
                .or_else(|| object.pointer("/publisher/name"))
                .and_then(|v| v.as_str())
                .and_then(clean_title);
        }
    }

    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    for raw in urls {
        let Ok(url) = base.join(raw.trim()) else {
            continue;
        };
        if !matches!(url.scheme(), "http" | "https") || !seen.insert(url.to_string()) {
            continue;
        }
        candidates.push(Candidate {
            url,
            width,
            height,
            declared_type: declared_type.clone(),
        });
        if candidates.len() >= MAX_CANDIDATES {
            break;
        }
    }
    PageMedia {
        title,
        description,
        duration,
        thumbnail: page.poster(),
        uploader,
        candidates,
        iframes: page.iframes(),
    }
}

/// Parses ISO 8601 durations such as `PT1H2M3.5S` or `P1DT2H`.
pub fn parse_iso_duration(s: &str) -> Option<Duration> {
    let span: Span = s.parse().ok()?;
    let signed = span.to_duration(SpanRelativeTo::days_are_24_hours()).ok()?;
    signed.is_positive().then(|| signed.unsigned_abs())
}

impl WebResolver {
    async fn probe_candidate(&self, candidate: &Candidate) -> Option<Vec<Variant>> {
        if candidate
            .declared_type
            .as_deref()
            .is_some_and(|t| t.starts_with("text/html") || t.contains("flash"))
            && candidate.url.path().ends_with(".html")
        {
            return None;
        }
        let response = self
            .http
            .get(candidate.url.clone())
            .user_agent(BROWSER_UA)
            .header("range", "bytes=0-0")
            .media()
            .send()
            .await
            .ok()?;
        if !response.status.is_success() {
            return None;
        }
        let content_type = response.content_type().map(str::to_owned);
        let final_url = response.url.clone();
        let size = response
            .header("content-range")
            .and_then(|v| v.rsplit('/').next())
            .and_then(|v| v.parse::<u64>().ok())
            .or_else(|| {
                if response.status.as_u16() == 200 {
                    response.content_length()
                } else {
                    None
                }
            });
        drop(response);
        match classify(&final_url, content_type.as_deref()) {
            Kind::File(container) => {
                let mut v = Variant::new(final_url, VariantKind::File);
                v.container = container;
                v.width = candidate.width;
                v.height = candidate.height;
                v.size = size;
                Some(vec![v])
            }
            Kind::Hls => hls::expand(&self.http, &final_url, WEB_PLATFORM, BROWSER_UA, &[])
                .await
                .ok()
                .map(|e| e.variants),
            Kind::Dash => Some(vec![Variant::new(final_url, VariantKind::Dash)]),
            Kind::Ism => Some(vec![Variant::new(final_url, VariantKind::Ism)]),
            Kind::Html | Kind::Other => None,
        }
    }

    async fn attempt(&self, user_agent: &str, url: &Url) -> Result<Option<Resolved>, ResolveError> {
        let response = self
            .http
            .get(url.clone())
            .user_agent(user_agent)
            .media()
            .send()
            .await?;
        if let Some(error) = status_error(response.status, url) {
            return Err(error);
        }
        let final_url = response.url.clone();
        let content_type = response.content_type().map(str::to_owned);
        let length = response.content_length();
        match classify(&final_url, content_type.as_deref()) {
            Kind::File(container) => {
                drop(response);
                let mut v = Variant::new(final_url.clone(), VariantKind::File);
                v.container = container;
                v.size = length;
                let mut resolved = Resolved::new("web");
                resolved.title = file_title(&final_url);
                resolved.webpage_url = Some(final_url);
                resolved.variants = vec![v];
                Ok(Some(resolved))
            }
            Kind::Hls => {
                drop(response);
                let expanded =
                    hls::expand(&self.http, &final_url, WEB_PLATFORM, user_agent, &[]).await?;
                let mut resolved = Resolved::new("web");
                resolved.title = file_title(&final_url);
                resolved.duration = expanded.duration;
                resolved.live = expanded.live;
                resolved.subtitles = expanded.subtitles;
                resolved.variants = expanded.variants;
                Ok(Some(resolved))
            }
            Kind::Dash => {
                drop(response);
                let mut resolved = Resolved::new("web");
                resolved.title = file_title(&final_url);
                resolved.variants = vec![Variant::new(final_url, VariantKind::Dash)];
                Ok(Some(resolved))
            }
            Kind::Ism => {
                drop(response);
                let mut resolved = Resolved::new("web");
                resolved.title = file_title(&final_url);
                resolved.variants = vec![Variant::new(final_url, VariantKind::Ism)];
                Ok(Some(resolved))
            }
            Kind::Html => {
                let (body, _) = response.bytes_up_to(MAX_PAGE).await?;
                let html = String::from_utf8_lossy(&body).into_owned();
                let media = extract(&html, &final_url);
                let mut variants = Vec::new();
                for candidate in &media.candidates {
                    if let Some(found) = self.probe_candidate(candidate).await {
                        variants.extend(found);
                    }
                }
                if variants.is_empty() {
                    // A page whose only video is an embedded player another resolver knows.
                    if let Some(embed) = media
                        .iframes
                        .iter()
                        .find(|frame| frame.host_str() != final_url.host_str())
                    {
                        return Err(ResolveError::Redirect(embed.clone()));
                    }
                    return Ok(None);
                }
                for v in &mut variants {
                    if v.duration.is_none() {
                        v.duration = media.duration;
                    }
                }
                let mut resolved = Resolved::new("web");
                resolved.title = media.title;
                resolved.description = media.description;
                resolved.uploader = media.uploader;
                resolved.duration = media.duration;
                resolved.thumbnail = media.thumbnail;
                resolved.webpage_url = Some(final_url);
                resolved.clip = timestamp_hint(url).map(|start| ClipRange { start, end: None });
                resolved.variants = variants;
                Ok(Some(resolved))
            }
            Kind::Other => Ok(None),
        }
    }
}

#[async_trait]
impl Resolver for WebResolver {
    fn id(&self) -> &'static str {
        "web"
    }

    fn platform(&self) -> Platform {
        Platform {
            id: "web",
            name: "Any web page or media URL",
            hosts: &["*"],
            features: &[
                "direct files",
                "hls",
                "dash",
                "smooth streaming",
                "open graph pages",
                "json-ld",
                "video tags",
                "embedded players",
            ],
            formats: &["mp4", "webm", "mkv", "mov", "hls", "dash", "ism"],
            session: SessionSupport::Optional,
            examples: &["https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        if let Some(found) = self.attempt(BROWSER_UA, url).await? {
            return Ok(Resolution::Media(found));
        }
        if let Some(found) = self.attempt(EMBED_BOT_UA, url).await? {
            return Ok(Resolution::Media(found));
        }
        Err(ResolveError::NotFound(url.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

    #[test]
    fn parses_time_and_day_durations() {
        assert_eq!(
            parse_iso_duration("PT1H2M3.5S"),
            Some(Duration::from_secs_f64(3723.5))
        );
        assert_eq!(
            parse_iso_duration("P1DT1H"),
            Some(Duration::from_secs(90_000))
        );
        assert_eq!(parse_iso_duration("PT45S"), Some(Duration::from_secs(45)));
    }

    #[test]
    fn rejects_calendar_units_zero_and_garbage() {
        assert_eq!(parse_iso_duration("P1M"), None);
        assert_eq!(parse_iso_duration("PT0S"), None);
        assert_eq!(parse_iso_duration("ten seconds"), None);
    }

    #[test]
    fn classifies_by_type_and_path() {
        let url = |s: &str| Url::parse(s).unwrap();
        assert_eq!(classify(&url("https://h/v.m3u8"), None), Kind::Hls);
        assert_eq!(
            classify(&url("https://h/v"), Some("application/dash+xml")),
            Kind::Dash
        );
        assert_eq!(classify(&url("https://h/v.ism/Manifest"), None), Kind::Ism);
        assert_eq!(
            classify(&url("https://h/v.mp4"), Some("application/octet-stream")),
            Kind::File(Some(Container::Mp4))
        );
        assert_eq!(
            classify(&url("https://h/page"), Some("text/html; charset=utf-8")),
            Kind::Html
        );
        assert_eq!(classify(&url("https://h/page"), None), Kind::Html);
        assert_eq!(
            classify(&url("https://h/x.json"), Some("application/json")),
            Kind::Other
        );
    }

    fn exchange(
        url: &str,
        content_type: &str,
        body: &str,
        status: u16,
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

    #[tokio::test]
    async fn pages_yield_their_open_graph_video() {
        let page = r#"<html><head><meta property="og:title" content="Clip"><meta property="og:video" content="https://cdn.test/clip.mp4"><meta property="og:video:width" content="1280"><meta property="og:video:height" content="720"><meta property="og:video:duration" content="12"></head></html>"#;
        let mut fixture = Fixture::new("web", None);
        fixture.exchanges.push(exchange(
            "https://site.test/watch",
            "text/html",
            page,
            200,
            &[],
        ));
        fixture.exchanges.push(exchange(
            "https://cdn.test/clip.mp4",
            "video/mp4",
            "",
            206,
            &[("content-range", "bytes 0-0/5000")],
        ));
        let resolver = WebResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://site.test/watch?t=5").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.title.as_deref(), Some("Clip"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(12)));
        assert_eq!(resolved.clip.unwrap().start, Duration::from_secs(5));
        let variant = &resolved.variants[0];
        assert_eq!(variant.kind, VariantKind::File);
        assert_eq!(variant.size, Some(5000));
        assert_eq!((variant.width, variant.height), (Some(1280), Some(720)));
        assert_eq!(variant.container, Some(Container::Mp4));
    }

    #[tokio::test]
    async fn pages_with_only_a_foreign_player_redirect_to_it() {
        let page = r#"<html><body><iframe src="https://www.youtube.com/embed/abc"></iframe></body></html>"#;
        let mut fixture = Fixture::new("web", None);
        fixture.exchanges.push(exchange(
            "https://blog.test/post",
            "text/html",
            page,
            200,
            &[],
        ));
        let resolver = WebResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://blog.test/post").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(error, ResolveError::Redirect(u) if u.as_str() == "https://www.youtube.com/embed/abc")
        );
    }

    #[tokio::test]
    async fn direct_files_and_missing_pages() {
        let mut fixture = Fixture::new("web", None);
        fixture.exchanges.push(exchange(
            "https://cdn.test/My%20Clip.webm",
            "video/webm",
            "",
            200,
            &[("content-length", "77")],
        ));
        fixture
            .exchanges
            .push(exchange("https://cdn.test/gone", "text/html", "", 404, &[]));
        let resolver = WebResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://cdn.test/My%20Clip.webm").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.title.as_deref(), Some("My Clip"));
        assert_eq!(resolved.variants[0].size, Some(77));
        assert_eq!(resolved.variants[0].container, Some(Container::Webm));
        let error = resolver
            .resolve(&Url::parse("https://cdn.test/gone").unwrap())
            .await
            .unwrap_err();
        assert!(matches!(error, ResolveError::NotFound(_)));
    }
}
