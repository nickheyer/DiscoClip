//! RaiPlay, Rai News and the Rai sites. A RaiPlay page has a JSON twin at its own path
//! with `.json` in place of `.html`, naming the relinker link its media comes from; a
//! live channel's page has the same; a programme's JSON lists blocks of content sets,
//! each a JSON of the episodes it holds, and a season link picks one set out. A Rai News
//! page carries its relinker link in the `<rainews-player>` tag, an article carries the
//! player of the video it embeds or a frame to it, and a rai.it or rai.tv page is read
//! through the ContentItem JSON the rai.tv catalogue serves for its id. The relinker,
//! asked as the `Rai` client so it lists every quality, answers with an HLS playlist, an
//! MP4 or an MP3, flags live streams, names the DRM licence when there is one, and hands
//! addresses outside Italy a placeholder video, reported as the geo gate it is. A video's
//! MP4 downloads come from the relinker with a quality named, one per rendition of its
//! playlist.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use jiff::civil::{Date, DateTime, Time};
use jiff::tz::TimeZone;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionSupport, SubtitleFormat, SubtitleTrack, Tag, Variant, VariantKind,
    clean_title, essence, fetch, hls, navigation_headers, path_extension, probe_file, status_error,
    util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "raiplay";
const RAIPLAY: &str = "https://www.raiplay.it";
const RAINEWS: &str = "https://www.rainews.it";
const RAI: &str = "https://www.rai.it";
/// The rai.tv catalogue's JSON for a ContentItem id.
const ITEM_JSON: &str = "https://www.rai.tv/dl/RaiTV/programmi/media/ContentItem-";
const GEO_COUNTRY: &str = "IT";
/// The client the relinker lists every quality for.
const RELINKER_UA: &str = "Rai";
/// The video the relinker hands addresses it refuses.
const PLACEHOLDER: &str = "/video_no_available.mp4";
/// How many episodes a programme's playlist holds at most.
const PROGRAMME_LIMIT: usize = 500;
const ZONE: &str = "Europe/Rome";

const UUID: &str = r"[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12}";

/// `/video/2014/04/Report-del-07042014-{uuid}.html`, or its `.json` twin.
static RE_PLAY_ITEM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"^(/.+?-({UUID}))\.(?:html|json)$")).unwrap());
/// `/dirette/rainews24`.
static RE_LIVE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/dirette/([^/?#.]+)(?:\.json)?/?$").unwrap());
/// `/programmi/report`, `/programmi/report/stagione-2024-2025/puntate`.
static RE_PROGRAMME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/programmi/([^/?#.]+)(?:\.json)?(?:/([^?#]+?))?/?$").unwrap());
/// `/video/2022/12/title-{uuid}.html`, `/iframe/video/2022/07/title-{uuid}.html`, and the
/// rai.it shapes such as `/dl/RaiTV/programmi/media/ContentItem-{uuid}.html`.
static RE_NEWS_ITEM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(&format!(r"^/.+?-({UUID})(?:-[^/?#]*)?\.html$")).unwrap());
/// `<rainews-player data='{…}'>`: the player's data, HTML-escaped JSON.
static RE_PLAYER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<rai(?:news|cultura)-player\s+data=(?:'([^']*)'|"([^"]*)")"#).unwrap()
});
/// `<iframe data-src="/iframe/video/…-{uuid}.html">`: the video an article embeds.
static RE_IFRAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"<iframe[^>]+(?:data-src|src)=["'](/iframe/[^"'?#]*?-{UUID}\.html)["']"#
    ))
    .unwrap()
});
/// `/2440873_,800,1800,.mp4.csmil/playlist.m3u8`: the qualities a playlist is cut in.
static RE_QUALITIES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"/\w+(?:_([\d,]+))?(?:\.mp4)?(?:\.csmil)?/playlist\.m3u8").unwrap()
});

/// The frame size of the relinker's MP4 of a bitrate in kb/s, for a quality no playlist
/// rendition matches.
const QUALITY_SIZES: &[(u64, u32, u32)] = &[
    (250, 352, 198),
    (400, 512, 288),
    (600, 512, 288),
    (700, 512, 288),
    (800, 700, 394),
    (1200, 736, 414),
    (1500, 920, 518),
    (1800, 1024, 576),
    (2400, 1280, 720),
    (3200, 1440, 810),
    (3600, 1440, 810),
    (5000, 1920, 1080),
    (10000, 1920, 1080),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A RaiPlay video: its path without the extension, and its id.
    Video { path: String, id: String },
    /// A RaiPlay live channel.
    Live { channel: String },
    /// A RaiPlay programme, or one `block/set` section of it.
    Programme {
        slug: String,
        section: Option<String>,
    },
    /// A Rai News video page, a regional one, or a player frame.
    News { url: Url, id: String },
    /// A Rai News article, read for the video it embeds.
    Article { url: Url },
    /// A rai.it or rai.tv page, read through the catalogue's ContentItem JSON.
    Item { id: String },
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let path = url.path();
    if host == "raiplay.it" || host == "www.raiplay.it" {
        if let Some(caps) = RE_LIVE.captures(path) {
            return Some(Link::Live {
                channel: caps[1].to_string(),
            });
        }
        if let Some(caps) = RE_PROGRAMME.captures(path) {
            return Some(Link::Programme {
                slug: caps[1].to_string(),
                section: caps
                    .get(2)
                    .map(|m| m.as_str().trim_matches('/').to_string())
                    .filter(|s| !s.is_empty()),
            });
        }
        if let Some(caps) = RE_PLAY_ITEM.captures(path) {
            return Some(Link::Video {
                path: caps[1].to_string(),
                id: caps[2].to_ascii_lowercase(),
            });
        }
        return None;
    }
    if host == "rainews.it" || host == "www.rainews.it" {
        let mut page = url.clone();
        page.set_query(None);
        page.set_fragment(None);
        if path.starts_with("/articoli/") && path.ends_with(".html") {
            return Some(Link::Article { url: page });
        }
        let caps = RE_NEWS_ITEM.captures(path)?;
        return Some(Link::News {
            url: page,
            id: caps[1].to_ascii_lowercase(),
        });
    }
    if host == "rai.it"
        || host.ends_with(".rai.it")
        || host == "rai.tv"
        || host.ends_with(".rai.tv")
    {
        let caps = RE_NEWS_ITEM.captures(path)?;
        return Some(Link::Item {
            id: caps[1].to_ascii_lowercase(),
        });
    }
    None
}

/// What the relinker answers for a content id.
#[derive(Debug, Clone, PartialEq)]
pub struct Relinked {
    pub media_url: Url,
    pub live: bool,
    pub duration: Option<Duration>,
    /// Kilobits per second, when the relinker names one.
    pub bitrate: Option<u64>,
    /// The DRM system the licence link names, when there is one.
    pub drm: Option<String>,
}

fn geo_error(url: &Url) -> ResolveError {
    ResolveError::unavailable(url, format!("available only in {GEO_COUNTRY}"))
}

/// A relinker link as the pages write it, over HTTPS.
fn relinker_link(raw: &str) -> Option<Url> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }
    let mut url = Url::parse(text).ok()?;
    if url.scheme() == "http" {
        url.set_scheme("https").ok()?;
    }
    url.host_str()?.contains("rai.it").then_some(url)
}

/// The DRM system a relinker's `license_url` JSON names.
fn licence_system(text: &str) -> String {
    let systems: Vec<String> = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|v| v.as_object().map(|o| o.keys().cloned().collect()))
        .unwrap_or_default();
    let named = systems.iter().find_map(|key| {
        let key = key.to_ascii_lowercase();
        if key.contains("widevine") {
            Some("Widevine")
        } else if key.contains("playready") {
            Some("PlayReady")
        } else if key.contains("fairplay") {
            Some("FairPlay")
        } else {
            None
        }
    });
    named
        .map(str::to_string)
        .or_else(|| systems.into_iter().next())
        .unwrap_or_else(|| "licensed".to_string())
}

