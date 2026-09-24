//! MediaFire files and folders. A file is known by the public file API, which names it
//! and gives its size, type and upload time, and downloaded through the link its
//! download page carries on the download button; the link is probed for what the host
//! serves, so a video, a song, a picture or any other file comes back as what it is. A
//! folder is listed through the public folder API, its subfolders included, as a
//! playlist of its files.

use std::collections::VecDeque;
use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, Tag, Variant, VariantKind, clean_title, essence, fetch, navigation_headers,
    probe_file, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "mediafire";
const SITE: &str = "https://www.mediafire.com";
const API: &str = "https://www.mediafire.com/api/1.4";
/// How many files a folder listing is read up to, subfolders included.
pub const LISTING_LIMIT: usize = 200;
/// How many files the folder API lists per chunk.
const CHUNK_SIZE: usize = 100;
/// How deep into subfolders a folder listing goes.
const FOLDER_DEPTH: usize = 3;

/// A quick key (a file's, 11 or 15 characters) or a folder key (13 characters).
static RE_KEY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9]{11,15}$").unwrap());
/// `<a … href="https://download123.mediafire.com/…" … id="downloadButton">`.
static RE_DOWNLOAD_BUTTON: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?s)<a\b[^>]*\bid="downloadButton"[^>]*>"#).unwrap());
static RE_HREF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"\bhref="([^"]+)""#).unwrap());
/// `data-scrambled-url="…"`: the link base64-encoded, as some pages carry it.
static RE_SCRAMBLED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-scrambled-url="([A-Za-z0-9+/=]+)""#).unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    File {
        key: String,
        /// The file name the link carries, when it does.
        name: Option<String>,
    },
    Folder {
        key: String,
    },
}

fn is_folder_key(key: &str) -> bool {
    key.len() == 13
}

