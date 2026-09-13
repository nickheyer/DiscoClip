//! Bilibili videos, multi-part videos, bangumi episodes and seasons, favourites,
//! collections, series and user spaces, through the web API the site itself uses: its
//! requests carry the device cookie the API hands out and the WBI signature it checks,
//! and `b23.tv` short links are unwrapped first. Videos locked to a region or to paying
//! members are reported as such.

use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use md5::{Digest, Md5};
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    ClipRange, MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionCheck, SessionSupport, SubtitleFormat, SubtitleTrack, Variant, VariantKind,
    clean_title, fetch, timestamp_hint,
};
use crate::http::{BROWSER_UA, Cookie, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "bilibili";
const API: &str = "https://api.bilibili.com";
const SITE: &str = "https://www.bilibili.com/";
/// The cookie a logged-in bilibili.com session carries.
const SESSION_COOKIE: &str = "SESSDATA";
/// The device cookie the API wants before it answers at all.
const DEVICE_COOKIE: &str = "buvid3";
/// Every delivery kind the DASH play URL can list: DASH, HDR, 4K, Dolby audio and vision,
/// 8K and AV1.
const FNVAL_DASH: u32 = 4048;
/// How long a WBI mixin key is used before it is read again.
const KEY_LIFETIME: Duration = Duration::from_secs(12 * 60 * 60);
const PAGE_SIZE: usize = 30;
/// The most pages one list link is read through.
const MAX_PAGES: usize = 10;
/// The browser's drawing fingerprints the space listing asks for besides the signature.
const DM_IMG_STR: &str = "V2ViR0wgMS4wIChPcGVuR0wgRVMgMi4wIENocm9taXVtKQ";
const DM_COVER_IMG_STR: &str = "QU5HTEUgKEludGVsLCBJbnRlbChSKSBVSEQgR3JhcGhpY3MgNjMwIERpcmVjdDNEMTEgdnNfNV8wIHBzXzVfMCwgRDNEMTEpR29vZ2xlIEluYy4gKEludGVsKQ";
const DM_IMG_INTER: &str = r#"{"ds":[],"wh":[0,0,0],"of":[0,0,0]}"#;

/// The shuffle the site applies to the two image keys before it hashes with them.
const MIXIN_TABLE: [usize; 64] = [
    46, 47, 18, 2, 53, 8, 23, 32, 15, 50, 10, 31, 58, 3, 45, 35, 27, 43, 5, 49, 33, 9, 42, 19, 29,
    28, 14, 39, 12, 38, 41, 13, 37, 48, 7, 16, 24, 55, 40, 61, 26, 17, 0, 1, 60, 51, 30, 4, 22, 25,
    54, 21, 56, 59, 6, 63, 57, 62, 11, 36, 20, 34, 44, 52,
];

static RE_BV: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(BV[0-9A-Za-z]{10})$").unwrap());
static RE_AV: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^av(\d+)$").unwrap());
static RE_EP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^ep(\d+)$").unwrap());
static RE_SS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^ss(\d+)$").unwrap());
static RE_ML: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^ml(\d+)$").unwrap());

/// The platform's own id for a video: the new `BV` string or the old `av` number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoId {
    Bv(String),
    Av(u64),
}

impl VideoId {
    fn query(&self) -> (&'static str, String) {
        match self {
            VideoId::Bv(bvid) => ("bvid", bvid.clone()),
            VideoId::Av(aid) => ("aid", aid.to_string()),
        }
    }

    fn parse(text: &str) -> Option<Self> {
        if let Some(caps) = RE_BV.captures(text) {
            return Some(VideoId::Bv(caps[1].to_string()));
        }
        RE_AV
            .captures(text)
            .and_then(|caps| caps[1].parse().ok())
            .map(VideoId::Av)
    }
}

/// What a link names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Video {
        id: VideoId,
        /// The part of a multi-part video, counted from one.
        page: Option<u32>,
    },
    Episode(u64),
    Season(u64),
    /// A favourites folder, by its media id.
    Favourites(u64),
    /// A collection (a "season" of a user's own videos).
    Collection {
        mid: u64,
        season: u64,
    },
    /// A series of a user's own videos.
    Series {
        mid: u64,
        series: u64,
    },
    /// Every video a user uploaded.
    Space(u64),
    /// A `b23.tv` short link.
    Short(Url),
}

fn query(url: &Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
        .filter(|v| !v.is_empty())
}

fn digits(text: &str) -> Option<u64> {
    (!text.is_empty() && text.chars().all(|c| c.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
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
    if host == "b23.tv" || host == "bili2233.cn" {
        return (!segments.is_empty()).then(|| Link::Short(url.clone()));
    }
    let page = query(url, "p").and_then(|p| p.parse().ok());
    let video = |id: VideoId| Link::Video { id, page };
    if host == "space.bilibili.com" {
        let mid = digits(segments.first()?)?;
        return Some(match segments.get(1).copied() {
            None | Some("video") | Some("upload") => Link::Space(mid),
            Some("favlist") => match query(url, "fid").as_deref().and_then(digits) {
                Some(fid) => Link::Favourites(fid),
                None => Link::Space(mid),
            },
            Some("channel") => {
                let sid = query(url, "sid").as_deref().and_then(digits)?;
                match segments.get(2).copied() {
                    Some("collectiondetail") => Link::Collection { mid, season: sid },
                    Some("seriesdetail") => Link::Series { mid, series: sid },
                    _ => return None,
                }
            }
            Some("lists") => {
                let sid = segments.get(2).copied().and_then(digits)?;
                match query(url, "type").as_deref() {
                    Some("series") => Link::Series { mid, series: sid },
                    _ => Link::Collection { mid, season: sid },
                }
            }
            _ => return None,
        });
    }
    if !(host == "bilibili.com" || host.ends_with(".bilibili.com")) {
        return None;
    }
    if let Some(bvid) = query(url, "bvid").and_then(|b| VideoId::parse(&b)) {
        return Some(video(bvid));
    }
    if let Some(aid) = query(url, "aid").as_deref().and_then(digits) {
        return Some(video(VideoId::Av(aid)));
    }
    match segments.as_slice() {
        ["video", id, ..] | ["s", "video", id, ..] => VideoId::parse(id).map(video),
        ["bangumi", "play", id, ..] | ["bangumi", "media", id, ..] => {
            if let Some(caps) = RE_EP.captures(id) {
                caps[1].parse().ok().map(Link::Episode)
            } else if let Some(caps) = RE_SS.captures(id) {
                caps[1].parse().ok().map(Link::Season)
            } else {
                None
            }
        }
        ["list", id] | ["medialist", "detail", id] | ["medialist", "play", id, ..] => RE_ML
            .captures(id)
            .and_then(|caps| caps[1].parse().ok())
            .map(Link::Favourites),
        _ => None,
    }
}

/// The quality labels the API's quality numbers stand for.
fn quality_label(id: u64) -> Option<&'static str> {
    Some(match id {
        6 => "240p",
        16 => "360p",
        32 => "480p",
        64 => "720p",
        74 => "720p60",
        80 => "1080p",
        100 => "1080p (intelligent repair)",
        112 => "1080p+",
        116 => "1080p60",
        120 => "4K",
        125 => "HDR",
        126 => "Dolby Vision",
        127 => "8K",
        _ => return None,
    })
}

fn first_str<'a>(value: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|k| value[k].as_str().filter(|s| !s.is_empty()))
}

