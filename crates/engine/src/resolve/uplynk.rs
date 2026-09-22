//! Uplynk content: the HLS master a content link names and the asset record kept beside
//! it, with the playback session a preplay link opens first.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use url::Url;

use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Tag,
    clean_title, fetch_ok, hls, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::MediaKind;

pub const PLATFORM: &str = "uplynk";
/// Every asset is served from here whichever Uplynk host the link names.
const CONTENT: &str = "https://content.uplynk.com/";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// `{path}.m3u8` or `{path}.json`, with the playback session (`pbs`) the link may carry.
    Content {
        path: String,
        session: Option<String>,
    },
    /// `/preplay/{path}.json` or `/preplay2/{path}.json`: a playback session is opened
    /// before the content is read.
    Preplay { path: String },
}

impl Link {
    /// The content path: an asset id, or `ext/{owner}/{external id}`.
    pub fn path(&self) -> &str {
        match self {
            Link::Content { path, .. } | Link::Preplay { path } => path,
        }
    }

    /// The asset id, or the external id of an `ext/{owner}/{external id}` path.
    pub fn display_id(&self) -> &str {
        self.path().rsplit('/').next().unwrap_or_default()
    }
}

static RE_HOST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\w-]+\.uplynk\.com$").unwrap());
/// `/{asset id}.m3u8`, `/ext/{owner}/{external id}.json`...
static RE_CONTENT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^/(ext/[0-9a-f]{32}/[^/?&]+|[0-9a-f]{32})\.(?:m3u8|json)").unwrap()
});
static RE_PREPLAY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^/preplay2?/(ext/[0-9a-f]{32}/[^/?&]+|[0-9a-f]{32})\.json").unwrap()
});

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !RE_HOST.is_match(&host) {
        return None;
    }
    if let Some(caps) = RE_PREPLAY.captures(url.path()) {
        return Some(Link::Preplay {
            path: caps[1].to_string(),
        });
    }
    let caps = RE_CONTENT.captures(url.path())?;
    Some(Link::Content {
        path: caps[1].to_string(),
        session: util::query_param(url, "pbs"),
    })
}

fn content_url(origin: &Url, tail: &str) -> Result<Url, ResolveError> {
    Url::parse(&format!("{CONTENT}{tail}"))
        .map_err(|e| ResolveError::malformed(origin, format!("content path {tail}: {e}")))
}

/// Reads the content at `path` (an asset id, or `ext/{owner}/{external id}`): the HLS
/// master at content.uplynk.com, joined by playback session `session` when one is open,
/// and the asset record beside it. `headers` (the referer or origin an embedding site
/// sends) go with the playlist requests and stay on the variants for the download.
/// `origin` is the link named in errors.
pub async fn resolve_content(
    http: &Http,
    origin: &Url,
    path: &str,
    session: Option<&str>,
    headers: &[(String, String)],
) -> Result<Resolved, ResolveError> {
    let mut master = content_url(origin, &format!("{path}.m3u8"))?;
    if let Some(session) = session {
        master = util::with_query(&master, &[("pbs", session)]);
    }
    let mut expanded = hls::expand(http, &master, PLATFORM, BROWSER_UA, headers).await?;
    // The session goes with every playlist and segment request, not the master alone.
    if let Some(session) = session {
        let signature = vec![("pbs".to_string(), session.to_string())];
        for variant in &mut expanded.variants {
            variant.url = variant.signed_with(&signature);
            if let Some(audio) = &variant.audio_url {
                variant.audio_url = Some(super::signed_url(audio, &signature));
            }
            variant.query = signature.clone();
        }
    }
    let asset_url = content_url(origin, &format!("player/assetinfo/{path}.json"))?;
    let asset = fetch_ok(http, &asset_url, PLATFORM, BROWSER_UA, &[], MAX_PAGE)
        .await?
        .json(origin)?;
    if asset["error"].as_i64() == Some(1) {
        let message = asset["msg"]
            .as_str()
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .unwrap_or("unknown error");
        return Err(ResolveError::unavailable(
            origin,
            format!("uplynk said: {message}"),
        ));
    }
    let display_id = path.rsplit('/').next().unwrap_or(path);
    let mut resolved = Resolved::new(PLATFORM);
    resolved.id = util::text(&asset["asset"])
        .filter(|id| !id.is_empty())
        .or_else(|| Some(display_id.to_string()));
    resolved.title = asset["desc"]
        .as_str()
        .and_then(clean_title)
        .or_else(|| clean_title(display_id));
    resolved.thumbnail = util::url_of(&asset["default_poster_url"], None);
    resolved.duration = util::float(&asset["duration"])
        .filter(|d| *d > 0.0)
        .map(Duration::from_secs_f64)
        .or(expanded.duration);
    resolved.uploader = util::text(&asset["owner"]).filter(|o| !o.is_empty());
    resolved.webpage_url = Some(origin.clone());
    resolved.live = expanded.live;
    resolved.subtitles = expanded.subtitles;
    resolved.variants = expanded.variants;
    Ok(resolved)
}

