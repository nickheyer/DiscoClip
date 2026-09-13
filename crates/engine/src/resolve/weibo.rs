//! Weibo posts with video, and Weibo TV shows, through the site's own API with the
//! visitor cookies it hands anonymous browsers, with `t.cn` short links unwrapped.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::page::{between, leading_json};
use super::{
    ClipRange, MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionCheck,
    SessionSupport, Variant, VariantKind, clean_title, fetch, parse_time_stamp, timestamp_hint,
};
use crate::http::cookies::parse_http_date;
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "weibo";
const SITE: &str = "https://weibo.com/";
const STATUS_API: &str = "https://weibo.com/ajax/statuses/show";
const TV_API: &str = "https://weibo.com/tv/api/component";
const GEN_VISITOR: &str = "https://passport.weibo.com/visitor/genvisitor";
const INCARNATE: &str = "https://passport.weibo.com/visitor/visitor";
/// The cookie every visitor and every logged-in session carries.
const SUB_COOKIE: &str = "SUB";

static RE_BID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9]{9}$").unwrap());
static RE_OID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d+:\d+$").unwrap());

/// What a link names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A post, by its number or its short id.
    Status(String),
    /// A Weibo TV show, by its object id such as `1034:4797699866951785`.
    Show(String),
    /// A `t.cn` short link.
    Short(Url),
}

fn digits(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_digit())
}

fn status_id(text: &str) -> bool {
    digits(text) || RE_BID.is_match(text)
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
    if host == "t.cn" {
        return (!segments.is_empty()).then(|| Link::Short(url.clone()));
    }
    let weibo = host == "weibo.com"
        || host.ends_with(".weibo.com")
        || host == "weibo.cn"
        || host.ends_with(".weibo.cn");
    if !weibo {
        return None;
    }
    if host == "video.weibo.com" {
        return query(url, "fid")
            .filter(|f| RE_OID.is_match(f))
            .map(Link::Show);
    }
    match segments.as_slice() {
        ["tv", "show", oid] if RE_OID.is_match(oid) => Some(Link::Show(oid.to_string())),
        ["status" | "detail", id, ..] if status_id(id) => Some(Link::Status(id.to_string())),
        [uid, id] if digits(uid) && RE_BID.is_match(id) => Some(Link::Status(id.to_string())),
        [uid, id] if digits(uid) && digits(id) && id.len() >= 15 => {
            Some(Link::Status(id.to_string()))
        }
        _ => None,
    }
}

/// The JSON a JSONP answer wraps, such as `gen_callback({...})`.
fn jsonp(text: &str) -> Option<Value> {
    let start = text.find('(')?;
    leading_json(&text[start + 1..]).map(|(value, _)| value)
}

fn absolute(text: &str) -> Option<Url> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if text.starts_with("//") {
        Url::parse(&format!("https:{text}")).ok()
    } else {
        Url::parse(text).ok()
    }
}

/// The height a quality label such as `高清 1080P`, `720p` or `mp4_720p_mp4` names: the
/// run of digits a `p` follows.
fn label_height(label: &str) -> Option<u32> {
    let chars: Vec<char> = label.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if !chars[index].is_ascii_digit() {
            index += 1;
            continue;
        }
        let start = index;
        while index < chars.len() && chars[index].is_ascii_digit() {
            index += 1;
        }
        if chars.get(index).is_some_and(|c| *c == 'p' || *c == 'P') {
            let digits: String = chars[start..index].iter().collect();
            return digits.parse().ok().filter(|h| *h > 0);
        }
    }
    None
}

