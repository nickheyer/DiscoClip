//! Internet Archive items, through the metadata API that lists an item's files and the
//! embeddable player that groups them: the video and audio files of an item, each
//! original with its derivatives as variants and the player's subtitle tracks, and an
//! item holding several recordings as a playlist of them. A link naming one file of the
//! item resolves that recording alone. Files marked private are listed only with a
//! logged-in session.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::TimeZone;
use percent_encoding::{AsciiSet, CONTROLS, utf8_percent_encode};
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, SubtitleFormat, SubtitleTrack, Variant, VariantKind, clean_title, fetch,
    parse_time_stamp, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "archive_org";
const SITE: &str = "https://archive.org/";
const METADATA_API: &str = "https://archive.org/metadata/";
/// Characters left as they are in a file name within a download URL.
const PATH: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'[')
    .add(b']')
    .add(b'`')
    .add(b'{')
    .add(b'}');
const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "webm", "mkv", "mov", "avi", "ogv", "mpg", "mpeg", "ts", "m2ts", "flv", "wmv",
    "3gp", "3g2", "gif", "mts", "divx", "asf", "f4v", "mk3d", "ogx",
];
const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "m4a", "m4b", "m4r", "aac", "flac", "ogg", "oga", "opus", "spx", "wav", "aiff", "alac",
    "ape", "wma", "mka", "weba", "f4a", "f4b",
];

/// An item, and one of its files when the link names one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub item: String,
    pub file: Option<String>,
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "archive.org" && host != "www.archive.org" {
        return None;
    }
    let segments: Vec<String> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .map(|s| {
            // Links write spaces in file names as `+` as often as `%20`.
            percent_encoding::percent_decode_str(&s.replace('+', "%20"))
                .decode_utf8_lossy()
                .into_owned()
        })
        .collect();
    let (kind, rest) = segments.split_first()?;
    if !matches!(kind.as_str(), "details" | "download" | "embed" | "stream") {
        return None;
    }
    let (item, path) = rest.split_first()?;
    if item.is_empty()
        || !item
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    {
        return None;
    }
    let file = (!path.is_empty()).then(|| path.join("/"));
    Some(Link {
        item: item.clone(),
        file,
    })
}

fn extension_of(name: &str) -> Option<String> {
    name.rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .filter(|ext| !ext.is_empty() && ext.len() <= 4)
}

pub fn is_video_name(name: &str) -> bool {
    if name.contains(".thumbs/") {
        return false;
    }
    extension_of(name).is_some_and(|ext| VIDEO_EXTENSIONS.contains(&ext.as_str()))
}

pub fn is_audio_name(name: &str) -> bool {
    extension_of(name).is_some_and(|ext| AUDIO_EXTENSIONS.contains(&ext.as_str()))
}

/// Whether a file is one that plays: a video or an audio file, not a thumbnail strip.
pub fn is_media_name(name: &str) -> bool {
    is_video_name(name) || is_audio_name(name)
}

/// One file of the item as the API lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct ItemFile {
    pub name: String,
    pub format: String,
    pub source: String,
    pub original: Option<String>,
    pub size: Option<u64>,
    pub length: Option<Duration>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// The file is served only to accounts allowed it.
    pub private: bool,
    pub title: Option<String>,
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Array(items) => items.iter().find_map(text),
        _ => None,
    }
}

fn number<T: std::str::FromStr>(value: &Value) -> Option<T> {
    text(value).and_then(|t| t.trim().parse().ok())
}

pub fn files_of(metadata: &Value) -> Vec<ItemFile> {
    metadata["files"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|file| {
            let name = file["name"].as_str()?.to_string();
            Some(ItemFile {
                format: file["format"].as_str().unwrap_or_default().to_string(),
                source: file["source"].as_str().unwrap_or_default().to_string(),
                original: file["original"].as_str().map(String::from),
                size: number(&file["size"]),
                length: text(&file["length"]).and_then(|l| parse_time_stamp(&l)),
                width: number(&file["width"]).filter(|w: &u32| *w > 0),
                height: number(&file["height"]).filter(|h: &u32| *h > 0),
                private: matches!(&file["private"], Value::Bool(true))
                    || text(&file["private"]).is_some_and(|p| p == "true"),
                title: text(&file["title"]).and_then(|t| clean_title(&t)),
                name,
            })
        })
        .collect()
}