/// Opens the playback session the preplay record at `preplay` names and reads its
/// content: the record's `sid` joins the playlist requests as `pbs`.
pub async fn resolve_preplay(
    http: &Http,
    preplay: &Url,
    path: &str,
    headers: &[(String, String)],
) -> Result<Resolved, ResolveError> {
    let record = fetch_ok(http, preplay, PLATFORM, BROWSER_UA, &[], MAX_PAGE)
        .await?
        .json(preplay)?;
    let session = util::text(&record["sid"]).filter(|s| !s.is_empty());
    resolve_content(http, preplay, path, session.as_deref(), headers).await
}

/// Resolves any Uplynk link, content or preplay, sending `headers` (the referer or origin
/// of the site that embeds it) with the playlist requests: what another resolver calls
/// when it hands an Uplynk link on.
pub async fn resolve_link(
    http: &Http,
    url: &Url,
    headers: &[(String, String)],
) -> Result<Resolved, ResolveError> {
    match parse_link(url) {
        Some(Link::Content { path, session }) => {
            resolve_content(http, url, &path, session.as_deref(), headers).await
        }
        Some(Link::Preplay { path }) => resolve_preplay(http, url, &path, headers).await,
        None => Err(ResolveError::Unsupported(url.clone())),
    }
}

pub struct UplynkResolver {
    http: Http,
}

