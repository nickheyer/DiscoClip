//! Pixeldrain files, lists and filesystem shares, through the JSON API the site's own
//! viewer reads: a file's info names what it is, how big it is and whether the host still
//! serves it, and the file itself comes from the same API with byte ranges. A list is
//! the files it was made of, each a file link of its own. A filesystem share is a
//! directory tree: a directory is the files and directories in it, a file is served
//! from its path. Files are of any kind, told apart by the type the host stores and
//! the name the file was uploaded with.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, Tag, Variant, VariantKind, clean_title, dropbox, fetch, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "pixeldrain";
const SITE: &str = "https://pixeldrain.com";
const API: &str = "https://pixeldrain.com/api";

static RE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]{4,32}$").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// One uploaded file.
    File { id: String },
    /// A list of uploaded files.
    List { id: String },
    /// A file or directory in a filesystem share: the share's id and the path below it.
    Filesystem { id: String, path: Vec<String> },
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "pixeldrain.com" && host != "www.pixeldrain.com" {
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
    let id_of = |s: &str| RE_ID.is_match(s).then(|| s.to_string());
    let refs: Vec<&str> = segments.iter().map(String::as_str).collect();
    match refs.as_slice() {
        ["u", id] | ["api", "file", id] | ["api", "file", id, "info"] => {
            Some(Link::File { id: id_of(id)? })
        }
        ["l", id] | ["api", "list", id] => Some(Link::List { id: id_of(id)? }),
        ["d", id, rest @ ..] | ["api", "filesystem", id, rest @ ..] => Some(Link::Filesystem {
            id: id_of(id)?,
            path: rest.iter().map(|s| s.to_string()).collect(),
        }),
        _ => None,
    }
}

/// The codec an audio container implies, for the variant's `audio`.
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
pub fn file_variant(
    url: Url,
    kind: MediaKind,
    container: Option<Container>,
    size: Option<u64>,
) -> Variant {
    let mut v = Variant::new(url, VariantKind::File);
    match kind {
        MediaKind::Audio => {
            v.audio_only = true;
            v.audio = container.as_ref().and_then(audio_codec);
        }
        MediaKind::Video if container == Some(Container::Mp4) => {
            v.video = Some(VideoCodec::H264);
            v.audio = Some(AudioCodec::Aac);
        }
        _ => {}
    }
    v.container = container;
    v.size = size;
    v.format_id = Some("original".to_string());
    v.label = Some("original".to_string());
    v
}

/// A file's name without its extension, as a title.
pub fn title_of(name: &str) -> Option<String> {
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    clean_title(stem).or_else(|| clean_title(name))
}

/// The API path of a share's file or directory: the id and each path segment, encoded.
fn filesystem_path(id: &str, path: &[String]) -> String {
    let mut out = format!("{API}/filesystem/{id}");
    for segment in path {
        out.push('/');
        out.push_str(&util::url_encode(segment));
    }
    out
}

/// The page of a share's file or directory.
fn filesystem_page(id: &str, path: &[String]) -> Option<Url> {
    let mut out = format!("{SITE}/d/{id}");
    for segment in path {
        out.push('/');
        out.push_str(&util::url_encode(segment));
    }
    Url::parse(&out).ok()
}

pub struct PixeldrainResolver {
    http: Http,
}