/// The file every derivative of `name` descends from, within `files`.
fn root_of(name: &str, files: &[ItemFile]) -> String {
    let mut current = name.to_string();
    for _ in 0..8 {
        let Some(file) = files.iter().find(|f| f.name == current) else {
            break;
        };
        match &file.original {
            Some(original)
                if original != &current
                    && is_media_name(original)
                    && files.iter().any(|f| &f.name == original) =>
            {
                current = original.clone();
            }
            _ => break,
        }
    }
    current
}

/// One recording of an item: the original file's name, what the player says about it,
/// and the files that play it, the original and its derivatives.
#[derive(Debug, Clone, PartialEq)]
pub struct Recording {
    pub root: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub image: Option<Url>,
    pub duration: Option<Duration>,
    pub subtitles: Vec<SubtitleTrack>,
    pub files: Vec<ItemFile>,
}

/// An entry of the embeddable player's playlist: the recording it plays, by its
/// original file, with the player's title, artist, poster and subtitle tracks.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerEntry {
    pub orig: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub image: Option<Url>,
    pub duration: Option<Duration>,
    pub subtitles: Vec<SubtitleTrack>,
}

/// The playlist the embeddable player at `/embed/{item}` renders: the `playlist`
/// attribute of its `<play-av>` element.
pub fn player_playlist(html: &str) -> Vec<PlayerEntry> {
    let Some(start) = html.find("<play-av") else {
        return Vec::new();
    };
    let tag = &html[start..];
    let end = tag.find('>').unwrap_or(tag.len());
    let attributes = util::extract_attributes(&tag[..end + 1]);
    let Some(playlist) = attributes
        .iter()
        .find(|(name, _)| name == "playlist")
        .map(|(_, value)| util::html_unescape(value))
    else {
        return Vec::new();
    };
    let Ok(Value::Array(entries)) = serde_json::from_str::<Value>(&playlist) else {
        return Vec::new();
    };
    let site = Url::parse(SITE).expect("valid");
    entries
        .iter()
        .filter_map(|entry| {
            let orig = entry["orig"].as_str()?.to_string();
            let subtitles = entry["tracks"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|track| track["kind"].as_str() == Some("subtitles"))
                .filter_map(|track| {
                    let file = track["file"].as_str()?;
                    let url = site.join(file.trim_start_matches('/')).ok()?;
                    let label = track["label"].as_str().unwrap_or("").trim().to_string();
                    let language = util::language_code(&label).unwrap_or_else(|| "und".to_string());
                    Some(SubtitleTrack {
                        format: if url.path().ends_with(".srt") {
                            SubtitleFormat::Srt
                        } else {
                            SubtitleFormat::Vtt
                        },
                        url,
                        language,
                        name: (!label.is_empty()).then_some(label),
                        auto: false,
                        headers: Vec::new(),
                    })
                })
                .collect();
            Some(PlayerEntry {
                orig,
                title: entry["title"].as_str().and_then(clean_title),
                artist: entry["artist"].as_str().and_then(clean_title),
                image: entry["image"]
                    .as_str()
                    .and_then(|image| site.join(image.trim_start_matches('/')).ok()),
                duration: text(&entry["duration"])
                    .and_then(|d| d.parse::<f64>().ok())
                    .filter(|d| *d > 0.0)
                    .map(Duration::from_secs_f64),
                subtitles,
            })
        })
        .collect()
}