/// The best of the CDN URLs a stream lists.
fn stream_url(stream: &Value) -> Option<Url> {
    first_str(stream, &["baseUrl", "base_url"])
        .or_else(|| {
            stream["backupUrl"]
                .as_array()
                .or_else(|| stream["backup_url"].as_array())
                .and_then(|list| list.iter().find_map(|u| u.as_str()))
        })
        .and_then(|u| Url::parse(u).ok())
}

fn absolute(text: &str) -> Option<Url> {
    let text = text.trim();
    if text.starts_with("//") {
        Url::parse(&format!("https:{text}")).ok()
    } else {
        Url::parse(text).ok()
    }
}

struct MixinKey {
    key: String,
    fetched: Timestamp,
}

pub struct BilibiliResolver {
    http: Http,
    mixin: Mutex<Option<MixinKey>>,
}

/// The key the image and sub image file names shuffle into.
pub fn mixin_key(img_url: &str, sub_url: &str) -> Option<String> {
    let stem = |url: &str| -> Option<String> {
        let name = url.rsplit('/').next()?;
        Some(name.split('.').next()?.to_string())
    };
    let raw = format!("{}{}", stem(img_url)?, stem(sub_url)?);
    let chars: Vec<char> = raw.chars().collect();
    if chars.len() < 64 {
        return None;
    }
    Some(MIXIN_TABLE.iter().map(|&i| chars[i]).take(32).collect())
}

/// Signs `params` as the site does: sorted, URL-encoded without `!'()*`, with the
/// time stamp, and hashed with the mixin key into `w_rid`.
pub fn sign(params: &[(&str, String)], mixin: &str, wts: i64) -> Vec<(String, String)> {
    let mut all: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect();
    all.push(("wts".into(), wts.to_string()));
    all.sort();
    let cleaned = |value: &str| -> String {
        value
            .chars()
            .filter(|c| !matches!(c, '!' | '\'' | '(' | ')' | '*'))
            .collect()
    };
    // What `encodeURIComponent` leaves alone, once `!'()*` are gone.
    const ENCODE: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'_')
        .remove(b'.')
        .remove(b'~');
    let query: String = all
        .iter()
        .map(|(k, v)| {
            format!(
                "{k}={}",
                percent_encoding::utf8_percent_encode(&cleaned(v), ENCODE)
            )
        })
        .collect::<Vec<_>>()
        .join("&");
    let digest = Md5::digest(format!("{query}{mixin}").as_bytes());
    all.push(("w_rid".into(), hex::encode(digest)));
    all
}

impl BilibiliResolver {
    pub fn new(http: Http) -> Self {
        Self {
            http,
            mixin: Mutex::new(None),
        }
    }

    fn logged_in(&self) -> bool {
        self.http.jar(PLATFORM).get(SESSION_COOKIE).is_some()
    }

    /// Makes sure the jar carries the device cookie the API wants, asking for one when it
    /// does not.
    async fn device(&self, origin: &Url) -> Result<(), ResolveError> {
        if self.http.jar(PLATFORM).get(DEVICE_COOKIE).is_some() {
            return Ok(());
        }
        let url = Url::parse(&format!("{API}/x/frontend/finger/spi")).expect("valid");
        let fetched = fetch(&self.http, &url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        let value = fetched.json(origin)?;
        let b3 = value["data"]["b_3"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                ResolveError::malformed(origin, "the device cookie answer has no b_3")
            })?;
        let b4 = value["data"]["b_4"].as_str().unwrap_or("");
        self.http.with_jar(PLATFORM, |jar| {
            jar.insert(Cookie::new(DEVICE_COOKIE, b3, ".bilibili.com"));
            if !b4.is_empty() {
                jar.insert(Cookie::new("buvid4", b4, ".bilibili.com"));
            }
        });
        Ok(())
    }

    /// The site's navigation record: the WBI image keys, and who the cookies log in.
    async fn nav(&self, origin: &Url) -> Result<Value, ResolveError> {
        let url = Url::parse(&format!("{API}/x/web-interface/nav")).expect("valid");
        let headers = [("referer".to_string(), SITE.to_string())];
        let fetched = fetch(&self.http, &url, PLATFORM, BROWSER_UA, &headers, MAX_PAGE).await?;
        fetched.json(origin)
    }

    async fn mixin(&self, origin: &Url) -> Result<String, ResolveError> {
        if let Some(cached) = self
            .mixin
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            && Timestamp::now().duration_since(cached.fetched)
                < jiff::SignedDuration::try_from(KEY_LIFETIME).unwrap_or_default()
        {
            return Ok(cached.key.clone());
        }
        let nav = self.nav(origin).await?;
        let img = nav["data"]["wbi_img"]["img_url"].as_str().unwrap_or("");
        let sub = nav["data"]["wbi_img"]["sub_url"].as_str().unwrap_or("");
        let key = mixin_key(img, sub).ok_or_else(|| {
            ResolveError::malformed(origin, "the navigation answer has no WBI keys")
        })?;
        *self.mixin.lock().unwrap_or_else(|e| e.into_inner()) = Some(MixinKey {
            key: key.clone(),
            fetched: Timestamp::now(),
        });
        Ok(key)
    }

