//! Generic resolver for direct media files (video, audio and images), HLS, DASH and
//! Smooth Streaming manifests, and web pages that advertise their video through standard
//! metadata, `<video>` tags, or an embedded player the registry knows. A page reached
//! through a redirect to another host, as short links are, is handed back to the registry
//! so the host's own resolver takes it. A link to any other kind of file, a document or
//! an archive on an arbitrary host, is refused as unsupported: only the hosts with a
//! resolver of their own serve such files.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use jiff::{Span, SpanRelativeTo};
use scraper::Selector;
use url::Url;

use super::page::{Page, ld_objects_of_type};
use super::{
    ClipRange, MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport,
    Tag, Variant, VariantKind, clean_title, essence, hls, is_dash_type, is_hls_type, is_ism_type,
    path_extension, status_error, timestamp_hint,
};
use crate::http::{BROWSER_UA, EMBED_BOT_UA, Http, WEB_PLATFORM};
use crate::media::{Container, MediaKind};

const MAX_CANDIDATES: usize = 8;

pub struct WebResolver {
    http: Http,
    /// The resolvers whose players a page may embed; a page carrying one is handed to it.
    players: Vec<Arc<dyn Resolver>>,
}

impl WebResolver {
    pub fn new(http: Http, players: Vec<Arc<dyn Resolver>>) -> Self {
        Self { http, players }
    }
}

#[derive(Debug, PartialEq)]
pub enum Kind {
    /// One media file: a video, audio alone or a still image.
    File(Option<Container>, MediaKind),
    Hls,
    Dash,
    Ism,
    Html,
    Other,
}

/// What a URL points at, from its content type and path. A file is media when its type is
/// `video/*`, `audio/*` or `image/*`, or when it is served without a type and its
/// extension names a format; SVG, which is markup rather than a picture, is not taken.
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
    if ct == "image/svg+xml" {
        return Kind::Other;
    }
    if ct.starts_with("video/") || ct.starts_with("audio/") || ct.starts_with("image/") {
        let container = Container::from_mime(&ct)
            .or_else(|| ext.as_deref().and_then(Container::from_extension));
        let kind = container
            .as_ref()
            .map_or_else(|| MediaKind::from_mime(&ct), Container::kind);
        return Kind::File(container, kind);
    }
    if ct == "text/html" || ct == "application/xhtml+xml" {
        return Kind::Html;
    }
    if let Some(container) = ext.as_deref().and_then(Container::from_extension)
        && container != Container::Gif
        && (ct.is_empty() || ct == "application/octet-stream" || ct == "binary/octet-stream")
    {
        let kind = container.kind();
        return Kind::File(Some(container), kind);
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
    pub embeds: Vec<Url>,
}

/// Whether one of `players` takes `url`.
fn known_player(players: &[Arc<dyn Resolver>], url: &Url) -> bool {
    players.iter().any(|p| p.matches(url))
}

/// Player links in frames, scripts, metadata and each provider's inline markup.
fn embedded_players(page: &Page, players: &[Arc<dyn Resolver>]) -> Vec<Url> {
    let mut links = page.iframes();
    for element in page
        .document()
        .select(&Selector::parse("script[src], object[data]").expect("valid"))
    {
        if let Some(url) = element
            .value()
            .attr("src")
            .or_else(|| element.value().attr("data"))
            .and_then(|src| page.url().join(src).ok())
        {
            links.push(url);
        }
    }
    for key in [
        "twitter:player",
        "og:video",
        "og:video:url",
        "og:video:secure_url",
    ] {
        links.extend(
            page.meta_all(key)
                .iter()
                .filter_map(|src| page.url().join(src).ok()),
        );
    }
    let ld = page.ld_json();
    for object in ld_objects_of_type(&ld, "VideoObject") {
        if let Some(url) = object["embedUrl"]
            .as_str()
            .and_then(|src| page.url().join(src).ok())
        {
            links.push(url);
        }
    }
    for player in players {
        links.extend(player.embeds_in(page));
    }
    let mut seen = HashSet::new();
    links
        .into_iter()
        .filter(|u| u != page.url() && known_player(players, u) && seen.insert(u.clone()))
        .collect()
}

