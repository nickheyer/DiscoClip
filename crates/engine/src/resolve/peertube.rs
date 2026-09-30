//! PeerTube videos, channels, accounts and playlists on any server running it: every
//! PeerTube answers `/api/v1/videos/{id}` and its listing endpoints alike, so a link
//! shaped like one of PeerTube's is tried against the API of whatever host it names,
//! and a host that turns out not to be a PeerTube is handed on to the next resolver and
//! remembered. A video lists its web video files by resolution and its HLS playlists,
//! whose fragmented MP4 renditions are files of their own; a live video streams through
//! its HLS playlist. Channels, accounts and playlists are the videos they list.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    ClipRange, MAX_PAGE, Page, Platform, Playlist, PlaylistEntry, Resolution, ResolveError,
    Resolved, Resolver, SessionSupport, SubtitleFormat, SubtitleTrack, Tag, Variant, VariantKind,
    clean_title, hls, timestamp_hint, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "peertube";
/// How long a server stays remembered as not being a PeerTube.
const DECLINE_MEMORY: Duration = Duration::from_secs(6 * 60 * 60);
/// How many videos a channel, account or playlist is read up to: the API's largest page.
const LISTING_LIMIT: usize = 100;
/// The header a password-protected video is unlocked with.
const PASSWORD_HEADER: &str = "x-peertube-video-password";

/// A video or playlist id: a UUID, or the 22-character short form.
static RE_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[0-9A-Za-z]{22}|[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})$")
        .unwrap()
});
/// A channel or account name, local or `name@server`.
static RE_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_.-]+(?:@[A-Za-z0-9.-]+)?$").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Video {
        server: Url,
        id: String,
        /// The password the link carries for a protected video.
        password: Option<String>,
    },
    Playlist {
        server: Url,
        id: String,
    },
    Channel {
        server: Url,
        name: String,
    },
    Account {
        server: Url,
        name: String,
    },
}

/// Whether the path is shaped like one of PeerTube's links, on whatever host.
pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.host_str()?;
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let mut server = url.clone();
    server.set_path("/");
    server.set_query(None);
    server.set_fragment(None);
    let id = |s: &str| RE_ID.is_match(s).then(|| s.to_string());
    let name = |s: &str| RE_NAME.is_match(s).then(|| s.to_string());
    Some(match segments.as_slice() {
        ["w", "p", playlist]
        | ["videos", "watch", "playlist", playlist]
        | ["video-playlists", playlist]
        | ["video-playlists", "embed", playlist]
        | ["api", "v1", "video-playlists", playlist] => Link::Playlist {
            server,
            id: id(playlist)?,
        },
        ["w", video]
        | ["videos", "watch", video]
        | ["videos", "embed", video]
        | ["api", "v1", "videos", video] => Link::Video {
            server,
            id: id(video)?,
            password: util::query_param(url, "password").filter(|p| !p.is_empty()),
        },
        ["c", channel]
        | ["c", channel, "videos"]
        | ["video-channels", channel]
        | ["video-channels", channel, "videos"]
        | ["api", "v1", "video-channels", channel] => Link::Channel {
            server,
            name: name(channel)?,
        },
        ["a", account]
        | ["a", account, "videos"]
        | ["accounts", account]
        | ["accounts", account, "videos"]
        | ["api", "v1", "accounts", account] => Link::Account {
            server,
            name: name(account)?,
        },
        _ => return None,
    })
}

/// Whether a JSON error is PeerTube's own: the problem document its API answers with,
/// or the `error` of older versions naming what was not found.
fn is_peertube_error(json: &Value) -> bool {
    if json["docs"]
        .as_str()
        .is_some_and(|d| d.contains("joinpeertube"))
    {
        return true;
    }
    if json["type"].as_str() == Some("about:blank") && json["status"].is_number() {
        return true;
    }
    json["error"].as_str().is_some_and(|e| {
        let e = e.to_ascii_lowercase();
        e.contains("video")
            || e.contains("account")
            || e.contains("channel")
            || e.contains("playlist")
    })
}

/// The message a PeerTube error carries.
fn error_detail(json: &Value) -> Option<String> {
    json["detail"]
        .as_str()
        .or_else(|| json["error"].as_str())
        .or_else(|| json["title"].as_str())
        .and_then(clean_title)
}

/// The label of a video's state, for the reason it has no files.
fn state_reason(video: &Value) -> String {
    match util::int(&video["state"]["id"]) {
        Some(2) => "the video is still being transcoded".into(),
        Some(3) => "the video is still being imported".into(),
        Some(4) => "the live stream has not started".into(),
        Some(5) => "the live stream has ended".into(),
        Some(6) | Some(8) => "the video is being moved between storages".into(),
        Some(7) => "the video's transcoding failed".into(),
        Some(9) => "the video is being edited".into(),
        _ => match video["state"]["label"].as_str().and_then(clean_title) {
            Some(label) => format!("the video has no files ({label})"),
            None => "the video has no files".into(),
        },
    }
}

