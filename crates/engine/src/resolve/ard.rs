//! ARD: Mediathek videos and live streams, its shows, series seasons and collections,
//! through the page-gateway API the web player reads; Audiothek episodes and shows
//! through the Audiothek GraphQL API; and the media-collection player JSON regional
//! broadcasters still serve, which SR Mediathek builds on.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionCheck, SessionSupport, SubtitleFormat, SubtitleTrack, Tag, Variant, VariantKind,
    clean_title, fetch, fetch_ok, geo, hls, path_extension, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "ard";
/// The SSO endpoint that turns the `ams` cookie into the id token the API wants for
/// age-rated videos.
pub const TOKEN_URL: &str = "https://sso.ardmediathek.de/sso/token";
/// The countries the Mediathek plays in; a refused video is retried once with an
/// address from the first.
pub const GEO_COUNTRIES: &[&str] = &["DE"];
const PAGE_GATEWAY: &str = "https://api.ardmediathek.de/page-gateway";
const GRAPHQL: &str = "https://api.ardaudiothek.de/graphql";
const PAGE_SIZE: usize = 100;
const MAX_ENTRIES: usize = 500;
const GEO_REASON: &str = "available only in DE";
const FSK_REASON: &str = "This video is only available for age verified users or after 22:00";
const FSK_EVENING_REASON: &str = "This video is only available after 20:00";

/// The three kinds of Mediathek listing: a show (`sendung`), a series (`serie`) and a
/// curated collection (`sammlung`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectionKind {
    Sendung,
    Serie,
    Sammlung,
}

/// The alternative version of a season a series link may ask for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Version {
    /// `OV`: the original language.
    Original,
    /// `AD`: with audio description.
    AudioDescription,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A Mediathek `video`, `player` or `live` page, by the id at the end of its path.
    Video(String),
    /// A Mediathek show, series (optionally one season in one version) or collection.
    Collection {
        kind: CollectionKind,
        id: String,
        season: Option<String>,
        version: Option<Version>,
    },
    /// An Audiothek episode, section or extra, by its URN.
    AudioEpisode(String),
    /// An Audiothek show, by its URN.
    AudioShow(String),
}

static RE_VIDEO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^/(?:[^/]+/)?(?:player|live|video)/(?:.+/)?([a-zA-Z0-9]+)/?$").unwrap()
});
static RE_COLLECTION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^/(?:[^/]+/)?(sendung|serie|sammlung)/(?:(.+?)/)?([a-zA-Z0-9]+)(?:/(\d+)(?:/(OV|AD))?)?/?$",
    )
    .unwrap()
});
static RE_AUDIO_EPISODE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^/episode/(urn:ard:(?:episode|section|extra):[a-f0-9]{16})").unwrap()
});
static RE_AUDIO_SHOW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/sendung/[\w-]+/(urn:ard:show:[a-f0-9]{16})").unwrap());
/// `_1280x720.mp4` at the end of a media-collection stream link.
static RE_STREAM_SIZE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"_(\d+)x(\d+)\.mp4$").unwrap());

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let path = url.path();
    match host {
        "ardmediathek.de" | "beta.ardmediathek.de" => {
            if let Some(caps) = RE_VIDEO.captures(path) {
                return Some(Link::Video(caps[1].to_string()));
            }
            let caps = RE_COLLECTION.captures(path)?;
            let kind = match &caps[1] {
                "sendung" => CollectionKind::Sendung,
                "serie" => CollectionKind::Serie,
                _ => CollectionKind::Sammlung,
            };
            let version = caps.get(5).map(|m| match m.as_str() {
                "OV" => Version::Original,
                _ => Version::AudioDescription,
            });
            Some(Link::Collection {
                kind,
                id: caps[3].to_string(),
                season: caps.get(4).map(|m| m.as_str().to_string()),
                version,
            })
        }
        "ardaudiothek.de" | "ardsounds.de" => {
            if let Some(caps) = RE_AUDIO_EPISODE.captures(path) {
                return Some(Link::AudioEpisode(caps[1].to_string()));
            }
            RE_AUDIO_SHOW
                .captures(path)
                .map(|caps| Link::AudioShow(caps[1].to_string()))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------
// The media-collection player JSON (`_mediaArray`), shared with SR Mediathek
// ---------------------------------------------------------------------------------------

/// What a media-collection player JSON describes: the streams, the subtitles ARD
/// publishes as EBU-TT and WebVTT, and the clip's length, poster and liveness.
#[derive(Debug, Clone)]
pub struct MediaInfo {
    pub duration: Option<Duration>,
    pub thumbnail: Option<Url>,
    pub live: bool,
    pub variants: Vec<Variant>,
    pub subtitles: Vec<SubtitleTrack>,
}

/// Reads the player JSON at `media_info_url` as `platform` and turns it into streams,
/// the way `ARDMediathekBaseIE._extract_media_info` does; `webpage` is the page that
/// named the JSON, whose `"fsk"` marker means the clip is held back until the evening.
/// A clip the broadcaster refuses by region is retried once from a German address.
pub async fn extract_media_info(
    http: &Http,
    platform: &str,
    media_info_url: &Url,
    webpage: &str,
    origin: &Url,
) -> Result<MediaInfo, ResolveError> {
    let fsk = webpage.contains("\"fsk\"");
    let media_info = fetch_ok(http, media_info_url, platform, BROWSER_UA, &[], MAX_PAGE)
        .await?
        .json(origin)?;
    let first =
        media_info_with_headers(http, platform, &media_info, fsk, media_info_url, &[]).await;
    let refused_by_region =
        matches!(&first, Err(ResolveError::Unavailable { reason, .. }) if reason == GEO_REASON);
    let Some(address) = refused_by_region
        .then(|| geo::random_ipv4(GEO_COUNTRIES[0]))
        .flatten()
    else {
        return first;
    };
    let headers = vec![("x-forwarded-for".to_string(), address)];
    let media_info = fetch_ok(
        http,
        media_info_url,
        platform,
        BROWSER_UA,
        &headers,
        MAX_PAGE,
    )
    .await?
    .json(origin)?;
    media_info_with_headers(http, platform, &media_info, fsk, media_info_url, &headers).await
}

/// Turns a player JSON already read into streams, the way
/// `ARDMediathekBaseIE._parse_media_info` does; `fsk` says the page carried the
/// evening-only marker, `base` resolves relative links in it.
pub async fn parse_media_info(
    http: &Http,
    platform: &str,
    media_info: &Value,
    fsk: bool,
    base: &Url,
) -> Result<MediaInfo, ResolveError> {
    media_info_with_headers(http, platform, media_info, fsk, base, &[]).await
}

async fn media_info_with_headers(
    http: &Http,
    platform: &str,
    media_info: &Value,
    fsk: bool,
    base: &Url,
    headers: &[(String, String)],
) -> Result<MediaInfo, ResolveError> {
    let formats = media_info_variants(http, platform, media_info, headers).await;
    if formats.variants.is_empty() {
        if fsk {
            return Err(ResolveError::unavailable(base, FSK_EVENING_REASON));
        }
        if media_info["_geoblocked"].as_bool() == Some(true) {
            return Err(ResolveError::unavailable(base, GEO_REASON));
        }
        if let Some(failure) = formats.failure {
            return Err(failure);
        }
        if formats.only_hds {
            return Err(ResolveError::unavailable(
                base,
                "only an HDS (f4m) stream is offered",
            ));
        }
        return Err(ResolveError::unavailable(
            base,
            "the player offers no streams",
        ));
    }
    let mut subtitles = Vec::new();
    if let Some(subtitle_url) = media_info["_subtitleUrl"].as_str() {
        if let Some(ttml) = util::join_url(Some(base), subtitle_url) {
            subtitles.push(SubtitleTrack {
                url: ttml,
                language: "de".into(),
                name: None,
                format: SubtitleFormat::Ttml,
                auto: false,
                headers: headers.to_vec(),
            });
        }
        let vtt = format!("{}.vtt", subtitle_url.replace("/ebutt/", "/webvtt/"));
        if let Some(vtt) = util::join_url(Some(base), &vtt) {
            subtitles.push(SubtitleTrack {
                url: vtt,
                language: "de".into(),
                name: None,
                format: SubtitleFormat::Vtt,
                auto: false,
                headers: headers.to_vec(),
            });
        }
    }
    let live = media_info["_isLive"].as_bool() == Some(true);
    let mut variants = formats.variants;
    for variant in &mut variants {
        variant.live |= live;
    }
    Ok(MediaInfo {
        duration: media_info["_duration"]
            .as_i64()
            .filter(|d| *d > 0)
            .map(|d| Duration::from_secs(d as u64))
            .or_else(|| variants.iter().find_map(|v| v.duration)),
        thumbnail: util::url_of(&media_info["_previewImage"], Some(base)),
        live,
        variants,
        subtitles,
    })
}

/// The streams a media-collection player JSON lists, with the last manifest that could
/// not be read, and whether the only streams offered were HDS.
#[derive(Debug)]
pub struct MediaFormats {
    pub variants: Vec<Variant>,
    pub failure: Option<ResolveError>,
    pub only_hds: bool,
}

/// Every stream of the player JSON's `_mediaArray`, the way
/// `ARDMediathekBaseIE._extract_formats` lists them: HLS playlists expanded, RTMP
/// servers joined with their play path, progressive files with the size in their name.
/// `headers` go with every media request.
pub async fn media_info_variants(
    http: &Http,
    platform: &str,
    media_info: &Value,
    headers: &[(String, String)],
) -> MediaFormats {
    let audio = media_info["_type"].as_str() == Some("audio");
    let mut variants = Vec::new();
    let mut failure = None;
    let mut saw_hds = false;
    for (index, media) in media_info["_mediaArray"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        for stream in media["_mediaStreamArray"].as_array().into_iter().flatten() {
            let stream_urls: Vec<&str> = match &stream["_stream"] {
                Value::String(one) => vec![one.as_str()],
                Value::Array(many) => many.iter().filter_map(Value::as_str).collect(),
                _ => continue,
            };
            let quality = match &stream["_quality"] {
                Value::Null => None,
                other => util::text(other),
            };
            let rtmp = stream["_server"]
                .as_str()
                .filter(|server| server.starts_with("rtmp"));
            for raw in stream_urls {
                // Streams are links as they stand, never joined with the player page; an
                // RTMP server's play path is joined with the server.
                let stream_url = match rtmp {
                    Some(server) => {
                        Url::parse(&format!("{}/{}", server.trim_end_matches('/'), raw)).ok()
                    }
                    None => util::join_url(None, raw),
                };
                let Some(stream_url) = stream_url else {
                    continue;
                };
                let ext = path_extension(&stream_url).unwrap_or_default();
                let manifest = ext == "f4m" || ext == "m3u8";
                if manifest && quality.as_deref() != Some("auto") {
                    continue;
                }
                if ext == "f4m" {
                    saw_hds = true;
                    continue;
                }
                if ext == "m3u8" {
                    match hls::expand(http, &stream_url, platform, BROWSER_UA, headers).await {
                        Ok(expanded) => variants.extend(expanded.variants),
                        Err(error) => failure = Some(error),
                    }
                    continue;
                }
                let quality_label = quality.clone().unwrap_or_else(|| "None".into());
                let mut variant = if rtmp.is_some() {
                    let mut v = Variant::new(stream_url.clone(), VariantKind::Rtmp);
                    v.format_id = Some(format!("a{index}-rtmp-{quality_label}"));
                    v
                } else {
                    let mut v = Variant::file(stream_url.clone());
                    v.format_id = Some(format!("a{index}-{ext}-{quality_label}"));
                    v.container = Container::from_extension(&ext)
                        .or_else(|| (!ext.is_empty()).then(|| Container::Other(ext.clone())));
                    v
                };
                if let Some(caps) = RE_STREAM_SIZE.captures(stream_url.path()) {
                    variant.width = caps[1].parse().ok();
                    variant.height = caps[2].parse().ok();
                }
                if audio {
                    variant.audio_only = true;
                    variant.audio = audio_codec_of_extension(&ext);
                } else if variant.container == Some(Container::Mp4) {
                    variant.video = variant.video.take().or(Some(VideoCodec::H264));
                    variant.audio = variant.audio.take().or(Some(AudioCodec::Aac));
                }
                variant.label = variant.height.map(|h| format!("{h}p"));
                variant.headers = headers.to_vec();
                variants.push(variant);
            }
        }
    }
    let only_hds = variants.is_empty() && failure.is_none() && saw_hds;
    MediaFormats {
        variants,
        failure,
        only_hds,
    }
}

fn audio_codec_of_extension(ext: &str) -> Option<AudioCodec> {
    match ext {
        "mp3" => Some(AudioCodec::Mp3),
        "m4a" | "mp4" | "aac" => Some(AudioCodec::Aac),
        "ogg" | "oga" => Some(AudioCodec::Vorbis),
        "opus" => Some(AudioCodec::Opus),
        "" => None,
        other => Some(AudioCodec::Other(other.to_string())),
    }
}

/// The codec an ARD API names, such as `H.264`, `mp3` or `aac`.
fn video_codec(name: &str) -> Option<VideoCodec> {
    let lower = name.to_ascii_lowercase();
    match lower.replace('.', "").as_str() {
        "" => None,
        "h264" | "avc" | "avc1" => Some(VideoCodec::H264),
        "h265" | "hevc" => Some(VideoCodec::H265),
        "vp8" => Some(VideoCodec::Vp8),
        "vp9" => Some(VideoCodec::Vp9),
        "av1" => Some(VideoCodec::Av1),
        _ => Some(VideoCodec::Other(lower)),
    }
}

fn audio_codec(name: &str) -> Option<AudioCodec> {
    let lower = name.to_ascii_lowercase();
    match lower.replace('.', "").as_str() {
        "" => None,
        "mp3" | "mpeg" => Some(AudioCodec::Mp3),
        "aac" | "mp4a" | "heaac" => Some(AudioCodec::Aac),
        "opus" => Some(AudioCodec::Opus),
        "vorbis" => Some(AudioCodec::Vorbis),
        _ => Some(AudioCodec::Other(lower)),
    }
}

// ---------------------------------------------------------------------------------------
// Age verification through the SSO cookie
// ---------------------------------------------------------------------------------------

/// The signed-in viewer the `ams` SSO cookie stands for: the id token the page gateway
/// wants and who it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgeToken {
    pub id_token: String,
    pub user_id: String,
    /// `18` once the account has verified its age.
    pub age_rating: Option<i64>,
}

/// The claims of a JWT, read without checking its signature.
pub fn jwt_payload(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = util::b64_decode(payload)?;
    serde_json::from_slice(&bytes).ok()
}

/// Asks the SSO for the id token of the `ams` cookie in `platform`'s jar; `None` when
/// there is no cookie, or the SSO does not name a user for it.
pub async fn age_token(http: &Http, platform: &str) -> Result<Option<AgeToken>, ResolveError> {
    if http.jar(platform).get("ams").is_none() {
        return Ok(None);
    }
    let token_url = Url::parse(TOKEN_URL).expect("valid");
    let fetched = fetch(http, &token_url, platform, BROWSER_UA, &[], MAX_PAGE).await?;
    if !fetched.status.is_success() {
        return Ok(None);
    }
    let token = fetched.json(&token_url)?;
    let Some(id_token) = token["idToken"].as_str() else {
        return Ok(None);
    };
    let Some(claims) = jwt_payload(id_token) else {
        return Ok(None);
    };
    let Some(user_id) = claims["user_id"]
        .as_str()
        .or_else(|| claims["sub"].as_str())
    else {
        return Ok(None);
    };
    Ok(Some(AgeToken {
        id_token: id_token.to_string(),
        user_id: user_id.to_string(),
        age_rating: claims["age_rating"].as_i64(),
    }))
}

// ---------------------------------------------------------------------------------------
// Episode numbering in Mediathek titles
// ---------------------------------------------------------------------------------------

/// What a Mediathek title says about the episode, such as `(S06/E07)` or `Folge 25/42:`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct EpisodeInfo {
    pub season_number: Option<u32>,
    pub episode_number: Option<u32>,
    /// The title without its numbering, or the quoted episode name after `Folge N`.
    pub episode: Option<String>,
}

