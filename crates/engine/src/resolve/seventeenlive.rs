//! 17LIVE (17.live) live rooms, clips and recordings, through the APIs the web app reads:
//! a room that is on air lists its HTTP-FLV pulls from every CDN by quality, a clip its MP4
//! source and transcode, and a recording its HLS playlist beside the source and re-encoded
//! MP4 files.

use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Tag, Variant,
    VariantKind, clean_title, fetch, hls,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "17live";
const SITE: &str = "https://17.live/";
const API: &str = "https://api-dsa.17app.co/api/v1/";
const WAP_API: &str = "https://wap-api.17app.co/api/v1/";
/// Where the API's bare picture file names live.
const PICTURES: &str = "https://cdn.17app.co/";

/// The pulls a live room offers, best first: the source, the enhanced renditions, the
/// default pull, its H.264 transcode, and the low quality rendition.
const LIVE_PULLS: [(&str, &str); 8] = [
    ("urlHighQuality", "source"),
    ("urlQualityEnhancedHD", "enhanced HD"),
    ("urlLowBitrateHD", "low bitrate HD"),
    ("url", "default"),
    ("webUrl", "web"),
    ("url264", "h264"),
    ("urlLowQuality", "low"),
    ("webUrlLowQuality", "web low"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A live room by its number: `/live/<room>` and the streamer's `/profile/r/<room>`.
    Live(String),
    /// `/profile/r/<room>/clip/<id>`.
    Clip { room: String, id: String },
    /// `/vod/<room>/<id>`.
    Vod { room: String, id: String },
}

impl Link {
    pub fn page(&self) -> Url {
        let path = match self {
            Link::Live(room) => format!("live/{room}"),
            Link::Clip { room, id } => format!("profile/r/{room}/clip/{id}"),
            Link::Vod { room, id } => format!("vod/{room}/{id}"),
        };
        Url::parse(&format!("{SITE}{path}")).expect("valid")
    }
}

fn is_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "17.live" && host != "www.17.live" {
        return None;
    }
    let mut segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    // The site prefixes its paths with the viewer's language: `/ja/live/...`.
    if segments
        .first()
        .is_some_and(|first| !matches!(*first, "live" | "profile" | "vod"))
    {
        segments.remove(0);
    }
    match segments.as_slice() {
        ["live", room] if is_digits(room) => Some(Link::Live(room.to_string())),
        ["profile", "r", room] if is_digits(room) => Some(Link::Live(room.to_string())),
        ["profile", "r", room, "clip", id] if is_digits(room) && is_token(id) => Some(Link::Clip {
            room: room.to_string(),
            id: id.to_string(),
        }),
        ["vod", room, id] if is_token(room) && is_token(id) => Some(Link::Vod {
            room: room.to_string(),
            id: id.to_string(),
        }),
        _ => None,
    }
}

fn picture(value: &Value) -> Option<Url> {
    let name = value.as_str()?.trim();
    if name.is_empty() {
        return None;
    }
    if name.starts_with("http://") || name.starts_with("https://") {
        return Url::parse(name).ok();
    }
    Url::parse(&format!("{PICTURES}{name}")).ok()
}

fn unix(value: &Value) -> Option<Timestamp> {
    value
        .as_i64()
        .filter(|t| *t > 0)
        .and_then(|t| Timestamp::from_second(t).ok())
}

fn seconds(value: &Value) -> Option<Duration> {
    value
        .as_f64()
        .filter(|d| *d > 0.0)
        .map(Duration::from_secs_f64)
}

fn streamer(info: &Value) -> Option<String> {
    ["displayName", "name", "openID"]
        .iter()
        .find_map(|key| info[key].as_str().and_then(clean_title))
}

fn mp4(url: Url, format_id: &str, label: &str, referer: &Url) -> Variant {
    let mut v = Variant::new(url, VariantKind::File);
    v.container = Some(Container::Mp4);
    v.video = Some(VideoCodec::H264);
    v.audio = Some(AudioCodec::Aac);
    v.format_id = Some(format_id.to_string());
    v.label = Some(label.to_string());
    v.headers.push(("referer".to_string(), referer.to_string()));
    v
}

