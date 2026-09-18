//! 20 Minuten (20min.ch) videos: the video pages' structured data names the video and
//! its player, and the player's files come from the site's video host by the video's
//! number: an HLS playlist with its renditions, and the high and low quality MP4 files.
//! Player embeds (`videoplayer.20min.ch?videoId=…`) carry only the number.

use async_trait::async_trait;
use jiff::Timestamp;
use serde_json::Value;
use url::Url;

use super::page::{Page, ld_objects_of_type};
use super::web::parse_iso_duration;
use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, Variant, VariantKind, clean_title, fetch_ok, hls, navigation_headers,
    probe_file,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "20min";
const HOST: &str = "https://unityvideo.appuser.ch/";
const PLAYER: &str = "https://videoplayer.20min.ch/";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A video page of the site, by its path.
    Page(String),
    /// The player, by the video's number.
    Player(String),
}

/// `uv` followed by digits, as the site numbers its videos.
pub fn video_number(text: &str) -> Option<String> {
    let text = text.trim();
    let digits = text.strip_prefix("uv")?;
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then(|| text.to_string())
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host == "videoplayer.20min.ch" {
        return url
            .query_pairs()
            .find(|(k, _)| k == "videoId")
            .and_then(|(_, v)| video_number(&v))
            .map(Link::Player);
    }
    if host != "20min.ch" && host != "www.20min.ch" {
        return None;
    }
    let mut segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    if segments
        .first()
        .is_some_and(|first| matches!(*first, "fr" | "it" | "en"))
    {
        segments.remove(0);
    }
    match segments.as_slice() {
        ["video" | "story" | "videotv", slug, ..] if !slug.is_empty() => {
            Some(Link::Page(url.path().to_string()))
        }
        _ => None,
    }
}

pub fn player_url(number: &str) -> Url {
    Url::parse(&format!("{PLAYER}?videoId={number}")).expect("valid")
}

/// The video number a structured-data record names, through its player link or file.
pub fn number_of(object: &Value) -> Option<String> {
    if let Some(number) = object["embedUrl"]
        .as_str()
        .and_then(|e| Url::parse(e).ok())
        .and_then(|e| match parse_link(&e) {
            Some(Link::Player(number)) => Some(number),
            _ => None,
        })
    {
        return Some(number);
    }
    let content = object["contentUrl"].as_str()?;
    let name = content.rsplit('/').next()?;
    let stem = name.strip_suffix(".mp4")?;
    video_number(
        stem.strip_suffix('h')
            .or_else(|| stem.strip_suffix('p'))
            .unwrap_or(stem),
    )
}

/// The video records a page's structured data carries, with their numbers.
pub fn videos_in(page: &Page) -> Vec<(String, Value)> {
    let ld = page.ld_json();
    let mut found: Vec<(String, Value)> = Vec::new();
    for object in ld_objects_of_type(&ld, "VideoObject") {
        if let Some(number) = number_of(object)
            && !found.iter().any(|(n, _)| *n == number)
        {
            found.push((number, object.clone()));
        }
    }
    found
}

fn playlist_url(number: &str) -> Url {
    Url::parse(&format!("{HOST}videos/{number}/playlist.m3u8")).expect("valid")
}

fn file_url(number: &str, suffix: &str) -> Url {
    Url::parse(&format!("{HOST}video/{number}{suffix}.mp4")).expect("valid")
}

pub struct TwentyMinResolver {
    http: Http,
}

