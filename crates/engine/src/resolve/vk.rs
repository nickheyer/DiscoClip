//! VK videos, clips, embeds and live streams, through the player request the site's own
//! pages make, with the visitor cookies it hands out first.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    ClipRange, MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionCheck,
    SessionSupport, SubtitleFormat, SubtitleTrack, Variant, VariantKind, clean_title,
    timestamp_hint,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "vk";
const SITE: &str = "https://vk.com/";
const PLAYER_API: &str = "https://vk.com/al_video.php?act=show";
/// The cookie a logged-in vk.com session carries.
const SESSION_COOKIE: &str = "remixsid";
/// The cookie every visitor is given, which the player request wants.
const VISITOR_COOKIE: &str = "remixstid";

static RE_VIDEO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:video|clip)(-?\d+)_(\d+)(?:[_/]([0-9a-f]+))?$").unwrap());
static RE_Z: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:video|clip)(-?\d+)_(\d+)(?:/([0-9a-f]+))?").unwrap());

/// A video, by its owner and its own number, with the hash a private share carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoRef {
    pub owner: i64,
    pub id: u64,
    pub hash: Option<String>,
}

impl VideoRef {
    fn key(&self) -> String {
        format!("{}_{}", self.owner, self.id)
    }
}

fn is_vk_host(host: &str) -> bool {
    const HOSTS: [&str; 5] = ["vk.com", "vk.ru", "vkvideo.ru", "vkontakte.ru", "vk.cc"];
    HOSTS
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

fn query(url: &Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
        .filter(|v| !v.is_empty())
}

pub fn parse_link(url: &Url) -> Option<VideoRef> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !is_vk_host(&host) {
        return None;
    }
    let from_caps = |caps: regex::Captures| -> Option<VideoRef> {
        Some(VideoRef {
            owner: caps[1].parse().ok()?,
            id: caps[2].parse().ok()?,
            hash: caps.get(3).map(|m| m.as_str().to_string()),
        })
    };
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    if segments.first() == Some(&"video_ext.php") {
        return Some(VideoRef {
            owner: query(url, "oid")?.parse().ok()?,
            id: query(url, "id")?.parse().ok()?,
            hash: query(url, "hash"),
        });
    }
    if let Some(z) = query(url, "z")
        && let Some(caps) = RE_Z.captures(&z)
    {
        return from_caps(caps);
    }
    let joined = segments.join("/");
    if let Some(caps) = RE_VIDEO.captures(&joined) {
        return from_caps(caps).map(|mut video| {
            video.hash = video.hash.or_else(|| query(url, "list"));
            video
        });
    }
    None
}

/// The player parameters the site's answer carries, when it carries any.
pub fn player_params(payload: &Value) -> Option<&Value> {
    payload["payload"]
        .as_array()?
        .get(1)?
        .as_array()?
        .iter()
        .find_map(|item| {
            let params = item.get("player")?.get("params")?;
            params.as_array()?.first()
        })
}

/// The message the site's answer refuses with, when it refuses.
pub fn refusal(payload: &Value) -> Option<String> {
    let list = payload["payload"].as_array()?;
    let code = match list.first()? {
        Value::String(s) => s.parse::<i64>().ok()?,
        Value::Number(n) => n.as_i64()?,
        _ => return None,
    };
    if code == 0 {
        return None;
    }
    let message = list
        .get(1)?
        .as_array()?
        .first()
        .and_then(|m| m.as_str())
        .map(|m| m.trim().trim_matches('"').to_string())
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| format!("the player answered code {code}"));
    Some(message)
}

fn absolute(text: &str) -> Option<Url> {
    let text = text.trim();
    if text.is_empty() || text.starts_with('/') && !text.starts_with("//") {
        return Url::parse(SITE).ok()?.join(text).ok();
    }
    if text.starts_with("//") {
        Url::parse(&format!("https:{text}")).ok()
    } else {
        Url::parse(text).ok()
    }
}

pub struct VkResolver {
    http: Http,
}

