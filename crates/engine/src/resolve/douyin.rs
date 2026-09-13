//! Douyin videos, through the web API when a browser's cookies are stored and through
//! the share page the app renders for visitors otherwise, with `v.douyin.com` short
//! links unwrapped. The share host answers some visits with the page's frame and no
//! video and others with the video, for one client and one link alike, so a visit that
//! brought no video is repeated; a client that asks too often is answered with a
//! JavaScript challenge instead of a page.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    ClipRange, MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionCheck,
    SessionSupport, Variant, VariantKind, clean_title, fetch, timestamp_hint,
};
use crate::http::{BROWSER_UA, Http, MOBILE_UA};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "douyin";
const SITE: &str = "https://www.douyin.com/";
const DETAIL_API: &str = "https://www.douyin.com/aweme/v1/web/aweme/detail/";
const SELF_API: &str = "https://www.douyin.com/aweme/v1/web/user/profile/self/";
const SHARE_PAGE: &str = "https://www.iesdouyin.com/share/video/";
/// The cookie a logged-in douyin.com session carries.
const SESSION_COOKIE: &str = "sessionid";
/// The cookies a browser visit leaves, which the web API answers to.
const BROWSER_COOKIES: [&str; 3] = ["ttwid", "s_v_web_id", "msToken"];
/// The script the share host answers with, in place of the page, once a client has asked
/// too often: a JavaScript challenge the client is expected to run before it is served
/// again.
const WAF_CHALLENGE: &str = "waf-jschallenge";
/// How many times the share page is visited for one video before its answers without a
/// video are taken as final.
const SHARE_VISITS: usize = 3;
/// The query the web app sends with every API request.
const WEB_QUERY: [(&str, &str); 22] = [
    ("device_platform", "webapp"),
    ("aid", "6383"),
    ("channel", "channel_pc_web"),
    ("pc_client_type", "1"),
    ("version_code", "190500"),
    ("version_name", "19.5.0"),
    ("cookie_enabled", "true"),
    ("screen_width", "1920"),
    ("screen_height", "1080"),
    ("browser_language", "en-US"),
    ("browser_platform", "Win32"),
    ("browser_name", "Chrome"),
    ("browser_version", "131.0.0.0"),
    ("browser_online", "true"),
    ("engine_name", "Blink"),
    ("engine_version", "131.0.0.0"),
    ("os_name", "Windows"),
    ("os_version", "10"),
    ("cpu_core_num", "8"),
    ("device_memory", "8"),
    ("platform", "PC"),
    ("downlink", "10"),
];

static RE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{15,}$").unwrap());

/// What a link names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Video(String),
    /// A `v.douyin.com` short link.
    Short(Url),
}

fn query(url: &Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
        .filter(|v| !v.is_empty())
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
    if host == "v.douyin.com" {
        return (!segments.is_empty()).then(|| Link::Short(url.clone()));
    }
    let douyin = host == "douyin.com"
        || host.ends_with(".douyin.com")
        || host == "iesdouyin.com"
        || host.ends_with(".iesdouyin.com");
    if !douyin {
        return None;
    }
    if let Some(id) = query(url, "modal_id").filter(|id| RE_ID.is_match(id)) {
        return Some(Link::Video(id));
    }
    match segments.as_slice() {
        ["video" | "note", id, ..] | ["share", "video" | "note", id, ..] if RE_ID.is_match(id) => {
            Some(Link::Video(id.to_string()))
        }
        _ => None,
    }
}

pub struct DouyinResolver {
    http: Http,
}