    /// GETs an API path, signed when `signed`, and turns the API's codes into errors.
    async fn api(
        &self,
        path: &str,
        params: &[(&str, String)],
        signed: bool,
        origin: &Url,
    ) -> Result<Value, ResolveError> {
        self.device(origin).await?;
        let mut url = Url::parse(&format!("{API}{path}"))
            .map_err(|e| ResolveError::malformed(origin, e.to_string()))?;
        if signed {
            let mixin = self.mixin(origin).await?;
            let signed = sign(params, &mixin, Timestamp::now().as_second());
            url.query_pairs_mut().extend_pairs(signed);
        } else {
            url.query_pairs_mut()
                .extend_pairs(params.iter().map(|(k, v)| (*k, v.as_str())));
        }
        let headers = [
            ("referer".to_string(), SITE.to_string()),
            ("origin".to_string(), "https://www.bilibili.com".to_string()),
        ];
        let fetched = fetch(&self.http, &url, PLATFORM, BROWSER_UA, &headers, MAX_PAGE).await?;
        match fetched.status.as_u16() {
            200..=299 => {}
            412 => {
                return Err(self.risk_refusal(origin, "the API blocked the request (HTTP 412)"));
            }
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the API answered HTTP {status}"),
                ));
            }
        }
        let value: Value = match serde_json::from_slice(&fetched.body) {
            Ok(value) => value,
            Err(_) if fetched.text().contains("<html") => {
                return Err(self.risk_refusal(origin, "the API answered with a verification page"));
            }
            Err(error) => return Err(ResolveError::malformed(origin, format!("JSON: {error}"))),
        };
        let code = value["code"].as_i64().unwrap_or(0);
        if code == 0 {
            return Ok(value);
        }
        let message = value["message"]
            .as_str()
            .filter(|m| !m.trim().is_empty())
            .map(|m| m.trim().to_string())
            .unwrap_or_else(|| format!("the API answered code {code}"));
        Err(match code {
            -404 | 62002 | 62004 | 62012 => match code {
                -404 => ResolveError::NotFound(origin.clone()),
                _ => ResolveError::unavailable(origin, message),
            },
            -400 => {
                ResolveError::malformed(origin, format!("the API refused the request: {message}"))
            }
            -799 | -509 => ResolveError::RateLimited(origin.clone()),
            -352 | -412 => self.risk_refusal(origin, &message),
            -403 | -101 | 6002 | 6010 | 10403 | -10403 => {
                if code == -10403 || code == 6002 {
                    ResolveError::unavailable(
                        origin,
                        format!("it is not available in this region ({message})"),
                    )
                } else if self.logged_in() {
                    ResolveError::unavailable(origin, message)
                } else {
                    ResolveError::login_required(origin, PLATFORM, message)
                }
            }
            _ => ResolveError::unavailable(origin, message),
        })
    }

    /// Risk control: a logged-in session passes it, a bare client does not.
    fn risk_refusal(&self, origin: &Url, message: &str) -> ResolveError {
        if self.logged_in() {
            ResolveError::unavailable(
                origin,
                format!("risk control refused the request: {message}"),
            )
        } else {
            ResolveError::login_required(
                origin,
                PLATFORM,
                format!("risk control refused the request without a session: {message}"),
            )
        }
    }

    /// The record of a video: title, owner, and the parts it has.
    async fn view(&self, id: &VideoId, origin: &Url) -> Result<Value, ResolveError> {
        let (key, value) = id.query();
        let answer = self
            .api("/x/web-interface/wbi/view", &[(key, value)], true, origin)
            .await?;
        Ok(answer["data"].clone())
    }

    /// The streams of one part, from the DASH play URL, with the MP4 play URL behind it
    /// for videos the site does not serve as DASH.
    async fn streams(
        &self,
        path: &str,
        params: &[(&str, String)],
        signed: bool,
        duration: Option<Duration>,
        origin: &Url,
    ) -> Result<Vec<Variant>, ResolveError> {
        let mut dash_params: Vec<(&str, String)> = params.to_vec();
        dash_params.extend([
            ("fnval", FNVAL_DASH.to_string()),
            ("fnver", "0".to_string()),
            ("fourk", "1".to_string()),
        ]);
        let answer = self.api(path, &dash_params, signed, origin).await?;
        let data = if answer["data"].is_object() {
            &answer["data"]
        } else {
            &answer["result"]
        };
        if data["is_preview"].as_i64() == Some(1) {
            return Err(if self.logged_in() {
                ResolveError::unavailable(
                    origin,
                    "only a preview is served; the video is for paying members",
                )
            } else {
                ResolveError::login_required(
                    origin,
                    PLATFORM,
                    "only a preview is served without a session",
                )
            });
        }
        let duration = data["timelength"]
            .as_u64()
            .filter(|t| *t > 0)
            .map(Duration::from_millis)
            .or(duration);
        let mut variants = variants_of(data, duration);
        if variants.is_empty() {
            let mut mp4_params: Vec<(&str, String)> = params.to_vec();
            mp4_params.extend([
                ("fnval", "1".to_string()),
                ("platform", "html5".to_string()),
                ("high_quality", "1".to_string()),
            ]);
            let answer = self.api(path, &mp4_params, signed, origin).await?;
            let data = if answer["data"].is_object() {
                &answer["data"]
            } else {
                &answer["result"]
            };
            variants = variants_of(data, duration);
        }
        if variants.is_empty() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        Ok(variants)
    }

    /// The subtitle tracks of one part.
    async fn subtitles(&self, id: &VideoId, cid: u64, origin: &Url) -> Vec<SubtitleTrack> {
        let (key, value) = id.query();
        let answer = match self
            .api(
                "/x/player/wbi/v2",
                &[(key, value), ("cid", cid.to_string())],
                true,
                origin,
            )
            .await
        {
            Ok(answer) => answer,
            Err(error) => {
                tracing::debug!(%origin, "bilibili subtitles not read: {error}");
                return Vec::new();
            }
        };
        answer["data"]["subtitle"]["subtitles"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|track| {
                let url = absolute(track["subtitle_url"].as_str()?)?;
                let language = track["lan"].as_str().unwrap_or("und").to_string();
                Some(SubtitleTrack {
                    url,
                    auto: language.starts_with("ai-")
                        || track["ai_type"].as_i64().is_some_and(|t| t != 0),
                    language: language.trim_start_matches("ai-").to_string(),
                    name: track["lan_doc"].as_str().map(String::from),
                    format: SubtitleFormat::BilibiliJson,
                    headers: vec![("referer".to_string(), SITE.to_string())],
                })
            })
            .collect()
    }

    async fn video(
        &self,
        id: &VideoId,
        page: Option<u32>,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let view = self.view(id, origin).await?;
        if let Some(redirect) = view["redirect_url"]
            .as_str()
            .and_then(|u| Url::parse(u).ok())
            && parse_link(&redirect).is_some_and(|l| matches!(l, Link::Episode(_)))
        {
            return Err(ResolveError::Redirect(redirect));
        }
        if view["is_upower_exclusive"].as_bool() == Some(true) {
            return Err(ResolveError::unavailable(
                origin,
                "it is for the uploader's paying supporters",
            ));
        }
        let bvid = view["bvid"].as_str().unwrap_or("").to_string();
        let id = if bvid.is_empty() {
            id.clone()
        } else {
            VideoId::Bv(bvid.clone())
        };
        let pages: Vec<&Value> = view["pages"].as_array().into_iter().flatten().collect();
        let title = view["title"].as_str().and_then(clean_title);
        let webpage = Url::parse(&format!("{SITE}video/{bvid}/")).ok();
        if pages.len() > 1 && page.is_none() {
            let entries = pages
                .iter()
                .filter_map(|part| {
                    let number = part["page"].as_u64()?;
                    let mut url = webpage.clone()?;
                    url.query_pairs_mut().append_pair("p", &number.to_string());
                    Some(PlaylistEntry {
                        url,
                        title: part["part"]
                            .as_str()
                            .and_then(clean_title)
                            .or_else(|| title.as_ref().map(|t| format!("{t} P{number}"))),
                        duration: part["duration"]
                            .as_u64()
                            .filter(|d| *d > 0)
                            .map(Duration::from_secs),
                    })
                })
                .collect::<Vec<_>>();
            return Ok(Resolution::Playlist(Playlist {
                resolver: PLATFORM.into(),
                id: Some(bvid),
                title,
                total: Some(entries.len()),
                entries,
            }));
        }
        let wanted = page.unwrap_or(1).max(1) as u64;
        let part = pages
            .iter()
            .find(|p| p["page"].as_u64() == Some(wanted))
            .copied()
            .or_else(|| pages.first().copied())
            .ok_or_else(|| ResolveError::malformed(origin, "the video record lists no parts"))?;
        let cid = part["cid"]
            .as_u64()
            .or_else(|| view["cid"].as_u64())
            .ok_or_else(|| ResolveError::malformed(origin, "the video part has no cid"))?;
        let duration = part["duration"]
            .as_u64()
            .or_else(|| view["duration"].as_u64())
            .filter(|d| *d > 0)
            .map(Duration::from_secs);
        let (key, value) = id.query();
        let variants = self
            .streams(
                "/x/player/wbi/playurl",
                &[(key, value), ("cid", cid.to_string())],
                true,
                duration,
                origin,
            )
            .await?;
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(match (&id, pages.len() > 1) {
            (VideoId::Bv(bvid), true) => format!("{bvid}_p{wanted}"),
            (VideoId::Bv(bvid), false) => bvid.clone(),
            (VideoId::Av(aid), _) => format!("av{aid}"),
        });
        resolved.title = match (pages.len() > 1, part["part"].as_str().and_then(clean_title)) {
            (true, Some(part_title)) => title
                .as_ref()
                .map(|t| format!("{t} - {part_title}"))
                .or(Some(part_title)),
            _ => title,
        };
        resolved.description = view["desc"].as_str().and_then(clean_title);
        resolved.uploader = view["owner"]["name"].as_str().and_then(clean_title);
        resolved.uploader_url = view["owner"]["mid"]
            .as_u64()
            .and_then(|mid| Url::parse(&format!("https://space.bilibili.com/{mid}")).ok());
        resolved.uploaded_at = view["pubdate"]
            .as_i64()
            .and_then(|t| Timestamp::from_second(t).ok());
        resolved.duration = variants.iter().find_map(|v| v.duration).or(duration);
        resolved.thumbnail = view["pic"].as_str().and_then(absolute);
        resolved.webpage_url = webpage.map(|mut url| {
            if pages.len() > 1 {
                url.query_pairs_mut().append_pair("p", &wanted.to_string());
            }
            url
        });
        resolved.clip = timestamp_hint(origin).map(|start| ClipRange { start, end: None });
        resolved.subtitles = self.subtitles(&id, cid, origin).await;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    /// A bangumi season's record, by episode or by season.
    async fn season(&self, key: &str, id: u64, origin: &Url) -> Result<Value, ResolveError> {
        let answer = self
            .api(
                "/pgc/view/web/season",
                &[(key, id.to_string())],
                false,
                origin,
            )
            .await?;
        Ok(answer["result"].clone())
    }

    async fn episode(&self, ep: u64, origin: &Url) -> Result<Resolution, ResolveError> {
        let season = self.season("ep_id", ep, origin).await?;
        let episodes: Vec<&Value> = season["episodes"]
            .as_array()
            .into_iter()
            .flatten()
            .collect();
        let episode = episodes
            .iter()
            .find(|e| e["id"].as_u64() == Some(ep))
            .copied()
            .ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
        let cid = episode["cid"]
            .as_u64()
            .ok_or_else(|| ResolveError::malformed(origin, "the episode has no cid"))?;
        let duration = episode["duration"]
            .as_u64()
            .filter(|d| *d > 0)
            .map(Duration::from_millis);
        let variants = self
            .streams(
                "/pgc/player/web/playurl",
                &[("ep_id", ep.to_string()), ("cid", cid.to_string())],
                false,
                duration,
                origin,
            )
            .await?;
        let season_title = season["season_title"]
            .as_str()
            .or_else(|| season["title"].as_str())
            .and_then(clean_title);
        let episode_title = [episode["title"].as_str(), episode["long_title"].as_str()]
            .into_iter()
            .flatten()
            .filter(|t| !t.trim().is_empty())
            .collect::<Vec<_>>()
            .join(" ");
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(format!("ep{ep}"));
        resolved.title = match (&season_title, clean_title(&episode_title)) {
            (Some(season), Some(episode)) => Some(format!("{season} {episode}")),
            (Some(season), None) => Some(season.clone()),
            (None, episode) => episode,
        };
        resolved.description = season["evaluate"].as_str().and_then(clean_title);
        resolved.uploader = season["up_info"]["uname"]
            .as_str()
            .and_then(clean_title)
            .or(season_title);
        resolved.uploaded_at = episode["pub_time"]
            .as_i64()
            .and_then(|t| Timestamp::from_second(t).ok());
        resolved.duration = variants.iter().find_map(|v| v.duration).or(duration);
        resolved.thumbnail = episode["cover"]
            .as_str()
            .or_else(|| season["cover"].as_str())
            .and_then(absolute);
        resolved.webpage_url = Url::parse(&format!("{SITE}bangumi/play/ep{ep}")).ok();
        resolved.clip = timestamp_hint(origin).map(|start| ClipRange { start, end: None });
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn season_playlist(&self, ss: u64, origin: &Url) -> Result<Resolution, ResolveError> {
        let season = self.season("season_id", ss, origin).await?;
        let entries: Vec<PlaylistEntry> = season["episodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|episode| {
                let id = episode["id"].as_u64()?;
                let title = [episode["title"].as_str(), episode["long_title"].as_str()]
                    .into_iter()
                    .flatten()
                    .filter(|t| !t.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join(" ");
                Some(PlaylistEntry {
                    url: Url::parse(&format!("{SITE}bangumi/play/ep{id}")).ok()?,
                    title: clean_title(&title),
                    duration: episode["duration"]
                        .as_u64()
                        .filter(|d| *d > 0)
                        .map(Duration::from_millis),
                })
            })
            .collect();
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.into(),
            id: Some(format!("ss{ss}")),
            title: season["season_title"]
                .as_str()
                .or_else(|| season["title"].as_str())
                .and_then(clean_title),
            total: season["total"]
                .as_u64()
                .map(|t| t as usize)
                .filter(|t| *t >= entries.len())
                .or(Some(entries.len())),
            entries,
        }))
    }

    /// Turns the `archives`, `medias` or `vlist` of one list page into entries.
    fn entries_of(items: &[Value]) -> Vec<PlaylistEntry> {
        items
            .iter()
            .filter_map(|item| {
                if item["type"].as_i64().is_some_and(|t| t != 2) {
                    return None;
                }
                let bvid = item["bvid"].as_str().filter(|b| !b.is_empty());
                let url = match (bvid, item["aid"].as_u64()) {
                    (Some(bvid), _) => Url::parse(&format!("{SITE}video/{bvid}/")).ok()?,
                    (None, Some(aid)) => Url::parse(&format!("{SITE}video/av{aid}/")).ok()?,
                    (None, None) => return None,
                };
                let duration = match &item["duration"] {
                    Value::Number(n) => n.as_u64().filter(|d| *d > 0).map(Duration::from_secs),
                    Value::String(s) => super::parse_time_stamp(s),
                    _ => None,
                }
                .or_else(|| item["length"].as_str().and_then(super::parse_time_stamp));
                Some(PlaylistEntry {
                    url,
                    title: item["title"].as_str().and_then(clean_title),
                    duration,
                })
            })
            .collect()
    }

    async fn favourites(&self, media_id: u64, origin: &Url) -> Result<Resolution, ResolveError> {
        let mut entries = Vec::new();
        let mut title = None;
        let mut total = None;
        for page in 1..=MAX_PAGES {
            let answer = self
                .api(
                    "/x/v3/fav/resource/list",
                    &[
                        ("media_id", media_id.to_string()),
                        ("pn", page.to_string()),
                        ("ps", PAGE_SIZE.to_string()),
                        ("platform", "web".to_string()),
                    ],
                    false,
                    origin,
                )
                .await?;
            let data = &answer["data"];
            if title.is_none() {
                title = data["info"]["title"].as_str().and_then(clean_title);
                total = data["info"]["media_count"].as_u64().map(|c| c as usize);
            }
            let items = data["medias"].as_array().cloned().unwrap_or_default();
            entries.extend(Self::entries_of(&items));
            if data["has_more"].as_bool() != Some(true) || items.is_empty() {
                break;
            }
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.into(),
            id: Some(format!("ml{media_id}")),
            title,
            total: total
                .filter(|t| *t >= entries.len())
                .or(Some(entries.len())),
            entries,
        }))
    }

    async fn collection(
        &self,
        mid: u64,
        season: u64,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let mut entries = Vec::new();
        let mut title = None;
        let mut total = None;
        for page in 1..=MAX_PAGES {
            let answer = self
                .api(
                    "/x/polymer/web-space/seasons_archives_list",
                    &[
                        ("mid", mid.to_string()),
                        ("season_id", season.to_string()),
                        ("page_num", page.to_string()),
                        ("page_size", PAGE_SIZE.to_string()),
                        ("sort_reverse", "false".to_string()),
                    ],
                    false,
                    origin,
                )
                .await?;
            let data = &answer["data"];
            if title.is_none() {
                title = data["meta"]["name"].as_str().and_then(clean_title);
                total = data["page"]["total"].as_u64().map(|t| t as usize);
            }
            let items = data["archives"].as_array().cloned().unwrap_or_default();
            entries.extend(Self::entries_of(&items));
            if items.len() < PAGE_SIZE || total.is_some_and(|t| entries.len() >= t) {
                break;
            }
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.into(),
            id: Some(format!("collection{season}")),
            title,
            total: total
                .filter(|t| *t >= entries.len())
                .or(Some(entries.len())),
            entries,
        }))
    }

    async fn series(
        &self,
        mid: u64,
        series: u64,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let meta = self
            .api(
                "/x/series/series",
                &[("series_id", series.to_string())],
                false,
                origin,
            )
            .await?;
        let title = meta["data"]["meta"]["name"].as_str().and_then(clean_title);
        let mut entries = Vec::new();
        let mut total = None;
        for page in 1..=MAX_PAGES {
            let answer = self
                .api(
                    "/x/series/archives",
                    &[
                        ("mid", mid.to_string()),
                        ("series_id", series.to_string()),
                        ("pn", page.to_string()),
                        ("ps", PAGE_SIZE.to_string()),
                        ("only_normal", "true".to_string()),
                        ("sort", "desc".to_string()),
                    ],
                    false,
                    origin,
                )
                .await?;
            let data = &answer["data"];
            if total.is_none() {
                total = data["page"]["total"].as_u64().map(|t| t as usize);
            }
            let items = data["archives"].as_array().cloned().unwrap_or_default();
            entries.extend(Self::entries_of(&items));
            if items.len() < PAGE_SIZE || total.is_some_and(|t| entries.len() >= t) {
                break;
            }
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.into(),
            id: Some(format!("series{series}")),
            title,
            total: total
                .filter(|t| *t >= entries.len())
                .or(Some(entries.len())),
            entries,
        }))
    }

    async fn space(&self, mid: u64, origin: &Url) -> Result<Resolution, ResolveError> {
        let mut entries = Vec::new();
        let mut title = None;
        let mut total = None;
        for page in 1..=MAX_PAGES {
            let answer = self
                .api(
                    "/x/space/wbi/arc/search",
                    &[
                        ("mid", mid.to_string()),
                        ("pn", page.to_string()),
                        ("ps", PAGE_SIZE.to_string()),
                        ("order", "pubdate".to_string()),
                        ("platform", "web".to_string()),
                        ("web_location", "1550101".to_string()),
                        ("dm_img_list", "[]".to_string()),
                        ("dm_img_str", DM_IMG_STR.to_string()),
                        ("dm_cover_img_str", DM_COVER_IMG_STR.to_string()),
                        ("dm_img_inter", DM_IMG_INTER.to_string()),
                    ],
                    true,
                    origin,
                )
                .await?;
            let data = &answer["data"];
            let items = data["list"]["vlist"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            if title.is_none() {
                title = items
                    .first()
                    .and_then(|item| item["author"].as_str())
                    .and_then(clean_title)
                    .map(|author| format!("{author}'s videos"));
                total = data["page"]["count"].as_u64().map(|c| c as usize);
            }
            entries.extend(Self::entries_of(&items));
            if items.len() < PAGE_SIZE || total.is_some_and(|t| entries.len() >= t) {
                break;
            }
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.into(),
            id: Some(format!("space{mid}")),
            title,
            total: total
                .filter(|t| *t >= entries.len())
                .or(Some(entries.len())),
            entries,
        }))
    }
}