/// Everything a page says about its video, with the players among `players` it embeds.
pub fn extract(html: &str, base: &Url, players: &[Arc<dyn Resolver>]) -> PageMedia {
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
        embeds: embedded_players(&page, players),
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
            Kind::File(container, MediaKind::Video) => {
                let mut v = Variant::new(final_url, VariantKind::File);
                v.container = container;
                v.width = candidate.width;
                v.height = candidate.height;
                v.size = size;
                Some(vec![v])
            }
            // A page's video metadata that points at a picture or a sound is not its video.
            Kind::File(_, _) => None,
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
            Kind::File(container, kind) => {
                drop(response);
                let mut v = Variant::new(final_url.clone(), VariantKind::File);
                v.container = container;
                v.audio_only = kind == MediaKind::Audio;
                v.size = length;
                let mut resolved = Resolved::of("web", kind);
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
                if final_url.host_str() != url.host_str() {
                    drop(response);
                    return Err(ResolveError::Redirect(final_url));
                }
                let (body, _) = response.bytes_up_to(MAX_PAGE).await?;
                let html = String::from_utf8_lossy(&body).into_owned();
                let media = extract(&html, &final_url, &self.players);
                let mut variants = Vec::new();
                for candidate in &media.candidates {
                    if candidate.url != final_url && known_player(&self.players, &candidate.url) {
                        return Err(ResolveError::Redirect(candidate.url.clone()));
                    }
                    if let Some(found) = self.probe_candidate(candidate).await {
                        variants.extend(found);
                    }
                }
                if variants.is_empty() {
                    if let Some(embed) = media.embeds.first() {
                        return Err(ResolveError::Redirect(embed.clone()));
                    }
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
            // A file that is neither media nor a page: a document, an archive, a feed.
            Kind::Other => Err(ResolveError::Unsupported(url.clone())),
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
                "direct video files",
                "direct audio files",
                "direct image files",
                "hls",
                "dash",
                "smooth streaming",
                "open graph pages",
                "json-ld",
                "video tags",
                "embedded players",
            ],
            formats: &[
                "mp4", "webm", "mkv", "mov", "hls", "dash", "ism", "mp3", "m4a", "ogg", "flac",
                "wav", "jpg", "png", "webp", "gif",
            ],
            media: &[MediaKind::Video, MediaKind::Audio, MediaKind::Image],
            tags: &[Tag::Video, Tag::Images, Tag::Music],
            session: SessionSupport::Optional,
            examples: &["https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        matches!(url.scheme(), "http" | "https") && url.host_str().is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        if let Some(found) = self.attempt(BROWSER_UA, url).await? {
            return Ok(Resolution::from(found));
        }
        if let Some(found) = self.attempt(EMBED_BOT_UA, url).await? {
            return Ok(Resolution::from(found));
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
    use crate::resolve::{
        ResolverRegistry, brightcove, builtin_resolvers, bunny, cloudflare_stream, jwplayer,
        kaltura, mux, vidyard, wistia,
    };

    /// Every resolver of the crate, for the players a page may embed.
    fn players(http: &Http) -> Vec<Arc<dyn Resolver>> {
        builtin_resolvers(http)
    }

    fn web(http: Http) -> WebResolver {
        let players = players(&http);
        WebResolver::new(http, players)
    }

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
            Kind::File(Some(Container::Mp4), MediaKind::Video)
        );
        assert_eq!(
            classify(&url("https://h/song"), Some("audio/mpeg")),
            Kind::File(Some(Container::Mp3), MediaKind::Audio)
        );
        assert_eq!(
            classify(&url("https://h/a.flac"), Some("application/octet-stream")),
            Kind::File(Some(Container::Flac), MediaKind::Audio)
        );
        assert_eq!(
            classify(&url("https://h/pic"), Some("image/png")),
            Kind::File(Some(Container::Png), MediaKind::Image)
        );
        assert_eq!(
            classify(&url("https://h/pic.heic"), Some("image/heic")),
            Kind::File(None, MediaKind::Image)
        );
        assert_eq!(
            classify(&url("https://h/anim.gif"), Some("image/gif")),
            Kind::File(Some(Container::Gif), MediaKind::Video)
        );
        assert_eq!(
            classify(&url("https://h/logo.svg"), Some("image/svg+xml")),
            Kind::Other
        );
        assert_eq!(
            classify(&url("https://h/paper.pdf"), Some("application/pdf")),
            Kind::Other
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
    async fn embedded_players_resolve_through_the_registry_with_recorded_responses() {
        let cases = [
            (
                include_str!("jwplayer_fixture.json"),
                "jwplayer",
                r#"<script src="https://cdn.jwplayer.com/players/nPripu9l-ALJ3XQCI.js"></script>"#,
            ),
            (
                include_str!("brightcove_fixture.json"),
                "brightcove",
                r#"<video-js data-account="1752604059001" data-player="default" data-video-id="4457254747001"></video-js>"#,
            ),
            (
                include_str!("wistia_fixture.json"),
                "wistia",
                r#"<div class="wistia_embed wistia_async_cmst5825to"></div>"#,
            ),
            (
                include_str!("kaltura_fixture.json"),
                "kaltura",
                r#"<script>kWidget.embed({wid: '_243342', entry_id: '1_sf5ovm7u'});</script>"#,
            ),
            (
                include_str!("vidyard_fixture.json"),
                "vidyard",
                r#"<img class="vidyard-player-embed" data-uuid="oTDMPlUv--51Th455G5u7Q">"#,
            ),
            (
                include_str!("cloudflare_stream_fixture.json"),
                "cloudflare_stream",
                r#"<stream src="6b9e68b07dfee8cc2d116e4c51d6a957" customer-domain-prefix="customer-f33zs165nr7gyfy4"></stream>"#,
            ),
            (
                include_str!("mux_fixture.json"),
                "mux",
                r#"<mux-player playback-id="DS00Spx1CV902MCtPj5WknGlR102V5HFkDe" metadata-video-title="Demo"></mux-player>"#,
            ),
            (
                include_str!("bunny_fixture.json"),
                "bunny",
                r#"<iframe data-src="https://iframe.mediadelivery.net/embed/136145/32e34c4b-0d72-437c-9abb-05e67657da34"></iframe>"#,
            ),
        ];
        for (recording, platform, embed) in cases {
            let mut fixture = Fixture::parse(recording).unwrap();
            let url = Url::parse("https://site.test/article").unwrap();
            // An unrelated frame often appears before the article's actual video.
            let page =
                format!(r#"<html><iframe src="https://ads.test/frame"></iframe>{embed}</html>"#);
            fixture
                .exchanges
                .push(exchange(url.as_str(), "text/html", &page, 200, &[]));
            let http = Http::replay(fixture);
            let players: Vec<Arc<dyn Resolver>> = vec![
                Arc::new(jwplayer::JwplayerResolver::new(http.clone())),
                Arc::new(brightcove::BrightcoveResolver::new(http.clone())),
                Arc::new(wistia::WistiaResolver::new(http.clone())),
                Arc::new(kaltura::KalturaResolver::new(http.clone())),
                Arc::new(vidyard::VidyardResolver::new(http.clone())),
                Arc::new(cloudflare_stream::CloudflareStreamResolver::new(
                    http.clone(),
                )),
                Arc::new(mux::MuxResolver::new(http.clone())),
                Arc::new(bunny::BunnyResolver::new(http.clone())),
            ];
            let mut list = players.clone();
            list.push(Arc::new(WebResolver::new(http, players)));
            let registry = ResolverRegistry::new(list);
            let media = registry
                .resolve(&url)
                .await
                .unwrap_or_else(|e| panic!("{platform}: {e}"))
                .media()
                .expect("one video");
            assert_eq!(media.resolver, platform);
            assert!(!media.variants.is_empty(), "{platform}");
            if platform == "mux" {
                assert_eq!(media.title.as_deref(), Some("Demo"));
            }
        }
    }

    #[tokio::test]
    async fn social_player_embeds_take_precedence_over_unrelated_frames() {
        for target in [
            "https://www.youtube.com/embed/BaW_jenozKc",
            "https://www.youtube-nocookie.com/embed/BaW_jenozKc",
            "https://player.vimeo.com/video/76979871?h=abc123",
            "https://player.twitch.tv/?video=v40791111&parent=site.test",
            "https://player.twitch.tv/?channel=someone&parent=site.test",
            "https://clips.twitch.tv/embed?clip=FaintLightGullWholeWheat&parent=site.test",
            "https://streamable.com/e/moo",
        ] {
            let html = format!(
                r#"<iframe src="https://ads.test/frame"></iframe><iframe src="{target}"></iframe>"#
            );
            let mut fixture = Fixture::new("web", None);
            fixture.exchanges.push(exchange(
                "https://site.test/article",
                "text/html",
                &html,
                200,
                &[],
            ));
            let error = web(Http::replay(fixture))
                .resolve(&Url::parse("https://site.test/article").unwrap())
                .await
                .unwrap_err();
            assert!(
                matches!(error, ResolveError::Redirect(ref url) if url.as_str() == target),
                "{target}: {error}"
            );
        }
    }

    #[test]
    fn metadata_players_and_brightcove_playlists_are_detected() {
        let url = Url::parse("https://site.test/article").unwrap();
        let target = "https://www.youtube.com/embed/BaW_jenozKc";
        let players = players(&Http::replay(Fixture::new("web", None)));
        for html in [
            format!(r#"<meta name="twitter:player" content="{target}">"#),
            format!(
                r#"<script type="application/ld+json">{{"@type":"VideoObject","embedUrl":"{target}"}}</script>"#
            ),
        ] {
            assert_eq!(
                extract(&html, &url, &players).embeds,
                vec![Url::parse(target).unwrap()]
            );
        }
        let html =
            r#"<video data-account="1752604059001" data-playlist-id="5743160747001"></video>"#;
        let embeds = extract(html, &url, &players).embeds;
        assert_eq!(embeds.len(), 1);
        assert!(matches!(
            brightcove::parse_link(&embeds[0]).unwrap().content,
            brightcove::Content::Playlist(_)
        ));
    }

    #[tokio::test]
    async fn a_short_link_to_another_host_is_handed_back_unwrapped() {
        let mut fixture = Fixture::new("web", None);
        fixture.exchanges.push(exchange(
            "https://t.co/abc",
            "text/html",
            "",
            301,
            &[("location", "https://x.com/someone/status/123")],
        ));
        fixture.exchanges.push(exchange(
            "https://x.com/someone/status/123",
            "text/html; charset=utf-8",
            "<html><body>a post</body></html>",
            200,
            &[],
        ));
        let resolver = web(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://t.co/abc").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Redirect(u) if u.as_str() == "https://x.com/someone/status/123"),
            "{error}"
        );
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
        let resolver = web(Http::replay(fixture));
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
        let resolver = web(Http::replay(fixture));
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
        let resolver = web(Http::replay(fixture));
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

    #[tokio::test]
    async fn direct_audio_and_image_files_are_what_they_are_and_other_files_are_refused() {
        let mut fixture = Fixture::new("web", None);
        fixture.exchanges.push(exchange(
            "https://cdn.test/Track%2001.mp3",
            "audio/mpeg",
            "",
            200,
            &[("content-length", "3000")],
        ));
        fixture.exchanges.push(exchange(
            "https://cdn.test/photo.jpg",
            "image/jpeg",
            "",
            200,
            &[("content-length", "800")],
        ));
        fixture.exchanges.push(exchange(
            "https://cdn.test/paper.pdf",
            "application/pdf",
            "",
            200,
            &[("content-length", "9")],
        ));
        let resolver = web(Http::replay(fixture));
        let track = resolver
            .resolve(&Url::parse("https://cdn.test/Track%2001.mp3").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(track.media, MediaKind::Audio);
        assert_eq!(track.title.as_deref(), Some("Track 01"));
        assert_eq!(track.variants[0].container, Some(Container::Mp3));
        assert!(track.variants[0].audio_only);
        assert_eq!(track.variants[0].size, Some(3000));
        let photo = resolver
            .resolve(&Url::parse("https://cdn.test/photo.jpg").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(photo.media, MediaKind::Image);
        assert_eq!(photo.variants[0].container, Some(Container::Jpeg));
        assert_eq!(photo.variants[0].kind, VariantKind::File);
        assert_eq!(photo.variants[0].size, Some(800));
        let error = resolver
            .resolve(&Url::parse("https://cdn.test/paper.pdf").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unsupported(u) if u.as_str() == "https://cdn.test/paper.pdf"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_page_whose_video_metadata_names_a_picture_has_no_video() {
        let page = r#"<html><head><meta property="og:video" content="https://cdn.test/poster.jpg"></head></html>"#;
        let mut fixture = Fixture::new("web", None);
        fixture.exchanges.push(exchange(
            "https://site.test/watch",
            "text/html",
            page,
            200,
            &[],
        ));
        fixture.exchanges.push(exchange(
            "https://cdn.test/poster.jpg",
            "image/jpeg",
            "",
            206,
            &[("content-range", "bytes 0-0/500")],
        ));
        fixture.exchanges.push(exchange(
            "https://site.test/watch",
            "text/html",
            page,
            200,
            &[],
        ));
        fixture.exchanges.push(exchange(
            "https://cdn.test/poster.jpg",
            "image/jpeg",
            "",
            206,
            &[("content-range", "bytes 0-0/500")],
        ));
        let resolver = web(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://site.test/watch").unwrap())
            .await
            .unwrap_err();
        assert!(matches!(error, ResolveError::NotFound(_)), "{error}");
    }
}