static RE_EPISODE_SEASON: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^.*( \(S(\d+)/E(\d+)\)).*$").unwrap());
static RE_EPISODE_BRACKETS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^.*( \((?:Folge |Teil )?(\d+)(?:/\d+)?\)).*$").unwrap());
static RE_EPISODE_QUOTED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"^.*(Folge (\d+)(?::| -|) )"(.+)".*$"#).unwrap());
static RE_EPISODE_FOLGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^.*(Folge (\d+)(?:/\d+)?(?::| -|) ).*$").unwrap());

pub fn episode_info(title: &str) -> EpisodeInfo {
    let number = |m: Option<regex::Match>| m.and_then(|m| m.as_str().parse().ok());
    let without = |marker: &str| {
        let rest = title.replace(marker, "");
        let rest = rest.trim();
        (!rest.is_empty()).then(|| rest.to_string())
    };
    if let Some(caps) = RE_EPISODE_SEASON.captures(title) {
        return EpisodeInfo {
            season_number: number(caps.get(2)),
            episode_number: number(caps.get(3)),
            episode: without(&caps[1]),
        };
    }
    if let Some(caps) = RE_EPISODE_BRACKETS.captures(title) {
        return EpisodeInfo {
            season_number: None,
            episode_number: number(caps.get(2)),
            episode: without(&caps[1]),
        };
    }
    if let Some(caps) = RE_EPISODE_QUOTED.captures(title) {
        let quoted = caps[3].trim();
        return EpisodeInfo {
            season_number: None,
            episode_number: number(caps.get(2)),
            episode: (!quoted.is_empty()).then(|| quoted.to_string()),
        };
    }
    if let Some(caps) = RE_EPISODE_FOLGE.captures(title) {
        return EpisodeInfo {
            season_number: None,
            episode_number: number(caps.get(2)),
            episode: without(&caps[1]),
        };
    }
    let whole = title.trim();
    EpisodeInfo {
        season_number: None,
        episode_number: None,
        episode: (!whole.is_empty()).then(|| whole.to_string()),
    }
}

// ---------------------------------------------------------------------------------------
// The resolver
// ---------------------------------------------------------------------------------------

pub struct ArdResolver {
    http: Http,
}

impl ArdResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

/// A Mediathek item page built from the page gateway, with what the retry needs to know.
struct BuiltVideo {
    resolved: Resolved,
    geoblocked: bool,
    stream_failure: Option<ResolveError>,
}

const QUERY_ITEM: &str = r#"
    query($id: ID!) {
        item(id: $id) {
            audioList {
                href
                distributionType
                audioBitrate
                audioCodec
            }
            show {
              title
            }
            image {
              url1X1
            }
            programSet {
              publicationService {
                organizationName
              }
            }
            description
            title
            duration
            startDate
            episodeNumber
        }
    }"#;