fn decoded(segment: &str) -> String {
    percent_encoding::percent_decode_str(segment)
        .decode_utf8_lossy()
        .into_owned()
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "mediafire.com" && !host.ends_with(".mediafire.com") {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let by_key = |key: &str| {
        let key = key.to_ascii_lowercase();
        if !RE_KEY.is_match(&key) {
            return None;
        }
        Some(if is_folder_key(&key) {
            Link::Folder { key }
        } else {
            Link::File { key, name: None }
        })
    };
    let file = |key: &str, rest: &[&str]| {
        let key = key.to_ascii_lowercase();
        RE_KEY.is_match(&key).then(|| Link::File {
            name: rest
                .first()
                .filter(|name| **name != "file")
                .map(|name| decoded(name)),
            key,
        })
    };
    if host.starts_with("download") {
        return match segments.as_slice() {
            [_, key, name] => file(key, &[name]),
            _ => None,
        };
    }
    match segments.as_slice() {
        [
            "file" | "file_premium" | "view" | "download",
            key,
            rest @ ..,
        ] => file(key, rest),
        ["folder", key, ..] => {
            let key = key.to_ascii_lowercase();
            RE_KEY.is_match(&key).then_some(Link::Folder { key })
        }
        [] => url.query().filter(|q| !q.contains('=')).and_then(by_key),
        [key] if host.starts_with("app.") => by_key(key),
        _ => None,
    }
}

/// What a file is and the format it is in: from the content type the host serves it as
/// when that names a format, else from the file's name, else from the broad type served.
pub fn classify(name: &str, content_type: &str) -> (MediaKind, Option<Container>) {
    if let Some(container) = Container::from_mime(content_type) {
        return (container.kind(), Some(container));
    }
    if let Some(container) = Container::from_name(name) {
        return (container.kind(), Some(container));
    }
    (MediaKind::from_mime(content_type), None)
}

/// The codec an audio container implies.
fn audio_codec(container: &Container) -> Option<AudioCodec> {
    Some(match container {
        Container::Mp3 => AudioCodec::Mp3,
        Container::M4a => AudioCodec::Aac,
        Container::Ogg => AudioCodec::Vorbis,
        Container::Opus => AudioCodec::Opus,
        Container::Flac => AudioCodec::Flac,
        Container::Wav => AudioCodec::Other("pcm".into()),
        _ => return None,
    })
}

/// A variant for a file of any kind, marked as the pipeline picks and shrinks it.
fn file_variant(
    url: Url,
    kind: MediaKind,
    container: Option<Container>,
    size: Option<u64>,
) -> Variant {
    let mut variant = Variant::new(url, VariantKind::File);
    match kind {
        MediaKind::Audio => {
            variant.audio_only = true;
            variant.audio = container.as_ref().and_then(audio_codec);
        }
        MediaKind::Video if container == Some(Container::Mp4) => {
            variant.video = Some(VideoCodec::H264);
            variant.audio = Some(AudioCodec::Aac);
        }
        _ => {}
    }
    variant.container = container;
    variant.size = size;
    variant.format_id = Some("original".to_string());
    variant.label = Some("original".to_string());
    variant
}

/// The direct download link a download page carries: on the download button, or
/// scrambled beside it.
pub fn download_link(html: &str) -> Option<Url> {
    if let Some(button) = RE_DOWNLOAD_BUTTON.find(html)
        && let Some(href) = util::search(&RE_HREF, button.as_str())
        && let Some(url) = util::join_url(None, &util::html_unescape(&href))
    {
        return Some(url);
    }
    let scrambled = util::search(&RE_SCRAMBLED, html)?;
    let bytes = util::b64_decode(&scrambled)?;
    util::join_url(None, &String::from_utf8_lossy(&bytes))
}

/// Why a download page withholds its link, when it says.
pub fn page_refusal(html: &str) -> Option<&'static str> {
    if html.contains("Google Safe Browsing") || html.contains("Download at Your Own Risk") {
        return Some("MediaFire flags the file as malicious and withholds its download link");
    }
    if html.contains("This file is no longer available")
        || html.contains("File Removed for Violation")
    {
        return Some("the file has been removed");
    }
    if html.contains("password_form") || html.contains("This file requires a password") {
        return Some("the file is password protected");
    }
    None
}

fn name_stem(name: &str) -> Option<String> {
    clean_title(name.rsplit_once('.').map_or(name, |(stem, _)| stem))
}

/// The page of a file: `/file/{key}/{name}/file`.
fn file_page(key: &str, name: Option<&str>) -> Url {
    let mut url = Url::parse(SITE).expect("valid");
    {
        let mut segments = url.path_segments_mut().expect("a base URL");
        segments.push("file");
        segments.push(key);
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            segments.push(name);
        }
        segments.push("file");
    }
    url
}

pub struct MediafireResolver {
    http: Http,
}

