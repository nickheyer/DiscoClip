//! Vimeo videos, unlisted links, embeds and live events, from the configuration the
//! player itself reads, with showcases, channels, groups and user pages as playlists
//! through the site's API. Videos locked with DRM are reported as such.

use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::page::leading_json;
use super::{
    ClipRange, MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionCheck, SessionSupport, SubtitleFormat, SubtitleTrack, Variant, VariantKind,
    clean_title, fetch, hls, path_extension, timestamp_hint,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "vimeo";
const SITE: &str = "https://vimeo.com/";
const PLAYER: &str = "https://player.vimeo.com/video/";
const VIEWER: &str = "https://vimeo.com/_next/viewer";
const API: &str = "https://api.vimeo.com";
const ACCEPT_JSON: &str = "application/json";
const ACCEPT_EVENT: &str = "application/vnd.vimeo.*+json;version=3.4.9";
const ACCEPT_EVENT_VIDEOS: &str = "application/vnd.vimeo.*;version=3.4.1";
/// The cookie a logged-in vimeo.com session carries.
const SESSION_COOKIE: &str = "vimeo";
/// Every DRM system Vimeo licenses locked videos with.
const DRM: &str = "Widevine/FairPlay/PlayReady";
const VIDEO_FIELDS: &str = "config_url,name,description,created_time,release_time,duration,link,user.name,user.link,pictures.base_link,live.status";
const EVENT_FIELDS: &str = "title,uri,schedule,stream_description,stream_privacy.embed,stream_privacy.view,clip_to_play.name,clip_to_play.uri,clip_to_play.config_url,clip_to_play.live.status,streamable_clip.name,streamable_clip.uri,streamable_clip.config_url,streamable_clip.live.status";
const PAGE_SIZE: usize = 100;
const MAX_PAGES: usize = 10;

static RE_HASH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{10}$").unwrap());
static RE_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_.-]+$").unwrap());
static RE_CONFIG_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\bdata-config-url="([^"]+)""#).unwrap());

/// Pages under these first segments are neither videos nor users.
const RESERVED: &[&str] = &[
    "_next",
    "_rv",
    "about",
    "album",
    "api",
    "blog",
    "categories",
    "channels",
    "cookie_policy",
    "create",
    "developer",
    "dmca",
    "embed",
    "enterprise",
    "event",
    "features",
    "groups",
    "help",
    "jobs",
    "join",
    "likes",
    "log_in",
    "login",
    "manage",
    "ondemand",
    "partners",
    "player",
    "press",
    "pricing",
    "privacy",
    "review",
    "search",
    "settings",
    "showcase",
    "site_map",
    "stats",
    "stock",
    "terms",
    "upgrade",
    "upload",
    "video",
    "videos",
    "watch",
    "watchlater",
];

/// What a link names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Video {
        id: String,
        /// The hash that unlocks an unlisted video.
        hash: Option<String>,
    },
    Event {
        id: String,
        hash: Option<String>,
        /// One recording of the event.
        video: Option<String>,
    },
    /// A showcase (once "album"), by number or by slug.
    Showcase(String),
    Channel(String),
    Group(String),
    /// A user's videos.
    User(String),
    /// A custom link such as `vimeo.com/staff/player`, which the site redirects to the
    /// video's own address.
    Custom(Url),
}

fn digits(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_digit())
}

fn hash(text: &str) -> Option<String> {
    RE_HASH.is_match(text).then(|| text.to_string())
}

fn name(text: &str) -> bool {
    RE_NAME.is_match(text) && !RESERVED.contains(&text.to_ascii_lowercase().as_str())
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let query_hash = url
        .query_pairs()
        .find(|(k, _)| k == "h")
        .and_then(|(_, v)| hash(&v));
    let video = |id: &str, h: Option<String>| Link::Video {
        id: id.to_string(),
        hash: h.or_else(|| query_hash.clone()),
    };
    match host {
        "player.vimeo.com" => match segments.as_slice() {
            ["video", id, rest @ ..] if digits(id) => {
                Some(video(id, rest.first().and_then(|h| hash(h))))
            }
            _ => None,
        },
        "vimeo.com" => {
            if let Some(id) = url
                .query_pairs()
                .find(|(k, _)| k == "clip_id")
                .map(|(_, v)| v.into_owned())
                .filter(|id| digits(id))
            {
                return Some(video(&id, None));
            }
            match segments.as_slice() {
                [id] if digits(id) => Some(video(id, None)),
                [id, h] if digits(id) && hash(h).is_some() => Some(video(id, hash(h))),
                ["video" | "videos", id, ..] if digits(id) => Some(video(id, None)),
                ["channels", _, id] if digits(id) => Some(video(id, None)),
                ["groups", _, "videos", id] if digits(id) => Some(video(id, None)),
                ["album" | "showcase", _, "video", id] if digits(id) => Some(video(id, None)),
                ["ondemand", _, id] if digits(id) => Some(video(id, None)),
                ["review", id, h] | [_, "review", id, h] if digits(id) => Some(video(id, hash(h))),
                ["manage", "videos", id, ..] if digits(id) => Some(video(id, None)),
                ["event", id, rest @ ..] if digits(id) => Some(Link::Event {
                    id: id.to_string(),
                    hash: rest
                        .iter()
                        .find_map(|s| hash(s))
                        .or_else(|| query_hash.clone()),
                    video: rest
                        .iter()
                        .position(|s| *s == "videos")
                        .and_then(|i| rest.get(i + 1))
                        .filter(|v| digits(v))
                        .map(|v| v.to_string()),
                }),
                ["album" | "showcase", id] | ["album" | "showcase", id, "embed" | "embed2"] => {
                    Some(Link::Showcase(id.to_string()))
                }
                ["channels", channel] | ["channels", channel, "videos"]
                    if RE_NAME.is_match(channel) =>
                {
                    Some(Link::Channel(channel.to_string()))
                }
                ["groups", group] | ["groups", group, "videos"] if RE_NAME.is_match(group) => {
                    Some(Link::Group(group.to_string()))
                }
                [user] | [user, "videos"] if name(user) => Some(Link::User(user.to_string())),
                [user, slug] if name(user) && RE_NAME.is_match(slug) => {
                    Some(Link::Custom(url.clone()))
                }
                _ => None,
            }
        }
        _ => None,
    }
}

