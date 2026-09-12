//! Dailymotion videos and live streams, from the metadata the player reads, with
//! playlists and user pages as playlists through the public API.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    ClipRange, MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionSupport, SubtitleFormat, SubtitleTrack, Variant, VariantKind, clean_title,
    fetch, hls, path_extension, status_error, timestamp_hint,
};
use crate::http::{BROWSER_UA, Cookie, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "dailymotion";
const METADATA: &str = "https://www.dailymotion.com/player/metadata/video/";
const REST: &str = "https://api.dailymotion.com";
const PAGE_SIZE: usize = 100;
const MAX_PAGES: usize = 10;
const MASTER_ATTEMPTS: usize = 3;

static RE_HOST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[a-z0-9-]+\.)*dailymotion\.[a-z]{2,3}$").unwrap());
static RE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^x[0-9a-z]{2,}$").unwrap());
static RE_USER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9._-]{2,}$").unwrap());
/// Progressive links carry their size: `/H264-1280x720-60/`.
static RE_SIZE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/H264-(\d+)x(\d+)(?:-(60))?/").unwrap());

/// Pages under these first segments are neither videos nor users.
const RESERVED: &[&str] = &[
    "about", "channel", "crawler", "embed", "explore", "games", "hub", "jobs", "legal", "library",
    "news", "partner", "player", "playlist", "press", "search", "settings", "signin", "signup",
    "sport", "swf", "topic", "topics", "upload", "video",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Video(String),
    Playlist(String),
    User(String),
}

fn id_of(segment: &str) -> Option<String> {
    let id = segment
        .split('_')
        .next()
        .unwrap_or(segment)
        .to_ascii_lowercase();
    RE_ID.is_match(&id).then_some(id)
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
    if host == "dai.ly" {
        return match segments.as_slice() {
            [id] => id_of(id).map(Link::Video),
            _ => None,
        };
    }
    if !RE_HOST.is_match(&host) {
        return None;
    }
    let query = |key: &str| {
        url.query_pairs()
            .find(|(k, _)| k == key)
            .and_then(|(_, v)| id_of(&v))
    };
    match segments.as_slice() {
        ["video", id, ..] | ["embed", "video", id, ..] | ["swf", "video", id, ..] => {
            id_of(id).map(Link::Video)
        }
        ["swf", id] => id_of(id).map(Link::Video),
        ["playlist", id, ..] | ["embed", "playlist", id, ..] => id_of(id).map(Link::Playlist),
        _ if query("video").is_some() => query("video").map(Link::Video),
        _ if query("playlist").is_some() => query("playlist").map(Link::Playlist),
        ["user", user] | ["user", user, "videos"] => Some(Link::User(user.to_string())),
        [user] | [user, "videos"]
            if !RESERVED.contains(&user.to_ascii_lowercase().as_str())
                && RE_USER.is_match(user) =>
        {
            Some(Link::User(user.to_string()))
        }
        _ => None,
    }
}

pub struct DailymotionResolver {
    http: Http,
}

