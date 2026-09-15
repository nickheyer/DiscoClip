//! Tenor GIFs, from the store the view page renders: every media format of the GIF, its
//! title, uploader and time; `tenor.com/{code}.gif` short links unwrapped to their view
//! page, and direct media links by their file.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use scraper::Selector;
use serde_json::Value;
use url::Url;

use super::page::Page;
use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Variant,
    VariantKind, clean_title, essence, fetch, navigation_headers, probe_file,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "tenor";
const SITE: &str = "https://tenor.com/";

static RE_VIEW_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?:^|-)(\d{5,})$").unwrap());
static RE_SHORT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([A-Za-z0-9_-]{6,20})\.gif$").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A view page, by the GIF's numeric id.
    View { page: Url, id: String },
    /// A `tenor.com/{code}.gif` link, which redirects to the view page.
    Short(Url),
    /// A file on the media hosts.
    Media(Url),
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    if host == "media.tenor.com"
        || host == "c.tenor.com"
        || (host.starts_with("media") && host.ends_with(".tenor.com"))
    {
        let name = segments.last()?;
        let ext = name.rsplit('.').next()?.to_ascii_lowercase();
        return matches!(ext.as_str(), "mp4" | "webm" | "gif").then(|| Link::Media(url.clone()));
    }
    if host != "tenor.com" && host != "www.tenor.com" {
        return None;
    }
    match segments.as_slice() {
        ["view", slug] | [_, "view", slug] => {
            let id = RE_VIEW_ID.captures(slug)?.get(1)?.as_str().to_string();
            let mut page = url.clone();
            page.set_query(None);
            page.set_fragment(None);
            Some(Link::View { page, id })
        }
        [name] if RE_SHORT.is_match(name) => Some(Link::Short(url.clone())),
        _ => None,
    }
}

/// The store the page renders in `<script id="store-cache">`.
pub fn store_in(page: &Page) -> Option<Value> {
    let selector = Selector::parse("script#store-cache").expect("valid");
    let text = page.document().select(&selector).next()?.text().collect::<String>();
    serde_json::from_str(&text).ok()
}

/// The GIF's record in the store: the one with `id`, else the first there is.
pub fn gif_in(store: &Value, id: &str) -> Option<Value> {
    let by_id = store["gifs"]["byId"].as_object()?;
    let entry = by_id
        .get(id)
        .or_else(|| by_id.values().next())?;
    entry["results"].as_array()?.first().cloned()
}

fn dims(format: &Value) -> (Option<u32>, Option<u32>) {
    match format["dims"].as_array().map(|d| d.as_slice()) {
        Some([w, h, ..]) => (
            w.as_u64().map(|w| w as u32),
            h.as_u64().map(|h| h as u32),
        ),
        _ => (None, None),
    }
}

/// The MP4 and WebM renditions of the GIF, largest first, and the GIF itself.
pub fn variants_of(gif: &Value) -> Vec<Variant> {
    let has_audio = gif["hasaudio"].as_bool().unwrap_or(false);
    let formats = &gif["media_formats"];
    let mut variants = Vec::new();
    for (key, container, video) in [
        ("mp4", Container::Mp4, Some(VideoCodec::H264)),
        ("webm", Container::Webm, None),
        ("tinymp4", Container::Mp4, Some(VideoCodec::H264)),
        ("nanomp4", Container::Mp4, Some(VideoCodec::H264)),
        ("gif", Container::Gif, None),
    ] {
        let format = &formats[key];
        let Some(url) = format["url"].as_str().and_then(|u| Url::parse(u).ok()) else {
            continue;
        };
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(container.clone());
        v.video = video;
        v.audio = if has_audio && container != Container::Gif {
            Some(if container == Container::Webm {
                AudioCodec::Opus
            } else {
                AudioCodec::Aac
            })
        } else {
            None
        };
        let (w, h) = dims(format);
        v.width = w;
        v.height = h;
        v.size = format["size"].as_u64().filter(|s| *s > 0);
        v.duration = format["duration"]
            .as_f64()
            .filter(|d| *d > 0.0)
            .map(Duration::from_secs_f64);
        v.format_id = Some(key.to_string());
        v.label = Some(key.to_string());
        variants.push(v);
    }
    variants
}

