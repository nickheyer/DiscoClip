//! Dropbox shared files: the share page names the transcoded HLS stream the site plays,
//! and, when the share allows downloads, the original file arrives from the content
//! host with its name and length. A password-protected share is unlocked with the
//! `password` the link carries.

use async_trait::async_trait;
use url::Url;

use std::sync::LazyLock;

use base64::Engine as _;
use regex::Regex;

use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Variant,
    VariantKind, clean_title, essence, fetch, hls, navigation_headers, probe_file, status_error,
    util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "dropbox";

/// `registerStreamedPrefetch("…", "<base64>")`: the page's prefetched data, in parts.
static RE_PREFETCH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"registerStreamedPrefetch\s*\(\s*"[\w/+=]+"\s*,\s*"([\w/+=]+)""#).unwrap()
});
/// The transcoded stream a prefetched part names.
static RE_TRANSCODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n.?(https://[^\x03\x08\x12\n]+\.m3u8)").unwrap());
static RE_THUMBNAIL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(https://www\.dropbox\.com/temp_thumb_from_token/[\w/?&=]+)").unwrap()
});
static RE_CONTENT_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"content_id=([\w.+=/-]+)").unwrap());

/// The prefetched parts of a share page, decoded, newest first.
pub fn prefetched_parts(html: &str) -> Vec<String> {
    let mut parts: Vec<String> = RE_PREFETCH
        .captures_iter(html)
        .filter_map(|caps| {
            base64::engine::general_purpose::STANDARD
                .decode(caps[1].as_bytes())
                .ok()
                .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        })
        .collect();
    parts.reverse();
    parts
}

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
        ["s", _, ..]
        | ["scl", "fi", _, ..]
        | ["e", "scl", "fi", _, ..]
        | ["scl", "fo", _, _, _, ..]
        | ["sh", _, ..] => Some(url.clone()),
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
        if link
            .host_str()
            .is_some_and(|h| h.ends_with("dl.dropboxusercontent.com"))
        {
            return self.file_only(&link, url).await;
        }
        let mut page_url = link.clone();
        page_url.query_pairs_mut().clear().append_pair("dl", "0");
        let fetched = fetch(
            &self.http,
            &page_url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        let mut html = fetched.text();
        let mut parts = prefetched_parts(&html);
        // A password-protected share names its content id; the password on the link
        // unlocks it for this session.
        if let Some(content_id) = parts
            .iter()
            .find(|part| part.contains("/sm/password"))
            .and_then(|part| util::search(&RE_CONTENT_ID, part))
        {
            let Some(password) = util::query_param(url, "password").filter(|p| !p.is_empty())
            else {
                return Err(ResolveError::unavailable(
                    url,
                    "the share is password protected; add ?password=… to the link",
                ));
            };
            let token = self
                .http
                .jar(PLATFORM)
                .get("t")
                .map(|c| c.value.clone())
                .ok_or_else(|| {
                    ResolveError::malformed(url, "the share page set no session cookie")
                })?;
            let relative = format!(
                "{}{}",
                link.path(),
                link.query().map(|q| format!("?{q}")).unwrap_or_default()
            );
            let response = self
                .http
                .post(Url::parse("https://www.dropbox.com/sm/auth").expect("valid"))
                .platform(PLATFORM)
                .user_agent(BROWSER_UA)
                .form(&[
                    ("is_xhr", "true"),
                    ("t", token.as_str()),
                    ("content_id", content_id.as_str()),
                    ("password", password.as_str()),
                    ("url", relative.as_str()),
                ])
                .send()
                .await?;
            let answer: serde_json::Value = response
                .json(MAX_PAGE)
                .await
                .map_err(|e| ResolveError::malformed(url, format!("password answer: {e}")))?;
            if answer["status"].as_str() != Some("authed") {
                return Err(ResolveError::unavailable(
                    url,
                    "the share refused the password",
                ));
            }
            let again = fetch(
                &self.http,
                &page_url,
                PLATFORM,
                BROWSER_UA,
                &navigation_headers(),
                MAX_PAGE,
            )
            .await?;
            html = again.text();
            parts = prefetched_parts(&html);
        }
        let mut resolved = Resolved::new(PLATFORM);
        let mut downloads_allowed = false;
        for part in &parts {
            if part.contains("anonymous:\tanonymous") {
                downloads_allowed = true;
            }
            let Some(transcode) =
                util::search(&RE_TRANSCODE, part).and_then(|t| Url::parse(&t).ok())
            else {
                continue;
            };
            let expanded = hls::expand(&self.http, &transcode, PLATFORM, BROWSER_UA, &[]).await?;
            resolved.duration = expanded.duration;
            resolved.subtitles = expanded.subtitles;
            for mut variant in expanded.variants {
                variant.format_id = Some(match &variant.label {
                    Some(label) => format!("hls-{label}"),
                    None => "hls".to_string(),
                });
                resolved.variants.push(variant);
            }
            resolved.thumbnail =
                util::search(&RE_THUMBNAIL, part).and_then(|t| Url::parse(&t).ok());
            break;
        }
        let name = name_from_path(&link);
        if downloads_allowed || resolved.variants.is_empty() {
            match self.original(&link, url).await {
                Ok((variant, served_name)) => {
                    if resolved.title.is_none() {
                        resolved.title = served_name
                            .as_deref()
                            .map(|n| n.rsplit_once('.').map_or(n, |(s, _)| s))
                            .and_then(clean_title);
                    }
                    resolved.variants.push(variant);
                }
                Err(error) if !resolved.variants.is_empty() => {
                    tracing::debug!(%url, "Dropbox original not served: {error}");
                }
                Err(error) => return Err(error),
            }
        }
        resolved.id = link
            .path_segments()
            .and_then(|mut s| s.nth(1))
            .map(String::from);
        if resolved.title.is_none() {
            resolved.title = name
                .as_deref()
                .map(|n| n.rsplit_once('.').map_or(n, |(s, _)| s))
                .and_then(clean_title)
                .or_else(|| {
                    super::page::Page::parse(&html, &page_url)
                        .meta("og:title")
                        .and_then(|t| clean_title(&t))
                });
        }
        resolved.webpage_url = Some(link.clone());
        Ok(Resolution::from(resolved))
    }
}

