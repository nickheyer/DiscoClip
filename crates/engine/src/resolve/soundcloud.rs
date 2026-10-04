//! SoundCloud tracks, sets, users and likes through the v2 API the web app reads. Every
//! call carries the client id the web app's scripts embed, which is read from the home
//! page's last script on the first request and read again when the API refuses it. A
//! track's transcodings (progressive MP3, HLS AAC and Opus) each name a stream endpoint
//! that answers with the signed CDN link. Private share links carry a secret token that
//! goes with every request for the track, `on.soundcloud.com` short links are unwrapped,
//! and `api.soundcloud.com` and player embed links name tracks and playlists by id.

use std::sync::{LazyLock, Mutex};

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionCheck, SessionSupport, Tag, Variant, VariantKind, clean_title, hls, parse_codecs,
    probe_file, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind};

pub const PLATFORM: &str = "soundcloud";
const API: &str = "https://api-v2.soundcloud.com/";
const SITE: &str = "https://soundcloud.com/";
/// How many entries a listing is read up to: one API page.
const LISTING_LIMIT: usize = 200;
/// How many track ids one `tracks?ids=` request names.
const IDS_PER_REQUEST: usize = 50;

/// `client_id:"…"` in the web app's scripts.
static RE_CLIENT_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"client_id\s*:\s*"([0-9a-zA-Z]{32})""#).unwrap());
/// The web app's script tags, in page order.
static RE_SCRIPT_SRC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<script[^>]+src="(https://a-v2\.sndcdn\.com/[^"]+\.js)""#).unwrap()
});
/// `-large.jpg` and its kin at the end of an artwork link.
static RE_ARTWORK_SIZE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"-[0-9a-z]+\.(jpg|png)$").unwrap());
/// `.128.mp3`, `.64.opus`: the bitrate a stream link names.
static RE_STREAM_BITRATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\.(\d+)\.(?:opus|mp3|m4a|aac)(?:[/?]|$)").unwrap());
/// `aac_160k`: the bitrate a preset names.
static RE_PRESET_BITRATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"_(\d+)k$").unwrap());
/// `s-8Pjrp`: a secret token in a share link.
static RE_SECRET: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^s-[A-Za-z0-9]+$").unwrap());
static RE_SLUG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\w.-]+$").unwrap());

/// The pages of a user's profile, each a listing of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserPage {
    /// Tracks and sets together, as the profile's front page shows them.
    All,
    Tracks,
    Albums,
    Sets,
    Reposts,
    Likes,
    Spotlight,
}

impl UserPage {
    fn from_segment(segment: &str) -> Option<Self> {
        Some(match segment {
            "tracks" | "popular-tracks" => UserPage::Tracks,
            "albums" => UserPage::Albums,
            "sets" => UserPage::Sets,
            "reposts" => UserPage::Reposts,
            "likes" => UserPage::Likes,
            "spotlight" => UserPage::Spotlight,
            _ => return None,
        })
    }

    fn label(self) -> &'static str {
        match self {
            UserPage::All => "all",
            UserPage::Tracks => "tracks",
            UserPage::Albums => "albums",
            UserPage::Sets => "sets",
            UserPage::Reposts => "reposts",
            UserPage::Likes => "likes",
            UserPage::Spotlight => "spotlight",
        }
    }

    /// The API path of the listing, under the user's id.
    fn endpoint(self, user_id: u64) -> String {
        match self {
            UserPage::All => format!("stream/users/{user_id}"),
            UserPage::Tracks => format!("users/{user_id}/tracks"),
            UserPage::Albums => format!("users/{user_id}/albums"),
            UserPage::Sets => format!("users/{user_id}/playlists"),
            UserPage::Reposts => format!("stream/users/{user_id}/reposts"),
            UserPage::Likes => format!("users/{user_id}/likes"),
            UserPage::Spotlight => format!("users/{user_id}/spotlight"),
        }
    }
}

/// The lists a track page links beside the track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    Albums,
    Sets,
    Recommended,
}

impl Relation {
    fn from_segment(segment: &str) -> Option<Self> {
        Some(match segment {
            "albums" => Relation::Albums,
            "sets" => Relation::Sets,
            "recommended" => Relation::Recommended,
            _ => return None,
        })
    }