/// The pulls a live room's `rtmpUrls` list: every CDN in the order the site prefers
/// them, each by quality, without repeating a link.
pub fn live_variants(room: &Value, referer: &Url) -> Vec<Variant> {
    let mut variants: Vec<Variant> = Vec::new();
    for (index, provider) in room["rtmpUrls"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        for (key, quality) in LIVE_PULLS {
            let Some(url) = provider[key].as_str().and_then(|u| Url::parse(u).ok()) else {
                continue;
            };
            if !matches!(url.scheme(), "http" | "https") || variants.iter().any(|v| v.url == url) {
                continue;
            }
            let mut v = Variant::new(url, VariantKind::File);
            v.container = Some(Container::Flv);
            v.audio = Some(AudioCodec::Aac);
            if key == "url264" {
                v.video = Some(VideoCodec::H264);
            }
            v.live = true;
            v.format_id = Some(format!("{}-{key}", index + 1));
            v.label = Some(match provider["provider"].as_i64() {
                Some(id) => format!("{quality}, CDN {id}"),
                None => quality.to_string(),
            });
            v.headers.push(("referer".to_string(), referer.to_string()));
            variants.push(v);
        }
    }
    variants
}

/// A clip's files: the source upload, then the site's transcode.
pub fn clip_variants(clip: &Value, referer: &Url) -> Vec<Variant> {
    let mut variants = Vec::new();
    for (key, format_id, label) in [
        ("srcVideoURL", "source", "source"),
        ("videoURL", "video", "transcoded"),
    ] {
        let Some(url) = clip[key].as_str().and_then(|u| Url::parse(u).ok()) else {
            continue;
        };
        if variants.iter().any(|v: &Variant| v.url == url) {
            continue;
        }
        let mut v = mp4(url, format_id, label, referer);
        v.duration = seconds(&clip["duration"]);
        variants.push(v);
    }
    variants
}

/// A recording's MP4 files: the source upload, then the 1280 and 640 re-encodes.
pub fn vod_files(vod: &Value, referer: &Url) -> Vec<Variant> {
    let mut variants = Vec::new();
    for (key, format_id, label) in [
        ("videoURL", "source", "source"),
        ("videoReencode1280", "reencode1280", "1280"),
        ("videoReencode640", "reencode640", "640"),
    ] {
        let Some(url) = vod[key].as_str().and_then(|u| Url::parse(u).ok()) else {
            continue;
        };
        if variants.iter().any(|v: &Variant| v.url == url) {
            continue;
        }
        let mut v = mp4(url, format_id, label, referer);
        v.duration = seconds(&vod["duration"]);
        variants.push(v);
    }
    variants
}

pub struct SeventeenLiveResolver {
    http: Http,
}

