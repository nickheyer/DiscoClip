//! Periscope broadcasts, replays and user pages, through the public API the web player
//! calls; the base Twitter's broadcasts build on.

use std::sync::LazyLock;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionSupport, SubtitleTrack, Variant, VariantKind, clean_title, fetch, fetch_ok,
    hls, navigation_headers, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::Container;

pub const PLATFORM: &str = "periscope";
/// The API the web player calls.
const API: &str = "https://api.periscope.tv/api/v2/";
/// The site, sent as the referer of every playlist and segment request.
pub const SITE: &str = "https://www.periscope.tv/";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A broadcast, live or replayed, by its token: `/{user}/{token}` or `/w/{token}`.
    Broadcast(String),
    /// A user's page, listing the broadcasts of the last day: `/{user}`.
    User(String),
}

/// `<iframe src="//www.periscope.tv/…">` players.
static RE_EMBED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)<iframe[^>]+src=(?:"((?:https?:)?//(?:www\.)?(?:periscope|pscp)\.tv/[^"]+)"|'((?:https?:)?//(?:www\.)?(?:periscope|pscp)\.tv/[^']+)')"#,
    )
    .unwrap()
});
/// The `data-store` attribute a user page keeps its state in.
static RE_DATA_STORE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-store=(?:"([^"]+)"|'([^']+)')"#).unwrap());

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    if host != "periscope.tv" && host != "pscp.tv" {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        [user] => Some(Link::User(util::url_decode(user))),
        [_, token, ..] => Some(Link::Broadcast(util::url_decode(token))),
        [] => None,
    }
}

/// Where a broadcast is in its life, from its `state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveStatus {
    Live,
    Upcoming,
    WasLive,
}