/// The item's recordings: the player's playlist entries, each with the files that are
/// its original or derive from it; without a player playlist, every original media file
/// with its derivatives, as the metadata chains them. Private files are left out unless
/// `logged_in`.
pub fn recordings_of(files: &[ItemFile], playlist: &[PlayerEntry], logged_in: bool) -> Vec<Recording> {
    let playable = |file: &ItemFile| is_media_name(&file.name) && (!file.private || logged_in);
    if !playlist.is_empty() {
        return playlist
            .iter()
            .map(|entry| Recording {
                root: entry.orig.clone(),
                title: entry.title.clone(),
                artist: entry.artist.clone(),
                image: entry.image.clone(),
                duration: entry.duration,
                subtitles: entry.subtitles.clone(),
                files: files
                    .iter()
                    .filter(|file| {
                        file.name == entry.orig || file.original.as_deref() == Some(&entry.orig)
                    })
                    .filter(|file| playable(file))
                    .cloned()
                    .collect(),
            })
            .collect();
    }
    let mut groups: BTreeMap<String, Vec<ItemFile>> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for file in files.iter().filter(|file| playable(file)) {
        let root = root_of(&file.name, files);
        if !groups.contains_key(&root) {
            order.push(root.clone());
        }
        groups.entry(root).or_default().push(file.clone());
    }
    order
        .into_iter()
        .filter_map(|root| {
            groups.remove(&root).map(|files| Recording {
                title: files.iter().find(|f| f.name == root).and_then(|f| f.title.clone()),
                artist: None,
                image: None,
                duration: files.iter().filter_map(|f| f.length).max(),
                subtitles: Vec::new(),
                files,
                root,
            })
        })
        .collect()
}

fn download_url(item: &str, name: &str) -> Url {
    Url::parse(&format!(
        "{SITE}download/{item}/{}",
        utf8_percent_encode(name, PATH)
    ))
    .expect("item and file names are URL safe once encoded")
}

fn stem(name: &str) -> String {
    let base = name.rsplit('/').next().unwrap_or(name);
    base.rsplit_once('.').map_or(base, |(s, _)| s).to_string()
}

fn codecs_for(format: &str, ext: &str) -> (Option<VideoCodec>, Option<AudioCodec>) {
    let format = format.to_ascii_lowercase();
    let video = if format.contains("h.264") || format.contains("h264") {
        Some(VideoCodec::H264)
    } else if format.contains("h.265") || format.contains("hevc") {
        Some(VideoCodec::H265)
    } else if format.contains("ogg video") || ext == "ogv" {
        Some(VideoCodec::Other("theora".into()))
    } else if format.contains("mpeg4") {
        Some(VideoCodec::Other("mpeg4".into()))
    } else if format.contains("mpeg2") {
        Some(VideoCodec::Other("mpeg2".into()))
    } else if format.contains("cinepack") || format.contains("cinepak") {
        Some(VideoCodec::Other("cinepak".into()))
    } else if format.contains("windows media") || ext == "wmv" {
        Some(VideoCodec::Other("wmv".into()))
    } else {
        None
    };
    let audio = match video {
        Some(VideoCodec::H264) | Some(VideoCodec::H265) => Some(AudioCodec::Aac),
        Some(VideoCodec::Other(ref name)) if name == "theora" => Some(AudioCodec::Vorbis),
        _ => None,
    };
    (video, audio)
}

/// The audio codec an audio file's extension names.
fn audio_codec_for(ext: &str) -> AudioCodec {
    match ext {
        "mp3" => AudioCodec::Mp3,
        "m4a" | "m4b" | "m4r" | "aac" | "f4a" | "f4b" => AudioCodec::Aac,
        "ogg" | "oga" | "spx" => AudioCodec::Vorbis,
        "opus" | "weba" => AudioCodec::Opus,
        other => AudioCodec::Other(other.to_string()),
    }
}