impl SeventeenLiveResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// GETs an API resource as the web app does, turning the API's own error answers
    /// into resolver errors for `link`.
    async fn api(&self, api: &Url, page: &Url, link: &Url) -> Result<Value, ResolveError> {
        let headers = [
            ("referer".to_string(), page.to_string()),
            ("accept".to_string(), "application/json".to_string()),
        ];
        let fetched = fetch(&self.http, api, PLATFORM, BROWSER_UA, &headers, MAX_PAGE).await?;
        match fetched.status.as_u16() {
            200..=299 => fetched.json(link),
            404 | 410 => Err(ResolveError::NotFound(link.clone())),
            429 => Err(ResolveError::RateLimited(link.clone())),
            status => {
                let message = fetched
                    .json(link)
                    .ok()
                    .and_then(|v| v["errorMessage"].as_str().map(str::to_string))
                    .filter(|m| !m.trim().is_empty());
                Err(match message {
                    Some(message) if message.starts_with("invalid") => {
                        ResolveError::NotFound(link.clone())
                    }
                    Some(message) => ResolveError::unavailable(
                        link,
                        format!("the API answered HTTP {status}: {message}"),
                    ),
                    None => {
                        ResolveError::unavailable(link, format!("the API answered HTTP {status}"))
                    }
                })
            }
        }
    }

    async fn live(&self, room: &str, link: &Url) -> Result<Resolution, ResolveError> {
        let page = Link::Live(room.to_string()).page();
        let api = Url::parse(&format!("{API}lives/{room}")).expect("valid");
        let data = self.api(&api, &page, link).await?;
        let variants = live_variants(&data, &page);
        if variants.is_empty() {
            let who = streamer(&data["userInfo"]).unwrap_or_else(|| format!("room {room}"));
            return Err(ResolveError::unavailable(
                link,
                match unix(&data["endTime"]) {
                    Some(ended) => format!("{who} is not live; the last stream ended at {ended}"),
                    None => format!("{who} is not live"),
                },
            ));
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(room.to_string());
        resolved.uploader = streamer(&data["userInfo"]);
        resolved.title = data["caption"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| resolved.uploader.as_ref().map(|who| format!("{who} live")));
        resolved.description = data["userInfo"]["bio"].as_str().and_then(clean_title);
        resolved.uploader_url =
            Some(Url::parse(&format!("{SITE}profile/r/{room}")).expect("valid"));
        resolved.uploaded_at = unix(&data["beginTime"]);
        resolved.duration = seconds(&data["duration"]);
        resolved.thumbnail = picture(&data["coverPhoto"])
            .or_else(|| picture(&data["thumbnail"]))
            .or_else(|| picture(&data["userInfo"]["picture"]));
        resolved.webpage_url = Some(page);
        resolved.live = true;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn clip(&self, room: &str, id: &str, link: &Url) -> Result<Resolution, ResolveError> {
        let page = Link::Clip {
            room: room.to_string(),
            id: id.to_string(),
        }
        .page();
        let api = Url::parse(&format!("{API}clips/{id}")).expect("valid");
        let clip = self.api(&api, &page, link).await?;
        if clip["isDeleted"].as_i64().is_some_and(|d| d != 0) {
            return Err(ResolveError::unavailable(link, "the clip was deleted"));
        }
        let variants = clip_variants(&clip, &page);
        if variants.is_empty() {
            return Err(ResolveError::NotFound(link.clone()));
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = clip["clipID"]
            .as_str()
            .map(str::to_string)
            .or_else(|| Some(id.to_string()));
        resolved.uploader = streamer(&clip["userInfo"]);
        resolved.description = clip["caption"].as_str().and_then(clean_title);
        resolved.title = clip["caption"]
            .as_str()
            .and_then(|c| c.lines().find_map(clean_title))
            .or_else(|| {
                resolved
                    .uploader
                    .as_ref()
                    .map(|who| format!("{who}'s clip"))
            });
        resolved.uploader_url =
            Some(Url::parse(&format!("{SITE}profile/r/{room}")).expect("valid"));
        resolved.uploaded_at = unix(&clip["createdAt"]);
        resolved.duration = seconds(&clip["duration"]);
        resolved.thumbnail = picture(&clip["imageURL"]);
        resolved.webpage_url = Some(page);
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn vod(&self, room: &str, id: &str, link: &Url) -> Result<Resolution, ResolveError> {
        let page = Link::Vod {
            room: room.to_string(),
            id: id.to_string(),
        }
        .page();
        let api = Url::parse(&format!("{WAP_API}vods/{id}")).expect("valid");
        let vod = self.api(&api, &page, link).await?;
        if vod["isDeleted"].as_i64().is_some_and(|d| d != 0) {
            return Err(ResolveError::unavailable(link, "the recording was deleted"));
        }
        if let Some(expired) = unix(&vod["expiredTime"])
            && expired < Timestamp::now()
        {
            return Err(ResolveError::unavailable(
                link,
                format!("the recording expired at {expired}"),
            ));
        }
        let mut variants = Vec::new();
        let mut subtitles = Vec::new();
        let mut duration = seconds(&vod["duration"]);
        if let Some(master) = vod["vodURL"].as_str().and_then(|u| Url::parse(u).ok()) {
            let headers = [("referer".to_string(), page.to_string())];
            let expanded = hls::expand(&self.http, &master, PLATFORM, BROWSER_UA, &headers).await?;
            duration = duration.or(expanded.duration);
            subtitles = expanded.subtitles;
            variants.extend(expanded.variants.into_iter().map(|mut v| {
                v.headers.push(("referer".to_string(), page.to_string()));
                v
            }));
        }
        variants.extend(vod_files(&vod, &page));
        if variants.is_empty() {
            return Err(match vod["status"].as_str() {
                Some(status) if status != "100%" => ResolveError::unavailable(
                    link,
                    format!("the recording is still being processed ({status})"),
                ),
                _ => ResolveError::NotFound(link.clone()),
            });
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = vod["vodID"]
            .as_str()
            .map(str::to_string)
            .or_else(|| Some(id.to_string()));
        resolved.uploader = streamer(&vod["userInfo"]);
        resolved.title = vod["title"].as_str().and_then(clean_title).or_else(|| {
            resolved
                .uploader
                .as_ref()
                .map(|who| format!("{who}'s recording"))
        });
        resolved.description = vod["description"].as_str().and_then(clean_title);
        resolved.uploader_url = vod["userInfo"]["roomID"]
            .as_i64()
            .and_then(|r| Url::parse(&format!("{SITE}profile/r/{r}")).ok());
        resolved.uploaded_at = unix(&vod["createdAt"]);
        resolved.duration = duration;
        resolved.thumbnail = picture(&vod["imageURL"]);
        resolved.webpage_url = Some(page);
        resolved.subtitles = subtitles;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }
}

#[async_trait]
impl Resolver for SeventeenLiveResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "17LIVE",
            hosts: &["17.live"],
            features: &["live", "clips", "recordings"],
            formats: &["flv", "mp4", "hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::Live],
            session: SessionSupport::None,
            examples: &[
                "https://17.live/profile/r/1789280/clip/1bHQSK8KUieruFXaCH4A4upCzlN",
                "https://17.live/ja/vod/27323042/2cf84520-e65e-4b22-891e-1d3a00b0f068",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Live(room) => self.live(&room, url).await,
            Link::Clip { room, id } => self.clip(&room, &id, url).await,
            Link::Vod { room, id } => self.vod(&room, &id, url).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Fixture;

    const CLIP: &str = "https://17.live/profile/r/1789280/clip/1bHQSK8KUieruFXaCH4A4upCzlN";
    const VOD: &str = "https://17.live/ja/vod/27323042/2cf84520-e65e-4b22-891e-1d3a00b0f068";

    fn resolver() -> SeventeenLiveResolver {
        let fixture = Fixture::parse(include_str!("seventeenlive_fixture.json")).unwrap();
        SeventeenLiveResolver::new(Http::replay(fixture))
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&url(s));
        assert_eq!(
            link("https://17.live/live/3773096"),
            Some(Link::Live("3773096".into()))
        );
        assert_eq!(
            link("https://17.live/ja/live/3773096?foo=bar"),
            Some(Link::Live("3773096".into()))
        );
        assert_eq!(
            link("https://17.live/en/profile/r/1789280"),
            Some(Link::Live("1789280".into()))
        );
        assert_eq!(
            link(CLIP),
            Some(Link::Clip {
                room: "1789280".into(),
                id: "1bHQSK8KUieruFXaCH4A4upCzlN".into()
            })
        );
        assert_eq!(
            link("https://www.17.live/ja/profile/r/1789280/clip/1bHQSK8KUieruFXaCH4A4upCzlN"),
            Some(Link::Clip {
                room: "1789280".into(),
                id: "1bHQSK8KUieruFXaCH4A4upCzlN".into()
            })
        );
        assert_eq!(
            link(VOD),
            Some(Link::Vod {
                room: "27323042".into(),
                id: "2cf84520-e65e-4b22-891e-1d3a00b0f068".into()
            })
        );
        assert_eq!(link("https://17.live/live/abc"), None);
        assert_eq!(link("https://17.live/ja/"), None);
        assert_eq!(link("https://17.live/ja/profile/r/1789280/clips"), None);
        assert_eq!(link("https://17.live.evil.test/live/3773096"), None);
        assert_eq!(
            Link::Vod {
                room: "1".into(),
                id: "a".into()
            }
            .page()
            .as_str(),
            "https://17.live/vod/1/a"
        );
    }

    #[tokio::test]
    async fn clips_resolve_to_their_source_and_transcode() {
        let resolver = resolver();
        assert!(resolver.matches(&url(CLIP)));
        let resolved = resolver.resolve(&url(CLIP)).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("1bHQSK8KUieruFXaCH4A4upCzlN"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("マチ戦隊 第一次 バスターコール")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("マチコ先生🦋Class💋"));
        assert_eq!(
            resolved.uploader_url.as_ref().map(Url::as_str),
            Some("https://17.live/profile/r/1789280")
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(58)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1588286538)
        );
        assert_eq!(
            resolved.thumbnail.as_ref().map(Url::as_str),
            Some("http://cdn.17app.co/go-prod/clip/1bHQSK8KUieruFXaCH4A4upCzlN.jpg")
        );
        assert!(!resolved.live);
        assert_eq!(
            resolved
                .variants
                .iter()
                .map(|v| (v.format_id.as_deref().unwrap(), v.url.as_str()))
                .collect::<Vec<_>>(),
            vec![
                (
                    "source",
                    "http://cdn.17app.co/go-prod/clip/1bHQSK8KUieruFXaCH4A4upCzlN.mp4"
                ),
                (
                    "video",
                    "http://cdn.17app.co/vod/prod/1bHQSK8KUieruFXaCH4A4upCzlN/3ce5bd7c65452c566d54e5ec31515059.mp4"
                ),
            ]
        );
        let source = &resolved.variants[0];
        assert_eq!(source.container, Some(Container::Mp4));
        assert_eq!(source.video, Some(VideoCodec::H264));
        assert_eq!(
            source.headers,
            vec![(
                "referer".to_string(),
                "https://17.live/profile/r/1789280/clip/1bHQSK8KUieruFXaCH4A4upCzlN".to_string()
            )]
        );
    }

    #[tokio::test]
    async fn recordings_resolve_to_hls_renditions_and_mp4_files() {
        let resolver = resolver();
        let resolved = resolver.resolve(&url(VOD)).await.unwrap().media().unwrap();
        assert_eq!(
            resolved.id.as_deref(),
            Some("2cf84520-e65e-4b22-891e-1d3a00b0f068")
        );
        assert_eq!(
            resolved.title.as_deref(),
            Some("オールナイトニッポン0アフタートーク(2025/03/03_FRUITS ZIPPER)")
        );
        assert_eq!(
            resolved.uploader.as_deref(),
            Some("オールナイトニッポンOfficial_アーカイブ")
        );
        assert_eq!(
            resolved.uploader_url.as_ref().map(Url::as_str),
            Some("https://17.live/profile/r/27323042")
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(549)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1741058645)
        );
        let hls: Vec<&Variant> = resolved
            .variants
            .iter()
            .filter(|v| v.kind == VariantKind::Hls)
            .collect();
        assert_eq!(hls.len(), 3, "{:?}", resolved.variants);
        assert!(hls.iter().all(|v| v.bitrate.is_some()));
        assert!(
            hls.iter()
                .all(|v| v.url.as_str().starts_with("http://cdn.17app.co/vod/prod/"))
        );
        let files: Vec<(&str, &str)> = resolved
            .variants
            .iter()
            .filter(|v| v.kind == VariantKind::File)
            .map(|v| (v.format_id.as_deref().unwrap(), v.url.as_str()))
            .collect();
        assert_eq!(
            files,
            vec![
                (
                    "source",
                    "https://storage.googleapis.com/17app-vod-prod/2cf84520-e65e-4b22-891e-1d3a00b0f068.mp4"
                ),
                (
                    "reencode1280",
                    "http://cdn.17app.co/vod/prod/2cf84520-e65e-4b22-891e-1d3a00b0f068/reencode-1280.mp4"
                ),
                (
                    "reencode640",
                    "http://cdn.17app.co/vod/prod/2cf84520-e65e-4b22-891e-1d3a00b0f068/reencode-640.mp4"
                ),
            ]
        );
    }

    #[tokio::test]
    async fn rooms_on_air_list_every_pull_and_rooms_off_air_say_so() {
        let resolver = resolver();
        let fixture = Fixture::parse(include_str!("seventeenlive_fixture.json")).unwrap();
        let live_room = fixture
            .notes
            .as_deref()
            .and_then(|n| n.split("live room ").nth(1))
            .and_then(|n| n.split_whitespace().next())
            .expect("the fixture notes name the room that was live");
        let resolved = resolver
            .resolve(&url(&format!("https://17.live/ja/live/{live_room}")))
            .await
            .unwrap()
            .media()
            .unwrap();
        assert!(resolved.live);
        assert_eq!(resolved.id.as_deref(), Some(live_room));
        assert!(resolved.title.is_some());
        assert!(resolved.uploader.is_some());
        assert!(resolved.uploaded_at.is_some());
        assert!(resolved.thumbnail.is_some());
        assert!(!resolved.variants.is_empty());
        assert!(resolved.variants.iter().all(|v| v.live
            && v.kind == VariantKind::File
            && v.container == Some(Container::Flv)
            && v.url.path().ends_with(".flv")));
        assert_eq!(
            resolved.variants[0].format_id.as_deref(),
            Some("1-urlHighQuality")
        );
        let urls: Vec<&Url> = resolved.variants.iter().map(|v| &v.url).collect();
        let mut unique = urls.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(urls.len(), unique.len(), "no pull is listed twice");
        assert!(resolved.variants.iter().any(
            |v| v.format_id.as_deref() == Some("1-url264") && v.video == Some(VideoCodec::H264)
        ));

        let error = resolver
            .resolve(&url("https://17.live/profile/r/1789280"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("is not live")),
            "{error}"
        );
    }

    #[test]
    fn pulls_are_ordered_by_quality_and_never_repeated() {
        let room = serde_json::json!({"rtmpUrls": [
            {"provider": 17, "url": "http://a/x_enhance003.flv", "urlLowQuality": "http://a/x.flv",
             "webUrl": "http://a/x_enhance003.flv", "urlHighQuality": "http://a/x.flv",
             "url264": "http://a/x_h264.flv", "urlQualityEnhancedHD": "http://a/x_enhance002.flv"},
            {"provider": 5, "url": "rtmp://b/x", "urlHighQuality": "https://b/x.flv"}
        ]});
        let referer = url("https://17.live/live/1");
        let variants = live_variants(&room, &referer);
        assert_eq!(
            variants
                .iter()
                .map(|v| (v.format_id.as_deref().unwrap(), v.url.as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("1-urlHighQuality", "http://a/x.flv"),
                ("1-urlQualityEnhancedHD", "http://a/x_enhance002.flv"),
                ("1-url", "http://a/x_enhance003.flv"),
                ("1-url264", "http://a/x_h264.flv"),
                ("2-urlHighQuality", "https://b/x.flv"),
            ]
        );
        assert_eq!(variants[0].label.as_deref(), Some("source, CDN 17"));
        assert_eq!(variants[3].video, Some(VideoCodec::H264));
        assert_eq!(variants[0].video, None);
        assert!(live_variants(&serde_json::json!({"rtmpUrls": null}), &referer).is_empty());
    }

    #[test]
    fn pictures_are_taken_from_the_cdn_when_bare() {
        assert_eq!(
            picture(&serde_json::json!("42DE18C1.jpg"))
                .unwrap()
                .as_str(),
            "https://cdn.17app.co/42DE18C1.jpg"
        );
        assert_eq!(
            picture(&serde_json::json!("http://cdn.17app.co/snapshot/x?t=1"))
                .unwrap()
                .as_str(),
            "http://cdn.17app.co/snapshot/x?t=1"
        );
        assert_eq!(picture(&serde_json::json!("")), None);
        assert_eq!(picture(&serde_json::json!(null)), None);
    }
}
