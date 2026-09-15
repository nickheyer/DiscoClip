//! Brightcove players, through the Playback API the player calls with the policy key its
//! script carries: the MP4 renditions with their sizes, the HLS and DASH manifests, the
//! text tracks and the video's name, description, length and poster. A video locked with
//! DRM is reported by its key system.

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
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, SubtitleFormat, SubtitleTrack, Variant, VariantKind, clean_title, essence,
    fetch, is_dash_type, is_hls_type,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "brightcove";
const PLAYERS: &str = "https://players.brightcove.net/";
const PLAYBACK_API: &str = "https://edge.api.brightcove.com/playback/v1/accounts/";

static RE_POLICY_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"policyKey\\?"?\s*:\s*\\?"(BCpk[A-Za-z0-9_-]+)"#).unwrap());
static RE_ACCOUNT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{6,}$").unwrap());
static RE_PLAYER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]+$").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Video(String),
    Playlist(String),
}

/// A video in an account's player: `videoId` may be an id or `ref:<reference id>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub account: String,
    pub player: String,
    pub content: Content,
}

/// A player embed link for the parts a page names.
pub fn embed_url(account: &str, player: &str, video: &str) -> Url {
    let mut url =
        Url::parse(&format!("{PLAYERS}{account}/{player}_default/index.html")).expect("valid");
    url.query_pairs_mut().append_pair("videoId", video);
    url
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "players.brightcove.net" {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let (account, player_dir) = match segments.as_slice() {
        [account, player_dir, ..] if RE_ACCOUNT.is_match(account) => (account, player_dir),
        _ => return None,
    };
    let player = player_dir.strip_suffix("_default").unwrap_or(player_dir);
    if !RE_PLAYER.is_match(player) {
        return None;
    }
    let query = |key: &str| {
        url.query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, value)| value.into_owned())
            .filter(|s| !s.trim().is_empty())
    };
    let content = query("videoId")
        .map(Content::Video)
        .or_else(|| query("playlistId").map(Content::Playlist))?;
    Some(Link {
        account: account.to_string(),
        player: player.to_string(),
        content,
    })
}

/// Brightcove's inline video and video-js elements carry the account, player and
/// video or playlist in data attributes, without an iframe.
pub fn embeds_in(page: &Page) -> Vec<Url> {
    let mut found = Vec::new();
    let selector = Selector::parse("video[data-account], video-js[data-account]").expect("valid");
    for element in page.document().select(&selector) {
        let attrs = element.value();
        let Some(account) = attrs
            .attr("data-account")
            .filter(|s| RE_ACCOUNT.is_match(s))
        else {
            continue;
        };
        let player = attrs.attr("data-player").unwrap_or("default");
        if !RE_PLAYER.is_match(player) {
            continue;
        }
        let target = attrs
            .attr("data-video-id")
            .filter(|s| !s.is_empty())
            .map(|id| ("videoId", id))
            .or_else(|| {
                attrs
                    .attr("data-playlist-id")
                    .filter(|s| !s.is_empty())
                    .map(|id| ("playlistId", id))
            });
        if let Some((key, id)) = target {
            let mut url = embed_url(account, player, id);
            url.query_pairs_mut().clear().append_pair(key, id);
            if !found.contains(&url) {
                found.push(url);
            }
        }
    }
    found
}

fn drm_system(source: &Value) -> Option<String> {
    let systems = source["key_systems"].as_object()?;
    systems.keys().next().map(|k| match k.as_str() {
        "com.widevine.alpha" => "widevine".to_string(),
        "com.microsoft.playready" => "playready".to_string(),
        "com.apple.fps.1_0" | "com.apple.fps" => "fairplay".to_string(),
        other => other.to_string(),
    })
}

