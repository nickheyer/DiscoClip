//! Jixie-hosted videos, through the stream API Jixie's player calls. Jixie's own API
//! hosts (`apidam.jixie.io`, `stream.jixie.media`) are gone; the player now reads the
//! stream API through the proxy of the publisher whose page it runs on, and Kompas's is
//! the public one, so every Jixie id is read there. Kompas video pages name their id in
//! the path.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport,
    Variant, clean_title, fetch, hls, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "jixie";

/// The stream API the player reads, through Kompas's proxy of it, asked with
/// `?format=hls&metadata=full&video_id=…`.
const API: &str = "https://apiv.kompas.com/jixie-stream";
/// The page the proxy serves.
const KOMPAS_SITE: &str = "https://video.kompas.com/";

/// `/watch/{id}/{slug}` on video.kompas.com.
static RE_KOMPAS_WATCH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/watch/(\d+)(?:/|$)").unwrap());

fn is_video_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The video id a link names: a link to the player's stream API on `apidam.jixie.io`,
/// `stream.jixie.media` or `apiv.kompas.com` (`…?video_id=164474`), or a Kompas video
/// page (`video.kompas.com/watch/1924197/…`).
pub fn video_id(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let path = url.path().trim_end_matches('/');
    let api = match host.as_str() {
        "apidam.jixie.io" | "stream.jixie.media" => path == "/api/public/stream",
        "apiv.kompas.com" => path == "/jixie-stream",
        "video.kompas.com" | "www.video.kompas.com" => {
            return RE_KOMPAS_WATCH
                .captures(url.path())
                .map(|caps| caps[1].to_string());
        }
        _ => false,
    };
    if !api {
        return None;
    }
    util::query_param(url, "video_id").filter(|id| is_video_id(id))
}

/// The streams the API lists: `streams` as the API once listed them, or those of every
/// player platform (`platforms.jxhls.streams`…) as it lists them now, each once.
fn api_streams(data: &Value) -> Option<Vec<Value>> {
    let mut streams = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut listed = false;
    let mut take = |list: &Value| {
        if let Some(list) = list.as_array() {
            listed = true;
            for stream in list {
                let key = stream["url"].as_str().unwrap_or("").to_string();
                if key.is_empty() || seen.insert(key) {
                    streams.push(stream.clone());
                }
            }
        }
    };
    take(&data["streams"]);
    if let Some(platforms) = data["platforms"].as_object() {
        for platform in platforms.values() {
            take(&platform["streams"]);
        }
    }
    listed.then_some(streams)
}

/// The DRM system the API's `drm` field names: a system's name, the systems of a
/// per-system map, or `drm` when it only says the video is locked; nothing when it says
/// the video is free.
fn drm_system(value: &Value) -> Option<String> {
    match value {
        Value::String(name) => match util::boolean(value) {
            Some(false) => None,
            Some(true) => Some("drm".into()),
            None => Some(name.trim().to_ascii_lowercase()),
        },
        Value::Object(map) => (!map.is_empty()).then(|| {
            map.keys()
                .map(|k| k.to_ascii_lowercase())
                .collect::<Vec<_>>()
                .join(",")
        }),
        Value::Array(list) => (!list.is_empty()).then(|| "drm".to_string()),
        other => util::boolean(other)
            .filter(|locked| *locked)
            .map(|_| "drm".to_string()),
    }
}

/// The largest of the thumbnails the metadata lists, given as `{url, width, height}`
/// objects or plain links; among equals the last listed, as yt-dlp chooses.
fn largest_thumbnail(thumbnails: &Value) -> Option<Url> {
    let mut best: Option<(u64, Url)> = None;
    for entry in thumbnails.as_array()? {
        let link = match entry {
            Value::String(text) => text.as_str(),
            other => other["url"].as_str().unwrap_or(""),
        };
        let Some(link) = util::join_url(None, link) else {
            continue;
        };
        let area =
            util::uint(&entry["width"]).unwrap_or(0) * util::uint(&entry["height"]).unwrap_or(0);
        if best.as_ref().is_none_or(|(largest, _)| area >= *largest) {
            best = Some((area, link));
        }
    }
    best.map(|(_, link)| link)
}

/// What a site's own page adds to the stream API's answer: the title and description its
/// meta tags carry, read before the API is called.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PageMeta {
    pub title: Option<String>,
    pub description: Option<String>,
}

impl PageMeta {
    pub fn of(page: &Page) -> Self {
        Self {
            title: page.meta("og:title").or_else(|| page.meta("twitter:title")),
            description: page
                .meta("description")
                .or_else(|| page.meta("og:description"))
                .or_else(|| page.meta("twitter:description")),
        }
    }
}