impl DailymotionResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// GETs a public API path, reading the API's own error object.
    async fn rest(
        &self,
        path: &str,
        query: &[(&str, &str)],
        origin: &Url,
    ) -> Result<Value, ResolveError> {
        let mut url = Url::parse(&format!("{REST}{path}")).expect("valid");
        url.query_pairs_mut().extend_pairs(query);
        let fetched = fetch(&self.http, &url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        let value = fetched.json(origin)?;
        if let Some(error) = value.get("error").filter(|e| e.is_object()) {
            let message = error["message"].as_str().unwrap_or("unavailable");
            return Err(if error["type"].as_str() == Some("not_found") {
                ResolveError::NotFound(origin.clone())
            } else {
                ResolveError::unavailable(origin, message)
            });
        }
        if let Some(error) = status_error(fetched.status, origin) {
            return Err(error);
        }
        Ok(value)
    }

    /// The master playlist behind the CDN director, which refuses requests whose header
    /// fingerprint it has seen too often: every attempt carries a fresh set of headers.
    async fn master_playlist(
        &self,
        master: &Url,
        origin: &Url,
    ) -> Result<hls::Expanded, ResolveError> {
        let mut attempt = 0;
        let fetched = loop {
            attempt += 1;
            let fetched = fetch(
                &self.http,
                master,
                PLATFORM,
                BROWSER_UA,
                &scrambled_headers(),
                MAX_PAGE,
            )
            .await?;
            if fetched.status.as_u16() == 403 && attempt < MASTER_ATTEMPTS {
                tracing::debug!(%origin, attempt, "the CDN director refused the playlist");
                continue;
            }
            break fetched;
        };
        if let Some(error) = status_error(fetched.status, master) {
            return Err(error);
        }
        hls::expand_playlist(
            &self.http,
            master,
            &fetched.url,
            &fetched.body,
            PLATFORM,
            BROWSER_UA,
            &[],
        )
        .await
    }

    async fn video(&self, id: &str, url: &Url) -> Result<Resolved, ResolveError> {
        let metadata_url =
            Url::parse(&format!("{METADATA}{id}?app=com.dailymotion.neon")).expect("valid");
        let fetched = fetch(
            &self.http,
            &metadata_url,
            PLATFORM,
            BROWSER_UA,
            &[],
            MAX_PAGE,
        )
        .await?;
        let metadata = fetched.json(url)?;
        if let Some(error) = metadata.get("error").filter(|e| e.is_object()) {
            return Err(metadata_error(error, &metadata, url));
        }
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        let live = matches!(metadata["stream_type"].as_str(), Some("live"))
            || matches!(metadata["mode"].as_str(), Some("live"));
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.to_string());
        resolved.title = metadata["title"].as_str().and_then(clean_title);
        resolved.uploader = metadata["owner"]["screenname"]
            .as_str()
            .and_then(clean_title);
        resolved.uploader_url = metadata["owner"]["url"]
            .as_str()
            .and_then(|u| Url::parse(u).ok());
        resolved.uploaded_at = metadata["created_time"]
            .as_i64()
            .and_then(|t| Timestamp::from_second(t).ok());
        resolved.duration = metadata["duration"]
            .as_f64()
            .filter(|d| *d > 0.0)
            .map(Duration::from_secs_f64);
        resolved.thumbnail = largest_thumbnail(&metadata["posters"])
            .or_else(|| largest_thumbnail(&metadata["thumbnails"]));
        resolved.webpage_url = Url::parse(&format!("https://www.dailymotion.com/video/{id}")).ok();
        resolved.live = live;
        resolved.age_limit = metadata["explicit"].as_bool().filter(|e| *e).map(|_| 18);
        resolved.clip = timestamp_hint(url).map(|start| ClipRange { start, end: None });

        let mut variants = Vec::new();
        for (quality, list) in metadata["qualities"].as_object().into_iter().flatten() {
            for media in list.as_array().into_iter().flatten() {
                let Some(source) = media["url"].as_str().and_then(|u| Url::parse(u).ok()) else {
                    continue;
                };
                match media["type"].as_str().unwrap_or("") {
                    "application/vnd.lumberjack.manifest" => {}
                    "application/x-mpegURL" | "application/vnd.apple.mpegurl" => {
                        let expanded = self.master_playlist(&source, url).await?;
                        if resolved.duration.is_none() {
                            resolved.duration = expanded.duration;
                        }
                        resolved.live |= expanded.live;
                        resolved.subtitles.extend(expanded.subtitles);
                        variants.extend(expanded.variants);
                    }
                    _ => {
                        let mut v = Variant::new(source.clone(), VariantKind::File);
                        v.container = Some(Container::Mp4);
                        v.video = Some(VideoCodec::H264);
                        v.audio = Some(AudioCodec::Aac);
                        if let Some(caps) = RE_SIZE.captures(source.path()) {
                            v.width = caps[1].parse().ok();
                            v.height = caps[2].parse().ok();
                            v.fps = caps.get(3).map(|_| 60.0);
                        } else {
                            v.height = quality.trim_end_matches("@60").parse().ok();
                        }
                        if v.fps.is_none() && quality.ends_with("@60") {
                            v.fps = Some(60.0);
                        }
                        v.duration = resolved.duration;
                        v.format_id = Some(format!("http-{quality}"));
                        v.label = v.height.map(|h| format!("{h}p"));
                        variants.push(v);
                    }
                }
            }
        }
        if variants.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        if resolved.live {
            for v in &mut variants {
                v.live = true;
            }
        }
        for (language, tracks) in metadata["subtitles"]["data"]
            .as_object()
            .into_iter()
            .flatten()
        {
            for track in tracks["urls"].as_array().into_iter().flatten() {
                let Some(track_url) = track.as_str().and_then(|u| Url::parse(u).ok()) else {
                    continue;
                };
                let format = match path_extension(&track_url).as_deref() {
                    Some("srt") => SubtitleFormat::Srt,
                    Some("ttml") | Some("dfxp") | Some("xml") => SubtitleFormat::Ttml,
                    _ => SubtitleFormat::Vtt,
                };
                resolved.subtitles.push(SubtitleTrack {
                    url: track_url,
                    language: language.clone(),
                    name: tracks["label"].as_str().map(String::from),
                    format,
                    auto: false,
                    headers: Vec::new(),
                });
            }
        }
        resolved.variants = variants;
        Ok(resolved)
    }

    /// A playlist or a user's videos, read page by page through the public API.
    async fn listing(&self, link: &Link, url: &Url) -> Result<Playlist, ResolveError> {
        let (about, videos, id, fields) = match link {
            Link::Playlist(id) => (
                format!("/playlist/{id}"),
                format!("/playlist/{id}/videos"),
                id.clone(),
                "id,name,videos_total",
            ),
            Link::User(name) => (
                format!("/user/{name}"),
                format!("/user/{name}/videos"),
                name.clone(),
                "id,screenname,videos_total",
            ),
            Link::Video(_) => unreachable!("videos are resolved on their own"),
        };
        let info = self.rest(&about, &[("fields", fields)], url).await?;
        let title = info["name"]
            .as_str()
            .or_else(|| info["screenname"].as_str())
            .and_then(clean_title);
        let total = info["videos_total"].as_u64().map(|t| t as usize);
        let mut entries: Vec<PlaylistEntry> = Vec::new();
        let limit = PAGE_SIZE.to_string();
        for page in 1..=MAX_PAGES {
            let answer = self
                .rest(
                    &videos,
                    &[
                        ("fields", "id,title,duration"),
                        ("limit", &limit),
                        ("page", &page.to_string()),
                    ],
                    url,
                )
                .await?;
            let before = entries.len();
            for video in answer["list"].as_array().into_iter().flatten() {
                let Some(id) = video["id"].as_str() else {
                    continue;
                };
                let Ok(link) = Url::parse(&format!("https://www.dailymotion.com/video/{id}"))
                else {
                    continue;
                };
                if entries.iter().any(|e| e.url == link) {
                    continue;
                }
                entries.push(PlaylistEntry {
                    url: link,
                    title: video["title"].as_str().and_then(clean_title),
                    duration: video["duration"]
                        .as_f64()
                        .filter(|d| *d > 0.0)
                        .map(Duration::from_secs_f64),
                });
            }
            if answer["has_more"].as_bool() != Some(true) || entries.len() == before {
                break;
            }
        }
        if entries.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        Ok(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(id),
            title,
            total: total.filter(|t| *t > entries.len()),
            entries,
        })
    }
}

