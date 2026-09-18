//! Catbox files and albums: a file on the hosts by its name, probed for its length and
//! told apart as a video, audio, an image or another file by what the host serves it as
//! and by its name, and an album's files as a playlist.

use async_trait::async_trait;
use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use scraper::Selector;
use url::Url;

use super::page::Page;
use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, Tag, Variant, VariantKind, clean_title, essence, fetch, probe_file,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "catbox";
const FILE_HOSTS: [&str; 4] = [
    "files.catbox.moe",
    "litter.catbox.moe",
    "de.catbox.moe",
    "litterbox.catbox.moe",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    File(Url),
    Album(String),
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    if FILE_HOSTS.contains(&host.as_str()) {
        return match segments.as_slice() {
            [name] if name.contains('.') => Some(Link::File(url.clone())),
            _ => None,
        };
    }
    if host == "catbox.moe" || host == "www.catbox.moe" {
        return match segments.as_slice() {
            ["c", album] if album.chars().all(|c| c.is_ascii_alphanumeric()) => {
                Some(Link::Album(album.to_string()))
            }
            _ => None,
        };
    }
    None
}

/// What a file is and the format it is in: from the content type the host serves it as
/// when that names a format, else from the file's own name, else from the broad type
/// the host serves (`image/…`, `audio/…`, `video/…`). The host serves anything it does
/// not know as `application/octet-stream`, so the name outranks that.
pub fn classify(name: &str, content_type: &str) -> (MediaKind, Option<Container>) {
    if let Some(container) = Container::from_mime(content_type) {
        return (container.kind(), Some(container));
    }
    if let Some(container) = Container::from_name(name) {
        return (container.kind(), Some(container));
    }
    (MediaKind::from_mime(content_type), None)
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
fn file_variant(
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
    v
}

fn stem(name: &str) -> String {
    name.rsplit_once('.').map_or(name, |(s, _)| s).to_string()
}

fn parse_created(text: &str) -> Option<Timestamp> {
    let text = text.trim().strip_prefix("Created")?.trim();
    Date::strptime("%B %d %Y", text)
        .ok()
        .and_then(|d| d.to_zoned(TimeZone::UTC).ok())
        .map(|z| z.timestamp())
}

pub struct CatboxResolver {
    http: Http,
}

impl CatboxResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    async fn file(&self, file: &Url, origin: &Url) -> Result<Resolved, ResolveError> {
        let name = percent_encoding::percent_decode_str(file.path().trim_start_matches('/'))
            .decode_utf8_lossy()
            .into_owned();
        let probed = probe_file(&self.http, file, PLATFORM, BROWSER_UA, &[]).await?;
        match probed.status.as_u16() {
            200 | 206 => {}
            404 | 410 => return Err(ResolveError::NotFound(origin.clone())),
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the host answered HTTP {status}"),
                ));
            }
        }
        // The host answers a missing file with a page rather than a 404 at times.
        let served = essence(probed.content_type.as_deref());
        if served == "text/html" {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        let (kind, container) = classify(&name, &served);
        let mut resolved = Resolved::of(PLATFORM, kind);
        resolved.id = Some(stem(&name));
        resolved.title = resolved.id.clone();
        resolved.webpage_url = Some(file.clone());
        resolved.variants = vec![file_variant(file.clone(), kind, container, probed.size)];
        Ok(resolved)
    }

    async fn album(&self, album: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let page_url = Url::parse(&format!("https://catbox.moe/c/{album}")).expect("valid");
        let fetched = fetch(&self.http, &page_url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        match fetched.status.as_u16() {
            200..=299 => {}
            404 | 410 => return Err(ResolveError::NotFound(origin.clone())),
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the album page answered HTTP {status}"),
                ));
            }
        }
        let html = fetched.text();
        // The page's document lives on one thread; everything is read from it before the
        // future crosses threads again.
        let (title, created, entries) = {
            let page = Page::parse(&html, &page_url);
            let title = page
                .document()
                .select(&Selector::parse(".title h1").expect("valid"))
                .next()
                .and_then(|h| clean_title(&h.text().collect::<String>()));
            let created = page
                .document()
                .select(&Selector::parse(".title p").expect("valid"))
                .find_map(|p| parse_created(&p.text().collect::<String>()));
            let links =
                Selector::parse(".imagecontainer a[href], .imagelist a[href]").expect("valid");
            let mut entries: Vec<PlaylistEntry> = Vec::new();
            for anchor in page.document().select(&links) {
                let Some(file) = anchor
                    .value()
                    .attr("href")
                    .and_then(|h| page_url.join(h).ok())
                else {
                    continue;
                };
                if !matches!(parse_link(&file), Some(Link::File(_))) {
                    continue;
                }
                let name = file.path().trim_start_matches('/').to_string();
                if entries.iter().any(|e| e.url == file) {
                    continue;
                }
                entries.push(PlaylistEntry {
                    url: file,
                    title: Some(stem(&name)),
                    duration: None,
                });
            }
            (title, created, entries)
        };
        if entries.is_empty() {
            return Err(if title.is_none() && !html.contains("imagecontainer") {
                ResolveError::NotFound(origin.clone())
            } else {
                ResolveError::unavailable(origin, "the album holds no files")
            });
        }
        if entries.len() == 1 {
            let mut resolved = self.file(&entries[0].url, origin).await?;
            if let Some(title) = title {
                resolved.title = Some(title);
            }
            resolved.uploaded_at = created;
            resolved.webpage_url = Some(page_url);
            return Ok(Resolution::from(resolved));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.into(),
            id: Some(album.to_string()),
            title,
            total: Some(entries.len()),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for CatboxResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Catbox",
            hosts: &[
                "catbox.moe",
                "files.catbox.moe",
                "de.catbox.moe",
                "litter.catbox.moe",
                "litterbox.catbox.moe",
            ],
            features: &[
                "files",
                "litterbox files",
                "albums",
                "audio",
                "images",
                "any file",
            ],
            formats: &[
                "mp4", "webm", "mkv", "mov", "gif", "mp3", "ogg", "flac", "jpg", "png", "webp",
                "zip", "pdf",
            ],
            media: &[
                MediaKind::Video,
                MediaKind::Audio,
                MediaKind::Image,
                MediaKind::File,
            ],
            tags: &[Tag::Files, Tag::Images],
            session: SessionSupport::None,
            examples: &[
                "https://files.catbox.moe/safuz8.mp4",
                "https://files.catbox.moe/00koca.jpg",
                "https://files.catbox.moe/53103j.zip",
                "https://catbox.moe/c/8xw6g4",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::File(file) => Ok(Resolution::from(self.file(&file, url).await?)),
            Link::Album(album) => self.album(&album, url).await,
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

    const ALBUM: &str = r#"<html><head><title>Catbox Collection</title></head><body><div class="title"><h1>blender_dump</h1><p>Created March 19 2022</p><p></p></div>
    <div class="imagecontainer"><a href='https://files.catbox.moe/00koca.jpg' data-fancybox='gallery'><img src='https://files.catbox.moe/thumbs/t_00koca.jpg'></a><a href='https://files.catbox.moe/s2dd6o.gif' data-fancybox='gallery'><img src='https://files.catbox.moe/s2dd6o.gif'></a><a href='https://files.catbox.moe/clip42.mp4' data-fancybox='gallery'><img src='x'></a></div>
    <div class="imagelist" style="display: none;"><a href='https://files.catbox.moe/00koca.jpg' target='_blank'>https://files.catbox.moe/00koca.jpg</a><br></div></body></html>"#;

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert!(matches!(
            link("https://files.catbox.moe/safuz8.mp4"),
            Some(Link::File(_))
        ));
        assert!(matches!(
            link("https://litter.catbox.moe/t8v3n9.webm"),
            Some(Link::File(_))
        ));
        assert_eq!(
            link("https://catbox.moe/c/8xw6g4"),
            Some(Link::Album("8xw6g4".into()))
        );
        assert_eq!(link("https://catbox.moe/faq.php"), None);
        assert_eq!(link("https://files.catbox.moe/"), None);
    }

    #[tokio::test]
    async fn files_are_probed_for_their_length() {
        let mut fixture = Fixture::new("catbox", None);
        fixture.exchanges.push(get(
            "https://files.catbox.moe/safuz8.mp4",
            206,
            "video/mp4",
            "",
            &[("content-range", "bytes 0-0/7971211")],
        ));
        fixture.exchanges.push(get(
            "https://files.catbox.moe/zzzzzz.mp4",
            404,
            "text/html; charset=UTF-8",
            "<html>404</html>",
            &[],
        ));
        let resolver = CatboxResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://files.catbox.moe/safuz8.mp4").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.title.as_deref(), Some("safuz8"));
        assert_eq!(resolved.media, MediaKind::Video);
        assert_eq!(resolved.variants[0].size, Some(7971211));
        assert_eq!(resolved.variants[0].container, Some(Container::Mp4));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://files.catbox.moe/zzzzzz.mp4").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    /// The JPEG, ZIP and Markdown exchanges were recorded from the host on 2026-09-18,
    /// which serves a type it does not know as `application/octet-stream`; the FLAC
    /// exchange has the same shape.
    #[tokio::test]
    async fn images_audio_and_other_files_are_told_apart() {
        let mut fixture = Fixture::new("catbox", None);
        fixture.exchanges.push(get(
            "https://files.catbox.moe/00koca.jpg",
            206,
            "image/jpeg",
            "",
            &[("content-range", "bytes 0-0/2401473")],
        ));
        fixture.exchanges.push(get(
            "https://files.catbox.moe/k2ln8a.flac",
            206,
            "application/octet-stream",
            "",
            &[("content-range", "bytes 0-0/30412201")],
        ));
        fixture.exchanges.push(get(
            "https://files.catbox.moe/53103j.zip",
            206,
            "application/zip",
            "",
            &[("content-range", "bytes 0-0/6376")],
        ));
        fixture.exchanges.push(get(
            "https://files.catbox.moe/mlzoua.md",
            206,
            "application/octet-stream",
            "",
            &[("content-range", "bytes 0-0/38552")],
        ));
        let resolver = CatboxResolver::new(Http::replay(fixture));
        let resolve = |link: &str| {
            let url = Url::parse(link).unwrap();
            let resolver = &resolver;
            async move { resolver.resolve(&url).await.unwrap().media().unwrap() }
        };
        let image = resolve("https://files.catbox.moe/00koca.jpg").await;
        assert_eq!(image.media, MediaKind::Image);
        assert_eq!(image.title.as_deref(), Some("00koca"));
        assert_eq!(image.variants[0].container, Some(Container::Jpeg));
        assert_eq!(image.variants[0].size, Some(2401473));
        assert!(!image.variants[0].audio_only);
        let audio = resolve("https://files.catbox.moe/k2ln8a.flac").await;
        assert_eq!(audio.media, MediaKind::Audio);
        assert_eq!(audio.variants[0].container, Some(Container::Flac));
        assert_eq!(audio.variants[0].audio, Some(AudioCodec::Flac));
        assert!(audio.variants[0].audio_only);
        assert_eq!(audio.variants[0].size, Some(30412201));
        let archive = resolve("https://files.catbox.moe/53103j.zip").await;
        assert_eq!(archive.media, MediaKind::File);
        assert_eq!(
            archive.variants[0].container,
            Some(Container::Other("zip".into()))
        );
        assert_eq!(archive.variants[0].size, Some(6376));
        let notes = resolve("https://files.catbox.moe/mlzoua.md").await;
        assert_eq!(notes.media, MediaKind::File);
        assert_eq!(
            notes.variants[0].container,
            Some(Container::Other("md".into()))
        );
        assert_eq!(notes.variants[0].size, Some(38552));
        assert_eq!(
            classify("clip.webm", "application/octet-stream"),
            (MediaKind::Video, Some(Container::Webm))
        );
        assert_eq!(
            classify("shot", "image/x-portable-pixmap"),
            (MediaKind::Image, None)
        );
    }

    #[tokio::test]
    async fn albums_list_their_files() {
        let mut fixture = Fixture::new("catbox", None);
        fixture.exchanges.push(get(
            "https://catbox.moe/c/8xw6g4",
            200,
            "text/html; charset=UTF-8",
            ALBUM,
            &[],
        ));
        fixture.exchanges.push(get("https://catbox.moe/c/zeara1", 200, "text/html; charset=UTF-8", r#"<html><body><div class="title"><h1>Six</h1></div><div class="imagecontainer"><a href='https://files.catbox.moe/3bs9a6.png'><img src='x'></a></div></body></html>"#, &[]));
        fixture.exchanges.push(get(
            "https://files.catbox.moe/3bs9a6.png",
            206,
            "image/png",
            "",
            &[("content-range", "bytes 0-0/51200")],
        ));
        fixture.exchanges.push(get("https://catbox.moe/c/empty1", 200, "text/html; charset=UTF-8", r#"<html><body><div class="title"><h1>Empty</h1></div><div class="imagecontainer"></div></body></html>"#, &[]));
        let resolver = CatboxResolver::new(Http::replay(fixture));
        let playlist = match resolver
            .resolve(&Url::parse("https://catbox.moe/c/8xw6g4").unwrap())
            .await
            .unwrap()
        {
            Resolution::Playlist(p) => p,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(playlist.title.as_deref(), Some("blender_dump"));
        assert_eq!(playlist.entries.len(), 3);
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://files.catbox.moe/00koca.jpg"
        );
        assert_eq!(playlist.entries[0].title.as_deref(), Some("00koca"));
        assert_eq!(
            playlist.entries[1].url.as_str(),
            "https://files.catbox.moe/s2dd6o.gif"
        );
        assert_eq!(playlist.entries[2].title.as_deref(), Some("clip42"));
        // An album of one file resolves that file, an image here, under the album's name.
        let single = resolver
            .resolve(&Url::parse("https://catbox.moe/c/zeara1").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(single.media, MediaKind::Image);
        assert_eq!(single.title.as_deref(), Some("Six"));
        assert_eq!(single.variants[0].container, Some(Container::Png));
        let error = resolver
            .resolve(&Url::parse("https://catbox.moe/c/empty1").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("no files")),
            "{error}"
        );
        assert_eq!(
            parse_created("Created March 19 2022").unwrap().to_string(),
            "2022-03-19T00:00:00Z"
        );
    }

    /// Every example link resolves live, and the image and the archive among them come
    /// back as what they are.
    #[tokio::test]
    #[ignore = "requires live Catbox access"]
    async fn live_examples_resolve_to_their_kinds() {
        use std::time::Duration;

        let resolver = CatboxResolver::new(Http::new(crate::http::HttpConfig::default()));
        let mut kinds = Vec::new();
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
            let resolution = tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&url))
                .await
                .expect("resolution timed out")
                .unwrap();
            match resolution {
                Resolution::Media(resolved) => {
                    assert!(resolved.variants[0].size.is_some(), "{link}: no size");
                    kinds.push((link.to_string(), resolved.media));
                }
                Resolution::Playlist(playlist) => assert!(!playlist.entries.is_empty()),
            }
        }
        assert!(kinds.contains(&(
            "https://files.catbox.moe/00koca.jpg".to_string(),
            MediaKind::Image
        )));
        assert!(kinds.contains(&(
            "https://files.catbox.moe/53103j.zip".to_string(),
            MediaKind::File
        )));
        assert!(kinds.contains(&(
            "https://files.catbox.moe/safuz8.mp4".to_string(),
            MediaKind::Video
        )));
    }
}
