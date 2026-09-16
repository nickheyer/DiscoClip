//! Wistia media, through the embed API the player reads: every MP4 asset with its
//! dimensions, bitrate and size, the original upload, the HLS manifest of media the
//! account streams, and the captions in every language. Media pages, player iframes,
//! embed scripts and `wvideo` links name the media; playlists and channels become
//! playlists of theirs.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use scraper::Selector;
use serde_json::Value;
use url::Url;

use super::page::Page;
use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, SubtitleFormat, SubtitleTrack, Variant, VariantKind, clean_title, fetch,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "wistia";
const EMBED_API: &str = "https://fast.wistia.com/embed/";
const PLAYER: &str = "https://fast.wistia.net/embed/iframe/";
const PLAYLIST_PLAYER: &str = "https://fast.wistia.net/embed/playlists/";
const CHANNEL_PLAYER: &str = "https://fast.wistia.net/embed/channel/";
const HLS: &str = "https://fast.wistia.net/embed/medias/";
const CAPTIONS: &str = "https://fast.wistia.com/embed/captions/";

static RE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9]{10}$").unwrap());
static RE_ASYNC_CLASS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"wistia_async_([a-z0-9]{10})").unwrap());
static RE_EMBED_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"wistia\.(?:com|net)/embed/(iframe|medias|playlists|channel)/([a-z0-9]{10})")
        .unwrap()
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Media(String),
    Playlist(String),
    Channel(String),
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !["wistia.com", "wistia.net"]
        .iter()
        .any(|domain| host == *domain || host.ends_with(&format!(".{domain}")))
    {
        return None;
    }
    let query = |key: &str| {
        url.query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.into_owned())
            .filter(|v| RE_ID.is_match(v))
    };
    if let Some(id) = query("wvideo").or_else(|| query("wmediaid")) {
        return Some(Link::Media(id));
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let rest = match segments.as_slice() {
        ["embed", rest @ ..] => rest,
        all => all,
    };
    let id_of = |name: &str| {
        let id = name.split('.').next().unwrap_or(name);
        RE_ID.is_match(id).then(|| id.to_string())
    };
    match rest {
        ["medias", name, ..] | ["iframe", name, ..] => id_of(name).map(Link::Media),
        ["playlists", name, ..] => id_of(name).map(Link::Playlist),
        ["channel", name, ..] | ["channels", name, ..] => id_of(name).map(Link::Channel),
        _ => None,
    }
}

/// The player iframe link for a media.
pub fn player_url(id: &str) -> Url {
    Url::parse(&format!("{PLAYER}{id}")).expect("valid")
}

fn canonical(link: &Link) -> Url {
    match link {
        Link::Media(id) => player_url(id),
        Link::Playlist(id) => Url::parse(&format!("{PLAYLIST_PLAYER}{id}")).expect("valid"),
        Link::Channel(id) => Url::parse(&format!("{CHANNEL_PLAYER}{id}")).expect("valid"),
    }
}

fn selector(text: &str) -> Selector {
    Selector::parse(text).expect("selectors in this module are valid")
}

/// Players a page embeds through the `wistia_async_` class, the `<wistia-player>`
/// element, an embed script or an iframe, as links this resolver takes.
pub fn embeds_in(page: &Page) -> Vec<Url> {
    let mut found: Vec<Url> = Vec::new();
    let mut push = |url: Url| {
        if !found.contains(&url) {
            found.push(url);
        }
    };
    for captures in RE_ASYNC_CLASS.captures_iter(page.html()) {
        push(player_url(&captures[1]));
    }
    for element in page.document().select(&selector("wistia-player[media-id]")) {
        if let Some(id) = element
            .value()
            .attr("media-id")
            .filter(|id| RE_ID.is_match(id))
        {
            push(player_url(id));
        }
    }
    for captures in RE_EMBED_URL.captures_iter(page.html()) {
        let id = captures[2].to_string();
        push(canonical(&match &captures[1] {
            "playlists" => Link::Playlist(id),
            "channel" => Link::Channel(id),
            _ => Link::Media(id),
        }));
    }
    found
}

fn seconds(value: &Value) -> Option<Duration> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|s| s.trim().parse().ok()))
        .filter(|d| *d > 0.0)
        .map(Duration::from_secs_f64)
}

