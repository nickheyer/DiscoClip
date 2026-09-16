//! BYUtv (byutv.org) episodes, clips and shows, through the views and media APIs the
//! site's player calls: a content entry names its media, the media entry names the
//! Uplynk assets (HLS in the clear, DASH under Widevine), and a show page lists the
//! episodes it carries.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionSupport, Variant, clean_title, fetch, hls, navigation_headers,
    status_error, util,
};
use crate::http::{BROWSER_UA, Http};

pub const PLATFORM: &str = "byutv";
const SITE: &str = "https://www.byutv.org";
const VIEWS_API: &str = "https://api.byub.org/views/v2/public/content/";
const MEDIA_API: &str = "https://api.byub.org/media/v1/public/media/";
/// The web client's key and version, sent with every API call.
const CLIENT_KEY: &str = "byutv-web-dk94tsvophi";
const CLIENT_VERSION: &str = "5.69.0";

/// `/{id}`, `/watch/{id}` or `/player/{id}`, with an optional display slug.
static RE_CONTENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^/(?:(?:watch|player)/)?([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})(?:/([^/?#&]+))?",
    )
    .unwrap()
});
/// `/{show}`, or one of its tabs.
static RE_SHOW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^/([a-z0-9][a-z0-9-]*)(?:/(?:episodes|details|discover|extras|clips|seasons?[^/]*))?/?$")
        .unwrap()
});
/// Episode links on a show page.
static RE_EPISODE_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"href="/([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})/([^"/?#]+)""#)
        .unwrap()
});
static RE_ALT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"\balt="([^"]*)""#).unwrap());
/// `d.hh:mm:ss.fffffff`, how the API writes lengths and markers.
static RE_SPAN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:(\d+)\.)?(\d{1,2}):(\d{2}):(\d{2})(?:\.(\d+))?$").unwrap());
/// Site sections that are not shows.
const RESERVED: &[&str] = &[
    "shows", "live", "search", "account", "schedule", "browse", "settings", "login", "signup",
    "movies", "sports", "kids", "faq", "about", "contact", "privacy", "terms", "help", "apps",
    "watch", "player", "playback", "byutv", "images", "_nuxt", "_i18n", "api", "home", "news",
    "devotionals", "byu-sports", "on-demand", "collections", "showoffs",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// An episode, clip or recording, by its content id.
    Content {
        id: String,
        display_id: Option<String>,
    },
    /// A show page, listing its episodes.
    Show { slug: String },
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    if host != "byutv.org" {
        return None;
    }
    let path = url.path();
    if path.starts_with("/watch/event/") || path.starts_with("/player/event/") {
        return None;
    }
    if let Some(caps) = RE_CONTENT.captures(path) {
        return Some(Link::Content {
            id: caps[1].to_string(),
            display_id: caps.get(2).map(|m| m.as_str().to_string()),
        });
    }
    let caps = RE_SHOW.captures(path)?;
    let slug = caps[1].to_string();
    if RESERVED.contains(&slug.as_str()) {
        return None;
    }
    Some(Link::Show { slug })
}

/// A span such as `0.00:24:34.4320000` as a duration.
pub fn parse_span(text: &str) -> Option<Duration> {
    let caps = RE_SPAN.captures(text.trim())?;
    let days: u64 = caps.get(1).map_or(Ok(0), |m| m.as_str().parse()).ok()?;
    let hours: u64 = caps[2].parse().ok()?;
    let minutes: u64 = caps[3].parse().ok()?;
    let seconds: u64 = caps[4].parse().ok()?;
    let fraction = caps
        .get(5)
        .map(|m| {
            let digits = m.as_str();
            let value: f64 = digits.parse().unwrap_or(0.0);
            value / 10f64.powi(digits.len() as i32)
        })
        .unwrap_or(0.0);
    let whole = ((days * 24 + hours) * 60 + minutes) * 60 + seconds;
    Some(Duration::from_secs_f64(whole as f64 + fraction))
}

/// The first `value` of a display field such as `title` or `description`.
fn display_value(standard: &Value, field: &str) -> Option<String> {
    standard[field]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|entry| entry["value"].as_str().and_then(clean_title))
}

/// The media the content's play button targets: `(media id, stop marker)`.
fn media_target(standard: &Value) -> Option<(String, Option<Duration>)> {
    let groups = standard["buttonGroups"]["primary"].as_array()?;
    groups
        .iter()
        .flat_map(|group| group["buttons"].as_array().into_iter().flatten())
        .find_map(|button| {
            let target = &button["target"];
            if target["type"].as_str() != Some("media") {
                return None;
            }
            let id = util::text(&target["id"])?;
            let stop = target["stop"].as_str().and_then(parse_span);
            Some((id, stop))
        })
}

