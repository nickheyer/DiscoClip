//! Dropbox shared files: a share link asked for as a download, so the file arrives from
//! the content host with its name and length.

use async_trait::async_trait;
use url::Url;

use super::{
    Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Variant, VariantKind,
    clean_title, essence, probe_file,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "dropbox";

pub fn parse_link(url: &Url) -> Option<Url> {
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
    if host == "dl.dropboxusercontent.com" || host.ends_with(".dl.dropboxusercontent.com") {
        return (!segments.is_empty()).then(|| url.clone());
    }
    if host != "dropbox.com" && host != "www.dropbox.com" && host != "dl.dropbox.com" {
        return None;
    }
    match segments.as_slice() {
        ["s", _, _, ..] | ["scl", "fi", _, _, ..] | ["sh", _, _, _, ..] => Some(url.clone()),
        _ => None,
    }
}

/// The link asked for as a download: `dl=1` in place of the preview.
pub fn download_link(url: &Url) -> Url {
    let mut link = url.clone();
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| k != "dl" && k != "raw")
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    link.set_query(None);
    {
        let mut query = link.query_pairs_mut();
        for (k, v) in &pairs {
            query.append_pair(k, v);
        }
        query.append_pair("dl", "1");
    }
    link
}

fn name_from_path(url: &Url) -> Option<String> {
    let last = url.path_segments()?.rfind(|s| !s.is_empty())?;
    let name = percent_encoding::percent_decode_str(last)
        .decode_utf8_lossy()
        .into_owned();
    name.contains('.').then_some(name)
}

pub struct DropboxResolver {
    http: Http,
}

