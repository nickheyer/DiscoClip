//! Naver TV clips, lives, channels and playlists, through the API the web app calls: every
//! request is signed with the HMAC key the app carries. A clip's play-info names the video
//! id and key its player passes to the VOD play API, which lists MP4 renditions by height,
//! an HLS stream with the query its segments carry, and captions. A live's play-info
//! carries the HLS playlist in its playback body. A channel's clips and playlists, and a
//! playlist's clips, come from the same API. `naver.me` short links are unwrapped to the
//! page they point at.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, SubtitleFormat, SubtitleTrack, Tag, Variant, clean_title, hls, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "navertv";

const API: &str = "https://apis.naver.com/now_web2/now_web_api/v1";
const VOD_API: &str = "https://apis.naver.com/rmcnmv/rmcnmv/vod/play/v2.0/";
/// The key the web app signs every API request with.
const SIGNING_KEY: &[u8] = b"nbxvs5nwNG9QKEWK0ADjYA4JZoujF4gHcIwvoCxFTPAeamq5eemvt5IWAYXxrbYM";
/// How many clips or playlists a channel listing is read up to.
const PAGE_SIZE: usize = 50;

static RE_CHANNEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_.-]+$").unwrap());
/// `.ttml` or `.vtt` at the end of a caption link: the same cues are served in both.
static RE_CAPTION_EXT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.(ttml|vtt)$").unwrap());

/// The section of a channel a link opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelTab {
    /// The channel's clips, newest first.
    Clips,
    Playlists,
    /// The channel's live, when one is on.
    Live,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Clip {
        no: u64,
    },
    Live {
        no: u64,
    },
    /// A playlist, by its own page or by a clip page that names it.
    Playlist {
        no: u64,
        /// The clip whose page named the playlist, when one did.
        clip: Option<u64>,
    },
    Channel {
        id: String,
        tab: ChannelTab,
    },
    /// A `naver.me` short link.
    Short(Url),
}

/// First path segments of tv.naver.com that are pages of the site, not channels.
const RESERVED: &[&str] = &[
    "v",
    "l",
    "h",
    "embed",
    "playlist",
    "search",
    "my",
    "live",
    "schedule",
    "about",
    "listen",
    "f",
    "i",
    "r",
    "rp",
    "br",
    "b",
    "ct",
    "chart",
    "redeem",
    "watch",
    "s",
    "n",
    "api",
    "monitor",
    "chromecast",
    "login",
    "logout",
    "apple-app-site-association",
    "apple-app-site-association.json",
    "robots.txt",
    "favicon.ico",
];

fn tab_named(name: &str) -> ChannelTab {
    match name {
        "playlists" | "playlist" => ChannelTab::Playlists,
        "live" => ChannelTab::Live,
        _ => ChannelTab::Clips,
    }
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
    if host == "naver.me" || host == "www.naver.me" {
        return match segments.as_slice() {
            [code] if RE_CHANNEL.is_match(code) => Some(Link::Short(url.clone())),
            _ => None,
        };
    }
    if !matches!(
        host.as_str(),
        "tv.naver.com" | "m.tv.naver.com" | "tvcast.naver.com" | "m.tvcast.naver.com"
    ) {
        return None;
    }
    let number = |s: &str| s.parse::<u64>().ok();
    match segments.as_slice() {
        ["v" | "embed" | "h", no] => {
            let no = number(no)?;
            match util::query_param(url, "playlistNo").and_then(|p| p.parse::<u64>().ok()) {
                Some(playlist) => Some(Link::Playlist {
                    no: playlist,
                    clip: Some(no),
                }),
                None => Some(Link::Clip { no }),
            }
        }
        ["l", no] | ["l", no, "sharePlayer"] => Some(Link::Live { no: number(no)? }),
        ["playlist", no] => Some(Link::Playlist {
            no: number(no)?,
            clip: None,
        }),
        ["s", id] if RE_CHANNEL.is_match(id) => Some(Link::Channel {
            id: id.to_string(),
            tab: ChannelTab::Clips,
        }),
        [id] if RE_CHANNEL.is_match(id) && !RESERVED.contains(id) => Some(Link::Channel {
            id: id.to_string(),
            tab: util::query_param(url, "tab")
                .map(|t| tab_named(&t))
                .unwrap_or(ChannelTab::Clips),
        }),
        [id, tab] if RE_CHANNEL.is_match(id) && !RESERVED.contains(id) => Some(Link::Channel {
            id: id.to_string(),
            tab: tab_named(tab),
        }),
        _ => None,
    }
}

/// `path` on the API, signed the way the web app signs it: `msgpad` is the time in
/// milliseconds and `md` the HMAC-SHA1 of the endpoint and `msgpad`.
pub fn signed_api_url(path: &str, now: Timestamp) -> Url {
    let endpoint = format!("{API}{path}");
    let msgpad = now.as_millisecond();
    let prefix: String = endpoint.chars().take(255).collect();
    let md = util::b64_encode(&util::hmac_sha1(
        SIGNING_KEY,
        format!("{prefix}{msgpad}").as_bytes(),
    ));
    let mut url = Url::parse(&endpoint).expect("the API path is a valid URL");
    url.query_pairs_mut()
        .append_pair("msgpad", &msgpad.to_string())
        .append_pair("md", &md);
    url
}

/// `2026-04-13T02:42:00+0900`: the API writes zone offsets without a colon.
pub fn parse_time(text: &str) -> Option<Timestamp> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let bytes = text.as_bytes();
    let fixed = match bytes.len().checked_sub(5) {
        Some(at)
            if matches!(bytes[at], b'+' | b'-')
                && bytes[at + 1..].iter().all(u8::is_ascii_digit) =>
        {
            format!("{}:{}", &text[..at + 3], &text[at + 3..])
        }
        _ => text.to_string(),
    };
    fixed
        .parse::<Timestamp>()
        .ok()
        .or_else(|| util::parse_timestamp(text))
}

