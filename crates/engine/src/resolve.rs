//! Turning a link into media: each platform has a resolver that knows its pages and APIs,
//! and every resolver returns the same shape, which the engine picks a variant from.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::http::{Http, HttpError, Response, StatusCode};
use crate::media::{AudioCodec, Container, VideoCodec};

pub mod hls;
pub mod page;
pub mod reddit;
pub mod twitter;
pub mod web;

const MAX_REDIRECT_HOPS: usize = 5;
/// The most of a page or API answer a resolver reads.
pub const MAX_PAGE: usize = 8 * 1024 * 1024;

/// Whether a platform's links need, or benefit from, stored cookies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionSupport {
    /// The platform is read without an account.
    None,
    /// Public links work without one; a session unlocks age-gated, private or higher
    /// quality media.
    Optional,
    /// Nothing resolves without a logged-in session.
    Required,
}

/// What a resolver covers, for the coverage page and the smoke tests.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Platform {
    pub id: &'static str,
    pub name: &'static str,
    /// Hosts the resolver takes links from, as suffixes.
    pub hosts: &'static [&'static str],
    /// The kinds of links it handles: `videos`, `shorts`, `live`, `playlists`, `clips`...
    pub features: &'static [&'static str],
    /// How the media arrives: `mp4`, `webm`, `hls`, `dash`, `ism`, `rtmp`, `whep`...
    pub formats: &'static [&'static str],
    pub session: SessionSupport,
    /// Public links the scheduled smoke tests resolve.
    pub examples: &'static [&'static str],
}

/// What stored cookies are worth at a platform.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum SessionCheck {
    /// The platform has no sessions to check.
    Unsupported,
    /// The cookies form no session, or none is stored.
    LoggedOut,
    LoggedIn {
        account: String,
    },
}

#[async_trait]
pub trait Resolver: Send + Sync {
    fn id(&self) -> &'static str;
    fn platform(&self) -> Platform;
    fn matches(&self, url: &Url) -> bool;
    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError>;
    /// Whether the platform's stored cookies log in, and as whom.
    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        Ok(SessionCheck::Unsupported)
    }
}

pub struct ResolverRegistry {
    resolvers: Vec<Box<dyn Resolver>>,
}

impl ResolverRegistry {
    pub fn new(resolvers: Vec<Box<dyn Resolver>>) -> Self {
        Self { resolvers }
    }

    pub fn ids(&self) -> Vec<&'static str> {
        self.resolvers.iter().map(|r| r.id()).collect()
    }

    pub fn platforms(&self) -> Vec<Platform> {
        self.resolvers.iter().map(|r| r.platform()).collect()
    }

    pub fn get(&self, id: &str) -> Option<&dyn Resolver> {
        self.resolvers
            .iter()
            .find(|r| r.id() == id)
            .map(|r| r.as_ref())
    }

    pub fn supports(&self, url: &Url) -> bool {
        self.resolvers.iter().any(|r| r.matches(url))
    }

    pub fn find(&self, url: &Url) -> Option<&dyn Resolver> {
        self.resolvers
            .iter()
            .find(|r| r.matches(url))
            .map(|r| r.as_ref())
    }

    /// Resolves with the first matching resolver, following resolver level redirects
    /// (a page that only links to media hosted elsewhere) through the registry again.
    pub async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let mut current = url.clone();
        for _ in 0..MAX_REDIRECT_HOPS {
            let resolver = self
                .find(&current)
                .ok_or_else(|| ResolveError::Unsupported(current.clone()))?;
            match resolver.resolve(&current).await {
                Err(ResolveError::Redirect(next)) if next != current => {
                    tracing::debug!(from = %current, to = %next, "resolver redirect");
                    current = next;
                }
                other => return other,
            }
        }
        Err(ResolveError::Unavailable {
            url: current,
            reason: "too many redirects between resolvers".into(),
        })
    }
}

