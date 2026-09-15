//! JW Player media, through the delivery API the player itself reads: every MP4 and
//! WebM rendition with its size and bitrate, the HLS manifest, the caption tracks and the
//! media's title, poster and time. Player scripts, embed pages, manifests and video files
//! all name the media; a playlist becomes a playlist of its media.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, SubtitleFormat, SubtitleTrack, Variant, VariantKind, clean_title, essence,
    fetch, is_dash_type, is_hls_type,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "jwplayer";
const MEDIA_API: &str = "https://cdn.jwplayer.com/v2/media/";
const PLAYLIST_API: &str = "https://cdn.jwplayer.com/v2/playlists/";

static RE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9]{8}$").unwrap());
static RE_PLAYER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([A-Za-z0-9]{8})(?:-[A-Za-z0-9]{8})?(?:\.js|\.html)?$").unwrap()
});
static RE_VIDEO_FILE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([A-Za-z0-9]{8})-[A-Za-z0-9]{8}\.[a-z0-9]{2,4}$").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Media(String),
    Playlist(String),
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !matches!(
        host.as_str(),
        "cdn.jwplayer.com"
            | "content.jwplatform.com"
            | "cdn.jwplatform.com"
            | "videos.jwplayer.com"
            | "playlists.jwplayer.com"
    ) {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    match segments.as_slice() {
        ["v2", "media", id, ..] | ["previews", id] | ["thumbs", id] if RE_ID.is_match(id) => {
            Some(Link::Media(id.to_string()))
        }
        ["v2", "playlists", id, ..] if RE_ID.is_match(id) => Some(Link::Playlist(id.to_string())),
        ["feeds", name] => {
            let id = name.trim_end_matches(".json").trim_end_matches(".rss");
            RE_ID.is_match(id).then(|| Link::Playlist(id.to_string()))
        }
        ["players", name] | ["players", name, ..] => RE_PLAYER
            .captures(name)
            .map(|c| Link::Media(c[1].to_string())),
        ["manifests", name] => {
            let id = name.trim_end_matches(".m3u8").trim_end_matches(".mpd");
            RE_ID.is_match(id).then(|| Link::Media(id.to_string()))
        }
        ["videos", name] | ["tracks", name] => RE_VIDEO_FILE
            .captures(name)
            .map(|c| Link::Media(c[1].to_string())),
        _ => None,
    }
}

fn subtitle_format(url: &Url) -> SubtitleFormat {
    match url
        .path()
        .rsplit('.')
        .next()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("srt") => SubtitleFormat::Srt,
        Some("xml") | Some("dfxp") | Some("ttml") => SubtitleFormat::Ttml,
        _ => SubtitleFormat::Vtt,
    }
}

/// The renditions a media item's sources list.
pub fn variants_of(item: &Value) -> Vec<Variant> {
    let duration = item["duration"]
        .as_f64()
        .filter(|d| *d > 0.0)
        .map(Duration::from_secs_f64);
    let mut variants = Vec::new();
    for source in item["sources"].as_array().into_iter().flatten() {
        let Some(url) = source["file"].as_str().and_then(|u| Url::parse(u).ok()) else {
            continue;
        };
        let mime = essence(source["type"].as_str());
        let ext = url
            .path()
            .rsplit('.')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let kind = if is_hls_type(&mime) || ext == "m3u8" {
            VariantKind::Hls
        } else if is_dash_type(&mime) || ext == "mpd" {
            VariantKind::Dash
        } else {
            VariantKind::File
        };
        let mut v = Variant::new(url, kind);
        if kind == VariantKind::File {
            v.container = Container::from_mime(&mime).or_else(|| Container::from_extension(&ext));
            let audio = mime.starts_with("audio/") || matches!(ext.as_str(), "mp3" | "m4a" | "aac");
            v.audio_only = audio;
            match v.container {
                Some(Container::Mp4) if !audio => {
                    v.video = Some(VideoCodec::H264);
                    v.audio = Some(AudioCodec::Aac);
                }
                Some(Container::Webm) => {
                    v.video = Some(VideoCodec::Vp8);
                    v.audio = Some(AudioCodec::Vorbis);
                }
                _ => {}
            }
            if audio {
                v.audio = Some(if ext == "mp3" {
                    AudioCodec::Mp3
                } else {
                    AudioCodec::Aac
                });
            }
        }
        v.width = source["width"].as_u64().map(|w| w as u32);
        v.height = source["height"].as_u64().map(|h| h as u32);
        v.bitrate = source["bitrate"].as_u64().filter(|b| *b > 0);
        v.size = source["filesize"].as_u64().filter(|s| *s > 0);
        v.fps = source["framerate"].as_f64().filter(|f| *f > 0.0);
        v.duration = duration;
        v.label = source["label"].as_str().map(String::from);
        v.format_id = source["label"]
            .as_str()
            .map(|l| l.to_ascii_lowercase().replace(' ', "_"))
            .or_else(|| Some(kind.as_str().to_string()));
        variants.push(v);
    }
    variants
}