impl UplynkResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for UplynkResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Uplynk",
            hosts: &["uplynk.com"],
            features: &["videos", "playback sessions"],
            formats: &["hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::Players],
            session: SessionSupport::None,
            examples: &[
                "https://content.uplynk.com/ext/b82f6b1017f64c7b872b8d80b276b280/0160476a-bfd0-425d-82f9-5757bde3bf37.m3u8",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        Ok(Resolution::from(resolve_link(&self.http, url, &[]).await?))
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

    const ASSET: &str = "e89eaf2ce9054aa89d92ddb2d817a52e";
    const OWNER: &str = "4413701bf5a1488db55b767f8ae9d4fa";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n0.ts\n#EXTINF:2.5,\n1.ts\n#EXT-X-ENDLIST\n";

    fn master(pbs: &str) -> String {
        format!(
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720,CODECS=\"avc1.64001f,mp4a.40.2\"\nhttps://content.uplynk.com/{ASSET}/b.m3u8{pbs}\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\nhttps://content.uplynk.com/{ASSET}/c.m3u8{pbs}\n"
        )
    }

    fn asset_info() -> String {
        json!({
            "asset": ASSET, "desc": "030816-kgo-530pm-solar-eclipse-vid_web.mp4", "owner": OWNER,
            "duration": 530.2739166666679, "default_poster_url": format!("https://content.uplynk.com/{ASSET}/slate.jpg"),
            "error": 0
        })
        .to_string()
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link(&format!("http://content.uplynk.com/{ASSET}.m3u8")),
            Some(Link::Content {
                path: ASSET.into(),
                session: None
            })
        );
        assert_eq!(
            link(&format!(
                "https://content-ause2.uplynk.com/{ASSET}.json?x=1&pbs=abc123"
            )),
            Some(Link::Content {
                path: ASSET.into(),
                session: Some("abc123".into())
            })
        );
        let external = link(&format!(
            "https://content.uplynk.com/ext/{OWNER}/my.clip.m3u8"
        ))
        .unwrap();
        assert_eq!(external.path(), format!("ext/{OWNER}/my.clip"));
        assert_eq!(external.display_id(), "my.clip");
        assert_eq!(
            link(&format!(
                "https://content.uplynk.com/preplay/{ASSET}.json?ad=1"
            )),
            Some(Link::Preplay { path: ASSET.into() })
        );
        assert_eq!(
            link(&format!(
                "https://content.uplynk.com/preplay2/ext/{OWNER}/clip.json"
            )),
            Some(Link::Preplay {
                path: format!("ext/{OWNER}/clip")
            })
        );
        assert_eq!(
            link(&format!(
                "https://content.uplynk.com/player/assetinfo/{ASSET}.json"
            )),
            None
        );
        assert_eq!(
            link(&format!("https://content.uplynk.com/{ASSET}.mp4")),
            None
        );
        assert_eq!(link("https://content.uplynk.com/abc.m3u8"), None);
        assert_eq!(link(&format!("https://example.com/{ASSET}.m3u8")), None);
        assert_eq!(
            link(&format!("ftp://content.uplynk.com/{ASSET}.m3u8")),
            None
        );
    }

    #[tokio::test]
    async fn content_links_resolve_to_hls_with_the_asset_record() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            &format!("https://content.uplynk.com/{ASSET}.m3u8"),
            200,
            "application/vnd.apple.mpegurl",
            master(""),
        ));
        fixture.exchanges.push(get(
            &format!("https://content.uplynk.com/{ASSET}/b.m3u8"),
            200,
            "application/vnd.apple.mpegurl",
            MEDIA.into(),
        ));
        fixture.exchanges.push(get(
            &format!("https://content.uplynk.com/player/assetinfo/{ASSET}.json"),
            200,
            "application/json",
            asset_info(),
        ));
        let resolver = UplynkResolver::new(Http::replay(fixture));
        let url = Url::parse(&format!("http://content.uplynk.com/{ASSET}.m3u8")).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some(ASSET));
        assert_eq!(
            resolved.title.as_deref(),
            Some("030816-kgo-530pm-solar-eclipse-vid_web.mp4")
        );
        assert_eq!(resolved.uploader.as_deref(), Some(OWNER));
        assert_eq!(
            resolved.duration,
            Some(Duration::from_secs_f64(530.2739166666679))
        );
        assert_eq!(
            resolved.thumbnail.unwrap().as_str(),
            format!("https://content.uplynk.com/{ASSET}/slate.jpg")
        );
        assert!(!resolved.live);
        assert_eq!(resolved.variants.len(), 2);
        assert_eq!(resolved.variants[0].height, Some(720));
        assert_eq!(
            resolved.variants[0].url.as_str(),
            format!("https://content.uplynk.com/{ASSET}/b.m3u8")
        );
        assert!(resolved.variants[0].headers.is_empty());
    }

    #[tokio::test]
    async fn preplay_links_open_a_session_and_carry_the_embedding_headers() {
        let preplay = format!("https://content.uplynk.com/preplay/ext/{OWNER}/clip.json?ad.foo=1");
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            &preplay,
            200,
            "application/json",
            json!({"sid": "sess1", "playURL": "ignored"}).to_string(),
        ));
        fixture.exchanges.push(get(
            &format!("https://content.uplynk.com/ext/{OWNER}/clip.m3u8?pbs=sess1"),
            200,
            "application/vnd.apple.mpegurl",
            master("?pbs=sess1"),
        ));
        fixture.exchanges.push(get(
            &format!("https://content.uplynk.com/{ASSET}/b.m3u8?pbs=sess1"),
            200,
            "application/vnd.apple.mpegurl",
            MEDIA.into(),
        ));
        fixture.exchanges.push(get(
            &format!("https://content.uplynk.com/player/assetinfo/ext/{OWNER}/clip.json"),
            200,
            "application/json",
            asset_info(),
        ));
        let http = Http::replay(fixture);
        let url = Url::parse(&preplay).unwrap();
        let headers = vec![(
            "origin".to_string(),
            "https://www.foxsports.com".to_string(),
        )];
        let resolved = resolve_link(&http, &url, &headers).await.unwrap();
        assert_eq!(resolved.id.as_deref(), Some(ASSET));
        assert_eq!(resolved.variants.len(), 2);
        assert_eq!(
            resolved.variants[0].url.as_str(),
            format!("https://content.uplynk.com/{ASSET}/b.m3u8?pbs=sess1")
        );
        assert_eq!(resolved.variants[0].headers, headers);
        assert_eq!(
            resolved.duration,
            Some(Duration::from_secs_f64(530.2739166666679))
        );
        assert_eq!(resolved.webpage_url.unwrap().as_str(), preplay);
    }

    #[tokio::test]
    async fn refused_assets_and_missing_playlists_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            &format!("https://content.uplynk.com/{ASSET}.m3u8"),
            200,
            "application/vnd.apple.mpegurl",
            master(""),
        ));
        fixture.exchanges.push(get(
            &format!("https://content.uplynk.com/{ASSET}/b.m3u8"),
            200,
            "application/vnd.apple.mpegurl",
            MEDIA.into(),
        ));
        fixture.exchanges.push(get(
            &format!("https://content.uplynk.com/player/assetinfo/{ASSET}.json"),
            200,
            "application/json",
            json!({"error": 1, "msg": "asset has been deleted"}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://content.uplynk.com/ffffffffffffffffffffffffffffffff.m3u8",
            404,
            "text/plain",
            "not found".into(),
        ));
        let resolver = UplynkResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse(&format!("https://content.uplynk.com/{ASSET}.m3u8")).unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("asset has been deleted")),
            "{error}"
        );
        let error = resolver
            .resolve(
                &Url::parse("https://content.uplynk.com/ffffffffffffffffffffffffffffffff.m3u8")
                    .unwrap(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ResolveError::NotFound(_)), "{error}");
        let other = Url::parse("https://example.com/video").unwrap();
        assert!(matches!(
            resolve_link(&resolver.http, &other, &[]).await.unwrap_err(),
            ResolveError::Unsupported(_)
        ));
    }
}