/// A link is one piece of media, or a list of links to resolve one by one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Resolution {
    Media(Box<Resolved>),
    Playlist(Playlist),
}

impl Resolution {
    pub fn media(self) -> Option<Resolved> {
        match self {
            Resolution::Media(resolved) => Some(*resolved),
            Resolution::Playlist(_) => None,
        }
    }
}

impl From<Resolved> for Resolution {
    fn from(resolved: Resolved) -> Self {
        Resolution::Media(Box::new(resolved))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Playlist {
    pub resolver: String,
    pub id: Option<String>,
    pub title: Option<String>,
    pub entries: Vec<PlaylistEntry>,
    /// How many the platform reports, when more than `entries` holds.
    pub total: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlaylistEntry {
    pub url: Url,
    pub title: Option<String>,
    pub duration: Option<Duration>,
}

/// A portion of the media to keep, as YouTube clips and time-stamped links name one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipRange {
    pub start: Duration,
    pub end: Option<Duration>,
}

impl ClipRange {
    pub fn length(&self) -> Option<Duration> {
        self.end.map(|end| end.saturating_sub(self.start))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubtitleFormat {
    Vtt,
    Srt,
    Ttml,
    Ass,
    /// YouTube's `json3` timed text.
    Json3,
    /// A playlist of WebVTT segments.
    HlsVtt,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubtitleTrack {
    pub url: Url,
    pub language: String,
    pub name: Option<String>,
    pub format: SubtitleFormat,
    /// Machine generated.
    #[serde(default)]
    pub auto: bool,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resolved {
    pub resolver: String,
    /// The platform's own id for the media.
    #[serde(default)]
    pub id: Option<String>,
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub uploader: Option<String>,
    #[serde(default)]
    pub uploader_url: Option<Url>,
    #[serde(default)]
    pub uploaded_at: Option<Timestamp>,
    pub duration: Option<Duration>,
    #[serde(default)]
    pub thumbnail: Option<Url>,
    #[serde(default)]
    pub webpage_url: Option<Url>,
    #[serde(default)]
    pub live: bool,
    #[serde(default)]
    pub age_limit: Option<u8>,
    #[serde(default)]
    pub clip: Option<ClipRange>,
    #[serde(default)]
    pub subtitles: Vec<SubtitleTrack>,
    pub variants: Vec<Variant>,
}

impl Resolved {
    pub fn new(resolver: &str) -> Self {
        Self {
            resolver: resolver.to_string(),
            id: None,
            title: None,
            description: None,
            uploader: None,
            uploader_url: None,
            uploaded_at: None,
            duration: None,
            thumbnail: None,
            webpage_url: None,
            live: false,
            age_limit: None,
            clip: None,
            subtitles: Vec::new(),
            variants: Vec::new(),
        }
    }

    /// The DRM system every playable variant is locked with, if all are.
    pub fn drm(&self) -> Option<&str> {
        if self.variants.is_empty() || self.variants.iter().any(|v| v.drm.is_none()) {
            return None;
        }
        self.variants.iter().find_map(|v| v.drm.as_deref())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VariantKind {
    /// One file over HTTP.
    File,
    Hls,
    Dash,
    /// Microsoft Smooth Streaming.
    Ism,
    Rtmp,
    Rtsp,
    /// WebRTC through a WHEP endpoint.
    Whep,
    /// A page whose player feeds a media source extension; captured in a headless browser.
    Browser,
}

impl VariantKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            VariantKind::File => "file",
            VariantKind::Hls => "hls",
            VariantKind::Dash => "dash",
            VariantKind::Ism => "ism",
            VariantKind::Rtmp => "rtmp",
            VariantKind::Rtsp => "rtsp",
            VariantKind::Whep => "whep",
            VariantKind::Browser => "browser",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Variant {
    pub url: Url,
    pub kind: VariantKind,
    /// A separate audio stream to mux with a video-only `url`.
    pub audio_url: Option<Url>,
    pub container: Option<Container>,
    pub video: Option<VideoCodec>,
    pub audio: Option<AudioCodec>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    #[serde(default)]
    pub fps: Option<f64>,
    /// Bits per second.
    pub bitrate: Option<u64>,
    pub size: Option<u64>,
    pub duration: Option<Duration>,
    pub headers: Vec<(String, String)>,
    /// The platform's name for the format, such as a YouTube itag.
    #[serde(default)]
    pub format_id: Option<String>,
    /// A label such as `1080p60`.
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub language: Option<String>,
    /// The RFC 6381 codec string, when known.
    #[serde(default)]
    pub codecs: Option<String>,
    #[serde(default)]
    pub video_only: bool,
    #[serde(default)]
    pub audio_only: bool,
    #[serde(default)]
    pub live: bool,
    /// The DRM system the stream is locked with; such variants cannot be downloaded.
    #[serde(default)]
    pub drm: Option<String>,
}

impl Variant {
    pub fn new(url: Url, kind: VariantKind) -> Self {
        Self {
            url,
            kind,
            audio_url: None,
            container: None,
            video: None,
            audio: None,
            width: None,
            height: None,
            fps: None,
            bitrate: None,
            size: None,
            duration: None,
            headers: Vec::new(),
            format_id: None,
            label: None,
            language: None,
            codecs: None,
            video_only: false,
            audio_only: false,
            live: false,
            drm: None,
        }
    }

    pub fn file(url: Url) -> Self {
        Self::new(url, VariantKind::File)
    }

    pub fn hls(url: Url) -> Self {
        Self::new(url, VariantKind::Hls)
    }

    pub fn dash(url: Url) -> Self {
        Self::new(url, VariantKind::Dash)
    }

    pub fn with_size(mut self, width: u32, height: u32) -> Self {
        self.width = Some(width);
        self.height = Some(height);
        self
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    /// Fills codec identities from an RFC 6381 codec string.
    pub fn with_codecs(mut self, codecs: &str) -> Self {
        let (video, audio) = parse_codecs(Some(codecs));
        if self.video.is_none() {
            self.video = video;
        }
        if self.audio.is_none() {
            self.audio = audio;
        }
        self.codecs = Some(codecs.to_string());
        self
    }

    pub fn is_playable(&self) -> bool {
        self.drm.is_none()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("no resolver handles {0}")]
    Unsupported(Url),
    #[error("no video found at {0}")]
    NotFound(Url),
    #[error("{url} is unavailable: {reason}")]
    Unavailable { url: Url, reason: String },
    #[error("unexpected response from {url}: {detail}")]
    Malformed { url: Url, detail: String },
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error("media lives at {0}")]
    Redirect(Url),
    #[error("{url} is protected by {system} DRM")]
    Drm { url: Url, system: String },
    #[error("{url} needs a logged-in {platform} session: {reason}")]
    LoginRequired {
        url: Url,
        platform: &'static str,
        reason: Box<str>,
    },
    #[error("{0} is rate limiting requests; try again later")]
    RateLimited(Url),
    #[error("{url} needs a headless browser, which is not available: {reason}")]
    BrowserUnavailable { url: Url, reason: String },
}

impl ResolveError {
    pub fn malformed(url: &Url, detail: impl Into<String>) -> Self {
        Self::Malformed {
            url: url.clone(),
            detail: detail.into(),
        }
    }

    pub fn unavailable(url: &Url, reason: impl Into<String>) -> Self {
        Self::Unavailable {
            url: url.clone(),
            reason: reason.into(),
        }
    }

    pub fn login_required(url: &Url, platform: &'static str, reason: impl Into<String>) -> Self {
        Self::LoginRequired {
            url: url.clone(),
            platform,
            reason: reason.into().into_boxed_str(),
        }
    }

    pub fn drm(url: &Url, system: impl Into<String>) -> Self {
        Self::Drm {
            url: url.clone(),
            system: system.into(),
        }
    }

    /// Whether the failure is the platform's doing rather than a bug: nothing there, taken
    /// down, locked, or busy.
    pub fn is_expected(&self) -> bool {
        matches!(
            self,
            ResolveError::NotFound(_)
                | ResolveError::Unavailable { .. }
                | ResolveError::Drm { .. }
                | ResolveError::LoginRequired { .. }
                | ResolveError::RateLimited(_)
        )
    }
}

/// Turns an HTTP status into the resolver error it means for `requested`: nothing there,
/// locked, rate limited, or unavailable.
pub fn check_status(response: &Response, requested: &Url) -> Result<(), ResolveError> {
    status_error(response.status, requested).map_or(Ok(()), Err)
}

pub fn status_error(status: StatusCode, requested: &Url) -> Option<ResolveError> {
    if status.is_success() {
        return None;
    }
    Some(match status.as_u16() {
        404 | 410 => ResolveError::NotFound(requested.clone()),
        429 => ResolveError::RateLimited(requested.clone()),
        401 | 403 => ResolveError::unavailable(requested, format!("HTTP {status}")),
        _ => ResolveError::unavailable(requested, format!("HTTP {status}")),
    })
}

/// A page or API answer read whole.
pub struct Fetched {
    pub url: Url,
    pub status: StatusCode,
    pub content_type: Option<String>,
    pub body: bytes::Bytes,
}

impl Fetched {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    pub fn json(&self, requested: &Url) -> Result<serde_json::Value, ResolveError> {
        serde_json::from_slice(&self.body)
            .map_err(|e| ResolveError::malformed(requested, format!("JSON: {e}")))
    }
}

/// GETs `url` as `platform` with `user_agent`, reading at most `limit` bytes.
pub async fn fetch(
    http: &Http,
    url: &Url,
    platform: &str,
    user_agent: &str,
    headers: &[(String, String)],
    limit: usize,
) -> Result<Fetched, ResolveError> {
    let response = http
        .get(url.clone())
        .platform(platform)
        .user_agent(user_agent)
        .headers(headers)
        .send()
        .await?;
    let status = response.status;
    let final_url = response.url.clone();
    let content_type = response.content_type().map(str::to_owned);
    let (body, _) = response.bytes_up_to(limit).await?;
    Ok(Fetched {
        url: final_url,
        status,
        content_type,
        body,
    })
}

/// [`fetch`], failing unless the answer is 2xx.
pub async fn fetch_ok(
    http: &Http,
    url: &Url,
    platform: &str,
    user_agent: &str,
    headers: &[(String, String)],
    limit: usize,
) -> Result<Fetched, ResolveError> {
    let fetched = fetch(http, url, platform, user_agent, headers, limit).await?;
    if let Some(error) = status_error(fetched.status, url) {
        return Err(error);
    }
    Ok(fetched)
}

/// The `type/subtype` of a `Content-Type` value, lower-cased, without parameters.
pub fn essence(content_type: Option<&str>) -> String {
    content_type
        .and_then(|value| value.parse::<mime::Mime>().ok())
        .map(|mime| mime.essence_str().to_ascii_lowercase())
        .unwrap_or_default()
}

pub fn is_hls_type(essence: &str) -> bool {
    matches!(
        essence,
        "application/vnd.apple.mpegurl"
            | "application/x-mpegurl"
            | "audio/mpegurl"
            | "audio/x-mpegurl"
    )
}

pub fn is_dash_type(essence: &str) -> bool {
    essence == "application/dash+xml"
}

pub fn is_ism_type(essence: &str) -> bool {
    matches!(
        essence,
        "application/vnd.ms-sstr+xml" | "application/vnd.ms-sstr" | "text/xml" | "application/xml"
    )
}

/// The lower-cased extension of the URL path, when it looks like a file extension.
pub fn path_extension(url: &Url) -> Option<String> {
    Path::new(url.path())
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| !e.is_empty() && e.len() <= 5)
        .map(str::to_ascii_lowercase)
}

/// Maps an RFC 6381 codec list such as `avc1.64001f,mp4a.40.2` to codec identities.
pub fn parse_codecs(codecs: Option<&str>) -> (Option<VideoCodec>, Option<AudioCodec>) {
    let mut video: Option<VideoCodec> = None;
    let mut audio: Option<AudioCodec> = None;
    for item in codecs.unwrap_or("").split(',') {
        let item = item.trim().to_ascii_lowercase();
        let tag = item.split('.').next().unwrap_or("");
        let found_video = match tag {
            "avc1" | "avc3" | "h264" => Some(VideoCodec::H264),
            "hvc1" | "hev1" | "hevc" | "h265" | "dvh1" | "dvhe" => Some(VideoCodec::H265),
            "vp09" | "vp9" => Some(VideoCodec::Vp9),
            "vp08" | "vp8" => Some(VideoCodec::Vp8),
            "av01" | "av1" => Some(VideoCodec::Av1),
            _ => None,
        };
        let found_audio = match tag {
            "mp4a" if item.starts_with("mp4a.40.34") || item.starts_with("mp4a.6b") => {
                Some(AudioCodec::Mp3)
            }
            "mp4a" | "aac" => Some(AudioCodec::Aac),
            "opus" => Some(AudioCodec::Opus),
            "vorbis" => Some(AudioCodec::Vorbis),
            "mp3" => Some(AudioCodec::Mp3),
            "ec-3" | "ac-3" | "flac" | "alac" => Some(AudioCodec::Other(item.clone())),
            _ => None,
        };
        if video.is_none() && found_video.is_some() {
            video = found_video;
        }
        if audio.is_none() && found_audio.is_some() {
            audio = found_audio;
        }
    }
    (video, audio)
}

pub fn clean_title(raw: &str) -> Option<String> {
    let collapsed: String = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = collapsed.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(200).collect())
}

/// A `?t=1h2m3s`, `?t=95`, `#t=95` or `?start=95` time-stamp on a link.
pub fn timestamp_hint(url: &Url) -> Option<Duration> {
    let value = url
        .query_pairs()
        .find(|(k, _)| k == "t" || k == "start" || k == "time_continue")
        .map(|(_, v)| v.into_owned())
        .or_else(|| {
            url.fragment()
                .and_then(|f| f.strip_prefix("t="))
                .map(str::to_string)
        })?;
    parse_time_stamp(&value)
}

/// `95`, `95s`, `1m35s`, `1h2m3s`, `01:35`, `1:02:03`.
pub fn parse_time_stamp(text: &str) -> Option<Duration> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.contains(':') {
        let mut total = 0f64;
        for part in text.split(':') {
            total = total * 60.0 + part.parse::<f64>().ok()?;
        }
        return (total >= 0.0).then(|| Duration::from_secs_f64(total));
    }
    if let Ok(secs) = text.parse::<f64>() {
        return (secs >= 0.0).then(|| Duration::from_secs_f64(secs));
    }
    let mut total = 0f64;
    let mut number = String::new();
    for ch in text.chars() {
        if ch.is_ascii_digit() || ch == '.' {
            number.push(ch);
            continue;
        }
        let value: f64 = number.parse().ok()?;
        number.clear();
        total += match ch {
            'h' => value * 3600.0,
            'm' => value * 60.0,
            's' => value,
            _ => return None,
        };
    }
    if !number.is_empty() {
        total += number.parse::<f64>().ok()?;
    }
    Some(Duration::from_secs_f64(total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn essence_drops_parameters_and_case() {
        assert_eq!(essence(Some("Video/MP4; codecs=\"avc1\"")), "video/mp4");
        assert_eq!(essence(Some("text/html;charset=utf-8")), "text/html");
        assert_eq!(essence(None), "");
        assert_eq!(essence(Some("not a type")), "");
    }

    #[test]
    fn path_extension_reads_file_like_paths_only() {
        let url = |s: &str| Url::parse(s).unwrap();
        assert_eq!(
            path_extension(&url("https://h/dir/Clip.MP4?x=1")),
            Some("mp4".into())
        );
        assert_eq!(path_extension(&url("https://h/dir/")), None);
        assert_eq!(
            path_extension(&url("https://h/archive.tar.gz")),
            Some("gz".into())
        );
        assert_eq!(path_extension(&url("https://h/a.reallylong")), None);
    }

    #[test]
    fn codec_strings_map_to_identities() {
        assert_eq!(
            parse_codecs(Some("avc1.64001f,mp4a.40.2")),
            (Some(VideoCodec::H264), Some(AudioCodec::Aac))
        );
        assert_eq!(
            parse_codecs(Some("vp09.00.10.08, opus")),
            (Some(VideoCodec::Vp9), Some(AudioCodec::Opus))
        );
        assert_eq!(parse_codecs(Some("mp4a.40.34")).1, Some(AudioCodec::Mp3));
        assert_eq!(parse_codecs(None), (None, None));
        let variant = Variant::file(Url::parse("https://h/v.mp4").unwrap())
            .with_codecs("hvc1.1.6.L93.B0,ec-3");
        assert_eq!(variant.video, Some(VideoCodec::H265));
        assert_eq!(variant.audio, Some(AudioCodec::Other("ec-3".into())));
    }

    #[test]
    fn time_stamps_in_every_common_shape() {
        assert_eq!(parse_time_stamp("95"), Some(Duration::from_secs(95)));
        assert_eq!(parse_time_stamp("95s"), Some(Duration::from_secs(95)));
        assert_eq!(parse_time_stamp("1m35s"), Some(Duration::from_secs(95)));
        assert_eq!(parse_time_stamp("1h2m3s"), Some(Duration::from_secs(3723)));
        assert_eq!(parse_time_stamp("1:02:03"), Some(Duration::from_secs(3723)));
        assert_eq!(parse_time_stamp("01:35"), Some(Duration::from_secs(95)));
        assert_eq!(parse_time_stamp("x"), None);
        let url = |s: &str| Url::parse(s).unwrap();
        assert_eq!(
            timestamp_hint(&url("https://youtu.be/x?t=1m35s")),
            Some(Duration::from_secs(95))
        );
        assert_eq!(
            timestamp_hint(&url("https://v.test/clip#t=10")),
            Some(Duration::from_secs(10))
        );
        assert_eq!(timestamp_hint(&url("https://v.test/clip")), None);
    }

    #[test]
    fn drm_is_reported_only_when_nothing_plays() {
        let mut resolved = Resolved::new("x");
        assert!(resolved.drm().is_none());
        let mut locked = Variant::dash(Url::parse("https://h/a.mpd").unwrap());
        locked.drm = Some("widevine".into());
        resolved.variants.push(locked.clone());
        assert_eq!(resolved.drm(), Some("widevine"));
        resolved
            .variants
            .push(Variant::file(Url::parse("https://h/a.mp4").unwrap()));
        assert!(resolved.drm().is_none());
        assert!(!locked.is_playable());
    }

    #[test]
    fn statuses_become_resolver_errors() {
        let url = Url::parse("https://h/v").unwrap();
        assert!(status_error(StatusCode::OK, &url).is_none());
        assert!(matches!(
            status_error(StatusCode::NOT_FOUND, &url),
            Some(ResolveError::NotFound(_))
        ));
        assert!(matches!(
            status_error(StatusCode::GONE, &url),
            Some(ResolveError::NotFound(_))
        ));
        assert!(matches!(
            status_error(StatusCode::TOO_MANY_REQUESTS, &url),
            Some(ResolveError::RateLimited(_))
        ));
        assert!(matches!(
            status_error(StatusCode::FORBIDDEN, &url),
            Some(ResolveError::Unavailable { .. })
        ));
        assert!(ResolveError::NotFound(url.clone()).is_expected());
        assert!(!ResolveError::Unsupported(url).is_expected());
    }
}
