//! Newgrounds movies, through the video sources the site hands its player: the MP4 of
//! every height, the author, the date and the poster from the submission's page, after
//! completing the site's proof of work when its guard requests it.

mod guard;

use std::sync::LazyLock;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::page::Page;
use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Tag, Variant,
    VariantKind, clean_title, navigation_headers, parse_codecs,
};
use crate::http::Http;
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "newgrounds";
const SITE: &str = "https://www.newgrounds.com/";

static RE_AUTHOR_HEADING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?s)<h4[^>]*>(.+?)</h4>.*?<em>\s*(?:Author|Artist)\s*</em>").unwrap()
});
static RE_AUTHOR_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:Author|Writer|Artist)\s*<a[^>]+>([^<]+)").unwrap());
static RE_PUBLISHED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"itemprop="(?:uploadDate|datePublished)"\s+content="([^"]+)""#).unwrap()
});
static RE_EMBED_FILE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"embedController\(\[\{"url"\s*:\s*("[^"]+")"#).unwrap());
static RE_HEIGHT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d+)p").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Movie(String),
    Audio(String),
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "newgrounds.com" && host != "www.newgrounds.com" {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    match segments.as_slice() {
        ["portal", "view" | "video", id, ..] if id.chars().all(|c| c.is_ascii_digit()) => {
            Some(Link::Movie(id.to_string()))
        }
        ["audio", "listen", id, ..] if id.chars().all(|c| c.is_ascii_digit()) => {
            Some(Link::Audio(id.to_string()))
        }
        _ => None,
    }
}

