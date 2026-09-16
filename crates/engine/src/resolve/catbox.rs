//! Catbox files and albums: a file on the hosts by its name, probed for its length, and an
//! album's video files as a playlist.

use async_trait::async_trait;
use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use scraper::Selector;
use url::Url;

use super::page::Page;
use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, Variant, VariantKind, clean_title, essence, fetch, probe_file,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "catbox";
const FILE_HOSTS: [&str; 4] = ["files.catbox.moe", "litter.catbox.moe", "de.catbox.moe", "litterbox.catbox.moe"];

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

const AUDIO_EXTENSIONS: &[&str] = &["mp3", "ogg", "oga", "opus", "flac", "m4a", "wav", "aac", "wma", "aiff"];

/// The container a file name's extension names: a video container, or an audio one.
fn media_container(name: &str) -> Option<Container> {
    let ext = name.rsplit('.').next()?.to_ascii_lowercase();
    Container::from_extension(&ext).or_else(|| {
        AUDIO_EXTENSIONS
            .contains(&ext.as_str())
            .then(|| Container::Other(ext.clone()))
    })
}

fn is_audio_name(name: &str) -> bool {
    name.rsplit('.')
        .next()
        .is_some_and(|ext| AUDIO_EXTENSIONS.contains(&ext.to_ascii_lowercase().as_str()))
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
        let container = media_container(&name).ok_or_else(|| {
            ResolveError::unavailable(origin, format!("{name} is not a video or audio file"))
        })?;
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
        let mut v = Variant::new(file.clone(), VariantKind::File);
        v.container = Some(container.clone());
        if is_audio_name(&name) {
            v.audio_only = true;
            v.audio = Some(match name.rsplit('.').next().map(|e| e.to_ascii_lowercase()).as_deref() {
                Some("mp3") => AudioCodec::Mp3,
                Some("m4a") | Some("aac") => AudioCodec::Aac,
                Some("ogg") | Some("oga") => AudioCodec::Vorbis,
                Some("opus") => AudioCodec::Opus,
                Some(other) => AudioCodec::Other(other.to_string()),
                None => AudioCodec::Other("audio".to_string()),
            });
        } else if container == Container::Mp4 {
            v.video = Some(VideoCodec::H264);
            v.audio = Some(AudioCodec::Aac);
        }
        v.size = probed.size;
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s).to_string());
        resolved.title = resolved.id.clone();
        resolved.webpage_url = Some(file.clone());
        resolved.variants = vec![v];
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
            let links = Selector::parse(".imagecontainer a[href], .imagelist a[href]").expect("valid");
            let mut entries: Vec<PlaylistEntry> = Vec::new();
            for anchor in page.document().select(&links) {
                let Some(file) = anchor.value().attr("href").and_then(|h| page_url.join(h).ok()) else {
                    continue;
                };
                if !matches!(parse_link(&file), Some(Link::File(_))) {
                    continue;
                }
                let name = file.path().trim_start_matches('/').to_string();
                if media_container(&name).is_none() || entries.iter().any(|e| e.url == file) {
                    continue;
                }
                entries.push(PlaylistEntry {
                    url: file,
                    title: Some(name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s).to_string()),
                    duration: None,
                });
            }
            (title, created, entries)
        };
        if entries.is_empty() {
            return Err(if title.is_none() && !html.contains("imagecontainer") {
                ResolveError::NotFound(origin.clone())
            } else {
                ResolveError::unavailable(origin, "the album holds no video files")
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
            hosts: &["catbox.moe", "files.catbox.moe", "de.catbox.moe", "litter.catbox.moe", "litterbox.catbox.moe"],
            features: &["files", "litterbox files", "albums", "audio"],
            formats: &["mp4", "webm", "mkv", "mov", "gif", "mp3", "ogg", "flac"],
            session: SessionSupport::None,
            examples: &[
                "https://files.catbox.moe/safuz8.mp4",
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

    fn get(url: &str, status: u16, content_type: &str, body: &str, headers: &[(&str, &str)]) -> Exchange {
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
        assert!(matches!(link("https://files.catbox.moe/safuz8.mp4"), Some(Link::File(_))));
        assert!(matches!(link("https://litter.catbox.moe/t8v3n9.webm"), Some(Link::File(_))));
        assert_eq!(link("https://catbox.moe/c/8xw6g4"), Some(Link::Album("8xw6g4".into())));
        assert_eq!(link("https://catbox.moe/faq.php"), None);
        assert_eq!(link("https://files.catbox.moe/"), None);
    }

    #[tokio::test]
    async fn files_are_probed_for_their_length() {
        let mut fixture = Fixture::new("catbox", None);
        fixture.exchanges.push(get("https://files.catbox.moe/safuz8.mp4", 206, "video/mp4", "", &[("content-range", "bytes 0-0/7971211")]));
        fixture.exchanges.push(get("https://files.catbox.moe/zzzzzz.mp4", 404, "text/html; charset=UTF-8", "<html>404</html>", &[]));
        let resolver = CatboxResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://files.catbox.moe/safuz8.mp4").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.title.as_deref(), Some("safuz8"));
        assert_eq!(resolved.variants[0].size, Some(7971211));
        assert_eq!(resolved.variants[0].container, Some(Container::Mp4));
        assert!(matches!(
            resolver.resolve(&Url::parse("https://files.catbox.moe/zzzzzz.mp4").unwrap()).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver.resolve(&Url::parse("https://files.catbox.moe/ulnqno.py").unwrap()).await.unwrap_err();
        assert!(matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("not a video")), "{error}");
    }

    #[tokio::test]
    async fn albums_list_their_video_files() {
        let mut fixture = Fixture::new("catbox", None);
        fixture.exchanges.push(get("https://catbox.moe/c/8xw6g4", 200, "text/html; charset=UTF-8", ALBUM, &[]));
        fixture.exchanges.push(get("https://catbox.moe/c/zeara1", 200, "text/html; charset=UTF-8", r#"<html><body><div class="title"><h1>Six</h1></div><div class="imagecontainer"><a href='https://files.catbox.moe/3bs9a6.png'><img src='x'></a></div></body></html>"#, &[]));
        let resolver = CatboxResolver::new(Http::replay(fixture));
        let playlist = match resolver.resolve(&Url::parse("https://catbox.moe/c/8xw6g4").unwrap()).await.unwrap() {
            Resolution::Playlist(p) => p,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(playlist.title.as_deref(), Some("blender_dump"));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(playlist.entries[0].url.as_str(), "https://files.catbox.moe/s2dd6o.gif");
        assert_eq!(playlist.entries[1].title.as_deref(), Some("clip42"));
        let error = resolver.resolve(&Url::parse("https://catbox.moe/c/zeara1").unwrap()).await.unwrap_err();
        assert!(matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("no video")), "{error}");
        assert_eq!(parse_created("Created March 19 2022").unwrap().to_string(), "2022-03-19T00:00:00Z");
    }
}