fn clip_page(no: u64) -> Url {
    Url::parse(&format!("https://tv.naver.com/v/{no}")).expect("valid")
}

fn channel_page(id: &str) -> Option<Url> {
    Url::parse(&format!("https://tv.naver.com/{id}")).ok()
}

/// The MP4 renditions and the HLS stream the VOD play API lists.
pub struct Played {
    pub variants: Vec<Variant>,
    pub subtitles: Vec<SubtitleTrack>,
    pub duration: Option<Duration>,
}

fn video_codec(kind: &str) -> VideoCodec {
    match kind.to_ascii_lowercase().as_str() {
        "hvc1" | "hev1" | "hevc" | "h265" => VideoCodec::H265,
        "vp9" | "vp09" => VideoCodec::Vp9,
        "av1" | "av01" => VideoCodec::Av1,
        _ => VideoCodec::H264,
    }
}

/// One MP4 rendition of the play API: `source` with its encoding option and bitrates.
fn file_variant(stream: &Value, kind: &str, query: &[(String, String)]) -> Option<Variant> {
    let source = util::url_of(&stream["source"], None)?;
    let pairs: Vec<(&str, &str)> = query
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let mut v = Variant::file(util::with_query(&source, &pairs));
    let encoding = &stream["encodingOption"];
    v.container = Some(Container::Mp4);
    v.video = Some(video_codec(kind));
    v.audio = Some(AudioCodec::Aac);
    v.width = util::u32_of(&encoding["width"]).filter(|w| *w > 0);
    v.height = util::u32_of(&encoding["height"]).filter(|h| *h > 0);
    let video_kbps = util::float(&stream["bitrate"]["video"]).unwrap_or(0.0);
    let audio_kbps = util::float(&stream["bitrate"]["audio"]).unwrap_or(0.0);
    let kbps = video_kbps + audio_kbps;
    v.bitrate = (kbps > 0.0).then_some((kbps * 1000.0) as u64);
    v.size = util::uint(&stream["size"]).filter(|s| *s > 0);
    v.duration = util::seconds(&stream["duration"]);
    let name = util::text(&encoding["name"])
        .or_else(|| util::text(&encoding["id"]))
        .unwrap_or_default();
    v.format_id = Some(format!("{kind}_{name}"));
    v.label = if name.is_empty() {
        v.height.map(|h| format!("{h}p"))
    } else {
        Some(name.to_ascii_lowercase())
    };
    Some(v)
}

/// The caption tracks the play API lists. Cues offered as TTML are taken as WebVTT, which
/// the same link serves under the other extension.
pub fn subtitles_of(played: &Value) -> Vec<SubtitleTrack> {
    let mut tracks = Vec::new();
    for caption in played["captions"]["list"].as_array().into_iter().flatten() {
        let Some(source) = util::text(&caption["source"]) else {
            continue;
        };
        let (link, format) = if RE_CAPTION_EXT.is_match(&source) {
            (
                RE_CAPTION_EXT.replace(&source, ".vtt").into_owned(),
                SubtitleFormat::Vtt,
            )
        } else if source.ends_with(".srt") {
            (source.clone(), SubtitleFormat::Srt)
        } else {
            (source.clone(), SubtitleFormat::Vtt)
        };
        let Ok(url) = Url::parse(&link) else {
            continue;
        };
        let kind = util::text(&caption["type"]).unwrap_or_default();
        let language = util::text(&caption["locale"])
            .or_else(|| {
                let language = util::text(&caption["language"])?;
                Some(match util::text(&caption["country"]) {
                    Some(country) => format!("{language}-{country}"),
                    None => language,
                })
            })
            .unwrap_or_else(|| "und".to_string());
        let name = match (
            util::text(&caption["label"]),
            util::text(&caption["fanName"]),
        ) {
            (Some(label), Some(fan)) => Some(format!("{label} - {fan}")),
            (label, fan) => label.or(fan),
        };
        tracks.push(SubtitleTrack {
            url,
            language,
            name,
            format,
            auto: matches!(kind.as_str(), "auto" | "stt"),
            headers: Vec::new(),
        });
    }
    tracks
}

pub struct NaverTvResolver {
    http: Http,
}