impl DouyinResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    fn logged_in(&self) -> bool {
        self.http.jar(PLATFORM).get(SESSION_COOKIE).is_some()
    }

    /// Whether a browser's cookies are stored: what the web API answers to.
    fn has_browser_cookies(&self) -> bool {
        let jar = self.http.jar(PLATFORM);
        BROWSER_COOKIES.iter().any(|name| jar.get(name).is_some())
    }

    /// The video's record through the web API; `None` when the API keeps quiet, as it
    /// does without a browser's cookies.
    async fn detail(&self, id: &str, origin: &Url) -> Result<Option<Value>, ResolveError> {
        let mut url = Url::parse(DETAIL_API).expect("valid");
        url.query_pairs_mut()
            .append_pair("aweme_id", id)
            .extend_pairs(WEB_QUERY);
        let headers = [
            ("referer".to_string(), format!("{SITE}video/{id}")),
            (
                "accept".to_string(),
                "application/json, text/plain, */*".to_string(),
            ),
        ];
        let fetched = fetch(&self.http, &url, PLATFORM, BROWSER_UA, &headers, MAX_PAGE).await?;
        match fetched.status.as_u16() {
            200..=299 => {}
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            404 => return Err(ResolveError::NotFound(origin.clone())),
            _ => return Ok(None),
        }
        if fetched.body.is_empty() {
            return Ok(None);
        }
        let value = fetched.json(origin)?;
        let detail = &value["aweme_detail"];
        if detail.is_null() {
            let code = value["status_code"].as_i64().unwrap_or(0);
            let message = value["status_msg"]
                .as_str()
                .filter(|m| !m.is_empty())
                .map(String::from);
            return Err(match (code, message) {
                (0, None) => ResolveError::NotFound(origin.clone()),
                (_, Some(message)) => ResolveError::unavailable(origin, message),
                (code, None) => {
                    ResolveError::unavailable(origin, format!("the API answered status {code}"))
                }
            });
        }
        Ok(Some(detail.clone()))
    }

    /// The video's record from the share page the app renders for visitors: the page a
    /// short link landed on, or the plain share page.
    async fn share(
        &self,
        id: &str,
        landing: Option<&Url>,
        had_cookies: bool,
        origin: &Url,
    ) -> Result<Value, ResolveError> {
        let url = match landing {
            Some(landing) => landing.clone(),
            None => Url::parse(&format!("{SHARE_PAGE}{id}/")).expect("valid"),
        };
        // The host answers some visits with the page's frame and no video, and the
        // same visit again with the video, whether or not it hands out cookies; a page
        // without its router data at all is the frame it serves when it skipped
        // rendering. Either is asked again, up to `SHARE_VISITS` times in all.
        let mut info = Value::Null;
        for _ in 0..SHARE_VISITS {
            let fetched = fetch(&self.http, &url, PLATFORM, MOBILE_UA, &[], MAX_PAGE).await?;
            match fetched.status.as_u16() {
                200..=299 => {}
                404 | 410 => return Err(ResolveError::NotFound(origin.clone())),
                429 => return Err(ResolveError::RateLimited(origin.clone())),
                status => {
                    return Err(ResolveError::unavailable(
                        origin,
                        format!("the share page answered HTTP {status}"),
                    ));
                }
            }
            let html = fetched.text();
            if html.contains(WAF_CHALLENGE) {
                return Err(ResolveError::RateLimited(origin.clone()));
            }
            let data = html.find("_ROUTER_DATA").and_then(|at| {
                let rest = &html[at..];
                let assign = rest.find('=')?;
                super::page::leading_json(&rest[assign + 1..]).map(|(v, _)| v)
            });
            let page = data.as_ref().and_then(|data| {
                data["loaderData"].as_object().and_then(|map| {
                    map.iter()
                        .find(|(key, _)| {
                            key.starts_with("video_(id)") || key.starts_with("note_(id)")
                        })
                        .map(|(_, value)| value.clone())
                })
            });
            info = page
                .map(|page| page["videoInfoRes"].clone())
                .unwrap_or(Value::Null);
            if !info.is_null() {
                break;
            }
        }
        if info.is_null() {
            return Err(self.no_share_data(had_cookies, origin));
        }
        if let Some(item) = info["item_list"].as_array().and_then(|list| list.first()) {
            return Ok(item.clone());
        }
        let filter = &info["filter_list"][0];
        let reason = filter["filter_reason"].as_str().unwrap_or("");
        let message = filter["detail_msg"]
            .as_str()
            .or_else(|| filter["notice"].as_str())
            .unwrap_or(reason)
            .to_string();
        Err(
            if reason.contains("self_see")
                || reason.contains("private")
                || reason.contains("friend")
            {
                ResolveError::unavailable(
                    origin,
                    format!("the video is not public ({reason}): {message}"),
                )
            } else if message.is_empty() || message.contains("删除") || message.contains("不存在")
            {
                ResolveError::NotFound(origin.clone())
            } else {
                ResolveError::unavailable(origin, message)
            },
        )
    }

    /// The share page carried no video: Douyin shows visitors from some places nothing
    /// at all, and then only a browser's cookies get an answer. `had_cookies` says whether
    /// a browser's cookies were stored before the request, since the share page leaves
    /// cookies of its own.
    fn no_share_data(&self, had_cookies: bool, origin: &Url) -> ResolveError {
        if had_cookies {
            ResolveError::unavailable(
                origin,
                "neither the web API nor the share page gave the video",
            )
        } else {
            ResolveError::login_required(
                origin,
                PLATFORM,
                "the share page hands visitors no video here; the web API answers to a browser's cookies",
            )
        }
    }
}

