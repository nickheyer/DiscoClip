//! AlloCiné (allocine.fr) videos, articles and film pages: the page's player model names
//! the video, which the site hosts on Dailymotion today and served as its own files
//! before. The older media links answer the AcVisiondata service.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Tag,
    Variant, clean_title, fetch, navigation_headers, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "allocine";
const MEDIA_API: &str = "https://www.allocine.fr/ws/AcVisiondataV5.ashx?media=";

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

/// The order the site's own renditions rank in.
fn quality_rank(format_id: &str) -> u8 {
    match format_id {
        "hd" => 3,
        "md" => 2,
        "ld" => 1,
        _ => 0,
    }
}

/// A file the site serves itself, named `{id}_{quality}.mp4`.
fn own_file(url: Url, format_id: &str) -> Variant {
    let mut variant = Variant::file(url);
    variant.container = Some(Container::Mp4);
    variant.video = Some(VideoCodec::H264);
    variant.audio = Some(AudioCodec::Aac);
    variant.format_id = Some(format_id.to_string());
    variant.label = Some(format_id.to_string());
    variant
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
        let display_id = video_id(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
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
        let html = fetched.text();
        // The page's own metadata, read before anything is awaited: its titles and, in
        // its structured data, the file the site serves itself.
        let (page_title, description, thumbnail, own_media) = {
            let page = Page::parse(&html, url);
            let own_media = page
                .ld_json()
                .iter()
                .find(|ld| ld["@type"].as_str() == Some("VideoObject"))
                .and_then(|ld| ld["contentUrl"].as_str())
                .map(util::html_unescape)
                .and_then(|link| util::join_url(Some(url), &link));
            (
                page.title()
                    .map(|t| t.trim_end_matches(" - AlloCiné").to_string())
                    .and_then(|t| clean_title(&t)),
                page.meta("og:description").and_then(|d| clean_title(&d)),
                page.meta("og:image")
                    .and_then(|t| util::join_url(Some(url), &t)),
                own_media,
            )
        };
        let mut resolved = Resolved::new(PLATFORM);
        resolved.description = description;
        resolved.thumbnail = thumbnail;
        resolved.webpage_url = Some(url.clone());

        if let Some(model) = player_model(&html)
            && let Some(video) = model["videos"].as_array().and_then(|list| list.first())
        {
            // The site's own files: the `sources` it once listed by quality, or the one
            // file its structured data names now. A video it only hosts on Dailymotion is
            // handed on.
            let mut variants: Vec<Variant> = video["sources"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(_, source)| {
                    let link = util::url_of(source, Some(url))?;
                    let name = link.path_segments()?.next_back()?.to_string();
                    let format_id = name.split('_').nth(1)?.trim_end_matches(".mp4").to_string();
                    Some(own_file(link, &format_id))
                })
                .collect();
            if variants.is_empty()
                && let Some(link) = own_media.clone()
            {
                let format_id = link
                    .path_segments()
                    .and_then(|mut segments| segments.next_back())
                    .and_then(|name| name.split('_').nth(1))
                    .unwrap_or("hd")
                    .to_string();
                variants.push(own_file(link, &format_id));
            }
            if variants.is_empty() {
                if let Some(dailymotion) = util::text(&video["idDailymotion"]) {
                    return Err(ResolveError::Redirect(
                        Url::parse(&format!("https://www.dailymotion.com/video/{dailymotion}"))
                            .map_err(|e| ResolveError::malformed(url, e.to_string()))?,
                    ));
                }
                return Err(ResolveError::NotFound(url.clone()));
            }
            variants.sort_by_key(|v| {
                std::cmp::Reverse(quality_rank(v.format_id.as_deref().unwrap_or("")))
            });
            resolved.id = util::text(&video["id"]).or_else(|| Some(display_id.clone()));
            resolved.title = video["title"].as_str().and_then(clean_title);
            resolved.duration = util::seconds(&video["duration"]);
            resolved.uploaded_at = video["added_at"]["date"]
                .as_str()
                .and_then(|date| util::parse_timestamp(&date.replacen(' ', "T", 1)));
            resolved.variants = variants;
            return Ok(Resolution::from(resolved));
        }

        // Pages without a player model name a media the AcVisiondata service describes.
        let api = Url::parse(&format!("{MEDIA_API}{display_id}")).expect("valid");
        let accept = [("accept".to_string(), "application/json".to_string())];
        let media = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &accept, MAX_PAGE).await?;
        if let Some(error) = status_error(media.status, url) {
            return Err(error);
        }
        let media = media.json(url)?;
        let mut variants: Vec<Variant> = media["video"]
            .as_object()
            .into_iter()
            .flatten()
            .filter_map(|(key, value)| {
                let format_id = key.strip_suffix("Path")?;
                let link = util::url_of(value, Some(url))?;
                Some(own_file(link, format_id))
            })
            .collect();
        if variants.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        variants
            .sort_by_key(|v| std::cmp::Reverse(quality_rank(v.format_id.as_deref().unwrap_or(""))));
        resolved.id = Some(display_id);
        resolved.title = page_title;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

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
    async fn dailymotion_hosted_videos_hand_off() {
        let model = json!({"videos": [{"id": 19550147, "idDailymotion": "x8a3u4k", "title": "Les gaffes de Cliffhanger", "duration": 346, "sources": null}]});
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.allocine.fr/video/video-19550147/",
            200,
            "text/html",
            model_page(&model),
        ));
        let with_file = json!({"videos": [{"id": 19550148, "idDailymotion": "x8a3u4l", "title": "Avec fichier", "duration": 10, "sources": null}]});
        fixture.exchanges.push(get(
            "https://www.allocine.fr/video/video-19550148/",
            200,
            "text/html",
            model_page(&with_file).replace(
                "<body>",
                r#"<body><script type="application/ld+json">{"@type":"VideoObject","name":"Avec fichier","contentUrl":"https://fr.vid.web.acsta.net/nmedia/33/14/12/12/19/19550148_hd_013.mp4?source=www&amp;platform=graph"}</script>"#,
            ),
        ));
        let resolver = AllocineResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.allocine.fr/video/video-19550147/").unwrap();
        assert!(resolver.matches(&url));
        let error = resolver.resolve(&url).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Redirect(u) if u.as_str() == "https://www.dailymotion.com/video/x8a3u4k"),
            "{error}"
        );
        let own = resolver
            .resolve(&Url::parse("https://www.allocine.fr/video/video-19550148/").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(own.title.as_deref(), Some("Avec fichier"));
        assert_eq!(own.variants.len(), 1);
        assert_eq!(own.variants[0].format_id.as_deref(), Some("hd"));
        assert_eq!(
            own.variants[0].url.as_str(),
            "https://fr.vid.web.acsta.net/nmedia/33/14/12/12/19/19550148_hd_013.mp4?source=www&platform=graph"
        );
    }

    #[tokio::test]
    async fn self_hosted_videos_resolve_from_the_model_or_the_media_service() {
        let model = json!({"videos": [{"id": 19540403, "idDailymotion": null, "title": "Bande-annonce", "duration": 120, "view_count": 5,
            "added_at": {"date": "2014-12-11 21:38:00.000000", "timezone": "Europe/Paris"},
            "sources": {"low": "https://fr.vid.web.acsta.net/nmedia/33/14/12/11/19540403_ld_013.mp4", "high": "//fr.vid.web.acsta.net/nmedia/33/14/12/11/19540403_hd_013.mp4", "medium": "https://fr.vid.web.acsta.net/nmedia/33/14/12/11/19540403_md_013.mp4"}}]});
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.allocine.fr/video/player_gen_cmedia=19540403&cfilm=222257.html",
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
        fixture.exchanges.push(get(
            "https://www.allocine.fr/ws/AcVisiondataV5.ashx?media=18635087",
            200,
            "application/json",
            json!({"video": {"ldPath": "https://fr.vid.web.acsta.net/a_ld.mp4", "hdPath": "https://fr.vid.web.acsta.net/a_hd.mp4", "title": "x"}}).to_string(),
        ));
        let resolver = AllocineResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(
                &Url::parse(
                    "https://www.allocine.fr/video/player_gen_cmedia=19540403&cfilm=222257.html",
                )
                .unwrap(),
            )
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("19540403"));
        assert_eq!(resolved.title.as_deref(), Some("Bande-annonce"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(120)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1418333880)
        );
        assert_eq!(resolved.variants.len(), 3);
        assert_eq!(resolved.variants[0].format_id.as_deref(), Some("hd"));
        assert_eq!(
            resolved.variants[0].url.as_str(),
            "https://fr.vid.web.acsta.net/nmedia/33/14/12/11/19540403_hd_013.mp4"
        );
        assert_eq!(resolved.variants[2].format_id.as_deref(), Some("ld"));
        let article = resolver
            .resolve(
                &Url::parse(
                    "https://www.allocine.fr/article/fichearticle_gen_carticle=18635087.html",
                )
                .unwrap(),
            )
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(article.title.as_deref(), Some("Astérix"));
        assert_eq!(article.variants.len(), 2);
        assert_eq!(article.variants[0].format_id.as_deref(), Some("hd"));
    }
}