impl VkResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    fn logged_in(&self) -> bool {
        self.http.jar(PLATFORM).get(SESSION_COOKIE).is_some()
    }

    /// Visits the site once so the jar holds the visitor cookies the player request wants.
    async fn visit(&self) -> Result<(), ResolveError> {
        if self.http.jar(PLATFORM).get(VISITOR_COOKIE).is_some() {
            return Ok(());
        }
        let response = self
            .http
            .get(Url::parse(SITE).expect("valid"))
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .send()
            .await?;
        let _ = response.bytes_up_to(64 * 1024).await?;
        Ok(())
    }

    async fn player(&self, video: &VideoRef, origin: &Url) -> Result<Value, ResolveError> {
        let key = video.key();
        let mut fields: Vec<(&str, &str)> = vec![
            ("act", "show"),
            ("al", "1"),
            ("module", "direct"),
            ("video", &key),
        ];
        if let Some(hash) = &video.hash {
            fields.push(("list", hash));
        }
        let response = self
            .http
            .post(Url::parse(PLAYER_API).expect("valid"))
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("x-requested-with", "XMLHttpRequest")
            .header("origin", "https://vk.com")
            .header("referer", &format!("{SITE}video{key}"))
            .form(&fields)
            .send()
            .await?;
        let status = response.status;
        if status.as_u16() == 429 {
            return Err(ResolveError::RateLimited(origin.clone()));
        }
        if !status.is_success() {
            return Err(ResolveError::unavailable(
                origin,
                format!("the player answered HTTP {status}"),
            ));
        }
        let content_type = response.content_type().map(str::to_owned);
        let bytes = response.bytes(MAX_PAGE).await?;
        let text = decode_body(&bytes, content_type.as_deref());
        let body = text.trim_start();
        let body = body.strip_prefix("<!--").unwrap_or(body);
        serde_json::from_str(body)
            .map_err(|e| ResolveError::malformed(origin, format!("player JSON: {e}")))
    }

    fn refuse(&self, message: String, origin: &Url) -> ResolveError {
        let lower = message.to_ascii_lowercase();
        if lower.contains("deleted") || lower.contains("not found") || lower.contains("удален")
        {
            ResolveError::NotFound(origin.clone())
        } else if (lower.contains("restricted")
            || lower.contains("private")
            || lower.contains("access")
            || lower.contains("log in")
            || lower.contains("sign in")
            || lower.contains("доступ"))
            && !self.logged_in()
        {
            ResolveError::login_required(origin, PLATFORM, message)
        } else {
            ResolveError::unavailable(origin, message)
        }
    }
}

/// The characters Windows-1251 puts at 0x80 through 0xBF; the rest of the upper half is
/// the Cyrillic block in order.
const CP1251_HIGH: [char; 64] = [
    '\u{0402}', '\u{0403}', '\u{201A}', '\u{0453}', '\u{201E}', '\u{2026}', '\u{2020}', '\u{2021}',
    '\u{20AC}', '\u{2030}', '\u{0409}', '\u{2039}', '\u{040A}', '\u{040C}', '\u{040B}', '\u{040F}',
    '\u{0452}', '\u{2018}', '\u{2019}', '\u{201C}', '\u{201D}', '\u{2022}', '\u{2013}', '\u{2014}',
    '\u{FFFD}', '\u{2122}', '\u{0459}', '\u{203A}', '\u{045A}', '\u{045C}', '\u{045B}', '\u{045F}',
    '\u{00A0}', '\u{040E}', '\u{045E}', '\u{0408}', '\u{00A4}', '\u{0490}', '\u{00A6}', '\u{00A7}',
    '\u{0401}', '\u{00A9}', '\u{0404}', '\u{00AB}', '\u{00AC}', '\u{00AD}', '\u{00AE}', '\u{0407}',
    '\u{00B0}', '\u{00B1}', '\u{0406}', '\u{0456}', '\u{0491}', '\u{00B5}', '\u{00B6}', '\u{00B7}',
    '\u{0451}', '\u{2116}', '\u{0454}', '\u{00BB}', '\u{0458}', '\u{0405}', '\u{0455}', '\u{0457}',
];

/// Decodes Windows-1251, the encoding the site still answers in.
pub fn decode_cp1251(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| match b {
            0x00..=0x7F => b as char,
            0x80..=0xBF => CP1251_HIGH[(b - 0x80) as usize],
            0xC0..=0xFF => char::from_u32(0x0410 + u32::from(b - 0xC0)).unwrap_or('\u{FFFD}'),
        })
        .collect()
}

