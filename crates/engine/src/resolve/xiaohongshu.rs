//! Xiaohongshu (RedNote) video notes, from the state the note page renders, with
//! `xhslink.com` share links followed to the note they name. The site opens a note to a
//! visitor through the `xsec_token` its share links carry, for as long as the token
//! lasts; a link without one, or with one that has run out, is turned away, and the
//! refusal is reported as such. A note the site cannot serve sends the visitor to its
//! explore feed with the reason, which is read from there.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use percent_encoding::percent_decode_str;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::page::leading_json;
use super::{
    ClipRange, MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionCheck,
    SessionSupport, Variant, VariantKind, clean_title, timestamp_hint,
};
use crate::http::{BROWSER_UA, Http, RequestBuilder};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "xiaohongshu";
const SITE: &str = "https://www.xiaohongshu.com/";
/// The cookie a logged-in xiaohongshu.com session carries.
const SESSION_COOKIE: &str = "web_session";
/// The headers a browser sends with a page it navigates to; the site sends a request
/// without them to its login page.
const PAGE_HEADERS: [(&str, &str); 2] = [
    (
        "accept",
        "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
    ),
    ("accept-language", "zh-CN,zh;q=0.9,en;q=0.8"),
];
/// How many redirects a share link is followed through for the note it names.
const SHARE_LINK_HOPS: usize = 5;

static RE_NOTE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{24}$").unwrap());

/// What a link names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A note, with the page that names it, whose `xsec_token` the site wants back.
    Note {
        id: String,
        page: Url,
    },
    Short(Url),
}

fn site_host(host: &str) -> bool {
    host == "xiaohongshu.com" || host.ends_with(".xiaohongshu.com")
}

fn on_site(url: &Url) -> bool {
    url.host_str()
        .is_some_and(|h| site_host(&h.to_ascii_lowercase()))
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
    if host == "xhslink.com" || host.ends_with(".xhslink.com") {
        return (!segments.is_empty()).then(|| Link::Short(url.clone()));
    }
    if !site_host(&host) {
        return None;
    }
    let id = match segments.as_slice() {
        ["explore", id] | ["discovery", "item", id] => *id,
        ["user", "profile", _, id] => *id,
        _ => return None,
    };
    RE_NOTE.is_match(id).then(|| Link::Note {
        id: id.to_string(),
        page: url.clone(),
    })
}

/// Whether `url` is the site's fallback for a note it could not serve: the explore feed,
/// with the note named to open over it and the reason it did not load.
pub fn is_fallback(url: &Url, id: &str) -> bool {
    on_site(url)
        && url.path().trim_end_matches('/') == "/explore"
        && query(url, "target_note_id").as_deref() == Some(id)
}

/// The reason a fallback page carries, which the site percent-encodes twice.
pub fn fallback_reason(url: &Url) -> Option<String> {
    query(url, "undertake_note_error")
        .map(|once| percent_decode_str(&once).decode_utf8_lossy().into_owned())
        .filter(|reason| !reason.trim().is_empty())
}

/// The `window.__INITIAL_STATE__` a page carries: JavaScript rather than JSON, with
/// `undefined` where JSON has `null`.
pub fn initial_state(html: &str) -> Option<Value> {
    let at = html.find("__INITIAL_STATE__")?;
    let rest = &html[at..];
    let assign = rest.find('=')?;
    let script_end = rest.find("</script>").unwrap_or(rest.len());
    let body = &rest[assign + 1..script_end];
    let cleaned = body
        .replace(":undefined", ":null")
        .replace(",undefined", ",null");
    leading_json(&cleaned).map(|(value, _)| value)
}

/// The note `id` names in a page's state.
pub fn note_of<'a>(state: &'a Value, id: &str) -> Option<&'a Value> {
    let note = state["note"]["noteDetailMap"][id]["note"].as_object()?;
    Some(&state["note"]["noteDetailMap"][id]["note"]).filter(|_| !note.is_empty())
}

/// The message in the answer a page's own request for the note got, which the state
/// keeps as the API's JSON document in a string when the request failed.
pub fn request_message(info: &Value) -> Option<String> {
    let raw = info["errMsg"].as_str().filter(|m| !m.trim().is_empty())?;
    Some(
        serde_json::from_str::<Value>(raw)
            .ok()
            .and_then(|answer| {
                answer["data"]["msg"]
                    .as_str()
                    .or_else(|| answer["msg"].as_str())
                    .filter(|m| !m.trim().is_empty())
                    .map(String::from)
            })
            .unwrap_or_else(|| raw.to_string()),
    )
}

fn query(url: &Url, key: &str) -> Option<String> {
    url.query_pairs()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
        .filter(|v| !v.is_empty())
}

pub struct XiaohongshuResolver {
    http: Http,
}