/// Between two and eight headers with made-up names, so no two playlist requests share a
/// header fingerprint. Vowels are left out so no real header name can come up.
fn scrambled_headers() -> Vec<(String, String)> {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    const LETTERS: &[u8] = b"bcdfghjklmnpqrstvwxz";
    let random = move || {
        let mut hasher = RandomState::new().build_hasher();
        hasher.write_u64(0);
        hasher.finish()
    };
    let word = |shortest: usize, longest: usize| -> String {
        let length = shortest + (random() as usize) % (longest - shortest + 1);
        (0..length)
            .map(|_| LETTERS[(random() as usize) % LETTERS.len()] as char)
            .collect()
    };
    let count = 2 + (random() as usize) % 7;
    (0..count).map(|_| (word(8, 24), word(16, 32))).collect()
}

/// What the player's `error` object means for the link.
fn metadata_error(error: &Value, metadata: &Value, url: &Url) -> ResolveError {
    let code = match &error["code"] {
        Value::String(code) => code.clone(),
        Value::Number(code) => code.to_string(),
        _ => String::new(),
    };
    let title = error["title"]
        .as_str()
        .or_else(|| error["raw_message"].as_str())
        .or_else(|| error["message"].as_str())
        .unwrap_or("unavailable");
    if error["type"].as_str() == Some("not_found") || code == "404" {
        return ResolveError::NotFound(url.clone());
    }
    if code == "DM007" || title == "video_geo_restricted" {
        return ResolveError::unavailable(url, format!("not available in this country: {title}"));
    }
    if metadata["is_password_protected"].as_bool() == Some(true)
        || title.to_ascii_lowercase().contains("password")
        || code.to_ascii_lowercase().contains("password")
    {
        return ResolveError::unavailable(url, "the video is password protected");
    }
    if code.is_empty() {
        ResolveError::unavailable(url, title)
    } else {
        ResolveError::unavailable(url, format!("{code}: {title}"))
    }
}