/// The broadcast's state in lower case, with its width and height:
/// `_extract_common_format_info`.
pub fn format_info(broadcast: &Value) -> (String, Option<u32>, Option<u32>) {
    let state = broadcast["state"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let size = |key: &str| util::u32_of(&broadcast[key]).filter(|n| *n > 0);
    (state, size("width"), size("height"))
}

pub fn live_status(broadcast: &Value) -> LiveStatus {
    match format_info(broadcast).0.as_str() {
        "running" => LiveStatus::Live,
        "not_started" => LiveStatus::Upcoming,
        _ => LiveStatus::WasLive,
    }
}

/// When the broadcast was, or is, scheduled to start.
pub fn scheduled_start(broadcast: &Value) -> Option<Timestamp> {
    ["scheduled_start_ms", "start_ms"]
        .iter()
        .find_map(|key| util::int(&broadcast[*key]))
        .filter(|ms| *ms > 0)
        .and_then(|ms| Timestamp::from_millisecond(ms).ok())
}

/// What a broadcast record says about itself, as `_parse_broadcast_data` reads it: its
/// id, its status line as the title, who broadcast it, when, its picture and whether it
/// is running. `platform` names the resolver the answer belongs to.
pub fn broadcast_info(platform: &str, broadcast: &Value, display_id: &str) -> Resolved {
    let text = |key: &str| {
        broadcast[key]
            .as_str()
            .map(util::clean_html)
            .and_then(|t| clean_title(&t))
    };
    let mut resolved = Resolved::new(platform);
    resolved.id = util::text(&broadcast["id"])
        .filter(|id| !id.is_empty())
        .or_else(|| Some(display_id.to_string()));
    resolved.title = text("status");
    resolved.uploader = text("user_display_name");
    resolved.uploader_url =
        text("username").and_then(|name| Url::parse(&format!("{SITE}{name}")).ok());
    resolved.uploaded_at = broadcast["created_at"]
        .as_str()
        .and_then(util::parse_timestamp)
        .or_else(|| {
            util::int(&broadcast["created_at_ms"])
                .filter(|ms| *ms > 0)
                .and_then(|ms| Timestamp::from_millisecond(ms).ok())
        });
    resolved.thumbnail = ["image_url", "image_url_medium", "image_url_small"]
        .iter()
        .find_map(|key| util::url_of(&broadcast[*key], None));
    resolved.live = live_status(broadcast) == LiveStatus::Live;
    resolved
}

/// The headers every playlist and segment request carries.
pub fn m3u8_headers() -> Vec<(String, String)> {
    vec![("referer".to_string(), SITE.to_string())]
}

/// Expands one of a broadcast's HLS playlists as `_extract_pscp_m3u8_formats`: the site
/// goes as referer with every request, a lone variant takes the broadcast's size, and a
/// running broadcast's variants are live. `format_id` names the playlist (`replay`,
/// `hls`, …) and prefixes each variant's id.
pub async fn m3u8_variants(
    http: &Http,
    platform: &str,
    playlist: &Url,
    format_id: &str,
    state: &str,
    width: Option<u32>,
    height: Option<u32>,
) -> Result<hls::Expanded, ResolveError> {
    let mut expanded = hls::expand(http, playlist, platform, BROWSER_UA, &m3u8_headers()).await?;
    if let [only] = expanded.variants.as_mut_slice() {
        if only.width.is_none() {
            only.width = width;
        }
        if only.height.is_none() {
            only.height = height;
        }
    }
    let running = state == "running";
    for variant in &mut expanded.variants {
        variant.format_id = Some(match variant.bitrate {
            Some(bitrate) => format!("{format_id}-{}", bitrate / 1000),
            None => format_id.to_string(),
        });
        variant.live |= running;
    }
    expanded.live |= running;
    Ok(expanded)
}

/// GETs `https://api.periscope.tv/api/v2/{method}` with `query`, as the web player does.
pub async fn call_api(
    http: &Http,
    method: &str,
    query: &[(&str, &str)],
    origin: &Url,
) -> Result<Value, ResolveError> {
    let mut url = Url::parse(&format!("{API}{method}")).expect("valid");
    url.query_pairs_mut().extend_pairs(query);
    let fetched = fetch(http, &url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
    if let Some(error) = status_error(fetched.status, origin) {
        return Err(error);
    }
    fetched.json(origin)
}

/// A broadcast by its token, as the web player reads it: `accessVideoPublic`, then every
/// playlist the answer names (`replay`, `hls`, `https_hls`, `lhls`, `lhlsweb`) and its
/// RTMP stream.
pub async fn resolve_broadcast(
    http: &Http,
    token: &str,
    origin: &Url,
) -> Result<Resolved, ResolveError> {
    let stream = call_api(
        http,
        "accessVideoPublic",
        &[("broadcast_id", token)],
        origin,
    )
    .await?;
    let broadcast = &stream["broadcast"];
    if !broadcast.is_object() {
        return Err(ResolveError::malformed(
            origin,
            "the API answered without a broadcast",
        ));
    }
    let mut resolved = broadcast_info(PLATFORM, broadcast, token);
    if live_status(broadcast) == LiveStatus::Upcoming {
        let when = scheduled_start(broadcast)
            .map(|start| format!(", scheduled for {start}"))
            .unwrap_or_default();
        return Err(ResolveError::unavailable(
            origin,
            format!("the broadcast has not started{when}"),
        ));
    }
    let (state, width, height) = format_info(broadcast);
    let mut seen: Vec<String> = Vec::new();
    let mut variants: Vec<Variant> = Vec::new();
    let mut subtitles: Vec<SubtitleTrack> = Vec::new();
    let mut duration = None;
    let mut failure: Option<ResolveError> = None;
    for format_id in ["replay", "rtmp", "hls", "https_hls", "lhls", "lhlsweb"] {
        let Some(video_url) = stream[format!("{format_id}_url")]
            .as_str()
            .map(str::trim)
            .filter(|u| !u.is_empty())
        else {
            continue;
        };
        if seen.iter().any(|s| s == video_url) {
            continue;
        }
        seen.push(video_url.to_string());
        let Ok(url) = Url::parse(video_url) else {
            continue;
        };
        if format_id == "rtmp" {
            let mut variant = Variant::new(url, VariantKind::Rtmp);
            variant.container = Some(Container::Flv);
            variant.width = width;
            variant.height = height;
            variant.format_id = Some("rtmp".to_string());
            variant.live = state == "running";
            variants.push(variant);
            continue;
        }
        match m3u8_variants(http, PLATFORM, &url, format_id, &state, width, height).await {
            Ok(expanded) => {
                variants.extend(expanded.variants);
                subtitles.extend(expanded.subtitles);
                duration = duration.or(expanded.duration);
            }
            Err(error) => failure = Some(error),
        }
    }
    if variants.is_empty() {
        return Err(match failure {
            Some(error) => ResolveError::unavailable(
                origin,
                format!("none of the broadcast's playlists could be read: {error}"),
            ),
            None => ResolveError::unavailable(origin, "the broadcast offers no stream"),
        });
    }
    resolved.duration = duration;
    resolved.variants = variants;
    resolved.subtitles = subtitles;
    resolved.webpage_url = Url::parse(&format!("{SITE}w/{token}")).ok();
    Ok(resolved)
}

/// The JSON a user page keeps in its `data-store` attribute.
pub fn data_store(html: &str) -> Option<Value> {
    let caps = RE_DATA_STORE.captures(html)?;
    let raw = caps.get(1).or_else(|| caps.get(2))?.as_str();
    serde_json::from_str(&util::html_unescape(raw)).ok()
}

/// Periscope players framed in another site's page, as broadcast or user links.
pub fn embed_urls(html: &str, page: Option<&Url>) -> Vec<Url> {
    let mut found: Vec<Url> = Vec::new();
    for caps in RE_EMBED.captures_iter(html) {
        let Some(raw) = caps.get(1).or_else(|| caps.get(2)) else {
            continue;
        };
        let unescaped = util::html_unescape(raw.as_str());
        if let Some(url) = util::join_url(page, &unescaped)
            && parse_link(&url).is_some()
            && !found.contains(&url)
        {
            found.push(url);
        }
    }
    found
}

pub struct PeriscopeResolver {
    http: Http,
}

impl PeriscopeResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// A user's page: the user and the session token its state carries, then the
    /// broadcasts `getUserBroadcastsPublic` lists for them.
    async fn resolve_user(&self, user_name: &str, url: &Url) -> Result<Resolution, ResolveError> {
        let fetched = fetch_ok(
            &self.http,
            url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        let store = data_store(&fetched.text())
            .ok_or_else(|| ResolveError::malformed(url, "the page carries no data store"))?;
        let user = store["UserCache"]["users"]
            .as_object()
            .and_then(|users| users.values().next())
            .map(|entry| &entry["user"])
            .filter(|user| user.is_object())
            .ok_or_else(|| ResolveError::malformed(url, "the data store names no user"))?;
        let user_id = util::text(&user["id"])
            .filter(|id| !id.is_empty())
            .ok_or_else(|| ResolveError::malformed(url, "the user has no id"))?;
        let session_id =
            util::text(&store["SessionToken"]["public"]["broadcastHistory"]["token"]["session_id"])
                .filter(|id| !id.is_empty())
                .ok_or_else(|| {
                    ResolveError::malformed(url, "the data store carries no session token")
                })?;
        let answer = call_api(
            &self.http,
            "getUserBroadcastsPublic",
            &[("user_id", &user_id), ("session_id", &session_id)],
            url,
        )
        .await?;
        let entries = answer["broadcasts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|broadcast| {
                let id = util::text(&broadcast["id"]).filter(|id| !id.is_empty())?;
                Some(PlaylistEntry {
                    url: Url::parse(&format!("{SITE}{user_name}/{id}")).ok()?,
                    title: broadcast["status"]
                        .as_str()
                        .map(util::clean_html)
                        .and_then(|t| clean_title(&t)),
                    duration: None,
                })
            })
            .collect();
        let title = ["display_name", "username"]
            .iter()
            .find_map(|key| user[*key].as_str().and_then(clean_title))
            .or_else(|| clean_title(user_name));
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(user_id),
            title,
            entries,
            total: None,
        }))
    }
}

