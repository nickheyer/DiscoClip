//! Brightcove players, through the Playback API the player calls with the policy key its
//! script carries: every rendition (MP4 and other files, RTMP streams, the HLS and DASH
//! manifests expanded into their streams, Smooth Streaming), the text tracks and the
//! video's name, description, length and poster. A video locked with DRM is reported by
//! its key system. The older players (`c.brightcove.com`, `link.brightcove.com`,
//! `bcove.me`) name the publisher in their key and are read through the new player.
//! Players that only answer their own site are asked with that site as referer, which
//! the generic web resolver names when it finds the player on a page.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use jiff::Timestamp;
use regex::Regex;
use scraper::Selector;
use serde_json::Value;
use url::Url;

use super::page::Page;
use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, SubtitleFormat, SubtitleTrack, Variant, VariantKind, clean_title, essence,
    fetch, is_dash_type, is_hls_type, is_ism_type, manifests, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "brightcove";
const PLAYERS: &str = "https://players.brightcove.net/";
const PLAYBACK_API: &str = "https://edge.api.brightcove.com/playback/v1/accounts/";
const LEGACY_PLAYER: &str = "https://link.brightcove.com/services/player/bcpid";
const LEGACY_VIEWER: &str = "https://c.brightcove.com/services/viewer/htmlFederated";

/// `policyKey: "BCpk…"` or `"policyKey":"…"` in a player script.
static RE_POLICY_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"policyKey\\?"?\s*:\s*\\?["']([^"'\\]+)"#).unwrap());
/// `catalog({…})` in a player script, whose object names the policy key.
static RE_CATALOG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"catalog\((\{.+?\})\);").unwrap());
static RE_ACCOUNT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{6,}$").unwrap());
static RE_PLAYER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]+$").unwrap());
/// `/bcpid{player id}` in a legacy player link.
static RE_BCPID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/bcpid(\d+)").unwrap());
/// `<param name="playerKey" value="…">` on a legacy player page.
static RE_PARAM_PLAYER_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<param\s+name="playerKey"\s+value="([\w~,-]+)""#).unwrap());
/// A legacy video id: a number, a GUID, or a reference id.
static RE_LEGACY_VIDEO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:\d+|[\da-fA-F]{8}-?[\da-fA-F]{4}-?[\da-fA-F]{4}-?[\da-fA-F]{4}-?[\da-fA-F]{12}|ref:.+)$")
        .unwrap()
});
/// `<iframe src="//players.brightcove.net/{account}/{player}/index.html?…">`.
static RE_IFRAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<iframe[^>]+src=["']((?:https?:)?//players\.brightcove\.net/\d+/[^/"']+/index\.html[^"']*)["']"#)
        .unwrap()
});
/// `<video …>` or `<video-js …>` elements, whose attributes name the video.
static RE_VIDEO_TAG: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?is)<video(?:-js)?\s+[^>]*\bdata-video-id\s*=\s*[^>]*>").unwrap()
});
/// `<script src="//players.brightcove.net/{account}/{player}_{embed}/index.min.js">`.
static RE_PLAYER_SCRIPT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<script[^>]+src=["'](?:https?:)?//players\.brightcove\.net/(\d+)/([^/_"']+)_([^/"']+)/index(?:\.min)?\.js"#)
        .unwrap()
});
/// `<meta property="og:video" content="https://c.brightcove.com/…">`, the legacy player
/// in a page's metadata.
static RE_META_LEGACY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<meta\s+(?:property|itemprop)=["'](?:og:video|embedURL)["'][^>]+content=["'](https?://(?:secure|c)\.brightcove\.com/[^"']+)["']"#)
        .unwrap()
});
/// `<object class="BrightcoveExperience">…</object>` or an object whose movie is at
/// brightcove.com: the legacy player embedded in a page.
static RE_OBJECT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<object(?:[^>]+?class=["'][^>]*?BrightcoveExperience.*?["']|[^>]*?>\s*<param\s+name="movie"\s+value="https?://[^/]*brightcove\.com/).+?>\s*</object>"#)
        .unwrap()
});
static RE_PARAM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<param\s+name=["']([^"']+)["']\s+value=["']([^"']*)["']"#).unwrap()
});
/// `customBC.createVideo(width, height, "playerID", "playerKey", "videoID", …)`.
static RE_CREATE_VIDEO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)customBC\.createVideo\(.*?["'](\d+)["']\s*,\s*["'](AQ[^"']{48})[^"']*["']\s*,\s*["'](\d+)["']"#)
        .unwrap()
});
/// `<iframe src="//link.brightcove.com/services/player/…">`.
static RE_LEGACY_IFRAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<iframe[^>]+src=["']((?:https?:)?//link\.brightcove\.com/services/player/[^"']+)["']"#)
        .unwrap()
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Content {
    Video(String),
    Playlist(String),
}

/// A video in an account's player: `videoId` may be an id or `ref:<reference id>`. The
/// embed is the player's build (`default` for most); the referrer is the page the player
/// was found on, which players locked to their site are asked with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub account: String,
    pub player: String,
    pub embed: String,
    pub content: Content,
    pub referrer: Option<Url>,
}

impl Link {
    pub fn new(account: impl Into<String>, player: impl Into<String>, content: Content) -> Self {
        Self {
            account: account.into(),
            player: player.into(),
            embed: "default".to_string(),
            content,
            referrer: None,
        }
    }

    fn base(&self) -> String {
        format!("{PLAYERS}{}/{}_{}/", self.account, self.player, self.embed)
    }
}

/// A link to one of the older players, which names the video and, one way or another,
/// the publisher: outright, through the player's key, or through the player whose page
/// carries the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Legacy {
    pub video: String,
    pub publisher: Option<String>,
    pub player_key: Option<String>,
    pub player_id: Option<String>,
    pub referrer: Option<Url>,
}

/// A player embed link for the parts a page names.
pub fn embed_url(account: &str, player: &str, video: &str) -> Url {
    let mut url =
        Url::parse(&format!("{PLAYERS}{account}/{player}_default/index.html")).expect("valid");
    url.query_pairs_mut().append_pair("videoId", video);
    url
}