/// The episodes a show page links, in page order, each with the title its card shows.
pub fn show_episodes(html: &str) -> Vec<(String, String, Option<String>)> {
    let mut seen = std::collections::HashSet::new();
    let mut episodes = Vec::new();
    for caps in RE_EPISODE_LINK.captures_iter(html) {
        let id = caps[1].to_string();
        if !seen.insert(id.clone()) {
            continue;
        }
        let slug = caps[2].to_string();
        let after = &html[caps.get(0).unwrap().end()..];
        let window = &after[..after.len().min(4000)];
        let title = RE_ALT
            .captures(window)
            .and_then(|alt| clean_title(&util::html_unescape(&alt[1])));
        episodes.push((id, slug, title));
    }
    episodes
}

pub struct ByutvResolver {
    http: Http,
}

impl ByutvResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// One API call, with the client headers the site sends.
    async fn api(&self, url: &Url, origin: &Url) -> Result<Value, ResolveError> {
        let device = uuid::Uuid::new_v4().to_string();
        let session = uuid::Uuid::new_v4().to_string();
        let response = self
            .http
            .get(url.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "application/json")
            .header("accept-language", "en")
            .header("origin", SITE)
            .header("referer", &format!("{SITE}/"))
            .header("x-byub-client", CLIENT_KEY)
            .header("x-byub-clientversion", CLIENT_VERSION)
            .header("x-byub-location", "us")
            .header("x-byub-isauthenticated", "false")
            .header("x-byub-device", &device)
            .header("x-byub-session", &session)
            .header("x-byub-offset", "0")
            .send()
            .await?;
        if let Some(error) = status_error(response.status, origin) {
            return Err(error);
        }
        response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, format!("API JSON: {e}")))
    }

    async fn resolve_content(
        &self,
        id: &str,
        display_id: Option<&str>,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let content_url = Url::parse(&format!("{VIEWS_API}{id}")).expect("valid");
        let content = self.api(&content_url, url).await?;
        let standard = &content["display"]["standard"];
        let title = display_value(standard, "title");
        let description = display_value(standard, "description");
        let thumbnail = {
            let image = &standard["images"]["primary"];
            match (util::text(&image["baseUrl"]), util::text(&image["imageId"])) {
                (Some(base), Some(image_id)) => {
                    Url::parse(&format!("{}/{image_id}/1280x720.webp", base.trim_end_matches('/')))
                        .ok()
                }
                _ => None,
            }
        };
        let (media_id, stop) =
            media_target(standard).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let media_url = Url::parse(&format!("{MEDIA_API}{media_id}")).expect("valid");
        let media = self.api(&media_url, url).await?;

        let mut resolved = Resolved::new(PLATFORM);
        let mut failure = None;
        for asset in media["assets"].as_array().into_iter().flatten() {
            if asset["multimediaType"].as_str().is_some_and(|kind| kind != "video") {
                continue;
            }
            let Some(asset_url) = util::url_of(&asset["url"], None) else {
                continue;
            };
            let language = util::text(&asset["language"]);
            match asset["assetType"].as_str().unwrap_or("") {
                kind if kind.starts_with("hls") => {
                    match hls::expand(&self.http, &asset_url, PLATFORM, BROWSER_UA, &[]).await {
                        Ok(expanded) => {
                            for mut variant in expanded.variants {
                                variant.format_id = Some(format!(
                                    "hls-{}",
                                    variant.label.clone().unwrap_or_default()
                                ));
                                variant.language = variant.language.take().or(language.clone());
                                resolved.variants.push(variant);
                            }
                            resolved.subtitles.extend(expanded.subtitles);
                            resolved.live |= expanded.live;
                            resolved.duration = resolved.duration.or(expanded.duration);
                        }
                        Err(error) => failure = Some(error),
                    }
                }
                kind if kind.starts_with("dash") => {
                    // The DASH manifest is served under Widevine (`rmt=wv`); the HLS
                    // rendition of the same asset plays in the clear. A clear manifest
                    // expands into its representations.
                    let mut variant = Variant::dash(asset_url.clone());
                    variant.format_id = Some(kind.to_string());
                    variant.language = language.clone();
                    if util::query_param(&asset_url, "rmt").as_deref() == Some("wv") {
                        variant.drm = Some("widevine".to_string());
                        resolved.variants.push(variant);
                    } else {
                        let mut subtitles = Vec::new();
                        for mut representation in super::manifests::expand_all(
                            &self.http,
                            PLATFORM,
                            vec![variant],
                            &mut subtitles,
                            resolved.duration,
                        )
                        .await
                        {
                            representation.language = representation.language.take().or(language.clone());
                            resolved.variants.push(representation);
                        }
                        resolved.subtitles.extend(subtitles);
                    }
                }
                // Any other asset is a file of its own.
                kind => {
                    let mut variant = Variant::file(asset_url.clone());
                    variant.container = super::path_extension(&asset_url)
                        .and_then(|ext| crate::media::Container::from_extension(&ext))
                        .or(Some(crate::media::Container::Mp4));
                    variant.video = Some(crate::media::VideoCodec::H264);
                    variant.audio = Some(crate::media::AudioCodec::Aac);
                    variant.format_id = Some(if kind.is_empty() { "file".to_string() } else { kind.to_string() });
                    variant.language = language;
                    resolved.variants.push(variant);
                }
            }
        }
        if resolved.variants.is_empty() {
            return Err(failure.unwrap_or_else(|| ResolveError::NotFound(url.clone())));
        }
        resolved.id = Some(id.to_string());
        resolved.title = title.or_else(|| display_id.and_then(clean_title));
        resolved.description = description;
        resolved.thumbnail = thumbnail;
        resolved.duration = media["length"]
            .as_str()
            .and_then(parse_span)
            .or(stop)
            .or(resolved.duration);
        resolved.uploaded_at = util::time(&media["programStart"]);
        resolved.webpage_url = Url::parse(&match display_id {
            Some(slug) => format!("{SITE}/{id}/{slug}"),
            None => format!("{SITE}/{id}"),
        })
        .ok();
        Ok(Resolution::from(resolved))
    }

    async fn resolve_show(&self, slug: &str, url: &Url) -> Result<Resolution, ResolveError> {
        let page_url = Url::parse(&format!("{SITE}/{slug}")).expect("valid");
        let fetched = fetch(
            &self.http,
            &page_url,
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
        let episodes = show_episodes(&html);
        if episodes.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let page = Page::parse(&html, &page_url);
        let title = page
            .ld_json()
            .iter()
            .find(|ld| ld["@type"].as_str() == Some("TVSeries"))
            .and_then(|ld| util::text(&ld["name"]))
            .or_else(|| {
                page.meta("og:title")
                    .map(|t| t.trim_end_matches(" - BYUtv").to_string())
            })
            .and_then(|t| clean_title(&t));
        let entries = episodes
            .into_iter()
            .filter_map(|(id, episode_slug, episode_title)| {
                Some(PlaylistEntry {
                    url: Url::parse(&format!("{SITE}/{id}/{episode_slug}")).ok()?,
                    title: episode_title.or_else(|| clean_title(&episode_slug.replace('-', " "))),
                    duration: None,
                })
            })
            .collect::<Vec<_>>();
        let total = entries.len();
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(slug.to_string()),
            title,
            entries,
            total: Some(total),
        }))
    }
}