fn strip_tags(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Whether the page is the guard the site shows clients it wants to challenge.
pub fn is_guard(html: &str) -> bool {
    html.contains("NG Guard") || html.contains("/_guard/")
}

/// The MP4s the video endpoint lists, by height.
pub fn variants_of(video: &Value) -> Vec<Variant> {
    let mut variants = Vec::new();
    let Some(sources) = video["sources"].as_object() else {
        return variants;
    };
    for (label, list) in sources {
        for source in list.as_array().into_iter().flatten() {
            let Some(url) = source["src"].as_str().and_then(|u| Url::parse(u).ok()) else {
                continue;
            };
            let mime = source["type"].as_str().unwrap_or("video/mp4");
            let mut v = Variant::new(url, VariantKind::File);
            v.container = Container::from_mime(mime).or(Some(Container::Mp4));
            let (codec_video, codec_audio) = parse_codecs(
                mime.split(';')
                    .find_map(|p| p.trim().strip_prefix("codecs="))
                    .map(|c| c.trim_matches('"')),
            );
            v.video = codec_video.or(Some(VideoCodec::H264));
            v.audio = codec_audio.or(Some(AudioCodec::Aac));
            v.height = RE_HEIGHT
                .captures(label)
                .and_then(|c| c.get(1))
                .and_then(|m| m.as_str().parse().ok());
            v.format_id = Some(label.clone());
            v.label = Some(label.clone());
            v.headers = vec![("referer".to_string(), SITE.to_string())];
            variants.push(v);
        }
    }
    variants.sort_by_key(|v| std::cmp::Reverse(v.height));
    variants
}

pub struct NewgroundsResolver {
    http: Http,
    guard: guard::Guard,
}

impl NewgroundsResolver {
    pub fn new(http: Http) -> Self {
        Self {
            http,
            guard: guard::Guard::default(),
        }
    }
}

#[async_trait]
impl Resolver for NewgroundsResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Newgrounds",
            hosts: &["newgrounds.com"],
            features: &["movies", "video submissions"],
            formats: &["mp4"],
            media: &[MediaKind::Video],
            tags: &[Tag::Video],
            session: SessionSupport::Optional,
            examples: &[
                "https://www.newgrounds.com/portal/view/1004201",
                "https://www.newgrounds.com/portal/view/297383",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let id = match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Movie(id) => id,
            Link::Audio(_) => {
                return Err(ResolveError::unavailable(
                    url,
                    "the submission is an audio track, not a movie",
                ));
            }
        };
        let page_url = Url::parse(&format!("{SITE}portal/view/{id}")).expect("valid");
        let fetched = self
            .guard
            .fetch(&self.http, &page_url, &navigation_headers())
            .await?;
        let html = fetched.text();
        match fetched.status.as_u16() {
            200..=299 => {}
            401 => {
                return Err(ResolveError::login_required(
                    url,
                    PLATFORM,
                    "the submission is shown to logged-in members only",
                ));
            }
            404 | 410 => return Err(ResolveError::NotFound(url.clone())),
            429 => return Err(ResolveError::RateLimited(url.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    url,
                    format!("the page answered HTTP {status}"),
                ));
            }
        }
        let (page_title, page_description, page_thumbnail) = {
            let page = Page::parse(&html, &fetched.url);
            (
                page.meta("og:title")
                    .and_then(|t| clean_title(&t))
                    .or_else(|| page.title()),
                page.meta("og:description")
                    .or_else(|| page.meta("description"))
                    .and_then(|d| clean_title(&d)),
                page.meta("og:image").and_then(|u| Url::parse(&u).ok()),
            )
        };
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.clone());
        resolved.title = page_title;
        resolved.description = page_description;
        resolved.thumbnail = page_thumbnail;
        resolved.uploaded_at = RE_PUBLISHED
            .captures(&html)
            .and_then(|c| c.get(1))
            .and_then(|m| m.as_str().parse::<Timestamp>().ok());
        resolved.webpage_url = Some(page_url.clone());
        let mut uploader = RE_AUTHOR_HEADING
            .captures(&html)
            .and_then(|c| c.get(1))
            .map(|m| strip_tags(m.as_str()))
            .or_else(|| {
                RE_AUTHOR_LINK
                    .captures(&html)
                    .and_then(|c| c.get(1))
                    .map(|m| m.as_str().to_string())
            })
            .and_then(|a| clean_title(&a));
        if html.contains("adult-content")
            && !html.contains("videoplayer")
            && html.contains("log in")
        {
            return Err(ResolveError::login_required(
                url,
                PLATFORM,
                "the submission is rated for adults and shown to logged-in members only",
            ));
        }

        // A legacy submission embeds its one file in the page's player call.
        let mut variants = Vec::new();
        if let Some(raw) = RE_EMBED_FILE.captures(&html).and_then(|c| c.get(1))
            && let Ok(Value::String(file)) = serde_json::from_str::<Value>(raw.as_str())
            && let Ok(file_url) = Url::parse(&file)
        {
            let mut v = Variant::new(file_url, VariantKind::File);
            v.container =
                Container::from_extension(v.url.path().rsplit('.').next().unwrap_or("mp4"));
            v.format_id = Some("source".into());
            v.headers = vec![("referer".to_string(), SITE.to_string())];
            variants.push(v);
        }
        if variants.is_empty() {
            let video_url = Url::parse(&format!("{SITE}portal/video/{id}")).expect("valid");
            let headers = [
                ("accept".to_string(), "application/json".to_string()),
                ("x-requested-with".to_string(), "XMLHttpRequest".to_string()),
                ("referer".to_string(), page_url.to_string()),
            ];
            let answer = self.guard.fetch(&self.http, &video_url, &headers).await?;
            match answer.status.as_u16() {
                200..=299 => {}
                404 => return Err(ResolveError::NotFound(url.clone())),
                status => {
                    return Err(ResolveError::unavailable(
                        url,
                        format!("the video endpoint answered HTTP {status}"),
                    ));
                }
            }
            let video = answer.json(url)?;
            if uploader.is_none() {
                uploader = video["author"].as_str().and_then(clean_title);
            }
            if resolved.title.is_none() {
                resolved.title = video["title"].as_str().and_then(clean_title);
            }
            if resolved.thumbnail.is_none() {
                resolved.thumbnail = video["poster"].as_str().and_then(|u| Url::parse(u).ok());
            }
            variants = variants_of(&video);
        }
        if variants.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        resolved.uploader = uploader;
        resolved.age_limit = if html.contains("rated-a") || html.contains("Rated A") {
            Some(18)
        } else if html.contains("rated-m") || html.contains("Rated M") {
            Some(17)
        } else {
            None
        };
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

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

    const PAGE: &str = r#"<html><head><title>Scrotum 1</title><meta property="og:title" content="Scrotum 1"><meta property="og:description" content="A cartoon"><meta property="og:image" content="https://picon.ngfiles.com/1004000/flash_1004201_card.png"></head>
    <body><span itemprop="datePublished" content="2023-01-03T21:33:16-05:00"></span>
    <div class="item-details"><h4><a href="https://someone.newgrounds.com">Someone</a></h4><em>Author</em></div>
    <div id="videoplayer"></div></body></html>"#;
    const VIDEO: &str = r#"{"title":"Scrotum 1","description":"A cartoon","author":"Someone","poster":"https://picon.ngfiles.com/1004000/flash_1004201_card.png","sources":{"1080p":[{"src":"https://uploads.ungrounded.net/alternate/1801000/1801346_alternate_212052.1080p.mp4?1673005000","type":"video/mp4"}],"720p":[{"src":"https://uploads.ungrounded.net/alternate/1801000/1801346_alternate_212052.720p.mp4?1673005000","type":"video/mp4"}],"360p":[{"src":"https://uploads.ungrounded.net/alternate/1801000/1801346_alternate_212052.360p.mp4?1673005000","type":"video/mp4"}]}}"#;

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://www.newgrounds.com/portal/view/1004201"),
            Some(Link::Movie("1004201".into()))
        );
        assert_eq!(
            link("https://www.newgrounds.com/portal/view/297383/format/flash"),
            Some(Link::Movie("297383".into()))
        );
        assert_eq!(
            link("https://www.newgrounds.com/audio/listen/549479"),
            Some(Link::Audio("549479".into()))
        );
        assert_eq!(
            link("https://www.newgrounds.com/art/view/someone/piece"),
            None
        );
        assert_eq!(link("https://www.newgrounds.com/"), None);
    }

    #[tokio::test]
    async fn movies_resolve_from_the_video_endpoint() {
        let mut fixture = Fixture::new("newgrounds", None);
        fixture.exchanges.push(get(
            "https://www.newgrounds.com/portal/view/1004201",
            200,
            "text/html",
            PAGE,
        ));
        fixture.exchanges.push(get(
            "https://www.newgrounds.com/portal/video/1004201",
            200,
            "application/json",
            VIDEO,
        ));
        let resolver = NewgroundsResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.newgrounds.com/portal/view/1004201").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.title.as_deref(), Some("Scrotum 1"));
        assert_eq!(resolved.uploader.as_deref(), Some("Someone"));
        assert_eq!(
            resolved.uploaded_at.unwrap().to_string(),
            "2023-01-04T02:33:16Z"
        );
        assert_eq!(resolved.variants.len(), 3);
        assert_eq!(resolved.variants[0].height, Some(1080));
        assert_eq!(resolved.variants[2].height, Some(360));
        assert_eq!(resolved.variants[0].video, Some(VideoCodec::H264));
        assert!(resolved.variants[0].url.as_str().contains("1080p.mp4"));
    }

    #[tokio::test]
    async fn legacy_files_audio_and_missing_pages_say_so() {
        let legacy = r#"<html><head><title>Metal Gear Awesome</title></head><body><script>embedController([{"url":"https:\/\/uploads.ungrounded.net\/297000\/297383_metalgear_awesome.swf","is_published":true}]);</script></body></html>"#;
        let mut fixture = Fixture::new("newgrounds", None);
        fixture.exchanges.push(get(
            "https://www.newgrounds.com/portal/view/297383",
            200,
            "text/html",
            legacy,
        ));
        fixture.exchanges.push(get(
            "https://www.newgrounds.com/portal/view/2",
            404,
            "text/html",
            "",
        ));
        let resolver = NewgroundsResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://www.newgrounds.com/portal/view/297383").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.variants.len(), 1);
        assert!(
            resolved.variants[0]
                .url
                .as_str()
                .ends_with("297383_metalgear_awesome.swf")
        );
        assert_eq!(resolved.title.as_deref(), Some("Metal Gear Awesome"));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.newgrounds.com/portal/view/2").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(&Url::parse("https://www.newgrounds.com/audio/listen/5").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("audio")),
            "{error}"
        );
    }

    #[tokio::test]
    #[ignore = "requires live Newgrounds and media CDN access"]
    async fn live_reported_movies_download_media() {
        use std::time::Duration;

        let http = Http::new(crate::http::HttpConfig::default());
        let resolver = NewgroundsResolver::new(http.clone());
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
            let resolved = tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&url))
                .await
                .expect("resolution timed out")
                .unwrap()
                .media()
                .unwrap();
            let variant = crate::plan::select_variant(
                &resolved.variants,
                &crate::config::Limits::default(),
                PLATFORM,
                resolved.media,
            )
            .unwrap();
            assert_eq!(variant.container, Some(Container::Mp4));
            let response = http
                .get(variant.url.clone())
                .platform(PLATFORM)
                .headers(&variant.headers)
                .timeout(Duration::from_secs(20))
                .send()
                .await
                .unwrap();
            let status = response.status;
            assert!(status.is_success(), "{link}: HTTP {status}");
            let (sample, _) = response.bytes_up_to(64 * 1024).await.unwrap();
            assert_eq!(
                sample.get(4..8),
                Some(b"ftyp".as_slice()),
                "expected MP4 media"
            );
            println!(
                "{}: {} variants, selected {:?}, HTTP {status}, {} media bytes read",
                resolved.id.unwrap(),
                resolved.variants.len(),
                variant.format_id,
                sample.len()
            );
        }
    }
}