/// Reads `video_id` from the stream API as `platform`, the way yt-dlp's `JixieBaseIE`
/// does: each stream becomes a variant (HLS playlists expanded, and locked when the video
/// carries DRM), and `page`, what the caller read from the site's own page, fills the
/// title and description the API leaves blank.
pub async fn resolve_video(
    http: &Http,
    platform: &str,
    video_id: &str,
    page: Option<PageMeta>,
    origin: &Url,
) -> Result<Resolved, ResolveError> {
    let api = util::with_query(
        &Url::parse(API).expect("valid"),
        &[
            ("format", "hls"),
            ("metadata", "full"),
            ("video_id", video_id),
        ],
    );
    let headers = [
        ("accept".to_string(), "application/json".to_string()),
        ("referer".to_string(), KOMPAS_SITE.to_string()),
    ];
    let fetched = fetch(http, &api, platform, BROWSER_UA, &headers, MAX_PAGE).await?;
    if let Some(error) = status_error(fetched.status, origin) {
        return Err(error);
    }
    let answer = fetched.json(origin)?;
    let data = &answer["data"];
    // An unknown id is refused (`success: false`), or answered with an entry that has
    // neither metadata nor streams.
    let unknown = answer["success"].as_bool() == Some(false)
        || (data["metadata"].is_null() && api_streams(data).is_none());
    if unknown {
        return Err(ResolveError::NotFound(origin.clone()));
    }
    let Some(streams) = api_streams(data) else {
        return Err(ResolveError::malformed(
            origin,
            "the stream API lists no streams",
        ));
    };
    let locked = drm_system(&data["drm"]);
    let mut variants = Vec::new();
    let mut subtitles = Vec::new();
    let mut playlist_duration = None;
    let mut live = false;
    for stream in &streams {
        let Some(stream_url) = stream["url"].as_str().and_then(|u| util::join_url(None, u)) else {
            continue;
        };
        if stream["type"].as_str() == Some("HLS") {
            let expanded = hls::expand(http, &stream_url, platform, BROWSER_UA, &[]).await?;
            playlist_duration = playlist_duration.or(expanded.duration);
            live |= expanded.live;
            for mut variant in expanded.variants {
                variant.drm = locked.clone();
                variants.push(variant);
            }
            subtitles.extend(expanded.subtitles);
        } else {
            let mut variant = Variant::file(stream_url);
            variant.container = Some(Container::Mp4);
            variant.video = Some(VideoCodec::H264);
            variant.audio = Some(AudioCodec::Aac);
            variant.width = util::u32_of(&stream["width"]).filter(|w| *w > 0);
            variant.height = util::u32_of(&stream["height"]).filter(|h| *h > 0);
            variant.label = variant.height.map(|h| format!("{h}p"));
            variant.format_id = stream["type"].as_str().map(|t| t.to_ascii_lowercase());
            variants.push(variant);
        }
    }
    if variants.is_empty() {
        return Err(ResolveError::unavailable(
            origin,
            "the video has no streams",
        ));
    }
    if let Some(system) = &locked
        && variants.iter().all(|v| v.drm.is_some())
    {
        return Err(ResolveError::drm(origin, system.clone()));
    }
    let metadata = &data["metadata"];
    let mut resolved = Resolved::new(platform);
    resolved.id = Some(video_id.to_string());
    resolved.title = data["title"]
        .as_str()
        .and_then(clean_title)
        .or_else(|| metadata["title"].as_str().and_then(clean_title))
        .or_else(|| {
            page.as_ref()
                .and_then(|p| p.title.as_deref())
                .and_then(clean_title)
        });
    resolved.description = metadata["description"]
        .as_str()
        .map(util::clean_html)
        .filter(|d| !d.is_empty())
        .or_else(|| page.as_ref().and_then(|p| p.description.clone()));
    resolved.thumbnail = largest_thumbnail(&metadata["thumbnails"]);
    resolved.duration = util::float(&metadata["duration"])
        .filter(|d| *d > 0.0)
        .map(Duration::from_secs_f64)
        .or(playlist_duration);
    resolved.live = live;
    resolved.webpage_url = Some(origin.clone());
    resolved.subtitles = subtitles;
    resolved.variants = variants;
    Ok(resolved)
}

pub struct JixieResolver {
    http: Http,
}