impl XiaohongshuResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    fn logged_in(&self) -> bool {
        self.http.jar(PLATFORM).get(SESSION_COOKIE).is_some()
    }

    /// A page request as a browser navigating there makes it, with its redirects left to
    /// the caller.
    fn page(&self, url: Url) -> RequestBuilder {
        let mut request = self
            .http
            .get(url)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("referer", SITE)
            .follow_redirects(false);
        for (name, value) in PAGE_HEADERS {
            request = request.header(name, value);
        }
        request
    }

    /// The site turned the note away, with the error code and message it gave: the login
    /// wall for strangers, or a reason of its own.
    fn refusal(&self, code: Option<i64>, message: Option<String>, origin: &Url) -> ResolveError {
        let message = message.filter(|m| !m.trim().is_empty());
        match (code, message) {
            (Some(300031), message) | (None, message) if !self.logged_in() => {
                ResolveError::login_required(
                    origin,
                    PLATFORM,
                    format!(
                        "{}; the site opens a note to visitors through the xsec_token its share links carry",
                        message.unwrap_or_else(|| "the site turned the visitor away".into())
                    ),
                )
            }
            (Some(300012), _) => ResolveError::NotFound(origin.clone()),
            (code, message) => ResolveError::unavailable(
                origin,
                match (code, message) {
                    (Some(code), Some(message)) => format!("{message} (error {code})"),
                    (Some(code), None) => format!("the site refused the note (error {code})"),
                    (None, Some(message)) => message,
                    (None, None) => "the site refused the note".into(),
                },
            ),
        }
    }

    /// The refusal a `/404` page the site sent the client to spells out in its query.
    fn refused_to(&self, location: &Url, origin: &Url) -> ResolveError {
        let code = query(location, "error_code").and_then(|c| c.parse().ok());
        self.refusal(code, query(location, "error_msg"), origin)
    }

    /// Why a page's state carries no note: the answer the page's own request for it got,
    /// the message the page shows for it, or the reason the fallback page was sent with.
    fn not_served(&self, state: &Value, reason: Option<String>, origin: &Url) -> ResolveError {
        let info = &state["note"]["serverRequestInfo"];
        let code = info["errorCode"].as_i64().unwrap_or(0);
        let message = request_message(info)
            .or_else(|| {
                state["note"]["undertakeNoteError"]
                    .as_str()
                    .filter(|m| !m.trim().is_empty())
                    .map(String::from)
            })
            .or(reason);
        match code {
            -510000 | -510001 | 300012 => ResolveError::NotFound(origin.clone()),
            0 => self.refusal(None, message, origin),
            code => self.refusal(Some(code), message, origin),
        }
    }

    /// What a page's status means when it is not a redirect.
    fn served(&self, status: u16, origin: &Url) -> Result<(), ResolveError> {
        match status {
            200..=299 => Ok(()),
            404 | 410 => Err(ResolveError::NotFound(origin.clone())),
            429 => Err(ResolveError::RateLimited(origin.clone())),
            461 | 471 => Err(self.refusal(None, None, origin)),
            _ => Err(ResolveError::unavailable(
                origin,
                format!("the note page answered HTTP {status}"),
            )),
        }
    }

    /// The state the note page renders, or the state of the fallback page the site sends a
    /// visitor to instead, with the reason that page was sent with.
    async fn note_state(
        &self,
        url: &Url,
        id: &str,
        origin: &Url,
    ) -> Result<(Value, Option<String>), ResolveError> {
        let response = self.page(url.clone()).send().await?;
        let status = response.status.as_u16();
        if !(300..400).contains(&status) {
            self.served(status, origin)?;
            let html = response.text(MAX_PAGE).await?;
            let state = initial_state(&html)
                .ok_or_else(|| ResolveError::malformed(origin, "the note page has no state"))?;
            return Ok((state, None));
        }
        let Some(location) = response.header("location").and_then(|l| url.join(l).ok()) else {
            return Err(self.refusal(None, None, origin));
        };
        if location.path().starts_with("/404") {
            return Err(self.refused_to(&location, origin));
        }
        if location.path().starts_with("/login") {
            return Err(ResolveError::login_required(
                origin,
                PLATFORM,
                "the note page sent this client to log in",
            ));
        }
        if !is_fallback(&location, id) {
            return Err(ResolveError::Redirect(location));
        }
        let reason = fallback_reason(&location);
        let response = self.page(location.clone()).send().await?;
        let status = response.status.as_u16();
        if (300..400).contains(&status) {
            let next = response
                .header("location")
                .and_then(|l| location.join(l).ok());
            if next.as_ref().is_some_and(|l| l.path().starts_with("/404")) {
                return Err(self.refused_to(&next.expect("checked above"), origin));
            }
            if next.as_ref().is_some_and(|l| l.path().starts_with("/login")) {
                return Err(ResolveError::login_required(
                    origin,
                    PLATFORM,
                    "the note page sent this client to log in",
                ));
            }
            return Err(self.refusal(None, reason, origin));
        }
        self.served(status, origin)?;
        let html = response.text(MAX_PAGE).await?;
        let state = initial_state(&html)
            .ok_or_else(|| ResolveError::malformed(origin, "the explore page has no state"))?;
        Ok((state, reason))
    }

    async fn note(&self, id: &str, page: &Url, origin: &Url) -> Result<Resolution, ResolveError> {
        let mut url = Url::parse(&format!("{SITE}explore/{id}")).expect("valid");
        for key in ["xsec_token", "xsec_source"] {
            if let Some(value) = query(page, key) {
                url.query_pairs_mut().append_pair(key, &value);
            }
        }
        if url.query().is_none() {
            url.query_pairs_mut().append_pair("xsec_source", "pc_share");
        }
        let (state, reason) = self.note_state(&url, id, origin).await?;
        let note = note_of(&state, id).ok_or_else(|| self.not_served(&state, reason, origin))?;
        if note["type"].as_str().is_some_and(|t| t != "video") || note["video"].is_null() {
            return Err(ResolveError::unavailable(
                origin,
                "a photo note has no video",
            ));
        }
        let duration = note["video"]["capa"]["duration"]
            .as_f64()
            .filter(|d| *d > 0.0)
            .map(Duration::from_secs_f64);
        let variants = variants_of(&note["video"], duration);
        if variants.is_empty() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        let user = &note["user"];
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.to_string());
        resolved.title = note["title"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| note["desc"].as_str().and_then(clean_title));
        resolved.description = note["desc"].as_str().and_then(clean_title);
        resolved.uploader = user["nickname"].as_str().and_then(clean_title);
        resolved.uploader_url = user["userId"]
            .as_str()
            .and_then(|uid| Url::parse(&format!("{SITE}user/profile/{uid}")).ok());
        resolved.uploaded_at = note["time"]
            .as_i64()
            .and_then(|t| Timestamp::from_millisecond(t).ok());
        resolved.duration = duration.or_else(|| variants.iter().find_map(|v| v.duration));
        resolved.thumbnail = note["imageList"]
            .as_array()
            .and_then(|list| list.first())
            .and_then(|image| {
                image["urlDefault"]
                    .as_str()
                    .or_else(|| image["url"].as_str())
            })
            .and_then(|u| Url::parse(u).ok());
        resolved.webpage_url = Some(url);
        resolved.clip = timestamp_hint(origin).map(|start| ClipRange { start, end: None });
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    /// The note a share link names, followed one redirect at a time: the note page is
    /// then asked for as a browser would ask, rather than wherever it sends a bare
    /// request, and the token the link carries is kept.
    async fn share(&self, short: &Url, origin: &Url) -> Result<Resolution, ResolveError> {
        let mut current = short.clone();
        for _ in 0..SHARE_LINK_HOPS {
            let response = self.page(current.clone()).send().await?;
            let status = response.status.as_u16();
            if !(300..400).contains(&status) {
                return Err(match status {
                    200..=299 => ResolveError::unavailable(
                        origin,
                        "the share link leads to no note; share links expire",
                    ),
                    404 | 410 => ResolveError::NotFound(origin.clone()),
                    429 => ResolveError::RateLimited(origin.clone()),
                    _ => ResolveError::unavailable(
                        origin,
                        format!("the share link answered HTTP {status}"),
                    ),
                });
            }
            let location = response
                .header("location")
                .and_then(|l| current.join(l).ok())
                .ok_or_else(|| ResolveError::malformed(origin, "the share link redirects nowhere"))?;
            match parse_link(&location) {
                Some(Link::Note { id, page }) => return self.note(&id, &page, origin).await,
                Some(Link::Short(next)) => current = next,
                None if on_site(&location) && location.path().starts_with("/login") => {
                    return Err(ResolveError::login_required(
                        origin,
                        PLATFORM,
                        "the share link sent this client to log in",
                    ));
                }
                None if on_site(&location) && location.path().trim_matches('/').is_empty() => {
                    return Err(ResolveError::unavailable(
                        origin,
                        "the share link has expired: the site sends it to its front page",
                    ));
                }
                None => {
                    return Err(ResolveError::unavailable(
                        origin,
                        format!("the share link leads to {location}, which is no note"),
                    ));
                }
            }
        }
        Err(ResolveError::unavailable(
            origin,
            "the share link redirects too many times",
        ))
    }
}