/// A player embed link with its build and the page it was found on.
fn player_url(
    account: &str,
    player: &str,
    embed: &str,
    key: &str,
    id: &str,
    referrer: Option<&Url>,
) -> Url {
    let mut url =
        Url::parse(&format!("{PLAYERS}{account}/{player}_{embed}/index.html")).expect("valid");
    url.query_pairs_mut().append_pair(key, id);
    if let Some(referrer) = referrer {
        url.query_pairs_mut()
            .append_pair("referrer", referrer.as_str());
    }
    url
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "players.brightcove.net" {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let (account, player_dir) = match segments.as_slice() {
        [account, player_dir, ..] if RE_ACCOUNT.is_match(account) => (account, player_dir),
        _ => return None,
    };
    let (player, embed) = match player_dir.rsplit_once('_') {
        Some((player, embed)) if !player.is_empty() && !embed.is_empty() => (player, embed),
        _ => (*player_dir, "default"),
    };
    if !RE_PLAYER.is_match(player) || !RE_PLAYER.is_match(embed) {
        return None;
    }
    let query = |key: &str| {
        url.query_pairs()
            .find(|(k, _)| k == key)
            .map(|(_, value)| value.into_owned())
            .filter(|s| !s.trim().is_empty())
    };
    let content = query("videoId")
        .map(Content::Video)
        .or_else(|| query("playlistId").map(Content::Playlist))?;
    Some(Link {
        account: account.to_string(),
        player: player.to_string(),
        embed: embed.to_string(),
        content,
        referrer: query("referrer").and_then(|r| Url::parse(&r).ok()),
    })
}

/// The older players' links: `c.brightcove.com/services/viewer/htmlFederated?…`,
/// `link.brightcove.com/services/player/bcpid…?bctid=…`, `bcove.me/…?bckey=…`, and the
/// `brightcove:` form other pages write them in.
pub fn parse_legacy(url: &Url) -> Option<Legacy> {
    let query_text = match url.scheme() {
        "brightcove" => url.as_str().trim_start_matches("brightcove:").to_string(),
        "http" | "https" => {
            let host = url.host_str()?.to_ascii_lowercase();
            if !(host.ends_with("brightcove.com")
                && (url.path().contains("/services/") || url.path().contains("/viewer")))
                && host != "bcove.me"
            {
                return None;
            }
            url.query().unwrap_or("").to_string()
        }
        _ => return None,
    };
    let pairs: Vec<(String, String)> = url::form_urlencoded::parse(query_text.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let param = |names: &[&str]| {
        pairs
            .iter()
            .find(|(k, _)| names.contains(&k.as_str()))
            .map(|(_, v)| v.trim().to_string())
            .filter(|v| !v.is_empty())
    };
    let video = param(&["@videoPlayer", "videoId", "videoID", "idVideo", "bctid"])?;
    if !RE_LEGACY_VIDEO.is_match(&video) {
        return None;
    }
    let player_id = param(&["playerID", "playerId"]).or_else(|| {
        RE_BCPID
            .captures(url.path())
            .map(|caps| caps[1].to_string())
    });
    Some(Legacy {
        video,
        publisher: param(&["publisherId"]).filter(|p| p.chars().all(|c| c.is_ascii_digit())),
        player_key: param(&["playerKey", "bckey"]),
        player_id,
        referrer: param(&["linkBaseURL"]).and_then(|r| Url::parse(&r).ok()),
    })
}

/// The publisher a legacy player key encodes: its second part, base64 with `~` for `=`,
/// is the publisher's number.
pub fn publisher_of_key(key: &str) -> Option<String> {
    let encoded = key.split(',').nth(1)?.replace('~', "=");
    let bytes = base64::engine::general_purpose::URL_SAFE
        .decode(encoded.as_bytes())
        .or_else(|_| base64::engine::general_purpose::STANDARD.decode(encoded.as_bytes()))
        .ok()?;
    let bytes: [u8; 8] = bytes.get(..8)?.try_into().ok()?;
    Some(u64::from_be_bytes(bytes).to_string())
}

/// The legacy viewer link for the parts an object or script names.
fn legacy_url(params: &[(&str, &str)]) -> Url {
    let mut url = Url::parse(LEGACY_VIEWER).expect("valid");
    url.query_pairs_mut().extend_pairs(params.iter().copied());
    url
}

/// Brightcove's players embedded in a page: iframes of the new player; `<video>` and
/// `<video-js>` elements with their account, player and embed (from their attributes or
/// the player script beside them); and the legacy players, in a page's metadata, as
/// objects, as `customBC.createVideo` calls, or as iframes. Each is handed on with the
/// page as its referrer, for players locked to their site.
pub fn embeds_in(page: &Page) -> Vec<Url> {
    let html = page.html();
    let mut found: Vec<Url> = Vec::new();
    let mut push = |url: Url| {
        if !found.contains(&url) {
            found.push(url);
        }
    };
    for caps in RE_IFRAME.captures_iter(html) {
        let src = util::html_unescape(&caps[1]);
        let Some(mut url) = util::join_url(Some(page.url()), &src) else {
            continue;
        };
        if parse_link(&url).is_some() {
            url.query_pairs_mut()
                .append_pair("referrer", page.url().as_str());
            push(url);
        }
    }
    let scripts: Vec<(usize, String, String, String)> = RE_PLAYER_SCRIPT
        .captures_iter(html)
        .map(|caps| {
            (
                caps.get(0).map(|m| m.start()).unwrap_or(0),
                caps[1].to_string(),
                caps[2].to_string(),
                caps[3].to_string(),
            )
        })
        .collect();
    for tag in RE_VIDEO_TAG.find_iter(html) {
        let attrs = util::extract_attributes(tag.as_str());
        let attr = |name: &str| {
            attrs
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let Some(video) = attr("data-video-id") else {
            continue;
        };
        // The player script that follows the element names what the attributes leave
        // out.
        let script = scripts.iter().find(|(at, ..)| *at > tag.start());
        let account = attr("data-account")
            .or_else(|| script.map(|(_, account, ..)| account.clone()))
            .filter(|a| RE_ACCOUNT.is_match(a));
        let Some(account) = account else {
            continue;
        };
        let player = attr("data-player")
            .or_else(|| script.map(|(_, _, player, _)| player.clone()))
            .unwrap_or_else(|| "default".to_string());
        let embed = attr("data-embed")
            .or_else(|| script.map(|(_, _, _, embed)| embed.clone()))
            .unwrap_or_else(|| "default".to_string());
        if !RE_PLAYER.is_match(&player) || !RE_PLAYER.is_match(&embed) {
            continue;
        }
        push(player_url(
            &account,
            &player,
            &embed,
            "videoId",
            &video,
            Some(page.url()),
        ));
    }
    let selector =
        Selector::parse("video[data-playlist-id], video-js[data-playlist-id]").expect("valid");
    for element in page.document().select(&selector) {
        let attrs = element.value();
        let (Some(account), Some(playlist)) = (
            attrs
                .attr("data-account")
                .filter(|s| RE_ACCOUNT.is_match(s)),
            attrs.attr("data-playlist-id").filter(|s| !s.is_empty()),
        ) else {
            continue;
        };
        let player = attrs.attr("data-player").unwrap_or("default");
        let embed = attrs.attr("data-embed").unwrap_or("default");
        if RE_PLAYER.is_match(player) && RE_PLAYER.is_match(embed) {
            push(player_url(
                account,
                player,
                embed,
                "playlistId",
                playlist,
                Some(page.url()),
            ));
        }
    }
    for caps in RE_META_LEGACY.captures_iter(html) {
        let content = util::html_unescape(&caps[1]);
        if ["playerKey", "videoId", "idVideo"]
            .iter()
            .any(|k| content.contains(k))
            && let Ok(url) = Url::parse(&content)
            && parse_legacy(&url).is_some()
        {
            push(url);
        }
    }
    for object in RE_OBJECT.find_iter(html) {
        let params: Vec<(String, String)> = RE_PARAM
            .captures_iter(object.as_str())
            .map(|c| (c[1].to_string(), util::html_unescape(&c[2])))
            .collect();
        let param = |names: &[&str]| {
            params
                .iter()
                .find(|(k, _)| names.contains(&k.as_str()))
                .map(|(_, v)| v.trim().to_string())
                .filter(|v| !v.is_empty())
        };
        let Some(player_id) = param(&["playerID", "playerId"]) else {
            continue;
        };
        let Some(video) = param(&["@videoPlayer", "videoId", "videoID", "@videoList"]) else {
            continue;
        };
        if !RE_LEGACY_VIDEO.is_match(&video) {
            continue;
        }
        let mut query: Vec<(&str, &str)> = vec![("playerID", &player_id), ("@videoPlayer", &video)];
        let key = param(&["playerKey"]);
        if let Some(key) = &key {
            query.push(("playerKey", key));
        }
        let link_base = param(&["linkBaseURL"]);
        if let Some(base) = &link_base {
            query.push(("linkBaseURL", base));
        }
        push(legacy_url(&query));
    }
    for caps in RE_CREATE_VIDEO.captures_iter(html) {
        push(legacy_url(&[
            ("playerID", &caps[1]),
            ("playerKey", &caps[2]),
            ("@videoPlayer", &caps[3]),
        ]));
    }
    for caps in RE_LEGACY_IFRAME.captures_iter(html) {
        if let Some(url) = util::join_url(Some(page.url()), &util::html_unescape(&caps[1]))
            && parse_legacy(&url).is_some()
        {
            push(url);
        }
    }
    found
}

fn drm_system(source: &Value) -> Option<String> {
    let systems = source["key_systems"].as_object()?;
    systems.keys().next().map(|k| match k.as_str() {
        "com.widevine.alpha" => "widevine".to_string(),
        "com.microsoft.playready" => "playready".to_string(),
        "com.apple.fps.1_0" | "com.apple.fps" => "fairplay".to_string(),
        other => other.to_string(),
    })
}

fn container_of(ext: &str, container: Option<&str>) -> Option<Container> {
    let name = if ext.is_empty() {
        container.unwrap_or("").to_ascii_lowercase()
    } else {
        ext.to_string()
    };
    match name.as_str() {
        "" => None,
        "mp4" | "m4v" => Some(Container::Mp4),
        "webm" => Some(Container::Webm),
        "flv" => Some(Container::Flv),
        "mov" => Some(Container::Mov),
        other => {
            Container::from_extension(other).or_else(|| Some(Container::Other(other.to_string())))
        }
    }
}

/// The video's sources as variants: the HLS, DASH and Smooth Streaming manifests, every
/// file (MP4 and otherwise, audio alone when the source has no picture), and RTMP
/// streams. The plain HTTP copy of an HTTPS source is left out.
pub fn variants_of(video: &Value) -> Vec<Variant> {
    let duration = video["duration"]
        .as_u64()
        .filter(|d| *d > 0)
        .map(Duration::from_millis);
    let sources: Vec<&Value> = video["sources"].as_array().into_iter().flatten().collect();
    let mut variants: Vec<Variant> = Vec::new();
    for source in &sources {
        let mime = essence(source["type"].as_str());
        let container = source["container"].as_str().map(|c| c.to_ascii_uppercase());
        let src = source["src"]
            .as_str()
            .or(source["streaming_src"].as_str())
            .and_then(|u| Url::parse(u).ok());
        let ext = src
            .as_ref()
            .and_then(super::path_extension)
            .unwrap_or_default();
        let kind = if is_hls_type(&mime) || ext == "m3u8" || container.as_deref() == Some("M2TS") {
            VariantKind::Hls
        } else if is_dash_type(&mime) || ext == "mpd" {
            VariantKind::Dash
        } else if (is_ism_type(&mime) && mime != "text/xml" && mime != "application/xml")
            || ext == "ism"
            || ext == "isml"
        {
            VariantKind::Ism
        } else if src.is_some() {
            VariantKind::File
        } else {
            VariantKind::Rtmp
        };
        let url = match (&src, kind) {
            (Some(url), VariantKind::Rtmp) => url.clone(),
            (Some(url), _) => url.clone(),
            (None, _) => {
                let (Some(app), Some(stream)) =
                    (source["app_name"].as_str(), source["stream_name"].as_str())
                else {
                    continue;
                };
                match Url::parse(&format!("{}/{}", app.trim_end_matches('/'), stream)) {
                    Ok(url) => url,
                    Err(_) => continue,
                }
            }
        };
        if url.scheme() == "http"
            && sources.iter().any(|other| {
                other["src"].as_str().is_some_and(|o| {
                    o.strip_prefix("https://") == url.as_str().strip_prefix("http://")
                })
            })
        {
            continue;
        }
        if variants.iter().any(|v| v.url == url) {
            continue;
        }
        let mut v = Variant::new(url, kind);
        let width = source["width"].as_u64().map(|w| w as u32);
        let height = source["height"].as_u64().map(|h| h as u32);
        let bitrate = source["avg_bitrate"].as_u64().filter(|b| *b > 0);
        match kind {
            VariantKind::File | VariantKind::Rtmp => {
                v.container = container_of(
                    if kind == VariantKind::Rtmp {
                        "flv"
                    } else {
                        &ext
                    },
                    container.as_deref(),
                );
                if width == Some(0) && height == Some(0) {
                    v.audio_only = true;
                    v.audio = Some(AudioCodec::Aac);
                } else {
                    v.video = Some(
                        match source["codec"]
                            .as_str()
                            .map(|c| c.to_ascii_uppercase())
                            .as_deref()
                        {
                            Some("H265") | Some("HEVC") => VideoCodec::H265,
                            Some("VP9") => VideoCodec::Vp9,
                            Some("AV1") => VideoCodec::Av1,
                            _ => VideoCodec::H264,
                        },
                    );
                    v.audio = Some(AudioCodec::Aac);
                    v.width = width.filter(|w| *w > 0);
                    v.height = height.filter(|h| *h > 0);
                }
                v.bitrate = bitrate;
                v.size = source["size"].as_u64().filter(|s| *s > 0);
                let prefix = match (kind, source["src"].is_string()) {
                    (VariantKind::Rtmp, _) => "rtmp",
                    (_, true) => "http",
                    (_, false) => "http-streaming",
                };
                let mut id = prefix.to_string();
                if let Some(b) = bitrate {
                    id.push_str(&format!("-{}k", b / 1000));
                }
                if let Some(h) = v.height {
                    id.push_str(&format!("-{h}p"));
                }
                v.format_id = Some(id);
                v.label = v
                    .height
                    .map(|h| format!("{h}p"))
                    .or_else(|| v.audio_only.then(|| "audio".to_string()));
            }
            _ => {
                v.format_id = Some(match source["ext_x_version"].as_str() {
                    Some(x) => format!("{}-v{x}", kind.as_str()),
                    None => kind.as_str().to_string(),
                });
                v.width = width.filter(|w| *w > 0);
                v.height = height.filter(|h| *h > 0);
                v.bitrate = bitrate;
            }
        }
        v.duration = duration;
        v.drm = drm_system(source).or_else(|| {
            if container.as_deref() == Some("WVM") {
                Some("widevine".to_string())
            } else if kind == VariantKind::Ism {
                Some("playready".to_string())
            } else {
                None
            }
        });
        variants.push(v);
    }
    variants
}

pub fn subtitles_of(video: &Value) -> Vec<SubtitleTrack> {
    let mut tracks = Vec::new();
    for track in video["text_tracks"].as_array().into_iter().flatten() {
        if !matches!(track["kind"].as_str(), Some("captions") | Some("subtitles")) {
            continue;
        }
        let Some(url) = track["src"].as_str().and_then(|u| Url::parse(u).ok()) else {
            continue;
        };
        if tracks.iter().any(|t: &SubtitleTrack| t.url == url) {
            continue;
        }
        let mime = essence(track["mime_type"].as_str());
        tracks.push(SubtitleTrack {
            url,
            language: track["srclang"]
                .as_str()
                .or(track["label"].as_str())
                .unwrap_or("en")
                .to_ascii_lowercase(),
            name: track["label"].as_str().map(String::from),
            format: if mime.contains("ttml") || mime.contains("xml") {
                SubtitleFormat::Ttml
            } else {
                SubtitleFormat::Vtt
            },
            auto: false,
            headers: Vec::new(),
        });
    }
    tracks
}

pub struct BrightcoveResolver {
    http: Http,
}

impl BrightcoveResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// The publisher a legacy link's video belongs to: named outright, encoded in the
    /// player's key, or in the key on the player's page.
    async fn legacy_publisher(
        &self,
        legacy: &Legacy,
        origin: &Url,
    ) -> Result<String, ResolveError> {
        if let Some(publisher) = &legacy.publisher {
            return Ok(publisher.clone());
        }
        if let Some(publisher) = legacy.player_key.as_deref().and_then(publisher_of_key) {
            return Ok(publisher);
        }
        let Some(player_id) = legacy
            .player_id
            .as_deref()
            .filter(|p| p.chars().all(|c| c.is_ascii_digit()))
        else {
            return Err(ResolveError::unavailable(
                origin,
                "the legacy player link names neither a publisher nor a player",
            ));
        };
        let page = Url::parse(&format!("{LEGACY_PLAYER}{player_id}")).expect("valid");
        let mut headers = Vec::new();
        if let Some(referrer) = &legacy.referrer {
            headers.push(("referer".to_string(), referrer.to_string()));
        }
        let fetched = fetch(&self.http, &page, PLATFORM, BROWSER_UA, &headers, MAX_PAGE).await?;
        if !fetched.status.is_success() {
            return Err(ResolveError::unavailable(
                origin,
                format!("the legacy player page answered HTTP {}", fetched.status),
            ));
        }
        util::search(&RE_PARAM_PLAYER_KEY, &fetched.text())
            .as_deref()
            .and_then(publisher_of_key)
            .ok_or_else(|| {
                ResolveError::unavailable(origin, "the legacy player page names no publisher")
            })
    }
}

/// The policy key a player is built with, read as `platform`: from the player's
/// configuration, else from its script, where the catalog call or a `policyKey` names it.
pub async fn policy_key(
    http: &Http,
    platform: &str,
    link: &Link,
    origin: &Url,
) -> Result<String, ResolveError> {
    let base = link.base();
    let config = Url::parse(&format!("{base}config.json")).expect("valid");
    if let Ok(fetched) = fetch(http, &config, platform, BROWSER_UA, &[], MAX_PAGE).await
        && fetched.status.is_success()
        && let Ok(config) = fetched.json(origin)
        && let Some(key) = config["video_cloud"]["policy_key"]
            .as_str()
            .filter(|k| !k.is_empty())
    {
        return Ok(key.to_string());
    }
    let script = Url::parse(&format!("{base}index.min.js")).expect("valid");
    let fetched = fetch(http, &script, platform, BROWSER_UA, &[], MAX_PAGE).await?;
    match fetched.status.as_u16() {
        200..=299 => {}
        404 => {
            return Err(ResolveError::unavailable(
                origin,
                format!(
                    "the account has no player called {}_{}",
                    link.player, link.embed
                ),
            ));
        }
        status => {
            return Err(ResolveError::unavailable(
                origin,
                format!("the player script answered HTTP {status}"),
            ));
        }
    }
    let text = fetched.text();
    if let Some(caps) = RE_CATALOG.captures(&text)
        && let Some(catalog) = util::parse_js(&caps[1])
        && let Some(key) = catalog["policyKey"].as_str().filter(|k| !k.is_empty())
    {
        return Ok(key.to_string());
    }
    RE_POLICY_KEY
        .captures(&text)
        .map(|c| c[1].to_string())
        .ok_or_else(|| ResolveError::unavailable(origin, "the player script carries no policy key"))
}

/// The Playback API's answer for `link`'s content, read as `platform` with the player's
/// policy `key` and, for players locked to their site, the page as referer; its refusals
/// become the resolver errors they mean for `origin`. A key the API no longer accepts is
/// read again from the player once.
async fn playback(
    http: &Http,
    platform: &str,
    link: &Link,
    key: &str,
    origin: &Url,
) -> Result<Value, ResolveError> {
    let (resource, id) = match &link.content {
        Content::Video(id) => ("videos", id),
        Content::Playlist(id) => ("playlists", id),
    };
    let mut api =
        Url::parse(&format!("{PLAYBACK_API}{}/{resource}/", link.account)).expect("valid");
    api.path_segments_mut()
        .expect("base URL")
        .pop_if_empty()
        .push(id);
    let mut key = key.to_string();
    for attempt in 0..2 {
        let mut headers = vec![("accept".to_string(), format!("application/json;pk={key}"))];
        if let Some(referrer) = &link.referrer {
            headers.push(("referer".to_string(), referrer.to_string()));
            if let Some(origin_of) = referrer.host_str() {
                headers.push((
                    "origin".to_string(),
                    format!("{}://{origin_of}", referrer.scheme()),
                ));
            }
        }
        let fetched = fetch(http, &api, platform, BROWSER_UA, &headers, MAX_PAGE).await?;
        let body = fetched.json(origin).unwrap_or(Value::Null);
        match fetched.status.as_u16() {
            200..=299 => return Ok(body),
            404 => return Err(ResolveError::NotFound(origin.clone())),
            401 | 403 => {
                let error = if body.is_array() { &body[0] } else { &body };
                let code = error["error_code"].as_str().unwrap_or("ACCESS_DENIED");
                let subcode = error["error_subcode"].as_str().unwrap_or("");
                let message = error["message"]
                    .as_str()
                    .filter(|m| !m.trim().is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{code} {subcode}").trim().to_string());
                if subcode == "CLIENT_GEO" {
                    return Err(ResolveError::unavailable(
                        origin,
                        format!("it is not available in this region: {message}"),
                    ));
                }
                if code == "INVALID_POLICY_KEY" && attempt == 0 {
                    key = policy_key(http, platform, link, origin).await?;
                    continue;
                }
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the playback API refused the video: {message}"),
                ));
            }
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the playback API answered HTTP {status}"),
                ));
            }
        }
    }
    Err(ResolveError::unavailable(
        origin,
        "the playback API refused the player's key twice",
    ))
}