#[async_trait]
impl Resolver for ByutvResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "BYUtv",
            hosts: &["byutv.org"],
            features: &["videos", "recordings", "shows"],
            formats: &["hls", "dash"],
            session: SessionSupport::None,
            examples: &[
                "https://www.byutv.org/0160476a-bfd0-425d-82f9-5757bde3bf37/studio-c-season-9-episode-2",
                "https://www.byutv.org/watch/0160476a-bfd0-425d-82f9-5757bde3bf37/studio-c-season-9-episode-2",
                "https://www.byutv.org/studio-c",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Content { id, display_id } => {
                self.resolve_content(&id, display_id.as_deref(), url).await
            }
            Link::Show { slug } => self.resolve_show(&slug, url).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};
    use serde_json::json;

    const CONTENT_ID: &str = "0160476a-bfd0-425d-82f9-5757bde3bf37";
    const MEDIA_ID: &str = "8e5ccd03-f760-502e-980d-d67e88e43378";

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

    fn content_json() -> String {
        json!({"display": {"standard": {
            "title": [{"value": "Season 9 Episode 2"}],
            "description": [{"value": "Two security guards fight insecurity."}],
            "primaryInfo": [{"value": "Season 9, Episode 2"}],
            "images": {"primary": {"baseUrl": "https://assets.byub.org/images", "imageId": "d247bf51-b8c9-44d8-b4ec-8afc222cbbd2"}},
            "buttonGroups": {"primary": [{"buttons": [
                {"text": [{"value": "add"}], "target": {"type": "list", "id": "x"}},
                {"text": [{"value": "play"}], "target": {"type": "media", "id": MEDIA_ID, "contentId": CONTENT_ID, "start": "0.00:00:00.0000000", "stop": "0.00:24:34.4320000"}}
            ]}]}
        }}}).to_string()
    }

    fn media_json() -> String {
        json!({
            "videoId": "F0504154", "id": MEDIA_ID, "length": "0.00:24:34.4320000",
            "programStart": "2020-04-01T06:00:00.000Z",
            "assets": [
                {"multimediaType": "video", "provider": "verizon", "language": "en", "assetType": "dash1", "url": "https://content.uplynk.test/ext/o/e.mpd?rmt=wv&manifest=mpd"},
                {"multimediaType": "video", "provider": "verizon", "language": "en", "assetType": "hls1", "url": "https://content.uplynk.test/ext/o/e.m3u8?tc=1"},
                {"multimediaType": "audio", "assetType": "hls1", "url": "https://content.uplynk.test/ext/o/a.m3u8"}
            ],
            "transcripts": [{"url": "https://media.byub.org/x.json", "language": "en"}]
        })
        .to_string()
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let content = |id: &str, slug: Option<&str>| {
            Some(Link::Content {
                id: id.into(),
                display_id: slug.map(String::from),
            })
        };
        assert_eq!(
            link("https://www.byutv.org/0160476a-bfd0-425d-82f9-5757bde3bf37/season-9-episode-2"),
            content(CONTENT_ID, Some("season-9-episode-2"))
        );
        assert_eq!(
            link("http://www.byutv.org/watch/0160476a-bfd0-425d-82f9-5757bde3bf37/studio-c-season-9-episode-2"),
            content(CONTENT_ID, Some("studio-c-season-9-episode-2"))
        );
        assert_eq!(
            link("https://www.byutv.org/player/0160476a-bfd0-425d-82f9-5757bde3bf37"),
            content(CONTENT_ID, None)
        );
        assert_eq!(
            link("https://www.byutv.org/player/27741493-dc83-40b0-8420-e7ae38a2ae98/byu-football?listid=4fe0fee5&q=toledo"),
            content("27741493-dc83-40b0-8420-e7ae38a2ae98", Some("byu-football"))
        );
        assert_eq!(
            link("https://www.byutv.org/studio-c"),
            Some(Link::Show {
                slug: "studio-c".into()
            })
        );
        assert_eq!(
            link("https://byutv.org/studio-c/episodes"),
            Some(Link::Show {
                slug: "studio-c".into()
            })
        );
        assert_eq!(link("https://www.byutv.org/shows/comedy"), None);
        assert_eq!(link("https://www.byutv.org/live"), None);
        assert_eq!(link("https://www.byutv.org/watch/event/abc"), None);
        assert_eq!(link("https://example.com/studio-c"), None);
    }

    #[test]
    fn spans_are_read() {
        assert_eq!(
            parse_span("0.00:24:34.4320000"),
            Some(Duration::from_secs_f64(1474.432))
        );
        assert_eq!(parse_span("01:02:03"), Some(Duration::from_secs(3723)));
        assert_eq!(
            parse_span("1.02:00:00.5"),
            Some(Duration::from_secs_f64(93600.5))
        );
        assert_eq!(parse_span("PT24M"), None);
    }

    #[test]
    fn show_pages_list_their_episodes_once() {
        let html = r#"<a href="/bea34984-8997-4542-8bb0-ac0f702ca94e/dont-get-on-that-plane"><div><picture><img src="x.webp" alt="Don&#39;t Get on That Plane"></picture></div></a>
            <a href="/bea34984-8997-4542-8bb0-ac0f702ca94e/dont-get-on-that-plane">again</a>
            <a href="/0160476a-bfd0-425d-82f9-5757bde3bf37/season-9-episode-2"><img alt="Season 9 Episode 2"></a>
            <a href="/studio-c/episodes">tab</a>"#;
        assert_eq!(
            show_episodes(html),
            vec![
                (
                    "bea34984-8997-4542-8bb0-ac0f702ca94e".into(),
                    "dont-get-on-that-plane".into(),
                    Some("Don't Get on That Plane".into())
                ),
                (
                    CONTENT_ID.into(),
                    "season-9-episode-2".into(),
                    Some("Season 9 Episode 2".into())
                ),
            ]
        );
    }

    #[tokio::test]
    async fn episodes_resolve_through_the_media_api() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            &format!("{VIEWS_API}{CONTENT_ID}"),
            200,
            "application/json",
            content_json(),
        ));
        fixture.exchanges.push(get(
            &format!("{MEDIA_API}{MEDIA_ID}"),
            200,
            "application/json",
            media_json(),
        ));
        fixture.exchanges.push(get(
            "https://content.uplynk.test/ext/o/e.m3u8?tc=1",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=3227093,RESOLUTION=1280x720\nhttps://content.uplynk.test/o/f.m3u8\n".into(),
        ));
        fixture.exchanges.push(get(
            "https://content.uplynk.test/o/f.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:6.0,\n0.ts\n#EXT-X-ENDLIST\n".into(),
        ));
        let resolver = ByutvResolver::new(Http::replay(fixture));
        let url = Url::parse(&format!("https://www.byutv.org/{CONTENT_ID}/season-9-episode-2")).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some(CONTENT_ID));
        assert_eq!(resolved.title.as_deref(), Some("Season 9 Episode 2"));
        assert_eq!(
            resolved.description.as_deref(),
            Some("Two security guards fight insecurity.")
        );
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://assets.byub.org/images/d247bf51-b8c9-44d8-b4ec-8afc222cbbd2/1280x720.webp"
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(1474.432)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1585720800)
        );
        assert_eq!(
            resolved.webpage_url.as_ref().unwrap().as_str(),
            "https://www.byutv.org/0160476a-bfd0-425d-82f9-5757bde3bf37/season-9-episode-2"
        );
        assert_eq!(resolved.variants.len(), 2);
        let dash = resolved
            .variants
            .iter()
            .find(|v| v.format_id.as_deref() == Some("dash1"))
            .unwrap();
        assert_eq!(dash.drm.as_deref(), Some("widevine"));
        let playable = resolved.variants.iter().find(|v| v.is_playable()).unwrap();
        assert_eq!(playable.format_id.as_deref(), Some("hls-720p"));
        assert_eq!(playable.height, Some(720));
        assert_eq!(playable.language.as_deref(), Some("en"));
    }

    #[tokio::test]
    async fn shows_resolve_to_their_episodes() {
        let html = r#"<html><head><meta property="og:title" content="Studio C - BYUtv"><script type="application/ld+json">{"@context":"https://schema.org","@type":"TVSeries","@id":"https://www.byutv.org/studio-c","name":"Studio C"}</script></head><body>
            <a href="/bea34984-8997-4542-8bb0-ac0f702ca94e/dont-get-on-that-plane"><img alt="Don't Get on That Plane"></a>
            <a href="/0160476a-bfd0-425d-82f9-5757bde3bf37/season-9-episode-2"><img alt="Season 9 Episode 2"></a>
            </body></html>"#;
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.byutv.org/studio-c",
            200,
            "text/html",
            html.into(),
        ));
        fixture.exchanges.push(get(
            "https://www.byutv.org/no-such-show",
            404,
            "text/html",
            "<html>gone</html>".into(),
        ));
        let resolver = ByutvResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.byutv.org/studio-c/episodes").unwrap();
        let Resolution::Playlist(playlist) = resolver.resolve(&url).await.unwrap() else {
            panic!("a playlist");
        };
        assert_eq!(playlist.title.as_deref(), Some("Studio C"));
        assert_eq!(playlist.id.as_deref(), Some("studio-c"));
        assert_eq!(playlist.total, Some(2));
        assert_eq!(
            playlist.entries[1].url.as_str(),
            "https://www.byutv.org/0160476a-bfd0-425d-82f9-5757bde3bf37/season-9-episode-2"
        );
        assert_eq!(
            playlist.entries[0].title.as_deref(),
            Some("Don't Get on That Plane")
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.byutv.org/no-such-show").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn unknown_content_and_content_without_media_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            &format!("{VIEWS_API}00000000-0000-0000-0000-000000000000"),
            404,
            "application/json",
            json!({"errors": [{"message": "not found"}]}).to_string(),
        ));
        fixture.exchanges.push(get(
            &format!("{VIEWS_API}11111111-1111-1111-1111-111111111111"),
            200,
            "application/json",
            json!({"display": {"standard": {"title": [{"value": "A show"}], "buttonGroups": {"primary": [{"buttons": [{"target": {"type": "page", "id": "x"}}]}]}}}}).to_string(),
        ));
        let resolver = ByutvResolver::new(Http::replay(fixture));
        let resolve = |id: &str| {
            let url = Url::parse(&format!("https://www.byutv.org/{id}")).unwrap();
            let resolver = &resolver;
            async move { resolver.resolve(&url).await.unwrap_err() }
        };
        assert!(matches!(
            resolve("00000000-0000-0000-0000-000000000000").await,
            ResolveError::NotFound(_)
        ));
        assert!(matches!(
            resolve("11111111-1111-1111-1111-111111111111").await,
            ResolveError::NotFound(_)
        ));
    }
}
