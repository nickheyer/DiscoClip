//! Democracy Now! shows and stories, through the JSON block each page hands its player:
//! progressive video and audio files with their captions.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport,
    SubtitleFormat, SubtitleTrack, Tag, Variant, clean_title, fetch_ok, navigation_headers, page,
    path_extension, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "democracynow";

static RE_JSON_SCRIPT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<script[^>]+type="text/json"[^>]*>"#).unwrap());

/// The page path a link names, which is the show's or story's display id.
pub fn display_id(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    if host != "democracynow.org" {
        return None;
    }
    Some(url.path().trim_matches('/').to_string())
}

/// The player's JSON block on a page.
pub fn player_json(html: &str) -> Option<Value> {
    let tag = RE_JSON_SCRIPT.find(html)?;
    page::leading_json(&html[tag.end()..]).map(|(value, _)| value)
}

fn subtitle_format(url: &Url) -> SubtitleFormat {
    match path_extension(url).as_deref() {
        Some("srt") => SubtitleFormat::Srt,
        Some("xml") | Some("ttml") | Some("dfxp") => SubtitleFormat::Ttml,
        _ => SubtitleFormat::Vtt,
    }
}

pub struct DemocracynowResolver {
    http: Http,
}

impl DemocracynowResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for DemocracynowResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Democracy Now!",
            hosts: &["democracynow.org"],
            features: &["shows", "videos", "audio"],
            formats: &["mp4", "m4a"],
            media: &[MediaKind::Video, MediaKind::Audio],
            tags: &[Tag::News, Tag::Podcasts],
            session: SessionSupport::None,
            examples: &[
                "http://www.democracynow.org/shows/2015/7/3",
                "http://www.democracynow.org/2015/7/3/this_flag_comes_down_today_bree",
                "https://www.democracynow.org/shows/2001/9/11",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        display_id(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let display_id = display_id(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let fetched = fetch_ok(
            &self.http,
            url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        let html = fetched.text();
        let page = Page::parse(&html, &fetched.url);
        let data = player_json(&html)
            .ok_or_else(|| ResolveError::malformed(url, "the page has no player JSON"))?;
        let title = data["title"]
            .as_str()
            .and_then(clean_title)
            .ok_or_else(|| ResolveError::malformed(url, "the player JSON names no title"))?;
        let mut resolved = Resolved::new(PLATFORM);
        let mut video_id = None;
        for key in ["file", "audio", "video", "high_res_video"] {
            let Some(mut media_url) = util::url_of(&data[key], Some(url)) else {
                continue;
            };
            media_url.set_query(None);
            media_url.set_fragment(None);
            if video_id.is_none() {
                let basename = media_url
                    .path_segments()
                    .and_then(|mut s| s.next_back())
                    .unwrap_or("");
                let stem = basename
                    .rsplit_once('.')
                    .map(|(stem, _)| stem)
                    .unwrap_or(basename);
                let id = stem.strip_prefix("dn").unwrap_or(stem);
                if !id.is_empty() {
                    video_id = Some(id.to_string());
                }
            }
            let extension = path_extension(&media_url);
            let mut variant = Variant::file(media_url);
            variant.container = extension
                .as_deref()
                .and_then(Container::from_extension)
                .or_else(|| extension.clone().map(Container::Other));
            if key == "audio" {
                variant.audio_only = true;
                variant.audio = Some(match extension.as_deref() {
                    Some("mp3") => AudioCodec::Mp3,
                    _ => AudioCodec::Aac,
                });
            } else {
                variant.video = Some(VideoCodec::H264);
                variant.audio = Some(AudioCodec::Aac);
            }
            variant.format_id = Some(key.to_string());
            variant.label = Some(match key {
                "high_res_video" => "high resolution".to_string(),
                other => other.to_string(),
            });
            resolved.variants.push(variant);
        }
        if resolved.variants.is_empty() {
            return Err(ResolveError::unavailable(
                url,
                "the page offers no media file",
            ));
        }
        // The player names what the page carries: a show with only an audio file is audio.
        if resolved.variants.iter().all(|v| v.audio_only) {
            resolved.media = MediaKind::Audio;
        }
        if let Some(caption) = util::url_of(&data["caption_file"], Some(url)) {
            resolved.subtitles.push(SubtitleTrack {
                format: subtitle_format(&caption),
                url: caption,
                language: "en".into(),
                name: None,
                auto: false,
                headers: Vec::new(),
            });
        }
        for caption in data["captions"].as_array().into_iter().flatten() {
            let Some(track_url) = util::url_of(&caption["url"], Some(url)) else {
                continue;
            };
            let language = caption["language"]
                .as_str()
                .map(|l| l.trim().to_ascii_lowercase())
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| "en".into());
            resolved.subtitles.push(SubtitleTrack {
                format: subtitle_format(&track_url),
                url: track_url,
                language,
                name: None,
                auto: false,
                headers: Vec::new(),
            });
        }
        resolved.id = Some(video_id.unwrap_or(display_id));
        resolved.title = Some(title);
        resolved.description = page.meta("og:description").and_then(|d| clean_title(&d));
        resolved.thumbnail = util::url_of(&data["image"], Some(url));
        resolved.webpage_url = Some(fetched.url.clone());
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
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

    const SHOW: &str = r#"<html><head><meta property="og:description" content="A daily independent global news hour."></head><body>
        <script type="text/json" id="player-data">
        {"video":"https://democracynow.cachefly.net/democracynow/flash/dn2015-0703-001.mp4?t=1","high_res_video":"","audio":"/audio/dn2015-0703-001.m4a","image":"https://assets.democracynow.org/assets/default.jpg","captions":[{"url":"https://www.democracynow.org/resources/captions/shows/5075/English.vtt","language":"EN"}],"caption_file":"/resources/captions/5075.srt","chapters":"https://www.democracynow.org/resources/chapters/5075.vtt","title":"Daily Show for July 03, 2015","locale":"en"}
        </script></body></html>"#;

    #[test]
    fn links_are_read() {
        let id = |s: &str| display_id(&Url::parse(s).unwrap());
        assert_eq!(
            id("http://www.democracynow.org/shows/2015/7/3"),
            Some("shows/2015/7/3".into())
        );
        assert_eq!(
            id("http://www.democracynow.org/2015/7/3/this_flag_comes_down_today_bree"),
            Some("2015/7/3/this_flag_comes_down_today_bree".into())
        );
        assert_eq!(
            id("https://democracynow.org/shows/2015/7/3?x=1"),
            Some("shows/2015/7/3".into())
        );
        assert_eq!(id("https://example.org/shows/2015/7/3"), None);
        assert_eq!(id("ftp://www.democracynow.org/shows/2015/7/3"), None);
    }

    #[tokio::test]
    async fn shows_resolve_with_files_and_captions() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "http://www.democracynow.org/shows/2015/7/3",
            200,
            "text/html",
            SHOW.into(),
        ));
        let resolver = DemocracynowResolver::new(Http::replay(fixture));
        let url = Url::parse("http://www.democracynow.org/shows/2015/7/3").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("2015-0703-001"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Daily Show for July 03, 2015")
        );
        assert_eq!(
            resolved.description.as_deref(),
            Some("A daily independent global news hour.")
        );
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://assets.democracynow.org/assets/default.jpg"
        );
        assert_eq!(resolved.variants.len(), 2);
        let audio = &resolved.variants[0];
        assert_eq!(
            audio.url.as_str(),
            "http://www.democracynow.org/audio/dn2015-0703-001.m4a"
        );
        assert!(audio.audio_only);
        assert_eq!(audio.format_id.as_deref(), Some("audio"));
        let video = &resolved.variants[1];
        assert_eq!(
            video.url.as_str(),
            "https://democracynow.cachefly.net/democracynow/flash/dn2015-0703-001.mp4"
        );
        assert_eq!(video.container, Some(Container::Mp4));
        assert!(!video.audio_only);
        assert_eq!(resolved.media, MediaKind::Video);
        assert_eq!(resolved.subtitles.len(), 2);
        assert_eq!(
            resolved.subtitles[0].url.as_str(),
            "http://www.democracynow.org/resources/captions/5075.srt"
        );
        assert_eq!(resolved.subtitles[0].format, SubtitleFormat::Srt);
        assert_eq!(resolved.subtitles[0].language, "en");
        assert_eq!(resolved.subtitles[1].language, "en");
        assert_eq!(resolved.subtitles[1].format, SubtitleFormat::Vtt);
    }

    #[tokio::test]
    async fn shows_with_only_an_audio_file_are_audio() {
        let page = r#"<html><head><meta property="og:description" content="Democracy Now! for September 11, 2001."></head><body>
        <script type="text/json" id="player-data">
        {"video":"","high_res_video":"","audio":"https://www.archive.org/download/dn2001-0911/dn2001-0911-1_64kb.mp3","image":"https://assets.democracynow.org/assets/default.jpg","title":"Democracy Now! for September 11, 2001","locale":"en"}
        </script></body></html>"#;
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.democracynow.org/shows/2001/9/11",
            200,
            "text/html",
            page.into(),
        ));
        let resolver = DemocracynowResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.democracynow.org/shows/2001/9/11").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.media, MediaKind::Audio);
        assert_eq!(resolved.id.as_deref(), Some("2001-0911-1_64kb"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Democracy Now! for September 11, 2001")
        );
        assert_eq!(resolved.variants.len(), 1);
        let audio = &resolved.variants[0];
        assert!(audio.audio_only);
        assert_eq!(audio.container, Some(Container::Mp3));
        assert_eq!(audio.audio, Some(AudioCodec::Mp3));
        assert_eq!(
            audio.url.as_str(),
            "https://www.archive.org/download/dn2001-0911/dn2001-0911-1_64kb.mp3"
        );
    }

    #[tokio::test]
    async fn pages_without_a_player_and_missing_pages_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.democracynow.org/about",
            200,
            "text/html",
            "<html><body><p>About us</p></body></html>".into(),
        ));
        fixture.exchanges.push(get(
            "https://www.democracynow.org/gone",
            404,
            "text/html",
            "".into(),
        ));
        let resolver = DemocracynowResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://www.democracynow.org/about").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Malformed { detail, .. } if detail.contains("player JSON")),
            "{error}"
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.democracynow.org/gone").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