/// A body as text: UTF-8 when it is, Windows-1251 when the content type says so or the
/// bytes are not UTF-8.
pub fn decode_body(bytes: &[u8], content_type: Option<&str>) -> String {
    let declared_1251 = content_type
        .map(|t| t.to_ascii_lowercase())
        .is_some_and(|t| t.contains("1251"));
    match std::str::from_utf8(bytes) {
        Ok(text) if !declared_1251 || text.is_ascii() => text.to_string(),
        Ok(text) if declared_1251 => {
            // Declared 1251 yet valid UTF-8 with non-ASCII bytes: UTF-8 text it is.
            text.to_string()
        }
        _ => decode_cp1251(bytes),
    }
}

/// The variants a player's parameters list: MP4 files by height, HLS and DASH
/// manifests, and the live stream of a broadcast.
pub fn variants_of(params: &Value, duration: Option<Duration>, live: bool) -> Vec<Variant> {
    let headers = vec![("referer".to_string(), SITE.to_string())];
    let mut variants = Vec::new();
    for height in [144u32, 240, 360, 480, 720, 1080, 1440, 2160] {
        let Some(url) = params[format!("url{height}")].as_str().and_then(absolute) else {
            continue;
        };
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(Container::Mp4);
        v.video = Some(VideoCodec::H264);
        v.audio = Some(AudioCodec::Aac);
        v.height = Some(height);
        v.duration = duration;
        v.format_id = Some(format!("url{height}"));
        v.label = Some(format!("{height}p"));
        v.headers = headers.clone();
        variants.push(v);
    }
    let manifests: [(&str, VariantKind); 9] = [
        ("hls", VariantKind::Hls),
        ("hls_fmp4", VariantKind::Hls),
        ("hls_ondemand", VariantKind::Hls),
        ("dash_sep", VariantKind::Dash),
        ("dash_webm", VariantKind::Dash),
        ("dash_webm_av1", VariantKind::Dash),
        ("dash_ondemand", VariantKind::Dash),
        ("hls_live", VariantKind::Hls),
        ("dash_live", VariantKind::Dash),
    ];
    for (key, kind) in manifests {
        let Some(url) = params[key].as_str().and_then(absolute) else {
            continue;
        };
        if variants.iter().any(|v| v.url == url) {
            continue;
        }
        let mut v = Variant::new(url, kind);
        v.duration = duration;
        v.live = live || key.ends_with("_live");
        v.format_id = Some(key.to_string());
        v.headers = headers.clone();
        if key.contains("webm") {
            v.container = Some(Container::Webm);
            v.video = Some(if key.ends_with("av1") {
                VideoCodec::Av1
            } else {
                VideoCodec::Vp9
            });
        }
        variants.push(v);
    }
    if let Some(url) = params["live_mp4"].as_str().and_then(absolute) {
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(Container::Mp4);
        v.live = true;
        v.format_id = Some("live_mp4".into());
        v.headers = headers.clone();
        variants.push(v);
    }
    variants
}