#[async_trait]
impl Resolver for PeriscopeResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Periscope",
            hosts: &["periscope.tv", "pscp.tv"],
            features: &["broadcasts", "replays", "live", "user pages", "embeds"],
            formats: &["hls", "rtmp"],
            session: SessionSupport::None,
            examples: &[
                "https://www.periscope.tv/LularoeHusbandMike/1mrGmgaXAVqxy",
                "https://www.periscope.tv/LularoeHusbandMike/",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    fn embeds_in(&self, page: &Page) -> Vec<Url> {
        embed_urls(page.html(), Some(page.url()))
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Broadcast(token) => resolve_broadcast(&self.http, &token, url)
                .await
                .map(Resolution::from),
            Link::User(user_name) => self.resolve_user(&user_name, url).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};
    use serde_json::json;
    use std::time::Duration;

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

    const MASTER: &str = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1200000,CODECS=\"avc1.64001f,mp4a.40.2\"\nhttps://replay.pscp.tv/x/chunked/playlist.m3u8\n";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:3.0,\n0.ts\n#EXTINF:3.0,\n1.ts\n#EXT-X-ENDLIST\n";
    const LIVE_MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:3.0,\n0.ts\n";

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://www.periscope.tv/LularoeHusbandMike/1mrGmgaXAVqxy"),
            Some(Link::Broadcast("1mrGmgaXAVqxy".into()))
        );
        assert_eq!(
            link("https://www.periscope.tv/w/1ZkKzPbMVggJv"),
            Some(Link::Broadcast("1ZkKzPbMVggJv".into()))
        );
        assert_eq!(
            link("https://www.periscope.tv/bastaakanoggano/1OdKrlkZZjOJX"),
            Some(Link::Broadcast("1OdKrlkZZjOJX".into()))
        );
        assert_eq!(
            link("https://pscp.tv/w/1ZkKzPbMVggJv?t=1"),
            Some(Link::Broadcast("1ZkKzPbMVggJv".into()))
        );
        assert_eq!(
            link("https://www.periscope.tv/LularoeHusbandMike/"),
            Some(Link::User("LularoeHusbandMike".into()))
        );
        assert_eq!(
            link("http://periscope.tv/LularoeHusbandMike"),
            Some(Link::User("LularoeHusbandMike".into()))
        );
        assert_eq!(link("https://www.periscope.tv/"), None);
        assert_eq!(link("https://example.com/w/1ZkKzPbMVggJv"), None);
        assert_eq!(link("ftp://www.periscope.tv/w/1ZkKzPbMVggJv"), None);
    }

    #[tokio::test]
    async fn replays_resolve_with_the_site_as_referer() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://api.periscope.tv/api/v2/accessVideoPublic?broadcast_id=1mrGmgaXAVqxy",
            200,
            "application/json",
            json!({
                "broadcast": {
                    "id": "1mrGmgaXAVqxy", "state": "ENDED", "width": 720, "height": 1280,
                    "status": "🎉 BROWSE OUR ENTIRE <b>INVENTORY</b>! #lularoe",
                    "created_at": "2017-06-28T03:52:32.000Z", "start_ms": 1498621960000i64,
                    "user_display_name": "LuLaRoe Husband Mike", "username": "LularoeHusbandMike",
                    "image_url_small": "https://prod.video.pscp.tv/small.jpg",
                    "image_url": "https://prod.video.pscp.tv/big.jpg"
                },
                "replay_url": "https://replay.pscp.tv/x/master.m3u8",
                "hls_url": "https://replay.pscp.tv/x/master.m3u8"
            })
            .to_string(),
        ));
        fixture.exchanges.push(get(
            "https://replay.pscp.tv/x/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MASTER.into(),
        ));
        fixture.exchanges.push(get(
            "https://replay.pscp.tv/x/chunked/playlist.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MEDIA.into(),
        ));
        let resolver = PeriscopeResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.periscope.tv/LularoeHusbandMike/1mrGmgaXAVqxy").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("1mrGmgaXAVqxy"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("🎉 BROWSE OUR ENTIRE INVENTORY! #lularoe")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("LuLaRoe Husband Mike"));
        assert_eq!(
            resolved.uploader_url.as_ref().map(Url::as_str),
            Some("https://www.periscope.tv/LularoeHusbandMike")
        );
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1498621952)
        );
        assert_eq!(
            resolved.thumbnail.as_ref().map(Url::as_str),
            Some("https://prod.video.pscp.tv/big.jpg")
        );
        assert!(!resolved.live);
        assert_eq!(resolved.duration, Some(Duration::from_secs(6)));
        assert_eq!(resolved.variants.len(), 1);
        let variant = &resolved.variants[0];
        assert_eq!(variant.kind, VariantKind::Hls);
        assert_eq!((variant.width, variant.height), (Some(720), Some(1280)));
        assert_eq!(variant.format_id.as_deref(), Some("replay-1200"));
        assert!(!variant.live);
        assert!(
            variant
                .headers
                .iter()
                .any(|(k, v)| k == "referer" && v == SITE)
        );
        assert_eq!(
            resolved.webpage_url.as_ref().map(Url::as_str),
            Some("https://www.periscope.tv/w/1mrGmgaXAVqxy")
        );
    }

    #[tokio::test]
    async fn running_broadcasts_are_live_with_their_rtmp_stream() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://api.periscope.tv/api/v2/accessVideoPublic?broadcast_id=1ZkKzPbMVggJv",
            200,
            "application/json",
            json!({
                "broadcast": {"id": 77, "state": "RUNNING", "width": 540, "height": 960, "status": "live now",
                              "created_at_ms": 1498621952000i64, "username": "someone"},
                "rtmp_url": "rtmp://live.pscp.tv/live/1ZkKzPbMVggJv",
                "hls_url": "https://live.pscp.tv/x/master.m3u8",
                "https_hls_url": "https://live.pscp.tv/x/master.m3u8",
                "lhls_url": ""
            })
            .to_string(),
        ));
        fixture.exchanges.push(get(
            "https://live.pscp.tv/x/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            LIVE_MEDIA.into(),
        ));
        let resolver = PeriscopeResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://www.periscope.tv/w/1ZkKzPbMVggJv").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("77"));
        assert!(resolved.live);
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.variants.len(), 2);
        let rtmp = &resolved.variants[0];
        assert_eq!(rtmp.kind, VariantKind::Rtmp);
        assert_eq!(rtmp.container, Some(Container::Flv));
        assert_eq!((rtmp.width, rtmp.height), (Some(540), Some(960)));
        assert!(rtmp.live);
        let hls = &resolved.variants[1];
        assert_eq!(hls.kind, VariantKind::Hls);
        assert_eq!(hls.format_id.as_deref(), Some("hls"));
        assert!(hls.live);
        assert_eq!((hls.width, hls.height), (Some(540), Some(960)));
    }

    #[tokio::test]
    async fn upcoming_missing_and_streamless_broadcasts_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://api.periscope.tv/api/v2/accessVideoPublic?broadcast_id=soon",
            200,
            "application/json",
            json!({"broadcast": {"id": "soon", "state": "NOT_STARTED", "scheduled_start_ms": 1498621952000i64}})
                .to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.periscope.tv/api/v2/accessVideoPublic?broadcast_id=gone",
            404,
            "application/json",
            json!({"error": "not found"}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.periscope.tv/api/v2/accessVideoPublic?broadcast_id=bare",
            200,
            "application/json",
            json!({"broadcast": {"id": "bare", "state": "ENDED"}}).to_string(),
        ));
        let resolver = PeriscopeResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://www.periscope.tv/w/soon").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("not started") && reason.contains("2017-06-28")),
            "{error}"
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.periscope.tv/w/gone").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(&Url::parse("https://www.periscope.tv/w/bare").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("no stream")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn user_pages_list_their_broadcasts() {
        let store = json!({
            "UserCache": {"users": {"1234": {"user": {"id": "1234", "display_name": "LULAROE HUSBAND MIKE", "username": "LularoeHusbandMike", "description": "clothes"}}}},
            "SessionToken": {"public": {"broadcastHistory": {"token": {"session_id": "sess-1"}}}}
        })
        .to_string()
        .replace('"', "&quot;");
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.periscope.tv/LularoeHusbandMike/",
            200,
            "text/html",
            format!("<html><body><div id=\"root\" data-store=\"{store}\"></div></body></html>"),
        ));
        fixture.exchanges.push(get(
            "https://api.periscope.tv/api/v2/getUserBroadcastsPublic?user_id=1234&session_id=sess-1",
            200,
            "application/json",
            json!({"broadcasts": [{"id": "1mrGmgaXAVqxy", "status": "inventory"}, {"id": "", "status": "no id"}, {"id": "1OdKrlkZZjOJX"}]})
                .to_string(),
        ));
        let resolver = PeriscopeResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.periscope.tv/LularoeHusbandMike/").unwrap();
        let Resolution::Playlist(playlist) = resolver.resolve(&url).await.unwrap() else {
            panic!("a user page is a playlist");
        };
        assert_eq!(playlist.id.as_deref(), Some("1234"));
        assert_eq!(playlist.title.as_deref(), Some("LULAROE HUSBAND MIKE"));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://www.periscope.tv/LularoeHusbandMike/1mrGmgaXAVqxy"
        );
        assert_eq!(playlist.entries[0].title.as_deref(), Some("inventory"));
        assert_eq!(
            playlist.entries[1].url.as_str(),
            "https://www.periscope.tv/LularoeHusbandMike/1OdKrlkZZjOJX"
        );

        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.periscope.tv/nobody/",
            200,
            "text/html",
            "<html><body>nothing here</body></html>".into(),
        ));
        let resolver = PeriscopeResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://www.periscope.tv/nobody/").unwrap())
            .await
            .unwrap_err();
        assert!(matches!(error, ResolveError::Malformed { .. }), "{error}");
    }

    #[test]
    fn framed_players_are_found() {
        let page = Url::parse("https://example.com/post").unwrap();
        let html = r#"<p>watch</p>
            <iframe src="//www.periscope.tv/w/1ZkKzPbMVggJv" width="640"></iframe>
            <iframe src='https://pscp.tv/someone/1OdKrlkZZjOJX?autoplay=1&amp;muted=1'></iframe>
            <iframe src="https://www.youtube.com/embed/abc"></iframe>
            <iframe src="//www.periscope.tv/w/1ZkKzPbMVggJv"></iframe>"#;
        let found = embed_urls(html, Some(&page));
        assert_eq!(found.len(), 2);
        assert_eq!(
            found[0].as_str(),
            "https://www.periscope.tv/w/1ZkKzPbMVggJv"
        );
        assert_eq!(
            found[1].as_str(),
            "https://pscp.tv/someone/1OdKrlkZZjOJX?autoplay=1&muted=1"
        );
        let resolver = PeriscopeResolver::new(Http::replay(Fixture::new(PLATFORM, None)));
        assert!(found.iter().all(|u| resolver.matches(u)));
        assert_eq!(resolver.embeds_in(&Page::parse(html, &page)).len(), 2);
    }

    #[test]
    fn broadcast_records_are_read() {
        let broadcast = json!({
            "id": 5, "state": "TIMED_OUT", "width": "1280", "height": 720,
            "created_at": "2020-06-01T00:00:00Z", "image_url_medium": "https://img/medium.jpg"
        });
        let (state, width, height) = format_info(&broadcast);
        assert_eq!(state, "timed_out");
        assert_eq!((width, height), (Some(1280), Some(720)));
        assert_eq!(live_status(&broadcast), LiveStatus::WasLive);
        let info = broadcast_info("twitter", &broadcast, "display");
        assert_eq!(info.resolver, "twitter");
        assert_eq!(info.id.as_deref(), Some("5"));
        assert_eq!(info.title, None);
        assert_eq!(
            info.thumbnail.as_ref().map(Url::as_str),
            Some("https://img/medium.jpg")
        );
        assert_eq!(
            broadcast_info(PLATFORM, &json!({}), "display")
                .id
                .as_deref(),
            Some("display")
        );
        assert_eq!(live_status(&json!({"state": "Running"})), LiveStatus::Live);
        assert_eq!(
            scheduled_start(&json!({"start_ms": 1000})).map(|t| t.as_second()),
            Some(1)
        );
    }
}