/// The relinker's XML answer.
pub fn parse_relinker(xml: &str, origin: &Url) -> Result<Relinked, ResolveError> {
    let doc = util::xml(xml)
        .ok_or_else(|| ResolveError::malformed(origin, "the relinker answered with no XML"))?;
    let root = doc.root_element();
    let text_of = |name: &str| util::xml_child_text(root, name);
    let media = util::xml_find_all(root, "url")
        .into_iter()
        .find(|node| node.attribute("type") == Some("content"))
        .and_then(|node| node.text())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    let geo_protected = text_of("geoprotection").as_deref() == Some("Y");
    let Some(media) = media else {
        return Err(if geo_protected {
            geo_error(origin)
        } else {
            ResolveError::unavailable(origin, "the relinker returned no media url")
        });
    };
    if media.contains(PLACEHOLDER) {
        return Err(geo_error(origin));
    }
    let media_url = Url::parse(&media)
        .map_err(|e| ResolveError::malformed(origin, format!("relinker media url: {e}")))?;
    let drm = text_of("license_url")
        .filter(|t| t != "{}")
        .map(|t| licence_system(&t));
    Ok(Relinked {
        media_url,
        live: text_of("is_live").as_deref() == Some("Y"),
        duration: text_of("duration").and_then(|d| util::parse_duration(&d)),
        bitrate: text_of("bitrate")
            .and_then(|b| b.parse::<u64>().ok())
            .filter(|b| *b > 0),
        drm,
    })
}