#[async_trait]
impl Resolver for VkResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "VK",
            hosts: &["vk.com", "vk.ru", "vkvideo.ru", "vkontakte.ru"],
            features: &["videos", "clips", "embeds", "live", "private share links"],
            formats: &["mp4", "hls", "dash"],
            session: SessionSupport::Optional,
            examples: &[
                "https://vk.com/video-77521_162222515",
                "https://vk.com/video_ext.php?oid=-116782009&id=456239250&hash=4fa87c0d3d44c087",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let video = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        self.visit().await?;
        let mut payload = self.player(&video, url).await?;
        if refusal(&payload).is_some_and(|m| m.to_ascii_lowercase().contains("unknown error")) {
            // The visitor cookies were not taken; visit again and ask once more.
            self.http.with_jar(PLATFORM, |jar| {
                let doomed: Vec<(String, String)> = jar
                    .cookies()
                    .iter()
                    .filter(|c| c.name == VISITOR_COOKIE)
                    .map(|c| (c.name.clone(), c.domain.clone()))
                    .collect();
                for (name, domain) in doomed {
                    jar.remove(&name, &domain);
                }
            });
            self.visit().await?;
            payload = self.player(&video, url).await?;
        }
        if let Some(message) = refusal(&payload) {
            return Err(self.refuse(message, url));
        }
        let params = player_params(&payload)
            .ok_or_else(|| ResolveError::malformed(url, "the player answer has no parameters"))?;
        let live = matches!(params["live"].as_str(), Some("1") | Some("true"))
            || params["live"].as_i64().is_some_and(|l| l != 0)
            || params["live_mp4"].is_string()
            || params["hls_live"].is_string();
        let duration = params["duration"]
            .as_str()
            .and_then(|d| d.parse::<f64>().ok())
            .or_else(|| params["duration"].as_f64())
            .filter(|d| *d > 0.0)
            .map(Duration::from_secs_f64);
        let variants = variants_of(params, duration, live);
        if variants.is_empty() {
            return Err(if live {
                ResolveError::unavailable(url, "the broadcast has not started")
            } else {
                ResolveError::NotFound(url.clone())
            });
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(video.key());
        resolved.title = params["md_title"].as_str().and_then(clean_title);
        resolved.description = params["description"].as_str().and_then(clean_title);
        resolved.uploader = params["md_author"].as_str().and_then(clean_title);
        resolved.uploader_url = params["author_href"].as_str().and_then(absolute);
        resolved.uploaded_at = params["date"]
            .as_i64()
            .or_else(|| params["date"].as_str().and_then(|d| d.parse().ok()))
            .and_then(|t| Timestamp::from_second(t).ok());
        resolved.duration = duration;
        resolved.thumbnail = ["first_frame_1280", "first_frame_800", "jpg", "thumb"]
            .iter()
            .find_map(|k| params[k].as_str())
            .and_then(absolute)
            .filter(|u| !u.path().ends_with("video_l.png"));
        resolved.webpage_url = Url::parse(&format!("{SITE}video{}", video.key())).ok();
        resolved.live = live;
        resolved.clip = timestamp_hint(url).map(|start| ClipRange { start, end: None });
        resolved.subtitles = params["subtitles"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|track| {
                Some(SubtitleTrack {
                    url: absolute(track["url"].as_str()?)?,
                    language: track["lang"]
                        .as_str()
                        .or_else(|| track["language"].as_str())
                        .unwrap_or("und")
                        .to_string(),
                    name: track["title"].as_str().map(String::from),
                    format: SubtitleFormat::Vtt,
                    auto: track["auto"].as_bool().unwrap_or(false),
                    headers: Vec::new(),
                })
            })
            .collect();
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        if !self.logged_in() {
            return Ok(SessionCheck::LoggedOut);
        }
        let settings = Url::parse(&format!("{SITE}settings")).expect("valid");
        let response = self
            .http
            .get(settings.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .follow_redirects(false)
            .send()
            .await?;
        if !response.status.is_success() {
            return Ok(SessionCheck::LoggedOut);
        }
        let html = response.text(MAX_PAGE).await?;
        let id = super::page::between(&html, "\"id\":", ",")
            .map(str::trim)
            .and_then(|id| id.parse::<u64>().ok())
            .filter(|id| *id > 0);
        Ok(match id {
            Some(id) => SessionCheck::LoggedIn {
                account: super::page::between(&html, "\"first_name\":\"", "\"")
                    .and_then(clean_title)
                    .map(|name| format!("{name} (id{id})"))
                    .unwrap_or_else(|| format!("id{id}")),
            },
            None => SessionCheck::LoggedOut,
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

    fn exchange(
        method: &str,
        url: &str,
        status: u16,
        body: &str,
        headers: &[(&str, &str)],
    ) -> Exchange {
        let mut all: Vec<(String, String)> = vec![("content-type".into(), "text/html".into())];
        all.extend(headers.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        Exchange {
            request: RecordedRequest {
                method: method.into(),
                url: url.into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status,
                url: url.into(),
                headers: all,
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    fn home() -> Exchange {
        exchange(
            "GET",
            "https://vk.com/",
            200,
            "<html></html>",
            &[("set-cookie", "remixstid=123_abc; Domain=.vk.com; Path=/")],
        )
    }

    fn payload(params: Value) -> String {
        let answer = json!({"payload": ["0", ["ProtivoGunz - Хуёвая песня", "<div>box</div>", "addTemplates({});", "<div>info</div>",
            {"lang": {}, "player": {"params": [params]}}]]});
        format!("<!--{answer}")
    }

    fn params() -> Value {
        json!({
            "vid": "162222515", "oid": "-77521", "md_title": "ProtivoGunz - Хуёвая песня", "md_author": "Noize MC",
            "author_href": "/noizemc", "duration": "195", "date": 1379855916, "description": "from the album",
            "jpg": "https://sun9-55.userapi.com/c840729/v840729660/45fcc/VCSnkc3l4C4.jpg",
            "url144": "https://vkvd686.okcdn.ru/?expires=1&id=144", "url240": "https://vkvd686.okcdn.ru/?expires=1&id=240",
            "url720": "https://vkvd686.okcdn.ru/?expires=1&id=720",
            "dash_sep": "https://vkvd686.okcdn.ru/?expires=1&ct=6&type=4", "hls": "https://vkvd686.okcdn.ru/video.m3u8?cmd=videoPlayerCdn&expires=1",
            "dash_webm_av1": "https://vkvd686.okcdn.ru/?expires=1&ct=6&type=5",
            "subtitles": [{"url": "//vk.com/subs/ru.vtt", "lang": "ru", "title": "Русский"}]
        })
    }

    #[test]
    fn links_of_every_shape_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let video = VideoRef {
            owner: -77521,
            id: 162222515,
            hash: None,
        };
        assert_eq!(
            link("https://vk.com/video-77521_162222515"),
            Some(video.clone())
        );
        assert_eq!(
            link("https://vkvideo.ru/video-77521_162222515?t=5s"),
            Some(video.clone())
        );
        assert_eq!(
            link("https://m.vk.com/video-77521_162222515"),
            Some(video.clone())
        );
        assert_eq!(
            link("https://vk.com/clip-77521_162222515"),
            Some(video.clone())
        );
        assert_eq!(
            link("https://vk.com/video?z=video-77521_162222515%2Fpl_cat_trends"),
            Some(video.clone())
        );
        assert_eq!(
            link(
                "https://vk.com/video_ext.php?oid=-116782009&id=456239250&hash=4fa87c0d3d44c087&hd=1"
            ),
            Some(VideoRef {
                owner: -116782009,
                id: 456239250,
                hash: Some("4fa87c0d3d44c087".into())
            })
        );
        assert_eq!(
            link("https://vk.com/video-77521_162222515?list=ln-abc"),
            Some(VideoRef {
                hash: Some("ln-abc".into()),
                ..video
            })
        );
        assert_eq!(link("https://vk.com/noizemc"), None);
        assert_eq!(link("https://example.com/video-1_2"), None);
    }

    #[test]
    fn bodies_in_windows_1251_are_decoded() {
        let bytes: Vec<u8> = b"ProtivoGunz - "
            .iter()
            .copied()
            .chain([
                0xD5, 0xF3, 0xB8, 0xE2, 0xE0, 0xFF, 0x20, 0xEF, 0xE5, 0xF1, 0xED, 0xFF,
            ])
            .collect();
        assert_eq!(
            decode_body(&bytes, Some("text/html; charset=windows-1251")),
            "ProtivoGunz - Хуёвая песня"
        );
        assert_eq!(decode_body(&bytes, None), "ProtivoGunz - Хуёвая песня");
        assert_eq!(
            decode_body(
                "уже utf-8".as_bytes(),
                Some("text/html; charset=windows-1251")
            ),
            "уже utf-8"
        );
        assert_eq!(decode_body(b"plain", None), "plain");
        assert_eq!(decode_cp1251(&[0xA8, 0xB8, 0x85, 0x98]), "Ёё…\u{FFFD}");
    }

    #[tokio::test]
    async fn videos_resolve_through_the_player_request() {
        let mut fixture = Fixture::new("vk", None);
        fixture.exchanges.push(home());
        fixture
            .exchanges
            .push(exchange("POST", PLAYER_API, 200, &payload(params()), &[]));
        let http = Http::replay(fixture);
        let resolver = VkResolver::new(http.clone());
        let url = Url::parse("https://vk.com/video-77521_162222515?t=1m").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("-77521_162222515"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("ProtivoGunz - Хуёвая песня")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("Noize MC"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://vk.com/noizemc"
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(195)));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.clip.unwrap().start, Duration::from_secs(60));
        assert!(!resolved.live);
        assert_eq!(resolved.variants.len(), 6);
        let files: Vec<_> = resolved
            .variants
            .iter()
            .filter(|v| v.kind == VariantKind::File)
            .collect();
        assert_eq!(files.len(), 3);
        assert_eq!(files[2].height, Some(720));
        assert_eq!(files[2].label.as_deref(), Some("720p"));
        let hls = resolved
            .variants
            .iter()
            .find(|v| v.kind == VariantKind::Hls)
            .unwrap();
        assert!(hls.url.as_str().contains("video.m3u8"));
        let av1 = resolved
            .variants
            .iter()
            .find(|v| v.format_id.as_deref() == Some("dash_webm_av1"))
            .unwrap();
        assert_eq!(av1.video, Some(VideoCodec::Av1));
        assert_eq!(av1.kind, VariantKind::Dash);
        assert_eq!(resolved.subtitles.len(), 1);
        assert_eq!(
            resolved.subtitles[0].url.as_str(),
            "https://vk.com/subs/ru.vtt"
        );
        assert_eq!(
            http.jar(PLATFORM).get(VISITOR_COOKIE).unwrap().value,
            "123_abc"
        );
    }

    #[tokio::test]
    async fn refusals_are_told_apart_and_unknown_errors_retry_with_fresh_cookies() {
        let deleted = json!({"payload": ["8", ["\"This video has been deleted and is no longer available.\"", "false", "\"\""]]});
        let restricted = json!({"payload": ["8", ["\"Access restricted\"", "false", "\"\""]]});
        let unknown = json!({"payload": ["8", ["\"Unknown error\"", "false", "\"\""]]});
        let mut fixture = Fixture::new("vk", None);
        fixture.exchanges.push(home());
        fixture.exchanges.push(exchange(
            "POST",
            PLAYER_API,
            200,
            &format!("<!--{deleted}"),
            &[],
        ));
        fixture.exchanges.push(exchange(
            "POST",
            PLAYER_API,
            200,
            &format!("<!--{restricted}"),
            &[],
        ));
        fixture.exchanges.push(exchange(
            "POST",
            PLAYER_API,
            200,
            &format!("<!--{unknown}"),
            &[],
        ));
        fixture.exchanges.push(home());
        fixture
            .exchanges
            .push(exchange("POST", PLAYER_API, 200, &payload(params()), &[]));
        let resolver = VkResolver::new(Http::replay(fixture));
        let url = Url::parse("https://vk.com/video-34249_456240180").unwrap();
        assert!(matches!(
            resolver.resolve(&url).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
        assert!(matches!(
            resolver.resolve(&url).await.unwrap_err(),
            ResolveError::LoginRequired { .. }
        ));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.uploader.as_deref(), Some("Noize MC"));
    }

    #[tokio::test]
    async fn live_streams_are_marked() {
        let mut live = params();
        live["live"] = json!("1");
        live["hls_live"] = json!("https://live.vk.com/stream.m3u8");
        live["live_mp4"] = json!("https://live.vk.com/stream.mp4");
        for key in [
            "url144",
            "url240",
            "url720",
            "dash_sep",
            "hls",
            "dash_webm_av1",
        ] {
            live.as_object_mut().unwrap().remove(key);
        }
        let mut fixture = Fixture::new("vk", None);
        fixture.exchanges.push(home());
        fixture
            .exchanges
            .push(exchange("POST", PLAYER_API, 200, &payload(live), &[]));
        let resolver = VkResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://vk.com/video-24136539_456241101").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert!(resolved.live);
        assert_eq!(resolved.variants.len(), 2);
        assert!(resolved.variants.iter().all(|v| v.live));
        assert_eq!(resolved.variants[0].kind, VariantKind::Hls);
    }
}