impl DropboxResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for DropboxResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Dropbox",
            hosts: &["dropbox.com", "dl.dropboxusercontent.com"],
            features: &["shared files", "scl links", "content links"],
            formats: &["mp4", "webm", "mkv", "mov"],
            session: SessionSupport::None,
            examples: &[
                "https://www.dropbox.com/scl/fi/cttkzvl75vuqwn2o5ctx0/youtube-dl-test-video-BaW_jenozKc.mp4?rlkey=zae0yts5dh5e6hh4jduo25w7v&dl=0",
                "https://www.dropbox.com/s/nelirfsxnmcfbfh/youtube-dl%20test%20video%20%27%C3%A4%22BaW_jenozKc.mp4?dl=0",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let download = download_link(&link);
        let probed = probe_file(&self.http, &download, PLATFORM, BROWSER_UA, &[]).await?;
        match probed.status.as_u16() {
            200 | 206 => {}
            404 | 410 => return Err(ResolveError::NotFound(url.clone())),
            403 | 401 => {
                return Err(ResolveError::unavailable(
                    url,
                    "the link is password protected or has been disabled",
                ));
            }
            429 => return Err(ResolveError::RateLimited(url.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    url,
                    format!("the host answered HTTP {status}"),
                ));
            }
        }
        let content_type = essence(probed.content_type.as_deref());
        if content_type == "text/html" {
            return Err(ResolveError::unavailable(
                url,
                "the link opens a page rather than a file; it may be a folder or need a login",
            ));
        }
        let name = probed
            .filename
            .clone()
            .or_else(|| name_from_path(&probed.url))
            .or_else(|| name_from_path(&link));
        let container = name
            .as_deref()
            .and_then(|n| n.rsplit('.').next())
            .and_then(Container::from_extension)
            .or_else(|| Container::from_mime(&content_type));
        let Some(container) = container else {
            return Err(ResolveError::unavailable(
                url,
                format!(
                    "{} is not a video file",
                    name.as_deref().unwrap_or("the file")
                ),
            ));
        };
        let mut v = Variant::new(download, VariantKind::File);
        v.container = Some(container.clone());
        if container == Container::Mp4 {
            v.video = Some(VideoCodec::H264);
            v.audio = Some(AudioCodec::Aac);
        }
        v.size = probed.size;
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = link
            .path_segments()
            .and_then(|mut s| s.nth(1))
            .map(String::from);
        resolved.title = name
            .as_deref()
            .map(|n| n.rsplit_once('.').map_or(n, |(s, _)| s))
            .and_then(clean_title);
        resolved.webpage_url = Some(link.clone());
        resolved.variants = vec![v];
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

    fn get(url: &str, status: u16, content_type: &str, headers: &[(&str, &str)], final_url: &str) -> Exchange {
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
                url: final_url.into(),
                headers: all,
                body: RecordedBody::Empty,
                truncated: false,
            },
        }
    }

    const SCL: &str = "https://www.dropbox.com/scl/fi/cttkzvl75vuqwn2o5ctx0/youtube-dl-test-video-BaW_jenozKc.mp4?rlkey=zae0yts5dh5e6hh4jduo25w7v&dl=0";

    #[test]
    fn links_are_read_and_turned_into_downloads() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert!(link(SCL).is_some());
        assert!(link("https://www.dropbox.com/s/nelirfsxnmcfbfh/youtube-dl%20test.mp4?dl=0").is_some());
        assert!(link("https://dl.dropboxusercontent.com/scl/fi/abc/clip.mp4?rlkey=x").is_some());
        assert_eq!(link("https://www.dropbox.com/home"), None);
        assert_eq!(link("https://www.dropbox.com/scl/fo/abc/h?rlkey=x&dl=0"), None);
        assert_eq!(
            download_link(&Url::parse(SCL).unwrap()).as_str(),
            "https://www.dropbox.com/scl/fi/cttkzvl75vuqwn2o5ctx0/youtube-dl-test-video-BaW_jenozKc.mp4?rlkey=zae0yts5dh5e6hh4jduo25w7v&dl=1"
        );
    }

    #[tokio::test]
    async fn shared_files_resolve_through_their_download_link() {
        let download = "https://www.dropbox.com/scl/fi/cttkzvl75vuqwn2o5ctx0/youtube-dl-test-video-BaW_jenozKc.mp4?rlkey=zae0yts5dh5e6hh4jduo25w7v&dl=1";
        let mut fixture = Fixture::new("dropbox", None);
        fixture.exchanges.push(get(
            download,
            206,
            "application/json",
            &[("content-range", "bytes 0-0/1601434"), ("content-disposition", "attachment; filename=\"youtube-dl test video '?BaW_jenozKc.mp4\"; filename*=UTF-8''youtube-dl%20test%20video%20%27%C3%A4BaW_jenozKc.mp4")],
            "https://ucc7.dl.dropboxusercontent.com/cd/0/get/token/file?dl=1",
        ));
        fixture.exchanges.push(get("https://www.dropbox.com/s/gone/clip.mp4?dl=1", 404, "text/html", &[], "https://www.dropbox.com/s/gone/clip.mp4?dl=1"));
        fixture.exchanges.push(get("https://www.dropbox.com/s/locked/clip.mp4?dl=1", 403, "text/html", &[], "https://www.dropbox.com/s/locked/clip.mp4?dl=1"));
        let resolver = DropboxResolver::new(Http::replay(fixture));
        let resolved = resolver.resolve(&Url::parse(SCL).unwrap()).await.unwrap().media().unwrap();
        assert_eq!(resolved.title.as_deref(), Some("youtube-dl test video 'äBaW_jenozKc"));
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].url.as_str(), download);
        assert_eq!(resolved.variants[0].size, Some(1601434));
        assert_eq!(resolved.variants[0].container, Some(Container::Mp4));
        assert!(matches!(
            resolver.resolve(&Url::parse("https://www.dropbox.com/s/gone/clip.mp4?dl=0").unwrap()).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver.resolve(&Url::parse("https://www.dropbox.com/s/locked/clip.mp4?dl=0").unwrap()).await.unwrap_err();
        assert!(matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("password")), "{error}");
    }
}