impl PixeldrainResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// A JSON answer of the API, read as `origin`'s: the API's own error codes become
    /// the errors they mean, and any other failing status its own.
    async fn api(&self, url: &Url, origin: &Url) -> Result<Value, ResolveError> {
        let fetched = fetch(&self.http, url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        let json: Option<Value> = serde_json::from_slice(&fetched.body).ok();
        if let Some(json) = &json
            && json["success"].as_bool() == Some(false)
        {
            let code = json["value"].as_str().unwrap_or("");
            let message = json["message"]
                .as_str()
                .filter(|m| !m.is_empty())
                .unwrap_or("the API refused the request");
            return Err(match code {
                "not_found" | "file_not_found" | "list_not_found" | "path_not_found" => {
                    ResolveError::NotFound(origin.clone())
                }
                "rate_limit_reached" => ResolveError::RateLimited(origin.clone()),
                _ => ResolveError::unavailable(origin, message),
            });
        }
        if let Some(error) = status_error(fetched.status, origin) {
            return Err(error);
        }
        json.ok_or_else(|| ResolveError::malformed(origin, "the API answered without JSON"))
    }

    async fn resolve_file(&self, id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let info_url = Url::parse(&format!("{API}/file/{id}/info")).expect("valid");
        let info = self.api(&info_url, origin).await?;
        if let Some(reason) = util::text(&info["availability"]) {
            return Err(ResolveError::unavailable(
                origin,
                util::text(&info["availability_message"]).unwrap_or(reason),
            ));
        }
        if info["can_download"].as_bool() == Some(false) {
            return Err(ResolveError::unavailable(
                origin,
                "the host does not serve the file",
            ));
        }
        let name = util::text(&info["name"]).unwrap_or_default();
        let mime = util::text(&info["mime_type"]).unwrap_or_default();
        let (kind, container) = dropbox::classify(&name, &mime);
        let download = Url::parse(&format!("{API}/file/{id}?download")).expect("valid");
        let mut resolved = Resolved::of(PLATFORM, kind);
        resolved.id = Some(id.to_string());
        resolved.title = title_of(&name);
        resolved.uploaded_at = util::time(&info["date_upload"]);
        if matches!(kind, MediaKind::Video | MediaKind::Image) {
            // The largest thumbnail the API renders.
            resolved.thumbnail =
                Url::parse(&format!("{API}/file/{id}/thumbnail?width=256&height=256")).ok();
        }
        resolved.webpage_url = Url::parse(&format!("{SITE}/u/{id}")).ok();
        resolved.variants = vec![file_variant(
            download,
            kind,
            container,
            util::uint(&info["size"]).filter(|s| *s > 0),
        )];
        Ok(Resolution::from(resolved))
    }

    async fn resolve_list(&self, id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let list_url = Url::parse(&format!("{API}/list/{id}")).expect("valid");
        let list = self.api(&list_url, origin).await?;
        let entries: Vec<PlaylistEntry> = list["files"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|file| {
                let file_id = util::text(&file["id"])?;
                Some(PlaylistEntry {
                    url: Url::parse(&format!("{SITE}/u/{file_id}")).ok()?,
                    title: util::text(&file["name"]).and_then(|n| clean_title(&n)),
                    duration: None,
                })
            })
            .collect();
        if entries.is_empty() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(id.to_string()),
            title: util::text(&list["title"]).and_then(|t| clean_title(&t)),
            total: util::uint(&list["file_count"])
                .map(|n| n as usize)
                .filter(|n| *n >= entries.len())
                .or(Some(entries.len())),
            entries,
        }))
    }

    async fn resolve_filesystem(
        &self,
        id: &str,
        path: &[String],
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let stat_url = Url::parse(&format!("{}?stat", filesystem_path(id, path))).expect("valid");
        let stat = self.api(&stat_url, origin).await?;
        if stat["permissions"]["read"].as_bool() == Some(false) {
            return Err(ResolveError::unavailable(
                origin,
                "the share is not readable",
            ));
        }
        let Some(node) = stat["path"].as_array().and_then(|nodes| nodes.last()) else {
            return Err(ResolveError::malformed(origin, "the share names no path"));
        };
        let name = util::text(&node["name"]).unwrap_or_else(|| id.to_string());
        match node["type"].as_str() {
            Some("file") => {
                let mime = util::text(&node["file_type"]).unwrap_or_default();
                let (kind, container) = dropbox::classify(&name, &mime);
                let download = Url::parse(&filesystem_path(id, path)).expect("valid");
                let mut resolved = Resolved::of(PLATFORM, kind);
                resolved.id = util::text(&node["id"]).or_else(|| Some(id.to_string()));
                resolved.title = title_of(&name);
                resolved.uploaded_at = util::time(&node["created"]);
                resolved.webpage_url = filesystem_page(id, path);
                resolved.variants = vec![file_variant(
                    download,
                    kind,
                    container,
                    util::uint(&node["file_size"]).filter(|s| *s > 0),
                )];
                Ok(Resolution::from(resolved))
            }
            Some("dir") => {
                let entries: Vec<PlaylistEntry> = stat["children"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|child| {
                        let child_name = util::text(&child["name"])?;
                        // Dot files are the share's own indexes, not what was shared.
                        if child_name.starts_with('.') {
                            return None;
                        }
                        let mut child_path = path.to_vec();
                        child_path.push(child_name.clone());
                        Some(PlaylistEntry {
                            url: filesystem_page(id, &child_path)?,
                            title: clean_title(&child_name),
                            duration: None,
                        })
                    })
                    .collect();
                if entries.is_empty() {
                    return Err(ResolveError::NotFound(origin.clone()));
                }
                Ok(Resolution::Playlist(Playlist {
                    resolver: PLATFORM.to_string(),
                    id: Some(if path.is_empty() {
                        id.to_string()
                    } else {
                        format!("{id}/{}", path.join("/"))
                    }),
                    title: clean_title(&name),
                    total: Some(entries.len()),
                    entries,
                }))
            }
            other => Err(ResolveError::malformed(
                origin,
                format!("the share names a {} path", other.unwrap_or("typeless")),
            )),
        }
    }
}