/// A web video or HLS rendition file as a variant.
fn file_variant(file: &Value, format: &str, headers: &[(String, String)]) -> Option<Variant> {
    let url = util::url_of(&file["fileUrl"], None)?;
    let label = util::text(&file["resolution"]["label"]);
    let height = util::u32_of(&file["resolution"]["id"]).filter(|h| *h > 0);
    let mut v = Variant::new(url, VariantKind::File);
    v.container = Some(Container::Mp4);
    if height.is_none() && label.as_deref() == Some("0p") {
        v.audio_only = true;
        v.audio = Some(AudioCodec::Aac);
    } else {
        v.video = Some(VideoCodec::H264);
        v.audio = Some(AudioCodec::Aac);
        v.height = util::u32_of(&file["height"]).filter(|h| *h > 0).or(height);
        v.width = util::u32_of(&file["width"]).filter(|w| *w > 0);
        v.fps = util::float(&file["fps"]).filter(|f| *f > 0.0);
    }
    v.size = util::uint(&file["size"]).filter(|s| *s > 0);
    v.format_id = Some(match &label {
        Some(label) => format!("{format}-{label}"),
        None => format.to_string(),
    });
    v.label = label;
    v.headers = headers.to_vec();
    Some(v)
}

/// Adds `variant` unless one with its link is there already.
fn push_variant(variants: &mut Vec<Variant>, variant: Variant) {
    if !variants.iter().any(|v| v.url == variant.url) {
        variants.push(variant);
    }
}

pub struct PeertubeResolver {
    http: Http,
    /// Servers found not to be PeerTubes, so their links are handed on immediately.
    declined: Mutex<HashMap<String, Timestamp>>,
}

/// A JSON answer of a server's API, or the finding that the server is no PeerTube.
enum Answer {
    Json(Value),
    NotPeertube,
}

impl PeertubeResolver {
    pub fn new(http: Http) -> Self {
        Self {
            http,
            declined: Mutex::new(HashMap::new()),
        }
    }

    fn remembered_decline(&self, host: &str) -> bool {
        let mut declined = self.declined.lock().unwrap_or_else(|e| e.into_inner());
        let now = Timestamp::now();
        declined.retain(|_, at| {
            now.duration_since(*at)
                < jiff::SignedDuration::try_from(DECLINE_MEMORY).unwrap_or_default()
        });
        declined.contains_key(host)
    }

    fn remember_decline(&self, host: &str) {
        self.declined
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(host.to_string(), Timestamp::now());
    }

    /// Whether `server` was found not to be a PeerTube, so its links are handed on.
    fn decline_if_remembered(&self, server: &Url, origin: &Url) -> Result<(), ResolveError> {
        let host = server.host_str().unwrap_or("");
        if self.remembered_decline(host) {
            return Err(ResolveError::Unsupported(origin.clone()));
        }
        Ok(())
    }

    /// `path` of `server`'s API, read as `origin`'s. A successful answer that is not
    /// JSON, or an error that is not PeerTube's, means the server is no PeerTube.
    async fn api(
        &self,
        server: &Url,
        path: &str,
        headers: &[(String, String)],
        origin: &Url,
    ) -> Result<Answer, ResolveError> {
        self.decline_if_remembered(server, origin)?;
        let api = server
            .join(path)
            .map_err(|e| ResolveError::malformed(origin, e.to_string()))?;
        let response = self
            .http
            .get(api.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "application/json")
            .headers(headers)
            .send()
            .await?;
        let code = response.status.as_u16();
        let text = response.text(MAX_PAGE).await?;
        let json: Option<Value> = serde_json::from_str::<Value>(&text)
            .ok()
            .filter(|j| j.is_object());
        let Some(json) = json else {
            return Ok(Answer::NotPeertube);
        };
        match code {
            200..=299 => Ok(Answer::Json(json)),
            429 => Err(ResolveError::RateLimited(origin.clone())),
            _ if !is_peertube_error(&json) => Ok(Answer::NotPeertube),
            400 | 404 | 410 => Err(ResolveError::NotFound(origin.clone())),
            401 | 403 => {
                let code_name = json["code"].as_str().unwrap_or("");
                let detail = error_detail(&json).unwrap_or_default();
                Err(match code_name {
                    "video_requires_password" => ResolveError::unavailable(
                        origin,
                        "Password required. Add ?password= to the link.",
                    ),
                    "incorrect_video_password" => {
                        ResolveError::unavailable(origin, "the video refused the password")
                    }
                    _ if detail.to_ascii_lowercase().contains("password") => {
                        ResolveError::unavailable(
                            origin,
                            "Password required. Add ?password= to the link.",
                        )
                    }
                    _ => ResolveError::unavailable(
                        origin,
                        if detail.is_empty() {
                            format!("the server answered HTTP {code}")
                        } else {
                            detail
                        },
                    ),
                })
            }
            _ => Err(ResolveError::unavailable(
                origin,
                error_detail(&json).unwrap_or_else(|| format!("the server answered HTTP {code}")),
            )),
        }
    }

    /// [`Self::api`] for a server that must be a PeerTube: an answer without `key` means
    /// it is not one, which is remembered and handed on.
    async fn api_expecting(
        &self,
        server: &Url,
        path: &str,
        headers: &[(String, String)],
        key: &str,
        origin: &Url,
    ) -> Result<Value, ResolveError> {
        match self.api(server, path, headers, origin).await? {
            Answer::Json(json) if !json[key].is_null() => Ok(json),
            _ => {
                self.remember_decline(server.host_str().unwrap_or(""));
                Err(ResolveError::Unsupported(origin.clone()))
            }
        }
    }

