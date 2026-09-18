//! Bundesliga (bundesliga.com) videos: the `vid` a video page names is a JW Player media
//! id, read from JW Player's delivery API for its title, poster, time and every rendition.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use url::Url;

use super::{Platform, Resolution, ResolveError, Resolver, SessionSupport, jwplayer};
use crate::http::Http;

pub const PLATFORM: &str = "bundesliga";

/// `/{lang}/bundesliga/videos` with any further path.
static RE_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/[a-z]{2}/bundesliga/videos(?:/[^?]+)?$").unwrap());
static RE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-zA-Z0-9]{8}$").unwrap());

/// The JW Player media id a video link's `vid` names.
pub fn video_id(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    if host != "bundesliga.com" || !RE_PATH.is_match(url.path()) {
        return None;
    }
    url.query_pairs()
        .find(|(k, _)| k == "vid")
        .map(|(_, v)| v.into_owned())
        .filter(|v| RE_ID.is_match(v))
}

pub struct BundesligaResolver {
    http: Http,
}

impl BundesligaResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for BundesligaResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Bundesliga",
            hosts: &["bundesliga.com"],
            features: &["videos"],
            formats: &["mp4", "hls", "dash"],
            session: SessionSupport::None,
            examples: &["https://www.bundesliga.com/en/bundesliga/videos?vid=bhhHkKyN"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        video_id(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let id = video_id(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let mut resolved = jwplayer::media(&self.http, PLATFORM, &id, url).await?;
        resolved.id = Some(id);
        resolved.uploader = Some("Bundesliga".into());
        resolved.uploader_url = Some(Url::parse("https://www.bundesliga.com/").expect("valid"));
        resolved.webpage_url = Some(url.clone());
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use super::super::VariantKind;
    use super::*;
    use crate::http::Fixture;

    #[test]
    fn links_are_read() {
        let id = |s: &str| video_id(&Url::parse(s).unwrap());
        assert_eq!(
            id("https://www.bundesliga.com/en/bundesliga/videos?vid=bhhHkKyN"),
            Some("bhhHkKyN".into())
        );
        assert_eq!(
            id(
                "https://www.bundesliga.com/en/bundesliga/videos/latest-features/T8IKc8TX?vid=ROHjs06G"
            ),
            Some("ROHjs06G".into())
        );
        assert_eq!(
            id("https://www.bundesliga.com/en/bundesliga/videos/goals?vid=mOG56vWA"),
            Some("mOG56vWA".into())
        );
        assert_eq!(
            id("https://www.bundesliga.com/en/bundesliga/videos?vid=short"),
            None
        );
        assert_eq!(
            id("https://www.bundesliga.com/en/bundesliga/news?vid=bhhHkKyN"),
            None
        );
        assert_eq!(id("https://www.bundesliga.com/en/bundesliga/videos"), None);
    }

    #[tokio::test]
    async fn videos_resolve_through_jwplayers_delivery_api() {
        let fixture = Fixture::parse(include_str!("bundesliga_fixture.json")).unwrap();
        let resolver = BundesligaResolver::new(Http::replay(fixture));
        let url =
            Url::parse("https://www.bundesliga.com/en/bundesliga/videos?vid=bhhHkKyN").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.resolver, PLATFORM);
        assert_eq!(resolved.id.as_deref(), Some("bhhHkKyN"));
        assert_eq!(resolved.webpage_url.as_ref(), Some(&url));
        assert_eq!(resolved.uploader.as_deref(), Some("Bundesliga"));
        assert!(resolved.title.is_some());
        assert!(resolved.duration.is_some());
        assert!(resolved.thumbnail.is_some());
        assert!(
            resolved
                .variants
                .iter()
                .any(|v| v.kind == VariantKind::File)
        );
        assert!(resolved.variants.iter().any(|v| v.kind == VariantKind::Hls));
        assert!(
            resolved
                .variants
                .iter()
                .all(|v| v.kind != VariantKind::Hls || v.height.is_some())
        );
        assert!(resolved.variants.iter().any(|v| v.height == Some(1080)));
        let missing =
            Url::parse("https://www.bundesliga.com/en/bundesliga/videos?vid=zzzzzzzz").unwrap();
        assert!(matches!(
            resolver.resolve(&missing).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