    fn label(self) -> &'static str {
        match self {
            Relation::Albums => "albums",
            Relation::Sets => "sets",
            Relation::Recommended => "recommended",
        }
    }

    fn endpoint(self, track_id: u64) -> String {
        match self {
            Relation::Albums => format!("tracks/{track_id}/albums"),
            Relation::Sets => format!("tracks/{track_id}/playlists_without_albums"),
            Relation::Recommended => format!("tracks/{track_id}/related"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// `/{user}/{slug}`, with the secret token of a private share link.
    Track {
        user: String,
        slug: String,
        token: Option<String>,
    },
    /// `api.soundcloud.com/tracks/{id}`, with a `secret_token`.
    TrackId { id: u64, token: Option<String> },
    /// `/{user}/sets/{slug}`, with the secret token of a private share link.
    Set {
        user: String,
        slug: String,
        token: Option<String>,
    },
    /// `api.soundcloud.com/playlists/{id}`, with a `secret_token`.
    PlaylistId { id: u64, token: Option<String> },
    /// A user's profile, or one page of it.
    User { user: String, page: UserPage },
    /// `api.soundcloud.com/users/{id}`.
    UserId { id: u64 },
    /// `/{user}/{slug}/albums`, `/sets` or `/recommended`.
    Related {
        user: String,
        slug: String,
        relation: Relation,
    },
    /// An `on.soundcloud.com` short link, unwrapped by its redirect.
    Short(Url),
}

/// Paths on the main site that are pages of the app rather than a user's.
const RESERVED: &[&str] = &[
    "discover",
    "search",
    "stream",
    "upload",
    "you",
    "signin",
    "login",
    "logout",
    "settings",
    "notifications",
    "messages",
    "charts",
    "pages",
    "terms-of-use",
    "imprint",
    "jobs",
    "mobile",
    "pro",
    "premium",
    "go",
    "tags",
    "people",
    "groups",
    "explore",
    "feed",
    "stations",
    "popular",
    "oembed",
    "player",
    "trending",
    "artist-tools",
    "creators",
    "blog",
    "help",
];

fn segments(url: &Url) -> Vec<String> {
    url.path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .map(|s| {
            percent_encoding::percent_decode_str(s)
                .decode_utf8_lossy()
                .into_owned()
        })
        .collect()
}

/// An `api.soundcloud.com/tracks/soundcloud%3Atracks%3A123` id, with or without its urn.
fn api_id(segment: &str, kind: &str) -> Option<u64> {
    let bare = segment
        .strip_prefix(&format!("soundcloud:{kind}:"))
        .unwrap_or(segment);
    bare.parse().ok()
}

/// A link on the API hosts: a track, playlist or user by id, with the `secret_token`
/// the query carries.
fn parse_api_link(url: &Url) -> Option<Link> {
    let segments = segments(url);
    let token = util::query_param(url, "secret_token").filter(|t| !t.is_empty());
    match segments.as_slice() {
        [kind, id] if kind == "tracks" => Some(Link::TrackId {
            id: api_id(id, "tracks")?,
            token,
        }),
        [kind, id] if kind == "playlists" => Some(Link::PlaylistId {
            id: api_id(id, "playlists")?,
            token,
        }),
        [kind, id] if kind == "users" => Some(Link::UserId {
            id: api_id(id, "users")?,
        }),
        _ => None,
    }
}

/// A player embed: `w.soundcloud.com/player/?url=…`, whose `url` names the track, set
/// or page and whose `secret_token` unlocks a private one.
fn parse_embed(url: &Url) -> Option<Link> {
    if !url.path().trim_end_matches('/').ends_with("/player") {
        return None;
    }
    let inner = util::query_param(url, "url")?;
    let mut inner = Url::parse(&inner).ok()?;
    if let Some(token) = util::query_param(url, "secret_token").filter(|t| !t.is_empty())
        && util::query_param(&inner, "secret_token").is_none()
    {
        inner.query_pairs_mut().append_pair("secret_token", &token);
    }
    parse_link(&inner)
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    match host.as_str() {
        "on.soundcloud.com" => {
            let segments = segments(url);
            return match segments.as_slice() {
                [code] if RE_SLUG.is_match(code) => Some(Link::Short(url.clone())),
                _ => None,
            };
        }
        "api.soundcloud.com" | "api-v2.soundcloud.com" => return parse_api_link(url),
        "w.soundcloud.com" | "player.soundcloud.com" | "p.soundcloud.com" => {
            return parse_embed(url);
        }
        "soundcloud.com" | "www.soundcloud.com" | "m.soundcloud.com" => {}
        _ => return None,
    }
    let segments = segments(url);
    let user = segments.first()?.clone();
    if !RE_SLUG.is_match(&user) || RESERVED.contains(&user.as_str()) {
        return None;
    }
    match segments.as_slice() {
        [_] => Some(Link::User {
            user,
            page: UserPage::All,
        }),
        [_, second] => {
            if let Some(page) = UserPage::from_segment(second) {
                return Some(Link::User { user, page });
            }
            if !RE_SLUG.is_match(second) || second == "comments" {
                return None;
            }
            Some(Link::Track {
                user,
                slug: second.clone(),
                token: None,
            })
        }
        [_, second, third] if second == "sets" => {
            if !RE_SLUG.is_match(third) {
                return None;
            }
            Some(Link::Set {
                user,
                slug: third.clone(),
                token: None,
            })
        }
        [_, second, third] => {
            if !RE_SLUG.is_match(second) {
                return None;
            }
            if let Some(relation) = Relation::from_segment(third) {
                return Some(Link::Related {
                    user,
                    slug: second.clone(),
                    relation,
                });
            }
            if !RE_SECRET.is_match(third) {
                return None;
            }
            Some(Link::Track {
                user,
                slug: second.clone(),
                token: Some(third.clone()),
            })
        }
        [_, second, third, fourth] if second == "sets" => {
            if !RE_SLUG.is_match(third) || !RE_SECRET.is_match(fourth) {
                return None;
            }
            Some(Link::Set {
                user,
                slug: third.clone(),
                token: Some(fourth.clone()),
            })
        }
        _ => None,
    }
}

/// The artwork at 500 pixels, the largest size served for every track.
pub fn artwork(track: &Value) -> Option<Url> {
    let raw = track["artwork_url"]
        .as_str()
        .or_else(|| track["user"]["avatar_url"].as_str())?;
    let large = RE_ARTWORK_SIZE.replace(raw, "-t500x500.$1");
    Url::parse(&large).ok()
}

/// What a transcoding's preset and mime type say about its stream.
struct Encoding {
    protocol: &'static str,
    audio: Option<AudioCodec>,
    container: Option<Container>,
    bitrate: Option<u64>,
}

/// `aac_160k`, `mp3_1_0`, `opus_0_0` with `audio/mp4; codecs="mp4a.40.2"`: the codec,
/// container and bitrate a transcoding names, or nothing for a locked one.
fn encoding_of(transcoding: &Value) -> Option<Encoding> {
    let preset = transcoding["preset"].as_str()?;
    let format = &transcoding["format"];
    let protocol_raw = format["protocol"].as_str().unwrap_or("http");
    let url = transcoding["url"].as_str().unwrap_or("");
    if protocol_raw.starts_with("ctr-")
        || protocol_raw.starts_with("cbc-")
        || protocol_raw == "encrypted-hls"
        || url.contains("/encrypted-hls")
    {
        return None;
    }
    let protocol = if protocol_raw == "hls" || url.contains("/hls") {
        "hls"
    } else {
        "http"
    };
    let mime = format["mime_type"].as_str().unwrap_or("");
    let codecs = mime
        .split_once("codecs=")
        .map(|(_, c)| c.trim_matches('"').to_string());
    let base = preset.split('_').next().unwrap_or(preset);
    let audio = codecs
        .as_deref()
        .and_then(|c| parse_codecs(Some(c)).1)
        .or(match base {
            "mp3" => Some(AudioCodec::Mp3),
            "aac" => Some(AudioCodec::Aac),
            "opus" => Some(AudioCodec::Opus),
            _ => None,
        });
    let container = match (&audio, protocol) {
        (Some(AudioCodec::Mp3), "http") => Some(Container::Mp3),
        (Some(AudioCodec::Aac), "http") => Some(Container::M4a),
        (Some(AudioCodec::Opus), "http") => Some(Container::Opus),
        _ => None,
    };
    let bitrate = RE_PRESET_BITRATE
        .captures(preset)
        .and_then(|c| c[1].parse::<u64>().ok())
        .map(|k| k * 1000);
    Some(Encoding {
        protocol,
        audio,
        container,
        bitrate,
    })
}

/// A playlist entry for a track or set the API lists, or for the item a stream or likes
/// entry wraps.
fn entry_of(item: &Value) -> Option<PlaylistEntry> {
    let inner = if item["track"].is_object() {
        &item["track"]
    } else if item["playlist"].is_object() {
        &item["playlist"]
    } else {
        item
    };
    let url = util::url_of(&inner["permalink_url"], None)?;
    Some(PlaylistEntry {
        url,
        title: inner["title"].as_str().and_then(clean_title),
        duration: util::millis(&inner["duration"]),
    })
}

fn parse_time(value: &Value) -> Option<Timestamp> {
    value.as_str().and_then(|t| t.parse::<Timestamp>().ok())
}

pub struct SoundcloudResolver {
    http: Http,
    client_id: Mutex<Option<String>>,
}

impl SoundcloudResolver {
    pub fn new(http: Http) -> Self {
        Self {
            http,
            client_id: Mutex::new(None),
        }
    }

    /// The OAuth token a logged-in session's cookie holds.
    fn oauth_token(&self) -> Option<String> {
        self.http
            .jar(PLATFORM)
            .get("oauth_token")
            .map(|c| c.value.clone())
            .filter(|t| !t.is_empty())
    }

    /// Reads the client id the web app's scripts carry, last script first.
    async fn discover_client_id(&self, origin: &Url) -> Result<String, ResolveError> {
        let home = Url::parse(SITE).expect("valid");
        let page = super::fetch_ok(&self.http, &home, PLATFORM, BROWSER_UA, &[], MAX_PAGE)
            .await
            .map_err(|e| e.at(origin))?;
        let html = page.text();
        let scripts: Vec<String> = RE_SCRIPT_SRC
            .captures_iter(&html)
            .map(|c| c[1].to_string())
            .collect();
        for src in scripts.iter().rev() {
            let Ok(script_url) = Url::parse(src) else {
                continue;
            };
            let fetched =
                super::fetch(&self.http, &script_url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
            if !fetched.status.is_success() {
                continue;
            }
            if let Some(id) = util::search(&RE_CLIENT_ID, &fetched.text()) {
                tracing::debug!(script = %script_url, "SoundCloud client id read");
                return Ok(id);
            }
        }
        Err(ResolveError::malformed(
            origin,
            "no script of the web app carries a client id",
        ))
    }

    async fn client_id(&self, origin: &Url) -> Result<String, ResolveError> {
        if let Some(id) = self
            .client_id
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Ok(id);
        }
        let id = self.discover_client_id(origin).await?;
        *self.client_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.clone());
        Ok(id)
    }

    /// Forgets a client id the API refused.
    fn forget_client_id(&self) {
        *self.client_id.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// One request to `endpoint` (a path under the API, or a full link) with `query`
    /// and the client id, answered as JSON. A refusal of the client id reads a fresh
    /// one from the web app and asks once more.
    async fn api(
        &self,
        endpoint: &str,
        query: &[(&str, &str)],
        origin: &Url,
    ) -> Result<Value, ResolveError> {
        let base = if endpoint.starts_with("http") {
            Url::parse(endpoint)
        } else {
            Url::parse(&format!("{API}{endpoint}"))
        }
        .map_err(|e| ResolveError::malformed(origin, format!("API link: {e}")))?;
        for attempt in 0..2 {
            let client_id = self.client_id(origin).await?;
            let mut pairs: Vec<(&str, &str)> = query.to_vec();
            pairs.push(("client_id", client_id.as_str()));
            let url = util::with_query(&base, &pairs);
            let mut request = self
                .http
                .get(url.clone())
                .platform(PLATFORM)
                .impersonate()
                .header("accept", "application/json")
                .header("origin", "https://soundcloud.com")
                .header("referer", SITE);
            if let Some(token) = self.oauth_token() {
                request = request.header("authorization", &format!("OAuth {token}"));
            }
            let response = request.send().await?;
            let status = response.status.as_u16();
            match status {
                200..=299 => {
                    return response
                        .json(MAX_PAGE)
                        .await
                        .map_err(|e| ResolveError::malformed(origin, format!("API JSON: {e}")));
                }
                401 | 403 if attempt == 0 => {
                    tracing::debug!(%url, status, "SoundCloud refused the client id. Reading a fresh one.");
                    self.forget_client_id();
                    continue;
                }
                404 => return Err(ResolveError::NotFound(origin.clone())),
                429 => return Err(ResolveError::RateLimited(origin.clone())),
                _ => {
                    return Err(ResolveError::unavailable(
                        origin,
                        format!("the API answered HTTP {status}"),
                    ));
                }
            }
        }
        Err(ResolveError::unavailable(
            origin,
            "the API refuses every client id the web app carries",
        ))
    }

    /// The API's own record for a page of the site.
    async fn resolve_path(&self, path: &str, origin: &Url) -> Result<Value, ResolveError> {
        let page = format!("{SITE}{path}");
        self.api("resolve", &[("url", page.as_str())], origin).await
    }

    /// The stream link a transcoding's endpoint answers with.
    async fn stream_url(
        &self,
        endpoint: &str,
        token: Option<&str>,
        origin: &Url,
    ) -> Result<Option<Url>, ResolveError> {
        let mut query: Vec<(&str, &str)> = Vec::new();
        if let Some(token) = token {
            query.push(("secret_token", token));
        }
        match self.api(endpoint, &query, origin).await {
            Ok(answer) => Ok(util::url_of(&answer["url"], None)),
            Err(ResolveError::NotFound(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The original file, when the uploader allows downloads.
    async fn download_variant(&self, track: &Value, token: Option<&str>) -> Option<Variant> {
        let id = util::uint(&track["id"])?;
        if !util::boolean(&track["downloadable"]).unwrap_or(false)
            || !util::boolean(&track["has_downloads_left"]).unwrap_or(false)
        {
            return None;
        }
        let origin = util::url_of(&track["permalink_url"], None)?;
        let endpoint = format!("tracks/{id}/download");
        let mut query: Vec<(&str, &str)> = Vec::new();
        if let Some(token) = token {
            query.push(("secret_token", token));
        }
        let answer = match self.api(&endpoint, &query, &origin).await {
            Ok(answer) => answer,
            Err(error) => {
                tracing::debug!(id, "SoundCloud original download not offered: {error}");
                return None;
            }
        };
        let redirect = util::url_of(&answer["redirectUri"], None)?;
        let probed = probe_file(&self.http, &redirect, PLATFORM, BROWSER_UA, &[])
            .await
            .ok()?;
        if !matches!(probed.status.as_u16(), 200 | 206) {
            return None;
        }
        let name = probed
            .filename
            .clone()
            .unwrap_or_else(|| probed.url.path().to_string());
        let container = Container::from_name(&name).or_else(|| {
            probed
                .content_type
                .as_deref()
                .and_then(Container::from_mime)
        });
        let mut variant = Variant::new(probed.url.clone(), VariantKind::File);
        variant.audio_only = true;
        variant.audio = container.as_ref().and_then(|c| match c {
            Container::Mp3 => Some(AudioCodec::Mp3),
            Container::M4a => Some(AudioCodec::Aac),
            Container::Flac => Some(AudioCodec::Flac),
            Container::Wav => Some(AudioCodec::Other("pcm".into())),
            Container::Ogg => Some(AudioCodec::Vorbis),
            Container::Opus => Some(AudioCodec::Opus),
            _ => None,
        });
        variant.container = container;
        variant.size = probed.size;
        variant.format_id = Some("download".into());
        variant.label = Some("original".into());
        Some(variant)
    }

    /// The variants a track's transcodings make, each through its stream endpoint, and
    /// whether any transcoding was locked with DRM.
    async fn variants_of(
        &self,
        track: &Value,
        token: Option<&str>,
        origin: &Url,
    ) -> Result<(Vec<Variant>, bool), ResolveError> {
        let duration = util::millis(&track["duration"]);
        let mut variants: Vec<Variant> = Vec::new();
        let mut locked = false;
        for transcoding in track["media"]["transcodings"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let Some(endpoint) = transcoding["url"]
                .as_str()
                .filter(|u| u.starts_with("http"))
            else {
                continue;
            };
            let Some(preset) = transcoding["preset"].as_str() else {
                continue;
            };
            if preset.starts_with("abr") {
                continue;
            }
            let Some(encoding) = encoding_of(transcoding) else {
                locked = true;
                continue;
            };
            let Some(stream) = self.stream_url(endpoint, token, origin).await? else {
                tracing::debug!(preset, "SoundCloud transcoding has no stream");
                continue;
            };
            if variants.iter().any(|v| v.url == stream) {
                continue;
            }
            let preview = util::boolean(&transcoding["snipped"]).unwrap_or(false)
                || endpoint.contains("/preview/");
            let mut variant = if encoding.protocol == "hls" {
                match hls::expand(&self.http, &stream, PLATFORM, BROWSER_UA, &[]).await {
                    Ok(expanded) => {
                        let mut variant = expanded
                            .variants
                            .into_iter()
                            .next()
                            .unwrap_or_else(|| Variant::hls(stream.clone()));
                        if variant.duration.is_none() {
                            variant.duration = expanded.duration;
                        }
                        variant
                    }
                    Err(error) => {
                        tracing::debug!(preset, "SoundCloud HLS playlist not read: {error}");
                        Variant::hls(stream.clone())
                    }
                }
            } else {
                Variant::file(stream.clone())
            };
            variant.audio_only = true;
            if variant.audio.is_none() {
                variant.audio = encoding.audio.clone();
            }
            if variant.container.is_none() {
                variant.container = encoding.container.clone();
            }
            variant.bitrate = encoding.bitrate.or_else(|| {
                util::search(&RE_STREAM_BITRATE, stream.as_str())
                    .and_then(|k| k.parse::<u64>().ok())
                    .map(|k| k * 1000)
            });
            if variant.duration.is_none() {
                variant.duration = util::millis(&transcoding["duration"]).or(duration);
            }
            let quality = transcoding["quality"].as_str().unwrap_or("");
            variant.format_id = Some(if preview {
                format!("{}_{preset}_preview", encoding.protocol)
            } else {
                format!("{}_{preset}", encoding.protocol)
            });
            variant.label = Some(match (quality == "hq", preview) {
                (true, _) => format!("{preset} (Go+)"),
                (false, true) => format!("{preset} preview"),
                (false, false) => preset.to_string(),
            });
            variants.push(variant);
        }
        if let Some(original) = self.download_variant(track, token).await {
            variants.push(original);
        }
        Ok((variants, locked))
    }

    async fn resolve_track(
        &self,
        track: Value,
        token: Option<&str>,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        if track["kind"].as_str() != Some("track") {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        let token = track["secret_token"]
            .as_str()
            .map(String::from)
            .or_else(|| token.map(String::from));
        let (variants, locked) = self.variants_of(&track, token.as_deref(), origin).await?;
        if variants.is_empty() {
            if locked {
                return Err(ResolveError::drm(origin, "SoundCloud"));
            }
            if track["policy"].as_str() == Some("BLOCK") {
                return Err(ResolveError::unavailable(
                    origin,
                    "the track is withheld in this region",
                ));
            }
            if track["state"].as_str().is_some_and(|s| s != "finished") {
                return Err(ResolveError::unavailable(
                    origin,
                    "the track is still processing",
                ));
            }
            return Err(ResolveError::unavailable(origin, "the track has no stream"));
        }
        let mut resolved = Resolved::of(PLATFORM, MediaKind::Audio);
        resolved.id = util::uint(&track["id"]).map(|id| id.to_string());
        resolved.title = track["title"].as_str().and_then(clean_title);
        resolved.description = track["description"]
            .as_str()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(String::from);
        resolved.uploader = track["user"]["username"].as_str().and_then(clean_title);
        resolved.uploader_url = util::url_of(&track["user"]["permalink_url"], None);
        resolved.uploaded_at = parse_time(&track["created_at"]);
        resolved.duration =
            util::millis(&track["duration"]).or_else(|| variants.iter().find_map(|v| v.duration));
        resolved.thumbnail = artwork(&track);
        resolved.webpage_url =
            util::url_of(&track["permalink_url"], None).or_else(|| Some(origin.clone()));
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    /// The tracks of a set that the set itself lists only by id, read in batches.
    async fn fill_tracks(
        &self,
        set: &Value,
        token: Option<&str>,
        origin: &Url,
    ) -> Result<Vec<Value>, ResolveError> {
        let listed: Vec<Value> = set["tracks"].as_array().cloned().unwrap_or_default();
        let missing: Vec<u64> = listed
            .iter()
            .filter(|t| t["permalink_url"].as_str().is_none())
            .filter_map(|t| util::uint(&t["id"]))
            .collect();
        if missing.is_empty() {
            return Ok(listed);
        }
        let set_id = util::uint(&set["id"]).map(|id| id.to_string());
        let mut fetched: Vec<Value> = Vec::new();
        for chunk in missing.chunks(IDS_PER_REQUEST) {
            let ids = chunk
                .iter()
                .map(|id| id.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let mut query: Vec<(&str, &str)> = vec![("ids", ids.as_str())];
            if let Some(set_id) = set_id.as_deref() {
                query.push(("playlistId", set_id));
            }
            if let Some(token) = token {
                query.push(("playlistSecretToken", token));
            }
            let answer = self.api("tracks", &query, origin).await?;
            fetched.extend(answer.as_array().cloned().unwrap_or_default());
        }
        Ok(listed
            .into_iter()
            .map(|track| {
                if track["permalink_url"].as_str().is_some() {
                    return track;
                }
                let id = util::uint(&track["id"]);
                fetched
                    .iter()
                    .find(|f| util::uint(&f["id"]) == id)
                    .cloned()
                    .unwrap_or(track)
            })
            .collect())
    }

    async fn resolve_set(
        &self,
        set: Value,
        token: Option<&str>,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        if set["kind"].as_str() != Some("playlist") {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        let token = set["secret_token"]
            .as_str()
            .map(String::from)
            .or_else(|| token.map(String::from));
        let tracks = self.fill_tracks(&set, token.as_deref(), origin).await?;
        let entries: Vec<PlaylistEntry> = tracks
            .iter()
            .filter_map(|track| {
                let id = util::uint(&track["id"])?;
                let url = util::url_of(&track["permalink_url"], None).or_else(|| {
                    let mut url =
                        Url::parse(&format!("https://api.soundcloud.com/tracks/{id}")).ok()?;
                    if let Some(token) = token.as_deref() {
                        url.query_pairs_mut().append_pair("secret_token", token);
                    }
                    Some(url)
                })?;
                Some(PlaylistEntry {
                    url,
                    title: track["title"].as_str().and_then(clean_title),
                    duration: util::millis(&track["duration"]),
                })
            })
            .collect();
        if entries.is_empty() {
            return Err(ResolveError::unavailable(origin, "the set has no tracks"));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: util::uint(&set["id"]).map(|id| id.to_string()),
            title: set["title"].as_str().and_then(clean_title),
            total: util::uint(&set["track_count"])
                .map(|n| n as usize)
                .filter(|n| *n >= entries.len())
                .or(Some(entries.len())),
            entries,
        }))
    }

    /// A listing under `endpoint`, read page by page up to [`LISTING_LIMIT`] entries.
    async fn listing(
        &self,
        endpoint: &str,
        id: Option<String>,
        title: Option<String>,
        total: Option<usize>,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let mut entries: Vec<PlaylistEntry> = Vec::new();
        let limit = LISTING_LIMIT.to_string();
        let mut next: Option<String> = Some(endpoint.to_string());
        let mut first = true;
        while let Some(page_url) = next.take() {
            let answer = if first {
                first = false;
                self.api(
                    &page_url,
                    &[("limit", limit.as_str()), ("linked_partitioning", "1")],
                    origin,
                )
                .await?
            } else {
                self.api(&page_url, &[], origin).await?
            };
            for item in answer["collection"].as_array().into_iter().flatten() {
                if let Some(entry) = entry_of(item)
                    && !entries.iter().any(|e| e.url == entry.url)
                {
                    entries.push(entry);
                }
            }
            if entries.len() >= LISTING_LIMIT {
                break;
            }
            next = answer["next_href"].as_str().map(String::from);
        }
        if entries.is_empty() {
            return Err(ResolveError::unavailable(origin, "the list is empty"));
        }
        entries.truncate(LISTING_LIMIT);
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id,
            title,
            total: total
                .filter(|n| *n >= entries.len())
                .or(Some(entries.len())),
            entries,
        }))
    }

    async fn resolve_user(
        &self,
        user: Value,
        page: UserPage,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        if user["kind"].as_str() != Some("user") {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        let id = util::uint(&user["id"])
            .ok_or_else(|| ResolveError::malformed(origin, "the user has no id"))?;
        let name = user["username"]
            .as_str()
            .and_then(clean_title)
            .unwrap_or_else(|| id.to_string());
        let total = match page {
            UserPage::Tracks => util::uint(&user["track_count"]),
            UserPage::Sets | UserPage::Albums => util::uint(&user["playlist_count"]),
            UserPage::Likes => util::uint(&user["likes_count"]),
            UserPage::Reposts => util::uint(&user["reposts_count"]),
            UserPage::All | UserPage::Spotlight => None,
        }
        .map(|n| n as usize);
        self.listing(
            &page.endpoint(id),
            Some(format!("{id}-{}", page.label())),
            Some(format!("{name} ({})", page.label())),
            total,
            origin,
        )
        .await
    }

    async fn resolve_related(
        &self,
        user: &str,
        slug: &str,
        relation: Relation,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let track = self.resolve_path(&format!("{user}/{slug}"), origin).await?;
        if track["kind"].as_str() != Some("track") {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        let id = util::uint(&track["id"])
            .ok_or_else(|| ResolveError::malformed(origin, "the track has no id"))?;
        let title = track["title"]
            .as_str()
            .and_then(clean_title)
            .unwrap_or_else(|| slug.to_string());
        self.listing(
            &relation.endpoint(id),
            Some(format!("{id}-{}", relation.label())),
            Some(format!("{title} ({})", relation.label())),
            None,
            origin,
        )
        .await
    }

    async fn resolve_link(&self, link: Link, origin: &Url) -> Result<Resolution, ResolveError> {
        match link {
            Link::Track { user, slug, token } => {
                let mut path = format!("{user}/{slug}");
                if let Some(token) = &token {
                    path.push('/');
                    path.push_str(token);
                }
                let track = self.resolve_path(&path, origin).await?;
                self.resolve_track(track, token.as_deref(), origin).await
            }
            Link::TrackId { id, token } => {
                let mut query: Vec<(&str, &str)> = Vec::new();
                if let Some(token) = token.as_deref() {
                    query.push(("secret_token", token));
                }
                let track = self.api(&format!("tracks/{id}"), &query, origin).await?;
                self.resolve_track(track, token.as_deref(), origin).await
            }
            Link::Set { user, slug, token } => {
                let mut path = format!("{user}/sets/{slug}");
                if let Some(token) = &token {
                    path.push('/');
                    path.push_str(token);
                }
                let set = self.resolve_path(&path, origin).await?;
                self.resolve_set(set, token.as_deref(), origin).await
            }
            Link::PlaylistId { id, token } => {
                let mut query: Vec<(&str, &str)> = Vec::new();
                if let Some(token) = token.as_deref() {
                    query.push(("secret_token", token));
                }
                let set = self.api(&format!("playlists/{id}"), &query, origin).await?;
                self.resolve_set(set, token.as_deref(), origin).await
            }
            Link::User { user, page } => {
                let record = self.resolve_path(&user, origin).await?;
                self.resolve_user(record, page, origin).await
            }
            Link::UserId { id } => {
                let record = self.api(&format!("users/{id}"), &[], origin).await?;
                self.resolve_user(record, UserPage::Tracks, origin).await
            }
            Link::Related {
                user,
                slug,
                relation,
            } => self.resolve_related(&user, &slug, relation, origin).await,
            Link::Short(short) => {
                let target = self
                    .http
                    .unwrap_redirects(&short, Some(PLATFORM), BROWSER_UA)
                    .await?;
                if target == short {
                    return Err(ResolveError::NotFound(origin.clone()));
                }
                match parse_link(&target) {
                    Some(Link::Short(_)) | None => Err(ResolveError::Redirect(target)),
                    Some(link) => Box::pin(self.resolve_link(link, origin)).await,
                }
            }
        }
    }
}

#[async_trait]
impl Resolver for SoundcloudResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "SoundCloud",
            hosts: &[
                "soundcloud.com",
                "on.soundcloud.com",
                "api.soundcloud.com",
                "api-v2.soundcloud.com",
                "w.soundcloud.com",
            ],
            features: &[
                "tracks",
                "sets",
                "playlists",
                "users",
                "likes",
                "reposts",
                "related tracks",
                "private share links",
                "short links",
                "embeds",
            ],
            formats: &["mp3", "hls"],
            media: &[MediaKind::Audio],
            tags: &[Tag::Basic, Tag::Music],
            session: SessionSupport::Optional,
            on_by_default: true,
            examples: &[
                "https://soundcloud.com/ethmusic/lostin-powers-she-so-heavy",
                "https://soundcloud.com/jaimemf/youtube-dl-test-video-a-y-baw/s-8Pjrp",
                "https://on.soundcloud.com/2iQZMwo9IQ8wLY3vf9",
                "https://api.soundcloud.com/tracks/62986583",
                "https://soundcloud.com/the-concept-band/sets/the-royal-concept-ep",
                "https://w.soundcloud.com/player/?url=https%3A//api.soundcloud.com/playlists/2284613",
                "https://soundcloud.com/soft-cell-official",
                "https://soundcloud.com/soft-cell-official/tracks",
                "https://soundcloud.com/soft-cell-official/sets",
                "https://soundcloud.com/clalberg/likes",
                "https://soundcloud.com/ethmusic/lostin-powers-she-so-heavy/recommended",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        self.resolve_link(link, url).await
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        if self.oauth_token().is_none() {
            return Ok(SessionCheck::LoggedOut);
        }
        let origin = Url::parse(SITE).expect("valid");
        match self.api("me", &[], &origin).await {
            Ok(me) => Ok(
                match me["username"]
                    .as_str()
                    .or_else(|| me["permalink"].as_str())
                    .filter(|u| !u.is_empty())
                {
                    Some(account) => SessionCheck::LoggedIn {
                        account: account.to_string(),
                    },
                    None => SessionCheck::LoggedOut,
                },
            ),
            Err(ResolveError::Unavailable { .. }) | Err(ResolveError::NotFound(_)) => {
                Ok(SessionCheck::LoggedOut)
            }
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

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

    const CLIENT_ID: &str = "3uJIGBRwdofKn6QKzONvDxUM1Vs4bTv9";

    /// The home page and the two scripts it names: the last one carries the client id.
    fn web_app(fixture: &mut Fixture) {
        fixture.exchanges.push(get(
            "https://soundcloud.com/",
            200,
            "text/html",
            r#"<html><head><script crossorigin src="https://a-v2.sndcdn.com/assets/0-26d11547.js"></script><script crossorigin src="https://a-v2.sndcdn.com/assets/55-a0f130aa.js"></script></head></html>"#.into(),
        ));
        fixture.exchanges.push(get(
            "https://a-v2.sndcdn.com/assets/55-a0f130aa.js",
            200,
            "application/javascript",
            format!(
                r#"!function(){{var e={{client_id:"{CLIENT_ID}",baseUrl:"https://api-v2.soundcloud.com"}}}}()"#
            ),
        ));
    }

    fn track_json() -> Value {
        json!({
            "id": 62986583, "kind": "track", "title": "Lostin Powers - She so Heavy (SneakPreview) Adrian Ackers Blueprint 1",
            "permalink_url": "https://soundcloud.com/ethmusic/lostin-powers-she-so-heavy", "duration": 143206,
            "created_at": "2012-10-11T01:56:38Z", "artwork_url": "https://i1.sndcdn.com/artworks-000031955188-rwb18x-large.jpg",
            "downloadable": false, "policy": "MONETIZE", "state": "finished", "streamable": true, "secret_token": null,
            "description": "  A sneak preview.  ", "track_authorization": "eyJ0eXAiOiJKV1QifQ",
            "user": {"id": 1, "username": "E.T. ExTerrestrial Music", "permalink_url": "https://soundcloud.com/ethmusic", "avatar_url": "https://i1.sndcdn.com/avatars-000031930681-o82t4w-large.jpg"},
            "media": {"transcodings": [
                {"url": "https://api-v2.soundcloud.com/media/soundcloud:tracks:62986583/4b215b29/stream/hls", "preset": "aac_96k", "duration": 143206, "snipped": false, "format": {"protocol": "hls", "mime_type": "audio/mp4; codecs=\"mp4a.40.2\""}, "quality": "lq"},
                {"url": "https://api-v2.soundcloud.com/media/soundcloud:tracks:62986583/db6e08d3/stream/hls", "preset": "abr_sq", "duration": 143206, "snipped": false, "format": {"protocol": "hls", "mime_type": "audio/mpegurl"}, "quality": "sq"},
                {"url": "https://api-v2.soundcloud.com/media/soundcloud:tracks:62986583/a796985b/stream/hls", "preset": "mp3_0_0", "duration": 143216, "snipped": false, "format": {"protocol": "hls", "mime_type": "audio/mpeg"}, "quality": "sq"},
                {"url": "https://api-v2.soundcloud.com/media/soundcloud:tracks:62986583/a796985b/stream/progressive", "preset": "mp3_0_0", "duration": 143216, "snipped": false, "format": {"protocol": "progressive", "mime_type": "audio/mpeg"}, "quality": "sq"},
                {"url": "https://api-v2.soundcloud.com/media/soundcloud:tracks:62986583/c0ffee/stream/hls", "preset": "opus_0_0", "duration": 143206, "snipped": false, "format": {"protocol": "hls", "mime_type": "audio/ogg; codecs=\"opus\""}, "quality": "sq"}
            ]}
        })
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://soundcloud.com/ethmusic/lostin-powers-she-so-heavy?si=abc"),
            Some(Link::Track {
                user: "ethmusic".into(),
                slug: "lostin-powers-she-so-heavy".into(),
                token: None
            })
        );
        assert_eq!(
            link("https://m.soundcloud.com/jaimemf/youtube-dl-test-video-a-y-baw/s-8Pjrp"),
            Some(Link::Track {
                user: "jaimemf".into(),
                slug: "youtube-dl-test-video-a-y-baw".into(),
                token: Some("s-8Pjrp".into())
            })
        );
        assert_eq!(
            link("https://soundcloud.com/the-concept-band/sets/the-royal-concept-ep"),
            Some(Link::Set {
                user: "the-concept-band".into(),
                slug: "the-royal-concept-ep".into(),
                token: None
            })
        );
        assert_eq!(
            link("https://soundcloud.com/the-concept-band/sets/the-royal-concept-ep/s-abc"),
            Some(Link::Set {
                user: "the-concept-band".into(),
                slug: "the-royal-concept-ep".into(),
                token: Some("s-abc".into())
            })
        );
        assert_eq!(
            link("https://soundcloud.com/soft-cell-official"),
            Some(Link::User {
                user: "soft-cell-official".into(),
                page: UserPage::All
            })
        );
        assert_eq!(
            link("https://soundcloud.com/clalberg/likes/"),
            Some(Link::User {
                user: "clalberg".into(),
                page: UserPage::Likes
            })
        );
        assert_eq!(
            link("https://soundcloud.com/soft-cell-official/sets"),
            Some(Link::User {
                user: "soft-cell-official".into(),
                page: UserPage::Sets
            })
        );
        assert_eq!(
            link("https://soundcloud.com/wajang/sexapil-pingers-5/recommended"),
            Some(Link::Related {
                user: "wajang".into(),
                slug: "sexapil-pingers-5".into(),
                relation: Relation::Recommended
            })
        );
        assert_eq!(
            link("https://api.soundcloud.com/tracks/soundcloud%3Atracks%3A1083788353"),
            Some(Link::TrackId {
                id: 1083788353,
                token: None
            })
        );
        assert_eq!(
            link(
                "https://api.soundcloud.com/playlists/soundcloud:playlists:2104769627?secret_token=s-wmpCLuExeYX"
            ),
            Some(Link::PlaylistId {
                id: 2104769627,
                token: Some("s-wmpCLuExeYX".into())
            })
        );
        assert_eq!(
            link("https://api.soundcloud.com/users/30909869"),
            Some(Link::UserId { id: 30909869 })
        );
        assert_eq!(
            link(
                "https://w.soundcloud.com/player/?visual=true&url=https%3A%2F%2Fapi.soundcloud.com%2Fplaylists%2F922213810&show_artwork=true&secret_token=s-ziYey"
            ),
            Some(Link::PlaylistId {
                id: 922213810,
                token: Some("s-ziYey".into())
            })
        );
        assert_eq!(
            link("https://on.soundcloud.com/2iQZMwo9IQ8wLY3vf9"),
            Some(Link::Short(
                Url::parse("https://on.soundcloud.com/2iQZMwo9IQ8wLY3vf9").unwrap()
            ))
        );
        assert_eq!(link("https://soundcloud.com/discover"), None);
        assert_eq!(link("https://soundcloud.com/search?q=x"), None);
        assert_eq!(link("https://soundcloud.com/"), None);
        assert_eq!(link("https://soundcloud.com/user/track/comments"), None);
        assert_eq!(link("https://example.com/ethmusic/track"), None);
    }

    #[tokio::test]
    async fn tracks_resolve_to_their_transcodings_with_the_web_apps_client_id() {
        let mut fixture = Fixture::new(PLATFORM, None);
        web_app(&mut fixture);
        fixture.exchanges.push(get(
            &format!("https://api-v2.soundcloud.com/resolve?url=https%3A%2F%2Fsoundcloud.com%2Fethmusic%2Flostin-powers-she-so-heavy&client_id={CLIENT_ID}"),
            200,
            "application/json",
            track_json().to_string(),
        ));
        fixture.exchanges.push(get(
            &format!("https://api-v2.soundcloud.com/media/soundcloud:tracks:62986583/4b215b29/stream/hls?client_id={CLIENT_ID}"),
            200,
            "application/json",
            json!({"url": "https://playback.media-streaming.soundcloud.cloud/x/aac_96k/4b215b29/playlist.m3u8?expires=1"}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://playback.media-streaming.soundcloud.cloud/x/aac_96k/4b215b29/playlist.m3u8?expires=1",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:10\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:10.0,\ndata000.m4s\n#EXTINF:10.0,\ndata001.m4s\n#EXT-X-ENDLIST\n".into(),
        ));
        fixture.exchanges.push(get(
            &format!("https://api-v2.soundcloud.com/media/soundcloud:tracks:62986583/a796985b/stream/hls?client_id={CLIENT_ID}"),
            200,
            "application/json",
            json!({"url": "https://cf-hls-media.sndcdn.com/playlist/n6FLbx6ZzMiu.128.mp3/playlist.m3u8?Policy=p"}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://cf-hls-media.sndcdn.com/playlist/n6FLbx6ZzMiu.128.mp3/playlist.m3u8?Policy=p",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:16\n#EXTINF:15.0,\nhttps://cf-hls-media.sndcdn.com/media/0/15/n6FLbx6ZzMiu.128.mp3\n#EXT-X-ENDLIST\n".into(),
        ));
        fixture.exchanges.push(get(
            &format!("https://api-v2.soundcloud.com/media/soundcloud:tracks:62986583/a796985b/stream/progressive?client_id={CLIENT_ID}"),
            200,
            "application/json",
            json!({"url": "https://cf-media.sndcdn.com/n6FLbx6ZzMiu.128.mp3?Policy=p&Signature=s"}).to_string(),
        ));
        fixture.exchanges.push(get(
            &format!("https://api-v2.soundcloud.com/media/soundcloud:tracks:62986583/c0ffee/stream/hls?client_id={CLIENT_ID}"),
            404,
            "application/json",
            json!({"error": "not found"}).to_string(),
        ));
        let resolver = SoundcloudResolver::new(Http::replay(fixture));
        let url = Url::parse("https://soundcloud.com/ethmusic/lostin-powers-she-so-heavy").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.media, MediaKind::Audio);
        assert_eq!(resolved.id.as_deref(), Some("62986583"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Lostin Powers - She so Heavy (SneakPreview) Adrian Ackers Blueprint 1")
        );
        assert_eq!(resolved.description.as_deref(), Some("A sneak preview."));
        assert_eq!(
            resolved.uploader.as_deref(),
            Some("E.T. ExTerrestrial Music")
        );
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://soundcloud.com/ethmusic"
        );
        assert_eq!(resolved.duration, Some(Duration::from_millis(143206)));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://i1.sndcdn.com/artworks-000031955188-rwb18x-t500x500.jpg"
        );
        assert_eq!(
            resolved.variants.len(),
            3,
            "aac hls, mp3 hls and mp3 progressive; abr skipped, opus missing"
        );
        assert!(resolved.variants.iter().all(|v| v.audio_only));
        let aac = &resolved.variants[0];
        assert_eq!(aac.kind, VariantKind::Hls);
        assert_eq!(aac.audio, Some(AudioCodec::Aac));
        assert_eq!(aac.bitrate, Some(96_000));
        assert_eq!(aac.format_id.as_deref(), Some("hls_aac_96k"));
        assert_eq!(aac.duration, Some(Duration::from_secs(20)));
        let mp3_hls = &resolved.variants[1];
        assert_eq!(mp3_hls.kind, VariantKind::Hls);
        assert_eq!(mp3_hls.audio, Some(AudioCodec::Mp3));
        assert_eq!(mp3_hls.bitrate, Some(128_000));
        let mp3 = &resolved.variants[2];
        assert_eq!(mp3.kind, VariantKind::File);
        assert_eq!(mp3.container, Some(Container::Mp3));
        assert_eq!(mp3.audio, Some(AudioCodec::Mp3));
        assert_eq!(mp3.bitrate, Some(128_000));
        assert_eq!(mp3.format_id.as_deref(), Some("http_mp3_0_0"));
        assert_eq!(
            mp3.url.as_str(),
            "https://cf-media.sndcdn.com/n6FLbx6ZzMiu.128.mp3?Policy=p&Signature=s"
        );
        assert_eq!(
            resolver.client_id.lock().unwrap().as_deref(),
            Some(CLIENT_ID),
            "the client id is kept for the next link"
        );
    }

    #[tokio::test]
    async fn a_refused_client_id_is_read_again_and_locked_tracks_are_reported() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://api-v2.soundcloud.com/tracks/1?client_id=stale",
            401,
            "application/json",
            "{}".into(),
        ));
        web_app(&mut fixture);
        let mut locked = track_json();
        locked["id"] = json!(1);
        locked["media"] = json!({"transcodings": [
            {"url": "https://api-v2.soundcloud.com/media/soundcloud:tracks:1/x/stream/hls", "preset": "aac_256k", "duration": 1000, "snipped": false, "format": {"protocol": "ctr-encrypted-hls", "mime_type": "audio/mp4; codecs=\"mp4a.40.2\""}, "quality": "hq"}
        ]});
        fixture.exchanges.push(get(
            &format!("https://api-v2.soundcloud.com/tracks/1?client_id={CLIENT_ID}"),
            200,
            "application/json",
            locked.to_string(),
        ));
        fixture.exchanges.push(get(
            &format!("https://api-v2.soundcloud.com/tracks/2?client_id={CLIENT_ID}"),
            404,
            "application/json",
            json!({"errors": [{"error_message": "404 - Not Found"}]}).to_string(),
        ));
        let resolver = SoundcloudResolver::new(Http::replay(fixture));
        *resolver.client_id.lock().unwrap() = Some("stale".into());
        let error = resolver
            .resolve(&Url::parse("https://api.soundcloud.com/tracks/1").unwrap())
            .await
            .unwrap_err();
        assert!(matches!(error, ResolveError::Drm { .. }), "{error}");
        assert_eq!(
            resolver.client_id.lock().unwrap().as_deref(),
            Some(CLIENT_ID)
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://api.soundcloud.com/tracks/2").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn sets_list_their_tracks_and_fill_the_ones_named_by_id() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            &format!("https://api-v2.soundcloud.com/resolve?url=https%3A%2F%2Fsoundcloud.com%2Fthe-concept-band%2Fsets%2Fthe-royal-concept-ep&client_id={CLIENT_ID}"),
            200,
            "application/json",
            json!({
                "id": 2284613, "kind": "playlist", "title": "The Royal Concept EP", "track_count": 3, "secret_token": null,
                "permalink_url": "https://soundcloud.com/the-concept-band/sets/the-royal-concept-ep",
                "user": {"username": "The Royal Concept"},
                "tracks": [
                    {"id": 75206121, "kind": "track", "title": "World On Fire (Re-Mastered)", "permalink_url": "https://soundcloud.com/the-concept-band/world-on-fire-1", "duration": 199889},
                    {"id": 47127631, "kind": "track", "monetization_model": "NOT_APPLICABLE", "policy": "ALLOW"},
                    {"id": 30510138, "kind": "track", "monetization_model": "NOT_APPLICABLE", "policy": "ALLOW"}
                ]
            }).to_string(),
        ));
        fixture.exchanges.push(get(
            &format!("https://api-v2.soundcloud.com/tracks?ids=47127631%2C30510138&playlistId=2284613&client_id={CLIENT_ID}"),
            200,
            "application/json",
            json!([
                {"id": 30510138, "title": "D-D-Dance", "permalink_url": "https://soundcloud.com/the-concept-band/the-concept-d-d-dance", "duration": 219657},
                {"id": 47127631, "title": "Knocked Up", "permalink_url": "https://soundcloud.com/the-concept-band/knocked-up-mastered", "duration": 200000}
            ]).to_string(),
        ));
        let resolver = SoundcloudResolver::new(Http::replay(fixture));
        *resolver.client_id.lock().unwrap() = Some(CLIENT_ID.into());
        let Resolution::Playlist(set) = resolver
            .resolve(
                &Url::parse("https://soundcloud.com/the-concept-band/sets/the-royal-concept-ep")
                    .unwrap(),
            )
            .await
            .unwrap()
        else {
            panic!("a set is a playlist");
        };
        assert_eq!(set.id.as_deref(), Some("2284613"));
        assert_eq!(set.title.as_deref(), Some("The Royal Concept EP"));
        assert_eq!(set.total, Some(3));
        assert_eq!(set.entries.len(), 3);
        assert_eq!(set.entries[1].title.as_deref(), Some("Knocked Up"));
        assert_eq!(
            set.entries[2].url.as_str(),
            "https://soundcloud.com/the-concept-band/the-concept-d-d-dance"
        );
        assert_eq!(set.entries[2].duration, Some(Duration::from_millis(219657)));
    }

    #[tokio::test]
    async fn user_pages_likes_and_short_links_resolve() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            &format!("https://api-v2.soundcloud.com/resolve?url=https%3A%2F%2Fsoundcloud.com%2Fclalberg&client_id={CLIENT_ID}"),
            200,
            "application/json",
            json!({"id": 11817582, "kind": "user", "username": "clalberg", "likes_count": 44, "track_count": 2, "playlist_count": 0}).to_string(),
        ));
        fixture.exchanges.push(get(
            &format!("https://api-v2.soundcloud.com/users/11817582/likes?limit=200&linked_partitioning=1&client_id={CLIENT_ID}"),
            200,
            "application/json",
            json!({"collection": [
                {"created_at": "2013-01-01T00:00:00Z", "kind": "like", "track": {"id": 1, "title": "World Of Kengarden 003", "permalink_url": "https://soundcloud.com/kiengarden/world-of-kengarden-003-the-challenge", "duration": 3600000}},
                {"created_at": "2013-01-01T00:00:00Z", "kind": "like", "playlist": {"id": 2, "title": "A set", "permalink_url": "https://soundcloud.com/someone/sets/a-set", "duration": 100000}},
                {"created_at": "2013-01-01T00:00:00Z", "kind": "like", "track": {"id": 3, "title": "Gone"}}
            ], "next_href": null}).to_string(),
        ));
        let mut short = get(
            "https://on.soundcloud.com/2iQZMwo9IQ8wLY3vf9",
            200,
            "text/html",
            "<html></html>".into(),
        );
        short.response.url =
            "https://soundcloud.com/ilaytsa/crashout?si=34a2&utm_source=clipboard".into();
        fixture.exchanges.push(short);
        fixture.exchanges.push(get(
            &format!("https://api-v2.soundcloud.com/resolve?url=https%3A%2F%2Fsoundcloud.com%2Filaytsa%2Fcrashout&client_id={CLIENT_ID}"),
            200,
            "application/json",
            json!({
                "id": 9, "kind": "track", "title": "crashout", "permalink_url": "https://soundcloud.com/ilaytsa/crashout", "duration": 90000,
                "created_at": "2025-01-01T00:00:00Z", "user": {"username": "ilaytsa", "permalink_url": "https://soundcloud.com/ilaytsa", "avatar_url": "https://i1.sndcdn.com/avatars-x-large.jpg"},
                "media": {"transcodings": [
                    {"url": "https://api-v2.soundcloud.com/media/soundcloud:tracks:9/p/stream/progressive", "preset": "mp3_1_0", "duration": 90000, "snipped": false, "format": {"protocol": "progressive", "mime_type": "audio/mpeg"}, "quality": "sq"}
                ]}
            }).to_string(),
        ));
        fixture.exchanges.push(get(
            &format!("https://api-v2.soundcloud.com/media/soundcloud:tracks:9/p/stream/progressive?client_id={CLIENT_ID}"),
            200,
            "application/json",
            json!({"url": "https://cf-media.sndcdn.com/crashout.128.mp3?Policy=p"}).to_string(),
        ));
        let resolver = SoundcloudResolver::new(Http::replay(fixture));
        *resolver.client_id.lock().unwrap() = Some(CLIENT_ID.into());
        let Resolution::Playlist(likes) = resolver
            .resolve(&Url::parse("https://soundcloud.com/clalberg/likes").unwrap())
            .await
            .unwrap()
        else {
            panic!("likes are a playlist");
        };
        assert_eq!(likes.title.as_deref(), Some("clalberg (likes)"));
        assert_eq!(likes.total, Some(44));
        assert_eq!(
            likes.entries.len(),
            2,
            "the entry without a link is left out"
        );
        assert_eq!(
            likes.entries[0].url.as_str(),
            "https://soundcloud.com/kiengarden/world-of-kengarden-003-the-challenge"
        );
        assert_eq!(likes.entries[0].duration, Some(Duration::from_secs(3600)));
        assert_eq!(likes.entries[1].title.as_deref(), Some("A set"));
        let resolved = resolver
            .resolve(&Url::parse("https://on.soundcloud.com/2iQZMwo9IQ8wLY3vf9").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.title.as_deref(), Some("crashout"));
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://i1.sndcdn.com/avatars-x-t500x500.jpg"
        );
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].bitrate, Some(128_000));
    }

    /// Every example link resolves live: tracks to audio with a playable variant, lists
    /// to entries.
    #[ignore = "reaches the live site: cargo test -- --ignored"]
    #[tokio::test]

    async fn live_examples_resolve() {
        let resolver = SoundcloudResolver::new(Http::new(crate::http::HttpConfig::default()));
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
            let resolution = tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&url))
                .await
                .expect("resolution timed out")
                .unwrap_or_else(|e| panic!("{link}: {e}"));
            match resolution {
                Resolution::Media(resolved) => {
                    assert_eq!(resolved.media, MediaKind::Audio, "{link}");
                    assert!(
                        resolved.variants.iter().any(|v| v.is_playable()),
                        "{link}: no playable variant"
                    );
                    println!(
                        "{link}: audio {:?} with {} variants",
                        resolved.title,
                        resolved.variants.len()
                    );
                }
                Resolution::Playlist(playlist) => {
                    assert!(!playlist.entries.is_empty(), "{link}: no entries");
                    println!(
                        "{link}: playlist {:?} with {} entries (total {:?})",
                        playlist.title,
                        playlist.entries.len(),
                        playlist.total
                    );
                }
            }
        }
    }
}