/// A play address without the watermark the share page's addresses carry.
fn clean_play_url(raw: &str) -> Option<Url> {
    let text = raw.trim();
    if text.is_empty() {
        return None;
    }
    let text = text.replace("/playwm/", "/play/");
    let text = if text.starts_with("//") {
        format!("https:{text}")
    } else {
        text
    };
    Url::parse(&text).ok()
}

fn codec_of(info: &Value) -> VideoCodec {
    let h265 = info["is_h265"].as_i64() == Some(1)
        || info["is_h265"].as_bool() == Some(true)
        || info["gear_name"]
            .as_str()
            .is_some_and(|g| g.contains("h265") || g.contains("hevc"))
        || info["format"]
            .as_str()
            .is_some_and(|f| f.contains("265") || f.contains("hevc"));
    let bytevc1 = info["is_bytevc1"].as_i64() == Some(1)
        || info["codec_type"]
            .as_str()
            .is_some_and(|c| c.contains("bytevc1"));
    if h265 {
        VideoCodec::H265
    } else if bytevc1 {
        VideoCodec::Other("bytevc1".into())
    } else {
        VideoCodec::H264
    }
}

/// The variants a video record lists: every bit rate entry, and the plain play address
/// when there is none.
pub fn variants_of(item: &Value, user_agent: &str, referer: &str) -> Vec<Variant> {
    let video = &item["video"];
    let duration = video["duration"]
        .as_u64()
        .or_else(|| item["duration"].as_u64())
        .filter(|d| *d > 0)
        .map(Duration::from_millis);
    let headers = vec![
        ("referer".to_string(), referer.to_string()),
        ("user-agent".to_string(), user_agent.to_string()),
    ];
    let mut variants: Vec<Variant> = Vec::new();
    for info in video["bit_rate"].as_array().into_iter().flatten() {
        let Some(url) = info["play_addr"]["url_list"]
            .as_array()
            .and_then(|list| list.iter().rev().find_map(|u| u.as_str()))
            .and_then(clean_play_url)
        else {
            continue;
        };
        if variants.iter().any(|v| v.url == url) {
            continue;
        }
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(Container::Mp4);
        v.video = Some(codec_of(info));
        v.audio = Some(AudioCodec::Aac);
        v.bitrate = info["bit_rate"].as_u64().filter(|b| *b > 0);
        v.size = info["play_addr"]["data_size"].as_u64().filter(|s| *s > 0);
        v.width = info["play_addr"]["width"].as_u64().map(|w| w as u32);
        v.height = info["play_addr"]["height"].as_u64().map(|h| h as u32);
        v.fps = info["FPS"].as_f64().filter(|f| *f > 0.0);
        v.duration = duration;
        v.format_id = info["gear_name"].as_str().map(String::from);
        v.label = info["quality_type"]
            .as_u64()
            .map(|q| format!("quality {q}"));
        v.headers = headers.clone();
        variants.push(v);
    }
    if variants.is_empty() {
        let addresses = [
            "play_addr_h264",
            "play_addr",
            "play_addr_265",
            "download_addr",
        ];
        for key in addresses {
            let Some(url) = video[key]["url_list"]
                .as_array()
                .and_then(|list| list.iter().find_map(|u| u.as_str()))
                .and_then(clean_play_url)
            else {
                continue;
            };
            if variants.iter().any(|v| v.url == url) {
                continue;
            }
            let mut v = Variant::new(url, VariantKind::File);
            v.container = Some(Container::Mp4);
            v.video = Some(if key.ends_with("265") {
                VideoCodec::H265
            } else {
                VideoCodec::H264
            });
            v.audio = Some(AudioCodec::Aac);
            v.width = video[key]["width"]
                .as_u64()
                .or_else(|| video["width"].as_u64())
                .map(|w| w as u32);
            v.height = video[key]["height"]
                .as_u64()
                .or_else(|| video["height"].as_u64())
                .map(|h| h as u32);
            v.size = video[key]["data_size"].as_u64().filter(|s| *s > 0);
            v.bitrate = video["bit_rate"].as_u64().filter(|b| *b > 0);
            v.duration = duration;
            v.format_id = Some(key.to_string());
            v.headers = headers.clone();
            variants.push(v);
        }
    }
    variants
}