/// The video's sources as variants: HTTPS manifests and MP4 renditions, without the plain
/// HTTP copies of the same.
pub fn variants_of(video: &Value) -> Vec<Variant> {
    let duration = video["duration"]
        .as_u64()
        .filter(|d| *d > 0)
        .map(Duration::from_millis);
    let mut variants: Vec<Variant> = Vec::new();
    for source in video["sources"].as_array().into_iter().flatten() {
        let Some(url) = source["src"].as_str().and_then(|u| Url::parse(u).ok()) else {
            continue;
        };
        if url.scheme() != "https" {
            continue;
        }
        let mime = essence(source["type"].as_str());
        let container = source["container"].as_str().map(|c| c.to_ascii_uppercase());
        let kind = if is_hls_type(&mime) {
            VariantKind::Hls
        } else if is_dash_type(&mime) {
            VariantKind::Dash
        } else if container.as_deref() == Some("MP4") || url.path().ends_with(".mp4") {
            VariantKind::File
        } else {
            continue;
        };
        if variants.iter().any(|v| v.url == url) {
            continue;
        }
        let mut v = Variant::new(url, kind);
        if kind == VariantKind::File {
            v.container = Some(Container::Mp4);
            v.video = Some(
                match source["codec"]
                    .as_str()
                    .map(|c| c.to_ascii_uppercase())
                    .as_deref()
                {
                    Some("H265") | Some("HEVC") => VideoCodec::H265,
                    _ => VideoCodec::H264,
                },
            );
            v.audio = Some(AudioCodec::Aac);
            v.width = source["width"].as_u64().map(|w| w as u32);
            v.height = source["height"].as_u64().map(|h| h as u32);
            v.bitrate = source["avg_bitrate"].as_u64().filter(|b| *b > 0);
            v.size = source["size"].as_u64().filter(|s| *s > 0);
            v.label = v.height.map(|h| format!("{h}p"));
        } else {
            v.format_id = source["ext_x_version"]
                .as_str()
                .map(|x| format!("{}-v{x}", kind.as_str()));
        }
        v.duration = duration;
        v.drm = drm_system(source);
        variants.push(v);
    }
    variants
}

pub fn subtitles_of(video: &Value) -> Vec<SubtitleTrack> {
    let mut tracks = Vec::new();
    for track in video["text_tracks"].as_array().into_iter().flatten() {
        if !matches!(track["kind"].as_str(), Some("captions") | Some("subtitles")) {
            continue;
        }
        let Some(url) = track["src"].as_str().and_then(|u| Url::parse(u).ok()) else {
            continue;
        };
        if url.scheme() != "https" || tracks.iter().any(|t: &SubtitleTrack| t.url == url) {
            continue;
        }
        let mime = essence(track["mime_type"].as_str());
        tracks.push(SubtitleTrack {
            url,
            language: track["srclang"].as_str().unwrap_or("und").to_string(),
            name: track["label"].as_str().map(String::from),
            format: if mime == "text/vtt" || mime.is_empty() {
                SubtitleFormat::Vtt
            } else if mime.contains("ttml") || mime.contains("xml") {
                SubtitleFormat::Ttml
            } else {
                SubtitleFormat::Vtt
            },
            auto: false,
            headers: Vec::new(),
        });
    }
    tracks
}

pub struct BrightcoveResolver {
    http: Http,
}

impl BrightcoveResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// The policy key the player's script carries.
    async fn policy_key(&self, link: &Link, origin: &Url) -> Result<String, ResolveError> {
        let script = Url::parse(&format!(
            "{PLAYERS}{}/{}_default/index.min.js",
            link.account, link.player
        ))
        .expect("valid");
        let fetched = fetch(&self.http, &script, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        match fetched.status.as_u16() {
            200..=299 => {}
            404 => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the account has no player called {}", link.player),
                ));
            }
            status => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the player script answered HTTP {status}"),
                ));
            }
        }
        let text = fetched.text();
        RE_POLICY_KEY
            .captures(&text)
            .map(|c| c[1].to_string())
            .ok_or_else(|| {
                ResolveError::unavailable(origin, "the player script carries no policy key")
            })
    }
}