/// The variants a note's video lists: every stream of every codec it was encoded to.
/// A stream's bit rates are in bits per second and its `duration` in milliseconds.
pub fn variants_of(video: &Value, duration: Option<Duration>) -> Vec<Variant> {
    let headers = vec![("referer".to_string(), SITE.to_string())];
    let mut variants: Vec<Variant> = Vec::new();
    let streams = &video["media"]["stream"];
    for (key, codec) in [
        ("h264", VideoCodec::H264),
        ("h265", VideoCodec::H265),
        ("av1", VideoCodec::Av1),
        ("h266", VideoCodec::Other("h266".into())),
    ] {
        for stream in streams[key].as_array().into_iter().flatten() {
            let Some(url) = stream["masterUrl"]
                .as_str()
                .or_else(|| {
                    stream["backupUrls"]
                        .as_array()
                        .and_then(|b| b.iter().find_map(|u| u.as_str()))
                })
                .and_then(|u| Url::parse(u).ok())
            else {
                continue;
            };
            if variants.iter().any(|v| v.url == url) {
                continue;
            }
            let mut v = Variant::new(url, VariantKind::File);
            v.container = Some(Container::Mp4);
            v.video = Some(codec.clone());
            v.audio = Some(match stream["audioCodec"].as_str() {
                Some(name) if name.eq_ignore_ascii_case("aac") || name.is_empty() => {
                    AudioCodec::Aac
                }
                Some(name) => AudioCodec::Other(name.to_ascii_lowercase()),
                None => AudioCodec::Aac,
            });
            v.width = stream["width"].as_u64().map(|w| w as u32);
            v.height = stream["height"].as_u64().map(|h| h as u32);
            v.fps = stream["fps"].as_f64().filter(|f| *f > 0.0);
            v.bitrate = stream["avgBitrate"]
                .as_u64()
                .or_else(|| stream["videoBitrate"].as_u64())
                .filter(|b| *b > 0);
            v.size = stream["size"].as_u64().filter(|s| *s > 0);
            v.duration = stream["duration"]
                .as_u64()
                .filter(|d| *d > 0)
                .map(Duration::from_millis)
                .or(duration);
            v.format_id = stream["qualityType"]
                .as_str()
                .map(|q| format!("{key}-{q}"))
                .or_else(|| Some(key.to_string()));
            v.label = stream["qualityType"].as_str().map(String::from);
            v.headers = headers.clone();
            variants.push(v);
        }
    }
    if variants.is_empty()
        && let Some(url) = video["media"]["videoKey"]
            .as_str()
            .and_then(|key| Url::parse(&format!("https://sns-video-bd.xhscdn.com/{key}")).ok())
    {
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(Container::Mp4);
        v.video = Some(VideoCodec::H264);
        v.audio = Some(AudioCodec::Aac);
        v.duration = duration;
        v.format_id = Some("videoKey".into());
        v.headers = headers;
        variants.push(v);
    }
    variants
}