/// A video record as either source gives it, turned into a resolution.
fn resolved_of(
    item: &Value,
    id: &str,
    origin: &Url,
    user_agent: &str,
) -> Result<Resolution, ResolveError> {
    if item["images"]
        .as_array()
        .is_some_and(|images| !images.is_empty())
        || item["aweme_type"].as_i64() == Some(68)
    {
        return Err(ResolveError::unavailable(
            origin,
            "a photo post has no video",
        ));
    }
    let variants = variants_of(item, user_agent, SITE);
    if variants.is_empty() {
        return Err(ResolveError::NotFound(origin.clone()));
    }
    let author = &item["author"];
    let nickname = author["nickname"].as_str().unwrap_or("");
    let sec_uid = author["sec_uid"].as_str().unwrap_or("");
    let mut resolved = Resolved::new(PLATFORM);
    resolved.id = item["aweme_id"]
        .as_str()
        .map(String::from)
        .or_else(|| Some(id.to_string()));
    resolved.title = item["desc"]
        .as_str()
        .and_then(clean_title)
        .or_else(|| (!nickname.is_empty()).then(|| format!("Douyin by {nickname}")));
    resolved.description = item["desc"].as_str().and_then(clean_title);
    resolved.uploader = clean_title(nickname);
    resolved.uploader_url = (!sec_uid.is_empty())
        .then(|| Url::parse(&format!("{SITE}user/{sec_uid}")).ok())
        .flatten();
    resolved.uploaded_at = item["create_time"]
        .as_i64()
        .and_then(|t| Timestamp::from_second(t).ok());
    resolved.duration = variants.iter().find_map(|v| v.duration);
    resolved.thumbnail = ["cover", "origin_cover", "dynamic_cover"]
        .iter()
        .find_map(|k| {
            item["video"][k]["url_list"]
                .as_array()
                .and_then(|list| list.iter().find_map(|u| u.as_str()))
        })
        .and_then(|u| Url::parse(u).ok());
    resolved.webpage_url = Url::parse(&format!("{SITE}video/{id}")).ok();
    resolved.clip = timestamp_hint(origin).map(|start| ClipRange { start, end: None });
    resolved.variants = variants;
    Ok(Resolution::from(resolved))
}

