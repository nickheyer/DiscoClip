//! Google Drive files and folders: a file's streams from the player info the site's own
//! player reads, with the uploaded file itself from the download endpoint, and a folder's
//! videos as a playlist from the embeddable folder view.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use scraper::Selector;
use serde_json::Value;
use url::Url;

use super::page::Page;
use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, Variant, VariantKind, clean_title, essence, fetch, parse_codecs, probe_file,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "google_drive";
const VIDEO_INFO: &str = "https://drive.google.com/get_video_info";
const DOWNLOAD: &str = "https://drive.usercontent.google.com/download";
const FOLDER_VIEW: &str = "https://drive.google.com/embeddedfolderview";

static RE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]{10,}$").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    File(String),
    Folder(String),
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !matches!(
        host.as_str(),
        "drive.google.com" | "docs.google.com" | "drive.usercontent.google.com"
    ) {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let query_id = || {
        url.query_pairs()
            .find(|(k, _)| k == "id" || k == "docid")
            .map(|(_, v)| v.into_owned())
            .filter(|id| RE_ID.is_match(id))
    };
    // Account prefixes: /u/0/... and /a/domain/..., before or after /drive/.
    let stripped: Vec<&str> = match segments.as_slice() {
        ["u" | "a", _, rest @ ..] => rest.to_vec(),
        ["drive", "u" | "a", _, rest @ ..] => std::iter::once("drive")
            .chain(rest.iter().copied())
            .collect(),
        all => all.to_vec(),
    };
    match stripped.as_slice() {
        ["file", "d", id, ..] if RE_ID.is_match(id) => Some(Link::File(id.to_string())),
        ["drive", "folders", id, ..] | ["drive", "mobile", "folders", id, ..]
            if RE_ID.is_match(id) =>
        {
            Some(Link::Folder(id.to_string()))
        }
        ["embeddedfolderview"] | ["folderview"] | ["drive", "folders"] => {
            query_id().map(Link::Folder)
        }
        ["open"] | ["uc"] | ["download"] | ["get_video_info"] => query_id().map(Link::File),
        _ => None,
    }
}

/// The streams the player info names, from `player_response`, plus the legacy stream map.
pub fn stream_variants(fields: &[(String, String)]) -> Vec<Variant> {
    let field = |name: &str| {
        fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    let mut variants = Vec::new();
    let duration = field("length_seconds")
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs);
    if let Some(response) =
        field("player_response").and_then(|r| serde_json::from_str::<Value>(r).ok())
    {
        let streaming = &response["streamingData"];
        for (list, adaptive) in [("formats", false), ("adaptiveFormats", true)] {
            for format in streaming[list].as_array().into_iter().flatten() {
                let Some(url) = format["url"].as_str().and_then(|u| Url::parse(u).ok()) else {
                    continue;
                };
                let mime = format["mimeType"].as_str().unwrap_or_default();
                let essence_type = essence(Some(mime));
                let codecs = mime
                    .split(';')
                    .find_map(|p| p.trim().strip_prefix("codecs="))
                    .map(|c| c.trim_matches('"'));
                let (video, audio) = parse_codecs(codecs);
                let mut v = Variant::new(url, VariantKind::File);
                v.container = Container::from_mime(&essence_type).or(Some(Container::Mp4));
                v.video = video;
                v.audio = audio;
                v.width = format["width"].as_u64().map(|w| w as u32);
                v.height = format["height"].as_u64().map(|h| h as u32);
                v.fps = format["fps"].as_f64();
                v.bitrate = format["bitrate"].as_u64();
                v.size = format["contentLength"]
                    .as_str()
                    .and_then(|s| s.parse().ok());
                v.duration = format["approxDurationMs"]
                    .as_str()
                    .and_then(|ms| ms.parse::<u64>().ok())
                    .map(Duration::from_millis)
                    .or(duration);
                v.format_id = format["itag"].as_u64().map(|i| i.to_string());
                v.label = format["qualityLabel"].as_str().map(String::from);
                v.codecs = codecs.map(String::from);
                if adaptive {
                    v.video_only = essence_type.starts_with("video/");
                    v.audio_only = essence_type.starts_with("audio/");
                }
                variants.push(v);
            }
        }
    }
    if variants.is_empty()
        && let Some(map) = field("fmt_stream_map")
    {
        let sizes: Vec<(String, (u32, u32))> = field("fmt_list")
            .unwrap_or_default()
            .split(',')
            .filter_map(|entry| {
                let mut parts = entry.split('/');
                let itag = parts.next()?.to_string();
                let (w, h) = parts.next()?.split_once('x')?;
                Some((itag, (w.parse().ok()?, h.parse().ok()?)))
            })
            .collect();
        for entry in map.split(',') {
            let Some((itag, url)) = entry.split_once('|') else {
                continue;
            };
            let Ok(url) = Url::parse(url) else {
                continue;
            };
            let mut v = Variant::new(url, VariantKind::File);
            v.container = Some(Container::Mp4);
            v.video = Some(VideoCodec::H264);
            v.audio = Some(AudioCodec::Aac);
            if let Some((_, (w, h))) = sizes.iter().find(|(i, _)| i == itag) {
                v.width = Some(*w);
                v.height = Some(*h);
            }
            v.duration = duration;
            v.format_id = Some(itag.to_string());
            variants.push(v);
        }
    }
    variants
}