impl MediafireResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// A public API answer's `response`, or the error the API names.
    async fn api(&self, path: &str, origin: &Url) -> Result<Value, ResolveError> {
        let url = Url::parse(&format!("{API}/{path}&response_format=json"))
            .map_err(|e| ResolveError::malformed(origin, e.to_string()))?;
        let response = self
            .http
            .get(url)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "application/json")
            .send()
            .await?;
        let status = response.status;
        let text = response.text(MAX_PAGE).await?;
        let answer: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let response = answer["response"].clone();
        if status.as_u16() == 429 {
            return Err(ResolveError::RateLimited(origin.clone()));
        }
        if response.is_null() {
            return Err(match status_error(status, origin) {
                Some(error) => error,
                None => ResolveError::malformed(origin, "the API answered no JSON"),
            });
        }
        if response["result"].as_str() == Some("Error") {
            let code = util::int(&response["error"]).unwrap_or(0);
            let message = util::text(&response["message"]).unwrap_or_default();
            let lower = message.to_ascii_lowercase();
            return Err(
                if lower.contains("unknown or invalid") || matches!(code, 110..=112) {
                    ResolveError::NotFound(origin.clone())
                } else {
                    ResolveError::unavailable(origin, format!("MediaFire answered: {message}"))
                },
            );
        }
        Ok(response)
    }

    async fn resolve_file(
        &self,
        key: &str,
        name: Option<&str>,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let info = self
            .api(&format!("file/get_info.php?quick_key={key}"), origin)
            .await?["file_info"]
            .clone();
        if info.is_null() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        if info["password_protected"].as_str() == Some("yes") {
            return Err(ResolveError::unavailable(
                origin,
                "the file is password protected",
            ));
        }
        if info["ready"].as_str() == Some("no") {
            return Err(ResolveError::unavailable(
                origin,
                "the file is still being processed",
            ));
        }
        let api_name = util::text(&info["filename"]).filter(|n| !n.is_empty());
        let page_url = util::url_of(&info["links"]["normal_download"], None)
            .unwrap_or_else(|| file_page(key, api_name.as_deref().or(name)));
        let fetched = fetch(
            &self.http,
            &page_url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(fetched.status, origin) {
            return Err(error);
        }
        if fetched.url.path().contains("download_repair") {
            return Err(ResolveError::unavailable(
                origin,
                "MediaFire cannot serve the file and offers a download repair instead",
            ));
        }
        let html = fetched.text();
        let Some(direct) = download_link(&html) else {
            return Err(match page_refusal(&html) {
                Some(reason) => ResolveError::unavailable(origin, reason),
                None => ResolveError::malformed(origin, "the download page names no file link"),
            });
        };
        let probed = probe_file(&self.http, &direct, PLATFORM, BROWSER_UA, &[]).await?;
        match probed.status.as_u16() {
            200 | 206 => {}
            404 | 410 => return Err(ResolveError::NotFound(origin.clone())),
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the download host answered HTTP {status}"),
                ));
            }
        }
        let content_type = essence(probed.content_type.as_deref());
        if content_type == "text/html" {
            return Err(ResolveError::unavailable(
                origin,
                "the download host answered with a page instead of the file",
            ));
        }
        let file_name = probed
            .filename
            .clone()
            .or_else(|| api_name.clone())
            .or_else(|| name.map(str::to_string))
            .unwrap_or_else(|| key.to_string());
        let (kind, container) = classify(&file_name, &content_type);
        let size = probed.size.or_else(|| util::uint(&info["size"]));
        let mut resolved = Resolved::of(PLATFORM, kind);
        resolved.id = Some(key.to_string());
        resolved.title = name_stem(&file_name);
        resolved.description = util::text(&info["description"]).and_then(|d| clean_title(&d));
        resolved.uploader = util::text(&info["owner_name"]).and_then(|n| clean_title(&n));
        resolved.uploaded_at = util::text(&info["created_utc"])
            .or_else(|| util::text(&info["created"]))
            .and_then(|t| util::parse_timestamp(&t));
        resolved.webpage_url = Some(page_url);
        resolved.variants = vec![file_variant(direct, kind, container, size)];
        Ok(Resolution::from(resolved))
    }

    /// One chunk of a folder's files or subfolders.
    async fn folder_chunk(
        &self,
        key: &str,
        content: &str,
        chunk: usize,
        origin: &Url,
    ) -> Result<Value, ResolveError> {
        let answer = self
            .api(
                &format!(
                    "folder/get_content.php?folder_key={key}&content_type={content}&chunk={chunk}&chunk_size={CHUNK_SIZE}"
                ),
                origin,
            )
            .await?;
        Ok(answer["folder_content"].clone())
    }

    async fn resolve_folder(&self, key: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let info = self
            .api(&format!("folder/get_info.php?folder_key={key}"), origin)
            .await?["folder_info"]
            .clone();
        if info.is_null() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        if info["privacy"].as_str() == Some("private") {
            return Err(ResolveError::unavailable(origin, "the folder is private"));
        }
        let mut entries: Vec<PlaylistEntry> = Vec::new();
        let mut queue: VecDeque<(String, usize)> = VecDeque::from([(key.to_string(), 0)]);
        while let Some((folder, depth)) = queue.pop_front() {
            let mut chunk = 1;
            loop {
                let content = self.folder_chunk(&folder, "files", chunk, origin).await?;
                for file in content["files"].as_array().into_iter().flatten() {
                    let Some(quickkey) = util::text(&file["quickkey"]) else {
                        continue;
                    };
                    let file_name = util::text(&file["filename"]).filter(|n| !n.is_empty());
                    entries.push(PlaylistEntry {
                        url: file_page(&quickkey, file_name.as_deref()),
                        title: file_name.as_deref().and_then(clean_title),
                        duration: None,
                    });
                    if entries.len() >= LISTING_LIMIT {
                        break;
                    }
                }
                if entries.len() >= LISTING_LIMIT || content["more_chunks"].as_str() != Some("yes")
                {
                    break;
                }
                chunk += 1;
            }
            if entries.len() >= LISTING_LIMIT || depth >= FOLDER_DEPTH {
                continue;
            }
            let mut chunk = 1;
            loop {
                let content = self.folder_chunk(&folder, "folders", chunk, origin).await?;
                for subfolder in content["folders"].as_array().into_iter().flatten() {
                    if let Some(subkey) = util::text(&subfolder["folderkey"])
                        && subfolder["privacy"].as_str() != Some("private")
                    {
                        queue.push_back((subkey, depth + 1));
                    }
                }
                if content["more_chunks"].as_str() != Some("yes") {
                    break;
                }
                chunk += 1;
            }
        }
        if entries.is_empty() {
            return Err(ResolveError::unavailable(
                origin,
                "the folder holds no files",
            ));
        }
        let name = util::text(&info["name"]).filter(|n| !n.is_empty());
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(key.to_string()),
            title: name.as_deref().and_then(clean_title),
            total: util::uint(&info["file_count"])
                .map(|n| n as usize)
                .filter(|n| *n >= entries.len())
                .or(Some(entries.len())),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for MediafireResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "MediaFire",
            hosts: &["mediafire.com"],
            features: &[
                "files",
                "folders",
                "short links",
                "audio",
                "images",
                "any file",
            ],
            formats: &[
                "mp4", "mkv", "webm", "mov", "mp3", "m4a", "flac", "jpg", "png", "pdf", "zip",
            ],
            media: &[
                MediaKind::Video,
                MediaKind::Audio,
                MediaKind::Image,
                MediaKind::File,
            ],
            tags: &[Tag::Files],
            session: SessionSupport::None,
            examples: &[
                "https://www.mediafire.com/file/8x5ol3r8wpb477a/small.mp4/file",
                "https://www.mediafire.com/file/0003gwq2bqwcfof/12-AudioTrack_12.mp3/file",
                "https://www.mediafire.com/file/00004q3n5o7dyrv/dead_bubble_coral.png/file",
                "https://www.mediafire.com/file/xjrpa3x71gg3vw2/AppliedEnergistics.cfg/file",
                "https://www.mediafire.com/?8x5ol3r8wpb477a",
                "https://www.mediafire.com/download/8x5ol3r8wpb477a",
                "https://www.mediafire.com/folder/004cux108lmed/minetweaker",
                "https://www.mediafire.com/folder/005c9b1ufmc77/CARTAS",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::File { key, name } => self.resolve_file(&key, name.as_deref(), url).await,
            Link::Folder { key } => self.resolve_folder(&key, url).await,
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

    fn probe(url: &str, content_type: &str, size: u64, name: &str) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: "GET".into(),
                url: url.into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status: 206,
                url: url.into(),
                headers: vec![
                    ("content-type".into(), content_type.into()),
                    ("content-range".into(), format!("bytes 0-0/{size}")),
                    (
                        "content-disposition".into(),
                        format!("attachment; filename=\"{name}\""),
                    ),
                ],
                body: RecordedBody::Empty,
                truncated: false,
            },
        }
    }

    fn info(key: &str, name: &str, size: u64, mime: &str) -> String {
        json!({"response": {"action": "file/get_info", "file_info": {
            "quickkey": key, "filename": name, "ready": "yes", "created": "2018-05-06 04:43:11",
            "created_utc": "2018-05-06T09:43:11Z", "description": "", "size": size.to_string(),
            "privacy": "public", "password_protected": "no", "filetype": "video", "mimetype": mime,
            "owner_name": "bxp00202 bxp00202",
            "links": {"normal_download": format!("https://www.mediafire.com/file/{key}/{name}/file")}
        }, "result": "Success", "current_api_version": "1.5"}})
        .to_string()
    }

    fn page(direct: &str) -> String {
        format!(
            r#"<html><body><div class="filename">small.mp4</div><a class="input popsok" aria-label="Download file" href="{direct}" id="downloadButton" rel="nofollow">Download</a></body></html>"#
        )
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let file = |key: &str, name: Option<&str>| {
            Some(Link::File {
                key: key.into(),
                name: name.map(String::from),
            })
        };
        let folder = |key: &str| Some(Link::Folder { key: key.into() });
        assert_eq!(
            link("https://www.mediafire.com/file/8x5ol3r8wpb477a/small.mp4/file"),
            file("8x5ol3r8wpb477a", Some("small.mp4"))
        );
        assert_eq!(
            link("https://www.mediafire.com/file/8x5ol3r8wpb477a/small.mp4"),
            file("8x5ol3r8wpb477a", Some("small.mp4"))
        );
        assert_eq!(
            link("https://www.mediafire.com/file/8x5ol3r8wpb477a"),
            file("8x5ol3r8wpb477a", None)
        );
        assert_eq!(
            link(
                "https://www.mediafire.com/file_premium/9t9mhhch8mhr34s/Bob_Giant_Robot_Map.zip/file"
            ),
            file("9t9mhhch8mhr34s", Some("Bob_Giant_Robot_Map.zip"))
        );
        assert_eq!(
            link("https://www.mediafire.com/view/8x5ol3r8wpb477a/small.mp4"),
            file("8x5ol3r8wpb477a", Some("small.mp4"))
        );
        assert_eq!(
            link("https://www.mediafire.com/download/8x5ol3r8wpb477a"),
            file("8x5ol3r8wpb477a", None)
        );
        assert_eq!(
            link("https://www.mediafire.com/?8x5ol3r8wpb477a"),
            file("8x5ol3r8wpb477a", None)
        );
        assert_eq!(
            link("https://mediafire.com/file/00002fm4g4bdo34/Ja%203636.jpg/file"),
            file("00002fm4g4bdo34", Some("Ja 3636.jpg"))
        );
        assert_eq!(
            link(
                "https://download1479.mediafire.com/d3d60xz0c23g/00004q3n5o7dyrv/dead_bubble_coral.png"
            ),
            file("00004q3n5o7dyrv", Some("dead_bubble_coral.png"))
        );
        assert_eq!(
            link("https://www.mediafire.com/folder/004cux108lmed/minetweaker"),
            folder("004cux108lmed")
        );
        assert_eq!(
            link("https://www.mediafire.com/folder/004cux108lmed/"),
            folder("004cux108lmed")
        );
        assert_eq!(
            link("https://www.mediafire.com/?004cux108lmed"),
            folder("004cux108lmed")
        );
        assert_eq!(
            link("https://app.mediafire.com/004cux108lmed"),
            folder("004cux108lmed")
        );
        assert_eq!(link("https://www.mediafire.com/"), None);
        assert_eq!(link("https://www.mediafire.com/upgrade/"), None);
        assert_eq!(link("https://www.mediafire.com/file/short/x"), None);
        assert_eq!(link("https://www.mediafire.com/?dl=1"), None);
        assert_eq!(
            link("https://example.com/file/8x5ol3r8wpb477a/small.mp4/file"),
            None
        );
    }

    #[test]
    fn download_links_and_refusals_are_read() {
        assert_eq!(
            download_link(&page(
                "https://download2290.mediafire.com/abc/8x5ol3r8wpb477a/small.mp4"
            ))
            .unwrap()
            .as_str(),
            "https://download2290.mediafire.com/abc/8x5ol3r8wpb477a/small.mp4"
        );
        let scrambled = format!(
            r#"<a id="downloadButton" data-scrambled-url="{}">Download</a>"#,
            util::b64_encode(b"https://download1.mediafire.com/x/key/a.zip")
        );
        assert_eq!(
            download_link(&scrambled).unwrap().as_str(),
            "https://download1.mediafire.com/x/key/a.zip"
        );
        assert_eq!(download_link("<html></html>"), None);
        assert!(
            page_refusal("<p>Detected by Google Safe Browsing</p>")
                .unwrap()
                .contains("malicious")
        );
        assert_eq!(page_refusal("<html></html>"), None);
        assert_eq!(
            classify("a.zip", "application/zip"),
            (MediaKind::File, Some(Container::Other("zip".into())))
        );
        assert_eq!(
            classify("song", "audio/mpeg"),
            (MediaKind::Audio, Some(Container::Mp3))
        );
    }

    #[tokio::test]
    async fn files_resolve_as_what_the_host_serves() {
        let direct = "https://download2290.mediafire.com/q5fbyz0xkjsg/8x5ol3r8wpb477a/small.mp4";
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.mediafire.com/api/1.4/file/get_info.php?quick_key=8x5ol3r8wpb477a&response_format=json",
            200,
            "application/json",
            &info("8x5ol3r8wpb477a", "small.mp4", 383631, "video/mp4"),
        ));
        fixture.exchanges.push(get(
            "https://www.mediafire.com/file/8x5ol3r8wpb477a/small.mp4/file",
            200,
            "text/html",
            &page(direct),
        ));
        fixture
            .exchanges
            .push(probe(direct, "video/mp4", 383631, "small.mp4"));
        let song = "https://download1.mediafire.com/t/0003gwq2bqwcfof/12-AudioTrack_12.mp3";
        fixture.exchanges.push(get(
            "https://www.mediafire.com/api/1.4/file/get_info.php?quick_key=0003gwq2bqwcfof&response_format=json",
            200,
            "application/json",
            &info("0003gwq2bqwcfof", "12-AudioTrack 12.mp3", 1972959, "audio/mpeg"),
        ));
        fixture.exchanges.push(get(
            "https://www.mediafire.com/file/0003gwq2bqwcfof/12-AudioTrack 12.mp3/file",
            200,
            "text/html",
            &page(song),
        ));
        fixture
            .exchanges
            .push(probe(song, "audio/mpeg", 1972959, "12-AudioTrack 12.mp3"));
        let resolver = MediafireResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.mediafire.com/?8x5ol3r8wpb477a").unwrap();
        assert!(resolver.matches(&url));
        let video = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(video.media, MediaKind::Video);
        assert_eq!(video.id.as_deref(), Some("8x5ol3r8wpb477a"));
        assert_eq!(video.title.as_deref(), Some("small"));
        assert_eq!(video.uploader.as_deref(), Some("bxp00202 bxp00202"));
        assert!(video.uploaded_at.is_some());
        assert_eq!(video.variants.len(), 1);
        assert_eq!(video.variants[0].url.as_str(), direct);
        assert_eq!(video.variants[0].container, Some(Container::Mp4));
        assert_eq!(video.variants[0].size, Some(383631));
        assert_eq!(video.variants[0].video, Some(VideoCodec::H264));
        let audio = resolver
            .resolve(
                &Url::parse(
                    "https://www.mediafire.com/file/0003gwq2bqwcfof/12-AudioTrack_12.mp3/file",
                )
                .unwrap(),
            )
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(audio.media, MediaKind::Audio);
        assert_eq!(audio.title.as_deref(), Some("12-AudioTrack 12"));
        assert!(audio.variants[0].audio_only);
        assert_eq!(audio.variants[0].audio, Some(AudioCodec::Mp3));
    }

    #[tokio::test]
    async fn withheld_and_missing_files_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.mediafire.com/api/1.4/file/get_info.php?quick_key=zoggx72o8yo2aoe&response_format=json",
            200,
            "application/json",
            &info("zoggx72o8yo2aoe", "VID.mp4", 23612653, "video/mp4"),
        ));
        fixture.exchanges.push(get(
            "https://www.mediafire.com/file/zoggx72o8yo2aoe/VID.mp4/file",
            200,
            "text/html",
            "<html><body><h1>Detected by Google Safe Browsing</h1><button>Download Anyway</button></body></html>",
        ));
        fixture.exchanges.push(get(
            "https://www.mediafire.com/api/1.4/file/get_info.php?quick_key=zzzzzzzzzzzzzzz&response_format=json",
            404,
            "application/json",
            r#"{"response":{"action":"file/get_info","message":"Unknown or Invalid QuickKey","error":110,"result":"Error","current_api_version":"1.5"}}"#,
        ));
        fixture.exchanges.push(get(
            "https://www.mediafire.com/api/1.4/file/get_info.php?quick_key=9t9mhhch8mhr34s&response_format=json",
            200,
            "application/json",
            &info("9t9mhhch8mhr34s", "Map.zip", 59132738, "application/zip"),
        ));
        fixture.exchanges.push(Exchange {
            request: RecordedRequest {
                method: "GET".into(),
                url: "https://www.mediafire.com/file/9t9mhhch8mhr34s/Map.zip/file".into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status: 200,
                url: "https://www.mediafire.com/download_repair.php?qkey=9t9mhhch8mhr34s&origin=server_error".into(),
                headers: vec![("content-type".into(), "text/html".into())],
                body: RecordedBody::Text("<html>repair</html>".into()),
                truncated: false,
            },
        });
        let resolver = MediafireResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(
                &Url::parse("https://www.mediafire.com/file/zoggx72o8yo2aoe/VID.mp4/file").unwrap(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("malicious")),
            "{error}"
        );
        assert!(matches!(
            resolver
                .resolve(
                    &Url::parse("https://www.mediafire.com/file/zzzzzzzzzzzzzzz/x/file").unwrap()
                )
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(
                &Url::parse("https://www.mediafire.com/file/9t9mhhch8mhr34s/Map.zip/file").unwrap(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("repair")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn folders_list_their_files_and_subfolders() {
        let file = |key: &str, name: &str| {
            json!({"quickkey": key, "filename": name, "size": "1322", "mimetype": "text/plain", "privacy": "public",
                "links": {"normal_download": format!("https://www.mediafire.com/file/{key}/{name}/file")}})
        };
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.mediafire.com/api/1.4/folder/get_info.php?folder_key=005c9b1ufmc77&response_format=json",
            200,
            "application/json",
            &json!({"response": {"folder_info": {"folderkey": "005c9b1ufmc77", "name": "CARTAS", "privacy": "public", "file_count": "3", "folder_count": "1"}, "result": "Success"}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://www.mediafire.com/api/1.4/folder/get_content.php?folder_key=005c9b1ufmc77&content_type=files&chunk=1&chunk_size=100&response_format=json",
            200,
            "application/json",
            &json!({"response": {"folder_content": {"chunk_number": "1", "more_chunks": "no", "files": [file("xjrpa3x71gg3vw2", "Applied Energistics.cfg")]}, "result": "Success"}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://www.mediafire.com/api/1.4/folder/get_content.php?folder_key=005c9b1ufmc77&content_type=folders&chunk=1&chunk_size=100&response_format=json",
            200,
            "application/json",
            &json!({"response": {"folder_content": {"chunk_number": "1", "more_chunks": "no", "folders": [{"folderkey": "s71ud2sucysqq", "name": "CARGAS", "file_count": "2", "privacy": "public"}]}, "result": "Success"}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://www.mediafire.com/api/1.4/folder/get_content.php?folder_key=s71ud2sucysqq&content_type=files&chunk=1&chunk_size=100&response_format=json",
            200,
            "application/json",
            &json!({"response": {"folder_content": {"chunk_number": "1", "more_chunks": "no", "files": [file("edhlbhtccc3t6dp", "BigReactors.cfg"), file("9ioq99vm3padmlr", "Dictionary.cfg")]}, "result": "Success"}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://www.mediafire.com/api/1.4/folder/get_content.php?folder_key=s71ud2sucysqq&content_type=folders&chunk=1&chunk_size=100&response_format=json",
            200,
            "application/json",
            &json!({"response": {"folder_content": {"chunk_number": "1", "more_chunks": "no", "folders": []}, "result": "Success"}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://www.mediafire.com/api/1.4/folder/get_info.php?folder_key=4615ddx28gu65&response_format=json",
            404,
            "application/json",
            r#"{"response":{"action":"folder/get_info","message":"Unknown or invalid FolderKey","error":112,"result":"Error","current_api_version":"1.5"}}"#,
        ));
        let resolver = MediafireResolver::new(Http::replay(fixture));
        let Resolution::Playlist(playlist) = resolver
            .resolve(&Url::parse("https://www.mediafire.com/folder/005c9b1ufmc77/CARTAS").unwrap())
            .await
            .unwrap()
        else {
            panic!("a folder is a playlist");
        };
        assert_eq!(playlist.title.as_deref(), Some("CARTAS"));
        assert_eq!(playlist.total, Some(3));
        assert_eq!(playlist.entries.len(), 3);
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://www.mediafire.com/file/xjrpa3x71gg3vw2/Applied%20Energistics.cfg/file"
        );
        assert_eq!(
            playlist.entries[0].title.as_deref(),
            Some("Applied Energistics.cfg")
        );
        assert_eq!(
            playlist.entries[2].url.as_str(),
            "https://www.mediafire.com/file/9ioq99vm3padmlr/Dictionary.cfg/file"
        );
        assert!(matches!(
            resolver
                .resolve(
                    &Url::parse("https://www.mediafire.com/folder/4615ddx28gu65/Videos").unwrap()
                )
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    /// Every example link resolves live, and the files among them come back as what
    /// they are.
    #[tokio::test]
    #[ignore = "requires live MediaFire access"]
    async fn live_examples_resolve() {
        let resolver = MediafireResolver::new(Http::new(crate::http::HttpConfig::default()));
        let mut kinds = Vec::new();
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
            assert!(resolver.matches(&url), "{link}: not matched");
            let resolution = tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&url))
                .await
                .expect("resolution timed out")
                .unwrap_or_else(|e| panic!("{link}: {e}"));
            match resolution {
                Resolution::Media(resolved) => {
                    assert!(
                        resolved.variants.iter().any(|v| v.is_playable()),
                        "{link}: no variants"
                    );
                    println!(
                        "{link}: {:?} {} ({} bytes)",
                        resolved.title,
                        resolved.media,
                        resolved.variants[0].size.unwrap_or(0)
                    );
                    kinds.push(resolved.media);
                }
                Resolution::Playlist(playlist) => {
                    assert!(!playlist.entries.is_empty(), "{link}: no entries");
                    println!(
                        "{link}: {:?} ({} of {:?} entries)",
                        playlist.title,
                        playlist.entries.len(),
                        playlist.total
                    );
                }
            }
        }
        for kind in MediaKind::ALL {
            assert!(kinds.contains(&kind), "no example resolved to {kind}");
        }
    }
}
