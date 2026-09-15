//! Internet Archive items, through the metadata API that lists an item's files: the video
//! files of an item, each original with its derivatives as variants, and an item holding
//! several videos as a playlist of them. A link naming one file of the item resolves that
//! file alone.

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
    SessionSupport, Variant, VariantKind, clean_title, fetch, parse_time_stamp,
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
const VIDEO_EXTENSIONS: [&str; 16] = [
    "mp4", "m4v", "webm", "mkv", "mov", "avi", "ogv", "mpg", "mpeg", "ts", "m2ts", "flv", "wmv",
    "3gp", "gif", "mts",
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
            percent_encoding::percent_decode_str(s)
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
                width: number(&file["width"]),
                height: number(&file["height"]),
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
                    && is_video_name(original)
                    && files.iter().any(|f| &f.name == original) =>
            {
                current = original.clone();
            }
            _ => break,
        }
    }
    current
}

/// The item's videos: each original video with its derivatives, keyed by the original's
/// name, in the order the API lists them.
pub fn videos_of(files: &[ItemFile]) -> Vec<(String, Vec<ItemFile>)> {
    let mut groups: BTreeMap<String, Vec<ItemFile>> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for file in files.iter().filter(|f| is_video_name(&f.name)) {
        let root = root_of(&file.name, files);
        if !groups.contains_key(&root) {
            order.push(root.clone());
        }
        groups.entry(root).or_default().push(file.clone());
    }
    order
        .into_iter()
        .filter_map(|root| groups.remove(&root).map(|group| (root, group)))
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

pub fn variant_of(item: &str, file: &ItemFile) -> Variant {
    let ext = extension_of(&file.name).unwrap_or_default();
    let mut v = Variant::new(download_url(item, &file.name), VariantKind::File);
    v.container = Container::from_extension(&ext);
    let (video, audio) = codecs_for(&file.format, &ext);
    v.video = video;
    v.audio = audio;
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
        (_, Some(h)) => format!("{h}p {}", file.format),
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
}

fn resolved_of(
    item: &str,
    metadata: &Value,
    title: Option<String>,
    group: &[ItemFile],
) -> Resolved {
    let meta = &metadata["metadata"];
    let mut resolved = Resolved::new(PLATFORM);
    resolved.id = Some(item.to_string());
    resolved.title = title.or_else(|| text(&meta["title"]).and_then(|t| clean_title(&t)));
    resolved.description = text(&meta["description"]).and_then(|d| clean_title(&d));
    resolved.uploader = text(&meta["creator"])
        .or_else(|| text(&meta["uploader"]))
        .and_then(|u| clean_title(&u));
    resolved.uploaded_at = text(&meta["publicdate"])
        .or_else(|| text(&meta["addeddate"]))
        .and_then(|d| parse_date(&d));
    resolved.duration = group.iter().filter_map(|f| f.length).max();
    resolved.thumbnail = Url::parse(&format!("{SITE}services/img/{item}")).ok();
    resolved.webpage_url = Url::parse(&format!("{SITE}details/{item}")).ok();
    resolved.variants = group.iter().map(|f| variant_of(item, f)).collect();
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
            features: &["items", "files", "embeds", "multi-video items"],
            formats: &["mp4", "webm", "ogv", "avi", "mkv", "mov"],
            session: SessionSupport::None,
            examples: &[
                "https://archive.org/details/BigBuckBunny_124",
                "https://archive.org/download/BigBuckBunny_124/Content/big_buck_bunny_720p_surround.mp4",
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
        let videos = videos_of(&files);
        if let Some(wanted) = &link.file {
            let group = videos
                .iter()
                .find(|(_, group)| group.iter().any(|f| &f.name == wanted))
                .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
            let title = (videos.len() > 1).then(|| stem(&group.0));
            let mut resolved = resolved_of(&link.item, &metadata, title, &group.1);
            resolved.webpage_url = Url::parse(&format!(
                "{SITE}details/{}/{}",
                link.item,
                utf8_percent_encode(&group.0, PATH)
            ))
            .ok();
            return Ok(Resolution::from(resolved));
        }
        match videos.len() {
            0 => Err(ResolveError::unavailable(
                url,
                "the item holds no video files",
            )),
            1 => Ok(Resolution::from(resolved_of(
                &link.item,
                &metadata,
                None,
                &videos[0].1,
            ))),
            _ => {
                let entries = videos
                    .iter()
                    .map(|(root, group)| PlaylistEntry {
                        url: Url::parse(&format!(
                            "{SITE}details/{}/{}",
                            link.item,
                            utf8_percent_encode(root, PATH)
                        ))
                        .expect("valid"),
                        title: Some(stem(root)),
                        duration: group.iter().filter_map(|f| f.length).max(),
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
        fixture
            .exchanges
            .push(get("https://archive.org/metadata/nothing", 200, "{}"));
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
}