/// The video `link` names, read as `platform` from the Playback API with the player's
/// policy key: its name, description, length, poster and text tracks, every rendition,
/// and the streams of its HLS and DASH manifests. The result is attributed to
/// `platform`, with `origin` as its page.
pub async fn media(
    http: &Http,
    platform: &str,
    link: &Link,
    origin: &Url,
) -> Result<Resolved, ResolveError> {
    let key = policy_key(http, platform, link, origin).await?;
    let body = playback(http, platform, link, &key, origin).await?;
    let variants = variants_of(&body);
    if variants.is_empty() {
        let error = &body["errors"][0];
        if error["error_subcode"].as_str() == Some("TVE_AUTH") {
            return Err(ResolveError::login_required(
                origin,
                "brightcove",
                "the video is for TV subscribers, signed in through their provider",
            ));
        }
        let message = error["message"]
            .as_str()
            .or(error["error_subcode"].as_str())
            .or(error["error_code"].as_str())
            .unwrap_or("the video has no playable sources");
        return Err(ResolveError::unavailable(origin, message.to_string()));
    }
    let mut resolved = Resolved::new(platform);
    resolved.id = body["id"].as_str().map(String::from);
    resolved.title = body["name"].as_str().and_then(clean_title);
    resolved.description = body["long_description"]
        .as_str()
        .or(body["description"].as_str())
        .and_then(clean_title);
    resolved.uploaded_at = body["published_at"]
        .as_str()
        .or(body["created_at"].as_str())
        .and_then(|t| t.parse::<Timestamp>().ok());
    let duration_ms = body["duration"].as_i64();
    resolved.duration = duration_ms
        .filter(|d| *d > 0)
        .map(|d| Duration::from_millis(d as u64));
    // A video without a length is a live stream.
    resolved.live = duration_ms.is_some_and(|d| d <= 0);
    resolved.thumbnail = body["poster"]
        .as_str()
        .or(body["thumbnail"].as_str())
        .and_then(|u| Url::parse(u).ok());
    resolved.webpage_url = Some(origin.clone());
    resolved.subtitles = subtitles_of(&body);
    let duration = resolved.duration;
    let mut variants =
        manifests::expand_all(http, platform, variants, &mut resolved.subtitles, duration).await;
    if resolved.live {
        for v in &mut variants {
            v.live = true;
        }
    }
    resolved.variants = variants;
    Ok(resolved)
}