#[async_trait]
impl Resolver for BrightcoveResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Brightcove",
            hosts: &["players.brightcove.net"],
            features: &[
                "player embeds",
                "reference ids",
                "playlists",
                "text tracks",
                "drm reported",
            ],
            formats: &["mp4", "hls", "dash"],
            session: SessionSupport::None,
            examples: &[
                "https://players.brightcove.net/1752604059001/default_default/index.html?videoId=4457254747001",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let key = self.policy_key(&link, url).await?;
        let (resource, id) = match &link.content {
            Content::Video(id) => ("videos", id),
            Content::Playlist(id) => ("playlists", id),
        };
        let mut api =
            Url::parse(&format!("{PLAYBACK_API}{}/{resource}/", link.account)).expect("valid");
        api.path_segments_mut()
            .expect("base URL")
            .pop_if_empty()
            .push(id);
        let headers = [("accept".to_string(), format!("application/json;pk={key}"))];
        let fetched = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &headers, MAX_PAGE).await?;
        let body = fetched.json(url).unwrap_or(Value::Null);
        match fetched.status.as_u16() {
            200..=299 => {}
            404 => return Err(ResolveError::NotFound(url.clone())),
            403 => {
                let code = body[0]["error_subcode"]
                    .as_str()
                    .or(body[0]["error_code"].as_str())
                    .unwrap_or("ACCESS_DENIED");
                return Err(ResolveError::unavailable(
                    url,
                    format!("the playback API refused the video: {code}"),
                ));
            }
            429 => return Err(ResolveError::RateLimited(url.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    url,
                    format!("the playback API answered HTTP {status}"),
                ));
            }
        }
        if let Content::Playlist(id) = link.content {
            let entries: Vec<PlaylistEntry> = body["videos"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|video| {
                    Some(PlaylistEntry {
                        url: embed_url(&link.account, &link.player, video["id"].as_str()?),
                        title: video["name"].as_str().and_then(clean_title),
                        duration: video["duration"]
                            .as_u64()
                            .filter(|d| *d > 0)
                            .map(Duration::from_millis),
                    })
                })
                .collect();
            if entries.is_empty() {
                return Err(ResolveError::NotFound(url.clone()));
            }
            return Ok(Resolution::Playlist(Playlist {
                resolver: PLATFORM.into(),
                id: Some(id),
                title: body["name"].as_str().and_then(clean_title),
                total: Some(entries.len()),
                entries,
            }));
        }
        let variants = variants_of(&body);
        if variants.is_empty() {
            return Err(ResolveError::unavailable(
                url,
                "the video has no playable sources",
            ));
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = body["id"].as_str().map(String::from);
        resolved.title = body["name"].as_str().and_then(clean_title);
        resolved.description = body["long_description"]
            .as_str()
            .or(body["description"].as_str())
            .and_then(clean_title);
        resolved.uploaded_at = body["published_at"]
            .as_str()
            .or(body["created_at"].as_str())
            .and_then(|t| t.parse::<Timestamp>().ok());
        resolved.duration = body["duration"]
            .as_u64()
            .filter(|d| *d > 0)
            .map(Duration::from_millis);
        resolved.thumbnail = body["poster"]
            .as_str()
            .or(body["thumbnail"].as_str())
            .and_then(|u| Url::parse(u).ok());
        resolved.webpage_url = Some(url.clone());
        resolved.subtitles = subtitles_of(&body);
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

    fn get(url: &str, status: u16, content_type: &str, body: &str) -> Exchange {
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
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    const SCRIPT: &str = r#"!function(){var e={accountId:"1752604059001",policyKey:"BCpkADawqM3NCJTTAQ6n2c1-NDEqVU_WLKw1tLj45NwgjsWncRQbJcWW",playerId:"default"};}();"#;
    const VIDEO: &str = r#"{"id":"4457254747001","name":"Sea Marvels Collection","description":null,"long_description":"Marvels of the sea.","duration":155574,"published_at":"2015-09-01T16:25:20.060Z","account_id":"1752604059001","poster":"https://cf-images.eu-west-1.prod.boltdns.net/x/640x360/match/image.jpg","thumbnail":"https://cf-images.eu-west-1.prod.boltdns.net/x/160x90/match/image.jpg","sources":[{"src":"http://manifest.prod.boltdns.net/manifest/v1/hls/v4/clear/1752604059001/x/10s/master.m3u8?fastly_token=a","type":"application/x-mpegURL","ext_x_version":"4"},{"src":"https://manifest.prod.boltdns.net/manifest/v1/hls/v4/clear/1752604059001/x/10s/master.m3u8?fastly_token=a","type":"application/x-mpegURL","ext_x_version":"4"},{"src":"https://manifest.prod.boltdns.net/manifest/v2/hls/v7/clear/avc1_mp4a/1752604059001/x/10s/master.m3u8?fastly_token=b","type":"application/x-mpegURL","ext_x_version":"7"},{"src":"https://manifest.prod.boltdns.net/manifest/v1/dash/live-baseurl/clear/1752604059001/x/2s/manifest.mpd?fastly_token=c","type":"application/dash+xml"},{"src":"http://fastly-signed-eu-west-1-prod.brightcovecdn.com/media/v1/pmp4/static/clear/1752604059001/x/main.mp4?fastly_token=d","container":"MP4","codec":"H264","height":360,"width":640,"avg_bitrate":2007000,"size":39116979},{"src":"https://fastly-signed-eu-west-1-prod.brightcovecdn.com/media/v1/pmp4/static/clear/1752604059001/x/main.mp4?fastly_token=d","container":"MP4","codec":"H264","height":360,"width":640,"avg_bitrate":2007000,"size":39116979}],"text_tracks":[{"src":"https://manifest.prod.boltdns.net/thumbnail/v1/x/thumbnail.webvtt?fastly_token=e","kind":"metadata","label":"thumbnails","mime_type":"text/webvtt"},{"src":"https://manifest.prod.boltdns.net/captions/x/en.vtt?fastly_token=f","kind":"captions","srclang":"en","label":"English","mime_type":"text/vtt","default":true}]}"#;

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link(
                "https://players.brightcove.net/1752604059001/default_default/index.html?videoId=4457254747001"
            ),
            Some(Link {
                account: "1752604059001".into(),
                player: "default".into(),
                content: Content::Video("4457254747001".into())
            })
        );
        assert_eq!(
            link("https://players.brightcove.net/929656772001/e41d32dc-ec74-459e-a845-6c69f7b724ea_default/index.html?videoId=ref:myref").unwrap().content,
            Content::Video("ref:myref".into())
        );
        assert_eq!(
            link("https://players.brightcove.net/1752604059001/default_default/index.html"),
            None
        );
        assert_eq!(link("https://players.brightcove.net/"), None);
        assert_eq!(
            embed_url("1752604059001", "default", "4457254747001").as_str(),
            "https://players.brightcove.net/1752604059001/default_default/index.html?videoId=4457254747001"
        );
    }

    #[tokio::test]
    async fn videos_resolve_through_the_playback_api_with_the_players_policy_key() {
        let mut fixture = Fixture::new("brightcove", None);
        fixture.exchanges.push(get(
            "https://players.brightcove.net/1752604059001/default_default/index.min.js",
            200,
            "application/javascript",
            SCRIPT,
        ));
        fixture.exchanges.push(get("https://edge.api.brightcove.com/playback/v1/accounts/1752604059001/videos/4457254747001", 200, "application/json", VIDEO));
        fixture.exchanges.push(get(
            "https://edge.api.brightcove.com/playback/v1/accounts/1752604059001/videos/1",
            404,
            "application/json",
            r#"[{"error_code":"VIDEO_NOT_FOUND"}]"#,
        ));
        fixture.exchanges.push(get("https://edge.api.brightcove.com/playback/v1/accounts/1752604059001/videos/2", 403, "application/json", r#"[{"error_code":"ACCESS_DENIED","error_subcode":"CLIENT_GEO","message":"Access to this resource is forbidden by access policy."}]"#));
        let resolver = BrightcoveResolver::new(Http::replay(fixture));
        let url = Url::parse("https://players.brightcove.net/1752604059001/default_default/index.html?videoId=4457254747001").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.title.as_deref(), Some("Sea Marvels Collection"));
        assert_eq!(resolved.description.as_deref(), Some("Marvels of the sea."));
        assert_eq!(resolved.duration, Some(Duration::from_millis(155574)));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(
            resolved.variants.len(),
            4,
            "{:?}",
            resolved
                .variants
                .iter()
                .map(|v| v.url.as_str())
                .collect::<Vec<_>>()
        );
        assert!(resolved.variants.iter().all(|v| v.url.scheme() == "https"));
        let mp4 = resolved
            .variants
            .iter()
            .find(|v| v.kind == VariantKind::File)
            .unwrap();
        assert_eq!(mp4.size, Some(39116979));
        assert_eq!(mp4.height, Some(360));
        assert_eq!(mp4.bitrate, Some(2007000));
        assert!(
            resolved
                .variants
                .iter()
                .any(|v| v.kind == VariantKind::Dash)
        );
        assert_eq!(resolved.subtitles.len(), 1);
        assert_eq!(resolved.subtitles[0].language, "en");
        assert!(matches!(
            resolver.resolve(&Url::parse("https://players.brightcove.net/1752604059001/default_default/index.html?videoId=1").unwrap()).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver.resolve(&Url::parse("https://players.brightcove.net/1752604059001/default_default/index.html?videoId=2").unwrap()).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("CLIENT_GEO")),
            "{error}"
        );
    }

    #[test]
    fn drm_locked_sources_carry_their_key_system() {
        let locked: Value = serde_json::from_str(r#"{"sources":[{"src":"https://m.test/x.mpd","type":"application/dash+xml","key_systems":{"com.widevine.alpha":{"license_url":"https://l"}}}]}"#).unwrap();
        let variants = variants_of(&locked);
        assert_eq!(variants[0].drm.as_deref(), Some("widevine"));
        assert!(!variants[0].is_playable());
    }

    #[tokio::test]
    async fn recorded_videos_and_playlists_resolve() {
        let fixture = Fixture::parse(include_str!("brightcove_fixture.json")).unwrap();
        let resolver = BrightcoveResolver::new(Http::replay(fixture));
        let url = embed_url("1752604059001", "default", "4457254747001");
        let video = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(video.title.as_deref(), Some("Sea Marvels Collection"));
        assert!(
            video
                .variants
                .iter()
                .any(|v| v.kind == VariantKind::File && v.size == Some(39116979))
        );
        assert!(video.variants.iter().any(|v| v.kind == VariantKind::Hls));
        assert!(video.variants.iter().any(|v| v.kind == VariantKind::Dash));
        let playlist_url = Url::parse("https://players.brightcove.net/1752604059001/default_default/index.html?playlistId=5743160747001").unwrap();
        let Resolution::Playlist(playlist) = resolver.resolve(&playlist_url).await.unwrap() else {
            panic!("expected a playlist");
        };
        assert!(!playlist.entries.is_empty());
        assert!(
            playlist
                .entries
                .iter()
                .all(|entry| parse_link(&entry.url).is_some())
        );
        let missing = Url::parse(
            "https://players.brightcove.net/1752604059001/default_default/index.html?playlistId=1",
        )
        .unwrap();
        assert!(matches!(
            resolver.resolve(&missing).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
