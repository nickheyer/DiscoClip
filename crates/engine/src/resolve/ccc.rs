//! media.ccc.de, the Chaos Computer Club's conference recordings: every talk page names
//! its event, which the public API describes with one recording per rendition and
//! language; a conference page lists its talks.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, SubtitleFormat, SubtitleTrack, Variant, clean_title, fetch, navigation_headers,
    status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "ccc";
const API: &str = "https://media.ccc.de/public/";

/// `/v/{slug}` (a talk) or `/c/{acronym}` (a conference).
static RE_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/(v|c)/([^/?#&]+)").unwrap());
static RE_EVENT_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-id=["'](\d+)["']"#).unwrap());
/// The event's GUID, which the API also answers to.
static RE_EVENT_GUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"/public/events/([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})"#)
        .unwrap()
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Talk { slug: String },
    Conference { acronym: String },
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "media.ccc.de" && host != "www.media.ccc.de" {
        return None;
    }
    let caps = RE_PATH.captures(url.path())?;
    Some(match &caps[1] {
        "v" => Link::Talk {
            slug: caps[2].to_string(),
        },
        _ => Link::Conference {
            acronym: caps[2].to_string(),
        },
    })
}

/// A recording that is a subtitle file, as a track.
fn recording_subtitle(recording: &Value) -> Option<SubtitleTrack> {
    let url = util::url_of(&recording["recording_url"], None)?;
    let format = match super::path_extension(&url).as_deref() {
        Some("srt") => SubtitleFormat::Srt,
        Some("vtt") => SubtitleFormat::Vtt,
        _ => return None,
    };
    let language = util::text(&recording["language"]).unwrap_or_else(|| "und".to_string());
    Some(SubtitleTrack {
        url,
        name: recording["label"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| Some(language.clone())),
        language,
        format,
        auto: false,
        headers: Vec::new(),
    })
}

/// A recording as a variant: a video file with its size and language, or an audio file.
fn recording_variant(recording: &Value) -> Option<Variant> {
    let url = util::url_of(&recording["recording_url"], None)?;
    if matches!(
        super::path_extension(&url).as_deref(),
        Some("srt" | "vtt" | "sbv" | "txt")
    ) {
        return None;
    }
    let folder = recording["folder"].as_str().unwrap_or("").to_string();
    let language = util::text(&recording["language"]);
    let mime = recording["mime_type"]
        .as_str()
        .unwrap_or("")
        .to_ascii_lowercase();
    if !mime.is_empty() && !mime.starts_with("video/") && !mime.starts_with("audio/") {
        return None;
    }
    let audio_only = mime.starts_with("audio/") || matches!(folder.as_str(), "mp3" | "opus");
    let mut variant = Variant::file(url);
    variant.audio_only = audio_only;
    variant.width = util::u32_of(&recording["width"]).filter(|w| *w > 0);
    variant.height = util::u32_of(&recording["height"]).filter(|h| *h > 0);
    // Sizes are given in whole megabytes.
    variant.size = util::uint(&recording["size"]).map(|mb| mb * 1024 * 1024);
    variant.duration = util::seconds(&recording["length"]);
    variant.language = language.clone();
    if audio_only {
        let codec = if folder == "opus" || mime.contains("opus") || mime.contains("ogg") {
            "opus"
        } else {
            "mp3"
        };
        variant.container = Some(Container::Other(codec.to_string()));
        variant.audio = Some(if codec == "opus" {
            AudioCodec::Opus
        } else {
            AudioCodec::Mp3
        });
    } else {
        variant.container = Some(if mime.contains("webm") {
            Container::Webm
        } else {
            Container::Mp4
        });
        variant.video = Some(if folder.contains("h264") || mime.contains("mp4") {
            VideoCodec::H264
        } else if folder.contains("av1") {
            VideoCodec::Av1
        } else {
            VideoCodec::Vp9
        });
        variant.audio = Some(if mime.contains("webm") {
            AudioCodec::Opus
        } else {
            AudioCodec::Aac
        });
    }
    variant.label = recording["label"].as_str().and_then(clean_title);
    variant.format_id = Some(match (&language, folder.is_empty()) {
        (Some(lang), false) => format!("{lang}-{folder}"),
        (Some(lang), true) => lang.clone(),
        (None, _) => folder.clone(),
    });
    Some(variant)
}

pub struct CccResolver {
    http: Http,
}

impl CccResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    async fn api(&self, path: &str, origin: &Url) -> Result<Value, ResolveError> {
        let api = Url::parse(&format!("{API}{path}"))
            .map_err(|e| ResolveError::malformed(origin, e.to_string()))?;
        let accept = [("accept".to_string(), "application/json".to_string())];
        let fetched = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &accept, MAX_PAGE).await?;
        if let Some(error) = status_error(fetched.status, origin) {
            return Err(error);
        }
        fetched.json(origin)
    }

    async fn resolve_talk(&self, slug: &str, url: &Url) -> Result<Resolution, ResolveError> {
        let page = fetch(
            &self.http,
            url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(page.status, url) {
            return Err(error);
        }
        let html = page.text();
        let event_id = util::search(&RE_EVENT_ID, &html)
            .or_else(|| util::search(&RE_EVENT_GUID, &html))
            .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let event = self.api(&format!("events/{event_id}"), url).await?;
        let mut variants: Vec<Variant> = event["recordings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(recording_variant)
            .collect();
        if variants.is_empty() {
            return Err(ResolveError::unavailable(
                url,
                "the event has no recordings",
            ));
        }
        variants.sort_by_key(|v| {
            std::cmp::Reverse((!v.audio_only, v.height.unwrap_or(0), v.size.unwrap_or(0)))
        });
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(event_id);
        resolved.title = event["title"].as_str().and_then(clean_title);
        resolved.description = event["description"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| event["subtitle"].as_str().and_then(clean_title));
        resolved.thumbnail = util::url_of(&event["thumb_url"], None)
            .or_else(|| util::url_of(&event["poster_url"], None));
        resolved.uploaded_at =
            util::time(&event["date"]).or_else(|| util::time(&event["release_date"]));
        resolved.duration =
            util::seconds(&event["length"]).or_else(|| util::seconds(&event["duration"]));
        resolved.uploader = event["persons"]
            .as_array()
            .map(|people| {
                people
                    .iter()
                    .filter_map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|s| !s.is_empty())
            .or_else(|| event["conference_title"].as_str().and_then(clean_title));
        resolved.uploader_url = util::url_of(&event["conference_url"], None).and_then(|api_url| {
            // The conference's API link names its acronym; its page is `/c/{acronym}`.
            let acronym = api_url.path_segments()?.next_back()?.to_string();
            Url::parse(&format!("https://media.ccc.de/c/{acronym}")).ok()
        });
        resolved.webpage_url =
            util::url_of(&event["frontend_link"], None).or_else(|| Some(url.clone()));
        resolved.subtitles = event["recordings"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(recording_subtitle)
            .collect();
        resolved.variants = variants;
        let _ = slug;
        Ok(Resolution::from(resolved))
    }

    async fn resolve_conference(
        &self,
        acronym: &str,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let conference = self.api(&format!("conferences/{acronym}"), url).await?;
        let entries: Vec<PlaylistEntry> = conference["events"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|event| {
                Some(PlaylistEntry {
                    url: util::url_of(&event["frontend_link"], None)?,
                    title: event["title"].as_str().and_then(clean_title),
                    duration: util::seconds(&event["length"])
                        .or_else(|| util::seconds(&event["duration"])),
                })
            })
            .collect();
        if entries.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let total = entries.len();
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(acronym.to_string()),
            title: conference["title"].as_str().and_then(clean_title),
            entries,
            total: Some(total),
        }))
    }
}

#[async_trait]
impl Resolver for CccResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "media.ccc.de",
            hosts: &["media.ccc.de"],
            features: &["videos", "audio", "conferences"],
            formats: &["mp4", "webm", "mp3", "opus"],
            session: SessionSupport::None,
            examples: &[
                "https://media.ccc.de/v/39c3-schlechte-karten-it-sicherheit-im-jahr-null-der-epa-fur-alle",
                "https://media.ccc.de/v/32c3-7368-shopshifting#download",
                "https://media.ccc.de/c/39c3",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Talk { slug } => self.resolve_talk(&slug, url).await,
            Link::Conference { acronym } => self.resolve_conference(&acronym, url).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};
    use serde_json::json;

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

    const TALK: &str =
        "https://media.ccc.de/v/39c3-schlechte-karten-it-sicherheit-im-jahr-null-der-epa-fur-alle";

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link(TALK),
            Some(Link::Talk {
                slug: "39c3-schlechte-karten-it-sicherheit-im-jahr-null-der-epa-fur-alle".into()
            })
        );
        assert_eq!(
            link("https://media.ccc.de/v/32c3-7368-shopshifting#download"),
            Some(Link::Talk {
                slug: "32c3-7368-shopshifting".into()
            })
        );
        assert_eq!(
            link("https://media.ccc.de/c/30c3"),
            Some(Link::Conference {
                acronym: "30c3".into()
            })
        );
        assert_eq!(link("https://media.ccc.de/b/congress"), None);
        assert_eq!(link("https://example.com/v/32c3-7368-shopshifting"), None);
    }

    #[tokio::test]
    async fn talks_resolve_to_their_recordings() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            TALK,
            200,
            "text/html",
            r#"<html><body><div class="player" data-id="2403"></div></body></html>"#.into(),
        ));
        fixture.exchanges.push(get(
            "https://media.ccc.de/public/events/2403",
            200,
            "application/json",
            json!({
                "guid": "2b5a6a8e-327e-594d-8f92-b91201d18a02", "title": "Schlechte Karten - IT-Sicherheit im Jahr null der ePA für alle",
                "description": "Die ePA.", "persons": ["Bianca Kastl"], "date": "2025-12-29T17:15:00.000+01:00", "length": 3619,
                "thumb_url": "https://static.media.ccc.de/media/congress/2025/2403.jpg", "frontend_link": TALK,
                "conference_title": "39C3", "conference_url": "https://api.media.ccc.de/public/conferences/39c3",
                "recordings": [
                    {"length": 3619, "mime_type": "video/mp4", "language": "deu", "folder": "h264-hd", "width": 1920, "height": 1080, "label": "deu 1080p", "size": 510, "recording_url": "https://cdn.media.ccc.de/congress/2025/h264-hd/39c3-2403-deu.mp4"},
                    {"length": 3619, "mime_type": "video/webm", "language": "eng", "folder": "webm-sd", "width": 1024, "height": 576, "size": 200, "recording_url": "https://cdn.media.ccc.de/congress/2025/webm-sd/39c3-2403-eng.webm"},
                    {"length": 3619, "mime_type": "audio/mpeg", "language": "deu", "folder": "mp3", "size": 50, "recording_url": "https://cdn.media.ccc.de/congress/2025/mp3/39c3-2403-deu.mp3"},
                    {"length": 3619, "mime_type": "audio/opus", "language": "deu", "folder": "opus", "recording_url": ""},
                    {"mime_type": "text/plain", "language": "eng", "folder": "", "label": "English subtitles", "recording_url": "https://cdn.media.ccc.de/congress/2025/39c3-2403.en.srt"}
                ]
            })
            .to_string(),
        ));
        fixture.exchanges.push(get(
            "https://media.ccc.de/v/nothing",
            200,
            "text/html",
            "<html><body>no player</body></html>".into(),
        ));
        let resolver = CccResolver::new(Http::replay(fixture));
        let url = Url::parse(TALK).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("2403"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Schlechte Karten - IT-Sicherheit im Jahr null der ePA für alle")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("Bianca Kastl"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://media.ccc.de/c/39c3"
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(3619)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1767024900)
        );
        assert_eq!(resolved.variants.len(), 3);
        let best = &resolved.variants[0];
        assert_eq!(best.height, Some(1080));
        assert_eq!(best.language.as_deref(), Some("deu"));
        assert_eq!(best.format_id.as_deref(), Some("deu-h264-hd"));
        assert_eq!(best.size, Some(510 * 1024 * 1024));
        assert_eq!(best.video, Some(VideoCodec::H264));
        assert_eq!(resolved.variants[1].container, Some(Container::Webm));
        assert_eq!(resolved.variants[1].video, Some(VideoCodec::Vp9));
        let audio = &resolved.variants[2];
        assert!(audio.audio_only);
        assert_eq!(audio.audio, Some(AudioCodec::Mp3));
        assert_eq!(resolved.subtitles.len(), 1);
        assert_eq!(resolved.subtitles[0].language, "eng");
        assert_eq!(resolved.subtitles[0].format, SubtitleFormat::Srt);
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://media.ccc.de/v/nothing").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn conferences_list_their_talks() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://media.ccc.de/public/conferences/39c3",
            200,
            "application/json",
            json!({"acronym": "39c3", "title": "39th Chaos Communication Congress", "events": [
                {"title": "SOS FAFO", "frontend_link": "https://media.ccc.de/v/39c3-sos-fafo", "length": 1800},
                {"title": "No link"}
            ]}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://media.ccc.de/public/conferences/nothing",
            404,
            "application/json",
            json!({"error": "not found"}).to_string(),
        ));
        let resolver = CccResolver::new(Http::replay(fixture));
        let Resolution::Playlist(playlist) = resolver
            .resolve(&Url::parse("https://media.ccc.de/c/39c3").unwrap())
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(
            playlist.title.as_deref(),
            Some("39th Chaos Communication Congress")
        );
        assert_eq!(playlist.entries.len(), 1);
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://media.ccc.de/v/39c3-sos-fafo"
        );
        assert_eq!(
            playlist.entries[0].duration,
            Some(Duration::from_secs(1800))
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://media.ccc.de/c/nothing").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
