//! Xiaohongshu (RedNote) video notes, from the state the note page renders, with
//! `xhslink.com` share links unwrapped. The site opens a note to a visitor through the
//! `xsec_token` its share links carry, for as long as the token lasts; a link without one,
//! or with one that has run out, is turned away, and the refusal is reported as such.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::page::leading_json;
use super::{
    ClipRange, MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionCheck,
    SessionSupport, Variant, VariantKind, clean_title, timestamp_hint,
};
use crate::http::{BROWSER_UA, Http};
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
    if !(host == "xiaohongshu.com" || host.ends_with(".xiaohongshu.com")) {
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

    /// The site turned the note page away: to the login wall for strangers, or with a
    /// reason of its own.
    fn refusal(&self, location: Option<&Url>, origin: &Url) -> ResolveError {
        let code = location.and_then(|l| query(l, "error_code"));
        let message = location
            .and_then(|l| query(l, "error_msg"))
            .filter(|m| !m.is_empty());
        match (code.as_deref(), message) {
            (Some("300031"), message) | (None, message) if !self.logged_in() => {
                ResolveError::login_required(
                    origin,
                    PLATFORM,
                    format!(
                        "{}; the site opens a note to visitors through the xsec_token its share links carry",
                        message.unwrap_or_else(|| "the site turned the visitor away".into())
                    ),
                )
            }
            (Some("300012"), _) => ResolveError::NotFound(origin.clone()),
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
        let mut request = self
            .http
            .get(url.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("referer", SITE)
            .follow_redirects(false);
        for (name, value) in PAGE_HEADERS {
            request = request.header(name, value);
        }
        let response = request.send().await?;
        let status = response.status.as_u16();
        if (300..400).contains(&status) {
            let location = response.header("location").and_then(|l| url.join(l).ok());
            if location
                .as_ref()
                .is_some_and(|l| l.path().starts_with("/404"))
                || location.is_none()
            {
                return Err(self.refusal(location.as_ref(), origin));
            }
            if location
                .as_ref()
                .is_some_and(|l| l.path().starts_with("/login"))
            {
                return Err(ResolveError::login_required(
                    origin,
                    PLATFORM,
                    "the note page sent this client to log in",
                ));
            }
            return Err(ResolveError::Redirect(location.expect("checked above")));
        }
        match status {
            200..=299 => {}
            404 | 410 => return Err(ResolveError::NotFound(origin.clone())),
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            461 | 471 => return Err(self.refusal(None, origin)),
            _ => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the note page answered HTTP {status}"),
                ));
            }
        }
        let html = response.text(MAX_PAGE).await?;
        let state = initial_state(&html)
            .ok_or_else(|| ResolveError::malformed(origin, "the note page has no state"))?;
        let note = note_of(&state, id).ok_or_else(|| {
            let code = state["note"]["serverRequestInfo"]["errorCode"]
                .as_i64()
                .unwrap_or(0);
            match code {
                -510001 | 300012 => ResolveError::NotFound(origin.clone()),
                0 => self.refusal(None, origin),
                code => ResolveError::unavailable(
                    origin,
                    format!("the note was not served (error {code})"),
                ),
            }
        })?;
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
                "http://xhslink.com/o/2401ztjrXfq",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Note { id, page } => self.note(&id, &page, url).await,
            Link::Short(short) => {
                let target = self
                    .http
                    .unwrap_redirects(&short, Some(PLATFORM), BROWSER_UA)
                    .await?;
                match parse_link(&target) {
                    Some(Link::Note { id, page }) => self.note(&id, &page, url).await,
                    _ => Err(ResolveError::unavailable(
                        url,
                        "the share link leads to no note; share links expire",
                    )),
                }
            }
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

    fn note_page(note: &str) -> String {
        format!(
            r#"<html><script>window.__INITIAL_STATE__={{"global":{{"a":undefined}},"user":{{"loggedIn":false}},"note":{{"noteDetailMap":{{"{ID}":{{"note":{note},"comments":{{"list":[]}}}}}},"serverRequestInfo":{{"errorCode":0}}}}}}</script></html>"#
        )
    }

    fn video_note() -> String {
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
        .to_string()
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
        let state = initial_state(&note_page(&video_note())).unwrap();
        assert!(state["global"]["a"].is_null());
        assert_eq!(note_of(&state, ID).unwrap()["title"], "今天的天气");
    }

    #[tokio::test]
    async fn video_notes_resolve_with_their_token_carried_over() {
        let mut fixture = Fixture::new("xiaohongshu", None);
        fixture.exchanges.push(exchange(
            "https://www.xiaohongshu.com/explore/6411cf99000000001300b6d9?xsec_token=ABtok&xsec_source=pc_feed",
            200,
            &note_page(&video_note()),
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

    #[tokio::test]
    async fn visitors_are_served_through_a_share_links_token() {
        let mut fixture = Fixture::new("xiaohongshu", None);
        fixture.exchanges.push(exchange(
            "http://xhslink.com/o/2401ztjrXfq",
            302,
            "",
            &[("location", "https://www.xiaohongshu.com/discovery/item/6411cf99000000001300b6d9?app_platform=android&xsec_source=app_share&type=video&xsec_token=CBtok%3D&author_share=1")],
        ));
        fixture.exchanges.push(exchange(
            "https://www.xiaohongshu.com/discovery/item/6411cf99000000001300b6d9?app_platform=android&xsec_source=app_share&type=video&xsec_token=CBtok%3D&author_share=1",
            200,
            "<html></html>",
            &[],
        ));
        fixture.exchanges.push(exchange(
            "https://www.xiaohongshu.com/explore/6411cf99000000001300b6d9?xsec_token=CBtok%3D&xsec_source=app_share",
            200,
            &note_page(&video_note()),
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
        assert_eq!(
            resolved.webpage_url.as_ref().unwrap().as_str(),
            "https://www.xiaohongshu.com/explore/6411cf99000000001300b6d9?xsec_token=CBtok%3D&xsec_source=app_share"
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
        fixture.exchanges.push(exchange(
            "http://xhslink.com/m/ARQuOnXIImV",
            302,
            "",
            &[("location", "https://www.xiaohongshu.com/")],
        ));
        fixture.exchanges.push(exchange(
            "https://www.xiaohongshu.com/",
            200,
            "<html></html>",
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
        let error = resolver
            .resolve(&Url::parse("http://xhslink.com/m/ARQuOnXIImV").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("expire")),
            "{error}"
        );
    }
}
