//! Dropbox shared files: the share page names the transcoded HLS stream the site plays
//! for a video, and, when the share allows downloads, the original file arrives from the
//! content host with its name and length, whatever kind of file it is: a video, audio,
//! an image or anything else, told apart by the name the host gives it. A
//! password-protected share is unlocked with the `password` the link carries.

use async_trait::async_trait;
use url::Url;

use std::sync::LazyLock;

use base64::Engine as _;
use regex::Regex;

use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Tag, Variant,
    VariantKind, clean_title, essence, fetch, hls, navigation_headers, probe_file, status_error,
    util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

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

/// What a file is and the format it is in: from the content type the host serves it as
/// when that names a format, else from the file's name, else from the broad type served.
/// The content host serves everything as `application/binary`, so the name decides.
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
            features: &[
                "shared files",
                "scl links",
                "content links",
                "audio",
                "images",
                "any file",
            ],
            formats: &[
                "hls", "mp4", "webm", "mkv", "mov", "mp3", "m4a", "flac", "jpg", "png", "pdf",
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
                "https://www.dropbox.com/scl/fi/cttkzvl75vuqwn2o5ctx0/youtube-dl-test-video-BaW_jenozKc.mp4?rlkey=zae0yts5dh5e6hh4jduo25w7v&dl=0",
                "https://www.dropbox.com/s/nelirfsxnmcfbfh/youtube-dl%20test%20video%20%27%C3%A4%22BaW_jenozKc.mp4?dl=0",
                "https://www.dropbox.com/s/19wl7p7eubcxx9y/matrix.JPG?dl=0",
                "https://www.dropbox.com/s/w3cd1kaetwe715u/2022_1Q_conference_eng.pdf?dl=0",
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
        // A password-protected share names its content id. The password on the link
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
                    "Password required. Add ?password= to the link.",
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
        // The transcode is only ever a video's. Anything else is known by its original.
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
                Ok((variant, served_name, kind)) => {
                    if resolved.title.is_none() {
                        resolved.title = served_name
                            .as_deref()
                            .map(|n| n.rsplit_once('.').map_or(n, |(s, _)| s))
                            .and_then(clean_title);
                    }
                    if resolved.variants.is_empty() {
                        resolved.media = kind;
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
    /// The original file as a download, with its name, its length and what it is.
    async fn original(
        &self,
        link: &Url,
        origin: &Url,
    ) -> Result<(Variant, Option<String>, MediaKind), ResolveError> {
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
                "No file found. The link may point to a folder or require login.",
            ));
        }
        let name = probed
            .filename
            .clone()
            .or_else(|| name_from_path(&probed.url))
            .or_else(|| name_from_path(link));
        let (kind, container) = classify(name.as_deref().unwrap_or(""), &content_type);
        let mut v = file_variant(download, kind, container, probed.size);
        v.format_id = Some("original".to_string());
        v.label = Some("original".to_string());
        Ok((v, name, kind))
    }

    /// A link straight to the content host: the file alone.
    async fn file_only(&self, link: &Url, origin: &Url) -> Result<Resolution, ResolveError> {
        let (variant, name, kind) = self.original(link, origin).await?;
        let mut resolved = Resolved::of(PLATFORM, kind);
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
        assert_eq!(resolved.media, MediaKind::Video);
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

    /// The image and PDF download exchanges were recorded from the content host on
    /// 2026-09-18: it serves every file as `application/binary`, with the name in
    /// `Content-Disposition`. The share pages carry no transcode for them, only the
    /// downloads-allowed part. The audio share has the same shape.
    #[tokio::test]
    async fn images_audio_and_other_files_resolve_through_their_originals() {
        let allowed = r#"<html><head><meta property="og:title" content="%TITLE%"></head><body><script>registerStreamedPrefetch("a", "EngKYW5vbnltb3VzOglhbm9ueW1vdXMK");</script></body></html>"#;
        let mut fixture = Fixture::new("dropbox", None);
        fixture.exchanges.push(page(
            "https://www.dropbox.com/s/19wl7p7eubcxx9y/matrix.JPG?dl=0",
            200,
            &allowed.replace("%TITLE%", "matrix.JPG"),
        ));
        fixture.exchanges.push(get(
            "https://www.dropbox.com/s/19wl7p7eubcxx9y/matrix.JPG?dl=1",
            206,
            "application/binary",
            &[
                ("content-range", "bytes 0-0/62237"),
                (
                    "content-disposition",
                    "attachment; filename=\"matrix.JPG\"; filename*=UTF-8''matrix.JPG",
                ),
            ],
            "https://uc1234.dl.dropboxusercontent.com/cd/0/get/token/file?dl=1",
        ));
        fixture.exchanges.push(page(
            "https://www.dropbox.com/s/w3cd1kaetwe715u/2022_1Q_conference_eng.pdf?dl=0",
            200,
            &allowed.replace("%TITLE%", "2022_1Q_conference_eng.pdf"),
        ));
        fixture.exchanges.push(get(
            "https://www.dropbox.com/s/w3cd1kaetwe715u/2022_1Q_conference_eng.pdf?dl=1",
            206,
            "application/binary",
            &[
                ("content-range", "bytes 0-0/853270"),
                (
                    "content-disposition",
                    "attachment; filename=\"2022_1Q_conference_eng.pdf\"; filename*=UTF-8''2022_1Q_conference_eng.pdf",
                ),
            ],
            "https://uc5678.dl.dropboxusercontent.com/cd/0/get/token/file?dl=1",
        ));
        fixture.exchanges.push(page(
            "https://www.dropbox.com/s/aud1oshare/episode%2012.mp3?dl=0",
            200,
            &allowed.replace("%TITLE%", "episode 12.mp3"),
        ));
        fixture.exchanges.push(get(
            "https://www.dropbox.com/s/aud1oshare/episode%2012.mp3?dl=1",
            206,
            "application/binary",
            &[
                ("content-range", "bytes 0-0/41234567"),
                (
                    "content-disposition",
                    "attachment; filename=\"episode 12.mp3\"; filename*=UTF-8''episode%2012.mp3",
                ),
            ],
            "https://uc9012.dl.dropboxusercontent.com/cd/0/get/token/file?dl=1",
        ));
        let resolver = DropboxResolver::new(Http::replay(fixture));
        let resolve = |link: &str| {
            let url = Url::parse(link).unwrap();
            let resolver = &resolver;
            async move { resolver.resolve(&url).await.unwrap().media().unwrap() }
        };
        let image = resolve("https://www.dropbox.com/s/19wl7p7eubcxx9y/matrix.JPG?dl=0").await;
        assert_eq!(image.media, MediaKind::Image);
        assert_eq!(image.title.as_deref(), Some("matrix"));
        assert_eq!(image.variants.len(), 1);
        assert_eq!(image.variants[0].container, Some(Container::Jpeg));
        assert_eq!(image.variants[0].size, Some(62237));
        assert_eq!(image.variants[0].format_id.as_deref(), Some("original"));
        let document =
            resolve("https://www.dropbox.com/s/w3cd1kaetwe715u/2022_1Q_conference_eng.pdf?dl=0")
                .await;
        assert_eq!(document.media, MediaKind::File);
        assert_eq!(document.title.as_deref(), Some("2022_1Q_conference_eng"));
        assert_eq!(
            document.variants[0].container,
            Some(Container::Other("pdf".into()))
        );
        assert_eq!(document.variants[0].size, Some(853270));
        let audio = resolve("https://www.dropbox.com/s/aud1oshare/episode%2012.mp3?dl=0").await;
        assert_eq!(audio.media, MediaKind::Audio);
        assert_eq!(audio.title.as_deref(), Some("episode 12"));
        assert_eq!(audio.variants[0].container, Some(Container::Mp3));
        assert_eq!(audio.variants[0].audio, Some(AudioCodec::Mp3));
        assert!(audio.variants[0].audio_only);
        assert_eq!(audio.variants[0].size, Some(41234567));
        assert_eq!(classify("", "application/binary"), (MediaKind::File, None));
        assert_eq!(
            classify("clip.webm", "application/binary"),
            (MediaKind::Video, Some(Container::Webm))
        );
    }

    /// Every example link resolves live, and the image and the PDF among them come back
    /// as what they are.
    #[tokio::test]
    #[ignore = "requires live Dropbox access"]
    async fn live_examples_resolve_to_their_kinds() {
        use std::time::Duration;

        let resolver = DropboxResolver::new(Http::new(crate::http::HttpConfig::default()));
        let mut kinds = Vec::new();
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
            let resolved = tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&url))
                .await
                .expect("resolution timed out")
                .unwrap()
                .media()
                .unwrap();
            assert!(!resolved.variants.is_empty(), "{link}: no variants");
            kinds.push((link.to_string(), resolved.media));
        }
        assert!(kinds.contains(&(
            "https://www.dropbox.com/s/19wl7p7eubcxx9y/matrix.JPG?dl=0".to_string(),
            MediaKind::Image
        )));
        assert!(kinds.contains(&(
            "https://www.dropbox.com/s/w3cd1kaetwe715u/2022_1Q_conference_eng.pdf?dl=0".to_string(),
            MediaKind::File
        )));
        assert_eq!(
            kinds.iter().filter(|(_, k)| *k == MediaKind::Video).count(),
            2
        );
    }
}