const QUERY_PLAYLIST: &str = r#"
    query($id: ID!) {
        show(id: $id) {
            title
            description
            items(filter: { isPublished: { equalTo: true } }) {
                nodes {
                    url
                }
            }
        }
    }"#;

impl ArdResolver {
    async fn resolve_video(&self, display_id: &str, url: &Url) -> Result<Resolved, ResolveError> {
        let mut api_headers = vec![("accept".to_string(), "application/json".to_string())];
        let mut query = vec![
            ("embedded".to_string(), "false".to_string()),
            ("mcV6".to_string(), "true".to_string()),
        ];
        match age_token(&self.http, PLATFORM).await {
            Ok(Some(token)) => {
                api_headers.push((
                    "x-authorization".to_string(),
                    format!("Bearer {}", token.id_token),
                ));
                query.push(("userId".to_string(), token.user_id));
                if token.age_rating != Some(18) {
                    tracing::warn!(
                        platform = PLATFORM,
                        "the account is not verified as 18+; the video may be unavailable"
                    );
                }
            }
            Ok(None) => {
                if self.http.jar(PLATFORM).get("ams").is_some() {
                    tracing::warn!(
                        platform = PLATFORM,
                        "the SSO named no user for the ams cookie; continuing without it"
                    );
                }
            }
            Err(error) => {
                tracing::warn!(
                    platform = PLATFORM,
                    %error,
                    "the age verification token could not be fetched; continuing without it"
                );
            }
        }
        let pairs: Vec<(&str, &str)> = query
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let item_url = util::with_query(
            &Url::parse(&format!("{PAGE_GATEWAY}/pages/ard/item/{display_id}"))
                .map_err(|e| ResolveError::malformed(url, e.to_string()))?,
            &pairs,
        );
        let first = self
            .build_video(&item_url, display_id, &api_headers, &[], url)
            .await?;
        if !first.resolved.variants.is_empty() {
            return Ok(first.resolved);
        }
        if first.geoblocked {
            if let Some(address) = geo::random_ipv4(GEO_COUNTRIES[0]) {
                let forwarded = vec![("x-forwarded-for".to_string(), address)];
                let mut retry_headers = api_headers.clone();
                retry_headers.extend(forwarded.iter().cloned());
                let second = self
                    .build_video(&item_url, display_id, &retry_headers, &forwarded, url)
                    .await?;
                if !second.resolved.variants.is_empty() {
                    return Ok(second.resolved);
                }
            }
            return Err(ResolveError::unavailable(url, GEO_REASON));
        }
        Err(first
            .stream_failure
            .unwrap_or_else(|| ResolveError::unavailable(url, "the player offers no streams")))
    }

    async fn build_video(
        &self,
        item_url: &Url,
        display_id: &str,
        api_headers: &[(String, String)],
        media_headers: &[(String, String)],
        url: &Url,
    ) -> Result<BuiltVideo, ResolveError> {
        let fetched = fetch(
            &self.http,
            item_url,
            PLATFORM,
            BROWSER_UA,
            api_headers,
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        let page = fetched.json(url)?;
        let player = page["widgets"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|widget| {
                matches!(
                    widget["type"].as_str(),
                    Some("player_ondemand" | "player_live")
                )
            })
            .ok_or_else(|| ResolveError::unavailable(url, "the page offers no player"))?;
        let live = player["type"].as_str() == Some("player_live");
        if player["blockedByFsk"].as_bool() == Some(true) {
            return Err(ResolveError::login_required(url, PLATFORM, FSK_REASON));
        }
        let media = &player["mediaCollection"]["embedded"];
        let mut variants = Vec::new();
        let mut subtitles = Vec::new();
        let mut stream_failure = None;
        let streams: Vec<&Value> = media["streams"].as_array().into_iter().flatten().collect();
        // The main stream comes before sign language and other alternatives.
        let ordered = streams
            .iter()
            .filter(|s| s["kind"].as_str() == Some("main"))
            .chain(
                streams
                    .iter()
                    .filter(|s| s["kind"].as_str() != Some("main")),
            );
        for stream in ordered {
            let kind = stream["kind"].as_str().unwrap_or("");
            for item in stream["media"].as_array().into_iter().flatten() {
                let Some(media_url) = util::url_of(&item["url"], None) else {
                    continue;
                };
                let audio_kind = item["audios"][0]["kind"]
                    .as_str()
                    .unwrap_or("")
                    .replace("standard", "");
                let language_code = item["audios"][0]["languageCode"]
                    .as_str()
                    .filter(|c| !c.is_empty())
                    .unwrap_or("deu");
                let language = if audio_kind.is_empty() {
                    language_code.to_string()
                } else {
                    format!("{language_code}-{audio_kind}")
                };
                if path_extension(&media_url).as_deref() == Some("m3u8") {
                    match hls::expand(&self.http, &media_url, PLATFORM, BROWSER_UA, media_headers)
                        .await
                    {
                        Ok(expanded) => {
                            for mut variant in expanded.variants {
                                variant.language = Some(language.clone());
                                variant.format_id = Some(format!("hls-{kind}"));
                                variant.live |= live;
                                variants.push(variant);
                            }
                            subtitles.extend(expanded.subtitles);
                        }
                        Err(error) => stream_failure = Some(error),
                    }
                } else {
                    let mut variant = Variant::file(media_url.clone());
                    variant.format_id = Some(format!("http-{kind}"));
                    variant.language = Some(language);
                    variant.label = item["forcedLabel"].as_str().and_then(clean_title);
                    variant.width = util::u32_of(&item["maxHResolutionPx"]);
                    variant.height = util::u32_of(&item["maxVResolutionPx"]);
                    variant.video = item["videoCodec"].as_str().and_then(video_codec);
                    let ext = path_extension(&media_url).unwrap_or_default();
                    variant.container = Container::from_extension(&ext);
                    if variant.container == Some(Container::Mp4) {
                        variant.video = variant.video.take().or(Some(VideoCodec::H264));
                        variant.audio = Some(AudioCodec::Aac);
                    }
                    variant.live = live;
                    variant.headers = media_headers.to_vec();
                    variants.push(variant);
                }
            }
        }
        for subtitle in media["subtitles"].as_array().into_iter().flatten() {
            let language = subtitle["languageCode"]
                .as_str()
                .filter(|c| !c.is_empty())
                .unwrap_or("deu");
            for source in subtitle["sources"].as_array().into_iter().flatten() {
                let Some(track_url) = util::url_of(&source["url"], None) else {
                    continue;
                };
                let format = match source["kind"].as_str() {
                    Some("webvtt") => SubtitleFormat::Vtt,
                    Some("ebutt") => SubtitleFormat::Ttml,
                    _ => continue,
                };
                subtitles.push(SubtitleTrack {
                    url: track_url,
                    language: language.to_string(),
                    name: None,
                    format,
                    auto: false,
                    headers: media_headers.to_vec(),
                });
            }
        }
        let meta = &media["meta"];
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(
            util::int(&page["tracking"]["atiCustomVars"]["contentId"])
                .map(|id| id.to_string())
                .unwrap_or_else(|| display_id.to_string()),
        );
        resolved.title = meta["title"].as_str().and_then(clean_title).or_else(|| {
            page["title"]
                .as_str()
                .and_then(|t| episode_info(t).episode)
                .and_then(|e| clean_title(&e))
        });
        resolved.description = meta["synopsis"].as_str().and_then(clean_title);
        resolved.uploader = meta["clipSourceName"].as_str().and_then(clean_title);
        resolved.uploaded_at = meta["broadcastedOnDateTime"]
            .as_str()
            .and_then(util::parse_timestamp);
        resolved.duration = util::uint(&meta["durationSeconds"])
            .filter(|d| *d > 0)
            .map(Duration::from_secs)
            .or_else(|| variants.iter().find_map(|v| v.duration));
        resolved.thumbnail = util::url_of(&meta["images"][0]["url"], None);
        resolved.webpage_url = Some(url.clone());
        resolved.live = live || variants.iter().any(|v| v.live);
        resolved.age_limit = page["fskRating"]
            .as_str()
            .and_then(|rating| rating.strip_prefix("FSK"))
            .and_then(|n| n.trim().parse().ok());
        resolved.subtitles = subtitles;
        resolved.variants = variants;
        Ok(BuiltVideo {
            resolved,
            geoblocked: player["geoblocked"].as_bool() == Some(true),
            stream_failure,
        })
    }

    async fn resolve_collection(
        &self,
        kind: CollectionKind,
        id: &str,
        season: Option<&str>,
        version: Option<Version>,
        url: &Url,
    ) -> Result<Playlist, ResolveError> {
        let api_path = match kind {
            CollectionKind::Sammlung => "compilations/ard",
            CollectionKind::Sendung | CollectionKind::Serie => "widgets/ard/asset",
        };
        let base = Url::parse(&format!("{PAGE_GATEWAY}/{api_path}/{id}"))
            .map_err(|e| ResolveError::malformed(url, e.to_string()))?;
        let page_size = PAGE_SIZE.to_string();
        let mut entries = Vec::new();
        let mut title = None;
        let mut total = None;
        for page_number in 0usize.. {
            let number = page_number.to_string();
            let mut pairs: Vec<(&str, &str)> =
                vec![("pageNumber", &number), ("pageSize", &page_size)];
            if let Some(season) = season {
                pairs.push(("seasoned", "true"));
                pairs.push(("seasonNumber", season));
                pairs.push((
                    "withOriginalversion",
                    if version == Some(Version::Original) {
                        "true"
                    } else {
                        "false"
                    },
                ));
                pairs.push((
                    "withAudiodescription",
                    if version == Some(Version::AudioDescription) {
                        "true"
                    } else {
                        "false"
                    },
                ));
            }
            let page_url = util::with_query(&base, &pairs);
            let headers = [("accept".to_string(), "application/json".to_string())];
            let fetched = fetch(
                &self.http, &page_url, PLATFORM, BROWSER_UA, &headers, MAX_PAGE,
            )
            .await?;
            if let Some(error) = status_error(fetched.status, url) {
                return Err(error);
            }
            let data = fetched.json(url)?;
            if page_number == 0 {
                title = data["title"].as_str().and_then(clean_title);
                total = util::uint(&data["pagination"]["totalElements"]).map(|t| t as usize);
            }
            let mut yielded = 0;
            for item in data["teasers"].as_array().into_iter().flatten() {
                let target = &item["links"]["target"];
                let item_id = target["urlId"]
                    .as_str()
                    .or_else(|| target["id"].as_str())
                    .or_else(|| item["id"].as_str())
                    .filter(|i| !i.is_empty());
                let Some(item_id) = item_id else {
                    continue;
                };
                if item_id == id {
                    continue;
                }
                let mode = if item["type"].as_str() == Some("compilation") {
                    "sammlung"
                } else {
                    "video"
                };
                let Ok(entry_url) =
                    Url::parse(&format!("https://www.ardmediathek.de/{mode}/{item_id}"))
                else {
                    continue;
                };
                entries.push(PlaylistEntry {
                    url: entry_url,
                    title: item["longTitle"].as_str().and_then(clean_title),
                    duration: util::seconds(&item["duration"]).filter(|d| !d.is_zero()),
                });
                yielded += 1;
                if entries.len() >= MAX_ENTRIES {
                    break;
                }
            }
            if yielded < PAGE_SIZE || entries.len() >= MAX_ENTRIES {
                break;
            }
        }
        let mut full_id = id.to_string();
        if let Some(season) = season {
            full_id.push('_');
            full_id.push_str(season);
            if let Some(version) = version {
                full_id.push('_');
                full_id.push_str(match version {
                    Version::Original => "OV",
                    Version::AudioDescription => "AD",
                });
            }
        }
        let listed = entries.len();
        Ok(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(full_id),
            title,
            entries,
            total: total.filter(|t| *t > listed),
        })
    }

