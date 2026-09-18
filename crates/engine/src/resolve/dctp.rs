//! dctp.tv films, through the versioned REST API on S3 that the site's player reads; the
//! media itself is served as HLS and as progressive m4v files from two hosts.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Tag, Variant,
    clean_title, fetch_ok, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "dctp";
const API_BASE: &str = "http://dctp-ivms2-restapi.s3.amazonaws.com";

static RE_FILM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/filme/([^/?#&]+)").unwrap());

/// The film slug a link names, from `/filme/{slug}` or the `#/filme/{slug}` route.
pub fn film_slug(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    if host != "dctp.tv" {
        return None;
    }
    RE_FILM
        .captures(url.path())
        .or_else(|| {
            url.fragment()
                .and_then(|fragment| RE_FILM.captures(fragment))
        })
        .map(|c| c[1].to_string())
}

pub struct DctpResolver {
    http: Http,
}

impl DctpResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    async fn json(&self, api: &str, origin: &Url) -> Result<Value, ResolveError> {
        let url = Url::parse(api).map_err(|e| ResolveError::malformed(origin, e.to_string()))?;
        fetch_ok(&self.http, &url, PLATFORM, BROWSER_UA, &[], MAX_PAGE)
            .await?
            .json(origin)
    }
}

/// The three deliveries of one encoding: an HLS playlist, the S3 file and the CDN file.
fn variants_for(uuid: &str, suffix: &str) -> Vec<Variant> {
    let name = format!("{uuid}_dctp_{suffix}.m4v");
    let height = suffix.strip_suffix('p').and_then(|h| h.parse().ok());
    let mut out = Vec::new();
    let hls = Url::parse(&format!(
        "https://cdn-segments.dctp.tv/{name}/playlist.m3u8"
    ))
    .expect("valid");
    let mut playlist = Variant::hls(hls);
    playlist.format_id = Some(format!("hls-{suffix}"));
    playlist.height = height;
    out.push(playlist);
    for (prefix, host) in [
        ("s3", "completed-media.s3.amazonaws.com"),
        ("http", "cdn-media.dctp.tv"),
    ] {
        let mut file = Variant::file(Url::parse(&format!("https://{host}/{name}")).expect("valid"));
        file.container = Some(Container::Mp4);
        file.video = Some(VideoCodec::H264);
        file.audio = Some(AudioCodec::Aac);
        file.height = height;
        file.format_id = Some(format!("{prefix}-{suffix}"));
        out.push(file);
    }
    for variant in &mut out {
        variant.label = Some(suffix.to_string());
    }
    out
}

#[async_trait]
impl Resolver for DctpResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "dctp.tv",
            hosts: &["dctp.tv"],
            features: &["videos"],
            formats: &["mp4", "hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::Video],
            session: SessionSupport::None,
            examples: &["http://www.dctp.tv/filme/videoinstallation-fuer-eine-kaufhausfassade/"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        film_slug(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let slug = film_slug(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let version = self.json(&format!("{API_BASE}/version.json"), url).await?;
        let version_name = util::text(&version["version_name"])
            .ok_or_else(|| ResolveError::malformed(url, "the API names no version"))?;
        let restapi = format!("{API_BASE}/{version_name}/restapi");
        let info = self
            .json(&format!("{restapi}/slugs/{slug}.json"), url)
            .await?;
        let object_id = util::text(&info["object_id"])
            .ok_or_else(|| ResolveError::malformed(url, "the slug names no media object"))?;
        let media = self
            .json(&format!("{restapi}/media/{object_id}.json"), url)
            .await?;
        let uuid = util::text(&media["uuid"])
            .ok_or_else(|| ResolveError::malformed(url, "the media has no uuid"))?;
        let is_wide = media["is_wide"].as_bool().unwrap_or(false);
        let mut variants = variants_for(&uuid, if is_wide { "0500_16x9" } else { "0500_4x3" });
        if is_wide {
            variants.extend(variants_for(&uuid, "720p"));
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(uuid);
        resolved.title = media["title"].as_str().and_then(clean_title);
        resolved.description = media["description"]
            .as_str()
            .filter(|d| !d.trim().is_empty())
            .or_else(|| media["teaser"].as_str())
            .and_then(clean_title);
        resolved.uploaded_at = media["created"].as_str().and_then(util::parse_timestamp);
        resolved.duration = util::millis(&media["duration_in_ms"]);
        resolved.thumbnail = media["images"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|image| {
                let link = util::url_of(&image["url"], None)?;
                let area = util::u32_of(&image["width"]).unwrap_or(0) as u64
                    * util::u32_of(&image["height"]).unwrap_or(0) as u64;
                Some((area, link))
            })
            .max_by_key(|(area, _)| *area)
            .map(|(_, link)| link);
        resolved.webpage_url = Url::parse(&format!("https://www.dctp.tv/filme/{slug}/")).ok();
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

    #[test]
    fn links_are_read() {
        let slug = |s: &str| film_slug(&Url::parse(s).unwrap());
        assert_eq!(
            slug("http://www.dctp.tv/filme/videoinstallation-fuer-eine-kaufhausfassade/"),
            Some("videoinstallation-fuer-eine-kaufhausfassade".into())
        );
        assert_eq!(
            slug("http://www.dctp.tv/filme/sind-youtuber-die-besseren-lehrer/"),
            Some("sind-youtuber-die-besseren-lehrer".into())
        );
        assert_eq!(
            slug("https://dctp.tv/#/filme/some-film"),
            Some("some-film".into())
        );
        assert_eq!(slug("https://www.dctp.tv/"), None);
        assert_eq!(slug("https://www.dctp.tv/themen/kultur"), None);
        assert_eq!(slug("https://example.com/filme/x"), None);
    }

    #[tokio::test]
    async fn films_resolve_with_hls_and_files() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "http://dctp-ivms2-restapi.s3.amazonaws.com/version.json",
            200,
            "application/json",
            json!({"version_name": "20260915075017"}).to_string(),
        ));
        fixture.exchanges.push(get(
            "http://dctp-ivms2-restapi.s3.amazonaws.com/20260915075017/restapi/slugs/videoinstallation-fuer-eine-kaufhausfassade.json",
            200,
            "application/json",
            json!({"name": "videoinstallation-fuer-eine-kaufhausfassade", "object_id": 10511, "object_module": "medium"}).to_string(),
        ));
        fixture.exchanges.push(get(
            "http://dctp-ivms2-restapi.s3.amazonaws.com/20260915075017/restapi/media/10511.json",
            200,
            "application/json",
            json!({
                "uuid": "95eaa4f33dad413aa17b4ee613cccc6c", "title": "Videoinstallation für eine Kaufhausfassade",
                "subtitle": null, "teaser": "Kurzfilm", "description": "", "created": "2011-04-07T11:52:02+02:00",
                "duration_in_ms": 71240, "is_wide": false,
                "images": [{"url": "https://cdn-media.dctp.tv/small.jpg", "width": 320, "height": 240},
                           {"url": "https://cdn-media.dctp.tv/large.jpg", "width": 640, "height": 480}]
            }).to_string(),
        ));
        let resolver = DctpResolver::new(Http::replay(fixture));
        let url =
            Url::parse("http://www.dctp.tv/filme/videoinstallation-fuer-eine-kaufhausfassade/")
                .unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(
            resolved.id.as_deref(),
            Some("95eaa4f33dad413aa17b4ee613cccc6c")
        );
        assert_eq!(
            resolved.title.as_deref(),
            Some("Videoinstallation für eine Kaufhausfassade")
        );
        assert_eq!(resolved.description.as_deref(), Some("Kurzfilm"));
        assert_eq!(
            resolved.duration,
            Some(std::time::Duration::from_millis(71240))
        );
        assert_eq!(resolved.uploaded_at.unwrap().as_second(), 1302169922);
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://cdn-media.dctp.tv/large.jpg"
        );
        assert_eq!(resolved.variants.len(), 3);
        assert_eq!(
            resolved.variants[0].url.as_str(),
            "https://cdn-segments.dctp.tv/95eaa4f33dad413aa17b4ee613cccc6c_dctp_0500_4x3.m4v/playlist.m3u8"
        );
        assert_eq!(resolved.variants[0].kind, crate::resolve::VariantKind::Hls);
        assert_eq!(
            resolved.variants[2].url.as_str(),
            "https://cdn-media.dctp.tv/95eaa4f33dad413aa17b4ee613cccc6c_dctp_0500_4x3.m4v"
        );
        assert_eq!(resolved.variants[2].container, Some(Container::Mp4));
        assert_eq!(
            resolved.variants[2].format_id.as_deref(),
            Some("http-0500_4x3")
        );
    }

    #[tokio::test]
    async fn wide_films_add_the_720p_encoding_and_missing_films_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "http://dctp-ivms2-restapi.s3.amazonaws.com/version.json",
            200,
            "application/json",
            json!({"version_name": "v1"}).to_string(),
        ));
        fixture.exchanges.push(get(
            "http://dctp-ivms2-restapi.s3.amazonaws.com/v1/restapi/slugs/wide.json",
            200,
            "application/json",
            json!({"object_id": "7"}).to_string(),
        ));
        fixture.exchanges.push(get(
            "http://dctp-ivms2-restapi.s3.amazonaws.com/v1/restapi/media/7.json",
            200,
            "application/json",
            json!({"uuid": "abc", "title": "Wide", "is_wide": true, "duration_in_ms": "1000"})
                .to_string(),
        ));
        fixture.exchanges.push(get(
            "http://dctp-ivms2-restapi.s3.amazonaws.com/version.json",
            200,
            "application/json",
            json!({"version_name": "v1"}).to_string(),
        ));
        fixture.exchanges.push(get(
            "http://dctp-ivms2-restapi.s3.amazonaws.com/v1/restapi/slugs/gone.json",
            403,
            "application/xml",
            "<Error><Code>AccessDenied</Code></Error>".into(),
        ));
        let resolver = DctpResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://www.dctp.tv/filme/wide/").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.variants.len(), 6);
        assert_eq!(resolved.variants[3].format_id.as_deref(), Some("hls-720p"));
        assert_eq!(resolved.variants[3].height, Some(720));
        assert_eq!(resolved.variants[0].label.as_deref(), Some("0500_16x9"));
        let error = resolver
            .resolve(&Url::parse("https://www.dctp.tv/filme/gone/").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { .. }),
            "{error}"
        );
    }
}