/// The variants a play URL answer lists: every DASH video and audio stream, or the
/// whole MP4 files of the `durl` list.
pub fn variants_of(data: &Value, duration: Option<Duration>) -> Vec<Variant> {
    let headers = vec![
        ("referer".to_string(), SITE.to_string()),
        ("user-agent".to_string(), BROWSER_UA.to_string()),
    ];
    let mut variants = Vec::new();
    let dash = &data["dash"];
    for stream in dash["video"].as_array().into_iter().flatten() {
        let Some(url) = stream_url(stream) else {
            continue;
        };
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(Container::Mp4);
        if let Some(codecs) = stream["codecs"].as_str() {
            v = v.with_codecs(codecs);
        }
        v.width = stream["width"].as_u64().map(|w| w as u32);
        v.height = stream["height"].as_u64().map(|h| h as u32);
        v.fps = stream["frameRate"]
            .as_str()
            .or_else(|| stream["frame_rate"].as_str())
            .and_then(|f| f.parse().ok())
            .filter(|f: &f64| *f > 0.0);
        v.bitrate = stream["bandwidth"].as_u64().filter(|b| *b > 0);
        v.duration = duration;
        v.video_only = true;
        let quality = stream["id"].as_u64().unwrap_or(0);
        v.format_id = Some(format!(
            "dash-{quality}-{}",
            stream["codecid"].as_u64().unwrap_or(0)
        ));
        v.label = quality_label(quality).map(String::from);
        v.headers = headers.clone();
        variants.push(v);
    }
    let audio_streams = dash["audio"]
        .as_array()
        .into_iter()
        .flatten()
        .chain(
            dash["flac"]["audio"]
                .as_object()
                .map(|_| &dash["flac"]["audio"]),
        )
        .chain(dash["dolby"]["audio"].as_array().into_iter().flatten());
    for stream in audio_streams {
        let Some(url) = stream_url(stream) else {
            continue;
        };
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(Container::Mp4);
        if let Some(codecs) = stream["codecs"].as_str() {
            v = v.with_codecs(codecs);
        }
        v.bitrate = stream["bandwidth"].as_u64().filter(|b| *b > 0);
        v.duration = duration;
        v.audio_only = true;
        v.format_id = Some(format!("dash-audio-{}", stream["id"].as_u64().unwrap_or(0)));
        v.headers = headers.clone();
        variants.push(v);
    }
    if variants.is_empty() {
        for file in data["durl"].as_array().into_iter().flatten() {
            let Some(url) = file["url"].as_str().and_then(|u| Url::parse(u).ok()) else {
                continue;
            };
            let mut v = Variant::new(url, VariantKind::File);
            v.container = Some(match data["format"].as_str() {
                Some(format) if format.starts_with("flv") => Container::Flv,
                _ => Container::Mp4,
            });
            v.video = Some(VideoCodec::H264);
            v.audio = Some(AudioCodec::Aac);
            v.size = file["size"].as_u64().filter(|s| *s > 0);
            v.duration = file["length"]
                .as_u64()
                .filter(|l| *l > 0)
                .map(Duration::from_millis)
                .or(duration);
            let quality = data["quality"].as_u64().unwrap_or(0);
            v.format_id = Some(format!(
                "{}-{quality}",
                data["format"].as_str().unwrap_or("mp4")
            ));
            v.label = quality_label(quality).map(String::from);
            v.headers = headers.clone();
            variants.push(v);
        }
    }
    variants
}