#[async_trait]
impl Resolver for BrightcoveResolver {
    fn embeds_in(&self, page: &Page) -> Vec<Url> {
        embeds_in(page)
    }

    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Brightcove",
            hosts: &[
                "players.brightcove.net",
                "c.brightcove.com",
                "link.brightcove.com",
                "bcove.me",
            ],
            features: &[
                "player embeds",
                "legacy players",
                "reference ids",
                "playlists",
                "text tracks",
                "live",
                "drm reported",
            ],
            formats: &["mp4", "hls", "dash", "ism", "rtmp"],
            session: SessionSupport::None,
            examples: &[
                "https://players.brightcove.net/1752604059001/default_default/index.html?videoId=4457254747001",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some() || parse_legacy(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = match parse_link(url) {
            Some(link) => link,
            None => {
                let legacy =
                    parse_legacy(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
                let publisher = self.legacy_publisher(&legacy, url).await?;
                let mut link =
                    Link::new(publisher, "default", Content::Video(legacy.video.clone()));
                link.referrer = legacy.referrer.clone();
                link
            }
        };
        match &link.content {
            Content::Video(_) => Ok(Resolution::from(
                media(&self.http, PLATFORM, &link, url).await?,
            )),
            Content::Playlist(id) => {
                let key = policy_key(&self.http, PLATFORM, &link, url).await?;
                let body = playback(&self.http, PLATFORM, &link, &key, url).await?;
                let entries: Vec<PlaylistEntry> = body["videos"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|video| {
                        Some(PlaylistEntry {
                            url: player_url(
                                &link.account,
                                &link.player,
                                &link.embed,
                                "videoId",
                                video["id"].as_str()?,
                                link.referrer.as_ref(),
                            ),
                            title: video["name"].as_str().and_then(clean_title),
                            duration: video["duration"]
                                .as_u64()
                                .filter(|d| *d > 0)
                                .map(Duration::from_millis),
                        })
                    })
                    .collect();
                if entries.is_empty() {
                    return Err(ResolveError::NotFound(url.clone()));
                }
                Ok(Resolution::Playlist(Playlist {
                    resolver: PLATFORM.into(),
                    id: Some(id.clone()),
                    title: body["name"].as_str().and_then(clean_title),
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

    const SCRIPT: &str = r#"!function(){var e={accountId:"1752604059001",policyKey:"BCpkADawqM3NCJTTAQ6n2c1-NDEqVU_WLKw1tLj45NwgjsWncRQbJcWW",playerId:"default"};}();"#;
    const VIDEO: &str = r#"{"id":"4457254747001","name":"Sea Marvels Collection","description":null,"long_description":"Marvels of the sea.","duration":155574,"published_at":"2015-09-01T16:25:20.060Z","account_id":"1752604059001","poster":"https://cf-images.eu-west-1.prod.boltdns.net/x/640x360/match/image.jpg","thumbnail":"https://cf-images.eu-west-1.prod.boltdns.net/x/160x90/match/image.jpg","sources":[{"src":"http://manifest.prod.boltdns.net/manifest/v1/hls/v4/clear/1752604059001/x/10s/master.m3u8?fastly_token=a","type":"application/x-mpegURL","ext_x_version":"4"},{"src":"https://manifest.prod.boltdns.net/manifest/v1/hls/v4/clear/1752604059001/x/10s/master.m3u8?fastly_token=a","type":"application/x-mpegURL","ext_x_version":"4"},{"src":"https://manifest.prod.boltdns.net/manifest/v2/hls/v7/clear/avc1_mp4a/1752604059001/x/10s/master.m3u8?fastly_token=b","type":"application/x-mpegURL","ext_x_version":"7"},{"src":"https://manifest.prod.boltdns.net/manifest/v1/dash/live-baseurl/clear/1752604059001/x/2s/manifest.mpd?fastly_token=c","type":"application/dash+xml"},{"src":"http://fastly-signed-eu-west-1-prod.brightcovecdn.com/media/v1/pmp4/static/clear/1752604059001/x/main.mp4?fastly_token=d","container":"MP4","codec":"H264","height":360,"width":640,"avg_bitrate":2007000,"size":39116979},{"src":"https://fastly-signed-eu-west-1-prod.brightcovecdn.com/media/v1/pmp4/static/clear/1752604059001/x/main.mp4?fastly_token=d","container":"MP4","codec":"H264","height":360,"width":640,"avg_bitrate":2007000,"size":39116979}],"text_tracks":[{"src":"https://manifest.prod.boltdns.net/thumbnail/v1/x/thumbnail.webvtt?fastly_token=e","kind":"metadata","label":"thumbnails","mime_type":"text/webvtt"},{"src":"https://manifest.prod.boltdns.net/captions/x/en.vtt?fastly_token=f","kind":"captions","srclang":"en","label":"English","mime_type":"text/vtt","default":true}]}"#;

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link(
                "https://players.brightcove.net/1752604059001/default_default/index.html?videoId=4457254747001"
            ),
            Some(Link::new(
                "1752604059001",
                "default",
                Content::Video("4457254747001".into())
            ))
        );
        assert_eq!(
            link("https://players.brightcove.net/929656772001/e41d32dc-ec74-459e-a845-6c69f7b724ea_default/index.html?videoId=ref:myref").unwrap().content,
            Content::Video("ref:myref".into())
        );
        assert_eq!(
            link("https://players.brightcove.net/1752604059001/default_default/index.html"),
            None
        );
        assert_eq!(link("https://players.brightcove.net/"), None);
        assert_eq!(
            embed_url("1752604059001", "default", "4457254747001").as_str(),
            "https://players.brightcove.net/1752604059001/default_default/index.html?videoId=4457254747001"
        );
    }

    #[tokio::test]
    async fn videos_resolve_through_the_playback_api_with_the_players_policy_key() {
        let mut fixture = Fixture::new("brightcove", None);
        fixture.exchanges.push(get(
            "https://players.brightcove.net/1752604059001/default_default/index.min.js",
            200,
            "application/javascript",
            SCRIPT,
        ));
        fixture.exchanges.push(get("https://edge.api.brightcove.com/playback/v1/accounts/1752604059001/videos/4457254747001", 200, "application/json", VIDEO));
        fixture.exchanges.push(get(
            "https://edge.api.brightcove.com/playback/v1/accounts/1752604059001/videos/1",
            404,
            "application/json",
            r#"[{"error_code":"VIDEO_NOT_FOUND"}]"#,
        ));
        fixture.exchanges.push(get("https://edge.api.brightcove.com/playback/v1/accounts/1752604059001/videos/2", 403, "application/json", r#"[{"error_code":"ACCESS_DENIED","error_subcode":"CLIENT_GEO","message":"Access to this resource is forbidden by access policy."}]"#));
        let resolver = BrightcoveResolver::new(Http::replay(fixture));
        let url = Url::parse("https://players.brightcove.net/1752604059001/default_default/index.html?videoId=4457254747001").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.title.as_deref(), Some("Sea Marvels Collection"));
        assert_eq!(resolved.description.as_deref(), Some("Marvels of the sea."));
        assert_eq!(resolved.duration, Some(Duration::from_millis(155574)));
        assert!(resolved.uploaded_at.is_some());
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
        assert!(resolved.variants.iter().all(|v| v.url.scheme() == "https"));
        let mp4 = resolved
            .variants
            .iter()
            .find(|v| v.kind == VariantKind::File)
            .unwrap();
        assert_eq!(mp4.size, Some(39116979));
        assert_eq!(mp4.height, Some(360));
        assert_eq!(mp4.bitrate, Some(2007000));
        assert!(
            resolved
                .variants
                .iter()
                .any(|v| v.kind == VariantKind::Dash)
        );
        assert_eq!(resolved.subtitles.len(), 1);
        assert_eq!(resolved.subtitles[0].language, "en");
        assert!(matches!(
            resolver.resolve(&Url::parse("https://players.brightcove.net/1752604059001/default_default/index.html?videoId=1").unwrap()).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver.resolve(&Url::parse("https://players.brightcove.net/1752604059001/default_default/index.html?videoId=2").unwrap()).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("region")),
            "{error}"
        );
    }

    #[test]
    fn drm_locked_sources_carry_their_key_system() {
        let locked: Value = serde_json::from_str(r#"{"sources":[{"src":"https://m.test/x.mpd","type":"application/dash+xml","key_systems":{"com.widevine.alpha":{"license_url":"https://l"}}}]}"#).unwrap();
        let variants = variants_of(&locked);
        assert_eq!(variants[0].drm.as_deref(), Some("widevine"));
        assert!(!variants[0].is_playable());
    }

    #[tokio::test]
    async fn recorded_videos_and_playlists_resolve() {
        let fixture = Fixture::parse(include_str!("brightcove_fixture.json")).unwrap();
        let resolver = BrightcoveResolver::new(Http::replay(fixture));
        let url = embed_url("1752604059001", "default", "4457254747001");
        let video = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(video.title.as_deref(), Some("Sea Marvels Collection"));
        assert!(
            video
                .variants
                .iter()
                .any(|v| v.kind == VariantKind::File && v.size == Some(39116979))
        );
        assert!(video.variants.iter().any(|v| v.kind == VariantKind::Hls));
        assert!(video.variants.iter().any(|v| v.kind == VariantKind::Dash));
        let playlist_url = Url::parse("https://players.brightcove.net/1752604059001/default_default/index.html?playlistId=5718313430001").unwrap();
        let Resolution::Playlist(playlist) = resolver.resolve(&playlist_url).await.unwrap() else {
            panic!("expected a playlist");
        };
        assert!(!playlist.entries.is_empty());
        assert!(
            playlist
                .entries
                .iter()
                .all(|entry| parse_link(&entry.url).is_some())
        );
        let missing = Url::parse(
            "https://players.brightcove.net/1752604059001/default_default/index.html?videoId=1",
        )
        .unwrap();
        assert!(matches!(
            resolver.resolve(&missing).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[test]
    fn legacy_links_name_their_video_and_publisher() {
        let legacy = |s: &str| parse_legacy(&Url::parse(s).unwrap());
        let by_key = legacy("http://link.brightcove.com/services/player/bcpid756015033001?bckey=AQ~~,AAAApYJi_Ck~,GxhXCegT1Dp39ilhXuxMJxasUhVNZiil&bctid=2878862109001").unwrap();
        assert_eq!(by_key.video, "2878862109001");
        assert_eq!(by_key.player_id.as_deref(), Some("756015033001"));
        assert_eq!(
            publisher_of_key(by_key.player_key.as_deref().unwrap()).as_deref(),
            Some("710857129001")
        );
        let by_player = legacy("http://c.brightcove.com/services/viewer/htmlFederated?playerID=1654948606001&flashID=myExperience&%40videoPlayer=2371591881001").unwrap();
        assert_eq!(by_player.video, "2371591881001");
        assert_eq!(by_player.player_id.as_deref(), Some("1654948606001"));
        assert_eq!(by_player.player_key, None);
        let by_reference = legacy("http://c.brightcove.com/services/viewer/htmlFederated?%40videoPlayer=ref%3Aevent-stream-356&linkBaseURL=http%3A%2F%2Fwww.redbull.com%2Fen%2Fbike%2Fvideos%2F1331655630249%2Freplay&playerKey=AQ%7E%7E%2CAAAApYJ7UqE%7E%2Cxqr_zXk0I-zzNndy8NlHogrCb5QdyZRf&playerID=1398061561001").unwrap();
        assert_eq!(by_reference.video, "ref:event-stream-356");
        assert_eq!(
            by_reference.referrer.as_ref().unwrap().host_str(),
            Some("www.redbull.com")
        );
        assert_eq!(
            publisher_of_key(by_reference.player_key.as_deref().unwrap()).as_deref(),
            Some("710858724001")
        );
        let by_publisher = legacy("http://c.brightcove.com/services/viewer/federated_f9?&playerID=1265504713001&publisherID=AQ%7E%7E%2CAAABBzUwv1E%7E%2CxP-xFHVUstiMFlNYfvF4G9yFnNaqCw_9&videoID=2750934548001").unwrap();
        assert_eq!(by_publisher.video, "2750934548001");
        assert_eq!(
            publisher_of_key("AQ~~,AAABmA9XpXk~,-Kp7jNgisre1fG5OdqpAFUTcs0lP_ZoL").as_deref(),
            Some("1752604059001")
        );
        assert!(legacy("http://c.brightcove.com/services/viewer/htmlFederated?playerID=3550052898001&playerKey=AQ%7E%7E%2CAAABmA9XpXk%7E%2C-Kp7jNgisre1fG5OdqpAFUTcs0lP_ZoL").is_none(), "no video named");
        assert!(legacy("https://example.com/services/viewer?videoId=1").is_none());
        let embed = parse_link(&Url::parse("https://players.brightcove.net/929656772001/abc_myembed/index.html?videoId=1&referrer=https%3A%2F%2Fexample.com%2Fpage").unwrap()).unwrap();
        assert_eq!(embed.player, "abc");
        assert_eq!(embed.embed, "myembed");
        assert_eq!(
            embed.referrer.as_ref().unwrap().as_str(),
            "https://example.com/page"
        );
    }

    #[test]
    fn every_embed_shape_is_found_with_the_page_as_referrer() {
        let html = r#"<html><body>
            <iframe src="//players.brightcove.net/1752604059001/default_default/index.html?videoId=4457254747001"></iframe>
            <video-js data-video-id="5636927002001" data-account="3910869709001" data-player="default" data-embed="default"></video-js>
            <video data-video-id="4463156585001"></video>
            <script src="//players.brightcove.net/4463156585001/rJ7KKq2c_default/index.min.js"></script>
            <video data-video-id="ignored"></video>
            <object class="BrightcoveExperience"><param name="playerID" value="1654948606001"><param name="@videoPlayer" value="2371591881001"><param name="playerKey" value="AQ~~,AAAApYJi_Ck~,GxhX"></object>
            <script>customBC.createVideo(640, 360, "3550052898001", "AQ~~,AAABmA9XpXk~,-Kp7jNgisre1fG5OdqpAFUTcs0lP_ZoL", "4457254747001", "");</script>
            <meta property="og:video" content="https://c.brightcove.com/services/viewer/htmlFederated?playerID=1&videoId=2">
            <iframe src="//link.brightcove.com/services/player/bcpid756015033001?bctid=2878862109001"></iframe>
        </body></html>"#;
        let page = Page::parse(html, &Url::parse("https://example.com/story").unwrap());
        let found: Vec<String> = embeds_in(&page).iter().map(|u| u.to_string()).collect();
        assert_eq!(found.len(), 7, "{found:?}");
        let has = |needle: &str| found.iter().any(|u| u.contains(needle));
        assert!(
            has(
                "https://players.brightcove.net/1752604059001/default_default/index.html?videoId=4457254747001&referrer="
            ),
            "{found:?}"
        );
        assert!(
            has(
                "https://players.brightcove.net/3910869709001/default_default/index.html?videoId=5636927002001&referrer="
            ),
            "{found:?}"
        );
        assert!(
            has(
                "https://players.brightcove.net/4463156585001/rJ7KKq2c_default/index.html?videoId=4463156585001&referrer="
            ),
            "{found:?}"
        );
        assert!(
            has("htmlFederated?playerID=1654948606001&%40videoPlayer=2371591881001&playerKey="),
            "{found:?}"
        );
        assert!(
            has("playerID=3550052898001&playerKey=AQ") && has("%40videoPlayer=4457254747001"),
            "{found:?}"
        );
        assert!(
            has("https://c.brightcove.com/services/viewer/htmlFederated?playerID=1&"),
            "{found:?}"
        );
        assert!(
            has(
                "https://link.brightcove.com/services/player/bcpid756015033001?bctid=2878862109001"
            ),
            "{found:?}"
        );
        assert!(
            found
                .iter()
                .all(|u| parse_link(&Url::parse(u).unwrap()).is_some()
                    || parse_legacy(&Url::parse(u).unwrap()).is_some())
        );
    }

    #[test]
    fn every_source_kind_becomes_a_variant() {
        let video = serde_json::json!({"duration": 0, "sources": [
            {"src": "https://h/a.m3u8", "type": "application/x-mpegURL", "ext_x_version": "4"},
            {"src": "https://h/a.mpd", "type": "application/dash+xml"},
            {"src": "https://h/a.ism/manifest", "type": "application/vnd.ms-sstr+xml"},
            {"src": "http://h/b.mp4", "container": "MP4", "width": 640, "height": 360, "avg_bitrate": 500000, "codec": "H264"},
            {"src": "https://h/b.mp4", "container": "MP4", "width": 640, "height": 360, "avg_bitrate": 500000, "codec": "H264"},
            {"streaming_src": "https://h/stream.mp4", "container": "MP4", "width": 1280, "height": 720},
            {"src": "https://h/audio.m4a", "container": "M4A", "width": 0, "height": 0, "avg_bitrate": 128000},
            {"src": "https://h/c.webm", "container": "WEBM", "width": 1920, "height": 1080, "codec": "VP9"},
            {"app_name": "rtmp://h/app", "stream_name": "mp4:c.mp4", "container": "MP4", "width": 854, "height": 480},
            {"src": "https://h/d.wvm", "container": "WVM", "width": 640, "height": 360}
        ]});
        let variants = variants_of(&video);
        let ids: Vec<&str> = variants
            .iter()
            .filter_map(|v| v.format_id.as_deref())
            .collect();
        assert_eq!(
            ids,
            vec![
                "hls-v4",
                "dash",
                "ism",
                "http-500k-360p",
                "http-streaming-720p",
                "http-128k",
                "http-1080p",
                "rtmp-480p",
                "http-360p"
            ]
        );
        assert_eq!(variants[2].kind, VariantKind::Ism);
        assert_eq!(variants[2].drm.as_deref(), Some("playready"));
        assert_eq!(
            variants[3].url.as_str(),
            "https://h/b.mp4",
            "the HTTPS copy stands for both"
        );
        assert!(variants[5].audio_only);
        assert_eq!(variants[6].video, Some(VideoCodec::Vp9));
        assert_eq!(variants[6].container, Some(Container::Webm));
        assert_eq!(variants[7].kind, VariantKind::Rtmp);
        assert_eq!(variants[7].url.as_str(), "rtmp://h/app/mp4:c.mp4");
        assert_eq!(variants[8].drm.as_deref(), Some("widevine"));
    }
}