pub fn subtitles_of(item: &Value) -> Vec<SubtitleTrack> {
    item["tracks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| matches!(t["kind"].as_str(), Some("captions") | Some("subtitles")))
        .filter_map(|t| {
            let url = t["file"].as_str().and_then(|u| Url::parse(u).ok())?;
            let label = t["label"].as_str().unwrap_or("und").to_string();
            Some(SubtitleTrack {
                format: subtitle_format(&url),
                url,
                language: t["language"]
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| label.clone()),
                name: Some(label),
                auto: false,
                headers: Vec::new(),
            })
        })
        .collect()
}

fn resolved_of(item: &Value) -> Resolved {
    let mut resolved = Resolved::new(PLATFORM);
    resolved.id = item["mediaid"].as_str().map(String::from);
    resolved.title = item["title"].as_str().and_then(clean_title);
    resolved.description = item["description"].as_str().and_then(clean_title);
    resolved.uploaded_at = item["pubdate"]
        .as_i64()
        .filter(|t| *t > 0)
        .and_then(|t| Timestamp::from_second(t).ok());
    resolved.duration = item["duration"]
        .as_f64()
        .filter(|d| *d > 0.0)
        .map(Duration::from_secs_f64);
    resolved.thumbnail = item["image"].as_str().and_then(|u| Url::parse(u).ok());
    resolved.webpage_url = item["link"].as_str().and_then(|u| Url::parse(u).ok());
    resolved.subtitles = subtitles_of(item);
    resolved.variants = variants_of(item);
    resolved
}

pub struct JwplayerResolver {
    http: Http,
}

impl JwplayerResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    async fn api(&self, url: Url, origin: &Url) -> Result<Value, ResolveError> {
        let fetched = fetch(&self.http, &url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        match fetched.status.as_u16() {
            200..=299 => fetched.json(origin),
            404 | 410 => Err(ResolveError::NotFound(origin.clone())),
            403 => Err(ResolveError::unavailable(
                origin,
                "the media is not published for playback here",
            )),
            429 => Err(ResolveError::RateLimited(origin.clone())),
            status => Err(ResolveError::unavailable(
                origin,
                format!("the delivery API answered HTTP {status}"),
            )),
        }
    }
}