impl JixieResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for JixieResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Jixie",
            hosts: &["video.kompas.com", "apiv.kompas.com", "apidam.jixie.io", "stream.jixie.media"],
            features: &["videos"],
            formats: &["mp4", "hls"],
            session: SessionSupport::None,
            examples: &[
                "https://video.kompas.com/watch/1924197/chitra-subyakto-bajumu-yang-itu-itu-saja-menyelamatkanmu-dan-bumi-beginu-5-tahun",
                "https://apiv.kompas.com/jixie-stream?metadata=full&video_id=1924197",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        video_id(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let id = video_id(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let resolved = resolve_video(&self.http, PLATFORM, &id, None, url).await?;
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
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

    const STREAM_API: &str =
        "https://apiv.kompas.com/jixie-stream?format=hls&metadata=full&video_id=164474";
    const OLD_API: &str = "https://apidam.jixie.io/api/public/stream?metadata=full&video_id=164474";
    const MASTER: &str = "https://video.jixie.media/1001/164474/master.m3u8";
    const MEDIA: &str = "https://video.jixie.media/1001/164474/720p.m3u8";

    fn playlists(fixture: &mut Fixture, ended: bool) {
        fixture.exchanges.push(get(
            MASTER,
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720\n720p.m3u8\n".into(),
        ));
        let mut media =
            "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n0.ts\n#EXTINF:5.5,\n1.ts\n"
                .to_string();
        if ended {
            media.push_str("#EXT-X-ENDLIST\n");
        }
        fixture
            .exchanges
            .push(get(MEDIA, 200, "application/vnd.apple.mpegurl", media));
    }

    #[test]
    fn links_are_read() {
        let id = |s: &str| video_id(&Url::parse(s).unwrap());
        assert_eq!(id(STREAM_API), Some("164474".into()));
        assert_eq!(id(OLD_API), Some("164474".into()));
        assert_eq!(
            id("https://stream.jixie.media/api/public/stream?format=hls&video_id=abc-1"),
            Some("abc-1".into())
        );
        assert_eq!(
            id("https://video.kompas.com/watch/1924197/chitra-subyakto-bajumu"),
            Some("1924197".into())
        );
        assert_eq!(id("https://video.kompas.com/watch/1924197"), Some("1924197".into()));
        assert_eq!(id("https://video.kompas.com/channel/beginu"), None);
        assert_eq!(id("https://apiv.kompas.com/other?video_id=1"), None);
        assert_eq!(id("https://apidam.jixie.io/api/public/stream"), None);
        assert_eq!(
            id("https://apidam.jixie.io/api/public/other?video_id=1"),
            None
        );
        assert_eq!(
            id("https://video.jixie.media/1001/164474/master.m3u8"),
            None
        );
    }

    #[test]
    fn streams_are_read_from_either_shape() {
        let old = json!({"streams": [{"type": "HLS", "url": "https://v/a.m3u8"}]});
        assert_eq!(api_streams(&old).unwrap().len(), 1);
        let new = json!({"streams": null, "platforms": {
            "jxhls": {"streams": [{"url": "https://v/master.m3u8", "type": "HLS"}], "qualities": [{"label": "720p"}]},
            "jxmp4": {"streams": [{"url": "https://v/master.m3u8", "type": "HLS"}, {"url": "https://v/a.mp4", "type": "MP4", "width": 640, "height": 360}]}
        }});
        let streams = api_streams(&new).unwrap();
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[1]["type"], "MP4");
        assert_eq!(api_streams(&json!({"metadata": {}})), None);
    }

    #[tokio::test]
    async fn videos_resolve_with_every_stream() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(STREAM_API, 200, "application/json", json!({
            "data": {
                "title": "Kim Jong Un Siap Kirim Nuklir Lawan AS dan Korsel",
                "owner_id": "9262bf2590d558736cac4fff7978fcb1",
                "drm": false,
                "streams": [
                    {"type": "HLS", "url": MASTER},
                    {"type": "MP4", "url": "https://video.jixie.media/1001/164474/164474_640x360.mp4", "width": 640, "height": 360},
                    {"type": "MP4", "url": ""}
                ],
                "metadata": {
                    "description": "<p>Korea Utara &amp; nuklir</p>",
                    "duration": 85.066667,
                    "keywords": "kim jong un,nuklir",
                    "categories": "news",
                    "thumbnails": [
                        {"url": "https://video.jixie.media/1001/164474/164474_640x360.jpg", "width": 640, "height": 360},
                        {"url": "https://video.jixie.media/1001/164474/164474_1280x720.jpg", "width": 1280, "height": 720}
                    ]
                }
            }
        }).to_string()));
        playlists(&mut fixture, true);
        let resolver = JixieResolver::new(Http::replay(fixture));
        let url = Url::parse(OLD_API).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("164474"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Kim Jong Un Siap Kirim Nuklir Lawan AS dan Korsel")
        );
        assert_eq!(
            resolved.description.as_deref(),
            Some("Korea Utara & nuklir")
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(85.066667)));
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://video.jixie.media/1001/164474/164474_1280x720.jpg"
        );
        assert!(!resolved.live);
        assert_eq!(resolved.variants.len(), 2);
        let playlist = &resolved.variants[0];
        assert_eq!(playlist.kind, VariantKind::Hls);
        assert_eq!(playlist.url.as_str(), MEDIA);
        assert_eq!(playlist.height, Some(720));
        assert_eq!(playlist.drm, None);
        let file = &resolved.variants[1];
        assert_eq!(file.kind, VariantKind::File);
        assert_eq!(file.width, Some(640));
        assert_eq!(file.height, Some(360));
        assert_eq!(file.container, Some(Container::Mp4));
        assert_eq!(file.video, Some(VideoCodec::H264));
        assert_eq!(file.format_id.as_deref(), Some("mp4"));
        assert_eq!(file.label.as_deref(), Some("360p"));
    }

    #[tokio::test]
    async fn the_page_fills_what_the_api_leaves_blank() {
        let mut fixture = Fixture::new("kompas", None);
        fixture.exchanges.push(get(STREAM_API, 200, "application/json", json!({
            "data": {
                "streams": [{"type": "MP4", "url": "https://video.jixie.media/1001/164474/164474_640x360.mp4", "width": 640, "height": 360}],
                "metadata": {"description": "", "thumbnails": ["https://video.jixie.media/1001/164474/164474_1280x720.jpg"]}
            }
        }).to_string()));
        let http = Http::replay(fixture);
        let origin = Url::parse("https://video.kompas.com/watch/164474/kim-jong-un").unwrap();
        let page = Page::parse(
            r#"<html><head><meta property="og:title" content="Kim Jong Un  Siap"><meta name="description" content="From the page"></head></html>"#,
            &origin,
        );
        let resolved = resolve_video(
            &http,
            "kompas",
            "164474",
            Some(PageMeta::of(&page)),
            &origin,
        )
        .await
        .unwrap();
        assert_eq!(resolved.resolver, "kompas");
        assert_eq!(resolved.title.as_deref(), Some("Kim Jong Un Siap"));
        assert_eq!(resolved.description.as_deref(), Some("From the page"));
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://video.jixie.media/1001/164474/164474_1280x720.jpg"
        );
        assert_eq!(resolved.webpage_url.as_ref(), Some(&origin));
        assert_eq!(resolved.variants.len(), 1);
    }

    #[tokio::test]
    async fn locked_videos_say_which_drm() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(STREAM_API, 200, "application/json", json!({
            "data": {"title": "locked", "drm": {"widevine": {"license": "https://drm.jixie.media/w"}},
                     "streams": [{"type": "HLS", "url": MASTER}], "metadata": {}}
        }).to_string()));
        playlists(&mut fixture, true);
        let resolver = JixieResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse(STREAM_API).unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Drm { system, .. } if system == "widevine"),
            "{error}"
        );

        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(STREAM_API, 200, "application/json", json!({
            "data": {"title": "half locked", "drm": true,
                     "streams": [{"type": "HLS", "url": MASTER},
                                 {"type": "MP4", "url": "https://video.jixie.media/1001/164474/164474_640x360.mp4", "width": 640, "height": 360}],
                     "metadata": {}}
        }).to_string()));
        playlists(&mut fixture, false);
        let resolver = JixieResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse(STREAM_API).unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.variants[0].drm.as_deref(), Some("drm"));
        assert_eq!(resolved.variants[1].drm, None);
        assert!(resolved.live);
    }

    #[tokio::test]
    async fn missing_videos_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://apiv.kompas.com/jixie-stream?format=hls&metadata=full&video_id=1",
            404,
            "application/json",
            json!({"success": false, "message": "data not found.", "data": null}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://apiv.kompas.com/jixie-stream?format=hls&metadata=full&video_id=2",
            200,
            "application/json",
            json!({"data": {}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://apiv.kompas.com/jixie-stream?format=hls&metadata=full&video_id=3",
            200,
            "application/json",
            json!({"data": {"streams": [{"type": "MP4"}], "metadata": {}}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://apiv.kompas.com/jixie-stream?format=hls&metadata=full&video_id=4",
            200,
            "application/json",
            json!({"success": false, "message": "data not found.", "data": null}).to_string(),
        ));
        let resolver = JixieResolver::new(Http::replay(fixture));
        let link = |id: &str| {
            Url::parse(&format!(
                "https://apidam.jixie.io/api/public/stream?video_id={id}"
            ))
            .unwrap()
        };
        assert!(matches!(
            resolver.resolve(&link("1")).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
        assert!(matches!(
            resolver.resolve(&link("2")).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver.resolve(&link("3")).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("no streams")),
            "{error}"
        );
        assert!(matches!(
            resolver.resolve(&link("4")).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