pub fn variant_of(item: &str, file: &ItemFile) -> Variant {
    let ext = extension_of(&file.name).unwrap_or_default();
    let mut v = Variant::new(download_url(item, &file.name), VariantKind::File);
    let audio_only = is_audio_name(&file.name);
    v.container = Container::from_extension(&ext).or_else(|| Some(Container::Other(ext.clone())));
    let (video, audio) = codecs_for(&file.format, &ext);
    v.video = if audio_only { None } else { video };
    v.audio = if audio_only { Some(audio_codec_for(&ext)) } else { audio };
    v.audio_only = audio_only;
    v.width = file.width;
    v.height = file.height;
    v.size = file.size;
    v.duration = file.length;
    if let (Some(size), Some(length)) = (file.size, file.length)
        && length.as_secs_f64() > 0.0
    {
        v.bitrate = Some((size as f64 * 8.0 / length.as_secs_f64()) as u64);
    }
    v.format_id = Some(file.format.clone());
    v.label = Some(match (file.source.as_str(), file.height) {
        ("original", _) => "original".to_string(),
        (_, Some(h)) if !audio_only => format!("{h}p {}", file.format),
        _ => file.format.clone(),
    });
    v
}

fn parse_date(text: &str) -> Option<Timestamp> {
    let text = text.trim();
    if let Ok(ts) = text.parse::<Timestamp>() {
        return Some(ts);
    }
    DateTime::strptime("%Y-%m-%d %H:%M:%S", text)
        .ok()
        .and_then(|dt| dt.to_zoned(TimeZone::UTC).ok())
        .map(|z| z.timestamp())
}

pub struct ArchiveOrgResolver {
    http: Http,
}

impl ArchiveOrgResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    async fn metadata(&self, item: &str, origin: &Url) -> Result<Value, ResolveError> {
        let api = Url::parse(&format!("{METADATA_API}{item}")).expect("valid");
        let fetched = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        match fetched.status.as_u16() {
            200..=299 => {}
            404 | 410 => return Err(ResolveError::NotFound(origin.clone())),
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the metadata API answered HTTP {status}"),
                ));
            }
        }
        let value = fetched.json(origin)?;
        if value.as_object().is_none_or(|o| o.is_empty()) {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        if value["is_dark"].as_bool() == Some(true) {
            return Err(ResolveError::unavailable(
                origin,
                "the item has been made unavailable",
            ));
        }
        Ok(value)
    }

    /// The embeddable player's playlist for `item`: how the site groups the item's files
    /// into recordings, with their subtitle tracks. An item the player does not render
    /// (one without playable files) has none.
    async fn player(&self, item: &str, origin: &Url) -> Result<Vec<PlayerEntry>, ResolveError> {
        let embed = Url::parse(&format!("{SITE}embed/{item}")).expect("valid");
        let fetched = fetch(&self.http, &embed, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        match fetched.status.as_u16() {
            200..=299 => Ok(player_playlist(&fetched.text())),
            404 | 410 => Ok(Vec::new()),
            429 => Err(ResolveError::RateLimited(origin.clone())),
            status => Err(ResolveError::unavailable(
                origin,
                format!("the player page answered HTTP {status}"),
            )),
        }
    }

    /// Whether the stored cookies log in to the site.
    fn logged_in(&self) -> bool {
        self.http.jar(PLATFORM).get("logged-in-sig").is_some()
    }
}

/// A description, which the API gives as text or as a list of paragraphs.
fn joined_text(value: &Value) -> Option<String> {
    match value {
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().filter_map(text).collect();
            (!parts.is_empty()).then(|| parts.join(" "))
        }
        other => text(other),
    }
}