#[async_trait]
impl Resolver for JwplayerResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "JW Player",
            hosts: &["cdn.jwplayer.com", "content.jwplatform.com"],
            features: &[
                "media",
                "player embeds",
                "manifests",
                "video files",
                "playlists",
                "captions",
            ],
            formats: &["mp4", "webm", "hls"],
            session: SessionSupport::None,
            examples: &[
                "https://cdn.jwplayer.com/v2/media/nPripu9l",
                "https://cdn.jwplayer.com/players/nPripu9l-ALJ3XQCI.js",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Media(id) => {
                let api = Url::parse(&format!("{MEDIA_API}{id}")).expect("valid");
                let feed = self.api(api, url).await?;
                let item = feed["playlist"]
                    .as_array()
                    .and_then(|items| {
                        items
                            .iter()
                            .find(|i| i["mediaid"].as_str() == Some(&id))
                            .or_else(|| items.first())
                    })
                    .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
                let mut resolved = resolved_of(item);
                if resolved.variants.is_empty() {
                    return Err(ResolveError::unavailable(
                        url,
                        "the media has no playable sources",
                    ));
                }
                if resolved.title.is_none() {
                    resolved.title = feed["title"].as_str().and_then(clean_title);
                }
                if resolved.webpage_url.is_none() {
                    resolved.webpage_url = Some(url.clone());
                }
                Ok(Resolution::from(resolved))
            }
            Link::Playlist(id) => {
                let api = Url::parse(&format!("{PLAYLIST_API}{id}")).expect("valid");
                let feed = self.api(api, url).await?;
                let entries: Vec<PlaylistEntry> = feed["playlist"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|item| {
                        let media = item["mediaid"].as_str()?;
                        Some(PlaylistEntry {
                            url: Url::parse(&format!("{MEDIA_API}{media}")).ok()?,
                            title: item["title"].as_str().and_then(clean_title),
                            duration: item["duration"]
                                .as_f64()
                                .filter(|d| *d > 0.0)
                                .map(Duration::from_secs_f64),
                        })
                    })
                    .collect();
                if entries.is_empty() {
                    return Err(ResolveError::NotFound(url.clone()));
                }
                Ok(Resolution::Playlist(Playlist {
                    resolver: PLATFORM.into(),
                    id: Some(id),
                    title: feed["title"].as_str().and_then(clean_title),
                    total: Some(entries.len()),
                    entries,
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

    fn get(url: &str, status: u16, body: &str) -> Exchange {
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
                headers: vec![("content-type".into(), "application/json".into())],
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    const MEDIA: &str = r#"{"title":"Big Buck Bunny Trailer","description":"Big Buck Bunny is a short animated film by the Blender Institute.","kind":"Single Item","playlist":[{"title":"Big Buck Bunny Trailer","mediaid":"nPripu9l","link":"https://www.jwplayer.com/innovation/roadmap/","image":"https://cdn.jwplayer.com/v2/media/nPripu9l/poster.jpg?width=720","description":"Big Buck Bunny is a short animated film by the Blender Institute.","pubdate":1227796140,"duration":32,"sources":[{"file":"https://cdn.jwplayer.com/manifests/nPripu9l.m3u8","type":"application/vnd.apple.mpegurl"},{"file":"https://cdn.jwplayer.com/videos/nPripu9l-1ahmry41.mp4","type":"video/mp4","label":"MP4 480px","width":480,"height":270,"bitrate":764256,"filesize":3057046,"framerate":25.0},{"file":"https://cdn.jwplayer.com/videos/nPripu9l-twSo9iFz.mp4","type":"video/mp4","label":"MP4 1280px","width":1080,"height":608,"bitrate":1847776,"filesize":7391128,"framerate":25.0},{"file":"https://cdn.jwplayer.com/videos/nPripu9l-1Lq5Mnwq.webm","type":"video/webm","label":"WebM Video","width":480,"height":270,"bitrate":994168,"filesize":4100971,"framerate":25.0},{"file":"https://cdn.jwplayer.com/videos/nPripu9l-tL7ciKUL.mov","label":"Passthrough","width":1280,"height":180,"filesize":17810888},{"file":"https://cdn.jwplayer.com/videos/nPripu9l-ywAKK1m8.mp3","type":"audio/mp3","label":"MP3 Audio","bitrate":112024,"filesize":462661}],"tracks":[{"file":"https://cdn.jwplayer.com/tracks/2gAwO6yY.xml","kind":"captions","label":"English"},{"file":"https://cdn.jwplayer.com/strips/nPripu9l-120.vtt","kind":"thumbnails"}]}]}"#;

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://cdn.jwplayer.com/v2/media/nPripu9l"),
            Some(Link::Media("nPripu9l".into()))
        );
        assert_eq!(
            link("https://cdn.jwplayer.com/players/nPripu9l-ALJ3XQCI.js"),
            Some(Link::Media("nPripu9l".into()))
        );
        assert_eq!(
            link("https://content.jwplatform.com/players/nPripu9l-ALJ3XQCI.html"),
            Some(Link::Media("nPripu9l".into()))
        );
        assert_eq!(
            link("https://cdn.jwplayer.com/players/nPripu9l.html"),
            Some(Link::Media("nPripu9l".into()))
        );
        assert_eq!(
            link("https://cdn.jwplayer.com/manifests/nPripu9l.m3u8"),
            Some(Link::Media("nPripu9l".into()))
        );
        assert_eq!(
            link("https://cdn.jwplayer.com/videos/nPripu9l-1ahmry41.mp4"),
            Some(Link::Media("nPripu9l".into()))
        );
        assert_eq!(
            link("https://cdn.jwplayer.com/previews/nPripu9l"),
            Some(Link::Media("nPripu9l".into()))
        );
        assert_eq!(
            link("https://cdn.jwplayer.com/v2/playlists/zWLx6iQK"),
            Some(Link::Playlist("zWLx6iQK".into()))
        );
        assert_eq!(
            link("https://cdn.jwplayer.com/feeds/zWLx6iQK.json"),
            Some(Link::Playlist("zWLx6iQK".into()))
        );
        assert_eq!(link("https://cdn.jwplayer.com/libraries/abc.js"), None);
        assert_eq!(link("https://www.jwplayer.com/"), None);
    }

    #[tokio::test]
    async fn media_resolve_with_every_rendition_and_its_captions() {
        let mut fixture = Fixture::new("jwplayer", None);
        fixture.exchanges.push(get(
            "https://cdn.jwplayer.com/v2/media/nPripu9l",
            200,
            MEDIA,
        ));
        fixture.exchanges.push(get(
            "https://cdn.jwplayer.com/v2/media/gone1234",
            404,
            r#"{"errors":[{"code":"not_found"}]}"#,
        ));
        let resolver = JwplayerResolver::new(Http::replay(fixture));
        let url = Url::parse("https://cdn.jwplayer.com/players/nPripu9l-ALJ3XQCI.js").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.title.as_deref(), Some("Big Buck Bunny Trailer"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(32)));
        assert_eq!(resolved.uploaded_at.unwrap().as_second(), 1227796140);
        assert!(resolved.thumbnail.is_some());
        assert_eq!(resolved.variants.len(), 6);
        assert_eq!(resolved.variants[0].kind, VariantKind::Hls);
        let hd = resolved
            .variants
            .iter()
            .find(|v| v.height == Some(608))
            .unwrap();
        assert_eq!(hd.size, Some(7391128));
        assert_eq!(hd.bitrate, Some(1847776));
        assert_eq!(hd.fps, Some(25.0));
        assert_eq!(hd.video, Some(VideoCodec::H264));
        let webm = resolved
            .variants
            .iter()
            .find(|v| v.container == Some(Container::Webm))
            .unwrap();
        assert_eq!(webm.video, Some(VideoCodec::Vp8));
        let mov = resolved
            .variants
            .iter()
            .find(|v| v.container == Some(Container::Mov))
            .unwrap();
        assert_eq!(mov.size, Some(17810888));
        let audio = resolved.variants.iter().find(|v| v.audio_only).unwrap();
        assert_eq!(audio.audio, Some(AudioCodec::Mp3));
        assert_eq!(resolved.subtitles.len(), 1);
        assert_eq!(resolved.subtitles[0].format, SubtitleFormat::Ttml);
        assert_eq!(resolved.subtitles[0].name.as_deref(), Some("English"));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://cdn.jwplayer.com/v2/media/gone1234").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn playlists_list_their_media() {
        let playlist = r#"{"title":"Trailers","kind":"MANUAL","playlist":[{"mediaid":"nPripu9l","title":"Big Buck Bunny Trailer","duration":32},{"mediaid":"aBcDeFgH","title":"Sintel","duration":52}]}"#;
        let mut fixture = Fixture::new("jwplayer", None);
        fixture.exchanges.push(get(
            "https://cdn.jwplayer.com/v2/playlists/zWLx6iQK",
            200,
            playlist,
        ));
        let resolver = JwplayerResolver::new(Http::replay(fixture));
        let playlist = match resolver
            .resolve(&Url::parse("https://cdn.jwplayer.com/v2/playlists/zWLx6iQK").unwrap())
            .await
            .unwrap()
        {
            Resolution::Playlist(p) => p,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(playlist.title.as_deref(), Some("Trailers"));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[1].url.as_str(),
            "https://cdn.jwplayer.com/v2/media/aBcDeFgH"
        );
        assert_eq!(playlist.entries[1].duration, Some(Duration::from_secs(52)));
    }

    #[tokio::test]
    async fn recorded_delivery_response_resolves() {
        let fixture = Fixture::parse(include_str!("jwplayer_fixture.json")).unwrap();
        let resolver = JwplayerResolver::new(Http::replay(fixture));
        let media = resolver
            .resolve(&Url::parse("https://cdn.jwplayer.com/players/nPripu9l-ALJ3XQCI.js").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(media.id.as_deref(), Some("nPripu9l"));
        assert_eq!(media.title.as_deref(), Some("Big Buck Bunny Trailer"));
        assert_eq!(media.variants.len(), 10);
        assert!(media.variants.iter().any(|v| v.kind == VariantKind::Hls));
        assert!(media.variants.iter().any(|v| v.audio_only));
        let missing = Url::parse("https://cdn.jwplayer.com/v2/media/zzzzzzzz").unwrap();
        assert!(matches!(
            resolver.resolve(&missing).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