#[async_trait]
impl Resolver for XiaohongshuResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Xiaohongshu",
            hosts: &["xiaohongshu.com", "xhslink.com"],
            features: &["video notes", "share links"],
            formats: &["mp4"],
            session: SessionSupport::Optional,
            examples: &[
                "https://www.xiaohongshu.com/discovery/item/6a8212d4000000001102138d?xsec_source=app_share&xsec_token=CBuZUrHoqFVTefcIPvLwx9AVA4vQ2aaSRfsQF8dEcBmYE%3D",
                "http://xhslink.com/o/6fj5AEdbejP",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Note { id, page } => self.note(&id, &page, url).await,
            Link::Short(short) => self.share(&short, url).await,
        }
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        if !self.logged_in() {
            return Ok(SessionCheck::LoggedOut);
        }
        let explore = Url::parse(&format!("{SITE}explore")).expect("valid");
        let response = self
            .http
            .get(explore)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .send()
            .await?;
        if !response.is_success() {
            return Ok(SessionCheck::LoggedOut);
        }
        let html = response.text(MAX_PAGE).await?;
        let Some(state) = initial_state(&html) else {
            return Ok(SessionCheck::LoggedOut);
        };
        let user = &state["user"];
        let logged = user["loggedIn"].as_bool() == Some(true)
            || user["userInfo"]["userId"]
                .as_str()
                .is_some_and(|u| !u.is_empty());
        Ok(if logged {
            SessionCheck::LoggedIn {
                account: user["userInfo"]["nickname"]
                    .as_str()
                    .filter(|n| !n.is_empty())
                    .map(String::from)
                    .or_else(|| {
                        user["userInfo"]["userId"]
                            .as_str()
                            .map(|u| format!("user {u}"))
                    })
                    .unwrap_or_else(|| "a Xiaohongshu account".into()),
            }
        } else {
            SessionCheck::LoggedOut
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

    fn exchange(url: &str, status: u16, body: &str, headers: &[(&str, &str)]) -> Exchange {
        let mut all: Vec<(String, String)> = vec![("content-type".into(), "text/html".into())];
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

    const ID: &str = "6411cf99000000001300b6d9";
    const EXPLORE: &str =
        "https://www.xiaohongshu.com/explore/6411cf99000000001300b6d9?xsec_token=CBtok%3D&xsec_source=app_share";
    /// Where the site sends a visitor for a note it could not serve, with the reason
    /// percent-encoded twice: 该内容暂时无法查看.
    const FALLBACK: &str = "https://www.xiaohongshu.com/explore?xsec_token=CBtok%3D&xsec_source=app_share&target_note_id=6411cf99000000001300b6d9&undertake_note_error=%25E8%25AF%25A5%25E5%2586%2585%25E5%25AE%25B9%25E6%259A%2582%25E6%2597%25B6%25E6%2597%25A0%25E6%25B3%2595%25E6%259F%25A5%25E7%259C%258B";

    fn note_page(note: &str) -> String {
        format!(
            r#"<html><script>window.__INITIAL_STATE__={{"global":{{"a":undefined}},"user":{{"loggedIn":false}},"note":{{"noteDetailMap":{{"{ID}":{{"note":{note},"comments":{{"list":[]}}}}}},"serverRequestInfo":{{"errorCode":0}}}}}}</script></html>"#
        )
    }

    /// The fallback page's state: the request the page made for the note failed with
    /// `code` and the API's message, or succeeded with `note` when the page opens it.
    fn fallback_page(code: i64, message: &str, note: Option<Value>, shown: Option<&str>) -> String {
        let info = if code == 0 {
            json!({"state": "success", "errorCode": 0, "errMsg": ""})
        } else {
            let answer = json!({
                "name": "HTTPBizError", "code": code, "status": 200,
                "data": {"code": code, "success": false, "msg": message, "data": null}
            });
            json!({"state": "fail", "errorCode": code, "errMsg": answer.to_string()})
        };
        let mut state = json!({
            "user": {"loggedIn": false},
            "note": {
                "noteDetailMap": {},
                "serverRequestInfo": info,
                "undertakeNoteId": ID,
                "undertakeNoteOpen": true
            }
        });
        state["note"]["noteDetailMap"][ID] =
            json!({"note": note.unwrap_or_else(|| json!({})), "comments": {"list": []}});
        if let Some(shown) = shown {
            state["note"]["undertakeNoteError"] = json!(shown);
        }
        format!(
            r#"<html><script>window.__INITIAL_STATE__={state}</script></html>"#
        )
    }

    fn video_note() -> Value {
        json!({
            "type": "video", "title": "今天的天气", "desc": "今天的天气真好 #天气", "time": 1678900000000_u64,
            "user": {"userId": "5ff1e0a0000000000101d1c5", "nickname": "小红"},
            "imageList": [{"urlDefault": "https://sns-webpic-qc.xhscdn.com/cover.jpg"}],
            "video": {"capa": {"duration": 12}, "media": {"videoId": 1, "stream": {
                "h264": [{"masterUrl": "https://sns-video-bd.xhscdn.com/h264.mp4", "backupUrls": ["https://sns-video-qc.xhscdn.com/h264.mp4"], "width": 1080, "height": 1920, "videoBitrate": 2500000, "avgBitrate": 2300000, "fps": 30, "size": 3456789, "duration": 12345, "videoDuration": 12300, "qualityType": "HD", "audioCodec": "aac"}],
                "h265": [{"masterUrl": "https://sns-video-bd.xhscdn.com/h265.mp4", "width": 1080, "height": 1920, "videoBitrate": 1500000, "qualityType": "HD"}],
                "av1": [], "h266": []
            }}}
        })
    }

    #[test]
    fn links_are_read_and_state_is_parsed() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert!(matches!(
            link("https://www.xiaohongshu.com/explore/6411cf99000000001300b6d9?xsec_token=AB"),
            Some(Link::Note { id, .. }) if id == ID
        ));
        assert!(matches!(
            link("https://www.xiaohongshu.com/discovery/item/6411cf99000000001300b6d9"),
            Some(Link::Note { .. })
        ));
        assert!(matches!(
            link(
                "https://www.xiaohongshu.com/user/profile/5ff1e0a0000000000101d1c5/6411cf99000000001300b6d9"
            ),
            Some(Link::Note { .. })
        ));
        assert!(matches!(
            link("http://xhslink.com/m/ARQuOnXIImV"),
            Some(Link::Short(_))
        ));
        assert_eq!(link("https://www.xiaohongshu.com/explore"), None);
        assert_eq!(link("https://www.xiaohongshu.com/explore/not-a-note"), None);
        let state = initial_state(&note_page(&video_note().to_string())).unwrap();
        assert!(state["global"]["a"].is_null());
        assert_eq!(note_of(&state, ID).unwrap()["title"], "今天的天气");

        let fallback = Url::parse(FALLBACK).unwrap();
        assert!(is_fallback(&fallback, ID));
        assert!(!is_fallback(&fallback, "6411cf99000000001300b6d0"));
        assert!(!is_fallback(&Url::parse(EXPLORE).unwrap(), ID));
        assert_eq!(fallback_reason(&fallback).as_deref(), Some("该内容暂时无法查看"));
        assert_eq!(
            fallback_reason(&Url::parse("https://www.xiaohongshu.com/explore?undertake_note_error=%E7%AC%94%E8%AE%B0").unwrap()).as_deref(),
            Some("笔记")
        );
        assert_eq!(
            request_message(&json!({"errMsg": "{\"data\":{\"msg\":\"笔记不存在\"}}"})).as_deref(),
            Some("笔记不存在")
        );
        assert_eq!(
            request_message(&json!({"errMsg": "{\"msg\":\"busy\"}"})).as_deref(),
            Some("busy")
        );
        assert_eq!(
            request_message(&json!({"errMsg": "plain words"})).as_deref(),
            Some("plain words")
        );
        assert_eq!(request_message(&json!({"errMsg": ""})), None);
    }

    #[tokio::test]
    async fn video_notes_resolve_with_their_token_carried_over() {
        let mut fixture = Fixture::new("xiaohongshu", None);
        fixture.exchanges.push(exchange(
            "https://www.xiaohongshu.com/explore/6411cf99000000001300b6d9?xsec_token=ABtok&xsec_source=pc_feed",
            200,
            &note_page(&video_note().to_string()),
            &[],
        ));
        let http = Http::replay(fixture);
        http.with_jar(PLATFORM, |jar| {
            jar.insert(Cookie::new(SESSION_COOKIE, "s", ".xiaohongshu.com"));
        });
        let resolver = XiaohongshuResolver::new(http);
        let url = Url::parse(
            "https://www.xiaohongshu.com/explore/6411cf99000000001300b6d9?xsec_token=ABtok&xsec_source=pc_feed",
        )
        .unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some(ID));
        assert_eq!(resolved.title.as_deref(), Some("今天的天气"));
        assert_eq!(resolved.uploader.as_deref(), Some("小红"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://www.xiaohongshu.com/user/profile/5ff1e0a0000000000101d1c5"
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(12)));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.variants.len(), 2);
        assert_eq!(resolved.variants[0].video, Some(VideoCodec::H264));
        assert_eq!(resolved.variants[0].bitrate, Some(2_300_000));
        assert_eq!(resolved.variants[0].size, Some(3456789));
        assert_eq!(
            resolved.variants[0].duration,
            Some(Duration::from_millis(12345))
        );
        assert_eq!(resolved.variants[1].video, Some(VideoCodec::H265));
        assert_eq!(resolved.variants[1].bitrate, Some(1_500_000));
        assert_eq!(resolved.variants[1].duration, Some(Duration::from_secs(12)));
        assert!(
            resolved
                .webpage_url
                .as_ref()
                .unwrap()
                .query()
                .unwrap()
                .contains("xsec_token=ABtok")
        );
    }

    /// The note page the share link's first hop names is asked for with the token, not
    /// the page a bare request would be sent on to.
    #[tokio::test]
    async fn share_links_are_followed_one_hop_at_a_time_to_the_note() {
        let mut fixture = Fixture::new("xiaohongshu", None);
        fixture.exchanges.push(exchange(
            "http://xhslink.com/o/2401ztjrXfq",
            302,
            "",
            &[("location", "https://xhslink.com/m/hop2")],
        ));
        fixture.exchanges.push(exchange(
            "https://xhslink.com/m/hop2",
            302,
            "",
            &[("location", "https://www.xiaohongshu.com/discovery/item/6411cf99000000001300b6d9?app_platform=android&xsec_source=app_share&type=video&xsec_token=CBtok%3D&author_share=1")],
        ));
        fixture.exchanges.push(exchange(
            EXPLORE,
            200,
            &note_page(&video_note().to_string()),
            &[],
        ));
        let resolver = XiaohongshuResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("http://xhslink.com/o/2401ztjrXfq").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some(ID));
        assert_eq!(resolved.webpage_url.as_ref().unwrap().as_str(), EXPLORE);
    }

    #[tokio::test]
    async fn expired_share_links_and_walled_ones_are_reported() {
        let mut fixture = Fixture::new("xiaohongshu", None);
        fixture.exchanges.push(exchange(
            "http://xhslink.com/m/ARQuOnXIImV",
            302,
            "",
            &[("location", "https://www.xiaohongshu.com/")],
        ));
        fixture.exchanges.push(exchange(
            "http://xhslink.com/a/old",
            307,
            "",
            &[("location", "http://www.xiaohongshu.com")],
        ));
        fixture.exchanges.push(exchange(
            "http://xhslink.com/o/walled",
            302,
            "",
            &[("location", "https://www.xiaohongshu.com/login?redirectPath=x")],
        ));
        fixture.exchanges.push(exchange(
            "http://xhslink.com/o/elsewhere",
            302,
            "",
            &[("location", "https://www.xiaohongshu.com/user/profile/5ff1e0a0000000000101d1c5")],
        ));
        fixture.exchanges.push(exchange("http://xhslink.com/o/gone", 404, "", &[]));
        fixture.exchanges.push(exchange(
            "http://xhslink.com/o/loop",
            302,
            "",
            &[("location", "http://xhslink.com/o/loop")],
        ));
        let resolver = XiaohongshuResolver::new(Http::replay(fixture));
        let error = |code: &str| {
            let url = Url::parse(&format!("http://xhslink.com/{code}")).unwrap();
            let resolver = &resolver;
            async move { resolver.resolve(&url).await.unwrap_err() }
        };
        for code in ["m/ARQuOnXIImV", "a/old"] {
            let e = error(code).await;
            assert!(
                matches!(&e, ResolveError::Unavailable { url, reason } if reason.contains("expired") && url.path().ends_with(code)),
                "{e}"
            );
        }
        let e = error("o/walled").await;
        assert!(
            matches!(&e, ResolveError::LoginRequired { reason, .. } if reason.contains("log in")),
            "{e}"
        );
        let e = error("o/elsewhere").await;
        assert!(
            matches!(&e, ResolveError::Unavailable { reason, .. } if reason.contains("user/profile") && reason.contains("no note")),
            "{e}"
        );
        assert!(matches!(error("o/gone").await, ResolveError::NotFound(_)));
        let e = error("o/loop").await;
        assert!(
            matches!(&e, ResolveError::Unavailable { reason, .. } if reason.contains("too many")),
            "{e}"
        );
    }

    /// A note the site cannot serve sends the visitor to the explore feed with the note
    /// named; the state there says why, or carries the note after all.
    #[tokio::test]
    async fn notes_the_site_cannot_serve_are_read_from_its_fallback_page() {
        let short = "http://xhslink.com/o/2401ztjrXfq";
        let discovery = "https://www.xiaohongshu.com/discovery/item/6411cf99000000001300b6d9?xsec_source=app_share&type=video&xsec_token=CBtok%3D";
        let fallback_fixture = |page: String| {
            let mut fixture = Fixture::new("xiaohongshu", None);
            fixture.exchanges.push(exchange(short, 302, "", &[("location", discovery)]));
            fixture.exchanges.push(exchange(EXPLORE, 302, "", &[("location", FALLBACK)]));
            fixture.exchanges.push(exchange(FALLBACK, 200, &page, &[]));
            fixture
        };
        let url = Url::parse(short).unwrap();

        // Removed: the page's own request for the note is answered "笔记不存在".
        let resolver = XiaohongshuResolver::new(Http::replay(fallback_fixture(fallback_page(
            -510000,
            "笔记不存在",
            None,
            Some("笔记加载失败"),
        ))));
        let error = resolver.resolve(&url).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::NotFound(at) if at.as_str() == short),
            "{error}"
        );

        // Walled from strangers, with the site's message.
        let resolver = XiaohongshuResolver::new(Http::replay(fallback_fixture(fallback_page(
            300031,
            "当前笔记暂时无法浏览",
            None,
            Some("笔记加载失败"),
        ))));
        let error = resolver.resolve(&url).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::LoginRequired { reason, .. } if reason.contains("当前笔记暂时无法浏览") && reason.contains("xsec_token")),
            "{error}"
        );

        // Refused for a reason the state does not spell out: the fallback page's own
        // reason is used, and a logged-in session is told as much.
        let http = Http::replay(fallback_fixture(fallback_page(0, "", None, None)));
        http.with_jar(PLATFORM, |jar| {
            jar.insert(Cookie::new(SESSION_COOKIE, "s", ".xiaohongshu.com"));
        });
        let resolver = XiaohongshuResolver::new(http);
        let error = resolver.resolve(&url).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "该内容暂时无法查看"),
            "{error}"
        );

        // Refused with a code of the site's own, which no session is offered for.
        let resolver = XiaohongshuResolver::new(Http::replay(fallback_fixture(fallback_page(
            300013,
            "笔记已被限制",
            None,
            None,
        ))));
        let error = resolver.resolve(&url).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "笔记已被限制 (error 300013)"),
            "{error}"
        );

        // Served after all: the fallback page opens the note over the feed.
        let resolver = XiaohongshuResolver::new(Http::replay(fallback_fixture(fallback_page(
            0,
            "",
            Some(video_note()),
            None,
        ))));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.title.as_deref(), Some("今天的天气"));
        assert_eq!(resolved.webpage_url.as_ref().unwrap().as_str(), EXPLORE);

        // The fallback page itself sending the client on to log in, or to a refusal.
        let mut fixture = Fixture::new("xiaohongshu", None);
        fixture.exchanges.push(exchange(EXPLORE, 302, "", &[("location", FALLBACK)]));
        fixture.exchanges.push(exchange(
            FALLBACK,
            302,
            "",
            &[("location", "https://www.xiaohongshu.com/login?redirectPath=x")],
        ));
        let resolver = XiaohongshuResolver::new(Http::replay(fixture));
        let error = resolver.resolve(&Url::parse(EXPLORE).unwrap()).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::LoginRequired { reason, .. } if reason.contains("log in")),
            "{error}"
        );
        let mut fixture = Fixture::new("xiaohongshu", None);
        fixture.exchanges.push(exchange(EXPLORE, 302, "", &[("location", FALLBACK)]));
        fixture.exchanges.push(exchange(
            FALLBACK,
            302,
            "",
            &[("location", "https://www.xiaohongshu.com/404/sec_x?error_code=300012&error_msg=gone")],
        ));
        let resolver = XiaohongshuResolver::new(Http::replay(fixture));
        let error = resolver.resolve(&Url::parse(EXPLORE).unwrap()).await.unwrap_err();
        assert!(matches!(error, ResolveError::NotFound(_)), "{error}");

        // A redirect that is neither the wall nor the fallback is handed on.
        let mut fixture = Fixture::new("xiaohongshu", None);
        fixture.exchanges.push(exchange(
            EXPLORE,
            302,
            "",
            &[("location", "https://www.xiaohongshu.com/explore?target_note_id=6411cf99000000001300b6d0")],
        ));
        let resolver = XiaohongshuResolver::new(Http::replay(fixture));
        let error = resolver.resolve(&Url::parse(EXPLORE).unwrap()).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Redirect(to) if to.query().unwrap().contains("6411cf99000000001300b6d0")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn strangers_are_told_a_session_is_needed_and_photos_have_no_video() {
        let mut fixture = Fixture::new("xiaohongshu", None);
        fixture.exchanges.push(exchange(
            "https://www.xiaohongshu.com/explore/6411cf99000000001300b6d9?xsec_source=pc_share",
            302,
            "",
            &[("location", "https://www.xiaohongshu.com/404/sec_dSKd?redirectPath=x&error_code=300031&error_msg=%E5%BD%93%E5%89%8D%E7%AC%94%E8%AE%B0%E6%9A%82%E6%97%B6%E6%97%A0%E6%B3%95%E6%B5%8F%E8%A7%88")],
        ));
        fixture.exchanges.push(exchange(
            "https://www.xiaohongshu.com/explore/6411cf99000000001300b6d9?xsec_token=old&xsec_source=app_share",
            302,
            "",
            &[("location", "https://www.xiaohongshu.com/login?redirectPath=http%3A%2F%2Fwww.xiaohongshu.com%2Fexplore%2F6411cf99000000001300b6d9")],
        ));
        let resolver = XiaohongshuResolver::new(Http::replay(fixture));
        let url =
            Url::parse("https://www.xiaohongshu.com/explore/6411cf99000000001300b6d9").unwrap();
        let error = resolver.resolve(&url).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::LoginRequired { reason, .. } if reason.contains("当前笔记") && reason.contains("xsec_token")),
            "{error}"
        );
        let error = resolver
            .resolve(
                &Url::parse(
                    "https://www.xiaohongshu.com/explore/6411cf99000000001300b6d9?xsec_token=old&xsec_source=app_share",
                )
                .unwrap(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::LoginRequired { reason, .. } if reason.contains("log in")),
            "{error}"
        );

        let mut fixture = Fixture::new("xiaohongshu", None);
        fixture.exchanges.push(exchange(
            "https://www.xiaohongshu.com/explore/6411cf99000000001300b6d9?xsec_source=pc_share",
            200,
            &note_page(
                &json!({"type": "normal", "title": "照片", "imageList": [], "user": {}})
                    .to_string(),
            ),
            &[],
        ));
        let http = Http::replay(fixture);
        http.with_jar(PLATFORM, |jar| {
            jar.insert(Cookie::new(SESSION_COOKIE, "s", ".xiaohongshu.com"));
        });
        let resolver = XiaohongshuResolver::new(http);
        let error = resolver.resolve(&url).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("photo")),
            "{error}"
        );
    }
}