impl DropboxResolver {
    /// The original file as a download, with its name and length.
    async fn original(
        &self,
        link: &Url,
        origin: &Url,
    ) -> Result<(Variant, Option<String>), ResolveError> {
        let download = download_link(link);
        let probed = probe_file(&self.http, &download, PLATFORM, BROWSER_UA, &[]).await?;
        match probed.status.as_u16() {
            200 | 206 => {}
            404 | 410 => return Err(ResolveError::NotFound(origin.clone())),
            403 | 401 => {
                return Err(ResolveError::unavailable(
                    origin,
                    "the share does not allow downloads",
                ));
            }
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the host answered HTTP {status}"),
                ));
            }
        }
        let content_type = essence(probed.content_type.as_deref());
        if content_type == "text/html" {
            return Err(ResolveError::unavailable(
                origin,
                "the link opens a page rather than a file; it may be a folder or need a login",
            ));
        }
        let name = probed
            .filename
            .clone()
            .or_else(|| name_from_path(&probed.url))
            .or_else(|| name_from_path(link));
        let container = name
            .as_deref()
            .and_then(|n| n.rsplit('.').next())
            .and_then(Container::from_extension)
            .or_else(|| Container::from_mime(&content_type));
        let Some(container) = container else {
            return Err(ResolveError::unavailable(
                origin,
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
        v.format_id = Some("original".to_string());
        v.label = Some("original".to_string());
        Ok((v, name))
    }

    /// A link straight to the content host: the file alone.
    async fn file_only(&self, link: &Url, origin: &Url) -> Result<Resolution, ResolveError> {
        let (variant, name) = self.original(link, origin).await?;
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
        resolved.variants = vec![variant];
        Ok(Resolution::from(resolved))
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
        headers: &[(&str, &str)],
        final_url: &str,
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
        assert!(
            link("https://www.dropbox.com/s/nelirfsxnmcfbfh/youtube-dl%20test.mp4?dl=0").is_some()
        );
        assert!(link("https://dl.dropboxusercontent.com/scl/fi/abc/clip.mp4?rlkey=x").is_some());
        assert_eq!(link("https://www.dropbox.com/home"), None);
        assert!(link("https://www.dropbox.com/scl/fo/abc/h/sub/clip.mp4?rlkey=x&dl=0").is_some());
        assert!(link("https://www.dropbox.com/e/scl/fi/abc/clip.mp4").is_some());
        assert!(link("https://www.dropbox.com/s/abc").is_some());
        assert_eq!(
            link("https://www.dropbox.com/scl/fo/abc/h?rlkey=x&dl=0"),
            None
        );
        assert_eq!(
            download_link(&Url::parse(SCL).unwrap()).as_str(),
            "https://www.dropbox.com/scl/fi/cttkzvl75vuqwn2o5ctx0/youtube-dl-test-video-BaW_jenozKc.mp4?rlkey=zae0yts5dh5e6hh4jduo25w7v&dl=1"
        );
    }

    fn page(url: &str, status: u16, body: &str) -> Exchange {
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
                headers: vec![("content-type".into(), "text/html".into())],
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    const PAGE: &str = "https://www.dropbox.com/scl/fi/cttkzvl75vuqwn2o5ctx0/youtube-dl-test-video-BaW_jenozKc.mp4?dl=0";

    #[tokio::test]
    async fn shares_resolve_their_transcode_and_original_or_ask_for_a_password() {
        let download = "https://www.dropbox.com/scl/fi/cttkzvl75vuqwn2o5ctx0/youtube-dl-test-video-BaW_jenozKc.mp4?rlkey=zae0yts5dh5e6hh4jduo25w7v&dl=1";
        let html = concat!(
            r#"<html><head><meta property="og:title" content="youtube-dl test video"></head><body><script>registerStreamedPrefetch("a", "%s");</script>"#,
            r#"<script>registerStreamedPrefetch("b", "%s");</script></body></html>"#
        );
        let html = html.replacen("%s", "%TRANSCODE%", 1).replacen("%s", "%ALLOWED%", 1)
            .replace("%TRANSCODE%", "EggKaHR0cHM6Ly9wcmV2aWV3cy5kcm9wYm94LmNvbS9wL2hsc19wbGF5bGlzdC9hYmMvbWFzdGVyLm0zdTgDCmh0dHBzOi8vd3d3LmRyb3Bib3guY29tL3RlbXBfdGh1bWJfZnJvbV90b2tlbi94P3NpemU9MTAyNHg3NjgK")
            .replace("%ALLOWED%", "EngKYW5vbnltb3VzOglhbm9ueW1vdXMK");
        let mut fixture = Fixture::new("dropbox", None);
        fixture.exchanges.push(page(PAGE, 200, &html));
        fixture.exchanges.push(page(
            "https://previews.dropbox.com/p/hls_playlist/abc/master.m3u8",
            200,
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1500000,RESOLUTION=1280x720\n720.m3u8\n",
        ));
        fixture.exchanges.push(page(
            "https://previews.dropbox.com/p/hls_playlist/abc/720.m3u8",
            200,
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\n0.ts\n#EXT-X-ENDLIST\n",
        ));
        fixture.exchanges.push(get(
            download,
            206,
            "application/json",
            &[("content-range", "bytes 0-0/1601434"), ("content-disposition", "attachment; filename=\"youtube-dl test video '?BaW_jenozKc.mp4\"; filename*=UTF-8''youtube-dl%20test%20video%20%27%C3%A4BaW_jenozKc.mp4")],
            "https://ucc7.dl.dropboxusercontent.com/cd/0/get/token/file?dl=1",
        ));
        fixture.exchanges.push(page(
            "https://www.dropbox.com/s/gone/clip.mp4?dl=0",
            404,
            "<html>gone</html>",
        ));
        fixture.exchanges.push(page(
            "https://www.dropbox.com/s/locked/clip.mp4?dl=0",
            200,
            r#"<html><script>registerStreamedPrefetch("a", "Ei9zbS9wYXNzd29yZD9jb250ZW50X2lkPWlkJTNBQUJDMTIzJTNEeHl6Cg==");</script></html>"#,
        ));
        let resolver = DropboxResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse(SCL).unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(
            resolved.title.as_deref(),
            Some("youtube-dl test video 'äBaW_jenozKc")
        );
        assert_eq!(resolved.variants.len(), 2, "the transcode and the original");
        assert_eq!(resolved.variants[0].kind, VariantKind::Hls);
        assert_eq!(resolved.variants[0].height, Some(720));
        assert_eq!(resolved.variants[1].url.as_str(), download);
        assert_eq!(resolved.variants[1].size, Some(1601434));
        assert_eq!(resolved.variants[1].container, Some(Container::Mp4));
        assert_eq!(resolved.variants[1].format_id.as_deref(), Some("original"));
        assert!(
            resolved
                .thumbnail
                .as_ref()
                .unwrap()
                .as_str()
                .starts_with("https://www.dropbox.com/temp_thumb_from_token/")
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.dropbox.com/s/gone/clip.mp4?dl=0").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(&Url::parse("https://www.dropbox.com/s/locked/clip.mp4?dl=0").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("password")),
            "{error}"
        );
        let parts = prefetched_parts(&html);
        assert_eq!(parts.len(), 2);
        assert!(parts[0].contains("anonymous:\tanonymous"));
    }
}