#[async_trait]
impl Resolver for BilibiliResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Bilibili",
            hosts: &["bilibili.com", "b23.tv"],
            features: &[
                "videos",
                "multi-part videos",
                "bangumi episodes",
                "seasons",
                "favourites",
                "collections",
                "series",
                "user spaces",
                "short links",
                "subtitles",
            ],
            formats: &["mp4", "flv"],
            session: SessionSupport::Optional,
            examples: &[
                "https://www.bilibili.com/video/BV1xx411c7mD/",
                "https://b23.tv/BV1xx411c7mD",
                "https://www.bilibili.com/video/av170001",
                "https://space.bilibili.com/686127/favlist?fid=1052622027",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        match link {
            Link::Short(short) => {
                let target = self
                    .http
                    .unwrap_redirects(&short, Some(PLATFORM), BROWSER_UA)
                    .await?;
                if target.host_str() == short.host_str() {
                    return Err(ResolveError::NotFound(url.clone()));
                }
                match parse_link(&target) {
                    Some(Link::Short(_)) | None => Err(ResolveError::Redirect(target)),
                    Some(_) => Err(ResolveError::Redirect(target)),
                }
            }
            Link::Video { id, page } => self.video(&id, page, url).await,
            Link::Episode(ep) => self.episode(ep, url).await,
            Link::Season(ss) => self.season_playlist(ss, url).await,
            Link::Favourites(media_id) => self.favourites(media_id, url).await,
            Link::Collection { mid, season } => self.collection(mid, season, url).await,
            Link::Series { mid, series } => self.series(mid, series, url).await,
            Link::Space(mid) => self.space(mid, url).await,
        }
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        if !self.logged_in() {
            return Ok(SessionCheck::LoggedOut);
        }
        let nav_url = Url::parse(&format!("{API}/x/web-interface/nav")).expect("valid");
        let nav = self.nav(&nav_url).await?;
        Ok(match nav["data"]["isLogin"].as_bool() {
            Some(true) => SessionCheck::LoggedIn {
                account: nav["data"]["uname"]
                    .as_str()
                    .filter(|n| !n.is_empty())
                    .map(String::from)
                    .or_else(|| nav["data"]["mid"].as_u64().map(|m| format!("mid {m}")))
                    .unwrap_or_else(|| "a Bilibili account".into()),
            },
            _ => SessionCheck::LoggedOut,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };
    use serde_json::json;

    fn get(url: &str, status: u16, body: String) -> Exchange {
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
                body: RecordedBody::Text(body),
                truncated: false,
            },
        }
    }

    fn nav() -> Exchange {
        get(
            "https://api.bilibili.com/x/web-interface/nav",
            200,
            json!({"code": -101, "data": {"isLogin": false, "wbi_img": {
                "img_url": "https://i0.hdslb.com/bfs/wbi/7cd084941338484aae1ad9425b84077c.png",
                "sub_url": "https://i0.hdslb.com/bfs/wbi/4932caff0ff746eab6f01bf08b70ac45.png"}}})
            .to_string(),
        )
    }

    fn spi() -> Exchange {
        get(
            "https://api.bilibili.com/x/frontend/finger/spi",
            200,
            json!({"code": 0, "data": {"b_3": "DEVICE-3", "b_4": "DEVICE-4"}}).to_string(),
        )
    }

    fn view(pages: Vec<Value>) -> Exchange {
        get(
            "https://api.bilibili.com/x/web-interface/wbi/view",
            200,
            json!({"code": 0, "data": {
                "bvid": "BV1xx411c7mD", "aid": 2, "videos": pages.len(), "pic": "//i0.hdslb.com/pic.jpg",
                "title": "字幕君交流场所", "pubdate": 1252458549, "desc": "www", "duration": 2055,
                "owner": {"mid": 2, "name": "碧诗"}, "cid": 62131, "pages": pages}})
            .to_string(),
        )
    }

    fn playurl() -> Exchange {
        get(
            "https://api.bilibili.com/x/player/wbi/playurl",
            200,
            json!({"code": 0, "data": {"quality": 32, "format": "flv480", "timelength": 2055637,
                "dash": {
                    "video": [
                        {"id": 32, "baseUrl": "https://upos.bilivideo.com/v32-hevc.m4s", "bandwidth": 69367, "codecs": "hev1.1.6.L120.90", "width": 512, "height": 384, "frameRate": "15", "codecid": 12},
                        {"id": 32, "baseUrl": "https://upos.bilivideo.com/v32-avc.m4s", "bandwidth": 155817, "codecs": "avc1.64001E", "width": 512, "height": 384, "frameRate": "14.925", "codecid": 7},
                        {"id": 16, "base_url": "https://upos.bilivideo.com/v16-av1.m4s", "bandwidth": 206445, "codecs": "av01.0.01M.08", "width": 480, "height": 360, "frameRate": "15.009", "codecid": 13}
                    ],
                    "audio": [
                        {"id": 30216, "baseUrl": "https://upos.bilivideo.com/a64.m4s", "bandwidth": 68646, "codecs": "mp4a.40.2"},
                        {"id": 30232, "baseUrl": "https://upos.bilivideo.com/a132.m4s", "bandwidth": 134695, "codecs": "mp4a.40.2"}
                    ],
                    "flac": null, "dolby": {"type": 0, "audio": null}
                }}})
            .to_string(),
        )
    }

    fn subtitles() -> Exchange {
        get(
            "https://api.bilibili.com/x/player/wbi/v2",
            200,
            json!({"code": 0, "data": {"subtitle": {"subtitles": [
                {"lan": "ai-zh", "lan_doc": "中文（自动生成）", "subtitle_url": "//aisubtitle.hdslb.com/bfs/ai_subtitle/zh.json", "ai_type": 1},
                {"lan": "en-US", "lan_doc": "English", "subtitle_url": "https://i0.hdslb.com/bfs/subtitle/en.json", "ai_type": 0}
            ]}}})
            .to_string(),
        )
    }

    #[test]
    fn links_of_every_shape_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://www.bilibili.com/video/BV1xx411c7mD/?p=2&t=10"),
            Some(Link::Video {
                id: VideoId::Bv("BV1xx411c7mD".into()),
                page: Some(2)
            })
        );
        assert_eq!(
            link("https://m.bilibili.com/video/av170001"),
            Some(Link::Video {
                id: VideoId::Av(170001),
                page: None
            })
        );
        assert_eq!(
            link("https://www.bilibili.com/festival/2023bnj?bvid=BV1ov4y1i7sQ"),
            Some(Link::Video {
                id: VideoId::Bv("BV1ov4y1i7sQ".into()),
                page: None
            })
        );
        assert_eq!(
            link("https://www.bilibili.com/bangumi/play/ep330069"),
            Some(Link::Episode(330069))
        );
        assert_eq!(
            link("https://www.bilibili.com/bangumi/play/ss12548?from=x"),
            Some(Link::Season(12548))
        );
        assert_eq!(
            link("https://www.bilibili.com/list/ml1052622027"),
            Some(Link::Favourites(1052622027))
        );
        assert_eq!(
            link("https://space.bilibili.com/686127/favlist?fid=1052622027&ftype=create"),
            Some(Link::Favourites(1052622027))
        );
        assert_eq!(
            link("https://space.bilibili.com/946974/channel/collectiondetail?sid=12"),
            Some(Link::Collection {
                mid: 946974,
                season: 12
            })
        );
        assert_eq!(
            link("https://space.bilibili.com/946974/lists/34?type=series"),
            Some(Link::Series {
                mid: 946974,
                series: 34
            })
        );
        assert_eq!(
            link("https://space.bilibili.com/946974/video"),
            Some(Link::Space(946974))
        );
        assert!(matches!(
            link("https://b23.tv/abc123"),
            Some(Link::Short(_))
        ));
        assert_eq!(link("https://b23.tv/"), None);
        assert_eq!(link("https://www.bilibili.com/"), None);
        assert_eq!(link("https://example.com/video/BV1xx411c7mD"), None);
    }

    #[test]
    fn the_signature_matches_the_site() {
        let key = mixin_key(
            "https://i0.hdslb.com/bfs/wbi/7cd084941338484aae1ad9425b84077c.png",
            "https://i0.hdslb.com/bfs/wbi/4932caff0ff746eab6f01bf08b70ac45.png",
        )
        .unwrap();
        assert_eq!(key, "ea1db124af3c7062474693fa704f4ff8");
        let signed = sign(
            &[
                ("foo", "one one four".into()),
                ("bar", "五一四".into()),
                ("zab", "1919810".into()),
            ],
            &key,
            1702204169,
        );
        let rid = signed
            .iter()
            .find(|(k, _)| k == "w_rid")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert_eq!(rid, "993fe49bb52e4183d188d4712eed9862");
        assert!(signed.iter().any(|(k, v)| k == "wts" && v == "1702204169"));
        assert!(mixin_key("short", "x").is_none());
    }

    #[tokio::test]
    async fn videos_resolve_to_dash_streams_with_subtitles() {
        let mut fixture = Fixture::new("bilibili", None);
        fixture.exchanges.push(spi());
        fixture.exchanges.push(nav());
        fixture.exchanges.push(view(vec![
            json!({"cid": 62131, "page": 1, "part": "", "duration": 2055}),
        ]));
        fixture.exchanges.push(playurl());
        fixture.exchanges.push(subtitles());
        let http = Http::replay(fixture);
        let resolver = BilibiliResolver::new(http.clone());
        let url = Url::parse("https://www.bilibili.com/video/BV1xx411c7mD/?t=95").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("BV1xx411c7mD"));
        assert_eq!(resolved.title.as_deref(), Some("字幕君交流场所"));
        assert_eq!(resolved.uploader.as_deref(), Some("碧诗"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://space.bilibili.com/2"
        );
        assert_eq!(resolved.duration, Some(Duration::from_millis(2055637)));
        assert_eq!(resolved.clip.unwrap().start, Duration::from_secs(95));
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://i0.hdslb.com/pic.jpg"
        );
        assert_eq!(resolved.variants.len(), 5);
        let video: Vec<_> = resolved.variants.iter().filter(|v| v.video_only).collect();
        assert_eq!(video.len(), 3);
        assert_eq!(video[0].video, Some(VideoCodec::H265));
        assert_eq!(video[1].video, Some(VideoCodec::H264));
        assert_eq!(video[2].video, Some(VideoCodec::Av1));
        assert_eq!(video[0].label.as_deref(), Some("480p"));
        assert_eq!(video[1].fps, Some(14.925));
        assert_eq!(
            video[2].url.as_str(),
            "https://upos.bilivideo.com/v16-av1.m4s"
        );
        assert_eq!(video[0].headers[0], ("referer".into(), SITE.into()));
        let audio: Vec<_> = resolved.variants.iter().filter(|v| v.audio_only).collect();
        assert_eq!(audio.len(), 2);
        assert_eq!(audio[1].audio, Some(AudioCodec::Aac));
        assert_eq!(audio[1].bitrate, Some(134695));
        assert_eq!(resolved.subtitles.len(), 2);
        assert!(resolved.subtitles[0].auto);
        assert_eq!(resolved.subtitles[0].language, "zh");
        assert_eq!(
            resolved.subtitles[0].url.as_str(),
            "https://aisubtitle.hdslb.com/bfs/ai_subtitle/zh.json"
        );
        assert_eq!(resolved.subtitles[1].format, SubtitleFormat::BilibiliJson);
        assert!(!resolved.subtitles[1].auto);
        // The device cookie was stored for every request after the first.
        assert_eq!(http.jar(PLATFORM).get("buvid3").unwrap().value, "DEVICE-3");
    }

    #[tokio::test]
    async fn multi_part_videos_list_their_parts_and_a_part_is_one_video() {
        let pages = vec![
            json!({"cid": 279786, "page": 1, "part": "Хоп", "duration": 200}),
            json!({"cid": 275431, "page": 2, "part": "Imash li surce", "duration": 210}),
        ];
        let mut fixture = Fixture::new("bilibili", None);
        fixture.exchanges.push(spi());
        fixture.exchanges.push(nav());
        fixture.exchanges.push(view(pages.clone()));
        let resolver = BilibiliResolver::new(Http::replay(fixture));
        let playlist = match resolver
            .resolve(&Url::parse("https://www.bilibili.com/video/av2").unwrap())
            .await
            .unwrap()
        {
            Resolution::Playlist(playlist) => playlist,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[1].url.as_str(),
            "https://www.bilibili.com/video/BV1xx411c7mD/?p=2"
        );
        assert_eq!(playlist.entries[1].title.as_deref(), Some("Imash li surce"));
        assert_eq!(playlist.entries[1].duration, Some(Duration::from_secs(210)));

        let mut fixture = Fixture::new("bilibili", None);
        fixture.exchanges.push(spi());
        fixture.exchanges.push(nav());
        fixture.exchanges.push(view(pages));
        fixture.exchanges.push(playurl());
        fixture.exchanges.push(get(
            "https://api.bilibili.com/x/player/wbi/v2",
            200,
            json!({"code": 0, "data": {"subtitle": {"subtitles": []}}}).to_string(),
        ));
        let resolver = BilibiliResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://www.bilibili.com/video/BV1xx411c7mD?p=2").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("BV1xx411c7mD_p2"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("字幕君交流场所 - Imash li surce")
        );
        assert_eq!(
            resolved.webpage_url.as_ref().unwrap().as_str(),
            "https://www.bilibili.com/video/BV1xx411c7mD/?p=2"
        );
    }

    #[tokio::test]
    async fn api_codes_become_the_right_errors() {
        let cases = [
            (json!({"code": -404, "message": "啥都木有"}), "not found"),
            (
                json!({"code": 62002, "message": "稿件不可见"}),
                "unavailable",
            ),
            (json!({"code": -352, "message": "风控校验失败"}), "login"),
            (json!({"code": -799, "message": "请求过于频繁"}), "rate"),
        ];
        for (answer, expected) in cases {
            let mut fixture = Fixture::new("bilibili", None);
            fixture.exchanges.push(spi());
            fixture.exchanges.push(nav());
            fixture.exchanges.push(get(
                "https://api.bilibili.com/x/web-interface/wbi/view",
                200,
                answer.to_string(),
            ));
            let resolver = BilibiliResolver::new(Http::replay(fixture));
            let error = resolver
                .resolve(&Url::parse("https://www.bilibili.com/video/BV1GJ411x7h7").unwrap())
                .await
                .unwrap_err();
            let matched = match expected {
                "not found" => matches!(error, ResolveError::NotFound(_)),
                "unavailable" => matches!(error, ResolveError::Unavailable { .. }),
                "login" => matches!(error, ResolveError::LoginRequired { .. }),
                _ => matches!(error, ResolveError::RateLimited(_)),
            };
            assert!(matched, "{expected}: {error}");
        }
        // A verification page instead of JSON is risk control too.
        let mut fixture = Fixture::new("bilibili", None);
        fixture.exchanges.push(spi());
        fixture.exchanges.push(nav());
        fixture.exchanges.push(get(
            "https://api.bilibili.com/x/web-interface/wbi/view",
            200,
            "<!DOCTYPE html><html><title>出错啦!</title></html>".into(),
        ));
        let resolver = BilibiliResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://www.bilibili.com/video/BV1GJ411x7h7").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(error, ResolveError::LoginRequired { .. }),
            "{error}"
        );
    }

    #[tokio::test]
    async fn episodes_seasons_and_region_locks() {
        let season = json!({"code": 0, "result": {
        "season_id": 12548, "season_title": "让子弹飞", "title": "让子弹飞", "evaluate": "姜文", "cover": "//i0.hdslb.com/cover.png", "total": 2,
        "episodes": [
            {"id": 199612, "aid": 21071819, "cid": 27499037045_u64, "bvid": "BV14W411g72d", "title": "普通话", "long_title": "", "duration": 7914625, "pub_time": 1500000000, "cover": "https://i0.hdslb.com/ep1.jpg"},
            {"id": 328482, "aid": 243555765, "cid": 202720615, "bvid": "BV1dv411q7rK", "title": "四川话", "long_title": "方言版", "duration": 7931000}
        ]}});
        let mut fixture = Fixture::new("bilibili", None);
        fixture.exchanges.push(spi());
        fixture.exchanges.push(get(
            "https://api.bilibili.com/pgc/view/web/season",
            200,
            season.to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.bilibili.com/pgc/player/web/playurl",
            200,
            json!({"code": 0, "result": {"quality": 80, "format": "mp4", "timelength": 7914625, "durl": [
                {"order": 1, "length": 7914625, "size": 1234567, "url": "https://upos.bilivideo.com/ep1.mp4"}]}})
            .to_string(),
        ));
        let resolver = BilibiliResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://www.bilibili.com/bangumi/play/ep199612").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("ep199612"));
        assert_eq!(resolved.title.as_deref(), Some("让子弹飞 普通话"));
        assert_eq!(resolved.duration, Some(Duration::from_millis(7914625)));
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].size, Some(1234567));
        assert_eq!(resolved.variants[0].label.as_deref(), Some("1080p"));
        assert!(!resolved.variants[0].video_only);

        let mut fixture = Fixture::new("bilibili", None);
        fixture.exchanges.push(spi());
        fixture.exchanges.push(get(
            "https://api.bilibili.com/pgc/view/web/season",
            200,
            season.to_string(),
        ));
        let resolver = BilibiliResolver::new(Http::replay(fixture));
        let playlist = match resolver
            .resolve(&Url::parse("https://www.bilibili.com/bangumi/play/ss12548").unwrap())
            .await
            .unwrap()
        {
            Resolution::Playlist(playlist) => playlist,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(playlist.title.as_deref(), Some("让子弹飞"));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(playlist.entries[1].title.as_deref(), Some("四川话 方言版"));
        assert_eq!(
            playlist.entries[1].url.as_str(),
            "https://www.bilibili.com/bangumi/play/ep328482"
        );

        let mut fixture = Fixture::new("bilibili", None);
        fixture.exchanges.push(spi());
        fixture.exchanges.push(get(
            "https://api.bilibili.com/pgc/view/web/season",
            200,
            season.to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.bilibili.com/pgc/player/web/playurl",
            200,
            json!({"code": -10403, "message": "抱歉您所在地区不可观看！"}).to_string(),
        ));
        let resolver = BilibiliResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://www.bilibili.com/bangumi/play/ep199612").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("region")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn favourites_and_short_links() {
        let mut fixture = Fixture::new("bilibili", None);
        fixture.exchanges.push(spi());
        fixture.exchanges.push(get(
            "https://api.bilibili.com/x/v3/fav/resource/list",
            200,
            json!({"code": 0, "data": {"info": {"id": 1052622027, "title": "猛 男 生 存", "media_count": 2}, "has_more": false,
                "medias": [
                    {"id": 371494037, "type": 2, "title": "第一集", "bvid": "BV1CZ4y1T7gC", "duration": 600},
                    {"id": 1, "type": 12, "title": "an audio", "bvid": "", "duration": 10},
                    {"id": 371494038, "type": 2, "title": "第二集", "bvid": "BV1oA4y1x7xx", "duration": 720}
                ]}})
            .to_string(),
        ));
        let resolver = BilibiliResolver::new(Http::replay(fixture));
        let playlist = match resolver
            .resolve(&Url::parse("https://www.bilibili.com/list/ml1052622027").unwrap())
            .await
            .unwrap()
        {
            Resolution::Playlist(playlist) => playlist,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(playlist.title.as_deref(), Some("猛 男 生 存"));
        assert_eq!(playlist.total, Some(2));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://www.bilibili.com/video/BV1CZ4y1T7gC/"
        );
        assert_eq!(playlist.entries[1].duration, Some(Duration::from_secs(720)));

        let mut fixture = Fixture::new("bilibili", None);
        fixture.exchanges.push(Exchange {
            request: RecordedRequest {
                method: "GET".into(),
                url: "https://b23.tv/abc".into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status: 302,
                url: "https://b23.tv/abc".into(),
                headers: vec![(
                    "location".into(),
                    "https://www.bilibili.com/video/BV1xx411c7mD?p=1&share_source=copy_web".into(),
                )],
                body: RecordedBody::Empty,
                truncated: false,
            },
        });
        fixture.exchanges.push(get(
            "https://www.bilibili.com/video/BV1xx411c7mD?p=1&share_source=copy_web",
            200,
            "<html></html>".into(),
        ));
        let resolver = BilibiliResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://b23.tv/abc").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Redirect(u) if u.as_str().starts_with("https://www.bilibili.com/video/BV1xx411c7mD")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn sessions_are_checked_through_the_navigation_record() {
        let mut fixture = Fixture::new("bilibili", None);
        fixture.exchanges.push(get(
            "https://api.bilibili.com/x/web-interface/nav",
            200,
            json!({"code": 0, "data": {"isLogin": true, "uname": "nick", "mid": 5}}).to_string(),
        ));
        let http = Http::replay(fixture);
        let resolver = BilibiliResolver::new(http.clone());
        assert_eq!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedOut
        );
        http.with_jar(PLATFORM, |jar| {
            jar.insert(Cookie::new(SESSION_COOKIE, "s", ".bilibili.com"));
        });
        assert_eq!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedIn {
                account: "nick".into()
            }
        );
    }
}