struct Viewer {
    jwt: String,
    expires: Timestamp,
}

pub struct VimeoResolver {
    http: Http,
    viewer: Mutex<Option<Viewer>>,
}

/// When a JWT stops being accepted, from its own claims.
fn jwt_expiry(jwt: &str) -> Option<Timestamp> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    Timestamp::from_second(claims["exp"].as_i64()?).ok()
}

/// The `window.playerConfig = {…}` a player page carries.
pub fn player_config(html: &str) -> Option<Value> {
    let at = html.find("playerConfig")?;
    let rest = &html[at..];
    let assign = rest.find('=')?;
    leading_json(&rest[assign + 1..]).map(|(value, _)| value)
}

fn subtitle_format(url: &Url) -> SubtitleFormat {
    match path_extension(url).as_deref() {
        Some("srt") => SubtitleFormat::Srt,
        Some("ttml") | Some("dfxp") | Some("xml") => SubtitleFormat::Ttml,
        _ => SubtitleFormat::Vtt,
    }
}

impl VimeoResolver {
    pub fn new(http: Http) -> Self {
        Self {
            http,
            viewer: Mutex::new(None),
        }
    }

    fn logged_in(&self) -> bool {
        self.http.jar(PLATFORM).get(SESSION_COOKIE).is_some()
    }