impl NaverTvResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// The `result` of an API answer, or what its status means for `origin`.
    async fn api(&self, path: &str, origin: &Url) -> Result<Value, ResolveError> {
        let url = signed_api_url(path, Timestamp::now());
        let response = self
            .http
            .get(url.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "application/json")
            .header("referer", "https://tv.naver.com/")
            .send()
            .await?;
        let status = response.status.as_u16();
        let text = response.text(MAX_PAGE).await?;
        let answer: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let code = util::text(&answer["statusCode"]).unwrap_or_default();
        let message = util::text(&answer["errorMessage"])
            .filter(|m| !m.is_empty())
            .or_else(|| util::text(&answer["statusMessage"]).filter(|m| !m.is_empty()))
            .unwrap_or_else(|| format!("the API answered HTTP {status}"));
        if status == 429 {
            return Err(ResolveError::RateLimited(origin.clone()));
        }
        if code == "SUCCESS" && (200..300).contains(&status) {
            return Ok(answer["result"].clone());
        }
        if status == 404
            || code.ends_with("_NOT_FOUND")
            || message.contains("is not found")
            || message.contains("채널이 없습니다")
        {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        if code == "LOGIN_REQUIRED" {
            return Err(ResolveError::unavailable(
                origin,
                "Naver asks for a login to read this",
            ));
        }
        if answer.is_null() {
            return Err(ResolveError::malformed(
                origin,
                format!("the API answered HTTP {status} without JSON"),
            ));
        }
        Err(ResolveError::unavailable(origin, message))
    }

    /// The renditions of a video, from the play API the player calls with the clip's key.
    async fn play(&self, video_id: &str, key: &str, origin: &Url) -> Result<Played, ResolveError> {
        let mut api = Url::parse(&format!("{VOD_API}{video_id}"))
            .map_err(|e| ResolveError::malformed(origin, format!("video id: {e}")))?;
        api.query_pairs_mut().append_pair("key", key);
        let fetched = super::fetch_ok(&self.http, &api, PLATFORM, BROWSER_UA, &[], MAX_PAGE)
            .await
            .map_err(|e| e.at(origin))?;
        let played = fetched.json(origin)?;
        let mut variants = Vec::new();
        for stream in played["videos"]["list"].as_array().into_iter().flatten() {
            let kind = util::text(&stream["type"]).unwrap_or_else(|| "H264".to_string());
            if let Some(v) = file_variant(stream, &kind, &[]) {
                variants.push(v);
            }
        }
        for set in played["streams"].as_array().into_iter().flatten() {
            let query: Vec<(String, String)> = set["keys"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|k| Some((util::text(&k["name"])?, util::text(&k["value"])?)))
                .collect();
            let kind = util::text(&set["type"]).unwrap_or_else(|| "HLS".to_string());
            if let Some(videos) = set["videos"].as_array().filter(|v| !v.is_empty()) {
                for stream in videos {
                    if let Some(v) = file_variant(stream, &kind, &query) {
                        variants.push(v);
                    }
                }
                continue;
            }
            if kind != "HLS" {
                continue;
            }
            let Some(source) = util::url_of(&set["source"], None) else {
                continue;
            };
            let pairs: Vec<(&str, &str)> = query
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let playlist = util::with_query(&source, &pairs);
            let expanded = hls::expand(&self.http, &playlist, PLATFORM, BROWSER_UA, &[])
                .await
                .map_err(|e| e.at(origin))?;
            for mut v in expanded.variants {
                v.query = query.clone();
                v.format_id = Some(match &v.label {
                    Some(label) => format!("hls-{label}"),
                    None => "hls".to_string(),
                });
                variants.push(v);
            }
        }
        if variants.is_empty() {
            return Err(ResolveError::unavailable(
                origin,
                "the play API lists no renditions",
            ));
        }
        let duration = util::seconds(&played["meta"]["duration"])
            .or_else(|| variants.iter().find_map(|v| v.duration));
        Ok(Played {
            variants,
            subtitles: subtitles_of(&played),
            duration,
        })
    }

    async fn resolve_clip(&self, no: u64, url: &Url) -> Result<Resolution, ResolveError> {
        let info = self.api(&format!("/clips/{no}/play-info"), url).await?;
        let clip = &info["clip"];
        let play = &info["play"];
        let playable = util::text(&play["playable"]).unwrap_or_default();
        let (Some(video_id), Some(key)) =
            (util::text(&clip["videoId"]), util::text(&play["inKey"]))
        else {
            return Err(match playable.as_str() {
                "NOT_COUNTRY_AVAILABLE" => ResolveError::unavailable(url, "available only in KR"),
                _ if util::boolean(&clip["adultVideo"]).unwrap_or(false) => {
                    ResolveError::unavailable(
                        url,
                        "the clip is restricted to logged-in adults on Naver",
                    )
                }
                "" => ResolveError::malformed(url, "the play-info names no video id and key"),
                other => {
                    ResolveError::unavailable(url, format!("the clip is not playable ({other})"))
                }
            });
        };
        let played = self.play(&video_id, &key, url).await?;
        let channel_id = util::text(&clip["channelId"]).filter(|c| !c.is_empty());
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(no.to_string());
        resolved.title = clip["title"].as_str().and_then(clean_title);
        resolved.description = clip["description"]
            .as_str()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(String::from);
        resolved.uploader = clip["channelName"].as_str().and_then(clean_title);
        resolved.uploader_url = util::url_of(&clip["channelUrl"], None)
            .or_else(|| channel_id.as_deref().and_then(channel_page));
        resolved.uploaded_at = util::text(&clip["firstExposureDatetime"])
            .and_then(|t| parse_time(&t))
            .or_else(|| util::text(&clip["registerDateTime"]).and_then(|t| parse_time(&t)));
        resolved.duration = util::seconds(&clip["playTime"]).or(played.duration);
        resolved.thumbnail = util::url_of(&clip["thumbnailImageUrl"], None);
        resolved.webpage_url = Some(clip_page(no));
        resolved.age_limit = util::boolean(&clip["adultVideo"])
            .filter(|adult| *adult)
            .map(|_| 19);
        resolved.subtitles = played.subtitles;
        resolved.variants = played.variants;
        Ok(Resolution::from(resolved))
    }

    async fn resolve_live(&self, no: u64, url: &Url) -> Result<Resolution, ResolveError> {
        let info = self
            .api(
                &format!("/live-end/normal/{no}/play-info?renewLastPlayDate=true"),
                url,
            )
            .await?;
        let live = &info["live"];
        let status = util::text(&live["liveStatus"]).unwrap_or_default();
        if status == "CLOSED" {
            return Err(ResolveError::unavailable(url, "the live has ended"));
        }
        let playback: Value = util::text(&info["playbackBody"])
            .and_then(|body| serde_json::from_str(&body).ok())
            .unwrap_or(Value::Null);
        let playlist = playback["media"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| util::text(&m["protocol"]).is_some_and(|p| p.eq_ignore_ascii_case("HLS")))
            .find_map(|m| util::url_of(&m["path"], None));
        let Some(playlist) = playlist else {
            let playable = util::text(&info["playable"]).unwrap_or_default();
            return Err(match (status.as_str(), playable.as_str()) {
                (_, "NOT_COUNTRY_AVAILABLE") => {
                    ResolveError::unavailable(url, "available only in KR")
                }
                ("OPENED", _) => ResolveError::unavailable(url, "the live names no stream"),
                ("", _) => ResolveError::NotFound(url.clone()),
                (other, _) => {
                    ResolveError::unavailable(url, format!("the live is {}", other.to_lowercase()))
                }
            });
        };
        let headers = vec![("referer".to_string(), "https://tv.naver.com/".to_string())];
        let expanded = hls::expand(&self.http, &playlist, PLATFORM, BROWSER_UA, &headers)
            .await
            .map_err(|e| e.at(url))?;
        let channel_id = util::text(&live["channelId"]).filter(|c| !c.is_empty());
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(no.to_string());
        resolved.title = live["title"].as_str().and_then(clean_title);
        resolved.description = live["description"]
            .as_str()
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(String::from);
        resolved.uploader = live["channelName"].as_str().and_then(clean_title);
        resolved.uploader_url = channel_id.as_deref().and_then(channel_page);
        resolved.uploaded_at = ["startDateTime", "startTime", "startYmdt"]
            .iter()
            .find_map(|key| util::text(&live[*key]).and_then(|t| parse_time(&t)));
        resolved.thumbnail = util::url_of(&live["thumbnailImageUrl"], None);
        resolved.webpage_url = Url::parse(&format!("https://tv.naver.com/l/{no}")).ok();
        resolved.live = true;
        resolved.subtitles = expanded.subtitles;
        resolved.variants = expanded
            .variants
            .into_iter()
            .map(|mut v| {
                v.live = true;
                v
            })
            .collect();
        Ok(Resolution::from(resolved))
    }

    async fn resolve_playlist(
        &self,
        no: u64,
        clip: Option<u64>,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let path = match clip {
            Some(clip) => format!("/clips/{clip}/playlist?playlistNo={no}"),
            None => format!("/playlist/{no}"),
        };
        let result = self.api(&path, url).await?;
        if result.is_null() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let entries: Vec<PlaylistEntry> = result["clips"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|clip| {
                let clip_no = util::uint(&clip["clipNo"])?;
                Some(PlaylistEntry {
                    url: clip_page(clip_no),
                    title: clip["title"].as_str().and_then(clean_title),
                    duration: util::seconds(&clip["playTime"]),
                })
            })
            .collect();
        if entries.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let total = util::uint(&result["clipCount"])
            .map(|n| n as usize)
            .filter(|n| *n >= entries.len())
            .unwrap_or(entries.len());
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(no.to_string()),
            title: result["title"].as_str().and_then(clean_title),
            total: Some(total),
            entries,
        }))
    }

    async fn resolve_channel(
        &self,
        id: &str,
        tab: ChannelTab,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let info = self.api(&format!("/channel/{id}/info"), url).await?;
        let name = info["channelName"]
            .as_str()
            .and_then(clean_title)
            .unwrap_or_else(|| id.to_string());
        match tab {
            ChannelTab::Live => {
                let live = &info["currentLive"];
                match (util::text(&live["liveStatus"]), util::uint(&live["liveNo"])) {
                    (Some(status), Some(live_no)) if status == "OPENED" => {
                        Err(ResolveError::Redirect(
                            Url::parse(&format!("https://tv.naver.com/l/{live_no}"))
                                .expect("valid"),
                        ))
                    }
                    _ => Err(ResolveError::unavailable(
                        url,
                        format!("{name} is not live right now"),
                    )),
                }
            }
            ChannelTab::Clips => {
                let listing = self
                    .api(
                        &format!("/channel/{id}/general-clips?page=1&pageSize={PAGE_SIZE}"),
                        url,
                    )
                    .await?;
                let entries: Vec<PlaylistEntry> = listing["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|clip| {
                        let clip_no = util::uint(&clip["clipNo"])?;
                        Some(PlaylistEntry {
                            url: clip_page(clip_no),
                            title: clip["title"].as_str().and_then(clean_title),
                            duration: util::seconds(&clip["playTime"]),
                        })
                    })
                    .collect();
                if entries.is_empty() {
                    return Err(ResolveError::NotFound(url.clone()));
                }
                let total = util::uint(&listing["total"])
                    .map(|n| n as usize)
                    .filter(|n| *n >= entries.len())
                    .unwrap_or(entries.len());
                Ok(Resolution::Playlist(Playlist {
                    resolver: PLATFORM.to_string(),
                    id: Some(id.to_string()),
                    title: Some(name),
                    total: Some(total),
                    entries,
                }))
            }
            ChannelTab::Playlists => {
                let listing = self
                    .api(
                        &format!("/channel/{id}/playlists?page=1&pageSize={PAGE_SIZE}"),
                        url,
                    )
                    .await?;
                let entries: Vec<PlaylistEntry> = listing["data"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|playlist| {
                        let playlist_no = util::uint(&playlist["playlistNo"])?;
                        Some(PlaylistEntry {
                            url: Url::parse(&format!(
                                "https://tv.naver.com/playlist/{playlist_no}"
                            ))
                            .ok()?,
                            title: playlist["title"].as_str().and_then(clean_title),
                            duration: None,
                        })
                    })
                    .collect();
                if entries.is_empty() {
                    return Err(ResolveError::NotFound(url.clone()));
                }
                let total = util::uint(&listing["total"])
                    .map(|n| n as usize)
                    .filter(|n| *n >= entries.len())
                    .unwrap_or(entries.len());
                Ok(Resolution::Playlist(Playlist {
                    resolver: PLATFORM.to_string(),
                    id: Some(format!("{id}-playlists")),
                    title: Some(format!("{name} playlists")),
                    total: Some(total),
                    entries,
                }))
            }
        }
    }

    /// Where a `naver.me` link leads, handed back to the registry.
    async fn resolve_short(&self, short: &Url, url: &Url) -> Result<Resolution, ResolveError> {
        let target = self
            .http
            .unwrap_redirects(short, Some(PLATFORM), BROWSER_UA)
            .await?;
        if target == *short || target.host_str().is_some_and(|h| h.ends_with("naver.me")) {
            return Err(ResolveError::NotFound(url.clone()));
        }
        Err(ResolveError::Redirect(target))
    }
}