pub struct TenorResolver {
    http: Http,
}

impl TenorResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    async fn view(&self, page_url: &Url, id: Option<&str>, origin: &Url) -> Result<Resolved, ResolveError> {
        let fetched = fetch(
            &self.http,
            page_url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        match fetched.status.as_u16() {
            200..=299 => {}
            404 | 410 => return Err(ResolveError::NotFound(origin.clone())),
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the page answered HTTP {status}"),
                ));
            }
        }
        let html = fetched.text();
        let final_url = fetched.url.clone();
        let id = id
            .map(String::from)
            .or_else(|| match parse_link(&final_url) {
                Some(Link::View { id, .. }) => Some(id),
                _ => None,
            })
            .ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
        let page = Page::parse(&html, &final_url);
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.clone());
        resolved.webpage_url = Some(final_url.clone());
        if let Some(gif) = store_in(&page).and_then(|store| gif_in(&store, &id)) {
            resolved.title = gif["h1_title"]
                .as_str()
                .or(gif["title"].as_str())
                .filter(|t| !t.trim().is_empty())
                .or(gif["content_description"].as_str())
                .and_then(clean_title);
            resolved.description = gif["content_description"].as_str().and_then(clean_title);
            resolved.uploader = gif["user"]["username"].as_str().and_then(clean_title);
            resolved.uploader_url = gif["user"]["url"]
                .as_str()
                .and_then(|u| Url::parse(u).ok())
                .or_else(|| {
                    gif["user"]["username"]
                        .as_str()
                        .filter(|u| !u.is_empty())
                        .and_then(|u| Url::parse(&format!("{SITE}users/{u}")).ok())
                });
            resolved.uploaded_at = gif["created"]
                .as_f64()
                .filter(|t| *t > 0.0)
                .and_then(|t| Timestamp::from_second(t as i64).ok());
            resolved.thumbnail = gif["media_formats"]["gifpreview"]["url"]
                .as_str()
                .or(gif["media_formats"]["gif"]["url"].as_str())
                .and_then(|u| Url::parse(u).ok());
            resolved.variants = variants_of(&gif);
            resolved.duration = resolved.variants.iter().find_map(|v| v.duration);
            if !resolved.variants.is_empty() {
                return Ok(resolved);
            }
        }
        // The page's Open Graph tags name the MP4 when the store is not rendered.
        let video = page
            .meta("og:video")
            .or_else(|| page.meta("og:video:secure_url"))
            .and_then(|u| Url::parse(&u).ok())
            .ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
        let mut v = Variant::new(video, VariantKind::File);
        v.container = Some(Container::Mp4);
        v.video = Some(VideoCodec::H264);
        v.width = page.meta("og:video:width").and_then(|w| w.parse().ok());
        v.height = page.meta("og:video:height").and_then(|h| h.parse().ok());
        v.format_id = Some("mp4".into());
        resolved.title = page
            .meta("og:title")
            .map(|t| t.split(" - ").next().unwrap_or(&t).to_string())
            .and_then(|t| clean_title(&t));
        resolved.thumbnail = page.meta("og:image").and_then(|u| Url::parse(&u).ok());
        resolved.variants = vec![v];
        Ok(resolved)
    }

    async fn media(&self, file: &Url, origin: &Url) -> Result<Resolved, ResolveError> {
        let probed = probe_file(&self.http, file, PLATFORM, BROWSER_UA, &[]).await?;
        match probed.status.as_u16() {
            200 | 206 => {}
            404 | 410 | 403 => return Err(ResolveError::NotFound(origin.clone())),
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the media host answered HTTP {status}"),
                ));
            }
        }
        let name = file.path().rsplit('/').next().unwrap_or_default().to_string();
        let ext = name.rsplit('.').next().unwrap_or("mp4").to_ascii_lowercase();
        let mut v = Variant::new(file.clone(), VariantKind::File);
        v.container = Container::from_mime(&essence(probed.content_type.as_deref()))
            .or_else(|| Container::from_extension(&ext));
        if v.container == Some(Container::Mp4) {
            v.video = Some(VideoCodec::H264);
        }
        v.size = probed.size;
        let mut resolved = Resolved::new(PLATFORM);
        resolved.title = clean_title(&name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s).replace('-', " "));
        resolved.webpage_url = Some(file.clone());
        resolved.variants = vec![v];
        Ok(resolved)
    }
}