    /// The site's viewer record: a token for its API, and the account the cookies log in.
    async fn fetch_viewer(&self, origin: &Url) -> Result<Value, ResolveError> {
        let url = Url::parse(VIEWER).expect("valid");
        let response = self
            .http
            .get(url.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", ACCEPT_JSON)
            .send()
            .await?;
        let status = response.status;
        if !status.is_success() {
            return Err(match status.as_u16() {
                429 => ResolveError::RateLimited(origin.clone()),
                _ => ResolveError::unavailable(
                    origin,
                    format!("Vimeo gave no viewer token (HTTP {status})"),
                ),
            });
        }
        let value: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, format!("viewer JSON: {e}")))?;
        let jwt = value["jwt"]
            .as_str()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| ResolveError::malformed(origin, "the viewer answer has no token"))?;
        let expires =
            jwt_expiry(jwt).unwrap_or_else(|| Timestamp::now() + Duration::from_secs(5 * 60));
        *self.viewer.lock().unwrap_or_else(|e| e.into_inner()) = Some(Viewer {
            jwt: jwt.to_string(),
            expires,
        });
        Ok(value)
    }

    async fn jwt(&self, origin: &Url) -> Result<String, ResolveError> {
        if let Some(viewer) = self
            .viewer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            && viewer.expires > Timestamp::now() + Duration::from_secs(60)
        {
            return Ok(viewer.jwt.clone());
        }
        self.fetch_viewer(origin).await?;
        let viewer = self.viewer.lock().unwrap_or_else(|e| e.into_inner());
        Ok(viewer.as_ref().map(|v| v.jwt.clone()).unwrap_or_default())
    }

    /// GETs an API path with the viewer token, turning the API's errors into ours.
    async fn api(
        &self,
        path: &str,
        query: &[(&str, &str)],
        accept: &str,
        origin: &Url,
    ) -> Result<Value, ResolveError> {
        let jwt = self.jwt(origin).await?;
        let mut url = Url::parse(&format!("{API}{path}"))
            .map_err(|e| ResolveError::malformed(origin, e.to_string()))?;
        url.query_pairs_mut().extend_pairs(query);
        let response = self
            .http
            .get(url.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("authorization", &format!("jwt {jwt}"))
            .header("accept", accept)
            .send()
            .await?;
        let status = response.status;
        let text = response.text(MAX_PAGE).await?;
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        if status.is_success() {
            return Ok(value);
        }
        let message = value["error"]
            .as_str()
            .or_else(|| value["developer_message"].as_str())
            .filter(|m| !m.trim().is_empty())
            .map(|m| m.trim().to_string())
            .unwrap_or_else(|| format!("the API answered HTTP {status}"));
        let code = value["error_code"].as_i64().unwrap_or(0);
        let wants_password = code == 2204
            || value["invalid_parameters"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|p| p["field"].as_str() == Some("password"));
        Err(match status.as_u16() {
            _ if wants_password => ResolveError::unavailable(origin, "it is password protected"),
            _ if code == 3200 => ResolveError::unavailable(
                origin,
                "it may only be played when embedded on its owner's site",
            ),
            404 if code == 5460 => ResolveError::login_required(origin, PLATFORM, message),
            404 | 410 => ResolveError::NotFound(origin.clone()),
            429 => ResolveError::RateLimited(origin.clone()),
            401 | 403 if self.logged_in() => ResolveError::unavailable(origin, message),
            401 | 403 => ResolveError::login_required(origin, PLATFORM, message),
            _ => ResolveError::unavailable(origin, message),
        })
    }

    /// The player configuration at a signed `config_url`.
    async fn fetch_config(&self, config_url: &str, origin: &Url) -> Result<Value, ResolveError> {
        let url = Url::parse(config_url)
            .map_err(|e| ResolveError::malformed(origin, format!("config URL: {e}")))?;
        let headers = [("referer".to_string(), SITE.to_string())];
        let fetched = fetch(&self.http, &url, PLATFORM, BROWSER_UA, &headers, MAX_PAGE).await?;
        match fetched.status.as_u16() {
            200..=299 => fetched.json(origin),
            404 | 410 => Err(ResolveError::NotFound(origin.clone())),
            429 => Err(ResolveError::RateLimited(origin.clone())),
            status => {
                let message = fetched
                    .json(origin)
                    .ok()
                    .and_then(|v| v["message"].as_str().map(String::from))
                    .unwrap_or_else(|| format!("the player configuration answered HTTP {status}"));
                Err(ResolveError::unavailable(origin, message))
            }
        }
    }

    /// The player configuration from the embed page, which every playable public video
    /// serves, unlisted ones with their hash.
    async fn embed_config(
        &self,
        id: &str,
        hash: Option<&str>,
        origin: &Url,
    ) -> Result<Value, ResolveError> {
        let mut page = Url::parse(&format!("{PLAYER}{id}")).expect("valid");
        if let Some(hash) = hash {
            page.query_pairs_mut().append_pair("h", hash);
        }
        let referer = if origin.host_str() == Some("player.vimeo.com") {
            origin.as_str().to_string()
        } else {
            SITE.to_string()
        };
        let headers = [("referer".to_string(), referer)];
        let fetched = fetch(&self.http, &page, PLATFORM, BROWSER_UA, &headers, MAX_PAGE).await?;
        let html = fetched.text();
        match fetched.status.as_u16() {
            200..=299 => {}
            401 => {
                return Err(ResolveError::login_required(
                    origin,
                    PLATFORM,
                    "the video is private",
                ));
            }
            403 => {
                return Err(ResolveError::unavailable(
                    origin,
                    if html.contains("privacy settings") {
                        "it may only be played when embedded on its owner's site"
                    } else {
                        "the player refused the request (HTTP 403)"
                    },
                ));
            }
            404 | 410 => return Err(ResolveError::NotFound(origin.clone())),
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the player page answered HTTP {status}"),
                ));
            }
        }
        let config = player_config(&html).ok_or_else(|| {
            ResolveError::malformed(origin, "the player page has no configuration")
        })?;
        match config["view"].as_u64() {
            None | Some(1) => Ok(config),
            Some(2) => Err(ResolveError::login_required(
                origin,
                PLATFORM,
                "the video is private",
            )),
            Some(4) => Err(ResolveError::unavailable(
                origin,
                "it is password protected",
            )),
            Some(view) => Err(ResolveError::unavailable(
                origin,
                format!("the player will not play it (view {view})"),
            )),
        }
    }

    /// Media and metadata from a player configuration.
    async fn parse_config(
        &self,
        config: &Value,
        id: &str,
        origin: &Url,
    ) -> Result<Resolved, ResolveError> {
        let video = &config["video"];
        let request = &config["request"];
        let files = if video["files"].is_object() {
            &video["files"]
        } else {
            &request["files"]
        };
        let locked = request["drm"].is_object();
        let live_event = &video["live_event"];
        let live_status = live_event["status"].as_str();
        if matches!(live_status, Some("pending") | Some("active")) {
            let start = live_event["ingest"]["scheduled_start_time"]
                .as_str()
                .map(|s| format!("; it is scheduled for {s}"))
                .unwrap_or_default();
            return Err(ResolveError::unavailable(
                origin,
                format!("the live event has not started{start}"),
            ));
        }
        let live = live_status == Some("started");

        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = video["id"]
            .as_u64()
            .map(|i| i.to_string())
            .or_else(|| Some(id.to_string()));
        resolved.title = video["title"].as_str().and_then(clean_title);
        resolved.duration = video["duration"]
            .as_f64()
            .filter(|d| *d > 0.0)
            .map(Duration::from_secs_f64);
        resolved.uploader = video["owner"]["name"].as_str().and_then(clean_title);
        resolved.uploader_url = video["owner"]["url"]
            .as_str()
            .and_then(|u| Url::parse(u).ok());
        resolved.thumbnail = video["thumbnail_url"]
            .as_str()
            .or_else(|| video["thumbs"]["base"].as_str())
            .and_then(|t| Url::parse(t).ok());
        resolved.webpage_url = video["url"]
            .as_str()
            .and_then(|u| Url::parse(u).ok())
            .or_else(|| Url::parse(&format!("{SITE}{id}")).ok());
        resolved.live = live;
        resolved.clip = timestamp_hint(origin).map(|start| ClipRange { start, end: None });

        let mut variants = Vec::new();
        for file in files["progressive"].as_array().into_iter().flatten() {
            let Some(url) = file["url"].as_str().and_then(|u| Url::parse(u).ok()) else {
                continue;
            };
            let mut v = Variant::new(url, VariantKind::File);
            v.container = Some(Container::Mp4);
            v.video = Some(VideoCodec::H264);
            v.audio = Some(AudioCodec::Aac);
            v.width = file["width"].as_u64().map(|w| w as u32);
            v.height = file["height"].as_u64().map(|h| h as u32);
            v.fps = file["fps"].as_f64().filter(|f| *f > 0.0);
            v.bitrate = file["bitrate"]
                .as_u64()
                .filter(|b| *b > 0)
                .map(|b| b * 1000);
            v.duration = resolved.duration;
            v.format_id = file["quality"].as_str().map(|q| format!("progressive-{q}"));
            v.label = file["quality"].as_str().map(String::from);
            variants.push(v);
        }
        let hls = &files["hls"];
        let master = hls["default_cdn"]
            .as_str()
            .and_then(|cdn| hls["cdns"][cdn]["url"].as_str())
            .or_else(|| {
                hls["cdns"]
                    .as_object()
                    .and_then(|cdns| cdns.values().find_map(|c| c["url"].as_str()))
            })
            .and_then(|u| Url::parse(u).ok());
        if let Some(master) = master {
            let expanded = hls::expand(&self.http, &master, PLATFORM, BROWSER_UA, &[]).await?;
            if resolved.duration.is_none() {
                resolved.duration = expanded.duration;
            }
            resolved.live |= expanded.live;
            resolved.subtitles.extend(expanded.subtitles);
            let mut streams = expanded.variants;
            for v in &mut streams {
                v.live |= live;
            }
            variants.extend(streams);
        }
        let archive = &live_event["archive"];
        if archive["status"].as_str() == Some("done")
            && let Some(source) = archive["source_url"]
                .as_str()
                .and_then(|u| Url::parse(u).ok())
        {
            let mut v = Variant::new(source, VariantKind::File);
            v.container = Some(Container::Mp4);
            v.duration = resolved.duration;
            v.format_id = Some("live-archive-source".into());
            variants.push(v);
        }
        if variants.is_empty() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        if locked {
            for v in &mut variants {
                v.drm = Some(DRM.to_string());
            }
        }
        let player_base = Url::parse(PLAYER).expect("valid");
        for track in request["text_tracks"].as_array().into_iter().flatten() {
            let (Some(language), Some(track_url)) = (
                track["lang"].as_str(),
                track["url"].as_str().and_then(|u| player_base.join(u).ok()),
            ) else {
                continue;
            };
            if resolved.subtitles.iter().any(|s| s.url == track_url) {
                continue;
            }
            resolved.subtitles.push(SubtitleTrack {
                format: subtitle_format(&track_url),
                url: track_url,
                language: language.to_string(),
                name: track["label"].as_str().map(String::from),
                auto: track["provenance"].as_str().is_some_and(|p| {
                    p.contains("auto") || p.contains("asr") || p.contains("machine")
                }),
                headers: Vec::new(),
            });
        }
        resolved.variants = variants;
        Ok(resolved)
    }

    /// The API's record of a video: the signed configuration for a logged-in session, and
    /// the description and dates the player leaves out. `None` when the API keeps quiet.
    async fn api_video(
        &self,
        id: &str,
        hash: Option<&str>,
        origin: &Url,
    ) -> Result<Option<Value>, ResolveError> {
        let path = match hash {
            Some(hash) => format!("/videos/{id}:{hash}"),
            None => format!("/videos/{id}"),
        };
        match self
            .api(&path, &[("fields", VIDEO_FIELDS)], ACCEPT_JSON, origin)
            .await
        {
            Ok(value) => Ok(Some(value)),
            Err(ResolveError::NotFound(_)) | Err(ResolveError::Unavailable { .. }) => {
                tracing::debug!(%origin, "the Vimeo API has no record of the video");
                Ok(None)
            }
            Err(ResolveError::LoginRequired { .. }) => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn video(
        &self,
        id: &str,
        hash: Option<&str>,
        origin: &Url,
    ) -> Result<Resolved, ResolveError> {
        let record = self.api_video(id, hash, origin).await?;
        let config = match record.as_ref().and_then(|r| r["config_url"].as_str()) {
            Some(config_url) => self.fetch_config(config_url, origin).await?,
            None => self.embed_config(id, hash, origin).await?,
        };
        let mut resolved = self.parse_config(&config, id, origin).await?;
        if let Some(record) = record {
            if resolved.title.is_none() {
                resolved.title = record["name"].as_str().and_then(clean_title);
            }
            resolved.description = record["description"].as_str().and_then(clean_title);
            resolved.uploaded_at = record["release_time"]
                .as_str()
                .or_else(|| record["created_time"].as_str())
                .and_then(|t| t.parse::<Timestamp>().ok());
            if resolved.uploader.is_none() {
                resolved.uploader = record["user"]["name"].as_str().and_then(clean_title);
            }
            if resolved.uploader_url.is_none() {
                resolved.uploader_url = record["user"]["link"]
                    .as_str()
                    .and_then(|u| Url::parse(u).ok());
            }
            if resolved.thumbnail.is_none() {
                resolved.thumbnail = record["pictures"]["base_link"]
                    .as_str()
                    .and_then(|u| Url::parse(u).ok());
            }
            if resolved.webpage_url.is_none() {
                resolved.webpage_url = record["link"].as_str().and_then(|u| Url::parse(u).ok());
            }
        }
        Ok(resolved)
    }

    /// A live event: its running stream, or the recording a link picks out.
    async fn event(
        &self,
        id: &str,
        hash: Option<&str>,
        video: Option<&str>,
        origin: &Url,
    ) -> Result<Resolved, ResolveError> {
        let key = match hash {
            Some(hash) => format!("{id}:{hash}"),
            None => id.to_string(),
        };
        let event = self
            .api(
                &format!("/live_events/{key}"),
                &[
                    ("fields", EVENT_FIELDS),
                    ("clip_to_play_id", video.unwrap_or("0")),
                ],
                ACCEPT_EVENT,
                origin,
            )
            .await?;
        let view = event["stream_privacy"]["view"].as_str().unwrap_or("");
        if view == "nobody" {
            return Err(ResolveError::unavailable(
                origin,
                "the event has not been made available to anyone",
            ));
        }
        let clip = &event["clip_to_play"];
        let clip_status = clip["live"]["status"].as_str();
        if clip_status == Some("unavailable") {
            let start = event["schedule"]["start_time"].as_str();
            let upcoming = start
                .and_then(|s| s.parse::<Timestamp>().ok())
                .is_some_and(|at| at > Timestamp::now());
            return Err(ResolveError::unavailable(
                origin,
                match (upcoming, start) {
                    (true, Some(start)) => {
                        format!("the live event has not started; it is scheduled for {start}")
                    }
                    (false, Some(start)) => {
                        format!("the live event has nothing to play; it was scheduled for {start}")
                    }
                    (_, None) => "the live event has nothing to play yet".to_string(),
                },
            ));
        }
        let mut config_url = clip["config_url"].as_str().map(String::from);
        if config_url.is_none() && view == "embed_only" {
            let mut page = format!("{SITE}event/{id}/embed");
            if let Some(hash) = hash {
                page.push('/');
                page.push_str(hash);
            }
            let page = Url::parse(&page).expect("valid");
            let fetched = fetch(&self.http, &page, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
            let html = fetched.text();
            config_url = RE_CONFIG_URL
                .captures(&html)
                .map(|c| c[1].replace("&amp;", "&"));
        }
        if config_url.is_none() {
            let recordings = self
                .api(
                    &format!("/live_events/{key}/videos"),
                    &[
                        ("fields", "items,uri,name,config_url,duration,live.status"),
                        ("page", "1"),
                        ("per_page", "100"),
                    ],
                    ACCEPT_EVENT_VIDEOS,
                    origin,
                )
                .await?;
            let wanted = recordings
                .get("data")
                .and_then(|d| d.as_array())
                .into_iter()
                .flatten()
                .find(|v| match video {
                    Some(video) => v["uri"].as_str().is_some_and(|uri| {
                        uri.trim_start_matches("/videos/").split(':').next() == Some(video)
                    }),
                    None => v["live"]["status"].as_str() != Some("unavailable"),
                });
            config_url = wanted.and_then(|v| v["config_url"].as_str().map(String::from));
        }
        let config_url = config_url.ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
        let config = self.fetch_config(&config_url, origin).await?;
        let mut resolved = self.parse_config(&config, id, origin).await?;
        if resolved.title.is_none() {
            resolved.title = event["title"].as_str().and_then(clean_title);
        }
        resolved.description = event["stream_description"].as_str().and_then(clean_title);
        resolved.webpage_url = Some(origin.clone());
        Ok(resolved)
    }

    /// The numeric id behind a showcase slug.
    async fn showcase_id(&self, slug: &str, origin: &Url) -> Result<String, ResolveError> {
        if digits(slug) {
            return Ok(slug.to_string());
        }
        let auth = Url::parse(&format!("{SITE}showcase/{slug}/auth"))
            .map_err(|e| ResolveError::malformed(origin, e.to_string()))?;
        let headers = [
            ("x-requested-with".to_string(), "XMLHttpRequest".to_string()),
            ("accept".to_string(), ACCEPT_JSON.to_string()),
        ];
        let fetched = fetch(&self.http, &auth, PLATFORM, BROWSER_UA, &headers, MAX_PAGE).await?;
        if matches!(fetched.status.as_u16(), 404 | 410) {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        let value = fetched.json(origin)?;
        value["metadata"]["id"]
            .as_u64()
            .map(|id| id.to_string())
            .or_else(|| value["metadata"]["id"].as_str().map(String::from))
            .ok_or_else(|| ResolveError::NotFound(origin.clone()))
    }

    /// A showcase, channel, group or user's videos, page by page.
    async fn collection(&self, link: &Link, origin: &Url) -> Result<Playlist, ResolveError> {
        let (about, id) = match link {
            Link::Showcase(slug) => {
                let id = self.showcase_id(slug, origin).await?;
                (format!("/albums/{id}"), id)
            }
            Link::Channel(name) => (format!("/channels/{name}"), name.clone()),
            Link::Group(name) => (format!("/groups/{name}"), name.clone()),
            Link::User(name) => (format!("/users/{name}"), name.clone()),
            Link::Video { .. } | Link::Event { .. } | Link::Custom(_) => {
                unreachable!("only collections are listed")
            }
        };
        let info = self
            .api(
                &about,
                &[("fields", "name,uri,link,metadata.connections.videos.total")],
                ACCEPT_JSON,
                origin,
            )
            .await?;
        let title = info["name"].as_str().and_then(clean_title);
        let total = info["metadata"]["connections"]["videos"]["total"]
            .as_u64()
            .map(|t| t as usize);
        let per_page = PAGE_SIZE.to_string();
        let mut entries: Vec<PlaylistEntry> = Vec::new();
        for page in 1..=MAX_PAGES {
            let answer = self
                .api(
                    &format!("{about}/videos"),
                    &[
                        ("fields", "uri,name,duration"),
                        ("per_page", &per_page),
                        ("page", &page.to_string()),
                    ],
                    ACCEPT_JSON,
                    origin,
                )
                .await?;
            let before = entries.len();
            for video in answer["data"].as_array().into_iter().flatten() {
                let Some(uri) = video["uri"]
                    .as_str()
                    .and_then(|u| u.strip_prefix("/videos/"))
                else {
                    continue;
                };
                let (video_id, video_hash) = match uri.split_once(':') {
                    Some((id, hash)) => (id, Some(hash)),
                    None => (uri, None),
                };
                let link = match video_hash {
                    Some(hash) => format!("{SITE}{video_id}/{hash}"),
                    None => format!("{SITE}{video_id}"),
                };
                let Ok(link) = Url::parse(&link) else {
                    continue;
                };
                if entries.iter().any(|e| e.url == link) {
                    continue;
                }
                entries.push(PlaylistEntry {
                    url: link,
                    title: video["name"].as_str().and_then(clean_title),
                    duration: video["duration"]
                        .as_f64()
                        .filter(|d| *d > 0.0)
                        .map(Duration::from_secs_f64),
                });
            }
            if answer["paging"]["next"].is_null() || entries.len() == before {
                break;
            }
        }
        if entries.is_empty() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        Ok(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(id),
            title,
            total: total.filter(|t| *t > entries.len()),
            entries,
        })
    }

    /// Where a custom link leads: the site answers with the video's own address.
    async fn custom(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let response = self
            .http
            .get(url.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .follow_redirects(false)
            .send()
            .await?;
        if let Some(location) = response.header("location")
            && let Ok(target) = url.join(location)
            && target != *url
            && matches!(parse_link(&target), Some(Link::Video { .. }))
        {
            return Err(ResolveError::Redirect(target));
        }
        Err(match response.status.as_u16() {
            429 => ResolveError::RateLimited(url.clone()),
            _ => ResolveError::NotFound(url.clone()),
        })
    }
}

#[async_trait]
impl Resolver for VimeoResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Vimeo",
            hosts: &["vimeo.com", "player.vimeo.com"],
            features: &[
                "videos",
                "unlisted links",
                "embeds",
                "custom links",
                "live events",
                "showcases",
                "channels",
                "groups",
                "user videos",
                "subtitles",
                "drm reported",
            ],
            formats: &["hls", "mp4"],
            session: SessionSupport::Optional,
            examples: &[
                "https://vimeo.com/22439234",
                "https://player.vimeo.com/video/22439234",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        match &link {
            Link::Video { id, hash } => Ok(Resolution::from(
                self.video(id, hash.as_deref(), url).await?,
            )),
            Link::Event { id, hash, video } => Ok(Resolution::from(
                self.event(id, hash.as_deref(), video.as_deref(), url)
                    .await?,
            )),
            Link::Showcase(_) | Link::Channel(_) | Link::Group(_) | Link::User(_) => {
                Ok(Resolution::Playlist(self.collection(&link, url).await?))
            }
            Link::Custom(custom) => self.custom(custom).await,
        }
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        if !self.logged_in() {
            return Ok(SessionCheck::LoggedOut);
        }
        let origin = Url::parse(SITE).expect("valid");
        let viewer = self.fetch_viewer(&origin).await?;
        let user = &viewer["user"];
        Ok(
            match user
                .get("name")
                .or_else(|| user.get("display_name"))
                .or_else(|| user.get("uri"))
                .and_then(|v| v.as_str())
            {
                Some(account) if user.is_object() => SessionCheck::LoggedIn {
                    account: account.to_string(),
                },
                _ if user.is_object() => SessionCheck::LoggedIn {
                    account: "a Vimeo account".to_string(),
                },
                _ => SessionCheck::LoggedOut,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };
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

    fn viewer() -> Exchange {
        get(VIEWER, 200, "application/json", json!({"user": null, "jwt": "eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9.eyJleHAiOjQ3ODkxODk1NjAsInVzZXJfaWQiOm51bGwsInNjb3BlcyI6InB1YmxpYyJ9.sig", "xsrft": "x"}).to_string())
    }

    fn api_video(id: &str) -> Exchange {
        get(&format!("https://api.vimeo.com/videos/{id}?fields={}", VIDEO_FIELDS.replace(',', "%2C")), 200, "application/json", json!({
            "name": "The Mountain", "description": "Time lapse", "link": "https://vimeo.com/22439234", "duration": 185,
            "created_time": "2011-04-15T18:08:29+00:00", "release_time": "2011-04-15T18:08:29+00:00",
            "user": {"name": "TSO Photography", "link": "https://vimeo.com/terjes"}, "pictures": {"base_link": "https://i.vimeocdn.com/video/1.jpg"}, "status": "available"
        }).to_string())
    }

    fn player_page(config: Value) -> String {
        format!(
            "<!DOCTYPE html><html><head><title>Vimeo</title></head><body><script>window.playerConfig = {config};\nwindow.other = 1;</script></body></html>"
        )
    }

    fn config(drm: bool) -> Value {
        json!({
            "cdn_url": "https://f.vimeocdn.com", "view": 1,
            "video": {"id": 22439234, "title": "The Mountain", "duration": 185, "width": 1920, "height": 1080, "fps": 30,
                      "owner": {"name": "TSO Photography", "url": "https://vimeo.com/terjes"}, "thumbnail_url": "https://i.vimeocdn.com/video/145027281-d",
                      "live_event": null, "privacy": "anybody", "url": "https://vimeo.com/22439234"},
            "request": {
                "drm": if drm { json!({"user": 0, "asset": "a", "fallback_cdms": {"fairplay": {}}}) } else { Value::Null },
                "files": {
                    "progressive": [],
                    "hls": {"default_cdn": "akfire_interconnect_quic", "cdns": {"akfire_interconnect_quic": {"url": "https://vod-adaptive-ak.vimeocdn.com/exp=1/v2/playlist/av/primary/playlist.m3u8?pathsig=x", "origin": "gcs"}}}
                },
                "text_tracks": [{"id": 170, "lang": "de", "url": "/texttrack/170.vtt?token=a", "kind": "subtitles", "label": "Deutsch", "provenance": "user_uploaded"}]
            }
        })
    }

    const MASTER: &str = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio-high\",NAME=\"English\",DEFAULT=YES,LANGUAGE=\"en\",URI=\"../../../a/avf/1/media.m3u8?st=audio\"\n#EXT-X-STREAM-INF:BANDWIDTH=3063040,AVERAGE-BANDWIDTH=2644000,RESOLUTION=1920x1080,FRAME-RATE=29.970,CODECS=\"avc1.64002A,mp4a.40.2\",AUDIO=\"audio-high\"\n../../../a/avf/1/media.m3u8?st=video\n#EXT-X-STREAM-INF:BANDWIDTH=464090,RESOLUTION=480x270,CODECS=\"avc1.42C01E,mp4a.40.2\",AUDIO=\"audio-high\"\n../../../a/avf/2/media.m3u8?st=video\n";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:6.0,\n0.m4s\n#EXTINF:4.5,\n1.m4s\n#EXT-X-ENDLIST\n";

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let video = |id: &str, hash: Option<&str>| {
            Some(Link::Video {
                id: id.into(),
                hash: hash.map(String::from),
            })
        };
        assert_eq!(link("https://vimeo.com/22439234"), video("22439234", None));
        assert_eq!(
            link("https://vimeo.com/22439234/abcdef0123"),
            video("22439234", Some("abcdef0123"))
        );
        assert_eq!(
            link("https://player.vimeo.com/video/22439234?h=abcdef0123&autoplay=1"),
            video("22439234", Some("abcdef0123"))
        );
        assert_eq!(
            link("https://vimeo.com/channels/staffpicks/22439234"),
            video("22439234", None)
        );
        assert_eq!(
            link("https://vimeo.com/groups/motion/videos/22439234"),
            video("22439234", None)
        );
        assert_eq!(
            link("https://vimeo.com/showcase/123/video/22439234"),
            video("22439234", None)
        );
        assert_eq!(
            link("https://vimeo.com/someone/review/22439234/abcdef0123"),
            video("22439234", Some("abcdef0123"))
        );
        assert_eq!(
            link("https://vimeo.com/moogaloop.swf?clip_id=22439234"),
            video("22439234", None)
        );
        assert_eq!(
            link("https://vimeo.com/event/4567/videos/22439234"),
            Some(Link::Event {
                id: "4567".into(),
                hash: None,
                video: Some("22439234".into())
            })
        );
        assert_eq!(
            link("https://vimeo.com/event/4567/embed/abcdef0123"),
            Some(Link::Event {
                id: "4567".into(),
                hash: Some("abcdef0123".into()),
                video: None
            })
        );
        assert_eq!(
            link("https://vimeo.com/showcase/11967401"),
            Some(Link::Showcase("11967401".into()))
        );
        assert_eq!(
            link("https://vimeo.com/album/11967401/embed"),
            Some(Link::Showcase("11967401".into()))
        );
        assert_eq!(
            link("https://vimeo.com/channels/staffpicks"),
            Some(Link::Channel("staffpicks".into()))
        );
        assert_eq!(
            link("https://vimeo.com/groups/motion/videos"),
            Some(Link::Group("motion".into()))
        );
        assert_eq!(
            link("https://vimeo.com/terjes/videos"),
            Some(Link::User("terjes".into()))
        );
        assert_eq!(
            link("https://vimeo.com/staff/player"),
            Some(Link::Custom(
                Url::parse("https://vimeo.com/staff/player").unwrap()
            ))
        );
        assert_eq!(link("https://vimeo.com/upgrade"), None);
        assert_eq!(link("https://vimeo.com/"), None);
        assert_eq!(link("https://example.com/22439234"), None);
    }

    #[tokio::test]
    async fn public_videos_come_from_the_player_page_with_api_metadata() {
        let mut fixture = Fixture::new("vimeo", None);
        fixture.exchanges.push(viewer());
        fixture.exchanges.push(api_video("22439234"));
        fixture.exchanges.push(get(
            "https://player.vimeo.com/video/22439234",
            200,
            "text/html",
            player_page(config(false)),
        ));
        fixture.exchanges.push(get("https://vod-adaptive-ak.vimeocdn.com/exp=1/v2/playlist/av/primary/playlist.m3u8?pathsig=x", 200, "application/vnd.apple.mpegurl", MASTER.into()));
        fixture.exchanges.push(get(
            "https://vod-adaptive-ak.vimeocdn.com/exp=1/v2/a/avf/1/media.m3u8?st=video",
            200,
            "application/vnd.apple.mpegurl",
            MEDIA.into(),
        ));
        let resolver = VimeoResolver::new(Http::replay(fixture));
        let url = Url::parse("https://vimeo.com/22439234#t=1m5s").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.title.as_deref(), Some("The Mountain"));
        assert_eq!(resolved.description.as_deref(), Some("Time lapse"));
        assert_eq!(resolved.uploader.as_deref(), Some("TSO Photography"));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.duration, Some(Duration::from_secs(185)));
        assert_eq!(resolved.clip.unwrap().start, Duration::from_secs(65));
        assert_eq!(resolved.variants.len(), 2);
        let best = &resolved.variants[0];
        assert_eq!(best.kind, VariantKind::Hls);
        assert_eq!(best.height, Some(1080));
        assert!(
            best.audio_url
                .as_ref()
                .unwrap()
                .as_str()
                .ends_with("/a/avf/1/media.m3u8?st=audio")
        );
        assert!(best.drm.is_none());
        assert_eq!(resolved.subtitles.len(), 1);
        assert_eq!(
            resolved.subtitles[0].url.as_str(),
            "https://player.vimeo.com/texttrack/170.vtt?token=a"
        );
        assert_eq!(resolved.subtitles[0].language, "de");
        assert!(!resolved.subtitles[0].auto);
    }

    #[tokio::test]
    async fn locked_private_password_and_missing_videos_say_so() {
        let mut fixture = Fixture::new("vimeo", None);
        fixture.exchanges.push(viewer());
        fixture.exchanges.push(get(
            "https://api.vimeo.com/videos/76979871",
            404,
            "application/json",
            json!({"error": "The requested video could not be found"}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://player.vimeo.com/video/76979871",
            200,
            "text/html",
            player_page(config(true)),
        ));
        fixture.exchanges.push(get("https://vod-adaptive-ak.vimeocdn.com/exp=1/v2/playlist/av/primary/playlist.m3u8?pathsig=x", 200, "application/vnd.apple.mpegurl", MASTER.into()));
        fixture.exchanges.push(get(
            "https://vod-adaptive-ak.vimeocdn.com/exp=1/v2/a/avf/1/media.m3u8?st=video",
            200,
            "application/vnd.apple.mpegurl",
            MEDIA.into(),
        ));
        fixture.exchanges.push(get(
            "https://api.vimeo.com/videos/1084537",
            404,
            "application/json",
            "{}".into(),
        ));
        fixture.exchanges.push(get(
            "https://player.vimeo.com/video/1084537",
            401,
            "text/html",
            "<html>private</html>".into(),
        ));
        fixture.exchanges.push(get(
            "https://api.vimeo.com/videos/56015672",
            404,
            "application/json",
            "{}".into(),
        ));
        fixture.exchanges.push(get(
            "https://player.vimeo.com/video/56015672",
            200,
            "text/html",
            player_page(json!({"view": 4, "video": {"id": 56015672}, "request": {}})),
        ));
        fixture.exchanges.push(get(
            "https://api.vimeo.com/videos/108734851",
            404,
            "application/json",
            "{}".into(),
        ));
        fixture.exchanges.push(get(
            "https://player.vimeo.com/video/108734851",
            404,
            "text/html",
            "<html>Sorry, this video does not exist.</html>".into(),
        ));
        let resolver = VimeoResolver::new(Http::replay(fixture));
        let resolve = |id: &str| {
            let resolver = &resolver;
            let url = Url::parse(&format!("https://vimeo.com/{id}")).unwrap();
            async move { resolver.resolve(&url).await }
        };
        let locked = resolve("76979871").await.unwrap().media().unwrap();
        assert_eq!(locked.drm(), Some(DRM));
        assert!(locked.variants.iter().all(|v| !v.is_playable()));
        assert!(matches!(
            resolve("1084537").await.unwrap_err(),
            ResolveError::LoginRequired { .. }
        ));
        assert!(
            matches!(resolve("56015672").await.unwrap_err(), ResolveError::Unavailable { reason, .. } if reason.contains("password"))
        );
        assert!(matches!(
            resolve("108734851").await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn showcases_list_their_videos_and_custom_links_redirect() {
        let mut fixture = Fixture::new("vimeo", None);
        fixture.exchanges.push(viewer());
        fixture.exchanges.push(get("https://api.vimeo.com/albums/11967401?fields=name%2Curi%2Clink%2Cmetadata.connections.videos.total", 200, "application/json", json!({
            "uri": "/users/152184/albums/11967401", "name": "Prod round up", "metadata": {"connections": {"videos": {"total": 3}}}
        }).to_string()));
        fixture.exchanges.push(get("https://api.vimeo.com/albums/11967401/videos?fields=uri%2Cname%2Cduration&per_page=100&page=1", 200, "application/json", json!({
            "paging": {"next": null}, "data": [
                {"uri": "/videos/1225609993", "name": "Robbery", "link": "https://vimeo.com/1225609993", "duration": 236},
                {"uri": "/videos/1225400313:abcdef0123", "name": "Devil", "link": "https://vimeo.com/1225400313", "duration": 100}
            ]
        }).to_string()));
        fixture.exchanges.push(get(
            "https://vimeo.com/staff/player",
            301,
            "text/html",
            String::new(),
        ));
        fixture.exchanges[3]
            .response
            .headers
            .push(("location".into(), "https://vimeo.com/76979871".into()));
        let resolver = VimeoResolver::new(Http::replay(fixture));
        let playlist = match resolver
            .resolve(&Url::parse("https://vimeo.com/showcase/11967401").unwrap())
            .await
            .unwrap()
        {
            Resolution::Playlist(playlist) => playlist,
            other => panic!("{other:?}"),
        };
        assert_eq!(playlist.title.as_deref(), Some("Prod round up"));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[1].url.as_str(),
            "https://vimeo.com/1225400313/abcdef0123"
        );
        assert_eq!(playlist.total, Some(3));
        let error = resolver
            .resolve(&Url::parse("https://vimeo.com/staff/player").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Redirect(u) if u.as_str() == "https://vimeo.com/76979871"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn live_events_play_their_current_clip_or_say_when_they_start() {
        let mut fixture = Fixture::new("vimeo", None);
        fixture.exchanges.push(viewer());
        fixture.exchanges.push(get(&format!("https://api.vimeo.com/live_events/4567?fields={}&clip_to_play_id=0", EVENT_FIELDS.replace(',', "%2C")), 200, "application/json", json!({
            "title": "Town hall", "uri": "/live_events/4567", "stream_description": "Every month", "schedule": {"start_time": "2026-01-01T10:00:00Z"},
            "stream_privacy": {"view": "anybody", "embed": "public"},
            "clip_to_play": {"uri": "/videos/22439234", "config_url": "https://player.vimeo.com/video/22439234/config?s=signed", "live": {"status": "streaming"}}
        }).to_string()));
        let mut live_config = config(false);
        live_config["video"]["live_event"] =
            json!({"status": "started", "ingest": {"start_time": 1767261600}});
        fixture.exchanges.push(get(
            "https://player.vimeo.com/video/22439234/config?s=signed",
            200,
            "application/json",
            live_config.to_string(),
        ));
        fixture.exchanges.push(get("https://vod-adaptive-ak.vimeocdn.com/exp=1/v2/playlist/av/primary/playlist.m3u8?pathsig=x", 200, "application/vnd.apple.mpegurl", MASTER.into()));
        fixture.exchanges.push(get(
            "https://vod-adaptive-ak.vimeocdn.com/exp=1/v2/a/avf/1/media.m3u8?st=video",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\n0.m4s\n".into(),
        ));
        fixture.exchanges.push(get(&format!("https://api.vimeo.com/live_events/8910?fields={}&clip_to_play_id=0", EVENT_FIELDS.replace(',', "%2C")), 200, "application/json", json!({
            "title": "Later", "stream_privacy": {"view": "anybody"}, "schedule": {"start_time": "2030-05-05T10:00:00Z"},
            "clip_to_play": {"uri": "/videos/1", "live": {"status": "unavailable"}}
        }).to_string()));
        let resolver = VimeoResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://vimeo.com/event/4567").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert!(resolved.live);
        assert_eq!(resolved.title.as_deref(), Some("The Mountain"));
        assert_eq!(resolved.description.as_deref(), Some("Every month"));
        assert_eq!(
            resolved.webpage_url.unwrap().as_str(),
            "https://vimeo.com/event/4567"
        );
        assert!(resolved.variants.iter().all(|v| v.live));
        let error = resolver
            .resolve(&Url::parse("https://vimeo.com/event/8910").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("2030-05-05")),
            "{error}"
        );
    }

    #[test]
    fn player_config_and_jwt_claims_are_read() {
        let html = player_page(json!({"view": 1, "video": {"id": 5, "title": "x = {y}"}}));
        let config = player_config(&html).unwrap();
        assert_eq!(config["video"]["title"], "x = {y}");
        assert!(player_config("<html></html>").is_none());
        let expiry =
            jwt_expiry("eyJ0eXAiOiJKV1QiLCJhbGciOiJIUzI1NiJ9.eyJleHAiOjE3ODkxODk1NjB9.sig")
                .unwrap();
        assert_eq!(expiry.as_second(), 1789189560);
    }
}