fn parse_fields(body: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(body.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

pub struct GoogleDriveResolver {
    http: Http,
}

impl GoogleDriveResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// The uploaded file itself, as the download endpoint serves it to anyone.
    async fn source_variant(
        &self,
        id: &str,
    ) -> Result<Option<(Variant, Option<String>)>, ResolveError> {
        let mut url = Url::parse(DOWNLOAD).expect("valid");
        url.query_pairs_mut()
            .append_pair("id", id)
            .append_pair("export", "download")
            .append_pair("confirm", "t");
        let probed = probe_file(&self.http, &url, PLATFORM, BROWSER_UA, &[]).await?;
        if !matches!(probed.status.as_u16(), 200 | 206) {
            return Ok(None);
        }
        let content_type = essence(probed.content_type.as_deref());
        if content_type == "text/html" {
            return Ok(None);
        }
        let name = probed.filename.clone();
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Container::from_mime(&content_type).or_else(|| {
            name.as_deref()
                .and_then(|n| n.rsplit('.').next())
                .and_then(Container::from_extension)
        });
        if v.container.is_none() && !content_type.starts_with("video/") {
            return Ok(None);
        }
        v.size = probed.size;
        v.format_id = Some("source".into());
        v.label = Some("original upload".into());
        Ok(Some((v, name)))
    }

    async fn file(&self, id: &str, origin: &Url) -> Result<Resolved, ResolveError> {
        let mut info_url = Url::parse(VIDEO_INFO).expect("valid");
        info_url.query_pairs_mut().append_pair("docid", id);
        let fetched = fetch(&self.http, &info_url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        match fetched.status.as_u16() {
            200..=299 => {}
            404 | 410 => return Err(ResolveError::NotFound(origin.clone())),
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the player info answered HTTP {status}"),
                ));
            }
        }
        let fields = parse_fields(&fetched.text());
        let field = |name: &str| {
            fields
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        };
        let mut variants = stream_variants(&fields);
        let mut title = field("title").and_then(|t| clean_title(&t));
        if let Some((source, name)) = self.source_variant(id).await? {
            if title.is_none() {
                title = name.as_deref().and_then(clean_title);
            }
            variants.insert(0, source);
        }
        if variants.is_empty() {
            let reason = field("reason").unwrap_or_default();
            let lower = reason.to_ascii_lowercase();
            let missing = reason.is_empty()
                || lower.contains("not found")
                || lower.contains("does not exist")
                || (field("status").as_deref() == Some("fail")
                    && field("errorcode").as_deref() == Some("100"));
            return Err(
                if lower.contains("sign in")
                    || lower.contains("permission")
                    || lower.contains("access")
                {
                    ResolveError::login_required(origin, PLATFORM, reason)
                } else if missing {
                    ResolveError::NotFound(origin.clone())
                } else {
                    ResolveError::unavailable(origin, reason)
                },
            );
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.to_string());
        resolved.title =
            title.map(|t| t.rsplit_once('.').map_or(t.clone(), |(s, _)| s.to_string()));
        resolved.duration = field("length_seconds")
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|s| *s > 0)
            .map(Duration::from_secs)
            .or_else(|| variants.iter().find_map(|v| v.duration));
        resolved.thumbnail = field("iurl").and_then(|u| Url::parse(&u).ok());
        resolved.webpage_url =
            Url::parse(&format!("https://drive.google.com/file/d/{id}/view")).ok();
        resolved.variants = variants;
        Ok(resolved)
    }

    async fn folder(&self, id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let mut view = Url::parse(FOLDER_VIEW).expect("valid");
        view.query_pairs_mut().append_pair("id", id);
        let fetched = fetch(&self.http, &view, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        match fetched.status.as_u16() {
            200..=299 => {}
            404 | 410 => return Err(ResolveError::NotFound(origin.clone())),
            403 => {
                return Err(ResolveError::login_required(
                    origin,
                    PLATFORM,
                    "the folder is not shared with everyone",
                ));
            }
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the folder view answered HTTP {status}"),
                ));
            }
        }
        let html = fetched.text();
        let page = Page::parse(&html, &view);
        let entries_selector = Selector::parse(".flip-entry").expect("valid");
        let link_selector = Selector::parse("a[href]").expect("valid");
        let title_selector = Selector::parse(".flip-entry-title").expect("valid");
        let icon_selector = Selector::parse(".flip-entry-list-icon img").expect("valid");
        let mut entries = Vec::new();
        for entry in page.document().select(&entries_selector) {
            let Some(link) = entry
                .select(&link_selector)
                .find_map(|a| a.value().attr("href"))
                .and_then(|h| Url::parse(h).ok())
            else {
                continue;
            };
            let Some(Link::File(file_id)) = parse_link(&link) else {
                continue;
            };
            let name = entry
                .select(&title_selector)
                .next()
                .map(|t| t.text().collect::<String>())
                .and_then(|t| clean_title(&t));
            let is_video = entry.select(&icon_selector).any(|img| {
                img.value()
                    .attr("src")
                    .is_some_and(|s| s.contains("/type/video/"))
            }) || name
                .as_deref()
                .and_then(|n| n.rsplit('.').next())
                .and_then(Container::from_extension)
                .is_some();
            if !is_video {
                continue;
            }
            entries.push(PlaylistEntry {
                url: Url::parse(&format!("https://drive.google.com/file/d/{file_id}/view"))
                    .expect("valid"),
                title: name.map(|n| n.rsplit_once('.').map_or(n.clone(), |(s, _)| s.to_string())),
                duration: None,
            });
        }
        if entries.is_empty() {
            return Err(if html.contains("flip-entries") {
                ResolveError::unavailable(origin, "the folder holds no videos")
            } else {
                ResolveError::NotFound(origin.clone())
            });
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.into(),
            id: Some(id.to_string()),
            title: page.title(),
            total: Some(entries.len()),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for GoogleDriveResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Google Drive",
            hosts: &[
                "drive.google.com",
                "docs.google.com",
                "drive.usercontent.google.com",
            ],
            features: &[
                "files",
                "folders",
                "open and uc links",
                "original uploads",
                "player streams",
            ],
            formats: &["mp4", "webm", "mkv", "mov"],
            session: SessionSupport::Optional,
            examples: &[
                "https://drive.google.com/file/d/0ByeS4oOUV-49Zzh4R1J6R09zazQ/view",
                "https://drive.google.com/drive/folders/1brGvAKJB4PY_CClqqRVh31Kr8fY6cTLs",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::File(id) => Ok(Resolution::from(self.file(&id, url).await?)),
            Link::Folder(id) => self.folder(&id, url).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

    fn get(
        url: &str,
        status: u16,
        content_type: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> Exchange {
        let mut all = vec![("content-type".to_string(), content_type.to_string())];
        all.extend(headers.iter().map(|(k, v)| (k.to_string(), v.to_string())));
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
                headers: all,
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    const ID: &str = "0ByeS4oOUV-49Zzh4R1J6R09zazQ";

    fn info() -> String {
        let response = r#"{"streamingData":{"formats":[{"itag":18,"url":"https://rr1.c.drive.google.com/videoplayback?itag=18","mimeType":"video/mp4; codecs=\"avc1.4D001F\"","width":640,"height":360,"contentLength":"2644926","bitrate":469864,"fps":30,"qualityLabel":"360p","approxDurationMs":"45116","audioQuality":"AUDIO_QUALITY_LOW"},{"itag":22,"url":"https://rr1.c.drive.google.com/videoplayback?itag=22","mimeType":"video/mp4; codecs=\"avc1.640029\"","width":1280,"height":718,"contentLength":"10295612","bitrate":1828989,"fps":30,"qualityLabel":"720p","approxDurationMs":"45116"}],"adaptiveFormats":[{"itag":137,"url":"https://rr1.c.drive.google.com/videoplayback?itag=137","mimeType":"video/mp4; codecs=\"avc1.42C028\"","width":1920,"height":1078,"contentLength":"17672836","bitrate":4913615,"fps":30,"qualityLabel":"1080p"},{"itag":140,"url":"https://rr1.c.drive.google.com/videoplayback?itag=140","mimeType":"audio/mp4; codecs=\"mp4a.40.2\"","contentLength":"731379","bitrate":130444}]}}"#;
        format!(
            "status=ok&docid={ID}&title=Big+Buck+Bunny.mp4&iurl=https%3A%2F%2Flh3.googleusercontent.com%2Fdrive-storage%2Fx%3Ds512&length_seconds=45&player_response={}",
            url::form_urlencoded::byte_serialize(response.as_bytes()).collect::<String>()
        )
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://drive.google.com/file/d/0ByeS4oOUV-49Zzh4R1J6R09zazQ/view?usp=sharing"),
            Some(Link::File(ID.into()))
        );
        assert_eq!(
            link("https://drive.google.com/file/d/0ByeS4oOUV-49Zzh4R1J6R09zazQ/edit"),
            Some(Link::File(ID.into()))
        );
        assert_eq!(
            link("https://drive.google.com/u/0/file/d/0ByeS4oOUV-49Zzh4R1J6R09zazQ/preview"),
            Some(Link::File(ID.into()))
        );
        assert_eq!(
            link("https://drive.google.com/open?id=0ByeS4oOUV-49Zzh4R1J6R09zazQ"),
            Some(Link::File(ID.into()))
        );
        assert_eq!(
            link("https://docs.google.com/uc?id=0ByeS4oOUV-49Zzh4R1J6R09zazQ&export=download"),
            Some(Link::File(ID.into()))
        );
        assert_eq!(
            link(
                "https://drive.usercontent.google.com/download?id=0ByeS4oOUV-49Zzh4R1J6R09zazQ&export=download"
            ),
            Some(Link::File(ID.into()))
        );
        assert_eq!(
            link(
                "https://drive.google.com/drive/folders/1brGvAKJB4PY_CClqqRVh31Kr8fY6cTLs?usp=sharing"
            ),
            Some(Link::Folder("1brGvAKJB4PY_CClqqRVh31Kr8fY6cTLs".into()))
        );
        assert_eq!(
            link("https://drive.google.com/drive/u/1/folders/1brGvAKJB4PY_CClqqRVh31Kr8fY6cTLs"),
            Some(Link::Folder("1brGvAKJB4PY_CClqqRVh31Kr8fY6cTLs".into()))
        );
        assert_eq!(
            link(
                "https://drive.google.com/embeddedfolderview?id=1brGvAKJB4PY_CClqqRVh31Kr8fY6cTLs"
            ),
            Some(Link::Folder("1brGvAKJB4PY_CClqqRVh31Kr8fY6cTLs".into()))
        );
        assert_eq!(link("https://drive.google.com/drive/my-drive"), None);
        assert_eq!(
            link("https://docs.google.com/document/d/abcdefghijklmnop/edit"),
            None
        );
    }

    #[tokio::test]
    async fn files_resolve_with_the_original_upload_and_the_player_streams() {
        let mut fixture = Fixture::new("google_drive", None);
        fixture.exchanges.push(get(
            &format!("https://drive.google.com/get_video_info?docid={ID}"),
            200,
            "text/plain",
            &info(),
            &[],
        ));
        fixture.exchanges.push(get(
            &format!(
                "https://drive.usercontent.google.com/download?id={ID}&export=download&confirm=t"
            ),
            200,
            "video/mp4",
            "",
            &[
                ("content-length", "185972864"),
                (
                    "content-disposition",
                    "attachment; filename=\"Big Buck Bunny.mp4\"",
                ),
            ],
        ));
        let resolver = GoogleDriveResolver::new(Http::replay(fixture));
        let url = Url::parse(&format!("https://drive.google.com/file/d/{ID}/view")).unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.title.as_deref(), Some("Big Buck Bunny"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(45)));
        assert!(resolved.thumbnail.is_some());
        assert_eq!(resolved.variants.len(), 5);
        let source = &resolved.variants[0];
        assert_eq!(source.format_id.as_deref(), Some("source"));
        assert_eq!(source.size, Some(185972864));
        assert_eq!(source.container, Some(Container::Mp4));
        let hd = resolved
            .variants
            .iter()
            .find(|v| v.format_id.as_deref() == Some("22"))
            .unwrap();
        assert_eq!(hd.height, Some(718));
        assert_eq!(hd.video, Some(VideoCodec::H264));
        assert_eq!(hd.size, Some(10295612));
        assert_eq!(hd.duration, Some(Duration::from_millis(45116)));
        assert!(!hd.video_only);
        let video_only = resolved
            .variants
            .iter()
            .find(|v| v.format_id.as_deref() == Some("137"))
            .unwrap();
        assert!(video_only.video_only);
        let audio = resolved
            .variants
            .iter()
            .find(|v| v.format_id.as_deref() == Some("140"))
            .unwrap();
        assert!(audio.audio_only);
        assert_eq!(audio.audio, Some(AudioCodec::Aac));
    }

    #[tokio::test]
    async fn locked_and_missing_files_say_so() {
        let mut fixture = Fixture::new("google_drive", None);
        fixture.exchanges.push(get(
            "https://drive.google.com/get_video_info?docid=lockedlockedlocked",
            200,
            "text/plain",
            "status=fail&errorcode=150&reason=You+need+permission+to+access+this+file",
            &[],
        ));
        fixture.exchanges.push(get("https://drive.usercontent.google.com/download?id=lockedlockedlocked&export=download&confirm=t", 200, "text/html", "<html>sign in</html>", &[]));
        fixture.exchanges.push(get(
            "https://drive.google.com/get_video_info?docid=missingmissingmissing",
            200,
            "text/plain",
            "status=fail&errorcode=100&reason=Video+not+found",
            &[],
        ));
        fixture.exchanges.push(get("https://drive.usercontent.google.com/download?id=missingmissingmissing&export=download&confirm=t", 404, "text/html", "", &[]));
        let resolver = GoogleDriveResolver::new(Http::replay(fixture));
        assert!(matches!(
            resolver
                .resolve(
                    &Url::parse("https://drive.google.com/file/d/lockedlockedlocked/view").unwrap()
                )
                .await
                .unwrap_err(),
            ResolveError::LoginRequired { .. }
        ));
        assert!(matches!(
            resolver
                .resolve(
                    &Url::parse("https://drive.google.com/file/d/missingmissingmissing/view")
                        .unwrap()
                )
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn folders_list_their_videos() {
        let view = r#"<html><head><title>Sample Videos</title></head><body class="flip-embedded"><div class="flip-entries">
        <div class="flip-entry" id="entry-1fDBH_uBH-LzJE3YiXq0y1HDeSBA9GcHi"><div class="flip-entry-info"><a href="https://drive.google.com/file/d/1fDBH_uBH-LzJE3YiXq0y1HDeSBA9GcHi/view?usp=drive_web" target="_blank"><div class="flip-entry-list-icon"><img src="https://drive-thirdparty.googleusercontent.com/16/type/video/mp4" alt=""/></div><div class="flip-entry-title">Pepsi Ad Creative 2.mp4</div></a></div></div>
        <div class="flip-entry" id="entry-1hnPJQjDQPLH11pKLz1f86j96YJdhrMZo"><div class="flip-entry-info"><a href="https://drive.google.com/file/d/1hnPJQjDQPLH11pKLz1f86j96YJdhrMZo/view?usp=drive_web" target="_blank"><div class="flip-entry-list-icon"><img src="https://drive-thirdparty.googleusercontent.com/16/type/video/mp4" alt=""/></div><div class="flip-entry-title">Pepsi Ad Creative.mp4</div></a></div></div>
        <div class="flip-entry" id="entry-1zzzzzzzzzzzzzzzzzzzzzzzzzzz"><div class="flip-entry-info"><a href="https://drive.google.com/file/d/1zzzzzzzzzzzzzzzzzzzzzzzzzzz/view" target="_blank"><div class="flip-entry-list-icon"><img src="https://drive-thirdparty.googleusercontent.com/16/type/application/pdf" alt=""/></div><div class="flip-entry-title">notes.pdf</div></a></div></div>
        </div></body></html>"#;
        let mut fixture = Fixture::new("google_drive", None);
        fixture.exchanges.push(get(
            "https://drive.google.com/embeddedfolderview?id=1brGvAKJB4PY_CClqqRVh31Kr8fY6cTLs",
            200,
            "text/html",
            view,
            &[],
        ));
        let resolver = GoogleDriveResolver::new(Http::replay(fixture));
        let playlist = match resolver
            .resolve(
                &Url::parse(
                    "https://drive.google.com/drive/folders/1brGvAKJB4PY_CClqqRVh31Kr8fY6cTLs",
                )
                .unwrap(),
            )
            .await
            .unwrap()
        {
            Resolution::Playlist(p) => p,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(playlist.title.as_deref(), Some("Sample Videos"));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://drive.google.com/file/d/1fDBH_uBH-LzJE3YiXq0y1HDeSBA9GcHi/view"
        );
        assert_eq!(
            playlist.entries[0].title.as_deref(),
            Some("Pepsi Ad Creative 2")
        );
    }
}