/// One recording of an item as media: named by the player (or the item, for an item of
/// one recording), with every file that plays it and its subtitle tracks.
fn resolved_of(item: &str, metadata: &Value, recording: &Recording, alone: bool) -> Resolved {
    let meta = &metadata["metadata"];
    let mut resolved = Resolved::new(PLATFORM);
    resolved.id = Some(if alone {
        item.to_string()
    } else {
        format!("{item}/{}", recording.root)
    });
    let item_title = text(&meta["title"]).and_then(|t| clean_title(&t));
    resolved.title = if alone {
        item_title.or_else(|| recording.title.clone())
    } else {
        recording.title.clone().or_else(|| Some(stem(&recording.root)))
    };
    resolved.description = joined_text(&meta["description"])
        .map(|d| util::clean_html(&d))
        .and_then(|d| clean_title(&d));
    resolved.uploader = recording
        .artist
        .clone()
        .or_else(|| text(&meta["creator"]))
        .or_else(|| text(&meta["uploader"]))
        .or_else(|| text(&meta["adder"]))
        .and_then(|u| clean_title(&u));
    resolved.uploaded_at = text(&meta["publicdate"])
        .or_else(|| text(&meta["addeddate"]))
        .and_then(|d| parse_date(&d));
    resolved.duration = recording
        .files
        .iter()
        .filter_map(|f| f.length)
        .max()
        .or(recording.duration);
    resolved.thumbnail = recording
        .image
        .clone()
        .or_else(|| Url::parse(&format!("{SITE}services/img/{item}")).ok());
    resolved.webpage_url = Url::parse(&format!("{SITE}details/{item}")).ok();
    resolved.subtitles = recording.subtitles.clone();
    resolved.variants = recording.files.iter().map(|f| variant_of(item, f)).collect();
    resolved
}