/// The qualities a playlist link lists between its commas, or `*` for the one MP4 of a
/// link without them.
pub fn mp4_qualities(media_url: &Url) -> Vec<String> {
    let listed: Vec<String> = RE_QUALITIES
        .captures(media_url.path())
        .and_then(|caps| caps.get(1))
        .map(|m| {
            m.as_str()
                .split(',')
                .filter(|q| !q.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    if listed.is_empty() {
        vec!["*".to_string()]
    } else {
        listed
    }
}

/// The frame of the rendition among `streams` whose bitrate is within a fifth of
/// `kbps`, else the frame the relinker's MP4 of that bitrate has, else the best
/// rendition's.
fn frame_of(kbps: Option<u64>, streams: &[Variant]) -> (Option<u32>, Option<u32>, Option<f64>) {
    if let Some(kbps) = kbps {
        let target = kbps * 1000;
        if let Some(stream) = streams.iter().find(|s| {
            s.bitrate
                .is_some_and(|b| b.abs_diff(target) < (target / 5).min(125_000))
        }) {
            return (stream.width, stream.height, stream.fps);
        }
        let rounded = if kbps > 300 { kbps / 100 * 100 } else { 250 };
        if let Some((_, w, h)) = QUALITY_SIZES.iter().find(|(q, _, _)| *q == rounded) {
            return (Some(*w), Some(*h), Some(25.0));
        }
    }
    let best = streams.iter().max_by_key(|s| s.bitrate.unwrap_or(0));
    best.map_or((None, None, None), |s| (s.width, s.height, s.fps))
}

/// The subtitle tracks a media item lists. An STL file is offered through its SRT twin.
pub fn subtitles_of(video: &Value, site: &str) -> Vec<SubtitleTrack> {
    let mut listed: Vec<(String, String, Option<String>)> = Vec::new();
    for key in ["subtitlesArray", "subtitleList"] {
        for entry in video[key].as_array().into_iter().flatten() {
            if let Some(url) = util::text(&entry["url"]) {
                listed.push((
                    url,
                    util::text(&entry["language"]).unwrap_or_else(|| "it".to_string()),
                    util::text(&entry["label"]),
                ));
            }
        }
    }
    for key in ["subtitles", "subtitlesUrl"] {
        if let Some(url) = util::text(&video[key]) {
            listed.push((url, "it".to_string(), None));
        }
    }
    let mut tracks: Vec<SubtitleTrack> = Vec::new();
    for (href, language, name) in listed {
        let Some(url) = site_url(site, &href) else {
            continue;
        };
        let (url, format) = match path_extension(&url).as_deref() {
            Some("stl") => {
                let srt = format!("{}srt", url.as_str().trim_end_matches("stl"));
                match Url::parse(&srt) {
                    Ok(srt) => (srt, SubtitleFormat::Srt),
                    Err(_) => continue,
                }
            }
            Some("srt") => (url, SubtitleFormat::Srt),
            Some("vtt") => (url, SubtitleFormat::Vtt),
            Some("ttml" | "dfxp" | "xml") => (url, SubtitleFormat::Ttml),
            _ => continue,
        };
        if tracks.iter().any(|t| t.url == url) {
            continue;
        }
        tracks.push(SubtitleTrack {
            url,
            language,
            name,
            format,
            auto: false,
            headers: Vec::new(),
        });
    }
    tracks
}

/// A civil Rai date and time in Rome: `19-11-2021` with `10:49`, `03/11/2016`, or the
/// ISO shapes the news pages write.
pub fn published(date: &str, time: &str) -> Option<Timestamp> {
    let date = date.trim();
    if date.is_empty() {
        return None;
    }
    if date.contains('T') || date.len() > 10 {
        return util::parse_timestamp(date);
    }
    let day = ["%d-%m-%Y", "%d/%m/%Y", "%Y-%m-%d"]
        .iter()
        .find_map(|format| Date::strptime(format, date).ok())?;
    let time = time.trim();
    let civil: DateTime = if time.is_empty() {
        day.to_datetime(Time::midnight())
    } else {
        let clock = ["%H:%M:%S", "%H:%M"]
            .iter()
            .find_map(|format| Time::strptime(format, time).ok())?;
        day.to_datetime(clock)
    };
    let zone = TimeZone::get(ZONE).ok()?;
    civil.to_zoned(zone).ok().map(|z| z.timestamp())
}

/// A site path joined to its site.
fn site_url(site: &str, path: &str) -> Option<Url> {
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    util::join_url(Some(&Url::parse(site).expect("valid")), path)
}

/// The first image a page's `images` names.
fn image_of(images: &Value, site: &str) -> Option<Url> {
    [
        "landscape",
        "landscape_logo",
        "square",
        "landscape43",
        "portrait",
    ]
    .iter()
    .find_map(|key| util::text(&images[key]).and_then(|path| site_url(site, &path)))
}

/// A ContentItem id without its prefix.
fn item_id(value: &Value) -> Option<String> {
    util::text(value).map(|id| id.trim_start_matches("ContentItem-").to_string())
}

/// The player data a Rai News page carries.
pub fn player_data(html: &str) -> Option<Value> {
    let caps = RE_PLAYER.captures(html)?;
    let escaped = caps.get(1).or_else(|| caps.get(2))?.as_str();
    serde_json::from_str(&util::html_unescape(escaped)).ok()
}

/// The player frame an article embeds.
pub fn embedded_frame(html: &str) -> Option<Url> {
    RE_IFRAME
        .captures(html)
        .and_then(|caps| site_url(RAINEWS, &caps[1]))
}

/// A block and set name as a season link writes them: `Stagione 2024-2025` and
/// `Puntate` is `stagione-2024-2025/puntate`.
fn section_key(block: &str, set: &str) -> String {
    format!("{}/{}", block.trim(), set.trim())
        .replace(' ', "-")
        .to_uppercase()
}

pub struct RaiplayResolver {
    http: Http,
}

impl RaiplayResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// A page's JSON twin.
    async fn json(&self, url: &Url, origin: &Url) -> Result<Value, ResolveError> {
        let fetched = fetch(&self.http, url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        if let Some(error) = status_error(fetched.status, origin) {
            return Err(error);
        }
        fetched.json(origin)
    }

    /// Asks the relinker where a content id plays from.
    async fn relink(&self, relinker: &Url, origin: &Url) -> Result<Relinked, ResolveError> {
        let asked = util::with_query(relinker, &[("output", "64")]);
        let response = self
            .http
            .get(asked)
            .platform(PLATFORM)
            .user_agent(RELINKER_UA)
            .send()
            .await?;
        if let Some(error) = status_error(response.status, origin) {
            return Err(error);
        }
        let text = response.text(MAX_PAGE).await?;
        parse_relinker(&text, origin)
    }

    /// The MP4 downloads of a video: the relinker with a quality named, one per quality
    /// its playlist link lists, each checked to be served.
    async fn mp4_variants(
        &self,
        relinker: &Url,
        media_url: &Url,
        streams: &[Variant],
    ) -> Vec<Variant> {
        let mut variants = Vec::new();
        for quality in mp4_qualities(media_url) {
            let asked = util::with_query(
                relinker,
                &[("overrideUserAgentRule", &format!("mp4-{quality}"))],
            );
            let probed = match probe_file(&self.http, &asked, PLATFORM, RELINKER_UA, &[]).await {
                Ok(probed) if matches!(probed.status.as_u16(), 200 | 206) => probed,
                Ok(probed) => {
                    tracing::debug!(url = %asked, status = %probed.status, "Rai MP4 not served");
                    continue;
                }
                Err(error) => {
                    tracing::debug!(url = %asked, "Rai MP4 not probed: {error}");
                    continue;
                }
            };
            let served = essence(probed.content_type.as_deref());
            if served.starts_with("text/") {
                continue;
            }
            let kbps = quality.parse::<u64>().ok();
            let (width, height, fps) = frame_of(kbps, streams);
            let url = if path_extension(&probed.url).as_deref() == Some("mp4") {
                probed.url
            } else {
                asked
            };
            let mut v = Variant::new(url, VariantKind::File);
            v.container = Some(Container::Mp4);
            v.video = Some(VideoCodec::H264);
            v.audio = Some(AudioCodec::Aac);
            v.width = width;
            v.height = height;
            v.fps = fps;
            v.bitrate = kbps.map(|k| k * 1000).or_else(|| {
                streams
                    .iter()
                    .max_by_key(|s| s.bitrate.unwrap_or(0))
                    .and_then(|s| s.bitrate)
            });
            v.size = probed.size;
            v.duration = streams.iter().find_map(|s| s.duration);
            v.format_id = Some(format!("mp4-{quality}"));
            v.label = height.map(|h| format!("{h}p"));
            variants.push(v);
        }
        variants
    }

    /// The variants a relinker answer plays as, and what kind of media they are.
    async fn variants_of(
        &self,
        relinker: &Url,
        relinked: &Relinked,
        origin: &Url,
    ) -> Result<(Vec<Variant>, MediaKind), ResolveError> {
        if let Some(system) = &relinked.drm {
            return Err(ResolveError::drm(origin, system.clone()));
        }
        let media_url = &relinked.media_url;
        let extension = path_extension(media_url).unwrap_or_default();
        let query = media_url.query().unwrap_or("");
        if extension == "mp3" {
            let mut v = Variant::new(media_url.clone(), VariantKind::File);
            v.container = Some(Container::Mp3);
            v.audio = Some(AudioCodec::Mp3);
            v.audio_only = true;
            v.bitrate = relinked.bitrate.map(|k| k * 1000);
            v.duration = relinked.duration;
            v.format_id = Some("mp3".to_string());
            return Ok((vec![v], MediaKind::Audio));
        }
        if extension == "m3u8" || query.contains("format=m3u8") {
            let expanded = hls::expand(&self.http, media_url, PLATFORM, BROWSER_UA, &[]).await?;
            if expanded.variants.is_empty() {
                return Err(ResolveError::unavailable(
                    origin,
                    "the playlist lists no streams",
                ));
            }
            let live = relinked.live || expanded.live;
            let mut streams = Vec::with_capacity(expanded.variants.len());
            for mut stream in expanded.variants {
                stream.live = live;
                if stream.duration.is_none() {
                    stream.duration = relinked.duration.or(expanded.duration);
                }
                stream.format_id = Some(match &stream.label {
                    Some(label) => format!("hls-{label}"),
                    None => "hls".to_string(),
                });
                streams.push(stream);
            }
            let audio_only = streams.iter().all(|s| s.audio_only);
            if !live && !audio_only {
                let downloads = self.mp4_variants(relinker, media_url, &streams).await;
                streams.extend(downloads);
            }
            let kind = if audio_only {
                MediaKind::Audio
            } else {
                MediaKind::Video
            };
            return Ok((streams, kind));
        }
        if extension == "mp4" {
            let mut v = Variant::new(media_url.clone(), VariantKind::File);
            v.container = Some(Container::Mp4);
            v.video = Some(VideoCodec::H264);
            v.audio = Some(AudioCodec::Aac);
            v.bitrate = relinked.bitrate.map(|k| k * 1000);
            v.duration = relinked.duration;
            v.live = relinked.live;
            v.format_id = Some(match relinked.bitrate {
                Some(kbps) => format!("mp4-{kbps}"),
                None => "mp4".to_string(),
            });
            return Ok((vec![v], MediaKind::Video));
        }
        Err(ResolveError::malformed(
            origin,
            format!("the relinker named a {extension:?} file"),
        ))
    }

    /// A relinker link resolved into media of a kind, with its variants.
    async fn media_from(
        &self,
        relinker: &Url,
        origin: &Url,
    ) -> Result<(Relinked, Vec<Variant>, MediaKind), ResolveError> {
        let relinked = self.relink(relinker, origin).await?;
        let (variants, kind) = self.variants_of(relinker, &relinked, origin).await?;
        Ok((relinked, variants, kind))
    }

    async fn resolve_video(
        &self,
        path: &str,
        id: &str,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let json_url = Url::parse(&format!("{RAIPLAY}{path}.json")).expect("valid");
        let media = self.json(&json_url, origin).await?;
        let rights = &media["rights_management"]["rights"]["drm"];
        if rights.as_object().is_some_and(|o| !o.is_empty()) || rights.as_bool() == Some(true) {
            return Err(ResolveError::drm(origin, "Rai"));
        }
        let video = &media["video"];
        let relinker = util::text(&video["content_url"])
            .and_then(|raw| relinker_link(&raw))
            .ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
        let (relinked, variants, kind) = self.media_from(&relinker, origin).await?;
        let program = &media["program_info"];
        let mut resolved = Resolved::of(PLATFORM, kind);
        resolved.id = Some(item_id(&media["id"]).unwrap_or_else(|| id.to_string()));
        resolved.title = util::text(&media["name"])
            .as_deref()
            .and_then(clean_title)
            .or_else(|| {
                util::text(&media["episode_title"])
                    .as_deref()
                    .and_then(clean_title)
            });
        resolved.description = util::text(&media["description"])
            .as_deref()
            .and_then(clean_title);
        resolved.uploader = util::text(&program["channel"])
            .or_else(|| util::text(&media["channel"]))
            .as_deref()
            .and_then(clean_title);
        resolved.uploader_url =
            util::text(&program["weblink"]).and_then(|path| site_url(RAIPLAY, &path));
        resolved.uploaded_at = published(
            &util::text(&media["date_published"]).unwrap_or_default(),
            &util::text(&media["time_published"]).unwrap_or_default(),
        );
        resolved.duration = util::text(&video["duration"])
            .and_then(|d| util::parse_duration(&d))
            .or(relinked.duration);
        resolved.thumbnail =
            image_of(&media["images"], RAIPLAY).or_else(|| image_of(&program["images"], RAIPLAY));
        resolved.webpage_url = Url::parse(&format!("{RAIPLAY}{path}.html")).ok();
        resolved.live = relinked.live;
        resolved.subtitles = subtitles_of(video, RAIPLAY);
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn resolve_live(&self, channel: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let json_url = Url::parse(&format!("{RAIPLAY}/dirette/{channel}.json")).expect("valid");
        let media = self.json(&json_url, origin).await?;
        let relinker = util::text(&media["video"]["content_url"])
            .and_then(|raw| relinker_link(&raw))
            .ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
        let (relinked, mut variants, kind) = self.media_from(&relinker, origin).await?;
        for variant in &mut variants {
            variant.live = true;
        }
        let page = Url::parse(&format!("{RAIPLAY}/dirette/{channel}")).ok();
        let mut resolved = Resolved::of(PLATFORM, kind);
        resolved.id = Some(item_id(&media["id"]).unwrap_or_else(|| channel.to_string()));
        resolved.title = util::text(&media["name"])
            .as_deref()
            .and_then(clean_title)
            .or_else(|| clean_title(channel));
        resolved.description = util::text(&media["description"])
            .as_deref()
            .and_then(clean_title);
        resolved.uploader = util::text(&media["channel"])
            .or_else(|| util::text(&media["editor"]))
            .as_deref()
            .and_then(clean_title);
        resolved.uploader_url = page.clone();
        resolved.uploaded_at = published(
            &util::text(&media["date_published"]).unwrap_or_default(),
            &util::text(&media["time_published"]).unwrap_or_default(),
        );
        resolved.thumbnail = util::text(&media["still_frame"])
            .and_then(|path| site_url(RAIPLAY, &path))
            .or_else(|| image_of(&media["images"], RAIPLAY));
        resolved.webpage_url = page;
        resolved.live = true;
        resolved.duration = relinked.duration;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn resolve_programme(
        &self,
        slug: &str,
        section: Option<&str>,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let json_url = Url::parse(&format!("{RAIPLAY}/programmi/{slug}.json")).expect("valid");
        let programme = self.json(&json_url, origin).await?;
        let wanted = section.map(|s| s.trim_matches('/').replace(' ', "-").to_uppercase());
        let mut title = util::text(&programme["name"])
            .as_deref()
            .and_then(clean_title);
        let mut entries: Vec<PlaylistEntry> = Vec::new();
        let mut matched = false;
        'blocks: for block in programme["blocks"].as_array().into_iter().flatten() {
            let block_name = util::text(&block["name"]).unwrap_or_default();
            for set in block["sets"].as_array().into_iter().flatten() {
                let set_name = util::text(&set["name"]).unwrap_or_default();
                if let Some(wanted) = &wanted {
                    if section_key(&block_name, &set_name) != *wanted {
                        continue;
                    }
                    matched = true;
                    if let Some(name) = clean_title(&set_name) {
                        title = Some(match &title {
                            Some(programme) => format!("{programme} - {name}"),
                            None => name,
                        });
                    }
                }
                let set_path = util::text(&set["path_id"]).or_else(|| {
                    util::text(&set["id"]).map(|id| format!("/programmi/{slug}/{id}.json"))
                });
                let Some(set_url) = set_path.and_then(|path| site_url(RAIPLAY, &path)) else {
                    continue;
                };
                let items = match self.json(&set_url, origin).await {
                    Ok(items) => items,
                    Err(error) => {
                        tracing::debug!(url = %set_url, "RaiPlay content set not read: {error}");
                        continue;
                    }
                };
                for item in items["items"].as_array().into_iter().flatten() {
                    let Some(url) = util::text(&item["weblink"])
                        .or_else(|| {
                            util::text(&item["path_id"])
                                .map(|p| format!("{}.html", p.trim_end_matches(".json")))
                        })
                        .and_then(|path| site_url(RAIPLAY, &path))
                    else {
                        continue;
                    };
                    if entries.iter().any(|e| e.url == url) {
                        continue;
                    }
                    entries.push(PlaylistEntry {
                        url,
                        title: util::text(&item["name"])
                            .as_deref()
                            .and_then(clean_title)
                            .or_else(|| {
                                util::text(&item["episode_title"])
                                    .as_deref()
                                    .and_then(clean_title)
                            }),
                        duration: util::text(&item["duration"])
                            .and_then(|d| util::parse_duration(&d)),
                    });
                    if entries.len() >= PROGRAMME_LIMIT {
                        break 'blocks;
                    }
                }
            }
        }
        if entries.is_empty() {
            return Err(if wanted.is_some() && !matched {
                ResolveError::NotFound(origin.clone())
            } else {
                ResolveError::unavailable(origin, "the programme lists no episodes")
            });
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(match section {
                Some(section) => format!("{slug}/{}", section.trim_matches('/')),
                None => slug.to_string(),
            }),
            title,
            total: Some(entries.len()),
            entries,
        }))
    }

    /// A Rai News page's HTML, read as a browser.
    async fn news_html(&self, page: &Url, origin: &Url) -> Result<String, ResolveError> {
        let fetched = fetch(
            &self.http,
            page,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(fetched.status, origin) {
            return Err(error);
        }
        Ok(fetched.text())
    }

    /// A Rai News page, article or player frame, resolved through its player data.
    async fn resolve_news(
        &self,
        page: &Url,
        id: Option<&str>,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let html = self.news_html(page, origin).await?;
        let (player, page) = match player_data(&html) {
            Some(player) => (player, page.clone()),
            None => {
                let frame =
                    embedded_frame(&html).ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
                let frame_html = self.news_html(&frame, origin).await?;
                let player = player_data(&frame_html)
                    .ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
                (player, frame)
            }
        };
        let relinker = util::text(&player["mediapolis"])
            .or_else(|| util::text(&player["content_url"]))
            .and_then(|raw| relinker_link(&raw))
            .ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
        let (relinked, mut variants, kind) = self.media_from(&relinker, origin).await?;
        let live = relinked.live || player["live"].as_bool() == Some(true);
        for variant in &mut variants {
            variant.live = live;
        }
        let track = &player["track_info"];
        let mut resolved = Resolved::of(PLATFORM, kind);
        resolved.id = id
            .map(str::to_string)
            .or_else(|| {
                RE_NEWS_ITEM
                    .captures(page.path())
                    .map(|caps| caps[1].to_ascii_lowercase())
            })
            .or_else(|| item_id(&track["id"]));
        resolved.title = util::text(&player["title"])
            .or_else(|| util::text(&track["title"]))
            .as_deref()
            .and_then(clean_title)
            .or_else(|| {
                Page::parse(&html, origin)
                    .meta("og:title")
                    .as_deref()
                    .and_then(clean_title)
            });
        resolved.description = util::text(&player["summary"])
            .as_deref()
            .and_then(clean_title);
        resolved.uploader = util::text(&track["editor"])
            .or_else(|| util::text(&track["channel"]))
            .as_deref()
            .and_then(clean_title);
        resolved.uploaded_at = util::text(&track["create_date"])
            .or_else(|| util::text(&track["date"]))
            .and_then(|d| published(&d, ""));
        resolved.duration = relinked.duration;
        resolved.thumbnail = util::text(&player["image"]).and_then(|path| site_url(RAINEWS, &path));
        resolved.webpage_url = Some(page);
        resolved.live = live;
        resolved.subtitles = subtitles_of(&player, RAINEWS);
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    /// A rai.it or rai.tv item, through the catalogue's ContentItem JSON.
    async fn resolve_item(&self, id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let json_url = Url::parse(&format!("{ITEM_JSON}{id}.html?json")).expect("valid");
        let item = self.json(&json_url, origin).await?;
        let kind = util::text(&item["type"]).unwrap_or_default();
        let (duration, variants, media) = if kind.contains("Audio") {
            let url = util::text(&item["audioUrl"])
                .and_then(|raw| Url::parse(raw.trim()).ok())
                .ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
            let format = util::text(&item["formatoAudio"]).unwrap_or_default();
            let container = Container::from_extension(&format).or_else(|| {
                path_extension(&url)
                    .as_deref()
                    .and_then(Container::from_extension)
            });
            let mut v = Variant::new(url, VariantKind::File);
            v.audio = match &container {
                Some(Container::Mp3) => Some(AudioCodec::Mp3),
                Some(Container::M4a) => Some(AudioCodec::Aac),
                Some(Container::Ogg) => Some(AudioCodec::Vorbis),
                Some(Container::Flac) => Some(AudioCodec::Flac),
                _ => None,
            };
            v.container = container;
            v.audio_only = true;
            v.format_id = Some(if format.is_empty() {
                "audio".to_string()
            } else {
                format
            });
            (None, vec![v], MediaKind::Audio)
        } else if kind.contains("Video") {
            let relinker = util::text(&item["mediaUri"])
                .or_else(|| util::text(&item["m3u8"]))
                .and_then(|raw| relinker_link(&raw))
                .ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
            let (relinked, variants, media) = self.media_from(&relinker, origin).await?;
            (relinked.duration, variants, media)
        } else {
            return Err(ResolveError::unavailable(
                origin,
                format!("the item is {kind:?}, not media"),
            ));
        };
        let mut resolved = Resolved::of(PLATFORM, media);
        resolved.id = Some(id.to_string());
        resolved.title = util::text(&item["name"])
            .or_else(|| util::text(&item["title"]))
            .as_deref()
            .and_then(clean_title);
        resolved.description = util::text(&item["desc"]).as_deref().and_then(clean_title);
        resolved.uploader = util::text(&item["author"]).as_deref().and_then(clean_title);
        resolved.uploaded_at = util::text(&item["date"]).and_then(|d| published(&d, ""));
        resolved.duration = util::text(&item["length"])
            .and_then(|l| util::parse_duration(&l))
            .or(duration);
        resolved.thumbnail = ["image_300", "image_medium", "image"]
            .iter()
            .find_map(|key| util::text(&item[key]).and_then(|path| site_url(RAI, &path)));
        resolved.webpage_url = util::text(&item["weblink"])
            .and_then(|link| Url::parse(link.trim()).ok())
            .or_else(|| Some(origin.clone()));
        resolved.live = variants.iter().any(|v| v.live);
        resolved.subtitles = subtitles_of(&item, RAI);
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }
}

#[async_trait]
impl Resolver for RaiplayResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "RaiPlay",
            hosts: &["raiplay.it", "rainews.it", "rai.it", "rai.tv"],
            features: &[
                "videos",
                "live channels",
                "programmes",
                "seasons",
                "news",
                "articles",
                "subtitles",
            ],
            formats: &["hls", "mp4", "mp3"],
            media: &[MediaKind::Video, MediaKind::Audio],
            tags: &[Tag::Video, Tag::News, Tag::Live],
            session: SessionSupport::None,
            examples: &[
                "https://www.raiplay.it/video/2014/04/Report-del-07042014-cb27157f-9dd0-4aee-b788-b1f67643a391.html",
                "https://www.raiplay.it/dirette/rainews24",
                "https://www.raiplay.it/programmi/report",
                "https://www.raiplay.it/programmi/report/stagione-2024-2025/puntate",
                "https://www.rainews.it/video/2022/12/ossi-di-seppia-dimissioni-papa-benedetto-6ed2cec3-f821-40e6-ab0b-e1da359db0e5.html",
                "https://www.rai.it/dl/RaiTV/programmi/media/ContentItem-efb17665-691c-45d5-a60c-5301333cbb0c.html",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Video { path, id } => self.resolve_video(&path, &id, url).await,
            Link::Live { channel } => self.resolve_live(&channel, url).await,
            Link::Programme { slug, section } => {
                self.resolve_programme(&slug, section.as_deref(), url).await
            }
            Link::News { url: page, id } => self.resolve_news(&page, Some(&id), url).await,
            Link::Article { url: page } => self.resolve_news(&page, None, url).await,
            Link::Item { id } => self.resolve_item(&id, url).await,
        }
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

    /// A relinker XML answer naming `media`.
    fn relinker_xml(media: &str, live: bool, duration: &str, bitrate: u32) -> String {
        format!(
            "<Mediapolis>\n\t<url type=\"content\">\n\t\t<![CDATA[{media}]]>\n\t</url>\n\t<url type=\"bumper\"></url>\n\t<ct>m3u8</ct>\n\t<bitrate>{bitrate}</bitrate>\n\t<is_live>{}</is_live>\n\t<geoprotection>N</geoprotection>\n\t<duration>{duration}</duration>\n\t<license_url>\n\t\t<![CDATA[{{}}]]>\n\t</license_url>\n</Mediapolis>",
            if live { "Y" } else { "N" }
        )
    }

    const REPORT_PATH: &str =
        "/video/2014/04/Report-del-07042014-cb27157f-9dd0-4aee-b788-b1f67643a391";
    const REPORT_RELINKER: &str = "https://mediapolisvod.rai.it/relinker/relinkerServlet.htm?cont=h3Gi3KmssSlashvT8eeqqEEqual";
    const REPORT_MASTER: &str = "https://cdnraivoddom3.msvdn.net/dom3/archive_k/Archive/raitre/report/report_puntate_2014/2440873_,800,1800,.mp4.csmil/playlist.m3u8?auth=t";

    fn report_json() -> Value {
        json!({
            "id": "ContentItem-cb27157f-9dd0-4aee-b788-b1f67643a391", "type": "RaiPlay Video Item",
            "date_published": "07-04-2014", "time_published": "21:38", "name": "Report del 07/04/2014",
            "subtitle": "St 2013/14 - Report", "toptitle": "Espresso nel caffè - 07/04/2014",
            "episode_title": "Espresso nel caffè - 07/04/2014", "channel": "Rai 3",
            "description": "Le inchieste della puntata.",
            "video": {"content_url": REPORT_RELINKER, "duration": "01:42:40",
                "subtitlesArray": [{"language": "it", "label": "Italiano", "url": "/dl/video/stl/Report 2014-04-07-20-22-19.txt.stl"}]},
            "images": {"landscape": "/dl/img/2014/04/08/1396945296702_report.jpg", "portrait": ""},
            "rights_management": {"rights": {"offline": {}, "drm": {}, "geoprotection": {}}},
            "program_info": {"name": "Report", "channel": "Rai 3", "editor": "Rai 3", "weblink": "/programmi/report",
                "images": {"landscape": "/dl/img/2026/09/14/1789400127711_2048x1152.jpg"}}
        })
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link(
                "https://www.raiplay.it/video/2014/04/Report-del-07042014-cb27157f-9dd0-4aee-b788-b1f67643a391.html"
            ),
            Some(Link::Video {
                path: REPORT_PATH.into(),
                id: "cb27157f-9dd0-4aee-b788-b1f67643a391".into()
            })
        );
        assert_eq!(
            link(
                "http://www.raiplay.it/video/2016/11/gazebotraindesi-efebe701-969c-4593-92f3-285f0d1ce750.json?"
            ),
            Some(Link::Video {
                path: "/video/2016/11/gazebotraindesi-efebe701-969c-4593-92f3-285f0d1ce750".into(),
                id: "efebe701-969c-4593-92f3-285f0d1ce750".into()
            })
        );
        assert_eq!(
            link("https://www.raiplay.it/dirette/rainews24"),
            Some(Link::Live {
                channel: "rainews24".into()
            })
        );
        assert_eq!(
            link("https://raiplay.it/programmi/report/"),
            Some(Link::Programme {
                slug: "report".into(),
                section: None
            })
        );
        assert_eq!(
            link("https://www.raiplay.it/programmi/nondirloalmiocapo/episodi/stagione-2/"),
            Some(Link::Programme {
                slug: "nondirloalmiocapo".into(),
                section: Some("episodi/stagione-2".into())
            })
        );
        assert_eq!(
            link("https://www.rainews.it/video/2022/12/ossi-di-seppia-6ed2cec3-f821-40e6-ab0b-e1da359db0e5.html?wt_mc=x"),
            Some(Link::News {
                url: Url::parse("https://www.rainews.it/video/2022/12/ossi-di-seppia-6ed2cec3-f821-40e6-ab0b-e1da359db0e5.html").unwrap(),
                id: "6ed2cec3-f821-40e6-ab0b-e1da359db0e5".into()
            })
        );
        assert!(matches!(
            link(
                "https://www.rainews.it/tgr/veneto/video/2021/06/ven-morte-24fa793f-386c-4794-a2da-127de4c97842.html"
            ),
            Some(Link::News { .. })
        ));
        assert!(matches!(
            link(
                "https://www.rainews.it/iframe/video/2022/07/euro2022-4de06a69-de75-4e32-a657-02f0885f8118.html"
            ),
            Some(Link::News { .. })
        ));
        assert!(matches!(
            link(
                "https://www.rainews.it/articoli/2022/05/referendum-991dee41-613f-4065-b750-d96440745475.html"
            ),
            Some(Link::Article { .. })
        ));
        assert_eq!(
            link(
                "https://www.rai.it/dl/RaiTV/programmi/media/ContentItem-efb17665-691c-45d5-a60c-5301333cbb0c.html"
            ),
            Some(Link::Item {
                id: "efb17665-691c-45d5-a60c-5301333cbb0c".into()
            })
        );
        assert_eq!(
            link(
                "https://www.raisport.rai.it/dl/raiSport/media/rassegna-stampa-04a9f4bd-b563-40cf-82a6-aad3529cb4a9.html"
            ),
            Some(Link::Item {
                id: "04a9f4bd-b563-40cf-82a6-aad3529cb4a9".into()
            })
        );
        assert_eq!(link("https://www.raiplay.it/"), None);
        assert_eq!(link("https://www.raiplay.it/login"), None);
        assert_eq!(link("https://www.rainews.it/"), None);
        assert_eq!(
            link("https://mediapolisvod.rai.it/relinker/relinkerServlet.htm?cont=x"),
            None
        );
        assert_eq!(link("https://www.raiplaysound.it/radio2"), None);
        assert_eq!(
            link("https://example.com/video/x-cb27157f-9dd0-4aee-b788-b1f67643a391.html"),
            None
        );
    }

    #[test]
    fn relinker_answers_are_read() {
        let origin = Url::parse("https://www.raiplay.it/video/x").unwrap();
        let relinked = parse_relinker(
            &relinker_xml(REPORT_MASTER, false, "$technical_metadata_duration", 800),
            &origin,
        )
        .unwrap();
        assert_eq!(relinked.media_url.as_str(), REPORT_MASTER);
        assert!(!relinked.live);
        assert_eq!(relinked.duration, None);
        assert_eq!(relinked.bitrate, Some(800));
        assert_eq!(relinked.drm, None);
        let live = parse_relinker(
            &relinker_xml("https://h/live/playlist.m3u8?tk=1", true, "", 0),
            &origin,
        )
        .unwrap();
        assert!(live.live);
        assert_eq!(live.bitrate, None);
        let blocked = parse_relinker(
            &relinker_xml(
                "https://download-rai-it.akamaized.net/video_no_available.mp4",
                false,
                "",
                0,
            ),
            &origin,
        )
        .unwrap_err();
        assert!(
            matches!(&blocked, ResolveError::Unavailable { reason, .. } if reason == "available only in IT"),
            "{blocked}"
        );
        let locked = relinker_xml("https://h/v.m3u8", false, "00:10:00", 0).replace(
            "<![CDATA[{}]]>",
            r#"<![CDATA[{"com.widevine.alpha":{"url":"https://l"}}]]>"#,
        );
        let locked = parse_relinker(&locked, &origin).unwrap();
        assert_eq!(locked.drm.as_deref(), Some("Widevine"));
        assert_eq!(locked.duration, Some(Duration::from_secs(600)));
        assert!(parse_relinker("<html>no</html>", &origin).is_err());
        assert_eq!(
            mp4_qualities(&Url::parse(REPORT_MASTER).unwrap()),
            vec!["800", "1800"]
        );
        assert_eq!(
            mp4_qualities(
                &Url::parse("https://h/news1/podcastcdn/rai24_vod/18764029.mp4/playlist.m3u8?a=1")
                    .unwrap()
            ),
            vec!["*"]
        );
    }

    #[test]
    fn dates_and_subtitles_are_read() {
        let at = published("19-11-2021", "10:49").unwrap();
        assert_eq!(at.to_string(), "2021-11-19T09:49:00Z");
        let day = published("03/11/2016", "").unwrap();
        assert_eq!(day.to_string(), "2016-11-02T23:00:00Z");
        assert!(published("2022-12-28T16:33:00+0000", "").is_some());
        assert_eq!(published("", "10:00"), None);
        let tracks = subtitles_of(&report_json()["video"], RAIPLAY);
        assert_eq!(tracks.len(), 1);
        assert_eq!(
            tracks[0].url.as_str(),
            "https://www.raiplay.it/dl/video/stl/Report%202014-04-07-20-22-19.txt.srt"
        );
        assert_eq!(tracks[0].format, SubtitleFormat::Srt);
        assert_eq!(tracks[0].language, "it");
        assert_eq!(tracks[0].name.as_deref(), Some("Italiano"));
        assert_eq!(
            section_key("Stagione 2024-2025", "Puntate"),
            "STAGIONE-2024-2025/PUNTATE"
        );
    }

    #[tokio::test]
    async fn videos_resolve_with_their_playlist_renditions_and_mp4_downloads() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            &format!("{RAIPLAY}{REPORT_PATH}.json"),
            200,
            "application/json",
            &report_json().to_string(),
        ));
        fixture.exchanges.push(get(
            &format!("{REPORT_RELINKER}&output=64"),
            200,
            "text/xml",
            &relinker_xml(REPORT_MASTER, false, "$technical_metadata_duration", 800),
        ));
        fixture.exchanges.push(get(
            REPORT_MASTER,
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=844514,RESOLUTION=700x394,FRAME-RATE=25.000,CODECS=\"avc1.4d401f,mp4a.40.2\"\nchunklist-f1-v1-a1.m3u8\n#EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=1843839,RESOLUTION=1024x576,FRAME-RATE=25.000,CODECS=\"avc1.4d401f,mp4a.40.2\"\nchunklist-f2-v1-a1.m3u8\n",
        ));
        for name in ["chunklist-f1-v1-a1.m3u8", "chunklist-f2-v1-a1.m3u8"] {
            fixture.exchanges.push(get(
                &format!("https://cdnraivoddom3.msvdn.net/dom3/archive_k/Archive/raitre/report/report_puntate_2014/2440873_,800,1800,.mp4.csmil/{name}?auth=t"),
                200,
                "application/vnd.apple.mpegurl",
                "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\nseg0.ts\n#EXT-X-ENDLIST\n",
            ));
        }
        for (quality, size, file) in [
            ("800", 652552660u64, "2440873_800.mp4"),
            ("1800", 1417339216u64, "2440873_1800.mp4"),
        ] {
            fixture.exchanges.push(Exchange {
                request: RecordedRequest {
                    method: "GET".into(),
                    url: format!("{REPORT_RELINKER}&overrideUserAgentRule=mp4-{quality}"),
                    headers: Vec::new(),
                    body: None,
                },
                response: RecordedResponse {
                    status: 206,
                    url: format!("https://creativemedia4-rai-it.akamaized.net/archive_k/Archive/raitre/report/report_puntate_2014/{file}"),
                    headers: vec![
                        ("content-type".into(), "video/mp4".into()),
                        ("content-range".into(), format!("bytes 0-0/{size}")),
                    ],
                    body: RecordedBody::Empty,
                    truncated: false,
                },
            });
        }
        let resolver = RaiplayResolver::new(Http::replay(fixture));
        let url = Url::parse(&format!("{RAIPLAY}{REPORT_PATH}.html")).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(
            resolved.id.as_deref(),
            Some("cb27157f-9dd0-4aee-b788-b1f67643a391")
        );
        assert_eq!(resolved.title.as_deref(), Some("Report del 07/04/2014"));
        assert_eq!(resolved.uploader.as_deref(), Some("Rai 3"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://www.raiplay.it/programmi/report"
        );
        assert_eq!(
            resolved.uploaded_at.unwrap().to_string(),
            "2014-04-07T19:38:00Z"
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(6160)));
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://www.raiplay.it/dl/img/2014/04/08/1396945296702_report.jpg"
        );
        assert_eq!(resolved.media, MediaKind::Video);
        assert!(!resolved.live);
        assert_eq!(resolved.subtitles.len(), 1);
        assert_eq!(resolved.variants.len(), 4, "two renditions and two MP4s");
        assert_eq!(resolved.variants[0].kind, VariantKind::Hls);
        assert_eq!(resolved.variants[0].height, Some(394));
        assert_eq!(resolved.variants[1].height, Some(576));
        let mp4 = &resolved.variants[2];
        assert_eq!(mp4.kind, VariantKind::File);
        assert_eq!(
            mp4.url.as_str(),
            "https://creativemedia4-rai-it.akamaized.net/archive_k/Archive/raitre/report/report_puntate_2014/2440873_800.mp4"
        );
        assert_eq!(mp4.height, Some(394));
        assert_eq!(mp4.size, Some(652552660));
        assert_eq!(mp4.bitrate, Some(800_000));
        assert_eq!(mp4.format_id.as_deref(), Some("mp4-800"));
        assert_eq!(mp4.container, Some(Container::Mp4));
        assert_eq!(resolved.variants[3].height, Some(576));
        assert_eq!(resolved.variants[3].label.as_deref(), Some("576p"));
    }

    #[tokio::test]
    async fn live_channels_geo_gates_and_drm_are_reported() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            &format!("{RAIPLAY}/dirette/rainews24.json"),
            200,
            "application/json",
            &json!({"id": "ContentItem-d784ad40-e0ae-4a69-aa76-37519d238a9c", "type": "RaiPlay Diretta Item",
                "date_published": "02-05-2009", "time_published": "14:57", "channel": "Rai News 24",
                "name": "Diretta di Rai News 24", "description": "L'informazione in tempo reale.",
                "still_frame": "/lfe_apple/RaiNews.png", "is_live": true,
                "video": {"content_url": "https://mediapolis.rai.it/relinker/relinkerServlet.htm?cont=1"}})
            .to_string(),
        ));
        fixture.exchanges.push(get(
            "https://mediapolis.rai.it/relinker/relinkerServlet.htm?cont=1&output=64",
            200,
            "text/xml",
            &relinker_xml(
                "https://h.msvdn.net/rainews1/hls/playlist_mo.m3u8?tk2=1",
                true,
                "",
                0,
            ),
        ));
        fixture.exchanges.push(get(
            "https://h.msvdn.net/rainews1/hls/playlist_mo.m3u8?tk2=1",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2536736,CODECS=\"avc1.4d401f,mp4a.40.2\",RESOLUTION=1280x720\nrainews_2400/chunklist.m3u8\n",
        ));
        fixture.exchanges.push(get(
            "https://h.msvdn.net/rainews1/hls/rainews_2400/chunklist.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\n1.ts\n",
        ));
        fixture.exchanges.push(get(
            &format!("{RAIPLAY}/dirette/rai1.json"),
            200,
            "application/json",
            &json!({"id": "ContentItem-48cc9aec-d6f0-4e53-843e-23565b24cd82", "name": "Diretta di Rai 1", "channel": "Rai 1",
                "video": {"content_url": "https://mediapolis.rai.it/relinker/relinkerServlet.htm?cont=2606803"}})
            .to_string(),
        ));
        fixture.exchanges.push(get(
            "https://mediapolis.rai.it/relinker/relinkerServlet.htm?cont=2606803&output=64",
            200,
            "text/xml",
            &relinker_xml(
                "https://download-rai-it.akamaized.net/video_no_available.mp4",
                false,
                "",
                0,
            ),
        ));
        fixture.exchanges.push(get(
            &format!("{RAIPLAY}/video/2021/06/Zoey-3ba992de-2332-41ad-9214-73e32ab209f4.json"),
            200,
            "application/json",
            &json!({"id": "ContentItem-3ba992de-2332-41ad-9214-73e32ab209f4", "name": "Zoey",
                "video": {"content_url": "https://mediapolisvod.rai.it/relinker/relinkerServlet.htm?cont=z"},
                "rights_management": {"rights": {"drm": {"widevine": {"licence": "https://l"}}}}})
            .to_string(),
        ));
        fixture.exchanges.push(get(
            &format!("{RAIPLAY}/video/2014/04/gone-00000000-0000-0000-0000-000000000000.json"),
            404,
            "text/html",
            "<html>404</html>",
        ));
        let resolver = RaiplayResolver::new(Http::replay(fixture));
        let live = resolver
            .resolve(&Url::parse("https://www.raiplay.it/dirette/rainews24").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(
            live.id.as_deref(),
            Some("d784ad40-e0ae-4a69-aa76-37519d238a9c")
        );
        assert_eq!(live.title.as_deref(), Some("Diretta di Rai News 24"));
        assert_eq!(live.uploader.as_deref(), Some("Rai News 24"));
        assert!(live.live);
        assert_eq!(
            live.thumbnail.as_ref().unwrap().as_str(),
            "https://www.raiplay.it/lfe_apple/RaiNews.png"
        );
        assert_eq!(live.variants.len(), 1);
        assert!(live.variants[0].live);
        assert_eq!(live.variants[0].height, Some(720));
        let blocked = resolver
            .resolve(&Url::parse("https://www.raiplay.it/dirette/rai1").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&blocked, ResolveError::Unavailable { reason, .. } if reason == "available only in IT"),
            "{blocked}"
        );
        let locked = resolver
            .resolve(
                &Url::parse(
                    "https://www.raiplay.it/video/2021/06/Zoey-3ba992de-2332-41ad-9214-73e32ab209f4.html",
                )
                .unwrap(),
            )
            .await
            .unwrap_err();
        assert!(matches!(locked, ResolveError::Drm { .. }), "{locked}");
        assert!(matches!(
            resolver
                .resolve(
                    &Url::parse(
                        "https://www.raiplay.it/video/2014/04/gone-00000000-0000-0000-0000-000000000000.html"
                    )
                    .unwrap()
                )
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn programmes_and_their_seasons_list_their_episodes() {
        let programme = json!({"name": "Report", "id": "Page-961db0ca", "path_id": "/programmi/report.json",
        "program_info": {"description": "Giornalismo investigativo."},
        "blocks": [
            {"name": "Puntate", "sets": [{"name": "Stagione 2025-2026", "id": "ContentSet-13a2c347", "path_id": "/programmi/report/ContentSet-13a2c347.json"}]},
            {"name": "Stagione 2024-2025", "sets": [
                {"name": "Puntate", "id": "ContentSet-4b7f0d82", "path_id": "/programmi/report/ContentSet-4b7f0d82.json"},
                {"name": "Clip", "id": "ContentSet-bfc6f684"}
            ]}
        ]});
        // A weblink built from the name alone, so a repeated episode shares its URL.
        let set = |names: &[&str]| {
            json!({"name": "x", "items": names.iter().map(|name| json!({
                "id": format!("ContentItem-{}", name.replace([' ', '/'], "-")), "name": name, "type": "RaiPlay Video Item",
                "weblink": format!("/video/2026/05/{}.html", name.replace([' ', '/'], "-")),
                "duration": "02:48:23"
            })).collect::<Vec<_>>()})
            .to_string()
        };
        let mut fixture = Fixture::new(PLATFORM, None);
        for _ in 0..2 {
            fixture.exchanges.push(get(
                &format!("{RAIPLAY}/programmi/report.json"),
                200,
                "application/json",
                &programme.to_string(),
            ));
        }
        fixture.exchanges.push(get(
            &format!("{RAIPLAY}/programmi/report/ContentSet-13a2c347.json"),
            200,
            "application/json",
            &set(&["Puntata del 31/05/2026", "Puntata del 24/05/2026"]),
        ));
        fixture.exchanges.push(get(
            &format!("{RAIPLAY}/programmi/report/ContentSet-4b7f0d82.json"),
            200,
            "application/json",
            &set(&[
                "Puntata del 25/05/2025",
                "Puntata del 18/05/2025",
                "Puntata del 25/05/2025",
            ]),
        ));
        fixture.exchanges.push(get(
            &format!("{RAIPLAY}/programmi/report/ContentSet-bfc6f684.json"),
            404,
            "text/html",
            "<html>gone</html>",
        ));
        fixture.exchanges.push(get(
            &format!("{RAIPLAY}/programmi/report/ContentSet-4b7f0d82.json"),
            200,
            "application/json",
            &set(&["Puntata del 25/05/2025", "Puntata del 18/05/2025"]),
        ));
        fixture.exchanges.push(get(
            &format!("{RAIPLAY}/programmi/nothing.json"),
            404,
            "text/html",
            "<html>gone</html>",
        ));
        let resolver = RaiplayResolver::new(Http::replay(fixture));
        let Resolution::Playlist(all) = resolver
            .resolve(&Url::parse("https://www.raiplay.it/programmi/report").unwrap())
            .await
            .unwrap()
        else {
            panic!("a programme is a playlist");
        };
        assert_eq!(all.title.as_deref(), Some("Report"));
        assert_eq!(all.id.as_deref(), Some("report"));
        assert_eq!(all.entries.len(), 4, "the repeated episode is listed once");
        assert_eq!(all.total, Some(4));
        assert_eq!(
            all.entries[0].url.as_str(),
            "https://www.raiplay.it/video/2026/05/Puntata-del-31-05-2026.html"
        );
        assert_eq!(
            all.entries[0].title.as_deref(),
            Some("Puntata del 31/05/2026")
        );
        assert_eq!(all.entries[0].duration, Some(Duration::from_secs(10103)));
        let Resolution::Playlist(season) = resolver
            .resolve(
                &Url::parse("https://www.raiplay.it/programmi/report/stagione-2024-2025/puntate")
                    .unwrap(),
            )
            .await
            .unwrap()
        else {
            panic!("a season is a playlist");
        };
        assert_eq!(season.title.as_deref(), Some("Report - Puntate"));
        assert_eq!(
            season.id.as_deref(),
            Some("report/stagione-2024-2025/puntate")
        );
        assert_eq!(season.entries.len(), 2);
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.raiplay.it/programmi/nothing").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn news_pages_articles_and_catalogue_items_resolve() {
        let player = json!({"real_type": "RaiNews Video Item", "title": "Le dimissioni di Ratzinger, da \"Ossi di seppia\"",
            "summary": "Le dimissioni viste da dietro le quinte.", "image": "/cropgd/806x453/dl/img/2022/12/28/1672226269078_GettyImages.jpeg",
            "mediapolis": "http://mediapolisvod.rai.it/relinker/relinkerServlet.htm?cont=h2Uiw", "live": false, "audio": false,
            "content_url": "http://mediapolisvod.rai.it/relinker/relinkerServlet.htm?cont=h2Uiw", "subtitles": "",
            "track_info": {"id": "ContentItem-6ed2cec3-f821-40e6-ab0b-e1da359db0e5", "editor": "rainews", "create_date": "2022-12-28", "date": "2022-12-28"}});
        let escaped = player
            .to_string()
            .replace('&', "&amp;")
            .replace('\'', "&#x27;")
            .replace('"', "&quot;");
        let page = format!(
            "<html><head><meta property=\"og:title\" content=\"Ossi di seppia\"></head><body><rainews-player data='{escaped}'></rainews-player></body></html>"
        );
        let news = "https://www.rainews.it/video/2022/12/ossi-di-seppia-dimissioni-papa-benedetto-6ed2cec3-f821-40e6-ab0b-e1da359db0e5.html";
        let frame = "https://www.rainews.it/iframe/video/2022/12/ossi-6ed2cec3-f821-40e6-ab0b-e1da359db0e5.html";
        let article = "https://www.rainews.it/articoli/2022/12/il-papa-emerito-991dee41-613f-4065-b750-d96440745475.html";
        let master = "https://cdnraivodnews1.msvdn.net/news1/podcastcdn/rai24_vod/18764029.mp4/playlist.m3u8?auth=t";
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(news, 200, "text/html", &page));
        fixture.exchanges.push(get(
            article,
            200,
            "text/html",
            "<html><body><p>Testo</p><iframe data-src=\"/iframe/video/2022/12/ossi-6ed2cec3-f821-40e6-ab0b-e1da359db0e5.html\"></iframe></body></html>",
        ));
        fixture.exchanges.push(get(frame, 200, "text/html", &page));
        for _ in 0..3 {
            fixture.exchanges.push(get(
                "https://mediapolisvod.rai.it/relinker/relinkerServlet.htm?cont=h2Uiw&output=64",
                200,
                "text/xml",
                &relinker_xml(master, false, "00:11:59", 0),
            ));
            fixture.exchanges.push(get(
                master,
                200,
                "application/vnd.apple.mpegurl",
                "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1200000,RESOLUTION=1024x576,CODECS=\"avc1.4d401f,mp4a.40.2\"\nchunklist.m3u8\n",
            ));
            fixture.exchanges.push(get(
                "https://cdnraivodnews1.msvdn.net/news1/podcastcdn/rai24_vod/18764029.mp4/chunklist.m3u8",
                200,
                "application/vnd.apple.mpegurl",
                "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n0.ts\n#EXT-X-ENDLIST\n",
            ));
            fixture.exchanges.push(get(
                "https://mediapolisvod.rai.it/relinker/relinkerServlet.htm?cont=h2Uiw&overrideUserAgentRule=mp4-*",
                403,
                "text/html",
                "",
            ));
        }
        fixture.exchanges.push(get(
            "https://www.rainews.it/articoli/2022/12/nessun-video-00000000-0000-0000-0000-000000000000.html",
            200,
            "text/html",
            "<html><body><p>Solo testo</p></body></html>",
        ));
        fixture.exchanges.push(get(
            &format!("{ITEM_JSON}efb17665-691c-45d5-a60c-5301333cbb0c.html?json"),
            200,
            "text/html",
            "\n\t\t{\n\t\t\t\"type\": \"RaiTv Media Video Item\",\n\t\t\t\"itemId\": \"ContentItem-efb17665-691c-45d5-a60c-5301333cbb0c\",\n\t\t\t\"weblink\": \"http://www.rai.tv/dl/RaiTV/programmi/media/ContentItem-efb17665-691c-45d5-a60c-5301333cbb0c.html\",\n\t\t\t\"mediaUri\": \"http://mediapolisvod.rai.it/relinker/relinkerServlet.htm?cont=5cWt\",\n\t\t\t\"image\": \"/dl/img/2016/11/105x79logo_tg1.jpg\",\n\t\t\t\"image_300\": \"/dl/img/2016/11/300x169logo_tg1.jpg\",\n\t\t\t\"length\": \"00:36:54\",\n\t\t\t\"date\": \"03/11/2016\",\n\t\t\t\"name\": \"TG1 ore 20:00 del 03/11/2016\",\n\t\t\t\"desc\": \"TG1 edizione integrale ore 20:00 del giorno 03/11/2016\",\n\t\t\t\"author\": \"\",\n\t\t\t\"subtitlesUrl\": \"\"\n\t\t}\n",
        ));
        fixture.exchanges.push(get(
            "https://mediapolisvod.rai.it/relinker/relinkerServlet.htm?cont=5cWt&output=64",
            200,
            "text/xml",
            &relinker_xml(
                "https://cdn.rai.it/tg1/6097899.mp4",
                false,
                "00:36:54",
                1200,
            ),
        ));
        fixture.exchanges.push(get(
            &format!("{ITEM_JSON}b63a4089-ac28-48cf-bca5-9f5b5bc46df5.html?json"),
            200,
            "text/html",
            "{\"type\": \"RaiTv Media Audio Item\", \"name\": \"GR1\", \"audioUrl\": \"https://cdn.rai.it/gr1.mp3\", \"formatoAudio\": \"mp3\", \"date\": \"03/11/2016\"}",
        ));
        let resolver = RaiplayResolver::new(Http::replay(fixture));
        let video = resolver
            .resolve(&Url::parse(news).unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(
            video.id.as_deref(),
            Some("6ed2cec3-f821-40e6-ab0b-e1da359db0e5")
        );
        assert_eq!(
            video.title.as_deref(),
            Some("Le dimissioni di Ratzinger, da \"Ossi di seppia\"")
        );
        assert_eq!(video.uploader.as_deref(), Some("rainews"));
        assert_eq!(video.duration, Some(Duration::from_secs(719)));
        assert_eq!(
            video.thumbnail.as_ref().unwrap().as_str(),
            "https://www.rainews.it/cropgd/806x453/dl/img/2022/12/28/1672226269078_GettyImages.jpeg"
        );
        assert_eq!(
            video.uploaded_at.unwrap().to_string(),
            "2022-12-27T23:00:00Z"
        );
        assert_eq!(
            video.variants.len(),
            1,
            "the MP4 the relinker refuses is left out"
        );
        assert_eq!(video.variants[0].height, Some(576));
        let embedded = resolver
            .resolve(&Url::parse(article).unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(
            embedded.id.as_deref(),
            Some("6ed2cec3-f821-40e6-ab0b-e1da359db0e5")
        );
        assert_eq!(embedded.webpage_url.as_ref().unwrap().as_str(), frame);
        assert_eq!(embedded.variants.len(), 1);
        let frame_only = resolver
            .resolve(&Url::parse(frame).unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(frame_only.variants.len(), 1);
        assert!(matches!(
            resolver
                .resolve(
                    &Url::parse(
                        "https://www.rainews.it/articoli/2022/12/nessun-video-00000000-0000-0000-0000-000000000000.html"
                    )
                    .unwrap()
                )
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let item = resolver
            .resolve(
                &Url::parse(
                    "https://www.rai.it/dl/RaiTV/programmi/media/ContentItem-efb17665-691c-45d5-a60c-5301333cbb0c.html",
                )
                .unwrap(),
            )
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(item.title.as_deref(), Some("TG1 ore 20:00 del 03/11/2016"));
        assert_eq!(
            item.description.as_deref(),
            Some("TG1 edizione integrale ore 20:00 del giorno 03/11/2016")
        );
        assert_eq!(item.duration, Some(Duration::from_secs(2214)));
        assert_eq!(
            item.thumbnail.as_ref().unwrap().as_str(),
            "https://www.rai.it/dl/img/2016/11/300x169logo_tg1.jpg"
        );
        assert_eq!(
            item.uploaded_at.unwrap().to_string(),
            "2016-11-02T23:00:00Z"
        );
        assert_eq!(item.variants.len(), 1);
        assert_eq!(item.variants[0].kind, VariantKind::File);
        assert_eq!(item.variants[0].bitrate, Some(1_200_000));
        assert_eq!(item.variants[0].format_id.as_deref(), Some("mp4-1200"));
        let audio = resolver
            .resolve(
                &Url::parse(
                    "https://www.rai.it/dl/RaiTV/programmi/media/ContentItem-b63a4089-ac28-48cf-bca5-9f5b5bc46df5.html",
                )
                .unwrap(),
            )
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(audio.media, MediaKind::Audio);
        assert_eq!(audio.title.as_deref(), Some("GR1"));
        assert_eq!(audio.variants[0].container, Some(Container::Mp3));
        assert_eq!(audio.variants[0].audio, Some(AudioCodec::Mp3));
        assert!(audio.variants[0].audio_only);
    }

    /// Every example link resolves live: the videos and the live channel to playable
    /// streams, the programme and its season to episodes. A live channel Rai keeps for
    /// Italy answers with its stream from Italy and with the geo gate from elsewhere.
    #[tokio::test]

    async fn live_examples_resolve() {
        let resolver = RaiplayResolver::new(Http::new(crate::http::HttpConfig::default()));
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
            assert!(resolver.matches(&url), "{link}");
            let resolution = tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&url))
                .await
                .expect("resolution timed out")
                .unwrap_or_else(|e| panic!("{link}: {e}"));
            match resolution {
                Resolution::Media(resolved) => {
                    let playable = resolved.variants.iter().filter(|v| v.is_playable()).count();
                    assert!(playable > 0, "{link}: no playable variant");
                    println!(
                        "{link}: {:?} with {playable} variants, live {}, title {:?}",
                        resolved.media, resolved.live, resolved.title
                    );
                }
                Resolution::Playlist(playlist) => {
                    assert!(!playlist.entries.is_empty(), "{link}: no entries");
                    println!(
                        "{link}: playlist of {} entries, title {:?}",
                        playlist.entries.len(),
                        playlist.title
                    );
                }
            }
        }
        let rai1 = Url::parse("https://www.raiplay.it/dirette/rai1").unwrap();
        match resolver.resolve(&rai1).await {
            Ok(Resolution::Media(resolved)) => {
                assert!(resolved.live);
                assert!(!resolved.variants.is_empty());
                println!("{rai1}: {} live variants", resolved.variants.len());
            }
            Ok(Resolution::Playlist(_)) => panic!("{rai1}: a live channel is not a playlist"),
            Err(ResolveError::Unavailable { reason, .. }) if reason == "available only in IT" => {
                println!("{rai1}: geo gate reported");
            }
            Err(error) => panic!("{rai1}: {error}"),
        }
    }
}