impl TwentyMinResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// One video by its number, described by `record` when a page carried one.
    async fn video(
        &self,
        number: &str,
        record: Option<&Value>,
        page_url: Option<Url>,
        page_size: Option<(u32, u32)>,
        link: &Url,
    ) -> Result<Resolution, ResolveError> {
        let master = playlist_url(number);
        let mut variants = Vec::new();
        let mut subtitles = Vec::new();
        let mut duration = record
            .and_then(|r| r["duration"].as_str())
            .and_then(parse_iso_duration);
        match hls::expand(&self.http, &master, PLATFORM, BROWSER_UA, &[]).await {
            Ok(expanded) => {
                duration = duration.or(expanded.duration);
                subtitles = expanded.subtitles;
                variants.extend(expanded.variants);
            }
            Err(ResolveError::Http(error)) => return Err(ResolveError::Http(error)),
            Err(error) => {
                tracing::debug!(url = %master, %error, "the HLS playlist could not be read");
            }
        }
        let files = [("h", "high"), ("", "low")];
        let probes = futures::future::join_all(files.iter().map(|(suffix, _)| {
            let target = file_url(number, suffix);
            let http = &self.http;
            async move { probe_file(http, &target, PLATFORM, BROWSER_UA, &[]).await }
        }))
        .await;
        for ((suffix, label), probe) in files.iter().zip(probes) {
            let probed = match probe {
                Ok(probed) if probed.status.is_success() => probed,
                Ok(_) => continue,
                Err(error) => return Err(error),
            };
            let mut v = Variant::new(file_url(number, suffix), VariantKind::File);
            v.container = Some(Container::Mp4);
            v.video = Some(VideoCodec::H264);
            v.audio = Some(AudioCodec::Aac);
            v.size = probed.size;
            v.duration = duration;
            v.format_id = Some(format!("mp4-{label}"));
            v.label = Some(label.to_string());
            if *suffix == "h" {
                let recorded = match (
                    record.and_then(|r| r["width"].as_u64()),
                    record.and_then(|r| r["height"].as_u64()),
                ) {
                    (Some(w), Some(h)) => Some((w as u32, h as u32)),
                    _ => None,
                };
                if let Some((w, h)) = recorded.or(page_size) {
                    v.width = Some(w);
                    v.height = Some(h);
                }
            }
            variants.push(v);
        }
        if variants.is_empty() {
            return Err(ResolveError::NotFound(link.clone()));
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(number.to_string());
        resolved.title = record
            .and_then(|r| r["name"].as_str())
            .and_then(clean_title)
            .or_else(|| Some(number.to_string()));
        resolved.description = record
            .and_then(|r| r["description"].as_str())
            .and_then(clean_title);
        resolved.uploader = record
            .and_then(|r| r["author"].as_array())
            .map(|authors| {
                authors
                    .iter()
                    .filter_map(|a| a["name"].as_str().and_then(clean_title))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|names| !names.is_empty())
            .or_else(|| Some("20 Minuten".to_string()));
        resolved.uploaded_at = record
            .and_then(|r| r["uploadDate"].as_str())
            .and_then(|t| t.parse::<Timestamp>().ok());
        resolved.duration = duration;
        resolved.thumbnail = record
            .and_then(|r| {
                r["thumbnailUrl"]
                    .as_str()
                    .or_else(|| r["thumbnailUrl"][0].as_str())
            })
            .and_then(|u| Url::parse(u).ok());
        resolved.webpage_url = page_url.or_else(|| Some(player_url(number)));
        resolved.subtitles = subtitles;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn page(&self, link: &Url) -> Result<Resolution, ResolveError> {
        let fetched = fetch_ok(
            &self.http,
            link,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        let html = fetched.text();
        let (videos, title, page_size) = {
            let page = Page::parse(&html, &fetched.url);
            let size = match (
                page.meta("og:video:width")
                    .and_then(|w| w.parse::<u32>().ok()),
                page.meta("og:video:height")
                    .and_then(|h| h.parse::<u32>().ok()),
            ) {
                (Some(w), Some(h)) if w > 0 && h > 0 => Some((w, h)),
                _ => None,
            };
            (videos_in(&page), page.title(), size)
        };
        match videos.as_slice() {
            [] => Err(ResolveError::NotFound(link.clone())),
            [(number, record)] => {
                self.video(
                    number,
                    Some(record),
                    Some(fetched.url.clone()),
                    page_size,
                    link,
                )
                .await
            }
            many => Ok(Resolution::Playlist(Playlist {
                resolver: PLATFORM.to_string(),
                id: Some(fetched.url.path().trim_matches('/').to_string()),
                title,
                entries: many
                    .iter()
                    .map(|(number, record)| PlaylistEntry {
                        url: player_url(number),
                        title: record["name"].as_str().and_then(clean_title),
                        duration: record["duration"].as_str().and_then(parse_iso_duration),
                    })
                    .collect(),
                total: Some(many.len()),
            })),
        }
    }
}

#[async_trait]
impl Resolver for TwentyMinResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "20 Minuten",
            hosts: &["20min.ch", "videoplayer.20min.ch"],
            features: &["videos", "stories", "embeds"],
            formats: &["hls", "mp4"],
            session: SessionSupport::None,
            examples: &[
                "https://www.20min.ch/video/adoptions-serie-dachte-mami-liebt-mich-nicht-adoptierte-suchen-antworten-103468193",
                "https://videoplayer.20min.ch/?videoId=uv10924877&lang=de",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Page(_) => self.page(url).await,
            Link::Player(number) => self.video(&number, None, None, None, url).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Fixture;
    use std::time::Duration;

    const VIDEO: &str = "https://www.20min.ch/video/adoptions-serie-dachte-mami-liebt-mich-nicht-adoptierte-suchen-antworten-103468193";
    const PLAYER_LINK: &str = "https://videoplayer.20min.ch/?videoId=uv10924877&lang=de";

    fn resolver() -> TwentyMinResolver {
        let fixture = Fixture::parse(include_str!("twentymin_fixture.json")).unwrap();
        TwentyMinResolver::new(Http::replay(fixture))
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&url(s));
        assert_eq!(
            link(VIDEO),
            Some(Link::Page(
                "/video/adoptions-serie-dachte-mami-liebt-mich-nicht-adoptierte-suchen-antworten-103468193".into()
            ))
        );
        assert_eq!(
            link("https://www.20min.ch/fr/video/un-titre-123"),
            Some(Link::Page("/fr/video/un-titre-123".into()))
        );
        assert_eq!(
            link(
                "https://www.20min.ch/story/so-kommen-sie-bei-eis-und-schnee-sicher-an-557858045456"
            ),
            Some(Link::Page(
                "/story/so-kommen-sie-bei-eis-und-schnee-sicher-an-557858045456".into()
            ))
        );
        assert_eq!(link(PLAYER_LINK), Some(Link::Player("uv10924877".into())));
        assert_eq!(link("https://videoplayer.20min.ch/?videoId=abc"), None);
        assert_eq!(link("https://www.20min.ch/video"), None);
        assert_eq!(link("https://www.20min.ch/"), None);
        assert_eq!(link("https://20min.ch.evil.test/video/x-1"), None);
        assert_eq!(
            player_url("uv1").as_str(),
            "https://videoplayer.20min.ch/?videoId=uv1"
        );
    }

    #[test]
    fn records_name_their_video_number() {
        let by_embed = serde_json::json!({"embedUrl": "https://videoplayer.20min.ch?videoId=uv10924877&lang=de"});
        assert_eq!(number_of(&by_embed).as_deref(), Some("uv10924877"));
        let by_file = serde_json::json!({"contentUrl": "https://unityvideo.appuser.ch/video/uv10924877h.mp4"});
        assert_eq!(number_of(&by_file).as_deref(), Some("uv10924877"));
        assert_eq!(number_of(&serde_json::json!({"name": "x"})), None);
    }

    #[tokio::test]
    async fn video_pages_resolve_to_the_playlist_and_files() {
        let resolver = resolver();
        assert!(resolver.matches(&url(VIDEO)));
        let resolved = resolver
            .resolve(&url(VIDEO))
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("uv10924877"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Benj, Malin und Ina: Auf der Suche nach der Wahrheit über ihre Adoption")
        );
        assert!(resolved.description.is_some());
        assert_eq!(resolved.duration, Some(Duration::from_secs(990)));
        assert!(resolved.uploaded_at.is_some());
        assert!(resolved.thumbnail.is_some());
        assert_eq!(resolved.webpage_url.as_ref().map(Url::as_str), Some(VIDEO));
        let hls: Vec<&Variant> = resolved
            .variants
            .iter()
            .filter(|v| v.kind == VariantKind::Hls)
            .collect();
        assert_eq!(hls.len(), 2, "{:?}", resolved.variants);
        assert!(hls.iter().all(|v| v.bitrate.is_some()));
        let files: Vec<(&str, &str, Option<u64>)> = resolved
            .variants
            .iter()
            .filter(|v| v.kind == VariantKind::File)
            .map(|v| (v.label.as_deref().unwrap(), v.url.as_str(), v.size))
            .collect();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].0, "high");
        assert_eq!(
            files[0].1,
            "https://unityvideo.appuser.ch/video/uv10924877h.mp4"
        );
        assert_eq!(
            files[1].1,
            "https://unityvideo.appuser.ch/video/uv10924877.mp4"
        );
        assert!(files.iter().all(|(_, _, size)| size.is_some_and(|s| s > 0)));
        assert_eq!(
            resolved
                .variants
                .iter()
                .find(|v| v.label.as_deref() == Some("high"))
                .unwrap()
                .width,
            Some(1280)
        );
    }

    #[tokio::test]
    async fn player_links_resolve_by_number_alone() {
        let resolver = resolver();
        let resolved = resolver
            .resolve(&url(PLAYER_LINK))
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("uv10924877"));
        assert_eq!(resolved.title.as_deref(), Some("uv10924877"));
        assert_eq!(
            resolved.webpage_url.as_ref().map(Url::as_str),
            Some("https://videoplayer.20min.ch/?videoId=uv10924877")
        );
        assert_eq!(resolved.variants.len(), 4);
    }
}