#[async_trait]
impl Resolver for TenorResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Tenor",
            hosts: &["tenor.com", "media.tenor.com", "c.tenor.com"],
            features: &["gifs", "short links", "media links", "localized pages"],
            formats: &["mp4", "webm", "gif"],
            session: SessionSupport::None,
            examples: &[
                "https://tenor.com/view/banana-cat-gif-2736737110227615394",
                "https://tenor.com/dqkrPHwUKcg.gif",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::View { page, id } => Ok(Resolution::from(self.view(&page, Some(&id), url).await?)),
            Link::Short(short) => Ok(Resolution::from(self.view(&short, None, url).await?)),
            Link::Media(file) => Ok(Resolution::from(self.media(&file, url).await?)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

    fn get(url: &str, status: u16, content_type: &str, body: &str, headers: &[(&str, &str)], final_url: &str) -> Exchange {
        let mut all = vec![("content-type".to_string(), content_type.to_string())];
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
                url: final_url.into(),
                headers: all,
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    const PAGE: &str = r#"<html><head><meta property="og:title" content="Banana Cat Meme - Banana cat - Discover &amp; Share GIFs"><meta property="og:video" content="https://media.tenor.com/JfrX5uK-_qIAAAPo/banana-cat.mp4"><meta property="og:video:width" content="380"><meta property="og:video:height" content="498"><meta property="og:image" content="https://media1.tenor.com/m/JfrX5uK-_qIAAAAC/banana-cat.gif"></head><body>
    <script id="store-cache" type="text/x-cache">{"gifs":{"byId":{"2736737110227615394":{"results":[{"id":"2736737110227615394","title":"","media_formats":{"gif":{"url":"https://media1.tenor.com/m/JfrX5uK-_qIAAAAC/banana-cat.gif","duration":0,"dims":[380,498],"size":53082},"mp4":{"url":"https://media.tenor.com/JfrX5uK-_qIAAAPo/banana-cat.mp4","duration":1.2,"dims":[380,498],"size":4543},"tinymp4":{"url":"https://media.tenor.com/JfrX5uK-_qIAAAP1/banana-cat.mp4","duration":0,"dims":[244,320],"size":3053},"webm":{"url":"https://media.tenor.com/JfrX5uK-_qIAAAPs/banana-cat.webm","duration":0,"dims":[380,498],"size":9306},"gifpreview":{"url":"https://media.tenor.com/JfrX5uK-_qIAAAAe/banana-cat.png","dims":[380,498],"size":59708}},"created":1766968109.51238,"content_description":"a cat is wearing a banana costume .","h1_title":"Banana Cat Meme","itemurl":"https://tenor.com/view/banana-cat-gif-2736737110227615394","url":"https://tenor.com/dqkrPHwUKcg.gif","user":{"username":"Darshuil","url":"https://tenor.com/users/Darshuil"},"hasaudio":false}]}}}}</script></body></html>"#;

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert!(matches!(
            link("https://tenor.com/view/banana-cat-gif-2736737110227615394?x=1"),
            Some(Link::View { id, .. }) if id == "2736737110227615394"
        ));
        assert!(matches!(link("https://tenor.com/de/view/katze-gif-123456"), Some(Link::View { id, .. }) if id == "123456"));
        assert!(matches!(link("https://tenor.com/dqkrPHwUKcg.gif"), Some(Link::Short(_))));
        assert!(matches!(link("https://media.tenor.com/JfrX5uK-_qIAAAPo/banana-cat.mp4"), Some(Link::Media(_))));
        assert!(matches!(link("https://media1.tenor.com/m/JfrX5uK-_qIAAAAC/banana-cat.gif"), Some(Link::Media(_))));
        assert_eq!(link("https://tenor.com/search/cat-gifs"), None);
        assert_eq!(link("https://tenor.com/view/no-id"), None);
    }

    #[tokio::test]
    async fn view_pages_resolve_from_the_rendered_store() {
        let mut fixture = Fixture::new("tenor", None);
        fixture.exchanges.push(get("https://tenor.com/view/banana-cat-gif-2736737110227615394", 200, "text/html", PAGE, &[], "https://tenor.com/view/banana-cat-gif-2736737110227615394"));
        let resolver = TenorResolver::new(Http::replay(fixture));
        let url = Url::parse("https://tenor.com/view/banana-cat-gif-2736737110227615394").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.title.as_deref(), Some("Banana Cat Meme"));
        assert_eq!(resolved.uploader.as_deref(), Some("Darshuil"));
        assert_eq!(resolved.uploader_url.unwrap().as_str(), "https://tenor.com/users/Darshuil");
        assert_eq!(resolved.uploaded_at.unwrap().as_second(), 1766968109);
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(1.2)));
        assert_eq!(resolved.variants.len(), 4);
        let mp4 = &resolved.variants[0];
        assert_eq!(mp4.url.as_str(), "https://media.tenor.com/JfrX5uK-_qIAAAPo/banana-cat.mp4");
        assert_eq!((mp4.width, mp4.height), (Some(380), Some(498)));
        assert_eq!(mp4.size, Some(4543));
        assert!(mp4.audio.is_none());
        assert_eq!(resolved.variants[1].container, Some(Container::Webm));
        assert_eq!(resolved.variants[3].container, Some(Container::Gif));
    }

    #[tokio::test]
    async fn short_links_land_on_the_view_page_and_bare_pages_use_open_graph() {
        let og_only = r#"<html><head><meta property="og:title" content="Banana Cat Meme - Discover &amp; Share GIFs"><meta property="og:video" content="https://media.tenor.com/JfrX5uK-_qIAAAPo/banana-cat.mp4"><meta property="og:video:width" content="380"><meta property="og:video:height" content="498"></head></html>"#;
        let mut fixture = Fixture::new("tenor", None);
        fixture.exchanges.push(get("https://tenor.com/dqkrPHwUKcg.gif", 200, "text/html", og_only, &[], "https://tenor.com/view/banana-cat-gif-2736737110227615394"));
        fixture.exchanges.push(get("https://media.tenor.com/JfrX5uK-_qIAAAPo/banana-cat.mp4", 206, "video/mp4", "", &[("content-range", "bytes 0-0/4543")], "https://media.tenor.com/JfrX5uK-_qIAAAPo/banana-cat.mp4"));
        fixture.exchanges.push(get("https://tenor.com/view/gone-gif-99999", 404, "text/html", "", &[], "https://tenor.com/view/gone-gif-99999"));
        let resolver = TenorResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://tenor.com/dqkrPHwUKcg.gif").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("2736737110227615394"));
        assert_eq!(resolved.title.as_deref(), Some("Banana Cat Meme"));
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].height, Some(498));
        let resolved = resolver
            .resolve(&Url::parse("https://media.tenor.com/JfrX5uK-_qIAAAPo/banana-cat.mp4").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.title.as_deref(), Some("banana cat"));
        assert_eq!(resolved.variants[0].size, Some(4543));
        assert!(matches!(
            resolver.resolve(&Url::parse("https://tenor.com/view/gone-gif-99999").unwrap()).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
