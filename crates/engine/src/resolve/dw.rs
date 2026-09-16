//! Deutsche Welle (dw.com) videos and audio: every media page carries its structured
//! data, whose content link is the HLS playlist, and its app state, which names the
//! same playlist, the length and the poster.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport,
    Variant, clean_title, fetch, hls, navigation_headers, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container};

pub const PLATFORM: &str = "dw";

/// `/{lang}/{slug}/{av|video|audio|e}-{id}`, with any further path.
static RE_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(?:[^/]+/)+(?:av|video|audio|e)-(\d+)(?:-\d+)?(?:/.*)?$").unwrap());
/// `/{lang}/{slug}/a-{id}`: an article, whose own videos and audios its data lists.
static RE_ARTICLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(?:[^/]+/)+a-(\d+)(?:/.*)?$").unwrap());
/// `window.__APP_STATE__ = {…}`: the data a page renders from.
static RE_APP_STATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"window\.__APP_STATE__\s*=\s*").unwrap());
static RE_HLS_SRC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""hlsVideoSrc"\s*:\s*"([^"]+)""#).unwrap());
static RE_AUDIO_SRC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""mp3Src"\s*:\s*"([^"]+)""#).unwrap());
static RE_DURATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""durationIso8601"\s*:\s*"([^"]+)""#).unwrap());

pub fn media_id(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "dw.com" && host != "www.dw.com" && host != "m.dw.com" {
        return None;
    }
    RE_PATH.captures(url.path()).map(|caps| caps[1].to_string())
}

/// The article an `a-{id}` link names.
pub fn article_id(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "dw.com" && host != "www.dw.com" && host != "m.dw.com" {
        return None;
    }
    RE_ARTICLE.captures(url.path()).map(|caps| caps[1].to_string())
}

/// The videos and audios an article's data lists as its own: their ids, names and
/// page paths.
pub fn article_media(html: &str) -> Vec<(String, Option<String>, Option<String>)> {
    let Some(start) = RE_APP_STATE.find(html).map(|m| m.end()) else {
        return Vec::new();
    };
    let rest = &html[start..];
    let Some(end) = util::balanced_js_end(rest) else {
        return Vec::new();
    };
    let Ok(state) = serde_json::from_str::<serde_json::Value>(&rest[..end]) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    for (key, node) in state.as_object().into_iter().flatten() {
        if !key.contains("/content/article/") {
            continue;
        }
        let content = &node["data"]["content"];
        for list in ["videos", "audios", "livestreams"] {
            for item in content[list].as_array().into_iter().flatten() {
                let Some(id) = util::text(&item["id"]) else {
                    continue;
                };
                found.push((
                    id,
                    item["name"].as_str().or(item["title"].as_str()).and_then(clean_title),
                    item["namedUrl"].as_str().or(item["permaLinkUrl"].as_str()).map(String::from),
                ));
            }
        }
    }
    found
}

pub struct DwResolver {
    http: Http,
}

impl DwResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// An article: its one video or audio is that media; several are a playlist of
    /// their pages.
    async fn resolve_article(&self, article: &str, url: &Url) -> Result<Resolution, ResolveError> {
        let fetched = fetch(&self.http, url, PLATFORM, BROWSER_UA, &super::navigation_headers(), MAX_PAGE).await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        let html = fetched.text();
        let media = article_media(&html);
        let entries: Vec<super::PlaylistEntry> = media
            .iter()
            .filter_map(|(id, name, path)| {
                let page = match path {
                    Some(path) => util::join_url(Some(&fetched.url), path)?,
                    None => Url::parse(&format!("https://www.dw.com/en/a/av-{id}")).ok()?,
                };
                Some(super::PlaylistEntry {
                    url: page,
                    title: name.clone(),
                    duration: None,
                })
            })
            .collect();
        match entries.len() {
            0 => Err(ResolveError::unavailable(url, "the article carries no video or audio of its own")),
            1 => Err(ResolveError::Redirect(entries.into_iter().next().expect("one").url)),
            count => Ok(Resolution::Playlist(super::Playlist {
                resolver: PLATFORM.into(),
                id: Some(format!("a-{article}")),
                title: super::page::Page::parse(&html, &fetched.url)
                    .meta("og:title")
                    .and_then(|t| clean_title(&t)),
                entries,
                total: Some(count),
            }))
        }
    }
}

#[async_trait]
impl Resolver for DwResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Deutsche Welle",
            hosts: &["dw.com"],
            features: &["videos", "audio"],
            formats: &["hls", "mp3"],
            session: SessionSupport::None,
            examples: &[
                "https://www.dw.com/en/intelligent-light/video-19112290",
                "http://www.dw.com/en/intelligent-light/av-19112290",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        media_id(url).is_some() || article_id(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        if media_id(url).is_none()
            && let Some(article) = article_id(url)
        {
            return self.resolve_article(&article, url).await;
        }
        let id = media_id(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let fetched = fetch(
            &self.http,
            url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        let html = fetched.text();
        let (ld_media, title, description, thumbnail, uploaded_at, ld_duration) = {
            let page = Page::parse(&html, &fetched.url);
            let ld = page
                .ld_json()
                .into_iter()
                .find(|ld| matches!(ld["@type"].as_str(), Some("VideoObject") | Some("AudioObject")));
            let ld_media = ld
                .as_ref()
                .and_then(|ld| ld["contentUrl"].as_str().or(ld["embedUrl"].as_str()))
                .and_then(|link| util::join_url(Some(&fetched.url), link));
            (
                ld_media,
                ld.as_ref()
                    .and_then(|ld| ld["name"].as_str())
                    .and_then(clean_title)
                    .or_else(|| page.meta("og:title").and_then(|t| clean_title(&t)))
                    .or_else(|| page.title()),
                ld.as_ref()
                    .and_then(|ld| ld["description"].as_str())
                    .and_then(clean_title)
                    .or_else(|| page.meta("og:description").and_then(|d| clean_title(&d))),
                ld.as_ref()
                    .and_then(|ld| util::url_of(&ld["thumbnailUrl"], None))
                    .or_else(|| page.meta("og:image").and_then(|t| util::join_url(Some(&fetched.url), &t))),
                ld.as_ref().and_then(|ld| util::time(&ld["uploadDate"])),
                ld.as_ref()
                    .and_then(|ld| ld["duration"].as_str())
                    .and_then(util::parse_duration),
            )
        };
        let hls_source = util::search(&RE_HLS_SRC, &html)
            .map(|s| s.replace("\\/", "/"))
            .and_then(|s| util::join_url(Some(&fetched.url), &s));
        let audio_source = util::search(&RE_AUDIO_SRC, &html)
            .map(|s| s.replace("\\/", "/"))
            .and_then(|s| util::join_url(Some(&fetched.url), &s));
        let mut resolved = Resolved::new(PLATFORM);
        let mut failure = None;
        let playlist = hls_source
            .clone()
            .or_else(|| ld_media.clone().filter(|m| m.path().ends_with(".m3u8")));
        if let Some(playlist) = playlist {
            match hls::expand(&self.http, &playlist, PLATFORM, BROWSER_UA, &[]).await {
                Ok(expanded) => {
                    resolved.variants.extend(expanded.variants);
                    resolved.subtitles.extend(expanded.subtitles);
                    resolved.duration = expanded.duration;
                    resolved.live |= expanded.live;
                }
                Err(error) => failure = Some(error),
            }
        }
        for file in [audio_source, ld_media.filter(|m| !m.path().ends_with(".m3u8"))].into_iter().flatten() {
            if resolved.variants.iter().any(|v| v.url == file) {
                continue;
            }
            let extension = super::path_extension(&file).unwrap_or_default();
            let mut variant = Variant::file(file);
            variant.audio_only = matches!(extension.as_str(), "mp3" | "m4a" | "aac" | "ogg");
            if variant.audio_only {
                variant.container = Some(Container::Other(extension.clone()));
                variant.audio = Some(if extension == "mp3" { AudioCodec::Mp3 } else { AudioCodec::Aac });
            } else {
                variant.container = Container::from_extension(&extension);
            }
            variant.format_id = Some(format!("http-{extension}"));
            resolved.variants.push(variant);
        }
        if resolved.variants.is_empty() {
            return Err(failure.unwrap_or_else(|| ResolveError::NotFound(url.clone())));
        }
        resolved.id = Some(id);
        resolved.title = title;
        resolved.description = description;
        resolved.thumbnail = thumbnail;
        resolved.uploaded_at = uploaded_at;
        resolved.duration = util::search(&RE_DURATION, &html)
            .and_then(|d| util::parse_duration(&d))
            .or(ld_duration)
            .or(resolved.duration);
        resolved.uploader = Some("DW".to_string());
        resolved.webpage_url = Some(fetched.url.clone());
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};

    fn get(url: &str, status: u16, content_type: &str, body: String) -> Exchange {
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
                body: RecordedBody::Text(body),
                truncated: false,
            },
        }
    }

    #[test]
    fn links_are_read() {
        let id = |s: &str| media_id(&Url::parse(s).unwrap());
        assert_eq!(id("http://www.dw.com/en/intelligent-light/av-19112290"), Some("19112290".into()));
        assert_eq!(id("https://www.dw.com/en/intelligent-light/video-19112290"), Some("19112290".into()));
        assert_eq!(id("http://www.dw.com/en/documentaries-welcome-to-the-90s-2016-05-21/e-19220158-9798"), Some("19220158".into()));
        assert_eq!(id("https://www.dw.com/de/nachrichten/audio-12345"), Some("12345".into()));
        assert_eq!(id("http://www.dw.com/en/no-hope/a-19111009"), None);
        assert_eq!(id("https://example.com/en/x/av-1"), None);
    }

    #[tokio::test]
    async fn videos_resolve_from_the_page_data() {
        let page = concat!(
            r#"<html><head><meta property="og:title" content="Intelligent light"><script type="application/ld+json">{"@type":"VideoObject","name":"Intelligent light","description":"An intelligent lighting system.","thumbnailUrl":"https://static.dw.com/image/19112288_605.jpg","uploadDate":"2016-06-03T01:27:02Z","duration":"PT3M14S","contentUrl":"https://hlsvod.dw.com/i/Events/mp4/tt/licht_,sd,hd,.mp4.csmil/master.m3u8"}</script></head>"#,
            r#"<body><script>window.__APP_STATE__={"x":{"hlsVideoSrc":"https:\/\/hlsvod.dw.com\/i\/Events\/mp4\/tt\/licht_,sd,hd,.mp4.csmil\/master.m3u8","durationIso8601":"PT3M14S"}};</script></body></html>"#
        );
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get("https://www.dw.com/en/intelligent-light/video-19112290", 200, "text/html", page.into()));
        fixture.exchanges.push(get(
            "https://hlsvod.dw.com/i/Events/mp4/tt/licht_,sd,hd,.mp4.csmil/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1500000,RESOLUTION=1280x720\nhttps://hlsvod.dw.com/i/Events/mp4/tt/hd.m3u8\n".into(),
        ));
        fixture.exchanges.push(get(
            "https://hlsvod.dw.com/i/Events/mp4/tt/hd.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n0.ts\n#EXT-X-ENDLIST\n".into(),
        ));
        fixture.exchanges.push(get(
            "https://www.dw.com/en/gone/video-1",
            200,
            "text/html",
            "<html><body>nothing</body></html>".into(),
        ));
        let resolver = DwResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.dw.com/en/intelligent-light/video-19112290").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("19112290"));
        assert_eq!(resolved.title.as_deref(), Some("Intelligent light"));
        assert_eq!(resolved.description.as_deref(), Some("An intelligent lighting system."));
        assert_eq!(resolved.duration, Some(Duration::from_secs(194)));
        assert_eq!(resolved.uploaded_at.map(|t| t.as_second()), Some(1464917222));
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://static.dw.com/image/19112288_605.jpg"
        );
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].height, Some(720));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.dw.com/en/gone/video-1").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