/// The largest of a `{"720": url, "1080": url}` map.
fn largest_thumbnail(sizes: &Value) -> Option<Url> {
    sizes
        .as_object()?
        .iter()
        .filter_map(|(size, link)| Some((size.parse::<u32>().ok()?, link.as_str()?)))
        .max_by_key(|(size, _)| *size)
        .and_then(|(_, link)| Url::parse(link).ok())
}

#[async_trait]
impl Resolver for DailymotionResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Dailymotion",
            hosts: &["dailymotion.com", "dai.ly"],
            features: &[
                "videos",
                "live",
                "embeds",
                "short links",
                "playlists",
                "user videos",
                "subtitles",
                "age-gated",
            ],
            formats: &["hls", "mp4"],
            session: SessionSupport::None,
            examples: &[
                "https://www.dailymotion.com/video/x5kesuj",
                "https://dai.ly/x26ezrb",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        match &link {
            Link::Video(id) => Ok(Resolution::from(self.video(id, url).await?)),
            Link::Playlist(_) | Link::User(_) => {
                Ok(Resolution::Playlist(self.listing(&link, url).await?))
            }
        }
    }

    /// The family filter hides age-gated videos until this cookie turns it off.
    fn consent_cookies(&self) -> Vec<Cookie> {
        vec![Cookie::new("ff", "off", "dailymotion.com")]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };
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

    const MASTER: &str = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=836280,CODECS=\"mp4a.40.2,avc1.64001f\",RESOLUTION=480x848,NAME=\"480\"\nhttps://vod3.cf.dmcdn.net/sec2(a)/video/fmp4/1/h264_aac_hq_vert/2/manifest.m3u8#cell=cf3\n#EXT-X-STREAM-INF:BANDWIDTH=2149280,CODECS=\"mp4a.40.2,avc1.64001f\",RESOLUTION=544x960,NAME=\"720\"\nhttps://vod3.cf.dmcdn.net/sec2(b)/video/fmp4/1/h264_aac_hd_vert/2/manifest.m3u8#cell=cf3\n";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n0.m4s\n#EXTINF:5.0,\n1.m4s\n#EXT-X-ENDLIST\n";

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://www.dailymotion.com/video/x5kesuj"),
            Some(Link::Video("x5kesuj".into()))
        );
        assert_eq!(
            link("https://www.dailymotion.com/video/x5kesuj_office-party-review?playlist=xv4bw"),
            Some(Link::Video("x5kesuj".into()))
        );
        assert_eq!(
            link("https://dai.ly/x26ezrb"),
            Some(Link::Video("x26ezrb".into()))
        );
        assert_eq!(
            link("https://www.dailymotion.com/embed/video/x26ezrb"),
            Some(Link::Video("x26ezrb".into()))
        );
        assert_eq!(
            link("https://geo.dailymotion.com/player/xf7zn.html?video=x26ezrb"),
            Some(Link::Video("x26ezrb".into()))
        );
        assert_eq!(
            link("https://geo.dailymotion.com/player/xf7zn.html?playlist=x7wdsj"),
            Some(Link::Playlist("x7wdsj".into()))
        );
        assert_eq!(
            link("https://www.dailymotion.com/playlist/xv4bw_nqtv_sport/1"),
            Some(Link::Playlist("xv4bw".into()))
        );
        assert_eq!(
            link("https://www.dailymotion.com/nqtv"),
            Some(Link::User("nqtv".into()))
        );
        assert_eq!(
            link("https://www.dailymotion.com/user/nqtv/videos"),
            Some(Link::User("nqtv".into()))
        );
        assert_eq!(link("https://www.dailymotion.com/search/cats/videos"), None);
        assert_eq!(link("https://www.dailymotion.com/"), None);
        assert_eq!(link("https://example.com/video/x5kesuj"), None);
    }

    #[tokio::test]
    async fn videos_expand_their_playlist_with_metadata_and_subtitles() {
        let mut fixture = Fixture::new("dailymotion", None);
        fixture.exchanges.push(get("https://www.dailymotion.com/player/metadata/video/x5kesuj?app=com.dailymotion.neon", 200, "application/json", json!({
            "id": "x5kesuj", "title": "Office Christmas Party Review", "duration": 187, "created_time": 1493651285, "explicit": true,
            "stream_type": "recorded", "owner": {"screenname": "Someone", "url": "https://www.dailymotion.com/someone"},
            "posters": null, "thumbnails": {"120": "https://s1.dmcdn.net/v/a/x120", "1080": "https://s2.dmcdn.net/v/a/x1080"},
            "qualities": {
                "auto": [{"type": "application/x-mpegURL", "url": "https://cdndirector.dailymotion.com/cdn/manifest/video/x5kesuj.m3u8?sec=abc"}],
                "720": [{"type": "video/mp4", "url": "https://vod.cf.dmcdn.net/sec(x)/video/H264-1280x720-60/x5kesuj.mp4"}]
            },
            "subtitles": {"enable": true, "data": {"en": {"label": "English", "urls": ["https://static1.dmcdn.net/static/video/x5kesuj/en.srt"]}}}
        }).to_string()));
        fixture.exchanges.push(get(
            "https://cdndirector.dailymotion.com/cdn/manifest/video/x5kesuj.m3u8?sec=abc",
            200,
            "application/vnd.apple.mpegurl",
            MASTER.into(),
        ));
        fixture.exchanges.push(get(
            "https://vod3.cf.dmcdn.net/sec2(b)/video/fmp4/1/h264_aac_hd_vert/2/manifest.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MEDIA.into(),
        ));
        let resolver = DailymotionResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.dailymotion.com/video/x5kesuj?start=30").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(
            resolved.title.as_deref(),
            Some("Office Christmas Party Review")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("Someone"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(187)));
        assert_eq!(resolved.age_limit, Some(18));
        assert_eq!(resolved.clip.unwrap().start, Duration::from_secs(30));
        assert_eq!(
            resolved.thumbnail.unwrap().as_str(),
            "https://s2.dmcdn.net/v/a/x1080"
        );
        assert_eq!(resolved.variants.len(), 3);
        let file = resolved
            .variants
            .iter()
            .find(|v| v.kind == VariantKind::File)
            .unwrap();
        assert_eq!(
            (file.width, file.height, file.fps),
            (Some(1280), Some(720), Some(60.0))
        );
        let best_hls = resolved
            .variants
            .iter()
            .filter(|v| v.kind == VariantKind::Hls)
            .max_by_key(|v| v.height)
            .unwrap();
        assert_eq!(best_hls.height, Some(960));
        assert_eq!(best_hls.video, Some(VideoCodec::H264));
        assert_eq!(resolved.subtitles.len(), 1);
        assert_eq!(resolved.subtitles[0].format, SubtitleFormat::Srt);
        assert_eq!(resolver.consent_cookies()[0].name, "ff");
    }

    #[test]
    fn scrambled_headers_differ_and_name_nothing_real() {
        let first = scrambled_headers();
        let second = scrambled_headers();
        assert!((2..=8).contains(&first.len()));
        assert_ne!(first, second);
        for (name, value) in first {
            assert!(name.len() >= 8 && value.len() >= 16, "{name}: {value}");
            assert!(
                name.chars().all(|c| "bcdfghjklmnpqrstvwxz".contains(c)),
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn player_errors_are_told_apart() {
        let mut fixture = Fixture::new("dailymotion", None);
        fixture.exchanges.push(get("https://www.dailymotion.com/player/metadata/video/x8xcz2t?app=com.dailymotion.neon", 200, "application/json", json!({
            "error": {"code": "404", "message": "Can't find object video for `id' parameter", "type": "not_found"}, "id": "x8xcz2t"
        }).to_string()));
        fixture.exchanges.push(get("https://www.dailymotion.com/player/metadata/video/x7geo00?app=com.dailymotion.neon", 200, "application/json", json!({
            "error": {"code": "DM007", "title": "video_geo_restricted", "raw_message": "This video is not available in your country."}
        }).to_string()));
        fixture.exchanges.push(get("https://www.dailymotion.com/player/metadata/video/x7pass0?app=com.dailymotion.neon", 200, "application/json", json!({
            "error": {"code": "DM009", "title": "Password protected"}, "is_password_protected": true
        }).to_string()));
        let resolver = DailymotionResolver::new(Http::replay(fixture));
        let resolve = |id: &str| {
            let url = Url::parse(&format!("https://www.dailymotion.com/video/{id}")).unwrap();
            let resolver = &resolver;
            async move { resolver.resolve(&url).await.unwrap_err() }
        };
        assert!(matches!(
            resolve("x8xcz2t").await,
            ResolveError::NotFound(_)
        ));
        assert!(
            matches!(resolve("x7geo00").await, ResolveError::Unavailable { reason, .. } if reason.contains("country"))
        );
        assert!(
            matches!(resolve("x7pass0").await, ResolveError::Unavailable { reason, .. } if reason.contains("password"))
        );
    }

    #[tokio::test]
    async fn playlists_and_user_pages_list_their_videos() {
        let mut fixture = Fixture::new("dailymotion", None);
        fixture.exchanges.push(get(
            "https://api.dailymotion.com/playlist/xv4bw?fields=id%2Cname%2Cvideos_total",
            200,
            "application/json",
            json!({"id": "xv4bw", "name": "SPORT", "videos_total": 3}).to_string(),
        ));
        fixture.exchanges.push(get("https://api.dailymotion.com/playlist/xv4bw/videos?fields=id%2Ctitle%2Cduration&limit=100&page=1", 200, "application/json", json!({
            "page": 1, "has_more": true, "list": [{"id": "x25b3hb", "title": "Remi vs Tony", "duration": 97}, {"id": "xl8v3q", "title": "Relais", "duration": 39}]
        }).to_string()));
        fixture.exchanges.push(get("https://api.dailymotion.com/playlist/xv4bw/videos?fields=id%2Ctitle%2Cduration&limit=100&page=2", 200, "application/json", json!({
            "page": 2, "has_more": false, "list": [{"id": "xgifai", "title": "Top 10", "duration": 194}]
        }).to_string()));
        fixture.exchanges.push(get("https://api.dailymotion.com/user/nobody?fields=id%2Cscreenname%2Cvideos_total", 200, "application/json", json!({
            "error": {"code": 404, "message": "Can't find object user for `id' parameter", "type": "not_found"}
        }).to_string()));
        let resolver = DailymotionResolver::new(Http::replay(fixture));
        let playlist = match resolver
            .resolve(
                &Url::parse("https://www.dailymotion.com/playlist/xv4bw_nqtv_sport/1").unwrap(),
            )
            .await
            .unwrap()
        {
            Resolution::Playlist(playlist) => playlist,
            other => panic!("{other:?}"),
        };
        assert_eq!(playlist.title.as_deref(), Some("SPORT"));
        assert_eq!(playlist.entries.len(), 3);
        assert_eq!(
            playlist.entries[2].url.as_str(),
            "https://www.dailymotion.com/video/xgifai"
        );
        assert_eq!(playlist.total, None);
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.dailymotion.com/nobody").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
