//! Resolve Cloudflare Stream HLS, DASH and optional MP4 downloads. Accept video IDs or
//! signed playback tokens from supported player URLs.
//!
//! Preserve customer subdomains when constructing manifest URLs.

use std::sync::LazyLock;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use regex::Regex;
use scraper::Selector;
use serde_json::Value;
use url::Url;

use super::page::Page;
use super::{
    Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Tag, Variant,
    VariantKind, hls, probe_file,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "cloudflare_stream";
const DOMAINS: [&str; 3] = [
    "cloudflarestream.com",
    "videodelivery.net",
    "bytehighway.net",
];

static RE_UID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{32}$").unwrap());
static RE_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^eyJ[\w-]+\.[\w-]+\.[\w-]+$").unwrap());

/// A video and the host its manifests are read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub host: String,
    /// The video id, or the signed token that stands for it.
    pub id: String,
}

fn is_id(text: &str) -> bool {
    RE_UID.is_match(text) || RE_TOKEN.is_match(text)
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let domain = DOMAINS
        .iter()
        .find(|domain| host == **domain || host.ends_with(&format!(".{domain}")))?;
    let id = url
        .query_pairs()
        .find(|(k, _)| k == "video")
        .map(|(_, v)| v.into_owned())
        .filter(|v| is_id(v))
        .or_else(|| {
            url.path_segments()?
                .find(|s| !s.is_empty())
                .filter(|s| is_id(s))
                .map(String::from)
        })?;
    // Every domain but bytehighway.net serves the manifests from cloudflarestream.com.
    let host = if host.starts_with("customer-") {
        host.replace("videodelivery.net", "cloudflarestream.com")
    } else if *domain == "bytehighway.net" {
        domain.to_string()
    } else {
        "cloudflarestream.com".to_string()
    };
    Some(Link { host, id })
}

impl Link {
    /// A resource of the video on its manifest host.
    pub fn resource(&self, path: &str) -> Url {
        Url::parse(&format!("https://{}/{}/{path}", self.host, self.id)).expect("valid")
    }

    /// The page that plays the video.
    pub fn page(&self) -> Url {
        if self.host == "cloudflarestream.com" {
            Url::parse(&format!("https://watch.cloudflarestream.com/{}", self.id)).expect("valid")
        } else {
            self.resource("iframe")
        }
    }
}

/// The video id: the id itself, or the subject a signed token carries.
pub fn video_id(id: &str) -> String {
    let Some(payload) = id.split('.').nth(1) else {
        return id.to_string();
    };
    URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|claims| claims["sub"].as_str().map(String::from))
        .unwrap_or_else(|| id.to_string())
}

fn selector(text: &str) -> Selector {
    Selector::parse(text).expect("selectors in this module are valid")
}

/// Players a page embeds through the `<stream>` element or the embed script, as links
/// this resolver takes.
pub fn embeds_in(page: &Page) -> Vec<Url> {
    let mut found: Vec<Url> = Vec::new();
    let mut push = |url: Url| {
        if !found.contains(&url) {
            found.push(url);
        }
    };
    for element in page.document().select(&selector("stream[src]")) {
        let Some(id) = element
            .value()
            .attr("src")
            .map(str::trim)
            .filter(|id| is_id(id))
        else {
            continue;
        };
        let host = element
            .value()
            .attr("customer-domain-prefix")
            .map(str::trim)
            .filter(|prefix| !prefix.is_empty())
            .map(|prefix| format!("{prefix}.cloudflarestream.com"))
            .unwrap_or_else(|| "cloudflarestream.com".to_string());
        push(
            Link {
                host,
                id: id.to_string(),
            }
            .page(),
        );
    }
    for element in page.document().select(&selector("script[src]")) {
        let Some(src) = element.value().attr("src") else {
            continue;
        };
        let full = if src.starts_with("//") {
            format!("https:{src}")
        } else {
            src.to_string()
        };
        if let Some(link) = Url::parse(&full).ok().as_ref().and_then(parse_link) {
            push(link.page());
        }
    }
    found
}

pub struct CloudflareStreamResolver {
    http: Http,
}