    /// Runs `query` for `urn` against the Audiothek GraphQL API, returning its `data`.
    async fn graphql(&self, urn: &str, query: &str, url: &Url) -> Result<Value, ResolveError> {
        let endpoint = Url::parse(GRAPHQL).expect("valid");
        let response = self
            .http
            .post(endpoint)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "application/json")
            .json(&json!({ "query": query, "variables": { "id": urn } }))
            .send()
            .await?;
        if let Some(error) = status_error(response.status, url) {
            return Err(error);
        }
        let mut body: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(url, format!("GraphQL JSON: {e}")))?;
        if let Some(message) = body["errors"][0]["message"].as_str() {
            return Err(ResolveError::unavailable(url, message.to_string()));
        }
        Ok(body["data"].take())
    }

    async fn resolve_audio_episode(&self, urn: &str, url: &Url) -> Result<Resolved, ResolveError> {
        let data = self.graphql(urn, QUERY_ITEM, url).await?;
        let item = &data["item"];
        if !item.is_object() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let mut variants = Vec::new();
        for audio in item["audioList"].as_array().into_iter().flatten() {
            let Some(href) = util::url_of(&audio["href"], None) else {
                continue;
            };
            let ext = path_extension(&href).unwrap_or_default();
            let mut variant = Variant::file(href);
            variant.audio_only = true;
            variant.format_id = audio["distributionType"].as_str().map(String::from);
            variant.bitrate = util::uint(&audio["audioBitrate"])
                .filter(|b| *b > 0)
                .map(|kbit| kbit * 1000);
            variant.audio = audio["audioCodec"]
                .as_str()
                .and_then(audio_codec)
                .or_else(|| audio_codec_of_extension(&ext));
            variant.container = Container::from_extension(&ext)
                .or_else(|| (!ext.is_empty()).then(|| Container::Other(ext.clone())));
            variants.push(variant);
        }
        if variants.is_empty() {
            return Err(ResolveError::unavailable(
                url,
                "the episode offers no audio",
            ));
        }
        let mut resolved = Resolved::of(PLATFORM, MediaKind::Audio);
        resolved.id = Some(urn.to_string());
        resolved.title = item["title"].as_str().and_then(clean_title);
        resolved.description = item["description"].as_str().and_then(clean_title);
        resolved.uploader = item["programSet"]["publicationService"]["organizationName"]
            .as_str()
            .and_then(clean_title);
        resolved.uploaded_at = item["startDate"].as_str().and_then(util::parse_timestamp);
        resolved.duration = util::uint(&item["duration"])
            .filter(|d| *d > 0)
            .map(Duration::from_secs);
        resolved.thumbnail = util::url_of(&item["image"]["url1X1"], None).map(|mut image| {
            image.set_query(None);
            image
        });
        resolved.webpage_url = Some(url.clone());
        resolved.variants = variants;
        Ok(resolved)
    }

    async fn resolve_audio_show(&self, urn: &str, url: &Url) -> Result<Playlist, ResolveError> {
        let data = self.graphql(urn, QUERY_PLAYLIST, url).await?;
        let show = &data["show"];
        if !show.is_object() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let mut entries = Vec::new();
        let mut listed = 0usize;
        for node in show["items"]["nodes"].as_array().into_iter().flatten() {
            let Some(episode_url) = util::url_of(&node["url"], None) else {
                continue;
            };
            listed += 1;
            if entries.len() < MAX_ENTRIES {
                entries.push(PlaylistEntry {
                    url: episode_url,
                    title: None,
                    duration: None,
                });
            }
        }
        Ok(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(urn.to_string()),
            title: show["title"].as_str().and_then(clean_title),
            total: (listed > entries.len()).then_some(listed),
            entries,
        })
    }
}