#[async_trait]
impl Resolver for NaverTvResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Naver TV",
            hosts: &["tv.naver.com", "tvcast.naver.com", "naver.me"],
            features: &["clips", "live", "channels", "playlists", "short links"],
            formats: &["mp4", "hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::Video, Tag::Live],
            session: SessionSupport::None,
            examples: &[
                "https://tv.naver.com/v/67838091",
                "https://tv.naver.com/v/2660764?playlistNo=188601",
                "https://tv.naver.com/playlist/188601",
                "https://tv.naver.com/l/184843",
                "https://tv.naver.com/kbsnews",
                "https://tv.naver.com/kbsnews/playlists",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Clip { no } => self.resolve_clip(no, url).await,
            Link::Live { no } => self.resolve_live(no, url).await,
            Link::Playlist { no, clip } => self.resolve_playlist(no, clip, url).await,
            Link::Channel { id, tab } => self.resolve_channel(&id, tab, url).await,
            Link::Short(short) => self.resolve_short(&short, url).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };
    use crate::resolve::VariantKind;
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

    fn redirect(url: &str, to: &str) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: "GET".into(),
                url: url.into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status: 200,
                url: to.into(),
                headers: vec![("content-type".into(), "text/html".into())],
                body: RecordedBody::Text("<html></html>".into()),
                truncated: false,
            },
        }
    }

    fn ok(result: Value) -> String {
        json!({"statusCode": "SUCCESS", "statusMessage": "", "errorMessage": "", "result": result})
            .to_string()
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://tv.naver.com/v/67838091"),
            Some(Link::Clip { no: 67838091 })
        );
        assert_eq!(
            link("http://m.tv.naver.com/v/81652?query=1"),
            Some(Link::Clip { no: 81652 })
        );
        assert_eq!(
            link("http://tvcast.naver.com/v/81652"),
            Some(Link::Clip { no: 81652 })
        );
        assert_eq!(
            link("https://tv.naver.com/embed/67838091"),
            Some(Link::Clip { no: 67838091 })
        );
        assert_eq!(
            link("https://tv.naver.com/h/67838091"),
            Some(Link::Clip { no: 67838091 })
        );
        assert_eq!(
            link("https://tv.naver.com/v/2660764?playlistNo=188601"),
            Some(Link::Playlist {
                no: 188601,
                clip: Some(2660764)
            })
        );
        assert_eq!(
            link("https://tv.naver.com/playlist/188601"),
            Some(Link::Playlist {
                no: 188601,
                clip: None
            })
        );
        assert_eq!(
            link("https://tv.naver.com/l/184843"),
            Some(Link::Live { no: 184843 })
        );
        assert_eq!(
            link("https://tv.naver.com/l/184843/sharePlayer"),
            Some(Link::Live { no: 184843 })
        );
        assert_eq!(
            link("https://tv.naver.com/kbsnews"),
            Some(Link::Channel {
                id: "kbsnews".into(),
                tab: ChannelTab::Clips
            })
        );
        assert_eq!(
            link("https://tv.naver.com/kbsnews/playlists"),
            Some(Link::Channel {
                id: "kbsnews".into(),
                tab: ChannelTab::Playlists
            })
        );
        assert_eq!(
            link("https://tv.naver.com/kbsnews?tab=live"),
            Some(Link::Channel {
                id: "kbsnews".into(),
                tab: ChannelTab::Live
            })
        );
        assert_eq!(
            link("https://tv.naver.com/s/kbsnews"),
            Some(Link::Channel {
                id: "kbsnews".into(),
                tab: ChannelTab::Clips
            })
        );
        assert_eq!(
            link("https://naver.me/5abcDEFG"),
            Some(Link::Short(
                Url::parse("https://naver.me/5abcDEFG").unwrap()
            ))
        );
        assert_eq!(link("https://tv.naver.com/"), None);
        assert_eq!(link("https://tv.naver.com/search/news"), None);
        assert_eq!(link("https://tv.naver.com/live"), None);
        assert_eq!(link("https://tv.naver.com/v/notanumber"), None);
        assert_eq!(link("https://naver.me/"), None);
        assert_eq!(link("https://blog.naver.com/kbsnews"), None);
        assert_eq!(link("https://example.com/v/67838091"), None);
    }

    #[test]
    fn api_links_are_signed_like_the_web_app() {
        let now = Timestamp::from_millisecond(1_700_000_000_000).unwrap();
        let url = signed_api_url("/clips/67838091/play-info", now);
        assert_eq!(url.host_str(), Some("apis.naver.com"));
        assert_eq!(
            url.path(),
            "/now_web2/now_web_api/v1/clips/67838091/play-info"
        );
        assert_eq!(
            util::query_param(&url, "msgpad").as_deref(),
            Some("1700000000000")
        );
        let md = util::query_param(&url, "md").unwrap();
        let expected = util::b64_encode(&util::hmac_sha1(
            SIGNING_KEY,
            b"https://apis.naver.com/now_web2/now_web_api/v1/clips/67838091/play-info1700000000000",
        ));
        assert_eq!(md, expected);
        let with_query = signed_api_url("/live-end/normal/1/play-info?renewLastPlayDate=true", now);
        assert_eq!(
            util::query_param(&with_query, "renewLastPlayDate").as_deref(),
            Some("true")
        );
        assert!(util::query_param(&with_query, "md").is_some());
    }

    #[test]
    fn times_without_a_colon_in_the_offset_are_read() {
        let at = parse_time("2026-04-13T02:42:00+0900").unwrap();
        assert_eq!(at, "2026-04-12T17:42:00Z".parse::<Timestamp>().unwrap());
        assert_eq!(
            parse_time("2025-01-08T23:50:53+09:00"),
            Some("2025-01-08T14:50:53Z".parse::<Timestamp>().unwrap())
        );
        assert_eq!(parse_time(""), None);
    }

    fn play_info() -> String {
        ok(json!({
            "clip": {
                "clipNo": 67838091, "videoId": "D061B557B30FF5EB072BB21FA7062EFA1C02",
                "title": "[라인W 날씨] 내일 아침 서울 체감 -19도…호남·충남 대설",
                "description": "\n내일은 이번 겨울 들어 가장 강력한 한파가 찾아오겠습니다.\n",
                "channelId": "kbsnews", "channelName": "KBS뉴스", "channelUrl": "https://tv.naver.com/kbsnews",
                "firstExposureDatetime": "2025-01-08T23:50:53+0900", "playTime": 69, "adultVideo": false,
                "thumbnailImageUrl": "https://phinf.pstatic.net/tvcast/cover.jpg"
            },
            "play": {"inKey": "V128521bbbec2f66f", "playable": "PLAYABLE"}
        }))
    }

    fn vod_play() -> String {
        json!({
            "meta": {"subject": "[라인W 날씨]", "user": {"id": "ne****", "name": "KBS뉴스", "url": "http://tvcast.naver.com/kbsnews"}, "cover": {"source": "https://phinf.pstatic.net/tvcast/cover.jpg?type=f640"}},
            "videos": {"list": [
                {"type": "avc1", "source": "https://a01-g-naver-vod.akamaized.net/navertv/270.mp4?__gda__=1", "encodingOption": {"id": "270P_480_500_128", "name": "270P", "width": 480, "height": 270}, "bitrate": {"video": 332.0, "audio": 96.0}, "size": 3763832, "duration": 69.9},
                {"type": "avc1", "source": "https://a01-g-naver-vod.akamaized.net/navertv/720.mp4?__gda__=1", "encodingOption": {"id": "720P_1280_2048_192_B", "name": "720P", "width": 1280, "height": 720}, "bitrate": {"video": 1325.0, "audio": 192.0}, "size": 13295005, "duration": 69.9}
            ]},
            "streams": [{"type": "HLS", "keys": [{"type": "param", "name": "__gda__", "value": "1790273971_118645b"}], "source": "https://a01-g-naver-vod.akamaized.net/navertv/hls/master.m3u8"}],
            "captions": {"list": [{"locale": "ko", "type": "stt", "label": "한국어", "source": "https://resources-rmcnmv.akamaized.net/navertv/c/ko_stt.ttml"}]}
        })
        .to_string()
    }

    #[tokio::test]
    async fn clips_resolve_with_mp4_renditions_hls_and_captions() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://apis.naver.com/now_web2/now_web_api/v1/clips/67838091/play-info",
            200,
            "application/json",
            play_info(),
        ));
        fixture.exchanges.push(get(
            "https://apis.naver.com/rmcnmv/rmcnmv/vod/play/v2.0/D061B557B30FF5EB072BB21FA7062EFA1C02?key=V128521bbbec2f66f",
            200,
            "application/json",
            vod_play(),
        ));
        fixture.exchanges.push(get(
            "https://a01-g-naver-vod.akamaized.net/navertv/hls/master.m3u8?__gda__=1790273971_118645b",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1553408,RESOLUTION=1280x720\n720.m3u8\n".into(),
        ));
        fixture.exchanges.push(get(
            "https://a01-g-naver-vod.akamaized.net/navertv/hls/720.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n0.ts\n#EXT-X-ENDLIST\n".into(),
        ));
        let resolver = NaverTvResolver::new(Http::replay(fixture));
        let url = Url::parse("https://tv.naver.com/v/67838091").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("67838091"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("[라인W 날씨] 내일 아침 서울 체감 -19도…호남·충남 대설")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("KBS뉴스"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://tv.naver.com/kbsnews"
        );
        assert_eq!(
            resolved.uploaded_at,
            Some("2025-01-08T14:50:53Z".parse::<Timestamp>().unwrap())
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(69)));
        assert_eq!(resolved.age_limit, None);
        assert_eq!(resolved.variants.len(), 3, "two MP4 renditions and one HLS");
        let best = &resolved.variants[1];
        assert_eq!(best.height, Some(720));
        assert_eq!(best.bitrate, Some(1_517_000));
        assert_eq!(best.size, Some(13295005));
        assert_eq!(best.format_id.as_deref(), Some("avc1_720P"));
        assert_eq!(best.label.as_deref(), Some("720p"));
        assert_eq!(best.container, Some(Container::Mp4));
        assert_eq!(best.video, Some(VideoCodec::H264));
        let stream = &resolved.variants[2];
        assert_eq!(stream.kind, VariantKind::Hls);
        assert_eq!(stream.height, Some(720));
        assert_eq!(stream.format_id.as_deref(), Some("hls-720p"));
        assert_eq!(
            stream.query,
            vec![("__gda__".to_string(), "1790273971_118645b".to_string())]
        );
        assert_eq!(resolved.subtitles.len(), 1);
        assert_eq!(resolved.subtitles[0].language, "ko");
        assert!(resolved.subtitles[0].auto);
        assert_eq!(
            resolved.subtitles[0].url.as_str(),
            "https://resources-rmcnmv.akamaized.net/navertv/c/ko_stt.vtt"
        );
        assert_eq!(resolved.subtitles[0].format, SubtitleFormat::Vtt);
    }

    #[tokio::test]
    async fn missing_geo_gated_and_adult_clips_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://apis.naver.com/now_web2/now_web_api/v1/clips/1/play-info",
            404,
            "application/json",
            json!({"statusCode": "CLIP_NOT_FOUND", "statusMessage": "삭제되었거나 제공이 중지된 동영상입니다.", "errorMessage": "Clip [1] is not found.", "result": null}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://apis.naver.com/now_web2/now_web_api/v1/clips/2/play-info",
            200,
            "application/json",
            ok(json!({"clip": {"clipNo": 2, "videoId": "X", "adultVideo": false}, "play": {"inKey": null, "playable": "NOT_COUNTRY_AVAILABLE"}})),
        ));
        fixture.exchanges.push(get(
            "https://apis.naver.com/now_web2/now_web_api/v1/clips/3/play-info",
            200,
            "application/json",
            ok(json!({"clip": {"clipNo": 3, "videoId": "X", "adultVideo": true}, "play": {"inKey": null, "playable": "NOT_ADULT_CERTIFIED"}})),
        ));
        let resolver = NaverTvResolver::new(Http::replay(fixture));
        let resolve = |s: &str| {
            let url = Url::parse(s).unwrap();
            let resolver = &resolver;
            async move { resolver.resolve(&url).await.unwrap_err() }
        };
        assert!(matches!(
            resolve("https://tv.naver.com/v/1").await,
            ResolveError::NotFound(_)
        ));
        let geo = resolve("https://tv.naver.com/v/2").await;
        assert!(
            matches!(&geo, ResolveError::Unavailable { reason, .. } if reason == "available only in KR"),
            "{geo}"
        );
        let adult = resolve("https://tv.naver.com/v/3").await;
        assert!(
            matches!(&adult, ResolveError::Unavailable { reason, .. } if reason.contains("adults")),
            "{adult}"
        );
    }

    #[tokio::test]
    async fn lives_resolve_their_playlist_or_report_the_end() {
        let playback = json!({
            "meta": {"videoId": "240273FA", "liveId": "18333419"},
            "media": [{"mediaId": "HLS", "protocol": "HLS", "path": "https://livecloud.akamaized.net/navertv/live/playlist.m3u8?hdnts=x"}]
        })
        .to_string();
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://apis.naver.com/now_web2/now_web_api/v1/live-end/normal/184843/play-info?renewLastPlayDate=true",
            200,
            "application/json",
            ok(json!({
                "playable": "NOT_COUNTRY_AVAILABLE",
                "live": {"liveStatus": "OPENED", "title": "멈추지 않는다! KBS 뉴스24", "description": "365일 24시간", "channelId": "kbsnews", "channelName": "KBS뉴스", "startDateTime": "2026-04-13T02:42:00+0900", "thumbnailImageUrl": "https://phinf.pstatic.net/tvcast/live.jpg", "liveNo": 184843},
                "playbackBody": playback
            })),
        ));
        fixture.exchanges.push(get(
            "https://livecloud.akamaized.net/navertv/live/playlist.m3u8?hdnts=x",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=4192000,CODECS=\"avc1.640028,mp4a.40.2\",RESOLUTION=1920x1080,FRAME-RATE=30.00\n1080/chunklist.m3u8\n".into(),
        ));
        fixture.exchanges.push(get(
            "https://livecloud.akamaized.net/navertv/live/1080/chunklist.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2.0,\n0.ts\n".into(),
        ));
        fixture.exchanges.push(get(
            "https://apis.naver.com/now_web2/now_web_api/v1/live-end/normal/140535/play-info?renewLastPlayDate=true",
            200,
            "application/json",
            ok(json!({"playable": "NOT_COUNTRY_AVAILABLE", "live": {"liveStatus": "CLOSED", "title": "KBS 뉴스 24"}, "playbackBody": null})),
        ));
        let resolver = NaverTvResolver::new(Http::replay(fixture));
        let url = Url::parse("https://tv.naver.com/l/184843").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert!(resolved.live);
        assert_eq!(resolved.id.as_deref(), Some("184843"));
        assert_eq!(resolved.title.as_deref(), Some("멈추지 않는다! KBS 뉴스24"));
        assert_eq!(resolved.uploader.as_deref(), Some("KBS뉴스"));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.variants.len(), 1);
        assert!(resolved.variants[0].live);
        assert_eq!(resolved.variants[0].height, Some(1080));
        let ended = resolver
            .resolve(&Url::parse("https://tv.naver.com/l/140535").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&ended, ResolveError::Unavailable { reason, .. } if reason == "the live has ended"),
            "{ended}"
        );
    }

    #[tokio::test]
    async fn channels_and_playlists_list_their_clips_and_short_links_are_unwrapped() {
        let info = ok(
            json!({"channelId": "kbsnews", "channelName": "KBS뉴스", "currentLive": {"liveNo": 184843, "liveStatus": "OPENED"}}),
        );
        let clips = json!({"pageRequest": {"page": 1, "pageSize": 50}, "total": null, "data": [
            {"clipNo": 106057190, "title": "북한군 포로 한국행", "playTime": 120},
            {"clipNo": 106049383, "title": "드디어 미국 땅 밟은 시진핑", "playTime": 179},
            {"title": "no number"}
        ]});
        let mut fixture = Fixture::new(PLATFORM, None);
        for _ in 0..3 {
            fixture.exchanges.push(get(
                "https://apis.naver.com/now_web2/now_web_api/v1/channel/kbsnews/info",
                200,
                "application/json",
                info.clone(),
            ));
        }
        fixture.exchanges.push(get(
            "https://apis.naver.com/now_web2/now_web_api/v1/channel/kbsnews/general-clips?page=1&pageSize=50",
            200,
            "application/json",
            ok(clips),
        ));
        fixture.exchanges.push(get(
            "https://apis.naver.com/now_web2/now_web_api/v1/channel/kbsnews/playlists?page=1&pageSize=50",
            200,
            "application/json",
            ok(json!({"total": 19210, "data": [{"playlistNo": 1047613, "title": "영상K_20260924", "clipCount": 4}]})),
        ));
        fixture.exchanges.push(get(
            "https://apis.naver.com/now_web2/now_web_api/v1/playlist/188601",
            200,
            "application/json",
            ok(json!({"playlistNo": 188601, "title": "아침뉴스타임_20180205", "channelId": "kbsnews", "clips": [
                {"clipNo": 2660764, "title": "北 대표단장", "playTime": 95},
                {"clipNo": 2660765, "title": "두 번째", "playTime": 80}
            ]})),
        ));
        fixture.exchanges.push(get(
            "https://apis.naver.com/now_web2/now_web_api/v1/clips/2660764/playlist?playlistNo=188601",
            200,
            "application/json",
            ok(json!({"playlistNo": 188601, "title": "아침뉴스타임_20180205", "clips": [{"clipNo": 2660764, "title": "北 대표단장", "playTime": 95}]})),
        ));
        fixture.exchanges.push(get(
            "https://apis.naver.com/now_web2/now_web_api/v1/channel/nosuch/info",
            500,
            "application/json",
            json!({"statusCode": "UNEXPECTED_ERROR", "statusMessage": "알 수 없는 에러", "errorMessage": "displayChannelId : nosuch 채널이 없습니다.", "result": null}).to_string(),
        ));
        fixture.exchanges.push(redirect(
            "https://naver.me/xAbCdEf1",
            "https://tv.naver.com/v/67838091",
        ));
        let resolver = NaverTvResolver::new(Http::replay(fixture));
        let resolve = |s: &str| {
            let url = Url::parse(s).unwrap();
            let resolver = &resolver;
            async move { resolver.resolve(&url).await }
        };
        let Resolution::Playlist(channel) = resolve("https://tv.naver.com/kbsnews").await.unwrap()
        else {
            panic!("a channel is a playlist");
        };
        assert_eq!(channel.title.as_deref(), Some("KBS뉴스"));
        assert_eq!(channel.entries.len(), 2);
        assert_eq!(channel.total, Some(2));
        assert_eq!(
            channel.entries[1].url.as_str(),
            "https://tv.naver.com/v/106049383"
        );
        assert_eq!(channel.entries[1].duration, Some(Duration::from_secs(179)));
        let Resolution::Playlist(playlists) = resolve("https://tv.naver.com/kbsnews/playlists")
            .await
            .unwrap()
        else {
            panic!("playlists are a playlist");
        };
        assert_eq!(playlists.title.as_deref(), Some("KBS뉴스 playlists"));
        assert_eq!(playlists.total, Some(19210));
        assert_eq!(
            playlists.entries[0].url.as_str(),
            "https://tv.naver.com/playlist/1047613"
        );
        let live = resolve("https://tv.naver.com/kbsnews/live")
            .await
            .unwrap_err();
        assert!(
            matches!(&live, ResolveError::Redirect(to) if to.as_str() == "https://tv.naver.com/l/184843"),
            "{live}"
        );
        let Resolution::Playlist(playlist) = resolve("https://tv.naver.com/playlist/188601")
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(playlist.id.as_deref(), Some("188601"));
        assert_eq!(playlist.title.as_deref(), Some("아침뉴스타임_20180205"));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://tv.naver.com/v/2660764"
        );
        let Resolution::Playlist(from_clip) =
            resolve("https://tv.naver.com/v/2660764?playlistNo=188601")
                .await
                .unwrap()
        else {
            panic!("a playlist named by a clip");
        };
        assert_eq!(from_clip.entries.len(), 1);
        assert!(matches!(
            resolve("https://tv.naver.com/nosuch").await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let short = resolve("https://naver.me/xAbCdEf1").await.unwrap_err();
        assert!(
            matches!(&short, ResolveError::Redirect(to) if to.as_str() == "https://tv.naver.com/v/67838091"),
            "{short}"
        );
    }

    /// Every example link resolves live: clips and the live to media with variants, the
    /// channel and playlists to entries.
    #[tokio::test]
    #[ignore = "requires live Naver TV access"]
    async fn live_examples_resolve() {
        let resolver = NaverTvResolver::new(Http::new(crate::http::HttpConfig::default()));
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
            let resolution = tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&url))
                .await
                .expect("resolution timed out")
                .unwrap_or_else(|e| panic!("{link}: {e}"));
            match resolution {
                Resolution::Media(resolved) => {
                    let playable = resolved.variants.iter().filter(|v| v.is_playable()).count();
                    assert!(playable > 0, "{link}: no playable variant");
                    println!(
                        "{link}: {:?} {playable} variants, live={} {:?}",
                        resolved.media, resolved.live, resolved.title
                    );
                }
                Resolution::Playlist(playlist) => {
                    assert!(!playlist.entries.is_empty(), "{link}: no entries");
                    println!(
                        "{link}: playlist {} entries (total {:?}) {:?}",
                        playlist.entries.len(),
                        playlist.total,
                        playlist.title
                    );
                }
            }
        }
    }
}