/// An asset's URL with its real extension in place of the `.bin` the API writes.
fn asset_url(asset: &Value, fallback_ext: &str) -> Option<Url> {
    let raw = asset["url"].as_str()?;
    let ext = asset["ext"]
        .as_str()
        .filter(|e| !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or(fallback_ext);
    let rewritten = match raw.strip_suffix(".bin") {
        Some(stem) if !ext.is_empty() => format!("{stem}.{ext}"),
        _ => raw.to_string(),
    };
    Url::parse(&rewritten).ok()
}

fn video_codec(name: &str) -> Option<VideoCodec> {
    match name.to_ascii_lowercase().as_str() {
        "h264" | "avc1" | "avc" => Some(VideoCodec::H264),
        "h265" | "hevc" | "hvc1" => Some(VideoCodec::H265),
        "vp8" => Some(VideoCodec::Vp8),
        "vp9" => Some(VideoCodec::Vp9),
        "av1" => Some(VideoCodec::Av1),
        _ => None,
    }
}

/// The media's assets as variants: every video and audio file, and the HLS manifest of
/// media the account streams that way.
pub fn variants_of(media: &Value) -> Vec<Variant> {
    let duration = seconds(&media["duration"]);
    let mut variants = Vec::new();
    for asset in media["assets"].as_array().into_iter().flatten() {
        let kind_name = asset["type"].as_str().unwrap_or_default();
        if matches!(
            kind_name,
            "preview" | "storyboard" | "still_image" | "still"
        ) {
            continue;
        }
        if asset["status"].as_i64().is_some_and(|status| status != 2) {
            continue;
        }
        let container_name = asset["container"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let ext = asset["ext"]
            .as_str()
            .unwrap_or_default()
            .to_ascii_lowercase();
        let hls = container_name == "m3u8" || ext == "m3u8";
        let Some(url) = asset_url(asset, if hls { "m3u8" } else { "" }) else {
            continue;
        };
        let display = asset["display_name"].as_str().map(String::from);
        let audio = display.as_deref() == Some("Audio")
            || kind_name.ends_with("_audio")
            || matches!(ext.as_str(), "mp3" | "m4a" | "aac" | "ogg");
        let mut v = Variant::new(
            url,
            if hls {
                VariantKind::Hls
            } else {
                VariantKind::File
            },
        );
        if !hls {
            v.container = Container::from_extension(&ext)
                .or_else(|| Container::from_extension(&container_name))
                .or_else(|| {
                    (audio && matches!(ext.as_str(), "m4a" | "aac")).then_some(Container::Mp4)
                });
            if audio {
                v.audio_only = true;
                v.audio = Some(if ext == "mp3" {
                    AudioCodec::Mp3
                } else {
                    AudioCodec::Aac
                });
            } else {
                v.video = asset["codec"].as_str().and_then(video_codec);
                if v.container == Some(Container::Mp4) {
                    v.audio = Some(AudioCodec::Aac);
                }
            }
            v.size = asset["size"].as_u64().filter(|s| *s > 0);
        }
        if !audio {
            v.width = asset["width"].as_u64().filter(|w| *w > 0).map(|w| w as u32);
            v.height = asset["height"]
                .as_u64()
                .filter(|h| *h > 0)
                .map(|h| h as u32);
        }
        v.bitrate = asset["bitrate"]
            .as_u64()
            .filter(|b| *b > 0)
            .map(|kbps| kbps * 1000);
        v.duration = duration;
        v.label = display;
        v.format_id = Some(kind_name.to_string());
        variants.push(v);
    }
    if media["hls_enabled"].as_bool() == Some(true)
        && let Some(id) = media["hashedId"].as_str()
    {
        let mut v = Variant::new(
            Url::parse(&format!("{HLS}{id}.m3u8")).expect("valid"),
            VariantKind::Hls,
        );
        v.duration = duration;
        v.format_id = Some("hls".into());
        variants.push(v);
    }
    variants
}

/// The captions the media carries, one WebVTT track per language.
pub fn subtitles_of(media: &Value) -> Vec<SubtitleTrack> {
    let Some(id) = media["hashedId"].as_str() else {
        return Vec::new();
    };
    media["captions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|caption| {
            let language = caption["language"].as_str().filter(|l| !l.is_empty())?;
            let mut url = Url::parse(&format!("{CAPTIONS}{id}.vtt")).ok()?;
            url.query_pairs_mut().append_pair("language", language);
            Some(SubtitleTrack {
                url,
                language: language.to_string(),
                name: None,
                format: SubtitleFormat::Vtt,
                auto: false,
                headers: Vec::new(),
            })
        })
        .collect()
}

fn resolved_of(media: &Value, origin: &Url) -> Resolved {
    let mut resolved = Resolved::new(PLATFORM);
    resolved.id = media["hashedId"].as_str().map(String::from);
    resolved.title = media["name"].as_str().and_then(clean_title);
    resolved.description = media["seoDescription"].as_str().and_then(clean_title);
    resolved.uploaded_at = media["createdAt"]
        .as_i64()
        .filter(|t| *t > 0)
        .and_then(|t| Timestamp::from_second(t).ok());
    resolved.duration = seconds(&media["duration"]);
    resolved.thumbnail = media["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|asset| matches!(asset["type"].as_str(), Some("still_image") | Some("still")))
        .and_then(|asset| asset_url(asset, "jpg"));
    resolved.webpage_url = Some(origin.clone());
    resolved.subtitles = subtitles_of(media);
    resolved.variants = variants_of(media);
    resolved
}

pub struct WistiaResolver {
    http: Http,
}

impl WistiaResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// Reads `path` under the embed API, sending the link as the referer so media
    /// restricted to their own pages answer.
    async fn embed(&self, path: &str, origin: &Url) -> Result<Value, ResolveError> {
        let api = Url::parse(&format!("{EMBED_API}{path}")).expect("valid");
        let headers = [("referer".to_string(), origin.to_string())];
        let fetched = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &headers, MAX_PAGE).await?;
        match fetched.status.as_u16() {
            200..=299 => fetched.json(origin),
            404 | 410 => Err(ResolveError::NotFound(origin.clone())),
            429 => Err(ResolveError::RateLimited(origin.clone())),
            status => Err(ResolveError::unavailable(
                origin,
                format!("the embed API answered HTTP {status}"),
            )),
        }
    }
}