/// A post's text without its HTML and trailing links.
fn plain_text(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for ch in text.chars() {
        match ch {
            '<' => {
                in_tag = true;
                out.push(' ');
            }
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    let out: String = out
        .chars()
        .filter(|c| !matches!(c, '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{feff}'))
        .collect();
    let words: Vec<&str> = out
        .split_whitespace()
        .filter(|w| !w.starts_with("http://") && !w.starts_with("https://"))
        .collect();
    clean_title(&words.join(" "))
}

/// Whether two links name the same file on the CDN, whatever their scheme and signature.
fn same_file(a: &Url, b: &Url) -> bool {
    a.host_str() == b.host_str() && a.path() == b.path()
}

pub struct WeiboResolver {
    http: Http,
}

impl WeiboResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    fn has_sub(&self) -> bool {
        self.http.jar(PLATFORM).get(SUB_COOKIE).is_some()
    }

    /// Whether the jar holds a logged-in session rather than visitor cookies.
    fn logged_in(&self) -> bool {
        let jar = self.http.jar(PLATFORM);
        jar.get("SSOLoginState").is_some()
            || jar.get("XSRF-TOKEN").is_some()
                && jar.get("WBPSESS").is_some()
                && jar.get("ALF").is_some()
    }

    /// Becomes a visitor: asks passport for a visitor id and has it incarnated into the
    /// `SUB` cookies the API wants.
    async fn visitor(&self, origin: &Url) -> Result<(), ResolveError> {
        let response = self
            .http
            .post(Url::parse(GEN_VISITOR).expect("valid"))
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .form(&[("cb", "gen_callback"), ("fp", "{}")])
            .send()
            .await?;
        if !response.is_success() {
            return Err(ResolveError::unavailable(
                origin,
                format!("the visitor service answered HTTP {}", response.status),
            ));
        }
        let text = response.text(MAX_PAGE).await?;
        let answer = jsonp(&text)
            .ok_or_else(|| ResolveError::malformed(origin, "the visitor answer is not JSONP"))?;
        let tid = answer["data"]["tid"]
            .as_str()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| ResolveError::malformed(origin, "the visitor answer has no tid"))?;
        let w = if answer["data"]["new_tid"].as_bool() == Some(true) {
            "3"
        } else {
            "2"
        };
        let mut incarnate = Url::parse(INCARNATE).expect("valid");
        incarnate.query_pairs_mut().extend_pairs([
            ("a", "incarnate"),
            ("t", tid),
            ("w", w),
            ("c", "095"),
            ("gc", ""),
            ("cb", "cross_domain"),
            ("from", "weibo"),
            ("_rand", "0.5"),
        ]);
        let response = self
            .http
            .get(incarnate)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .send()
            .await?;
        let _ = response.bytes_up_to(64 * 1024).await?;
        if !self.has_sub() {
            return Err(ResolveError::unavailable(
                origin,
                "the visitor service handed out no session",
            ));
        }
        Ok(())
    }

    /// GETs or POSTs an API URL as a visitor, becoming one first when needed and again
    /// when the API says the visitor is not known.
    async fn api(
        &self,
        url: Url,
        form: Option<&[(&str, &str)]>,
        referer: &str,
        origin: &Url,
    ) -> Result<Value, ResolveError> {
        for attempt in 0..2 {
            if !self.has_sub() {
                self.visitor(origin).await?;
            }
            let mut request = match form {
                Some(fields) => self.http.post(url.clone()).form(fields),
                None => self.http.get(url.clone()),
            };
            request = request
                .platform(PLATFORM)
                .user_agent(BROWSER_UA)
                .header("referer", referer)
                .header("accept", "application/json, text/plain, */*")
                .header("x-requested-with", "XMLHttpRequest")
                .follow_redirects(false);
            let response = request.send().await?;
            let status = response.status.as_u16();
            let text = response.text(MAX_PAGE).await?;
            let refused = status == 403
                || (300..400).contains(&status)
                || (status == 200 && text.trim_start().starts_with("<html"))
                || text.contains("\"error\":\"Forbidden\"");
            if refused {
                if attempt == 0 {
                    self.http.with_jar(PLATFORM, |jar| {
                        let doomed: Vec<(String, String)> = jar
                            .cookies()
                            .iter()
                            .filter(|c| c.name == SUB_COOKIE || c.name == "SUBP")
                            .map(|c| (c.name.clone(), c.domain.clone()))
                            .collect();
                        for (name, domain) in doomed {
                            jar.remove(&name, &domain);
                        }
                    });
                    continue;
                }
                return Err(ResolveError::unavailable(
                    origin,
                    "the API refused the visitor session twice",
                ));
            }
            if status == 429 {
                return Err(ResolveError::RateLimited(origin.clone()));
            }
            if status == 404 {
                return Err(ResolveError::NotFound(origin.clone()));
            }
            return serde_json::from_str(&text)
                .map_err(|e| ResolveError::malformed(origin, format!("JSON: {e}")));
        }
        Err(ResolveError::unavailable(
            origin,
            "the API refused the request",
        ))
    }

    async fn status(&self, id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let mut url = Url::parse(STATUS_API).expect("valid");
        url.query_pairs_mut().append_pair("id", id);
        let post = self.api(url, None, SITE, origin).await?;
        if post["id"].is_null() && post["idstr"].is_null() {
            let message = post["message"]
                .as_str()
                .or_else(|| post["msg"].as_str())
                .unwrap_or("")
                .to_string();
            let ok = post["ok"].as_i64().unwrap_or(0);
            return Err(
                if message.contains("不存在") || post["error_code"].as_i64() == Some(20101) {
                    ResolveError::NotFound(origin.clone())
                } else if ok == -100 && !self.logged_in() {
                    ResolveError::login_required(
                        origin,
                        PLATFORM,
                        "the post is shown only to logged-in users",
                    )
                } else if message.is_empty() {
                    ResolveError::unavailable(origin, format!("the API answered ok {ok}"))
                } else {
                    ResolveError::unavailable(origin, message)
                },
            );
        }
        let (media, page_info) = media_of(&post)
            .or_else(|| media_of(&post["retweeted_status"]))
            .ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
        let duration = media["duration"]
            .as_f64()
            .filter(|d| *d > 0.0)
            .map(Duration::from_secs_f64);
        let variants = variants_of(media, duration);
        if variants.is_empty() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        let user = &post["user"];
        let screen_name = user["screen_name"].as_str().unwrap_or("");
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = post["mblogid"]
            .as_str()
            .map(String::from)
            .or_else(|| post["idstr"].as_str().map(String::from))
            .or_else(|| post["id"].as_u64().map(|i| i.to_string()));
        resolved.title = post["text_raw"]
            .as_str()
            .and_then(plain_text)
            .or_else(|| post["text"].as_str().and_then(plain_text))
            .or_else(|| page_info["title"].as_str().and_then(clean_title))
            .or_else(|| media["kol_title"].as_str().and_then(clean_title))
            .or_else(|| (!screen_name.is_empty()).then(|| format!("Weibo by {screen_name}")));
        resolved.description = post["text_raw"].as_str().and_then(clean_title);
        resolved.uploader = clean_title(screen_name);
        resolved.uploader_url = user["idstr"]
            .as_str()
            .or_else(|| user["id"].as_u64().map(|_| "").filter(|_| false))
            .map(String::from)
            .or_else(|| user["id"].as_u64().map(|i| i.to_string()))
            .and_then(|uid| Url::parse(&format!("{SITE}u/{uid}")).ok());
        resolved.uploaded_at = post["created_at"].as_str().and_then(parse_http_date);
        resolved.duration = duration.or_else(|| variants.iter().find_map(|v| v.duration));
        resolved.thumbnail = page_info["page_pic"]
            .as_str()
            .or_else(|| page_info["page_pic"]["url"].as_str())
            .or_else(|| media["video_cover"].as_str())
            .and_then(absolute);
        resolved.webpage_url = match (user["idstr"].as_str(), post["mblogid"].as_str()) {
            (Some(uid), Some(bid)) => Url::parse(&format!("{SITE}{uid}/{bid}")).ok(),
            _ => resolved
                .id
                .as_ref()
                .and_then(|id| Url::parse(&format!("{SITE}detail/{id}")).ok()),
        };
        resolved.clip = timestamp_hint(origin).map(|start| ClipRange { start, end: None });
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn show(&self, oid: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let mut url = Url::parse(TV_API).expect("valid");
        url.query_pairs_mut()
            .append_pair("page", &format!("/tv/show/{oid}"));
        let data = format!(r#"{{"Component_Play_Playinfo":{{"oid":"{oid}"}}}}"#);
        let referer = format!("{SITE}tv/show/{oid}");
        let answer = self
            .api(url, Some(&[("data", data.as_str())]), &referer, origin)
            .await?;
        let info = &answer["data"]["Component_Play_Playinfo"];
        if info.is_null() {
            let message = answer["msg"]
                .as_str()
                .or_else(|| answer["message"].as_str())
                .unwrap_or("the show has no play information");
            return Err(
                if message.contains("不存在") || answer["code"].as_str() == Some("100002") {
                    ResolveError::NotFound(origin.clone())
                } else {
                    ResolveError::unavailable(origin, message)
                },
            );
        }
        let duration = info["duration"]
            .as_str()
            .and_then(parse_time_stamp)
            .or_else(|| info["duration"].as_f64().map(Duration::from_secs_f64));
        let headers = vec![("referer".to_string(), SITE.to_string())];
        // Several labels may name one file; the tallest label is the one it serves.
        let mut files: Vec<(Url, String, Option<u32>)> = Vec::new();
        for (label, value) in info["urls"].as_object().into_iter().flatten() {
            let Some(url) = value.as_str().and_then(absolute) else {
                continue;
            };
            let height = label_height(label);
            match files.iter_mut().find(|(u, _, _)| same_file(u, &url)) {
                Some(existing) if height > existing.2 => {
                    existing.1 = label.clone();
                    existing.2 = height;
                }
                Some(_) => {}
                None => files.push((url, label.clone(), height)),
            }
        }
        files.sort_by_key(|file| std::cmp::Reverse(file.2));
        let mut variants = Vec::new();
        for (url, label, height) in files {
            let mut v = Variant::new(url, VariantKind::File);
            v.container = Some(Container::Mp4);
            v.video = Some(VideoCodec::H264);
            v.audio = Some(AudioCodec::Aac);
            v.height = height;
            v.duration = duration;
            v.label = Some(label);
            v.headers = headers.clone();
            variants.push(v);
        }
        if variants.is_empty() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(oid.to_string());
        resolved.title = info["title"]
            .as_str()
            .and_then(plain_text)
            .or_else(|| info["text"].as_str().and_then(plain_text));
        resolved.description = info["text"].as_str().and_then(plain_text);
        resolved.uploader = info["author"].as_str().and_then(clean_title);
        resolved.uploader_url = info["user"]["id"]
            .as_u64()
            .or_else(|| info["uid"].as_u64())
            .and_then(|uid| Url::parse(&format!("{SITE}u/{uid}")).ok());
        resolved.uploaded_at = info["real_date"]
            .as_i64()
            .and_then(|t| Timestamp::from_second(t).ok());
        resolved.duration = duration;
        resolved.thumbnail = info["cover_image"].as_str().and_then(absolute);
        resolved.webpage_url = Url::parse(&referer).ok();
        resolved.clip = timestamp_hint(origin).map(|start| ClipRange { start, end: None });
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }
}

/// The media record and page info a post carries its video in: its own page, or the
/// first video of a mixed media post.
fn media_of(post: &Value) -> Option<(&Value, &Value)> {
    if post.is_null() {
        return None;
    }
    let page_info = &post["page_info"];
    if page_info["object_type"].as_str() == Some("video") && page_info["media_info"].is_object() {
        return Some((&page_info["media_info"], page_info));
    }
    post["mix_media_info"]["items"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|item| item["type"].as_str() == Some("video"))
        .and_then(|item| {
            let data = &item["data"];
            data["media_info"]
                .is_object()
                .then_some((&data["media_info"], data))
        })
}

/// The variants a media record lists: every entry of its playback list, then the plain
/// stream URLs behind it, without repeats.
pub fn variants_of(media: &Value, duration: Option<Duration>) -> Vec<Variant> {
    let headers = vec![("referer".to_string(), SITE.to_string())];
    let mut variants: Vec<Variant> = Vec::new();
    for item in media["playback_list"].as_array().into_iter().flatten() {
        let play = &item["play_info"];
        let Some(url) = play["url"].as_str().and_then(absolute) else {
            continue;
        };
        let mime = play["mime"].as_str().unwrap_or("");
        if !mime.is_empty() && !mime.starts_with("video/") {
            continue;
        }
        if mime.is_empty()
            && !url.path().contains(".mp4")
            && !url.host_str().is_some_and(|h| h.contains("video"))
        {
            continue;
        }
        if variants.iter().any(|v| same_file(&v.url, &url)) {
            continue;
        }
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(Container::Mp4);
        v.video = Some(
            if play["codec"]
                .as_str()
                .is_some_and(|c| c.contains("265") || c.contains("hevc"))
            {
                VideoCodec::H265
            } else {
                VideoCodec::H264
            },
        );
        v.audio = Some(AudioCodec::Aac);
        v.width = play["width"].as_u64().map(|w| w as u32);
        v.height = play["height"].as_u64().map(|h| h as u32);
        v.bitrate = play["bitrate"].as_u64().filter(|b| *b > 0);
        v.size = play["size"].as_u64().filter(|s| *s > 0);
        v.duration = play["duration"]
            .as_f64()
            .filter(|d| *d > 0.0)
            .map(Duration::from_secs_f64)
            .or(duration);
        v.label = play["quality_label"].as_str().map(String::from);
        v.format_id = play["quality_label"].as_str().map(String::from);
        v.headers = headers.clone();
        variants.push(v);
    }
    for key in [
        "mp4_1080p_mp4",
        "mp4_720p_mp4",
        "stream_url_hd",
        "mp4_hd_url",
        "h265_mp4_hd",
        "stream_url",
        "mp4_sd_url",
        "h265_mp4_ld",
        "mp4_ld_mp4",
    ] {
        let Some(url) = media[key].as_str().and_then(absolute) else {
            continue;
        };
        if variants.iter().any(|v| same_file(&v.url, &url)) {
            continue;
        }
        // The H.265 renditions are HLS playlists rather than files.
        let playlist = url.path().ends_with(".m3u8");
        let mut v = Variant::new(
            url,
            if playlist {
                VariantKind::Hls
            } else {
                VariantKind::File
            },
        );
        v.container = (!playlist).then_some(Container::Mp4);
        v.video = Some(if key.starts_with("h265") {
            VideoCodec::H265
        } else {
            VideoCodec::H264
        });
        v.audio = Some(AudioCodec::Aac);
        v.height = label_height(key).or(match key {
            "stream_url_hd" | "mp4_hd_url" | "h265_mp4_hd" => Some(720),
            "stream_url" | "mp4_sd_url" | "h265_mp4_ld" => Some(480),
            _ => None,
        });
        v.duration = duration;
        v.format_id = Some(key.to_string());
        v.headers = headers.clone();
        variants.push(v);
    }
    variants
}

#[async_trait]
impl Resolver for WeiboResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Weibo",
            hosts: &["weibo.com", "weibo.cn", "t.cn"],
            features: &["posts", "reposts", "weibo tv", "short links"],
            formats: &["mp4"],
            session: SessionSupport::Optional,
            examples: &[
                "https://weibo.com/7827771738/N4xlMvjhI",
                "https://m.weibo.cn/status/4189191225395228",
                "https://weibo.com/tv/show/1034:4797699866951785",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Short(short) => {
                let target = self
                    .http
                    .unwrap_redirects(&short, Some(PLATFORM), BROWSER_UA)
                    .await?;
                if target.host_str() == short.host_str() {
                    return Err(ResolveError::NotFound(url.clone()));
                }
                Err(ResolveError::Redirect(target))
            }
            Link::Status(id) => self.status(&id, url).await,
            Link::Show(oid) => self.show(&oid, url).await,
        }
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        if !self.has_sub() {
            return Ok(SessionCheck::LoggedOut);
        }
        let home = Url::parse(SITE).expect("valid");
        let fetched = fetch(&self.http, &home, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        let html = fetched.text();
        let Some(config) = between(&html, "window.$CONFIG = ", ";") else {
            return Ok(SessionCheck::LoggedOut);
        };
        let Ok(config) = serde_json::from_str::<Value>(config) else {
            return Ok(SessionCheck::LoggedOut);
        };
        let name = config["user"]["screen_name"]
            .as_str()
            .or_else(|| config["nick"].as_str())
            .filter(|n| !n.is_empty());
        let uid = config["user"]["idstr"]
            .as_str()
            .or_else(|| config["uid"].as_str())
            .filter(|u| !u.is_empty() && *u != "0");
        Ok(match (name, uid) {
            (Some(name), _) => SessionCheck::LoggedIn {
                account: name.to_string(),
            },
            (None, Some(uid)) => SessionCheck::LoggedIn {
                account: format!("uid {uid}"),
            },
            (None, None) => SessionCheck::LoggedOut,
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
        let mut all: Vec<(String, String)> =
            vec![("content-type".into(), "application/json".into())];
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

    fn visitor() -> [Exchange; 2] {
        [
            exchange(
                "POST",
                GEN_VISITOR,
                200,
                r#"window.gen_callback && gen_callback({"retcode":20000000,"msg":"succ","data":{"tid":"01ATzJ9X","new_tid":true}});"#,
                &[],
            ),
            exchange(
                "GET",
                INCARNATE,
                200,
                r#"window.cross_domain && cross_domain({"retcode":20000000,"msg":"succ","data":{"sub":"_2Ak","subp":"0033"}});"#,
                &[
                    (
                        "set-cookie",
                        "SUB=_2Ak; path=/; domain=.weibo.com; secure; httponly",
                    ),
                    ("set-cookie", "SUBP=0033; path=/; domain=.weibo.com"),
                ],
            ),
        ]
    }

    fn post() -> Value {
        json!({
            "id": 4910815147462302_u64, "idstr": "4910815147462302", "mid": "4910815147462302", "mblogid": "N4xlMvjhI",
            "created_at": "Fri Jun 09 20:17:12 +0800 2023",
            "text_raw": "【睡前消息暑假版第1期：拉泰国一把】<br/>对中国有好处 https://video.weibo.com/show?fid=1034:1",
            "user": {"id": 7827771738_u64, "idstr": "7827771738", "screen_name": "睡前视频基地"},
            "page_info": {"type": "11", "object_type": "video", "page_pic": "https://wx3.sinaimg.cn/orj480/cover.jpg",
                "media_info": {"duration": 918, "stream_url": "http://f.video.weibocdn.com/o0/sd.mp4?label=mp4_720p", "stream_url_hd": "http://f.video.weibocdn.com/o0/hd.mp4?label=mp4_720p",
                    "mp4_720p_mp4": "http://f.video.weibocdn.com/o0/hd.mp4?label=mp4_720p", "h265_mp4_hd": "http://f.us.sinaimg.cn/0036wuhhlx07gSu83S6A010f020000230k01.m3u8?ori=0",
                    "playback_list": [
                        {"play_info": {"url": "https://f.video.weibocdn.com/o0/1080.mp4", "width": 1920, "height": 1080, "bitrate": 1031615, "quality_label": "1080p", "mime": "video/mp4", "duration": 918.835, "size": 118485736}},
                        {"play_info": {"url": "https://f.video.weibocdn.com/o0/720.mp4", "width": 1280, "height": 720, "bitrate": 584109, "quality_label": "720p", "mime": "video/mp4", "duration": 918.835, "size": 67087577}},
                        {"play_info": {"url": "https://wx1.sinaimg.cn/large/still.jpg", "width": 320, "height": 180, "mime": "image/jpeg"}}
                    ]}}
        })
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://weibo.com/7827771738/N4xlMvjhI"),
            Some(Link::Status("N4xlMvjhI".into()))
        );
        assert_eq!(
            link("https://m.weibo.cn/status/4189191225395228?from=x"),
            Some(Link::Status("4189191225395228".into()))
        );
        assert_eq!(
            link("https://m.weibo.cn/detail/4189191225395228"),
            Some(Link::Status("4189191225395228".into()))
        );
        assert_eq!(
            link("https://weibo.com/tv/show/1034:4797699866951785?from=old_pc_videoshow"),
            Some(Link::Show("1034:4797699866951785".into()))
        );
        assert_eq!(
            link("https://video.weibo.com/show?fid=1034:4797699866951785"),
            Some(Link::Show("1034:4797699866951785".into()))
        );
        assert!(matches!(link("https://t.cn/A6abc"), Some(Link::Short(_))));
        assert_eq!(link("https://weibo.com/u/7827771738"), None);
        assert_eq!(link("https://weibo.com/"), None);
        assert_eq!(label_height("高清 1080P"), Some(1080));
        assert_eq!(label_height("mp4_720p_mp4"), Some(720));
        assert_eq!(label_height("stream_url"), None);
    }

    #[tokio::test]
    async fn posts_resolve_after_becoming_a_visitor() {
        let mut fixture = Fixture::new("weibo", None);
        fixture.exchanges.extend(visitor());
        fixture.exchanges.push(exchange(
            "GET",
            "https://weibo.com/ajax/statuses/show?id=N4xlMvjhI",
            200,
            &post().to_string(),
            &[],
        ));
        let http = Http::replay(fixture);
        let resolver = WeiboResolver::new(http.clone());
        let url = Url::parse("https://weibo.com/7827771738/N4xlMvjhI").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("N4xlMvjhI"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("【睡前消息暑假版第1期：拉泰国一把】 对中国有好处")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("睡前视频基地"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://weibo.com/u/7827771738"
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(918)));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(
            resolved.webpage_url.as_ref().unwrap().as_str(),
            "https://weibo.com/7827771738/N4xlMvjhI"
        );
        // Two playback entries, the still skipped, then the plain streams that name files
        // the playback list does not.
        assert_eq!(resolved.variants.len(), 5);
        assert_eq!(resolved.variants[0].height, Some(1080));
        assert_eq!(resolved.variants[0].size, Some(118485736));
        assert_eq!(resolved.variants[0].label.as_deref(), Some("1080p"));
        assert_eq!(
            resolved.variants[2].format_id.as_deref(),
            Some("mp4_720p_mp4")
        );
        assert_eq!(resolved.variants[2].height, Some(720));
        assert_eq!(
            resolved.variants[3].format_id.as_deref(),
            Some("h265_mp4_hd")
        );
        assert_eq!(resolved.variants[3].kind, VariantKind::Hls);
        assert_eq!(resolved.variants[3].video, Some(VideoCodec::H265));
        assert_eq!(
            resolved.variants[4].format_id.as_deref(),
            Some("stream_url")
        );
        assert_eq!(http.jar(PLATFORM).get("SUB").unwrap().value, "_2Ak");
    }

    #[tokio::test]
    async fn missing_posts_refusals_and_shows() {
        let mut fixture = Fixture::new("weibo", None);
        fixture.exchanges.extend(visitor());
        fixture.exchanges.push(exchange(
            "GET",
            "https://weibo.com/ajax/statuses/show?id=5100112345678901",
            200,
            &json!({"ok": 0, "message": "该微博不存在", "error_code": 20101}).to_string(),
            &[],
        ));
        // A refused visitor is replaced once, then the request goes through.
        fixture.exchanges.push(exchange(
            "GET",
            "https://weibo.com/ajax/statuses/show?id=4189191225395228",
            403,
            r#"{"error":"Forbidden"}"#,
            &[],
        ));
        fixture.exchanges.extend(visitor());
        let mut no_video = post();
        no_video["page_info"] = json!({"type": "2", "object_type": "article"});
        fixture.exchanges.push(exchange(
            "GET",
            "https://weibo.com/ajax/statuses/show?id=4189191225395228",
            200,
            &no_video.to_string(),
            &[],
        ));
        fixture.exchanges.push(exchange(
            "POST",
            "https://weibo.com/tv/api/component?page=/tv/show/1034:4797699866951785",
            200,
            &json!({"code": "100000", "data": {"Component_Play_Playinfo": {
                "title": "呃，稍微了解了一下", "author": "君子爱财陈平安", "duration": "1:16", "mid": 4797700463137878_u64,
                "urls": {"高清 1080P": "//f.video.weibocdn.com/o0/1080", "高清 720P": "//f.video.weibocdn.com/o0/720", "标清 480P": "//f.video.weibocdn.com/o0/720"},
                "cover_image": "//wx4.sinaimg.cn/large/cover.jpg", "real_date": 1659344248, "user": {"id": 1}}}})
            .to_string(),
            &[],
        ));
        let resolver = WeiboResolver::new(Http::replay(fixture));
        let url = |s: &str| Url::parse(s).unwrap();
        assert!(matches!(
            resolver
                .resolve(&url("https://weibo.com/detail/5100112345678901"))
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        assert!(matches!(
            resolver
                .resolve(&url("https://m.weibo.cn/status/4189191225395228"))
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let show = resolver
            .resolve(&url("https://weibo.com/tv/show/1034:4797699866951785"))
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(show.title.as_deref(), Some("呃，稍微了解了一下"));
        assert_eq!(show.uploader.as_deref(), Some("君子爱财陈平安"));
        assert_eq!(show.duration, Some(Duration::from_secs(76)));
        assert_eq!(show.variants.len(), 2);
        assert_eq!(show.variants[0].height, Some(1080));
        assert_eq!(
            show.variants[0].url.as_str(),
            "https://f.video.weibocdn.com/o0/1080"
        );
        assert_eq!(
            show.thumbnail.as_ref().unwrap().as_str(),
            "https://wx4.sinaimg.cn/large/cover.jpg"
        );
    }
}