#[async_trait]
impl Resolver for ArdResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "ARD Mediathek",
            hosts: &["ardmediathek.de", "ardaudiothek.de", "ardsounds.de"],
            features: &[
                "videos",
                "live",
                "shows",
                "series",
                "collections",
                "audio",
                "playlists",
            ],
            formats: &["hls", "mp4", "mp3"],
            media: &[MediaKind::Video, MediaKind::Audio],
            tags: &[Tag::News, Tag::Video, Tag::Podcasts],
            session: SessionSupport::Optional,
            examples: &[
                "https://www.ardmediathek.de/video/tatort/nachtschatten/mdr/Y3JpZDovL21kci5kZS9zZW5kdW5nLzI4MTA2MC8yMDI2MDEwMTIwMTUvdGF0b3J0LW1kci1pbS1lcnN0ZW4tMTE4",
                "https://www.ardmediathek.de/video/tagesschau-oder-tagesschau-20-00-uhr/das-erste/Y3JpZDovL2Rhc2Vyc3RlLmRlL3RhZ2Vzc2NoYXUvZmM4ZDUxMjgtOTE0ZC00Y2MzLTgzNzAtNDZkNGNiZWJkOTll",
                "https://www.ardmediathek.de/video/tatort/letzte-ernte/ndr/Y3JpZDovL25kci5kZS81Y2EzNmFjYy0yMWI5LTQzZDYtYjEyYi1hMmZkZGNjNjBmNTVfZ2FuemVTZW5kdW5n",
                "https://www.ardmediathek.de/video/lokalzeit-aus-duesseldorf/lokalzeit-aus-duesseldorf-oder-31-10-2024/wdr-duesseldorf/Y3JpZDovL3dkci5kZS9CZWl0cmFnLXNvcGhvcmEtOWFkMTc0ZWMtMDA5ZS00ZDEwLWFjYjctMGNmNTdhNzVmNzUz",
                "https://www.ardmediathek.de/serie/the-miniaturist/staffel-1-originalversion/Y3JpZDovL3dkci5kZS9vbmUvdGhlbWluaWF0dXJpc3Q/1/OV",
                "https://www.ardmediathek.de/serie/babylon-berlin/staffel-4-mit-audiodeskription/Y3JpZDovL2Rhc2Vyc3RlLmRlL2JhYnlsb24tYmVybGlu/4/AD",
                "https://www.ardmediathek.de/serie/babylon-berlin/staffel-1/Y3JpZDovL2Rhc2Vyc3RlLmRlL2JhYnlsb24tYmVybGlu/1/",
                "https://www.ardmediathek.de/sendung/tatort/Y3JpZDovL2Rhc2Vyc3RlLmRlL3RhdG9ydA",
                "https://www.ardmediathek.de/sammlung/borowski-und-sahin/6MOmy0tTVOX6qEUbw1j9PK",
                "https://www.ardaudiothek.de/episode/urn:ard:episode:eabead1add170e93/",
                "https://www.ardaudiothek.de/episode/urn:ard:section:855c7a53dac72e0a/",
                "https://www.ardsounds.de/episode/urn:ard:extra:d2fe7303d2dcbf5d/",
                "https://www.ardaudiothek.de/sendung/mia-insomnia/urn:ard:show:c405aa26d9a4060a/",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Video(display_id) => Ok(Resolution::from(
                self.resolve_video(&display_id, url).await?,
            )),
            Link::Collection {
                kind,
                id,
                season,
                version,
            } => Ok(Resolution::Playlist(
                self.resolve_collection(kind, &id, season.as_deref(), version, url)
                    .await?,
            )),
            Link::AudioEpisode(urn) => Ok(Resolution::from(
                self.resolve_audio_episode(&urn, url).await?,
            )),
            Link::AudioShow(urn) => Ok(Resolution::Playlist(
                self.resolve_audio_show(&urn, url).await?,
            )),
        }
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        Ok(match age_token(&self.http, PLATFORM).await? {
            Some(token) => SessionCheck::LoggedIn {
                account: token.user_id,
            },
            None => SessionCheck::LoggedOut,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Cookie, Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};

    fn exchange(
        method: &str,
        url: &str,
        status: u16,
        content_type: &str,
        body: String,
    ) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: method.into(),
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

    fn get(url: &str, status: u16, content_type: &str, body: String) -> Exchange {
        exchange("GET", url, status, content_type, body)
    }

    fn post(url: &str, status: u16, body: String) -> Exchange {
        exchange("POST", url, status, "application/json", body)
    }

    const MASTER: &str = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1280x720,CODECS=\"avc1.64001f,mp4a.40.2\"\nhttps://mdrmedia.akamaized.net/x/720/index.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=640x360\nhttps://mdrmedia.akamaized.net/x/360/index.m3u8\n";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n0.ts\n#EXTINF:5.0,\n1.ts\n#EXT-X-ENDLIST\n";
    const LIVE_MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n0.ts\n";

    fn item_page(
        player_type: &str,
        blocked_by_fsk: bool,
        geoblocked: bool,
        streams: Value,
    ) -> String {
        json!({
            "title": "Liebe auf vier Pfoten (Folge 3)",
            "fskRating": "FSK12",
            "tracking": {"atiCustomVars": {"contentId": 12939099}},
            "widgets": [
                {"type": "hero", "title": "not a player"},
                {"type": player_type, "blockedByFsk": blocked_by_fsk, "geoblocked": geoblocked,
                 "mediaCollection": {"embedded": {
                    "meta": {
                        "title": "Liebe auf vier Pfoten", "synopsis": "Claudia Schmitt, Anwältin in Salzburg",
                        "broadcastedOnDateTime": "2023-11-30T11:30:00Z", "seriesTitle": "Filme im MDR",
                        "images": [{"url": "https://api.ardmediathek.de/image-service/images/urn:ard:image:aee7cbf8f06de976?w=960"}],
                        "durationSeconds": 5222, "clipSourceName": "MDR"
                    },
                    "streams": streams,
                    "subtitles": [{"languageCode": "deu", "sources": [
                        {"kind": "ebutt", "url": "https://api.ardmediathek.de/subtitles/ebutt/x.xml"},
                        {"kind": "webvtt", "url": "https://api.ardmediathek.de/subtitles/webvtt/x.vtt"},
                        {"kind": "srt-like", "url": "https://api.ardmediathek.de/subtitles/other/x.srt"}
                    ]}]
                 }}}
            ]
        })
        .to_string()
    }

    fn full_streams() -> Value {
        json!([
            {"kind": "sign-language", "media": [
                {"url": "https://mdrmedia.akamaized.net/dgs/x_640x360.mp4", "maxHResolutionPx": 640, "maxVResolutionPx": 360,
                 "audios": [{"kind": "audio-description", "languageCode": "deu"}]}
            ]},
            {"kind": "main", "media": [
                {"url": "https://mdrmedia.akamaized.net/x/master.m3u8", "audios": [{"kind": "standard", "languageCode": "deu"}]},
                {"url": "https://mdrmedia.akamaized.net/x/1280x720.mp4", "maxHResolutionPx": 1280, "maxVResolutionPx": 720,
                 "videoCodec": "H.264", "forcedLabel": "HD", "audios": [{"kind": "standard", "languageCode": "eng"}]},
                {"url": "", "maxHResolutionPx": 1, "maxVResolutionPx": 1}
            ]}
        ])
    }

    const ITEM_URL: &str = "https://api.ardmediathek.de/page-gateway/pages/ard/item/Y3JpZDovL21kci5kZS9zZW5kdW5nLzI4MjA0MC80MjIwOTEtNDAyNTM0?embedded=false&mcV6=true";
    const VIDEO_URL: &str = "https://www.ardmediathek.de/video/filme-im-mdr/liebe-auf-vier-pfoten/mdr-fernsehen/Y3JpZDovL21kci5kZS9zZW5kdW5nLzI4MjA0MC80MjIwOTEtNDAyNTM0";

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link(VIDEO_URL),
            Some(Link::Video(
                "Y3JpZDovL21kci5kZS9zZW5kdW5nLzI4MjA0MC80MjIwOTEtNDAyNTM0".into()
            ))
        );
        assert_eq!(
            link("https://www.ardmediathek.de/mdr/video/die-robuste-roswita/Y3JpZDovL21kci5kZS9iZWl0cmFnL2Ntcy84MWMxN2MzZC0wMjkxLTRmMzUtODk4ZS0wYzhlOWQxODE2NGI/"),
            Some(Link::Video("Y3JpZDovL21kci5kZS9iZWl0cmFnL2Ntcy84MWMxN2MzZC0wMjkxLTRmMzUtODk4ZS0wYzhlOWQxODE2NGI".into()))
        );
        assert_eq!(
            link("https://beta.ardmediathek.de/ard/video/Y3JpZDovL2Rhc2Vyc3RlLmRlL3RhdG9ydC9mYmM4NGM1NC0xNzU4LTRmZGYtYWFhZS0wYzcyZTIxNGEyMDE"),
            Some(Link::Video("Y3JpZDovL2Rhc2Vyc3RlLmRlL3RhdG9ydC9mYmM4NGM1NC0xNzU4LTRmZGYtYWFhZS0wYzcyZTIxNGEyMDE".into()))
        );
        assert_eq!(
            link(
                "https://ardmediathek.de/ard/video/saartalk/saartalk-gesellschaftsgift-haltung-gegen-hass/sr-fernsehen/Y3JpZDovL3NyLW9ubGluZS5kZS9TVF84MTY4MA/"
            ),
            Some(Link::Video("Y3JpZDovL3NyLW9ubGluZS5kZS9TVF84MTY4MA".into()))
        );
        assert_eq!(
            link("https://www.ardmediathek.de/ard/player/Y3JpZDovL3N3ci5kZS9hZXgvbzEwNzE5MTU/"),
            Some(Link::Video("Y3JpZDovL3N3ci5kZS9hZXgvbzEwNzE5MTU".into()))
        );
        assert_eq!(
            link("https://www.ardmediathek.de/swr/live/Y3JpZDovL3N3ci5kZS8xMzQ4MTA0Mg"),
            Some(Link::Video("Y3JpZDovL3N3ci5kZS8xMzQ4MTA0Mg".into()))
        );
        assert_eq!(
            link(
                "https://www.ardmediathek.de/video/coronavirus-update-ndr-info/astrazeneca-kurz-lockdown-und-pims-syndrom-81/ndr/Y3JpZDovL25kci5kZS84NzE0M2FjNi0wMWEwLTQ5ODEtOTE5NS1mOGZhNzdhOTFmOTI/?query=1#top"
            ),
            Some(Link::Video(
                "Y3JpZDovL25kci5kZS84NzE0M2FjNi0wMWEwLTQ5ODEtOTE5NS1mOGZhNzdhOTFmOTI".into()
            ))
        );

        assert_eq!(
            link(
                "https://www.ardmediathek.de/serie/the-miniaturist/staffel-1-originalversion/Y3JpZDovL3dkci5kZS9vbmUvdGhlbWluaWF0dXJpc3Q/1/OV"
            ),
            Some(Link::Collection {
                kind: CollectionKind::Serie,
                id: "Y3JpZDovL3dkci5kZS9vbmUvdGhlbWluaWF0dXJpc3Q".into(),
                season: Some("1".into()),
                version: Some(Version::Original),
            })
        );
        assert_eq!(
            link(
                "https://www.ardmediathek.de/serie/babylon-berlin/staffel-4-mit-audiodeskription/Y3JpZDovL2Rhc2Vyc3RlLmRlL2JhYnlsb24tYmVybGlu/4/AD"
            ),
            Some(Link::Collection {
                kind: CollectionKind::Serie,
                id: "Y3JpZDovL2Rhc2Vyc3RlLmRlL2JhYnlsb24tYmVybGlu".into(),
                season: Some("4".into()),
                version: Some(Version::AudioDescription),
            })
        );
        assert_eq!(
            link(
                "https://www.ardmediathek.de/serie/babylon-berlin/staffel-1/Y3JpZDovL2Rhc2Vyc3RlLmRlL2JhYnlsb24tYmVybGlu/1/"
            ),
            Some(Link::Collection {
                kind: CollectionKind::Serie,
                id: "Y3JpZDovL2Rhc2Vyc3RlLmRlL2JhYnlsb24tYmVybGlu".into(),
                season: Some("1".into()),
                version: None,
            })
        );
        assert_eq!(
            link("https://www.ardmediathek.de/sendung/tatort/Y3JpZDovL2Rhc2Vyc3RlLmRlL3RhdG9ydA"),
            Some(Link::Collection {
                kind: CollectionKind::Sendung,
                id: "Y3JpZDovL2Rhc2Vyc3RlLmRlL3RhdG9ydA".into(),
                season: None,
                version: None,
            })
        );
        assert_eq!(
            link("https://www.ardmediathek.de/sammlung/borowski-und-sahin/6MOmy0tTVOX6qEUbw1j9PK"),
            Some(Link::Collection {
                kind: CollectionKind::Sammlung,
                id: "6MOmy0tTVOX6qEUbw1j9PK".into(),
                season: None,
                version: None,
            })
        );
        assert_eq!(
            link(
                "https://www.ardmediathek.de/ard/sendung/doctor-who/Y3JpZDovL3dkci5kZS9vbmUvZG9jdG9yIHdobw/"
            ),
            Some(Link::Collection {
                kind: CollectionKind::Sendung,
                id: "Y3JpZDovL3dkci5kZS9vbmUvZG9jdG9yIHdobw".into(),
                season: None,
                version: None,
            })
        );
        assert_eq!(
            link(
                "https://www.ardmediathek.de/serie/nachtstreife/staffel-1/Y3JpZDovL3N3ci5kZS9zZGIvc3RJZC8xMjQy/1"
            ),
            Some(Link::Collection {
                kind: CollectionKind::Serie,
                id: "Y3JpZDovL3N3ci5kZS9zZGIvc3RJZC8xMjQy".into(),
                season: Some("1".into()),
                version: None,
            })
        );
        assert_eq!(
            link("https://www.ardmediathek.de/ard/sammlung/team-muenster/5JpTzLSbWUAK8184IOvEir/"),
            Some(Link::Collection {
                kind: CollectionKind::Sammlung,
                id: "5JpTzLSbWUAK8184IOvEir".into(),
                season: None,
                version: None,
            })
        );

        assert_eq!(
            link("https://www.ardaudiothek.de/episode/urn:ard:episode:eabead1add170e93/"),
            Some(Link::AudioEpisode(
                "urn:ard:episode:eabead1add170e93".into()
            ))
        );
        assert_eq!(
            link("https://www.ardaudiothek.de/episode/urn:ard:section:855c7a53dac72e0a/"),
            Some(Link::AudioEpisode(
                "urn:ard:section:855c7a53dac72e0a".into()
            ))
        );
        assert_eq!(
            link("https://www.ardsounds.de/episode/urn:ard:extra:d2fe7303d2dcbf5d/"),
            Some(Link::AudioEpisode("urn:ard:extra:d2fe7303d2dcbf5d".into()))
        );
        assert_eq!(
            link("https://www.ardaudiothek.de/sendung/mia-insomnia/urn:ard:show:c405aa26d9a4060a/"),
            Some(Link::AudioShow("urn:ard:show:c405aa26d9a4060a".into()))
        );
        assert_eq!(
            link("https://www.ardsounds.de/sendung/100-berlin/urn:ard:show:4d248e0806ce37bc/"),
            Some(Link::AudioShow("urn:ard:show:4d248e0806ce37bc".into()))
        );

        assert_eq!(link("https://www.ardmediathek.de/"), None);
        assert_eq!(link("https://www.ardmediathek.de/suche/tatort"), None);
        assert_eq!(link("https://www.ardmediathek.de/video/"), None);
        assert_eq!(
            link("https://www.ardaudiothek.de/episode/mia-insomnia/folge-1/12345/"),
            None
        );
        assert_eq!(
            link("https://www.ardaudiothek.de/sendung/mia-insomnia/"),
            None
        );
        assert_eq!(link("https://www.zdf.de/video/abc"), None);
        assert_eq!(link("ftp://www.ardmediathek.de/video/abc"), None);
    }

    #[test]
    fn episode_numbering_is_read_from_titles() {
        let info = episode_info("Homo sapiens (S06/E07) - Originalversion");
        assert_eq!(info.season_number, Some(6));
        assert_eq!(info.episode_number, Some(7));
        assert_eq!(
            info.episode.as_deref(),
            Some("Homo sapiens - Originalversion")
        );

        let info = episode_info("Fritjof aus Norwegen (2) (AD)");
        assert_eq!(info.season_number, None);
        assert_eq!(info.episode_number, Some(2));
        assert_eq!(info.episode.as_deref(), Some("Fritjof aus Norwegen (AD)"));

        let info = episode_info("Folge 12: \"Der Anfang\" - Wiederholung");
        assert_eq!(info.episode_number, Some(12));
        assert_eq!(info.episode.as_deref(), Some("Der Anfang"));

        let info = episode_info("Folge 25/42: Symmetrie");
        assert_eq!(info.episode_number, Some(25));
        assert_eq!(info.episode.as_deref(), Some("Symmetrie"));

        let info = episode_info("Folge 1063 - Vertrauen");
        assert_eq!(info.episode_number, Some(1063));
        assert_eq!(info.episode.as_deref(), Some("Vertrauen"));

        let info = episode_info("tagesschau, 20:00 Uhr");
        assert_eq!(info.episode_number, None);
        assert_eq!(info.episode.as_deref(), Some("tagesschau, 20:00 Uhr"));
    }

    #[test]
    fn jwt_claims_are_read() {
        let payload = util::b64_encode_url(br#"{"user_id":"u-1","age_rating":18}"#);
        let claims = jwt_payload(&format!("eyJhbGciOiJIUzI1NiJ9.{payload}.sig")).unwrap();
        assert_eq!(claims["user_id"].as_str(), Some("u-1"));
        assert_eq!(claims["age_rating"].as_i64(), Some(18));
        assert_eq!(jwt_payload("not-a-token"), None);
    }

    #[tokio::test]
    async fn mediathek_videos_resolve_with_every_stream() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            ITEM_URL,
            200,
            "application/json",
            item_page("player_ondemand", false, false, full_streams()),
        ));
        fixture.exchanges.push(get(
            "https://mdrmedia.akamaized.net/x/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MASTER.into(),
        ));
        fixture.exchanges.push(get(
            "https://mdrmedia.akamaized.net/x/720/index.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MEDIA.into(),
        ));
        let resolver = ArdResolver::new(Http::replay(fixture));
        let url = Url::parse(VIDEO_URL).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("12939099"));
        assert_eq!(resolved.title.as_deref(), Some("Liebe auf vier Pfoten"));
        assert_eq!(
            resolved.description.as_deref(),
            Some("Claudia Schmitt, Anwältin in Salzburg")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("MDR"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(5222)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1701343800)
        );
        assert_eq!(resolved.age_limit, Some(12));
        assert!(!resolved.live);
        assert_eq!(
            resolved.thumbnail.as_ref().map(Url::as_str),
            Some(
                "https://api.ardmediathek.de/image-service/images/urn:ard:image:aee7cbf8f06de976?w=960"
            )
        );
        // Two HLS renditions and the progressive main file come before the sign-language file.
        assert_eq!(resolved.variants.len(), 4);
        assert_eq!(resolved.media, MediaKind::Video);
        assert_eq!(resolved.variants[0].kind, VariantKind::Hls);
        assert_eq!(resolved.variants[0].height, Some(720));
        assert_eq!(resolved.variants[0].language.as_deref(), Some("deu"));
        assert_eq!(resolved.variants[0].format_id.as_deref(), Some("hls-main"));
        assert_eq!(resolved.variants[0].duration, Some(Duration::from_secs(15)));
        let file = &resolved.variants[2];
        assert_eq!(file.kind, VariantKind::File);
        assert_eq!(
            file.url.as_str(),
            "https://mdrmedia.akamaized.net/x/1280x720.mp4"
        );
        assert_eq!(file.format_id.as_deref(), Some("http-main"));
        assert_eq!(file.label.as_deref(), Some("HD"));
        assert_eq!((file.width, file.height), (Some(1280), Some(720)));
        assert_eq!(file.video, Some(VideoCodec::H264));
        assert_eq!(file.audio, Some(AudioCodec::Aac));
        assert_eq!(file.language.as_deref(), Some("eng"));
        let signed = &resolved.variants[3];
        assert_eq!(signed.format_id.as_deref(), Some("http-sign-language"));
        assert_eq!(signed.language.as_deref(), Some("deu-audio-description"));
        assert_eq!(resolved.subtitles.len(), 2);
        assert_eq!(resolved.subtitles[0].format, SubtitleFormat::Ttml);
        assert_eq!(resolved.subtitles[1].format, SubtitleFormat::Vtt);
        assert_eq!(resolved.subtitles[1].language, "deu");
    }

    #[tokio::test]
    async fn live_streams_are_marked_live() {
        let mut fixture = Fixture::new(PLATFORM, None);
        let streams = json!([{"kind": "main", "media": [{"url": "https://mdrmedia.akamaized.net/x/master.m3u8"}]}]);
        fixture.exchanges.push(get(
            ITEM_URL,
            200,
            "application/json",
            item_page("player_live", false, false, streams),
        ));
        fixture.exchanges.push(get(
            "https://mdrmedia.akamaized.net/x/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MASTER.into(),
        ));
        fixture.exchanges.push(get(
            "https://mdrmedia.akamaized.net/x/720/index.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            LIVE_MEDIA.into(),
        ));
        let resolver = ArdResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse(VIDEO_URL).unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert!(resolved.live);
        assert!(resolved.variants.iter().all(|v| v.live));
        assert_eq!(resolved.variants[0].language.as_deref(), Some("deu"));
    }

    #[tokio::test]
    async fn age_verified_sessions_send_the_sso_token() {
        let payload = util::b64_encode_url(br#"{"sub":"user-77","age_rating":18}"#);
        let id_token = format!("eyJhbGciOiJIUzI1NiJ9.{payload}.sig");
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            TOKEN_URL,
            200,
            "application/json",
            json!({"idToken": id_token}).to_string(),
        ));
        let with_user = format!("{ITEM_URL}&userId=user-77");
        let streams = json!([{"kind": "main", "media": [{"url": "https://mdrmedia.akamaized.net/x/1280x720.mp4"}]}]);
        fixture.exchanges.push(get(
            &with_user,
            200,
            "application/json",
            item_page("player_ondemand", false, false, streams),
        ));
        let http = Http::replay(fixture);
        http.with_jar(PLATFORM, |jar| {
            jar.insert(Cookie::new("ams", "session", "sso.ardmediathek.de"))
        });
        let resolver = ArdResolver::new(http);
        assert_eq!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedIn {
                account: "user-77".into()
            }
        );
        let resolved = resolver
            .resolve(&Url::parse(VIDEO_URL).unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(
            resolved.variants[0].url.as_str(),
            "https://mdrmedia.akamaized.net/x/1280x720.mp4"
        );

        let resolver = ArdResolver::new(Http::replay(Fixture::new(PLATFORM, None)));
        assert_eq!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedOut
        );
    }

    #[tokio::test]
    async fn refused_videos_say_why() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            ITEM_URL,
            200,
            "application/json",
            item_page("player_ondemand", true, false, json!([])),
        ));
        let resolver = ArdResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse(VIDEO_URL).unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(
                &error,
                ResolveError::LoginRequired {
                    platform: "ard",
                    ..
                }
            ),
            "{error}"
        );

        // A geo-blocked page is asked again from a German address; still empty, it is refused.
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            ITEM_URL,
            200,
            "application/json",
            item_page("player_ondemand", false, true, json!([])),
        ));
        fixture.exchanges.push(get(
            ITEM_URL,
            200,
            "application/json",
            item_page("player_ondemand", false, true, json!([])),
        ));
        let resolver = ArdResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse(VIDEO_URL).unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == GEO_REASON),
            "{error}"
        );

        // The retry from a German address may succeed.
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            ITEM_URL,
            200,
            "application/json",
            item_page("player_ondemand", false, true, json!([])),
        ));
        let streams = json!([{"kind": "main", "media": [{"url": "https://mdrmedia.akamaized.net/x/1280x720.mp4"}]}]);
        fixture.exchanges.push(get(
            ITEM_URL,
            200,
            "application/json",
            item_page("player_ondemand", false, true, streams),
        ));
        let resolver = ArdResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse(VIDEO_URL).unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.variants.len(), 1);
        assert!(
            resolved.variants[0]
                .headers
                .iter()
                .any(|(name, _)| name == "x-forwarded-for")
        );

        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            ITEM_URL,
            404,
            "application/json",
            json!({"message": "not found"}).to_string(),
        ));
        let resolver = ArdResolver::new(Http::replay(fixture));
        assert!(matches!(
            resolver
                .resolve(&Url::parse(VIDEO_URL).unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(at) if at.as_str() == VIDEO_URL
        ));

        // A playlist that cannot be read leaves the reason.
        let mut fixture = Fixture::new(PLATFORM, None);
        let streams = json!([{"kind": "main", "media": [{"url": "https://mdrmedia.akamaized.net/x/master.m3u8"}]}]);
        fixture.exchanges.push(get(
            ITEM_URL,
            200,
            "application/json",
            item_page("player_ondemand", false, false, streams),
        ));
        fixture.exchanges.push(get(
            "https://mdrmedia.akamaized.net/x/master.m3u8",
            403,
            "text/plain",
            "denied".into(),
        ));
        let resolver = ArdResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse(VIDEO_URL).unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("403")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn collections_list_their_videos() {
        let mut fixture = Fixture::new(PLATFORM, None);
        let series = "https://api.ardmediathek.de/page-gateway/widgets/ard/asset/Y3JpZDovL3dkci5kZS9vbmUvdGhlbWluaWF0dXJpc3Q?pageNumber=0&pageSize=100&seasoned=true&seasonNumber=1&withOriginalversion=true&withAudiodescription=false";
        fixture.exchanges.push(get(series, 200, "application/json", json!({
            "title": "Staffel 1 Originalversion",
            "pagination": {"pageNumber": 0, "pageSize": 100, "totalElements": 3},
            "teasers": [
                {"id": "a1", "type": "ondemand", "longTitle": "The Miniaturist (S01/E01)", "duration": 1500, "links": {"target": {"urlId": "Y3JpZDovL3dkci5kZS9vbmUvdGhlbWluaWF0dXJpc3QvMQ", "id": "a1"}}},
                {"id": "a2", "type": "ondemand", "longTitle": "The Miniaturist (S01/E02)", "duration": 1520, "links": {"target": {"id": "Y3JpZDovL3dkci5kZS9vbmUvdGhlbWluaWF0dXJpc3QvMg"}}},
                {"id": "Y3JpZDovL3dkci5kZS9vbmUvdGhlbWluaWF0dXJpc3Q", "type": "ondemand", "longTitle": "the series itself"},
                {"id": "c1", "type": "compilation", "longTitle": "Extras", "links": {"target": {"urlId": "6MOmy0tTVOX6qEUbw1j9PK"}}}
            ]
        }).to_string()));
        let resolver = ArdResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.ardmediathek.de/serie/the-miniaturist/staffel-1-originalversion/Y3JpZDovL3dkci5kZS9vbmUvdGhlbWluaWF0dXJpc3Q/1/OV").unwrap();
        assert!(resolver.matches(&url));
        let Resolution::Playlist(playlist) = resolver.resolve(&url).await.unwrap() else {
            panic!("expected a playlist");
        };
        assert_eq!(
            playlist.id.as_deref(),
            Some("Y3JpZDovL3dkci5kZS9vbmUvdGhlbWluaWF0dXJpc3Q_1_OV")
        );
        assert_eq!(playlist.title.as_deref(), Some("Staffel 1 Originalversion"));
        assert_eq!(playlist.entries.len(), 3);
        assert_eq!(playlist.total, None);
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://www.ardmediathek.de/video/Y3JpZDovL3dkci5kZS9vbmUvdGhlbWluaWF0dXJpc3QvMQ"
        );
        assert_eq!(
            playlist.entries[0].title.as_deref(),
            Some("The Miniaturist (S01/E01)")
        );
        assert_eq!(
            playlist.entries[0].duration,
            Some(Duration::from_secs(1500))
        );
        assert_eq!(
            playlist.entries[1].url.as_str(),
            "https://www.ardmediathek.de/video/Y3JpZDovL3dkci5kZS9vbmUvdGhlbWluaWF0dXJpc3QvMg"
        );
        assert_eq!(
            playlist.entries[2].url.as_str(),
            "https://www.ardmediathek.de/sammlung/6MOmy0tTVOX6qEUbw1j9PK"
        );
        for entry in &playlist.entries {
            assert!(resolver.matches(&entry.url), "{}", entry.url);
        }

        // A collection reads the compilations endpoint, and pages while pages are full.
        let mut fixture = Fixture::new(PLATFORM, None);
        let first: Vec<Value> = (0..100)
            .map(|n| json!({"id": format!("v{n}"), "type": "ondemand", "longTitle": format!("Clip {n}"), "links": {"target": {"urlId": format!("id{n}")}}}))
            .collect();
        fixture.exchanges.push(get(
            "https://api.ardmediathek.de/page-gateway/compilations/ard/6MOmy0tTVOX6qEUbw1j9PK?pageNumber=0&pageSize=100",
            200, "application/json",
            json!({"title": "Tatort aus Kiel | Borowski und Sahin", "pagination": {"totalElements": 101}, "teasers": first}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.ardmediathek.de/page-gateway/compilations/ard/6MOmy0tTVOX6qEUbw1j9PK?pageNumber=1&pageSize=100",
            200, "application/json",
            json!({"title": "Tatort aus Kiel | Borowski und Sahin", "teasers": [{"id": "v100", "type": "ondemand", "links": {"target": {"urlId": "id100"}}}]}).to_string(),
        ));
        let resolver = ArdResolver::new(Http::replay(fixture));
        let url = Url::parse(
            "https://www.ardmediathek.de/sammlung/borowski-und-sahin/6MOmy0tTVOX6qEUbw1j9PK",
        )
        .unwrap();
        let Resolution::Playlist(playlist) = resolver.resolve(&url).await.unwrap() else {
            panic!("expected a playlist");
        };
        assert_eq!(playlist.id.as_deref(), Some("6MOmy0tTVOX6qEUbw1j9PK"));
        assert_eq!(
            playlist.title.as_deref(),
            Some("Tatort aus Kiel | Borowski und Sahin")
        );
        assert_eq!(playlist.entries.len(), 101);
        assert_eq!(
            playlist.entries[100].url.as_str(),
            "https://www.ardmediathek.de/video/id100"
        );

        // A listing the gateway no longer has is reported at the link the user gave.
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://api.ardmediathek.de/page-gateway/widgets/ard/asset/gone?pageNumber=0&pageSize=100",
            404, "application/json", "{}".into(),
        ));
        let resolver = ArdResolver::new(Http::replay(fixture));
        let gone = "https://www.ardmediathek.de/sendung/x/gone";
        assert!(matches!(
            resolver.resolve(&Url::parse(gone).unwrap()).await.unwrap_err(),
            ResolveError::NotFound(at) if at.as_str() == gone
        ));

        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://api.ardmediathek.de/page-gateway/compilations/ard/5eOHzt8XB2sqeFXbIoJlg2?pageNumber=0&pageSize=100",
            404, "application/json", json!({"message": "not found"}).to_string(),
        ));
        let resolver = ArdResolver::new(Http::replay(fixture));
        let expired =
            "https://www.ardmediathek.de/sammlung/die-kirche-bleibt-im-dorf/5eOHzt8XB2sqeFXbIoJlg2";
        assert!(matches!(
            resolver.resolve(&Url::parse(expired).unwrap()).await.unwrap_err(),
            ResolveError::NotFound(at) if at.as_str() == expired
        ));
    }

    #[tokio::test]
    async fn audiothek_episodes_and_shows_resolve() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(post(GRAPHQL, 200, json!({"data": {"item": {
            "audioList": [
                {"href": "https://ardmediathek-a.akamaihd.net/x/caiman.mp3", "distributionType": "Download", "audioBitrate": 128, "audioCodec": "mp3"},
                {"href": "https://ardmediathek-a.akamaihd.net/x/caiman-low.mp3", "distributionType": "Stream", "audioBitrate": 64, "audioCodec": null},
                {"href": null, "distributionType": "Broken"}
            ],
            "show": {"title": "1LIVE Caiman Club"},
            "image": {"url1X1": "https://api.ardmediathek.de/image-service/images/urn:ard:image:ed64411a07a4b405?w=448&ch=abc"},
            "programSet": {"publicationService": {"organizationName": "WDR"}},
            "description": "Cash Out.", "title": "CAIMAN CLUB (S04E04): Cash Out",
            "duration": 3339, "startDate": "2024-07-17T04:00:41+02:00", "episodeNumber": 4
        }}}).to_string()));
        let resolver = ArdResolver::new(Http::replay(fixture));
        let url =
            Url::parse("https://www.ardaudiothek.de/episode/urn:ard:episode:eabead1add170e93/")
                .unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(
            resolved.id.as_deref(),
            Some("urn:ard:episode:eabead1add170e93")
        );
        assert_eq!(
            resolved.title.as_deref(),
            Some("CAIMAN CLUB (S04E04): Cash Out")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("WDR"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(3339)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1721181641)
        );
        assert_eq!(
            resolved.thumbnail.as_ref().map(Url::as_str),
            Some("https://api.ardmediathek.de/image-service/images/urn:ard:image:ed64411a07a4b405")
        );
        assert_eq!(resolved.variants.len(), 2);
        let best = &resolved.variants[0];
        assert!(best.audio_only);
        assert_eq!(best.format_id.as_deref(), Some("Download"));
        assert_eq!(best.bitrate, Some(128_000));
        assert_eq!(best.audio, Some(AudioCodec::Mp3));
        assert_eq!(best.container, Some(Container::Mp3));
        assert_eq!(resolved.media, MediaKind::Audio);
        assert_eq!(resolved.variants[1].audio, Some(AudioCodec::Mp3));

        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(post(GRAPHQL, 200, json!({"data": {"show": {
            "title": "Mia Insomnia", "description": "Ein Hörspiel.",
            "items": {"nodes": [
                {"url": "https://www.ardaudiothek.de/episode/urn:ard:episode:1111111111111111/"},
                {"url": "https://www.ardaudiothek.de/episode/urn:ard:episode:2222222222222222/"},
                {"url": null}
            ]}
        }}}).to_string()));
        let resolver = ArdResolver::new(Http::replay(fixture));
        let url = Url::parse(
            "https://www.ardaudiothek.de/sendung/mia-insomnia/urn:ard:show:c405aa26d9a4060a/",
        )
        .unwrap();
        let Resolution::Playlist(playlist) = resolver.resolve(&url).await.unwrap() else {
            panic!("expected a playlist");
        };
        assert_eq!(
            playlist.id.as_deref(),
            Some("urn:ard:show:c405aa26d9a4060a")
        );
        assert_eq!(playlist.title.as_deref(), Some("Mia Insomnia"));
        assert_eq!(playlist.entries.len(), 2);
        assert!(resolver.matches(&playlist.entries[0].url));

        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(post(
            GRAPHQL,
            200,
            json!({"data": {"item": null}}).to_string(),
        ));
        let resolver = ArdResolver::new(Http::replay(fixture));
        let url =
            Url::parse("https://www.ardsounds.de/episode/urn:ard:extra:d2fe7303d2dcbf5d/").unwrap();
        assert!(matches!(
            resolver.resolve(&url).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn media_collections_of_regional_players_are_read() {
        let base = Url::parse("https://www.sr-mediathek.de/player.json?id=141317").unwrap();
        let origin = Url::parse("https://www.sr-mediathek.de/index.php?seite=7&id=141317").unwrap();
        let info = json!({
            "_type": "video", "_duration": 1788, "_isLive": false,
            "_previewImage": "/images/141317.jpg",
            "_subtitleUrl": "https://www.sr-mediathek.de/ebutt/141317.xml",
            "_mediaArray": [
                {"_mediaStreamArray": [
                    {"_quality": "auto", "_stream": "https://sr.akamaized.net/x/master.m3u8"},
                    {"_quality": "auto", "_stream": "https://sr.akamaized.net/x/manifest.f4m"},
                    {"_quality": 2, "_stream": "https://sr.akamaized.net/x/other.m3u8"},
                    {"_quality": 1, "_stream": ["https://sr.akamaized.net/x/clip_960x540.mp4", "https://sr.akamaized.net/x/clip_640x360.mp4"]},
                    {"_quality": 3, "_server": "rtmp://sr.fcod.llnwd.net/a1234/e1", "_stream": "mp4:clip_1280x720.mp4"},
                    {"_quality": 0, "_stream": "not a link"}
                ]}
            ]
        });
        let mut fixture = Fixture::new("srmediathek", None);
        fixture.exchanges.push(get(
            base.as_str(),
            200,
            "application/json",
            info.to_string(),
        ));
        fixture.exchanges.push(get(
            "https://sr.akamaized.net/x/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MASTER.into(),
        ));
        fixture.exchanges.push(get(
            "https://mdrmedia.akamaized.net/x/720/index.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MEDIA.into(),
        ));
        let http = Http::replay(fixture);
        let media = extract_media_info(
            &http,
            "srmediathek",
            &base,
            "<html>no marker</html>",
            &origin,
        )
        .await
        .unwrap();
        assert_eq!(media.duration, Some(Duration::from_secs(1788)));
        assert_eq!(
            media.thumbnail.as_ref().map(Url::as_str),
            Some("https://www.sr-mediathek.de/images/141317.jpg")
        );
        assert!(!media.live);
        let ids: Vec<&str> = media
            .variants
            .iter()
            .filter_map(|v| v.format_id.as_deref())
            .collect();
        assert_eq!(ids, ["a0-mp4-1", "a0-mp4-1", "a0-rtmp-3"]);
        assert_eq!(media.variants.len(), 5);
        let hls_count = media
            .variants
            .iter()
            .filter(|v| v.kind == VariantKind::Hls)
            .count();
        assert_eq!(hls_count, 2);
        let big = media
            .variants
            .iter()
            .find(|v| v.url.as_str().ends_with("clip_960x540.mp4"))
            .unwrap();
        assert_eq!((big.width, big.height), (Some(960), Some(540)));
        assert_eq!(big.video, Some(VideoCodec::H264));
        let rtmp = media
            .variants
            .iter()
            .find(|v| v.kind == VariantKind::Rtmp)
            .unwrap();
        assert_eq!(
            rtmp.url.as_str(),
            "rtmp://sr.fcod.llnwd.net/a1234/e1/mp4:clip_1280x720.mp4"
        );
        assert_eq!((rtmp.width, rtmp.height), (Some(1280), Some(720)));
        assert_eq!(media.subtitles.len(), 2);
        assert_eq!(media.subtitles[0].format, SubtitleFormat::Ttml);
        assert_eq!(
            media.subtitles[1].url.as_str(),
            "https://www.sr-mediathek.de/webvtt/141317.xml.vtt"
        );
        assert_eq!(media.subtitles[1].format, SubtitleFormat::Vtt);

        // Audio clips are audio-only files.
        let audio = json!({"_type": "audio", "_duration": 139, "_mediaArray": [{"_mediaStreamArray": [{"_quality": 1, "_stream": "https://sr.akamaized.net/x/clip.mp3"}]}]});
        let media = parse_media_info(
            &Http::replay(Fixture::new("srmediathek", None)),
            "srmediathek",
            &audio,
            false,
            &base,
        )
        .await
        .unwrap();
        assert_eq!(media.variants.len(), 1);
        assert!(media.variants[0].audio_only);
        assert_eq!(media.variants[0].audio, Some(AudioCodec::Mp3));
        assert_eq!(media.variants[0].format_id.as_deref(), Some("a0-mp3-1"));

        // Held back until the evening, refused by region, or HDS only.
        let empty = json!({"_mediaArray": [], "_geoblocked": true});
        let http = Http::replay(Fixture::new("srmediathek", None));
        let error = parse_media_info(&http, "srmediathek", &empty, true, &base)
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("20:00")),
            "{error}"
        );
        let error = parse_media_info(&http, "srmediathek", &empty, false, &base)
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == GEO_REASON),
            "{error}"
        );
        let hds = json!({"_mediaArray": [{"_mediaStreamArray": [{"_quality": "auto", "_stream": "https://sr.akamaized.net/x/manifest.f4m"}]}]});
        let error = parse_media_info(&http, "srmediathek", &hds, false, &base)
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("HDS")),
            "{error}"
        );

        // A region refusal is retried from a German address.
        let mut fixture = Fixture::new("srmediathek", None);
        fixture.exchanges.push(get(
            base.as_str(),
            200,
            "application/json",
            empty.to_string(),
        ));
        fixture.exchanges.push(get(
            base.as_str(),
            200,
            "application/json",
            audio.to_string(),
        ));
        let http = Http::replay(fixture);
        let media = extract_media_info(&http, "srmediathek", &base, "", &origin)
            .await
            .unwrap();
        assert_eq!(media.variants.len(), 1);
        assert!(
            media.variants[0]
                .headers
                .iter()
                .any(|(name, _)| name == "x-forwarded-for")
        );
    }
}