#[async_trait]
impl Resolver for WistiaResolver {
    fn embeds_in(&self, page: &Page) -> Vec<Url> {
        embeds_in(page)
    }

    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Wistia",
            hosts: &["wistia.com", "wistia.net"],
            features: &[
                "media",
                "player iframes",
                "embed scripts",
                "playlists",
                "channels",
                "captions",
            ],
            formats: &["mp4", "hls"],
            session: SessionSupport::None,
            examples: &[
                "https://fast.wistia.net/embed/iframe/cmst5825to",
                "https://fast.wistia.net/embed/playlists/aodt9etokc",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Media(id) => {
                let body = self.embed(&format!("medias/{id}.json"), url).await?;
                if body["error"].as_bool() == Some(true) || body["error"].is_string() {
                    return Err(ResolveError::NotFound(url.clone()));
                }
                let media = &body["media"];
                if !media.is_object() {
                    return Err(ResolveError::malformed(url, "the embed API named no media"));
                }
                let resolved = resolved_of(media, url);
                if resolved.variants.is_empty() {
                    let reason = if media["protected"].as_bool() == Some(true) {
                        "the media is password protected"
                    } else {
                        "the media has no playable assets"
                    };
                    return Err(ResolveError::unavailable(url, reason));
                }
                Ok(Resolution::from(resolved))
            }
            Link::Playlist(id) => {
                let body = self.embed(&format!("playlists/{id}.json"), url).await?;
                if body["error"].as_bool() == Some(true) || body["error"].is_string() {
                    return Err(ResolveError::NotFound(url.clone()));
                }
                let playlist = body
                    .as_array()
                    .and_then(|list| list.first())
                    .unwrap_or(&body);
                let entries: Vec<PlaylistEntry> = playlist["medias"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|item| {
                        let media = &item["embed_config"]["media"];
                        let hashed = media["hashedId"].as_str()?;
                        Some(PlaylistEntry {
                            url: player_url(hashed),
                            title: item["name"]
                                .as_str()
                                .or(media["name"].as_str())
                                .and_then(clean_title),
                            duration: seconds(&media["duration"]),
                        })
                    })
                    .collect();
                if entries.is_empty() {
                    return Err(ResolveError::NotFound(url.clone()));
                }
                Ok(Resolution::Playlist(Playlist {
                    resolver: PLATFORM.into(),
                    id: Some(id),
                    title: playlist["name"].as_str().and_then(clean_title),
                    total: Some(entries.len()),
                    entries,
                }))
            }
            Link::Channel(id) => {
                let body = self.embed(&format!("channel/{id}.json"), url).await?;
                if body["error"].is_string() {
                    return Err(ResolveError::NotFound(url.clone()));
                }
                let series = &body["series"][0];
                let entries: Vec<PlaylistEntry> = series["sections"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .flat_map(|section| section["episodes"].as_array().into_iter().flatten())
                    .filter_map(|episode| {
                        let hashed = episode["episodeMediaHashedId"]
                            .as_str()
                            .or(episode["hashedId"].as_str())?;
                        Some(PlaylistEntry {
                            url: player_url(hashed),
                            title: episode["episodeTitle"]
                                .as_str()
                                .or(episode["name"].as_str())
                                .and_then(clean_title),
                            duration: seconds(&episode["durationInSeconds"]),
                        })
                    })
                    .collect();
                if entries.is_empty() {
                    return Err(ResolveError::NotFound(url.clone()));
                }
                Ok(Resolution::Playlist(Playlist {
                    resolver: PLATFORM.into(),
                    id: Some(id),
                    title: series["title"].as_str().and_then(clean_title),
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
    use crate::http::transport::Fixture;

    fn fixture() -> Fixture {
        Fixture::parse(include_str!("wistia_fixture.json")).unwrap()
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://wistia.com/medias/cmst5825to"),
            Some(Link::Media("cmst5825to".into()))
        );
        assert_eq!(
            link("https://support.wistia.com/medias/cmst5825to?wtime=5"),
            Some(Link::Media("cmst5825to".into()))
        );
        assert_eq!(
            link("https://fast.wistia.net/embed/iframe/cmst5825to?videoFoam=true"),
            Some(Link::Media("cmst5825to".into()))
        );
        assert_eq!(
            link("https://fast.wistia.com/embed/medias/cmst5825to.jsonp"),
            Some(Link::Media("cmst5825to".into()))
        );
        assert_eq!(
            link("https://fast.wistia.net/embed/playlists/aodt9etokc"),
            Some(Link::Playlist("aodt9etokc".into()))
        );
        assert_eq!(
            link("https://fast.wistia.net/embed/channel/24076kc8tf"),
            Some(Link::Channel("24076kc8tf".into()))
        );
        assert_eq!(
            link("https://fast.wistia.net/embed/channel/24076kc8tf?wvideo=9pyv4cqb5n"),
            Some(Link::Media("9pyv4cqb5n".into()))
        );
        assert_eq!(
            link("https://wistia.com/learn/marketing/video-marketing-strategy"),
            None
        );
        assert_eq!(link("https://wistia.com/medias/"), None);
        assert_eq!(
            link("https://fast.wistia.net/assets/external/E-v1.js"),
            None
        );
    }

    #[test]
    fn embedded_players_are_found_in_pages() {
        let html = r#"<html><body>
            <div class="wistia_embed wistia_async_cmst5825to videoFoam=true"></div>
            <wistia-player media-id="9pyv4cqb5n" aspect="1.78"></wistia-player>
            <script src="https://fast.wistia.com/embed/medias/cmst5825to.jsonp" async></script>
            <iframe src="https://fast.wistia.net/embed/playlists/aodt9etokc"></iframe>
            </body></html>"#;
        let page = Page::parse(html, &Url::parse("https://blog.test/post").unwrap());
        let found: Vec<String> = embeds_in(&page).iter().map(|u| u.to_string()).collect();
        assert_eq!(
            found,
            vec![
                "https://fast.wistia.net/embed/iframe/cmst5825to",
                "https://fast.wistia.net/embed/iframe/9pyv4cqb5n",
                "https://fast.wistia.net/embed/playlists/aodt9etokc",
            ]
        );
    }

    #[tokio::test]
    async fn media_resolve_with_every_asset_and_their_captions() {
        let resolver = WistiaResolver::new(Http::replay(fixture()));
        let resolved = resolver
            .resolve(&Url::parse("https://fast.wistia.net/embed/iframe/cmst5825to").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("cmst5825to"));
        assert_eq!(resolved.title.as_deref(), Some("Playlists Overview"));
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(35.077)));
        assert_eq!(resolved.uploaded_at.unwrap().as_second(), 1343830501);
        assert_eq!(
            resolved.thumbnail.as_ref().map(|u| u.as_str()),
            Some(
                "https://embed-ssl.wistia.com/deliveries/3edc7b8b5d638393b68168c8cc1e035db6827040.jpg"
            )
        );
        assert_eq!(
            resolved.variants.len(),
            4,
            "{:?}",
            resolved
                .variants
                .iter()
                .map(|v| v.url.as_str())
                .collect::<Vec<_>>()
        );
        let original = resolved
            .variants
            .iter()
            .find(|v| v.format_id.as_deref() == Some("original"))
            .unwrap();
        assert_eq!((original.width, original.height), (Some(1280), Some(720)));
        assert_eq!(original.size, Some(43427280));
        assert_eq!(original.bitrate, Some(9672000));
        assert_eq!(original.container, Some(Container::Mp4));
        assert!(original.url.as_str().ends_with(".mp4"), "{}", original.url);
        let hd = resolved
            .variants
            .iter()
            .find(|v| v.format_id.as_deref() == Some("hd_mp4_video"))
            .unwrap();
        assert_eq!(hd.height, Some(540));
        assert_eq!(hd.size, Some(4997682));
        assert_eq!(hd.video, Some(VideoCodec::H264));
        assert_eq!(hd.label.as_deref(), Some("540p"));
        assert!(
            resolved
                .variants
                .iter()
                .all(|v| v.kind == VariantKind::File)
        );
        assert_eq!(resolved.subtitles.len(), 1);
        assert_eq!(resolved.subtitles[0].language, "eng");
        assert_eq!(
            resolved.subtitles[0].url.as_str(),
            "https://fast.wistia.com/embed/captions/cmst5825to.vtt?language=eng"
        );
        let error = resolver
            .resolve(&Url::parse("https://wistia.com/medias/zzzzzzzzzz").unwrap())
            .await
            .unwrap_err();
        assert!(matches!(error, ResolveError::NotFound(_)), "{error}");
    }

    #[test]
    fn streamed_media_add_their_manifest() {
        let media: Value = serde_json::from_str(
            r#"{"hashedId":"cmst5825to","hls_enabled":true,"duration":10,"assets":[{"type":"original","url":"https://embed-ssl.wistia.com/deliveries/abc.bin","ext":"mp4","width":1280,"height":720,"size":100,"bitrate":900,"status":2,"display_name":"Original File"},{"type":"hls_video","url":"https://embed-ssl.wistia.com/deliveries/def.bin","container":"m3u8","status":2}]}"#,
        )
        .unwrap();
        let variants = variants_of(&media);
        assert_eq!(variants.len(), 3);
        assert_eq!(variants[1].kind, VariantKind::Hls);
        assert!(variants[1].url.as_str().ends_with("def.m3u8"));
        assert_eq!(
            variants[2].url.as_str(),
            "https://fast.wistia.net/embed/medias/cmst5825to.m3u8"
        );
    }

    #[tokio::test]
    async fn playlists_and_channels_list_their_media() {
        let resolver = WistiaResolver::new(Http::replay(fixture()));
        let playlist = match resolver
            .resolve(&Url::parse("https://fast.wistia.net/embed/playlists/aodt9etokc").unwrap())
            .await
            .unwrap()
        {
            Resolution::Playlist(p) => p,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(playlist.entries.len(), 3);
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://fast.wistia.net/embed/iframe/cmst5825to"
        );
        assert_eq!(
            playlist.entries[0].title.as_deref(),
            Some("Playlists Overview")
        );
        assert_eq!(
            playlist.entries[0].duration,
            Some(Duration::from_secs_f64(35.077))
        );
        let channel = match resolver
            .resolve(&Url::parse("https://fast.wistia.net/embed/channel/24076kc8tf").unwrap())
            .await
            .unwrap()
        {
            Resolution::Playlist(p) => p,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(
            channel.title.as_deref(),
            Some("Lenny Videos Hosted on Wistia")
        );
        assert!(channel.entries.len() > 1, "{}", channel.entries.len());
        assert_eq!(
            channel.entries[0].url.as_str(),
            "https://fast.wistia.net/embed/iframe/9pyv4cqb5n"
        );
        assert_eq!(
            channel.entries[0].title.as_deref(),
            Some("Lenny - A Better Way to Deliver Video")
        );
        assert_eq!(
            channel.entries[0].duration,
            Some(Duration::from_secs_f64(40.207))
        );
        let error = resolver
            .resolve(&Url::parse("https://fast.wistia.net/embed/channel/zzzzzzzzzz").unwrap())
            .await
            .unwrap_err();
        assert!(matches!(error, ResolveError::NotFound(_)), "{error}");
    }
}