#[async_trait]
impl Resolver for ArchiveOrgResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Internet Archive",
            hosts: &["archive.org"],
            features: &["items", "files", "embeds", "multi-recording items", "audio", "subtitles"],
            formats: &["mp4", "webm", "ogv", "avi", "mkv", "mov", "mp3", "ogg", "flac"],
            session: SessionSupport::Optional,
            examples: &[
                "https://archive.org/details/BigBuckBunny_124",
                "https://archive.org/download/BigBuckBunny_124/Content/big_buck_bunny_720p_surround.mp4",
                "https://archive.org/details/Greatest_Speeches_of_the_20th_Century",
                "https://archive.org/details/Greatest_Speeches_of_the_20th_Century/AbdicationAddress.mp3",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let metadata = self.metadata(&link.item, url).await?;
        let files = files_of(&metadata);
        let playlist = self.player(&link.item, url).await?;
        let recordings = recordings_of(&files, &playlist, self.logged_in());
        if let Some(wanted) = &link.file {
            let recording = recordings
                .iter()
                .find(|r| r.root == *wanted || r.files.iter().any(|f| &f.name == wanted))
                .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
            let mut resolved = resolved_of(&link.item, &metadata, recording, recordings.len() == 1);
            resolved.webpage_url = Url::parse(&format!(
                "{SITE}details/{}/{}",
                link.item,
                utf8_percent_encode(&recording.root, PATH)
            ))
            .ok();
            return Ok(Resolution::from(resolved));
        }
        match recordings.len() {
            0 => Err(ResolveError::unavailable(
                url,
                "the item holds no video or audio files",
            )),
            1 => Ok(Resolution::from(resolved_of(
                &link.item,
                &metadata,
                &recordings[0],
                true,
            ))),
            _ => {
                let entries = recordings
                    .iter()
                    .map(|recording| PlaylistEntry {
                        url: Url::parse(&format!(
                            "{SITE}details/{}/{}",
                            link.item,
                            utf8_percent_encode(&recording.root, PATH)
                        ))
                        .expect("valid"),
                        title: recording
                            .title
                            .clone()
                            .or_else(|| Some(stem(&recording.root))),
                        duration: recording
                            .files
                            .iter()
                            .filter_map(|f| f.length)
                            .max()
                            .or(recording.duration),
                    })
                    .collect::<Vec<_>>();
                Ok(Resolution::Playlist(Playlist {
                    resolver: PLATFORM.into(),
                    id: Some(link.item.clone()),
                    title: text(&metadata["metadata"]["title"]).and_then(|t| clean_title(&t)),
                    total: Some(entries.len()),
                    entries,
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

    fn get(url: &str, status: u16, body: &str) -> Exchange {
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
                headers: vec![("content-type".into(), "application/json".into())],
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    const ITEM: &str = r#"{"created":1789286000,"d1":"dn801201.us.archive.org","dir":"/0/items/BigBuckBunny_124",
      "files":[
        {"name":"BigBuckBunny_124.thumbs/Content/big_buck_bunny_720p_surround_000001.jpg","source":"derivative","format":"Thumbnail","size":"10377","original":"Content/big_buck_bunny_720p_surround.avi"},
        {"name":"BigBuckBunny_124_meta.xml","source":"original","format":"Metadata","size":"1484"},
        {"name":"Content/big_buck_bunny_720p_surround.avi","source":"derivative","format":"Cinepack","size":"332243668","length":"596.46","width":"1280","height":"720","original":"blender_foundation_-_big_buck_bunny_720p.torrent"},
        {"name":"Content/big_buck_bunny_720p_surround.gif","source":"derivative","format":"Animated GIF","size":"276424","original":"Content/big_buck_bunny_720p_surround.avi"},
        {"name":"Content/big_buck_bunny_720p_surround.mp4","source":"derivative","format":"h.264","size":"61878609","length":"596.5","width":"640","height":"360","original":"Content/big_buck_bunny_720p_surround.avi"},
        {"name":"Content/big_buck_bunny_720p_surround.ogv","source":"derivative","format":"Ogg Video","size":"46935223","length":"596.48","width":"533","height":"300","original":"Content/big_buck_bunny_720p_surround.avi"},
        {"name":"blender_foundation_-_big_buck_bunny_720p.torrent","source":"original","format":"BitTorrent","size":"51283"}
      ],
      "metadata":{"identifier":"BigBuckBunny_124","title":"Big Buck Bunny","description":"Big Buck Bunny is a comedy about a well-tempered rabbit.","mediatype":"movies","uploader":"jake@archive.org","publicdate":"2011-07-01 21:10:23","addeddate":"2011-07-01 21:03:22"},
      "server":"dn801201.us.archive.org"}"#;

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://archive.org/details/BigBuckBunny_124"),
            Some(Link {
                item: "BigBuckBunny_124".into(),
                file: None
            })
        );
        assert_eq!(
            link("https://archive.org/download/BigBuckBunny_124/Content/big_buck_bunny_720p_surround.mp4"),
            Some(Link {
                item: "BigBuckBunny_124".into(),
                file: Some("Content/big_buck_bunny_720p_surround.mp4".into())
            })
        );
        assert_eq!(
            link("https://archive.org/details/x/My%20File.mp4").unwrap().file,
            Some("My File.mp4".into())
        );
        assert_eq!(link("https://archive.org/search?query=bunny"), None);
        assert_eq!(link("https://archive.org/details/"), None);
    }

    #[tokio::test]
    async fn an_item_with_one_video_resolves_with_every_derivative() {
        let mut fixture = Fixture::new("archive_org", None);
        fixture
            .exchanges
            .push(get("https://archive.org/metadata/BigBuckBunny_124", 200, ITEM));
        fixture.exchanges.push(get("https://archive.org/embed/BigBuckBunny_124", 404, "<html>not found</html>"));
        let resolver = ArchiveOrgResolver::new(Http::replay(fixture));
        let url = Url::parse("https://archive.org/details/BigBuckBunny_124").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.title.as_deref(), Some("Big Buck Bunny"));
        assert_eq!(resolved.uploader.as_deref(), Some("jake@archive.org"));
        assert_eq!(
            resolved.uploaded_at.unwrap().to_string(),
            "2011-07-01T21:10:23Z"
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(596.5)));
        assert_eq!(resolved.variants.len(), 4);
        let mp4 = resolved
            .variants
            .iter()
            .find(|v| v.container == Some(Container::Mp4))
            .unwrap();
        assert_eq!(
            mp4.url.as_str(),
            "https://archive.org/download/BigBuckBunny_124/Content/big_buck_bunny_720p_surround.mp4"
        );
        assert_eq!(mp4.video, Some(VideoCodec::H264));
        assert_eq!(mp4.height, Some(360));
        assert_eq!(mp4.size, Some(61878609));
        assert!(mp4.bitrate.unwrap() > 800_000);
        let ogv = resolved
            .variants
            .iter()
            .find(|v| v.url.as_str().ends_with(".ogv"))
            .unwrap();
        assert_eq!(ogv.video, Some(VideoCodec::Other("theora".into())));
        let avi = resolved
            .variants
            .iter()
            .find(|v| v.url.as_str().ends_with(".avi"))
            .unwrap();
        assert_eq!(avi.height, Some(720));
        assert!(
            resolved
                .thumbnail
                .as_ref()
                .unwrap()
                .as_str()
                .ends_with("/services/img/BigBuckBunny_124")
        );
        // A link to one file of the item yields the same video.
        let file = Url::parse(
            "https://archive.org/download/BigBuckBunny_124/Content/big_buck_bunny_720p_surround.ogv",
        )
        .unwrap();
        let resolved = resolver.resolve(&file).await.unwrap().media().unwrap();
        assert_eq!(resolved.variants.len(), 4);
        assert!(
            resolved
                .webpage_url
                .unwrap()
                .as_str()
                .ends_with("/details/BigBuckBunny_124/Content/big_buck_bunny_720p_surround.avi")
        );
    }

    #[tokio::test]
    async fn items_with_several_videos_are_playlists_and_missing_items_say_so() {
        let two = r#"{"files":[
            {"name":"a.mp4","source":"original","format":"MPEG4","size":"10","length":"5"},
            {"name":"a.ogv","source":"derivative","format":"Ogg Video","size":"9","length":"5","original":"a.mp4"},
            {"name":"b.mp4","source":"original","format":"MPEG4","size":"20","length":"8"},
            {"name":"notes.txt","source":"original","format":"Text","size":"1"}
          ],"metadata":{"identifier":"two","title":"Two videos"}}"#;
        let mut fixture = Fixture::new("archive_org", None);
        fixture
            .exchanges
            .push(get("https://archive.org/metadata/two", 200, two));
        fixture.exchanges.push(get("https://archive.org/embed/two", 404, "<html>not found</html>"));
        fixture
            .exchanges
            .push(get("https://archive.org/metadata/nothing", 200, "{}"));
        fixture.exchanges.push(get("https://archive.org/embed/nothing", 404, "<html>not found</html>"));
        fixture.exchanges.push(get("https://archive.org/embed/text", 404, "<html>not found</html>"));
        fixture.exchanges.push(get(
            "https://archive.org/metadata/text",
            200,
            r#"{"files":[{"name":"notes.txt","format":"Text"}],"metadata":{"identifier":"text","title":"Text"}}"#,
        ));
        let resolver = ArchiveOrgResolver::new(Http::replay(fixture));
        let playlist = match resolver
            .resolve(&Url::parse("https://archive.org/details/two").unwrap())
            .await
            .unwrap()
        {
            Resolution::Playlist(p) => p,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(playlist.title.as_deref(), Some("Two videos"));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://archive.org/details/two/a.mp4"
        );
        assert_eq!(playlist.entries[0].title.as_deref(), Some("a"));
        assert_eq!(playlist.entries[1].duration, Some(Duration::from_secs(8)));
        // One entry resolves to its own group.
        let resolved = resolver
            .resolve(&playlist.entries[0].url)
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.title.as_deref(), Some("a"));
        assert_eq!(resolved.variants.len(), 2);
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://archive.org/details/nothing").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(&Url::parse("https://archive.org/details/text").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("no video")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn audio_items_follow_the_players_playlist_with_its_subtitles() {
        let metadata = serde_json::json!({
            "metadata": {"identifier": "speeches", "title": "Speeches", "creator": ["A", "B"], "description": ["First.", "Second."], "publicdate": "2010-01-02 03:04:05"},
            "files": [
                {"name": "One.mp3", "source": "original", "format": "VBR MP3", "length": "402.29", "size": "6435000", "title": "Number one", "width": "0", "height": "0"},
                {"name": "One_64kb.mp3", "source": "derivative", "original": "One.mp3", "format": "64Kbps MP3", "length": "402.29", "size": "3218000"},
                {"name": "One.ogg", "source": "derivative", "original": "One.mp3", "format": "Ogg Vorbis", "length": "402.29", "size": "3230000"},
                {"name": "Two.flac", "source": "original", "format": "Flac", "length": "31.74", "size": "9000000", "private": true},
                {"name": "Two.mp3", "source": "derivative", "original": "Two.flac", "format": "VBR MP3", "length": "31.74", "size": "500000"},
                {"name": "One.png", "source": "derivative", "original": "One.mp3", "format": "PNG"},
                {"name": "speeches_meta.xml", "source": "original", "format": "Metadata"}
            ]
        }).to_string();
        let player = r#"<html><play-av args='{"identifier":"speeches"}' playlist='[{"title":"Number one","orig":"One.mp3","artist":"King Edward VIII","image":"/download/speeches/One.png","duration":"402.29","sources":[{"file":"/download/speeches/One_64kb.mp3","type":"mp3"}],"tracks":[{"kind":"subtitles","file":"/download/speeches/One.vtt","label":"English"},{"kind":"thumbnails","file":"/stream/speeches/One.thumbs"}]},{"title":"Number two","orig":"Two.flac","artist":"J. Pershing","duration":"31.74","sources":[{"file":"/download/speeches/Two.mp3","type":"mp3"}]}]'></play-av></html>"#;
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture
            .exchanges
            .push(get("https://archive.org/metadata/speeches", 200, &metadata));
        fixture
            .exchanges
            .push(get("https://archive.org/embed/speeches", 200, player));
        let resolver = ArchiveOrgResolver::new(Http::replay(fixture));
        let Resolution::Playlist(playlist) = resolver
            .resolve(&Url::parse("https://archive.org/details/speeches").unwrap())
            .await
            .unwrap()
        else {
            panic!("two recordings are a playlist");
        };
        assert_eq!(playlist.title.as_deref(), Some("Speeches"));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(playlist.entries[0].title.as_deref(), Some("Number one"));
        assert_eq!(playlist.entries[1].title.as_deref(), Some("Number two"));
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://archive.org/details/speeches/One.mp3"
        );

        let one = resolver
            .resolve(&Url::parse("https://archive.org/details/speeches/One.mp3").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(one.id.as_deref(), Some("speeches/One.mp3"));
        assert_eq!(one.title.as_deref(), Some("Number one"));
        assert_eq!(one.uploader.as_deref(), Some("King Edward VIII"));
        assert_eq!(one.description.as_deref(), Some("First. Second."));
        assert_eq!(one.duration, Some(Duration::from_secs_f64(402.29)));
        assert_eq!(
            one.thumbnail.as_ref().map(|t| t.as_str()),
            Some("https://archive.org/download/speeches/One.png")
        );
        assert_eq!(one.variants.len(), 3);
        assert!(one.variants.iter().all(|v| v.audio_only && v.video.is_none() && v.width.is_none()));
        assert_eq!(one.variants[0].label.as_deref(), Some("original"));
        assert_eq!(one.variants[0].audio, Some(AudioCodec::Mp3));
        assert_eq!(one.variants[2].label.as_deref(), Some("Ogg Vorbis"));
        assert_eq!(one.variants[2].audio, Some(AudioCodec::Vorbis));
        assert_eq!(one.subtitles.len(), 1);
        assert_eq!(one.subtitles[0].language, "en");
        assert_eq!(one.subtitles[0].name.as_deref(), Some("English"));
        assert_eq!(
            one.subtitles[0].url.as_str(),
            "https://archive.org/download/speeches/One.vtt"
        );

        // The private original is left out without a session; its derivative plays.
        let two = resolver
            .resolve(&Url::parse("https://archive.org/details/speeches/Two.flac").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(two.uploader.as_deref(), Some("J. Pershing"));
        assert_eq!(two.variants.len(), 1);
        assert!(two.variants[0].url.path().ends_with("/Two.mp3"));
        assert_eq!(
            parse_link(&Url::parse("https://archive.org/details/speeches/Number+One.mp3").unwrap()).unwrap().file.as_deref(),
            Some("Number One.mp3")
        );
    }
}