    /// The subtitle tracks a video lists, none when the server lists none.
    async fn captions(&self, server: &Url, id: &str, origin: &Url) -> Vec<SubtitleTrack> {
        let json = match self
            .api(server, &format!("api/v1/videos/{id}/captions"), &[], origin)
            .await
        {
            Ok(Answer::Json(json)) => json,
            Ok(Answer::NotPeertube) => return Vec::new(),
            Err(error) => {
                tracing::debug!(%server, id, "PeerTube captions not read: {error}");
                return Vec::new();
            }
        };
        json["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|caption| {
                let url = util::url_of(&caption["fileUrl"], None)
                    .or_else(|| util::url_of(&caption["captionPath"], Some(server)))?;
                let language =
                    util::text(&caption["language"]["id"]).unwrap_or_else(|| "und".into());
                Some(SubtitleTrack {
                    url,
                    language,
                    name: util::text(&caption["language"]["label"]),
                    format: SubtitleFormat::Vtt,
                    auto: caption["automaticallyGenerated"].as_bool().unwrap_or(false),
                    headers: Vec::new(),
                })
            })
            .collect()
    }

    async fn resolve_video(
        &self,
        server: &Url,
        id: &str,
        password: Option<&str>,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let headers: Vec<(String, String)> = password
            .map(|p| vec![(PASSWORD_HEADER.to_string(), p.to_string())])
            .unwrap_or_default();
        let video = self
            .api_expecting(
                server,
                &format!("api/v1/videos/{id}"),
                &headers,
                "uuid",
                origin,
            )
            .await?;
        if video["blacklisted"].as_bool() == Some(true) {
            return Err(ResolveError::unavailable(
                origin,
                util::text(&video["blacklistedReason"])
                    .unwrap_or_else(|| "the video is blacklisted on its server".into()),
            ));
        }
        let live = video["isLive"].as_bool() == Some(true);
        let mut variants: Vec<Variant> = Vec::new();
        for file in video["files"].as_array().into_iter().flatten() {
            if let Some(variant) = file_variant(file, "web", &headers) {
                push_variant(&mut variants, variant);
            }
        }
        let playlists: Vec<&Value> = video["streamingPlaylists"]
            .as_array()
            .into_iter()
            .flatten()
            .collect();
        for playlist in &playlists {
            for file in playlist["files"].as_array().into_iter().flatten() {
                if let Some(variant) = file_variant(file, "hls", &headers) {
                    push_variant(&mut variants, variant);
                }
            }
        }
        let mut failure = None;
        if live || variants.is_empty() {
            for playlist in &playlists {
                let Some(playlist_url) = util::url_of(&playlist["playlistUrl"], None) else {
                    continue;
                };
                match hls::expand(&self.http, &playlist_url, PLATFORM, BROWSER_UA, &headers).await {
                    Ok(expanded) => {
                        for mut variant in expanded.variants {
                            variant.live = live;
                            variant.format_id = Some(match &variant.label {
                                Some(label) => format!("hls-{label}"),
                                None => "hls".to_string(),
                            });
                            push_variant(&mut variants, variant);
                        }
                    }
                    Err(error) => failure = Some(error),
                }
            }
        }
        if variants.is_empty() {
            return Err(match failure {
                Some(error) if live => error,
                _ => ResolveError::unavailable(origin, state_reason(&video)),
            });
        }
        let uuid = util::text(&video["uuid"]).unwrap_or_else(|| id.to_string());
        let short = util::text(&video["shortUUID"]).unwrap_or_else(|| uuid.clone());
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(uuid.clone());
        resolved.title = util::text(&video["name"]).and_then(|t| clean_title(&t));
        resolved.description = util::text(&video["description"])
            .or_else(|| util::text(&video["truncatedDescription"]));
        resolved.uploader = util::text(&video["channel"]["displayName"])
            .or_else(|| util::text(&video["account"]["displayName"]))
            .and_then(|u| clean_title(&u));
        resolved.uploader_url = util::url_of(&video["channel"]["url"], None)
            .or_else(|| util::url_of(&video["account"]["url"], None));
        resolved.uploaded_at = util::time(&video["publishedAt"])
            .or_else(|| util::time(&video["originallyPublishedAt"]));
        resolved.duration = (!live)
            .then(|| util::seconds(&video["duration"]).filter(|d| !d.is_zero()))
            .flatten();
        resolved.thumbnail = util::url_of(&video["previewPath"], Some(server))
            .or_else(|| util::url_of(&video["thumbnailPath"], Some(server)));
        resolved.webpage_url = server.join(&format!("w/{short}")).ok();
        resolved.live = live;
        resolved.age_limit = video["nsfw"].as_bool().filter(|n| *n).map(|_| 18);
        resolved.clip = (!live)
            .then(|| timestamp_hint(origin).map(|start| ClipRange { start, end: None }))
            .flatten();
        if !live {
            resolved.subtitles = self.captions(server, &uuid, origin).await;
        }
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    /// The videos a listing endpoint answers with, as entries, and how many there are.
    fn entries_of(server: &Url, listing: &Value) -> (Vec<PlaylistEntry>, Option<usize>) {
        let entries = listing["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                // A playlist lists elements wrapping their videos; channels and accounts
                // list videos directly.
                let video = if item["video"].is_object() {
                    &item["video"]
                } else {
                    item
                };
                let id = util::text(&video["shortUUID"]).or_else(|| util::text(&video["uuid"]))?;
                Some(PlaylistEntry {
                    url: server.join(&format!("w/{id}")).ok()?,
                    title: util::text(&video["name"]).and_then(|t| clean_title(&t)),
                    duration: util::seconds(&video["duration"]).filter(|d| !d.is_zero()),
                })
            })
            .collect();
        (entries, util::uint(&listing["total"]).map(|n| n as usize))
    }

    async fn resolve_listing(
        &self,
        server: &Url,
        base: &str,
        name: &str,
        query: &str,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let listing = self
            .api_expecting(
                server,
                &format!("api/v1/{base}/{name}/videos?start=0&count={LISTING_LIMIT}{query}"),
                &[],
                "data",
                origin,
            )
            .await?;
        let (entries, total) = Self::entries_of(server, &listing);
        let info = match self
            .api(server, &format!("api/v1/{base}/{name}"), &[], origin)
            .await
        {
            Ok(Answer::Json(info)) => info,
            Ok(Answer::NotPeertube) | Err(_) => Value::Null,
        };
        if entries.is_empty() {
            return Err(ResolveError::unavailable(origin, "no videos are listed"));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: util::text(&info["shortUUID"]).or_else(|| Some(name.to_string())),
            title: util::text(&info["displayName"])
                .or_else(|| util::text(&info["name"]))
                .and_then(|t| clean_title(&t))
                .or_else(|| clean_title(name)),
            total: total
                .filter(|t| *t >= entries.len())
                .or(Some(entries.len())),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for PeertubeResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "PeerTube",
            hosts: &["*"],
            features: &[
                "videos",
                "live",
                "channels",
                "accounts",
                "playlists",
                "embeds",
                "any server",
            ],
            formats: &["mp4", "hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::Video, Tag::Live],
            session: SessionSupport::None,
            examples: &[
                "https://framatube.org/w/kkGMgK9ZtnKfYAgnEtQxbv",
                "https://video.blender.org/videos/watch/7ad3cbee-fe5f-41c2-93ea-b108986939f4",
                "https://tilvids.com/videos/embed/wx2iLhD3pTipbKFJKLyx5t",
                "https://peertube.livespotting.com/videos/watch/6735499e-f5bb-4879-a364-19d15abc2bb6",
                "https://peertube.debian.social/w/p/hFdJoTuyhNJVa1cDWd1d12",
                "https://framatube.org/c/joinpeertube/videos",
                "https://video.blender.org/video-channels/blender_open_movies",
                "https://framatube.org/a/framasoft/videos",
                "https://framatube.org/accounts/framasoft",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Video {
                server,
                id,
                password,
            } => {
                self.resolve_video(&server, &id, password.as_deref(), url)
                    .await
            }
            Link::Playlist { server, id } => {
                self.resolve_listing(&server, "video-playlists", &id, "", url)
                    .await
            }
            Link::Channel { server, name } => {
                self.resolve_listing(
                    &server,
                    "video-channels",
                    &name,
                    "&sort=-publishedAt&nsfw=both",
                    url,
                )
                .await
            }
            Link::Account { server, name } => {
                self.resolve_listing(
                    &server,
                    "accounts",
                    &name,
                    "&sort=-publishedAt&nsfw=both",
                    url,
                )
                .await
            }
        }
    }

    /// PeerTube players other pages embed: `<iframe src="https://server/videos/embed/id">`.
    fn embeds_in(&self, page: &Page) -> Vec<Url> {
        page.iframes()
            .into_iter()
            .filter(|frame| {
                matches!(parse_link(frame), Some(Link::Video { .. }))
                    && frame.path().contains("/videos/embed/")
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };
    use serde_json::json;

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

    #[test]
    fn links_shaped_like_peertubes_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let frama = Url::parse("https://framatube.org/").unwrap();
        let video = |id: &str| {
            Some(Link::Video {
                server: frama.clone(),
                id: id.into(),
                password: None,
            })
        };
        assert_eq!(
            link("https://framatube.org/w/kkGMgK9ZtnKfYAgnEtQxbv"),
            video("kkGMgK9ZtnKfYAgnEtQxbv")
        );
        assert_eq!(
            link(
                "https://framatube.org/videos/watch/9c9de5e8-0a1e-484a-b099-e80766180a6d?start=1m2s"
            ),
            video("9c9de5e8-0a1e-484a-b099-e80766180a6d")
        );
        assert_eq!(
            link("https://framatube.org/videos/embed/kkGMgK9ZtnKfYAgnEtQxbv"),
            video("kkGMgK9ZtnKfYAgnEtQxbv")
        );
        assert_eq!(
            link("https://framatube.org/api/v1/videos/kkGMgK9ZtnKfYAgnEtQxbv"),
            video("kkGMgK9ZtnKfYAgnEtQxbv")
        );
        assert_eq!(
            link("https://framatube.org/w/kkGMgK9ZtnKfYAgnEtQxbv?password=pw"),
            Some(Link::Video {
                server: frama.clone(),
                id: "kkGMgK9ZtnKfYAgnEtQxbv".into(),
                password: Some("pw".into())
            })
        );
        let playlist = Some(Link::Playlist {
            server: Url::parse("https://peertube.debian.social/").unwrap(),
            id: "hFdJoTuyhNJVa1cDWd1d12".into(),
        });
        assert_eq!(
            link("https://peertube.debian.social/w/p/hFdJoTuyhNJVa1cDWd1d12"),
            playlist
        );
        assert_eq!(
            link("https://peertube.debian.social/videos/watch/playlist/hFdJoTuyhNJVa1cDWd1d12"),
            playlist
        );
        assert_eq!(
            link("https://peertube.debian.social/video-playlists/hFdJoTuyhNJVa1cDWd1d12"),
            playlist
        );
        let channel = Some(Link::Channel {
            server: frama.clone(),
            name: "joinpeertube".into(),
        });
        assert_eq!(link("https://framatube.org/c/joinpeertube"), channel);
        assert_eq!(link("https://framatube.org/c/joinpeertube/videos"), channel);
        assert_eq!(
            link("https://framatube.org/video-channels/joinpeertube/videos"),
            channel
        );
        assert_eq!(
            link("https://peertube2.cpy.re/c/blender_open_movies@video.blender.org/videos"),
            Some(Link::Channel {
                server: Url::parse("https://peertube2.cpy.re/").unwrap(),
                name: "blender_open_movies@video.blender.org".into(),
            })
        );
        let account = Some(Link::Account {
            server: frama.clone(),
            name: "framasoft".into(),
        });
        assert_eq!(link("https://framatube.org/a/framasoft"), account);
        assert_eq!(link("https://framatube.org/a/framasoft/videos"), account);
        assert_eq!(link("https://framatube.org/accounts/framasoft"), account);
        assert_eq!(link("https://framatube.org/w/short"), None);
        assert_eq!(link("https://framatube.org/w/p/short"), None);
        assert_eq!(link("https://framatube.org/videos/trending"), None);
        assert_eq!(
            link("https://framatube.org/c/joinpeertube/video-playlists"),
            None
        );
        assert_eq!(link("https://framatube.org/"), None);
        assert_eq!(
            link("https://mastodon.social/@user/117256980960208736"),
            None
        );
    }

    fn video_json(files: Value, playlists: Value, live: bool) -> Value {
        json!({
            "id": 1, "uuid": "9c9de5e8-0a1e-484a-b099-e80766180a6d", "shortUUID": "kkGMgK9ZtnKfYAgnEtQxbv",
            "url": "https://framatube.org/videos/watch/9c9de5e8-0a1e-484a-b099-e80766180a6d", "name": "What is PeerTube?",
            "nsfw": false, "privacy": {"id": 1, "label": "Public"}, "description": "A free software to take back control of your videos",
            "truncatedDescription": "A free software", "duration": 113, "publishedAt": "2018-10-01T10:52:46.396Z",
            "thumbnailPath": "/lazy-static/thumbnails/2463a64d.jpg", "previewPath": "/lazy-static/thumbnails/294e3020.jpg",
            "isLive": live, "state": {"id": 1, "label": "Published"}, "blacklisted": false,
            "account": {"url": "https://framatube.org/accounts/framasoft", "name": "framasoft", "displayName": "Framasoft"},
            "channel": {"url": "https://framatube.org/video-channels/joinpeertube", "name": "joinpeertube", "displayName": "A propos de PeerTube"},
            "files": files, "streamingPlaylists": playlists
        })
    }

    fn hls_playlist(files: Value) -> Value {
        json!([{"id": 1, "type": 1, "playlistUrl": "https://framatube.org/static/streaming-playlists/hls/9c9de5e8/master.m3u8", "files": files}])
    }

    fn file(height: u32, size: u64) -> Value {
        json!({"resolution": {"id": height, "label": format!("{height}p")}, "fileUrl": format!("https://framatube.org/static/streaming-playlists/hls/9c9de5e8/9c9de5e8-{height}-fragmented.mp4"),
            "size": size, "fps": 24, "width": if height == 1080 { 1920 } else { 1280 }, "height": height})
    }

    #[tokio::test]
    async fn videos_resolve_with_their_files_and_captions() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://framatube.org/api/v1/videos/kkGMgK9ZtnKfYAgnEtQxbv",
            200,
            "application/json",
            &video_json(json!([]), hls_playlist(json!([file(1080, 16815344), file(720, 10017104), {"resolution": {"id": 0, "label": "0p"}, "fileUrl": "https://framatube.org/static/streaming-playlists/hls/9c9de5e8/9c9de5e8-0-fragmented.mp4", "size": 1800000}])), false).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://framatube.org/api/v1/videos/9c9de5e8-0a1e-484a-b099-e80766180a6d/captions",
            200,
            "application/json",
            &json!({"total": 2, "data": [
                {"language": {"id": "en", "label": "English"}, "automaticallyGenerated": false, "captionPath": "/lazy-static/video-captions/abc-en.vtt", "fileUrl": "https://framatube.org/lazy-static/video-captions/abc-en.vtt"},
                {"language": {"id": "fr", "label": "French"}, "automaticallyGenerated": true, "captionPath": "/lazy-static/video-captions/abc-fr.vtt", "fileUrl": null}
            ]}).to_string(),
        ));
        let resolver = PeertubeResolver::new(Http::replay(fixture));
        let url = Url::parse("https://framatube.org/w/kkGMgK9ZtnKfYAgnEtQxbv?start=1m2s").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(
            resolved.id.as_deref(),
            Some("9c9de5e8-0a1e-484a-b099-e80766180a6d")
        );
        assert_eq!(resolved.title.as_deref(), Some("What is PeerTube?"));
        assert_eq!(resolved.uploader.as_deref(), Some("A propos de PeerTube"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://framatube.org/video-channels/joinpeertube"
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(113)));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://framatube.org/lazy-static/thumbnails/294e3020.jpg"
        );
        assert_eq!(
            resolved.webpage_url.as_ref().unwrap().as_str(),
            "https://framatube.org/w/kkGMgK9ZtnKfYAgnEtQxbv"
        );
        assert_eq!(
            resolved.clip.map(|c| c.start),
            Some(Duration::from_secs(62))
        );
        assert!(!resolved.live);
        assert_eq!(resolved.variants.len(), 3);
        assert_eq!(resolved.variants[0].height, Some(1080));
        assert_eq!(resolved.variants[0].width, Some(1920));
        assert_eq!(resolved.variants[0].fps, Some(24.0));
        assert_eq!(resolved.variants[0].size, Some(16815344));
        assert_eq!(resolved.variants[0].format_id.as_deref(), Some("hls-1080p"));
        assert_eq!(resolved.variants[0].container, Some(Container::Mp4));
        assert_eq!(resolved.variants[0].video, Some(VideoCodec::H264));
        assert!(resolved.variants[2].audio_only);
        assert!(resolved.variants[2].video.is_none());
        assert_eq!(resolved.subtitles.len(), 2);
        assert_eq!(resolved.subtitles[0].language, "en");
        assert_eq!(resolved.subtitles[0].format, SubtitleFormat::Vtt);
        assert!(resolved.subtitles[1].auto);
        assert_eq!(
            resolved.subtitles[1].url.as_str(),
            "https://framatube.org/lazy-static/video-captions/abc-fr.vtt"
        );
    }

    #[tokio::test]
    async fn web_videos_and_live_streams_resolve() {
        let mut fixture = Fixture::new(PLATFORM, None);
        let mut web = video_json(
            json!([{"resolution": {"id": 480, "label": "480p"}, "fileUrl": "https://video.blender.org/object-storage/web_videos/e2adcbdf-480.mp4", "size": 12283676, "fps": 24, "width": 1146, "height": 480}]),
            json!([]),
            false,
        );
        web["uuid"] = json!("7ad3cbee-fe5f-41c2-93ea-b108986939f4");
        web["shortUUID"] = json!("gaGB17Qu1F5tXZyXN6idEQ");
        fixture.exchanges.push(get(
            "https://video.blender.org/api/v1/videos/7ad3cbee-fe5f-41c2-93ea-b108986939f4",
            200,
            "application/json",
            &web.to_string(),
        ));
        fixture.exchanges.push(get(
            "https://video.blender.org/api/v1/videos/7ad3cbee-fe5f-41c2-93ea-b108986939f4/captions",
            200,
            "application/json",
            r#"{"total":0,"data":[]}"#,
        ));
        let mut live = video_json(
            json!([]),
            json!([{"id": 2, "type": 1, "playlistUrl": "https://peertube.livespotting.com/static/streaming-playlists/hls/6735499e/master.m3u8", "files": []}]),
            true,
        );
        live["uuid"] = json!("6735499e-f5bb-4879-a364-19d15abc2bb6");
        live["shortUUID"] = json!("nJnLwK3sYS6vP2Bsv8DDQy");
        live["duration"] = json!(0);
        fixture.exchanges.push(get(
            "https://peertube.livespotting.com/api/v1/videos/6735499e-f5bb-4879-a364-19d15abc2bb6",
            200,
            "application/json",
            &live.to_string(),
        ));
        fixture.exchanges.push(get(
            "https://peertube.livespotting.com/static/streaming-playlists/hls/6735499e/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1920x1080,CODECS=\"avc1.64001f,mp4a.40.2\"\n1080.m3u8\n",
        ));
        fixture.exchanges.push(get(
            "https://peertube.livespotting.com/static/streaming-playlists/hls/6735499e/1080.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n0.mp4\n",
        ));
        let mut waiting = video_json(json!([]), json!([]), true);
        waiting["uuid"] = json!("11111111-1111-1111-1111-111111111111");
        waiting["state"] = json!({"id": 4, "label": "Waiting for live"});
        fixture.exchanges.push(get(
            "https://peertube.livespotting.com/api/v1/videos/11111111-1111-1111-1111-111111111111",
            200,
            "application/json",
            &waiting.to_string(),
        ));
        let resolver = PeertubeResolver::new(Http::replay(fixture));
        let web = resolver
            .resolve(
                &Url::parse(
                    "https://video.blender.org/videos/watch/7ad3cbee-fe5f-41c2-93ea-b108986939f4",
                )
                .unwrap(),
            )
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(web.variants.len(), 1);
        assert_eq!(web.variants[0].format_id.as_deref(), Some("web-480p"));
        assert_eq!(web.variants[0].width, Some(1146));
        assert_eq!(
            web.webpage_url.as_ref().unwrap().as_str(),
            "https://video.blender.org/w/gaGB17Qu1F5tXZyXN6idEQ"
        );
        let live = resolver
            .resolve(
                &Url::parse(
                    "https://peertube.livespotting.com/videos/watch/6735499e-f5bb-4879-a364-19d15abc2bb6",
                )
                .unwrap(),
            )
            .await
            .unwrap()
            .media()
            .unwrap();
        assert!(live.live);
        assert!(live.duration.is_none());
        assert!(live.subtitles.is_empty());
        assert_eq!(live.variants.len(), 1);
        assert!(live.variants[0].live);
        assert_eq!(live.variants[0].kind, VariantKind::Hls);
        assert_eq!(live.variants[0].height, Some(1080));
        assert_eq!(live.variants[0].format_id.as_deref(), Some("hls-1080p"));
        let error = resolver
            .resolve(
                &Url::parse(
                    "https://peertube.livespotting.com/w/11111111-1111-1111-1111-111111111111",
                )
                .unwrap(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("not started")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn channels_accounts_and_playlists_list_their_videos() {
        let mut fixture = Fixture::new(PLATFORM, None);
        let listed = |uuid: &str, short: &str, name: &str| json!({"uuid": uuid, "shortUUID": short, "name": name, "duration": 98});
        fixture.exchanges.push(get(
            "https://framatube.org/api/v1/video-channels/joinpeertube/videos?start=0&count=100&sort=-publishedAt&nsfw=both",
            200,
            "application/json",
            &json!({"total": 15, "data": [listed("4294a720-f263-4ea4-9392-cf9cea4d5277", "9dRFC6Ya11NCVeYKn8ZhiD", "What is the Fediverse?"), listed("9c9de5e8-0a1e-484a-b099-e80766180a6d", "kkGMgK9ZtnKfYAgnEtQxbv", "What is PeerTube?")]}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://framatube.org/api/v1/video-channels/joinpeertube",
            200,
            "application/json",
            &json!({"url": "https://framatube.org/video-channels/joinpeertube", "name": "joinpeertube", "displayName": "A propos de PeerTube"}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://framatube.org/api/v1/accounts/framasoft/videos?start=0&count=100&sort=-publishedAt&nsfw=both",
            200,
            "application/json",
            &json!({"total": 726, "data": [listed("4294a720-f263-4ea4-9392-cf9cea4d5277", "9dRFC6Ya11NCVeYKn8ZhiD", "What is the Fediverse?")]}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://framatube.org/api/v1/accounts/framasoft",
            200,
            "application/json",
            &json!({"url": "https://framatube.org/accounts/framasoft", "name": "framasoft", "displayName": "Framasoft"}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://peertube.debian.social/api/v1/video-playlists/hFdJoTuyhNJVa1cDWd1d12/videos?start=0&count=100",
            200,
            "application/json",
            &json!({"total": 9, "data": [{"id": 4090, "position": 1, "video": listed("e1d88eec-d087-4953-a0b6-df500efd7b68", "tTwD5ZQ484sKuF8P46Lkum", "Free Software")}]}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://peertube.debian.social/api/v1/video-playlists/hFdJoTuyhNJVa1cDWd1d12",
            200,
            "application/json",
            &json!({"uuid": "870c1241-76a0-4380-b395-57fe5f2e92f1", "shortUUID": "hFdJoTuyhNJVa1cDWd1d12", "displayName": "Richard Stallman no Brasil", "videosLength": 9}).to_string(),
        ));
        let resolver = PeertubeResolver::new(Http::replay(fixture));
        let playlist_of = |link: &str| {
            let url = Url::parse(link).unwrap();
            let resolver = &resolver;
            async move {
                match resolver.resolve(&url).await.unwrap() {
                    Resolution::Playlist(playlist) => playlist,
                    other => panic!("expected a playlist, got {other:?}"),
                }
            }
        };
        let channel = playlist_of("https://framatube.org/c/joinpeertube/videos").await;
        assert_eq!(channel.title.as_deref(), Some("A propos de PeerTube"));
        assert_eq!(channel.total, Some(15));
        assert_eq!(channel.entries.len(), 2);
        assert_eq!(
            channel.entries[0].url.as_str(),
            "https://framatube.org/w/9dRFC6Ya11NCVeYKn8ZhiD"
        );
        assert_eq!(channel.entries[0].duration, Some(Duration::from_secs(98)));
        let account = playlist_of("https://framatube.org/accounts/framasoft").await;
        assert_eq!(account.title.as_deref(), Some("Framasoft"));
        assert_eq!(account.total, Some(726));
        let playlist =
            playlist_of("https://peertube.debian.social/w/p/hFdJoTuyhNJVa1cDWd1d12").await;
        assert_eq!(
            playlist.title.as_deref(),
            Some("Richard Stallman no Brasil")
        );
        assert_eq!(playlist.id.as_deref(), Some("hFdJoTuyhNJVa1cDWd1d12"));
        assert_eq!(playlist.total, Some(9));
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://peertube.debian.social/w/tTwD5ZQ484sKuF8P46Lkum"
        );
        assert_eq!(playlist.entries[0].title.as_deref(), Some("Free Software"));
    }

    #[tokio::test]
    async fn servers_that_are_no_peertube_are_handed_on_and_missing_videos_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://blog.example/api/v1/videos/kkGMgK9ZtnKfYAgnEtQxbv",
            404,
            "text/html",
            "<html>not here</html>",
        ));
        fixture.exchanges.push(get(
            "https://api.example/api/v1/video-channels/news/videos?start=0&count=100&sort=-publishedAt&nsfw=both",
            200,
            "application/json",
            r#"{"items": []}"#,
        ));
        fixture.exchanges.push(get(
            "https://framatube.org/api/v1/videos/9c9de5e8-0a1e-484a-b099-e80766180a6e",
            404,
            "application/problem+json",
            r#"{"type":"about:blank","title":"Not Found","detail":"Video not found","status":404,"docs":"https://docs.joinpeertube.org/api-rest-reference.html#operation/getVideo"}"#,
        ));
        fixture.exchanges.push(get(
            "https://framatube.org/api/v1/videos/aaaaaaaaaaaaaaaaaaaaaa",
            400,
            "application/problem+json",
            r#"{"type":"about:blank","title":"Bad Request","detail":"Incorrect request parameters: id","status":400,"docs":"https://docs.joinpeertube.org/api-rest-reference.html#operation/getVideo"}"#,
        ));
        fixture.exchanges.push(get(
            "https://framatube.org/api/v1/videos/bbbbbbbbbbbbbbbbbbbbbb",
            403,
            "application/problem+json",
            r#"{"type":"about:blank","title":"Forbidden","detail":"This video requires a password","status":403,"code":"video_requires_password","docs":"https://docs.joinpeertube.org/"}"#,
        ));
        let resolver = PeertubeResolver::new(Http::replay(fixture));
        let url = |s: &str| Url::parse(s).unwrap();
        assert!(matches!(
            resolver
                .resolve(&url("https://blog.example/w/kkGMgK9ZtnKfYAgnEtQxbv"))
                .await
                .unwrap_err(),
            ResolveError::Unsupported(_)
        ));
        // Remembered: no request is made the second time.
        assert!(matches!(
            resolver
                .resolve(&url("https://blog.example/c/anything"))
                .await
                .unwrap_err(),
            ResolveError::Unsupported(_)
        ));
        assert!(matches!(
            resolver
                .resolve(&url("https://api.example/c/news"))
                .await
                .unwrap_err(),
            ResolveError::Unsupported(_)
        ));
        assert!(matches!(
            resolver
                .resolve(&url(
                    "https://framatube.org/w/9c9de5e8-0a1e-484a-b099-e80766180a6e"
                ))
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        assert!(matches!(
            resolver
                .resolve(&url("https://framatube.org/w/aaaaaaaaaaaaaaaaaaaaaa"))
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(&url("https://framatube.org/w/bbbbbbbbbbbbbbbbbbbbbb"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("Password required")),
            "{error}"
        );
        let page = Page::parse(
            r#"<html><body><iframe src="https://framatube.org/videos/embed/kkGMgK9ZtnKfYAgnEtQxbv" allowfullscreen></iframe><iframe src="https://www.youtube.com/embed/abc"></iframe></body></html>"#,
            &url("https://blog.example/post"),
        );
        let embeds = resolver.embeds_in(&page);
        assert_eq!(embeds.len(), 1);
        assert_eq!(
            embeds[0].as_str(),
            "https://framatube.org/videos/embed/kkGMgK9ZtnKfYAgnEtQxbv"
        );
    }

    /// Every example link resolves live on its server: videos with variants, the live
    /// webcam as a live stream, and the channel, account and playlist with entries.
    #[tokio::test]

    async fn live_examples_resolve() {
        let resolver = PeertubeResolver::new(Http::new(crate::http::HttpConfig::default()));
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
            let resolution = tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&url))
                .await
                .expect("resolution timed out")
                .unwrap();
            match resolution {
                Resolution::Media(resolved) => {
                    assert!(
                        resolved.variants.iter().any(|v| v.is_playable()),
                        "{link}: no variants"
                    );
                    println!(
                        "{link}: {:?} live={} variants={} subtitles={}",
                        resolved.title,
                        resolved.live,
                        resolved.variants.len(),
                        resolved.subtitles.len()
                    );
                }
                Resolution::Playlist(playlist) => {
                    assert!(!playlist.entries.is_empty(), "{link}: no entries");
                    println!(
                        "{link}: {:?} {} entries of {:?}",
                        playlist.title,
                        playlist.entries.len(),
                        playlist.total
                    );
                }
            }
        }
    }
}