#[async_trait]
impl Resolver for PixeldrainResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Pixeldrain",
            hosts: &["pixeldrain.com"],
            features: &[
                "files",
                "lists",
                "filesystem shares",
                "directories",
                "audio",
                "images",
                "any file",
            ],
            formats: &[
                "mp4", "webm", "mkv", "mov", "mp3", "m4a", "flac", "jpg", "png", "gif", "zip",
                "pdf",
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
                "https://pixeldrain.com/u/VFjemoqC",
                "https://pixeldrain.com/u/dt92pNgf",
                "https://pixeldrain.com/l/6ceqkTSn",
                "https://pixeldrain.com/d/8xz8hcYJ",
                "https://pixeldrain.com/d/qTnZkhCJ",
                "https://pixeldrain.com/d/qTnZkhCJ/subdir",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::File { id } => self.resolve_file(&id, url).await,
            Link::List { id } => self.resolve_list(&id, url).await,
            Link::Filesystem { id, path } => self.resolve_filesystem(&id, &path, url).await,
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

    fn get(url: &str, status: u16, body: String) -> Exchange {
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
                body: RecordedBody::Text(body),
                truncated: false,
            },
        }
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let file = |id: &str| Some(Link::File { id: id.into() });
        assert_eq!(link("https://pixeldrain.com/u/VFjemoqC"), file("VFjemoqC"));
        assert_eq!(
            link("https://www.pixeldrain.com/u/VFjemoqC?embed"),
            file("VFjemoqC")
        );
        assert_eq!(
            link("https://pixeldrain.com/api/file/VFjemoqC"),
            file("VFjemoqC")
        );
        assert_eq!(
            link("https://pixeldrain.com/api/file/VFjemoqC/info"),
            file("VFjemoqC")
        );
        assert_eq!(
            link("https://pixeldrain.com/l/6ceqkTSn"),
            Some(Link::List {
                id: "6ceqkTSn".into()
            })
        );
        assert_eq!(
            link("https://pixeldrain.com/api/list/6ceqkTSn"),
            Some(Link::List {
                id: "6ceqkTSn".into()
            })
        );
        assert_eq!(
            link("https://pixeldrain.com/d/qTnZkhCJ"),
            Some(Link::Filesystem {
                id: "qTnZkhCJ".into(),
                path: vec![]
            })
        );
        assert_eq!(
            link("https://pixeldrain.com/d/qTnZkhCJ/subdir/test%203.mp4"),
            Some(Link::Filesystem {
                id: "qTnZkhCJ".into(),
                path: vec!["subdir".into(), "test 3.mp4".into()]
            })
        );
        assert_eq!(
            link("https://pixeldrain.com/api/filesystem/qTnZkhCJ/subdir"),
            Some(Link::Filesystem {
                id: "qTnZkhCJ".into(),
                path: vec!["subdir".into()]
            })
        );
        assert_eq!(link("https://pixeldrain.com/"), None);
        assert_eq!(link("https://pixeldrain.com/api"), None);
        assert_eq!(link("https://pixeldrain.com/u/"), None);
        assert_eq!(link("https://pixeldrain.com/u/a%20b"), None);
        assert_eq!(link("https://pixeldrain.com/login"), None);
        assert_eq!(link("https://example.com/u/VFjemoqC"), None);
    }

    fn info(id: &str, name: &str, mime: &str, size: u64) -> Value {
        json!({
            "success": true, "id": id, "name": name, "size": size, "views": 243,
            "date_upload": "2023-06-09T00:42:01.728Z", "date_last_view": "2026-09-14T16:58:00.677Z",
            "mime_type": mime, "thumbnail_href": format!("/file/{id}/thumbnail"),
            "availability": "", "availability_message": "", "can_edit": false, "can_download": true
        })
    }

    #[tokio::test]
    async fn files_of_every_kind_resolve_through_their_info() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://pixeldrain.com/api/file/VFjemoqC/info",
            200,
            info(
                "VFjemoqC",
                "youtube-dl_test_video.mp4",
                "video/mp4",
                1828366,
            )
            .to_string(),
        ));
        fixture.exchanges.push(get(
            "https://pixeldrain.com/api/file/dt92pNgf/info",
            200,
            info("dt92pNgf", "image.png", "image/png", 84708).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://pixeldrain.com/api/file/s0ngs0ng/info",
            200,
            info("s0ngs0ng", "01 Holy Wars.mp3", "audio/mp3", 9000000).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://pixeldrain.com/api/file/z1pz1pz1/info",
            200,
            info(
                "z1pz1pz1",
                "Unity-debugging-5.x.zip",
                "application/zip",
                96309913,
            )
            .to_string(),
        ));
        let resolver = PixeldrainResolver::new(Http::replay(fixture));
        let resolve = |link: &str| {
            let url = Url::parse(link).unwrap();
            let resolver = &resolver;
            async move { resolver.resolve(&url).await.unwrap().media().unwrap() }
        };
        let video = resolve("https://pixeldrain.com/u/VFjemoqC").await;
        assert_eq!(video.media, MediaKind::Video);
        assert_eq!(video.id.as_deref(), Some("VFjemoqC"));
        assert_eq!(video.title.as_deref(), Some("youtube-dl_test_video"));
        assert!(video.uploaded_at.is_some());
        assert!(video.thumbnail.is_some());
        assert_eq!(
            video.webpage_url.as_ref().unwrap().as_str(),
            "https://pixeldrain.com/u/VFjemoqC"
        );
        assert_eq!(video.variants.len(), 1);
        assert_eq!(
            video.variants[0].url.as_str(),
            "https://pixeldrain.com/api/file/VFjemoqC?download"
        );
        assert_eq!(video.variants[0].container, Some(Container::Mp4));
        assert_eq!(video.variants[0].video, Some(VideoCodec::H264));
        assert_eq!(video.variants[0].size, Some(1828366));
        let image = resolve("https://pixeldrain.com/u/dt92pNgf").await;
        assert_eq!(image.media, MediaKind::Image);
        assert_eq!(image.variants[0].container, Some(Container::Png));
        let audio = resolve("https://pixeldrain.com/u/s0ngs0ng").await;
        assert_eq!(audio.media, MediaKind::Audio);
        assert_eq!(audio.title.as_deref(), Some("01 Holy Wars"));
        assert!(audio.variants[0].audio_only);
        assert_eq!(audio.variants[0].audio, Some(AudioCodec::Mp3));
        assert!(audio.thumbnail.is_none());
        let archive = resolve("https://pixeldrain.com/u/z1pz1pz1").await;
        assert_eq!(archive.media, MediaKind::File);
        assert_eq!(
            archive.variants[0].container,
            Some(Container::Other("zip".into()))
        );
        assert_eq!(archive.variants[0].size, Some(96309913));
    }

    #[tokio::test]
    async fn lists_and_shares_resolve_to_their_files_and_missing_ones_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://pixeldrain.com/api/list/6ceqkTSn",
            200,
            json!({"success": true, "id": "6ceqkTSn", "title": "dnspy mono builds", "date_created": "2021-02-24T10:00:00Z",
                "file_count": 2, "can_edit": false, "files": [
                {"detail_href": "/file/aaaaaaaa/info", "id": "aaaaaaaa", "name": "Unity-debugging-2017.x.zip", "size": 221885189, "mime_type": "application/zip"},
                {"detail_href": "/file/bbbbbbbb/info", "id": "bbbbbbbb", "name": "Unity-debugging-2018.x.zip", "size": 268408857, "mime_type": "application/zip"}
            ]}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://pixeldrain.com/api/filesystem/qTnZkhCJ?stat",
            200,
            json!({"path": [{"type": "dir", "path": "/qTnZkhCJ", "name": "g-dl-dir-with-subdir-and-files", "created": "2025-05-20T19:00:09.617Z", "file_size": 0, "file_type": "", "id": "qTnZkhCJ"}],
                "base_index": 0, "children": [
                {"type": "file", "path": "/qTnZkhCJ/.search_index.gz", "name": ".search_index.gz", "file_size": 56, "file_type": "application/gzip"},
                {"type": "dir", "path": "/qTnZkhCJ/subdir", "name": "subdir", "file_size": 0, "file_type": ""},
                {"type": "file", "path": "/qTnZkhCJ/test1.mp4", "name": "test1.mp4", "file_size": 3026, "file_type": "video/mp4"},
                {"type": "file", "path": "/qTnZkhCJ/test 2.mp4", "name": "test 2.mp4", "file_size": 3026, "file_type": "video/mp4"}
            ], "permissions": {"owner": false, "read": true, "write": false, "delete": false}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://pixeldrain.com/api/filesystem/qTnZkhCJ/test%202.mp4?stat",
            200,
            json!({"path": [{"type": "dir", "path": "/qTnZkhCJ", "name": "g-dl-dir-with-subdir-and-files", "file_size": 0, "file_type": "", "id": "qTnZkhCJ"},
                {"type": "file", "path": "/qTnZkhCJ/test 2.mp4", "name": "test 2.mp4", "created": "2025-05-19T15:27:54.851Z", "file_size": 3026, "file_type": "video/mp4", "id": "8xz8hcYJ"}],
                "base_index": 0, "children": [], "permissions": {"owner": false, "read": true, "write": false, "delete": false}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://pixeldrain.com/api/file/g0neg0ne/info",
            404,
            json!({"success": false, "value": "not_found", "message": "The entity you requested could not be found"}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://pixeldrain.com/api/file/t4ken0ff/info",
            200,
            json!({"success": true, "id": "t4ken0ff", "name": "x.mp4", "size": 5, "mime_type": "video/mp4",
                "availability": "unavailable_for_legal_reasons", "availability_message": "This file has received a takedown report", "can_download": false}).to_string(),
        ));
        let resolver = PixeldrainResolver::new(Http::replay(fixture));
        let Resolution::Playlist(list) = resolver
            .resolve(&Url::parse("https://pixeldrain.com/l/6ceqkTSn").unwrap())
            .await
            .unwrap()
        else {
            panic!("a list is a playlist");
        };
        assert_eq!(list.title.as_deref(), Some("dnspy mono builds"));
        assert_eq!(list.total, Some(2));
        assert_eq!(list.entries.len(), 2);
        assert_eq!(
            list.entries[0].url.as_str(),
            "https://pixeldrain.com/u/aaaaaaaa"
        );
        assert_eq!(
            list.entries[1].title.as_deref(),
            Some("Unity-debugging-2018.x.zip")
        );

        let Resolution::Playlist(dir) = resolver
            .resolve(&Url::parse("https://pixeldrain.com/d/qTnZkhCJ").unwrap())
            .await
            .unwrap()
        else {
            panic!("a directory is a playlist");
        };
        assert_eq!(dir.title.as_deref(), Some("g-dl-dir-with-subdir-and-files"));
        assert_eq!(dir.entries.len(), 3, "the index file is left out");
        assert_eq!(
            dir.entries[0].url.as_str(),
            "https://pixeldrain.com/d/qTnZkhCJ/subdir"
        );
        assert_eq!(
            dir.entries[2].url.as_str(),
            "https://pixeldrain.com/d/qTnZkhCJ/test%202.mp4"
        );
        let file = resolver
            .resolve(&dir.entries[2].url)
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(file.media, MediaKind::Video);
        assert_eq!(file.id.as_deref(), Some("8xz8hcYJ"));
        assert_eq!(file.title.as_deref(), Some("test 2"));
        assert_eq!(
            file.variants[0].url.as_str(),
            "https://pixeldrain.com/api/filesystem/qTnZkhCJ/test%202.mp4"
        );
        assert_eq!(file.variants[0].size, Some(3026));
        assert!(file.uploaded_at.is_some());

        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://pixeldrain.com/u/g0neg0ne").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(&Url::parse("https://pixeldrain.com/u/t4ken0ff").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("takedown")),
            "{error}"
        );
    }

    /// Every example link resolves live: files of their kinds, the list and the share
    /// directories with entries.
    #[ignore = "reaches the live site: cargo test -- --ignored"]
    #[tokio::test]

    async fn live_examples_resolve() {
        use std::time::Duration;

        let resolver = PixeldrainResolver::new(Http::new(crate::http::HttpConfig::default()));
        let mut kinds = Vec::new();
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
                    kinds.push((link.to_string(), resolved.media));
                }
                Resolution::Playlist(playlist) => {
                    assert!(!playlist.entries.is_empty(), "{link}: no entries");
                }
            }
        }
        assert!(kinds.contains(&(
            "https://pixeldrain.com/u/VFjemoqC".to_string(),
            MediaKind::Video
        )));
        assert!(kinds.contains(&(
            "https://pixeldrain.com/u/dt92pNgf".to_string(),
            MediaKind::Image
        )));
        assert!(kinds.contains(&(
            "https://pixeldrain.com/d/8xz8hcYJ".to_string(),
            MediaKind::Video
        )));
    }
}