impl CloudflareStreamResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for CloudflareStreamResolver {
    fn embeds_in(&self, page: &Page) -> Vec<Url> {
        embeds_in(page)
    }

    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Cloudflare Stream",
            hosts: &[
                "cloudflarestream.com",
                "videodelivery.net",
                "bytehighway.net",
            ],
            features: &[
                "videos",
                "watch pages",
                "player embeds",
                "signed tokens",
                "live",
                "downloads",
            ],
            formats: &["hls", "dash", "mp4"],
            media: &[MediaKind::Video],
            tags: &[Tag::Players],
            session: SessionSupport::None,
            examples: &[
                "https://customer-f33zs165nr7gyfy4.cloudflarestream.com/6b9e68b07dfee8cc2d116e4c51d6a957/iframe",
                "https://watch.cloudflarestream.com/6b9e68b07dfee8cc2d116e4c51d6a957",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        // Either manifest may be withheld while the other serves.
        let master = link.resource("manifest/video.m3u8");
        let (mut variants, mut subtitles, mut duration, mut live, mut failure) =
            match hls::expand(&self.http, &master, PLATFORM, BROWSER_UA, &[]).await {
                Ok(expanded) => (
                    expanded.variants,
                    expanded.subtitles,
                    expanded.duration,
                    expanded.live,
                    None,
                ),
                Err(error) => (Vec::new(), Vec::new(), None, false, Some(error.at(url))),
            };
        match super::dash::expand(
            &self.http,
            &link.resource("manifest/video.mpd"),
            PLATFORM,
            BROWSER_UA,
            &[],
        )
        .await
        {
            Ok(expanded) => {
                duration = duration.or(expanded.duration);
                live |= expanded.live;
                for mut representation in expanded.variants {
                    representation.format_id = Some(match &representation.label {
                        Some(label) => format!("dash-{label}"),
                        None => "dash".to_string(),
                    });
                    variants.push(representation);
                }
                for track in expanded.subtitles {
                    if !subtitles.iter().any(|t| t.url == track.url) {
                        subtitles.push(track);
                    }
                }
            }
            Err(error) => {
                if variants.is_empty() {
                    failure = Some(error.at(url));
                } else {
                    tracing::debug!(%url, "Cloudflare Stream DASH manifest not read: {error}");
                }
            }
        }
        if variants.is_empty()
            && let Some(error) = failure
        {
            return Err(error);
        }
        let expanded = hls::Expanded {
            variants: Vec::new(),
            subtitles,
            duration,
            live,
        };
        let download = link.resource("downloads/default.mp4");
        let probed = probe_file(&self.http, &download, PLATFORM, BROWSER_UA, &[]).await?;
        if probed.status.is_success() {
            let mut v = Variant::new(download, VariantKind::File);
            v.container = Some(Container::Mp4);
            v.video = Some(VideoCodec::H264);
            v.audio = Some(AudioCodec::Aac);
            v.size = probed.size;
            v.duration = expanded.duration;
            v.label = Some("download".into());
            v.format_id = Some("download".into());
            variants.push(v);
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(video_id(&link.id));
        resolved.title = Some(video_id(&link.id));
        resolved.duration = expanded.duration;
        resolved.live = expanded.live;
        resolved.thumbnail = Some(link.resource("thumbnails/thumbnail.jpg"));
        resolved.webpage_url = Some(link.page());
        resolved.subtitles = expanded.subtitles;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::Fixture;

    fn fixture() -> Fixture {
        Fixture::parse(include_str!("cloudflare_stream_fixture.json")).unwrap()
    }

    const UID: &str = "6b9e68b07dfee8cc2d116e4c51d6a957";

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let on = |host: &str, id: &str| {
            Some(Link {
                host: host.into(),
                id: id.into(),
            })
        };
        assert_eq!(
            link(
                "https://customer-f33zs165nr7gyfy4.cloudflarestream.com/6b9e68b07dfee8cc2d116e4c51d6a957/iframe"
            ),
            on("customer-f33zs165nr7gyfy4.cloudflarestream.com", UID)
        );
        assert_eq!(
            link("https://watch.cloudflarestream.com/9df17203414fd1db3e3ed74abbe936c1"),
            on("cloudflarestream.com", "9df17203414fd1db3e3ed74abbe936c1")
        );
        assert_eq!(
            link(
                "https://cloudflarestream.com/31c9291ab41fac05471db4e73aa11717/manifest/video.mpd"
            ),
            on("cloudflarestream.com", "31c9291ab41fac05471db4e73aa11717")
        );
        assert_eq!(
            link(
                "https://embed.cloudflarestream.com/embed/we4g.fla9.latest.js?video=31c9291ab41fac05471db4e73aa11717"
            ),
            on("cloudflarestream.com", "31c9291ab41fac05471db4e73aa11717")
        );
        assert_eq!(
            link("https://iframe.videodelivery.net/81d80727f3022488598f68d323c1ad5e"),
            on("cloudflarestream.com", "81d80727f3022488598f68d323c1ad5e")
        );
        assert_eq!(
            link(
                "https://embed.videodelivery.net/embed/r4xu.fla9.latest.js?video=81d80727f3022488598f68d323c1ad5e"
            ),
            on("cloudflarestream.com", "81d80727f3022488598f68d323c1ad5e")
        );
        let token = "eyJhbGciOiJSUzI1NiIsImtpZCI6ImsxIn0.eyJzdWIiOiI2YjllNjhiMDdkZmVlOGNjMmQxMTZlNGM1MWQ2YTk1NyIsImtpZCI6ImsxIn0.c2lnbmF0dXJl";
        assert_eq!(
            link(&format!("https://watch.cloudflarestream.com/{token}")),
            on("cloudflarestream.com", token)
        );
        assert_eq!(video_id(token), UID);
        assert_eq!(video_id(UID), UID);
        assert_eq!(link("https://cloudflarestream.com/"), None);
        assert_eq!(
            link("https://www.cloudflare.com/products/cloudflare-stream/"),
            None
        );
        assert_eq!(
            link("https://customer-x.cloudflarestream.com/not-a-video/iframe"),
            None
        );
    }

    #[test]
    fn embedded_players_are_found_in_pages() {
        let html = r#"<html><body>
            <stream src="6b9e68b07dfee8cc2d116e4c51d6a957" customer-domain-prefix="customer-f33zs165nr7gyfy4" controls></stream>
            <stream src="9df17203414fd1db3e3ed74abbe936c1" controls></stream>
            <script data-cfasync="false" defer src="https://embed.cloudflarestream.com/embed/r4xu.fla9.latest.js?video=31c9291ab41fac05471db4e73aa11717"></script>
            </body></html>"#;
        let page = Page::parse(html, &Url::parse("https://site.test/page").unwrap());
        let found: Vec<String> = embeds_in(&page).iter().map(|u| u.to_string()).collect();
        assert_eq!(
            found,
            vec![
                "https://customer-f33zs165nr7gyfy4.cloudflarestream.com/6b9e68b07dfee8cc2d116e4c51d6a957/iframe",
                "https://watch.cloudflarestream.com/9df17203414fd1db3e3ed74abbe936c1",
                "https://watch.cloudflarestream.com/31c9291ab41fac05471db4e73aa11717",
            ]
        );
    }

    #[tokio::test]
    async fn videos_resolve_from_their_manifests() {
        let resolver = CloudflareStreamResolver::new(Http::replay(fixture()));
        let resolved = resolver
            .resolve(
                &Url::parse(&format!(
                    "https://customer-f33zs165nr7gyfy4.cloudflarestream.com/{UID}/iframe"
                ))
                .unwrap(),
            )
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some(UID));
        assert!(
            resolved.duration.unwrap().as_secs_f64() > 25.0,
            "{:?}",
            resolved.duration
        );
        assert!(!resolved.live);
        assert_eq!(
            resolved.thumbnail.as_ref().map(|u| u.as_str()),
            Some(
                "https://customer-f33zs165nr7gyfy4.cloudflarestream.com/6b9e68b07dfee8cc2d116e4c51d6a957/thumbnails/thumbnail.jpg"
            )
        );
        assert_eq!(
            resolved.variants.len(),
            10,
            "{:?}",
            resolved
                .variants
                .iter()
                .map(|v| v.url.as_str())
                .collect::<Vec<_>>()
        );
        let best = resolved
            .variants
            .iter()
            .find(|v| v.height == Some(1080))
            .unwrap();
        assert_eq!(best.kind, VariantKind::Hls);
        assert_eq!(best.video, Some(VideoCodec::H264));
        assert_eq!(best.width, Some(1920));
        let dash = resolved
            .variants
            .iter()
            .find(|v| v.kind == VariantKind::Dash)
            .unwrap();
        assert!(dash.url.as_str().ends_with("/manifest/video.mpd"));
        assert!(
            resolved
                .variants
                .iter()
                .all(|v| v.kind != VariantKind::File)
        );
        let watched = resolver
            .resolve(&Url::parse(&format!("https://watch.cloudflarestream.com/{UID}")).unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert!(
            watched
                .variants
                .iter()
                .all(|v| v.url.as_str().starts_with("https://cloudflarestream.com/")),
            "{:?}",
            watched
                .variants
                .iter()
                .map(|v| v.url.as_str())
                .collect::<Vec<_>>()
        );
        assert_eq!(
            watched.webpage_url.as_ref().map(|u| u.as_str()),
            Some("https://watch.cloudflarestream.com/6b9e68b07dfee8cc2d116e4c51d6a957")
        );
        let error = resolver
            .resolve(
                &Url::parse("https://watch.cloudflarestream.com/00000000000000000000000000000000")
                    .unwrap(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ResolveError::NotFound(_)), "{error}");
    }
}
