//! AlloCiné (allocine.fr) videos, articles and film pages: the page's player model names
//! the video, which the site hosts on Dailymotion, and the Dailymotion resolver reads it
//! from there.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolver, SessionSupport, Tag, fetch,
    navigation_headers, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::MediaKind;

pub const PLATFORM: &str = "allocine";

/// `/article/fichearticle_gen_carticle={id}.html`, `/video/player_gen_cmedia={id}…`,
/// `/film/fichefilm_gen_cfilm={id}.html` or `/video/video-{id}/`.
static RE_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^/(?:article|video|film)/(?:fichearticle_gen_carticle=|player_gen_cmedia=|fichefilm_gen_cfilm=|video-)(\d+)(?:\.html)?",
    )
    .unwrap()
});
static RE_MODEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"data-model="([^"]+)""#).unwrap());

pub fn video_id(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "allocine.fr" && host != "www.allocine.fr" {
        return None;
    }
    RE_PATH.captures(url.path()).map(|caps| caps[1].to_string())
}

/// The player model a page carries: `data-model="{…}"`, HTML-escaped.
pub fn player_model(html: &str) -> Option<Value> {
    let escaped = util::search(&RE_MODEL, html)?;
    serde_json::from_str(&util::html_unescape(&escaped)).ok()
}

pub struct AllocineResolver {
    http: Http,
}

impl AllocineResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for AllocineResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "AlloCiné",
            hosts: &["allocine.fr"],
            features: &["videos", "articles", "films"],
            formats: &["mp4", "hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::News, Tag::Video],
            session: SessionSupport::None,
            examples: &[
                "https://www.allocine.fr/video/video-19550147/",
                "https://www.allocine.fr/video/player_gen_cmedia=19540403&cfilm=222257.html",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        video_id(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        if video_id(url).is_none() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let fetched = fetch(
            &self.http,
            url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        // The player model names the Dailymotion video the page plays, which the
        // Dailymotion resolver reads.
        let model =
            player_model(&fetched.text()).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let dailymotion = model["videos"]
            .as_array()
            .and_then(|list| list.first())
            .and_then(|video| util::text(&video["idDailymotion"]))
            .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        Err(ResolveError::Redirect(
            Url::parse(&format!("https://www.dailymotion.com/video/{dailymotion}"))
                .map_err(|e| ResolveError::malformed(url, e.to_string()))?,
        ))
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

    fn model_page(model: &Value) -> String {
        let escaped = model
            .to_string()
            .replace('&', "&amp;")
            .replace('"', "&quot;");
        format!(
            r#"<html><head><title>Les gaffes - AlloCiné</title><meta property="og:description" content="Desc"><meta property="og:image" content="https://fr.web.img6.acsta.net/x.jpg"></head><body><div data-model="{escaped}"></div></body></html>"#
        )
    }

    #[test]
    fn links_are_read() {
        let id = |s: &str| video_id(&Url::parse(s).unwrap());
        assert_eq!(
            id("http://www.allocine.fr/article/fichearticle_gen_carticle=18635087.html"),
            Some("18635087".into())
        );
        assert_eq!(
            id("http://www.allocine.fr/video/player_gen_cmedia=19540403&cfilm=222257.html"),
            Some("19540403".into())
        );
        assert_eq!(
            id("http://www.allocine.fr/video/video-19550147/"),
            Some("19550147".into())
        );
        assert_eq!(
            id("https://www.allocine.fr/film/fichefilm_gen_cfilm=222257.html"),
            Some("222257".into())
        );
        assert_eq!(id("https://www.allocine.fr/"), None);
        assert_eq!(id("https://example.com/video/video-19550147/"), None);
    }

    #[tokio::test]
    async fn videos_hand_off_to_dailymotion() {
        let model = json!({"videos": [{"id": 19550147, "idDailymotion": "x8a3u4k", "title": "Les gaffes de Cliffhanger", "duration": 346, "sources": null}]});
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.allocine.fr/video/video-19550147/",
            200,
            "text/html",
            model_page(&model),
        ));
        let trailer = json!({"videos": [{"id": 19540403, "idDailymotion": "x2c9pjn", "title": "Bande-annonce", "duration": 120,
            "added_at": {"date": "2014-12-11 21:38:00.000000", "timezone": "Europe/Paris"}, "sources": null}]});
        fixture.exchanges.push(get(
            "https://www.allocine.fr/video/player_gen_cmedia=19540403&cfilm=222257.html",
            200,
            "text/html",
            model_page(&trailer),
        ));
        let resolver = AllocineResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.allocine.fr/video/video-19550147/").unwrap();
        assert!(resolver.matches(&url));
        let error = resolver.resolve(&url).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Redirect(u) if u.as_str() == "https://www.dailymotion.com/video/x8a3u4k"),
            "{error}"
        );
        let error = resolver
            .resolve(
                &Url::parse(
                    "https://www.allocine.fr/video/player_gen_cmedia=19540403&cfilm=222257.html",
                )
                .unwrap(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Redirect(u) if u.as_str() == "https://www.dailymotion.com/video/x2c9pjn"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn pages_without_a_dailymotion_video_are_not_found() {
        let model = json!({"videos": [{"id": 19540404, "idDailymotion": null, "title": "Sans vidéo", "duration": 120, "sources": null}]});
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.allocine.fr/video/video-19540404/",
            200,
            "text/html",
            model_page(&model),
        ));
        fixture.exchanges.push(get(
            "https://www.allocine.fr/article/fichearticle_gen_carticle=18635087.html",
            200,
            "text/html",
            r#"<html><head><title>Astérix - AlloCiné</title></head><body>no model</body></html>"#
                .into(),
        ));
        let resolver = AllocineResolver::new(Http::replay(fixture));
        for link in [
            "https://www.allocine.fr/video/video-19540404/",
            "https://www.allocine.fr/article/fichearticle_gen_carticle=18635087.html",
        ] {
            let url = Url::parse(link).unwrap();
            assert!(matches!(
                resolver.resolve(&url).await.unwrap_err(),
                ResolveError::NotFound(u) if u == url
            ));
        }
    }
}
