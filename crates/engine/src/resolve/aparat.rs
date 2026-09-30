//! Aparat (aparat.com) videos, through the site's own API, which names a video's HLS
//! manifest.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Tag,
    clean_title, fetch, hls, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::MediaKind;

pub const PLATFORM: &str = "aparat";
const SITE: &str = "https://www.aparat.com/";
const API: &str = "https://www.aparat.com/api/fa/v1/video/video/show/videohash/";

/// `/v/{hash}` or `/video/video/embed/videohash/{hash}`.
static RE_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(?:v/|video/video/embed/videohash/)([a-zA-Z0-9]+)").unwrap());

pub fn video_id(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "aparat.com" && host != "www.aparat.com" {
        return None;
    }
    RE_PATH.captures(url.path()).map(|caps| caps[1].to_string())
}

fn referer() -> Vec<(String, String)> {
    vec![
        ("accept".to_string(), "application/json".to_string()),
        ("referer".to_string(), SITE.to_string()),
    ]
}

pub struct AparatResolver {
    http: Http,
}

impl AparatResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for AparatResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Aparat",
            hosts: &["aparat.com"],
            features: &["videos"],
            formats: &["hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::Video],
            session: SessionSupport::None,
            examples: &["https://www.aparat.com/v/wP8On"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        video_id(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let id = video_id(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let api = Url::parse(&format!("{API}{id}")).expect("valid");
        let fetched = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &referer(), MAX_PAGE).await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        let answer = fetched.json(url)?;
        let Some(video) = answer["data"]["attributes"].as_object() else {
            return Err(ResolveError::NotFound(url.clone()));
        };
        let video = Value::Object(video.clone());
        if util::boolean(&video["deleted"]) == Some(true) {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let manifest = util::url_of(&video["hls_link"], None)
            .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let headers = [("referer".to_string(), SITE.to_string())];
        let expanded = hls::expand(&self.http, &manifest, PLATFORM, BROWSER_UA, &headers).await?;
        if expanded.variants.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.variants = expanded.variants;
        resolved.subtitles = expanded.subtitles;
        resolved.id = Some(id.clone());
        resolved.title = video["title"].as_str().and_then(clean_title);
        resolved.description = video["description"]
            .as_str()
            .map(util::clean_html)
            .and_then(|d| clean_title(&d));
        resolved.thumbnail = util::url_of(&video["big_poster"], None)
            .or_else(|| util::url_of(&video["medium_poster"], None));
        resolved.duration = util::seconds(&video["duration"]).or(expanded.duration);
        resolved.uploaded_at = util::time(&video["sdate_real"]).or_else(|| {
            // The age the API states, seconds before now.
            util::int(&video["sdate_timediff"]).map(|age| util::from_now(-age))
        });
        resolved.uploader = video["owner_username"]
            .as_str()
            .or(video["username"].as_str())
            .and_then(clean_title);
        resolved.uploader_url = resolved
            .uploader
            .as_deref()
            .and_then(|name| Url::parse(&format!("{SITE}{name}")).ok());
        resolved.webpage_url = Url::parse(&format!("{SITE}v/{id}")).ok();
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};
    use crate::resolve::VariantKind;
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
        assert_eq!(id("http://www.aparat.com/v/wP8On"), Some("wP8On".into()));
        assert_eq!(id("https://www.aparat.com/v/8dflw/"), Some("8dflw".into()));
        assert_eq!(
            id("https://www.aparat.com/video/video/embed/videohash/wP8On/vt/frame"),
            Some("wP8On".into())
        );
        assert_eq!(id("https://www.aparat.com/zoomit"), None);
        assert_eq!(id("https://example.com/v/wP8On"), None);
    }

    #[tokio::test]
    async fn videos_resolve_through_their_manifest() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.aparat.com/api/fa/v1/video/video/show/videohash/wP8On",
            200,
            "application/json",
            json!({"data": {"type": "VideoShow", "id": "878887", "attributes": {
                "id": 878887, "title": "تیم گلکسی 11 - زومیت", "description": "www.zoomit.ir", "uid": "wP8On",
                "big_poster": "https://static.cdn.asset.aparat.cloud/avt/878887.jpg?width=900", "duration": "231",
                "sdate_real": "2013-12-18T22:57:00+03:30", "sdate_timediff": 402126888, "owner_username": "zoomit",
                "hls_link": "https://www.aparat.com/external/chelsea/manifest.m3u8?q=abc"
            }}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://www.aparat.com/external/chelsea/manifest.m3u8?q=abc",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=779144,RESOLUTION=640x352\nhttps://aspb3.cdn.asset.aparat.com/aparat-video/a-360p.m3u8\n".into(),
        ));
        fixture.exchanges.push(get(
            "https://aspb3.cdn.asset.aparat.com/aparat-video/a-360p.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n0.ts\n#EXT-X-ENDLIST\n".into(),
        ));
        fixture.exchanges.push(get(
            "https://www.aparat.com/api/fa/v1/video/video/show/videohash/gone1",
            200,
            "application/json",
            json!({"data": [], "errors": [{"detail": "not found"}]}).to_string(),
        ));
        let resolver = AparatResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.aparat.com/v/wP8On").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("wP8On"));
        assert_eq!(resolved.title.as_deref(), Some("تیم گلکسی 11 - زومیت"));
        assert_eq!(resolved.uploader.as_deref(), Some("zoomit"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(231)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1387394820)
        );
        crate::resolve::assert_one_family(&resolved.variants);
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].kind, VariantKind::Hls);
        assert_eq!(resolved.variants[0].height, Some(352));
        assert_eq!(
            resolved.variants[0].headers,
            vec![("referer".to_string(), SITE.to_string())]
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.aparat.com/v/gone1").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
