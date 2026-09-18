//! Dagbladet TV (dagbladet.no/video): every video is a JW Player media, named by the
//! eight-character id its link ends with, read from JW Player's delivery API for its
//! title, poster, time and every rendition. Older links name a YouTube video instead.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use url::Url;

use super::{Platform, Resolution, ResolveError, Resolver, SessionSupport, jwplayer};
use crate::http::Http;

pub const PLATFORM: &str = "dbtv";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Media {
    /// A JW Player media id.
    Jwplayer(String),
    /// A YouTube video id, as older links carry.
    Youtube(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub media: Media,
    /// The title slug a video page's link carries before the id.
    pub display_id: Option<String>,
}

/// `/video/{slug}/{id}` or `/video/{id}`, the id being JW Player's eight characters or
/// YouTube's eleven.
static RE_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^/video/(?:([^/]+)/)?([a-zA-Z0-9]{8}|[a-zA-Z0-9_-]{11})/?$").unwrap()
});

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    if host != "dagbladet.no" {
        return None;
    }
    let caps = RE_PATH.captures(url.path())?;
    let id = caps[2].to_string();
    Some(Link {
        media: if id.len() == 8 {
            Media::Jwplayer(id)
        } else {
            Media::Youtube(id)
        },
        display_id: caps.get(1).map(|m| m.as_str().to_string()),
    })
}

pub struct DbtvResolver {
    http: Http,
}

impl DbtvResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for DbtvResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Dagbladet TV",
            hosts: &["dagbladet.no"],
            features: &["videos", "youtube links"],
            formats: &["hls", "mp4"],
            session: SessionSupport::None,
            examples: &["https://www.dagbladet.no/video/ranet-bank-med-chilipulver/J12GzewM"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        match link.media {
            Media::Youtube(id) => Err(ResolveError::Redirect(
                Url::parse(&format!("https://www.youtube.com/watch?v={id}")).expect("valid"),
            )),
            Media::Jwplayer(id) => {
                let mut resolved = jwplayer::media(&self.http, PLATFORM, &id, url).await?;
                resolved.id = Some(id);
                resolved.uploader = Some("Dagbladet".into());
                resolved.uploader_url =
                    Some(Url::parse("https://www.dagbladet.no/video/").expect("valid"));
                resolved.webpage_url = Some(url.clone());
                Ok(Resolution::from(resolved))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::VariantKind;
    use super::*;
    use crate::http::Fixture;

    const VIDEO: &str = "https://www.dagbladet.no/video/ranet-bank-med-chilipulver/J12GzewM";

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link(VIDEO),
            Some(Link {
                media: Media::Jwplayer("J12GzewM".into()),
                display_id: Some("ranet-bank-med-chilipulver".into())
            })
        );
        assert_eq!(
            link("https://dagbladet.no/video/J12GzewM/"),
            Some(Link {
                media: Media::Jwplayer("J12GzewM".into()),
                display_id: None
            })
        );
        assert_eq!(
            link("http://www.dagbladet.no/video/truer-iran-bor-passe-dere/PalfB2Cw?autoplay=false"),
            Some(Link {
                media: Media::Jwplayer("PalfB2Cw".into()),
                display_id: Some("truer-iran-bor-passe-dere".into())
            })
        );
        assert_eq!(link("https://www.dagbladet.no/video/"), None);
        assert_eq!(
            link("https://www.dagbladet.no/video/category/underholdning"),
            None
        );
        assert_eq!(
            link("https://www.dagbladet.no/video/PynxJnNWChE/"),
            Some(Link {
                media: Media::Youtube("PynxJnNWChE".into()),
                display_id: None
            })
        );
        assert_eq!(link("https://www.dagbladet.no/nyheter/83325693"), None);
        assert_eq!(link("https://www.vg.no/video/J12GzewM/"), None);
        assert_eq!(link("ftp://www.dagbladet.no/video/J12GzewM"), None);
    }

    #[tokio::test]
    async fn videos_resolve_through_jwplayers_delivery_api() {
        let fixture = Fixture::parse(include_str!("dbtv_fixture.json")).unwrap();
        let resolver = DbtvResolver::new(Http::replay(fixture));
        let url = Url::parse(VIDEO).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.resolver, PLATFORM);
        assert_eq!(resolved.id.as_deref(), Some("J12GzewM"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Ranet bank med chilipulver")
        );
        assert_eq!(resolved.webpage_url.as_ref(), Some(&url));
        assert!(resolved.duration.is_some());
        assert!(
            resolved.variants.len() > 1,
            "the HLS manifest expands into its streams"
        );
        assert!(
            resolved
                .variants
                .iter()
                .all(|v| v.kind == VariantKind::Hls && v.height.is_some())
        );
        assert!(
            resolved
                .variants
                .iter()
                .any(|v| v.height.is_some_and(|h| h >= 1080))
        );
        let youtube = Url::parse("https://www.dagbladet.no/video/PynxJnNWChE/").unwrap();
        assert!(matches!(
            resolver.resolve(&youtube).await.unwrap_err(),
            ResolveError::Redirect(to) if to.as_str() == "https://www.youtube.com/watch?v=PynxJnNWChE"
        ));
        let missing = Url::parse("https://www.dagbladet.no/video/zzzzzzzz").unwrap();
        assert!(matches!(
            resolver.resolve(&missing).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