#[async_trait]
impl Resolver for DouyinResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Douyin",
            hosts: &["douyin.com", "iesdouyin.com", "v.douyin.com"],
            features: &["videos", "notes", "short links", "share pages"],
            formats: &["mp4"],
            session: SessionSupport::Optional,
            examples: &[
                "https://www.douyin.com/video/6961737553342991651",
                "https://v.douyin.com/L4FJNR3/",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let had_cookies = self.has_browser_cookies();
        let (id, landing) =
            match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
                Link::Video(id) => (id, None),
                Link::Short(short) => {
                    let target = self
                        .http
                        .unwrap_redirects(&short, Some(PLATFORM), MOBILE_UA)
                        .await?;
                    match parse_link(&target) {
                        Some(Link::Video(id)) => (id, Some(target)),
                        _ => return Err(ResolveError::NotFound(url.clone())),
                    }
                }
            };
        if had_cookies && let Some(detail) = self.detail(&id, url).await? {
            return resolved_of(&detail, &id, url, BROWSER_UA);
        }
        let item = self.share(&id, landing.as_ref(), had_cookies, url).await?;
        resolved_of(&item, &id, url, MOBILE_UA)
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        if !self.logged_in() {
            return Ok(SessionCheck::LoggedOut);
        }
        let mut url = Url::parse(SELF_API).expect("valid");
        url.query_pairs_mut().extend_pairs(WEB_QUERY);
        let headers = [("referer".to_string(), SITE.to_string())];
        let fetched = fetch(&self.http, &url, PLATFORM, BROWSER_UA, &headers, MAX_PAGE).await?;
        if !fetched.status.is_success() || fetched.body.is_empty() {
            return Ok(SessionCheck::LoggedOut);
        }
        let value = fetched.json(&url)?;
        Ok(match value["user"]["nickname"].as_str() {
            Some(name) if value["status_code"].as_i64() == Some(0) && !name.is_empty() => {
                SessionCheck::LoggedIn {
                    account: name.to_string(),
                }
            }
            _ => SessionCheck::LoggedOut,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Cookie;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };
    use serde_json::json;

    fn exchange(
        url: &str,
        status: u16,
        content_type: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> Exchange {
        let mut all: Vec<(String, String)> = vec![("content-type".into(), content_type.into())];
        all.extend(headers.iter().map(|(k, v)| (k.to_string(), v.to_string())));
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
                headers: all,
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    fn item() -> Value {
        json!({
            "aweme_id": "7298145681699622182", "desc": "一起来看 #抖音", "create_time": 1699262000,
            "author": {"nickname": "某人", "sec_uid": "MS4wLjABAAAA_sec"},
            "video": {"duration": 15000, "width": 720, "height": 1280,
                "cover": {"url_list": ["https://p3-sign.douyinpic.com/cover.jpeg"]},
                "play_addr": {"url_list": ["https://www.iesdouyin.com/aweme/v1/playwm/?video_id=v0200f&ratio=720p&line=0"], "width": 720, "height": 1280},
                "bit_rate": [
                    {"gear_name": "normal_1080_0", "bit_rate": 2500000, "quality_type": 1, "is_h265": 0, "FPS": 30, "play_addr": {"url_list": ["https://v26-web.douyinvod.com/a.mp4", "https://v3-web.douyinvod.com/a.mp4"], "width": 1080, "height": 1920, "data_size": 4600000}},
                    {"gear_name": "adapt_lower_540_0", "bit_rate": 900000, "quality_type": 20, "is_h265": 1, "play_addr": {"url_list": ["https://v26-web.douyinvod.com/b.mp4"], "width": 540, "height": 960, "data_size": 1700000}}
                ]}
        })
    }

    fn share_page(info: Value) -> String {
        let data = json!({"loaderData": {"video_layout": null, "video_(id)/page": {"videoInfoRes": info}}, "errors": null});
        format!("<html><body><script>window._ROUTER_DATA = {data}</script></body></html>")
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://www.douyin.com/video/7298145681699622182"),
            Some(Link::Video("7298145681699622182".into()))
        );
        assert_eq!(
            link("https://www.douyin.com/note/7298145681699622182"),
            Some(Link::Video("7298145681699622182".into()))
        );
        assert_eq!(
            link("https://www.iesdouyin.com/share/video/7298145681699622182/?region=CN"),
            Some(Link::Video("7298145681699622182".into()))
        );
        assert_eq!(
            link("https://www.douyin.com/discover?modal_id=7298145681699622182"),
            Some(Link::Video("7298145681699622182".into()))
        );
        assert!(matches!(
            link("https://v.douyin.com/iRNBho6u/"),
            Some(Link::Short(_))
        ));
        assert_eq!(link("https://www.douyin.com/user/MS4wLjABAAAA"), None);
        assert_eq!(link("https://www.douyin.com/video/abc"), None);
        assert_eq!(
            clean_play_url("https://www.iesdouyin.com/aweme/v1/playwm/?video_id=v0&line=0")
                .unwrap()
                .as_str(),
            "https://www.iesdouyin.com/aweme/v1/play/?video_id=v0&line=0"
        );
    }

    #[tokio::test]
    async fn visitors_read_the_share_page_with_short_links_unwrapped() {
        let mut fixture = Fixture::new("douyin", None);
        fixture.exchanges.push(exchange(
            "https://v.douyin.com/iRNBho6u/",
            302,
            "text/html",
            "",
            &[(
                "location",
                "https://www.iesdouyin.com/share/video/7298145681699622182/?region=CN&mid=1",
            )],
        ));
        fixture.exchanges.push(exchange(
            "https://www.iesdouyin.com/share/video/7298145681699622182/?region=CN&mid=1",
            200,
            "text/html",
            "<html></html>",
            &[("set-cookie", "ttwid=1%7Cabc; Domain=.iesdouyin.com; Path=/")],
        ));
        fixture.exchanges.push(exchange(
            "https://www.iesdouyin.com/share/video/7298145681699622182/?region=CN&mid=1",
            200,
            "text/html",
            &share_page(json!({"item_list": [item()], "status_code": 0})),
            &[],
        ));
        let resolver = DouyinResolver::new(Http::replay(fixture));
        let url = Url::parse("https://v.douyin.com/iRNBho6u/").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("7298145681699622182"));
        assert_eq!(resolved.title.as_deref(), Some("一起来看 #抖音"));
        assert_eq!(resolved.uploader.as_deref(), Some("某人"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://www.douyin.com/user/MS4wLjABAAAA_sec"
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(15)));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.variants.len(), 2);
        assert_eq!(resolved.variants[0].height, Some(1920));
        assert_eq!(resolved.variants[0].size, Some(4600000));
        assert_eq!(resolved.variants[0].fps, Some(30.0));
        assert_eq!(
            resolved.variants[0].url.as_str(),
            "https://v3-web.douyinvod.com/a.mp4"
        );
        assert_eq!(resolved.variants[1].video, Some(VideoCodec::H265));
        assert_eq!(resolved.variants[0].headers[1].1, MOBILE_UA);
    }

    #[tokio::test]
    async fn empty_share_pages_ask_for_cookies_and_cookies_use_the_api() {
        let mut fixture = Fixture::new("douyin", None);
        for _ in 0..SHARE_VISITS {
            fixture.exchanges.push(exchange(
                "https://www.iesdouyin.com/share/video/7298145681699622182/",
                200,
                "text/html",
                r#"<html><script>window._ROUTER_DATA = {"loaderData":{"video_(id)/page":{"ua":"x","isSpider":false,"webId":"7684788415880300051"}},"errors":null}</script></html>"#,
                &[],
            ));
        }
        let http = Http::replay(fixture);
        let resolver = DouyinResolver::new(http.clone());
        let url = Url::parse("https://www.douyin.com/video/7298145681699622182").unwrap();
        let error = resolver.resolve(&url).await.unwrap_err();
        assert!(
            matches!(error, ResolveError::LoginRequired { .. }),
            "{error}"
        );

        let mut fixture = Fixture::new("douyin", None);
        let mut detail = item();
        detail["video"]["bit_rate"] = json!([]);
        fixture.exchanges.push(exchange(
            DETAIL_API,
            200,
            "application/json",
            &json!({"aweme_detail": detail, "status_code": 0}).to_string(),
            &[],
        ));
        fixture.exchanges.push(exchange(
            DETAIL_API,
            200,
            "application/json",
            &json!({"aweme_detail": null, "status_code": 0}).to_string(),
            &[],
        ));
        fixture.exchanges.push(exchange(
            DETAIL_API,
            200,
            "application/json",
            &json!({"aweme_detail": {"aweme_id": "1", "images": [{"url_list": ["https://p.douyinpic.com/1.jpg"]}], "video": {}}, "status_code": 0}).to_string(),
            &[],
        ));
        let http = Http::replay(fixture);
        http.with_jar(PLATFORM, |jar| {
            jar.insert(Cookie::new("ttwid", "1|abc", ".douyin.com"));
        });
        let resolver = DouyinResolver::new(http);
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(
            resolved.variants[0].url.as_str(),
            "https://www.iesdouyin.com/aweme/v1/play/?video_id=v0200f&ratio=720p&line=0"
        );
        assert_eq!(resolved.variants[0].height, Some(1280));
        assert_eq!(resolved.variants[0].headers[1].1, BROWSER_UA);
        assert!(matches!(
            resolver.resolve(&url).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver.resolve(&url).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("photo")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn filtered_videos_report_the_notice() {
        let mut fixture = Fixture::new("douyin", None);
        fixture.exchanges.push(exchange(
            "https://www.iesdouyin.com/share/video/7298145681699622182/",
            200,
            "text/html",
            &share_page(json!({"item_list": [], "filter_list": [{"aweme_id": "7298145681699622182", "filter_reason": "status_deleted", "detail_msg": "作品已删除", "notice": "作品已删除"}], "status_code": 0})),
            &[],
        ));
        fixture.exchanges.push(exchange(
            "https://www.iesdouyin.com/share/video/7298145681699622183/",
            200,
            "text/html",
            &share_page(json!({"item_list": [], "filter_list": [{"detail_msg": "该作品仅对粉丝可见"}], "status_code": 0})),
            &[],
        ));
        fixture.exchanges.push(exchange(
            "https://www.iesdouyin.com/share/video/7298145681699622184/",
            200,
            "text/html",
            &share_page(json!({"item_list": [], "filter_list": [{"filter_reason": "status_self_see", "notice": "抱歉，作品不见了", "detail_msg": "因作品权限或已被删除，无法观看"}], "status_code": 0})),
            &[],
        ));
        let resolver = DouyinResolver::new(Http::replay(fixture));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.douyin.com/video/7298145681699622182").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(&Url::parse("https://www.douyin.com/video/7298145681699622183").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("粉丝")),
            "{error}"
        );
        let error = resolver
            .resolve(&Url::parse("https://www.douyin.com/video/7298145681699622184").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("status_self_see")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_bare_share_page_is_asked_again() {
        // The first visit answers with the frame and no video, and sets no cookie; the
        // second carries the video.
        let mut fixture = Fixture::new("douyin", None);
        fixture.exchanges.push(exchange(
            "https://www.iesdouyin.com/share/video/7298145681699622182/",
            200,
            "text/html",
            r#"<html><script>window._ROUTER_DATA = {"loaderData":{"video_(id)/page":{"ua":"x","webId":"7684788415880300051"}},"errors":null}</script></html>"#,
            &[],
        ));
        fixture.exchanges.push(exchange(
            "https://www.iesdouyin.com/share/video/7298145681699622182/",
            200,
            "text/html",
            &share_page(json!({"item_list": [item()], "status_code": 0})),
            &[],
        ));
        let http = Http::replay(fixture);
        let resolver = DouyinResolver::new(http.clone());
        let resolved = resolver
            .resolve(&Url::parse("https://www.douyin.com/video/7298145681699622182").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.title.as_deref(), Some("一起来看 #抖音"));
        assert!(http.jar(PLATFORM).is_empty());
    }

    #[tokio::test]
    async fn a_share_page_is_visited_three_times_before_giving_up() {
        let bare = r#"<html><script>window._ROUTER_DATA = {"loaderData":{"video_(id)/page":{"ua":"x","webId":"7684788415880300051"}},"errors":null}</script></html>"#;
        let mut fixture = Fixture::new("douyin", None);
        for _ in 0..2 {
            fixture.exchanges.push(exchange(
                "https://www.iesdouyin.com/share/video/7298145681699622182/",
                200,
                "text/html",
                bare,
                &[],
            ));
        }
        fixture.exchanges.push(exchange(
            "https://www.iesdouyin.com/share/video/7298145681699622182/",
            200,
            "text/html",
            &share_page(json!({"item_list": [item()], "status_code": 0})),
            &[],
        ));
        let resolver = DouyinResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://www.douyin.com/video/7298145681699622182").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.title.as_deref(), Some("一起来看 #抖音"));
    }

    #[tokio::test]
    async fn challenges_are_rate_limits_and_unrendered_pages_are_asked_again() {
        let fallback = r#"<html><head><script id="__edenx_ssr_fallback_reason__" type="application/json">{"reason":"query"}</script></head><body><div id="root"></div></body></html>"#;
        let mut fixture = Fixture::new("douyin", None);
        fixture.exchanges.push(exchange(
            "https://www.iesdouyin.com/share/video/7298145681699622182/",
            200,
            "text/html",
            r#"<html><head><script nonce="argus-csp-token" src="https://lf-waf-js.byted-static.com/obj/waf-jschallenge/out-sha256.js"></script></head><body></body></html>"#,
            &[],
        ));
        fixture.exchanges.push(exchange(
            "https://www.iesdouyin.com/share/video/7298145681699622183/",
            200,
            "text/html",
            fallback,
            &[],
        ));
        fixture.exchanges.push(exchange(
            "https://www.iesdouyin.com/share/video/7298145681699622183/",
            200,
            "text/html",
            &share_page(json!({"item_list": [item()], "status_code": 0})),
            &[],
        ));
        for _ in 0..SHARE_VISITS {
            fixture.exchanges.push(exchange(
                "https://www.iesdouyin.com/share/video/7298145681699622184/",
                200,
                "text/html",
                fallback,
                &[],
            ));
        }
        let resolver = DouyinResolver::new(Http::replay(fixture));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.douyin.com/video/7298145681699622182").unwrap())
                .await
                .unwrap_err(),
            ResolveError::RateLimited(_)
        ));
        let resolved = resolver
            .resolve(&Url::parse("https://www.douyin.com/video/7298145681699622183").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.title.as_deref(), Some("一起来看 #抖音"));
        let error = resolver
            .resolve(&Url::parse("https://www.douyin.com/video/7298145681699622184").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::LoginRequired { reason, .. } if reason.contains("cookies")),
            "{error}"
        );
    }
}
