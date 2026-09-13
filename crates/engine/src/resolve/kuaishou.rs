//! Kuaishou videos, from the state the desktop page renders and the state the mobile
//! share page renders behind it, with `v.kuaishou.com` and `kuaishou.com/f/` short links
//! unwrapped. The site asks unfamiliar clients to verify themselves; a stored browser
//! session passes, and the refusal is reported as such.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::{Value, json};
use url::Url;

use super::page::json_after;
use super::{
    ClipRange, MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionCheck,
    SessionSupport, Variant, VariantKind, clean_title, fetch, timestamp_hint,
};
use crate::http::{BROWSER_UA, Http, MOBILE_UA};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "kuaishou";
const SITE: &str = "https://www.kuaishou.com/";
const GRAPHQL: &str = "https://www.kuaishou.com/graphql";
const MOBILE_SHARE: &str = "https://v.m.chenzhongtech.com/fw/photo/";
/// The cookie a logged-in kuaishou.com session carries.
const SESSION_COOKIE: &str = "kuaishou.server.web_st";
/// The desktop page's status for a client it wants verified first.
const STATUS_VERIFY: i64 = 1040;

static RE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^3x[a-z0-9]{5,}$").unwrap());

/// What a link names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Video(String),
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
    if host == "v.kuaishou.com" {
        return (!segments.is_empty()).then(|| Link::Short(url.clone()));
    }
    if host.ends_with(".chenzhongtech.com") || host == "chenzhongtech.com" {
        if let Some(id) = query(url, "photoId").filter(|id| RE_ID.is_match(id)) {
            return Some(Link::Video(id));
        }
        return match segments.as_slice() {
            ["fw", "photo", id, ..] if RE_ID.is_match(id) => Some(Link::Video(id.to_string())),
            _ => None,
        };
    }
    if !(host == "kuaishou.com" || host.ends_with(".kuaishou.com")) {
        return None;
    }
    match segments.as_slice() {
        ["short-video" | "photo", id, ..] if RE_ID.is_match(id) => {
            Some(Link::Video(id.to_string()))
        }
        ["f", code] if !code.is_empty() => Some(Link::Short(url.clone())),
        _ => None,
    }
}

/// The pieces of the desktop page's state that describe a video.
#[derive(Debug, Default)]
pub struct DetailState {
    pub status: Option<i64>,
    pub photo: Option<Value>,
    pub author: Option<Value>,
}

/// Puts an Apollo cache entry back together: `{"type":"id","id":…}` references are
/// replaced by the entries they point at, `{"type":"json","json":…}` wrappers by what
/// they hold, through every nested object and list.
pub fn denormalize(value: &Value, client: &Value, depth: u8) -> Value {
    if depth == 0 {
        return value.clone();
    }
    match value {
        Value::Object(map) => {
            if map.get("type").and_then(|t| t.as_str()) == Some("json")
                && let Some(json) = map.get("json")
            {
                return denormalize(json, client, depth - 1);
            }
            if map.get("type").and_then(|t| t.as_str()) == Some("id")
                && let Some(id) = map.get("id").and_then(|i| i.as_str())
                && let Some(target) = client.get(id)
            {
                return denormalize(target, client, depth - 1);
            }
            Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), denormalize(v, client, depth - 1)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|v| denormalize(v, client, depth - 1))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Reads the desktop page's Apollo state for `id`.
pub fn detail_state(html: &str, id: &str) -> Option<DetailState> {
    let state = json_after(html, "window.__APOLLO_STATE__=")
        .or_else(|| json_after(html, "window.__APOLLO_STATE__ ="))?;
    let client = &state["defaultClient"];
    let mut found = DetailState::default();
    for (key, value) in client["ROOT_QUERY"].as_object().into_iter().flatten() {
        if key.starts_with("visionVideoDetail(") && key.contains(id) {
            found.status = value["status"].as_i64();
        }
    }
    found.photo = client
        .get(format!("VisionVideoDetailPhoto:{id}"))
        .filter(|v| v.is_object())
        .map(|photo| denormalize(photo, client, 8));
    found.author = client.as_object().and_then(|map| {
        map.iter()
            .find(|(key, _)| key.starts_with("VisionVideoDetailAuthor:"))
            .map(|(_, value)| denormalize(value, client, 4))
    });
    Some(found)
}

/// The photo record the mobile share page's state carries, and the result code when it
/// carries a refusal instead.
pub fn mobile_state(html: &str) -> Option<Result<Value, (i64, String)>> {
    let state = json_after(html, "window.INIT_STATE=")
        .or_else(|| json_after(html, "window.INIT_STATE ="))?;
    let mut refusal = None;
    for value in state.as_object()?.values() {
        if value["photo"].is_object() {
            return Some(Ok(value.clone()));
        }
        if let Some(result) = value["result"].as_i64()
            && result != 1
            && let Some(message) = value["error_msg"].as_str()
        {
            refusal = Some((result, message.to_string()));
        }
    }
    refusal.map(Err)
}

/// The JSON a manifest field holds, whether given as an object, as its text, or wrapped
/// the way Apollo wraps JSON scalars.
fn manifest_of(value: &Value) -> Option<Value> {
    match value {
        Value::Object(map) if map.get("type").and_then(|t| t.as_str()) == Some("json") => {
            manifest_of(map.get("json")?)
        }
        Value::Object(_) => Some(value.clone()),
        Value::String(text) => serde_json::from_str(text).ok(),
        _ => None,
    }
}

pub struct KuaishouResolver {
    http: Http,
}

impl KuaishouResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    fn logged_in(&self) -> bool {
        self.http.jar(PLATFORM).get(SESSION_COOKIE).is_some()
    }

    fn verification(&self, origin: &Url) -> ResolveError {
        if self.logged_in() {
            ResolveError::unavailable(
                origin,
                "Kuaishou asks this client to verify itself before it answers",
            )
        } else {
            ResolveError::login_required(
                origin,
                PLATFORM,
                "Kuaishou asks unfamiliar clients to verify themselves; a browser's session passes",
            )
        }
    }

    /// The desktop page's state for the video; `None` when the page carries no state,
    /// as it does when the site answers with a verification page.
    async fn desktop(&self, id: &str, origin: &Url) -> Result<Option<DetailState>, ResolveError> {
        let url = Url::parse(&format!("{SITE}short-video/{id}")).expect("valid");
        let headers = [("accept-language".to_string(), "zh-CN,zh;q=0.9".to_string())];
        let fetched = match fetch(&self.http, &url, PLATFORM, BROWSER_UA, &headers, MAX_PAGE).await
        {
            Ok(fetched) => fetched,
            Err(ResolveError::Http(error)) if error.is_retryable() => {
                tracing::debug!(%origin, "kuaishou desktop page unreachable: {error}");
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        match fetched.status.as_u16() {
            200..=299 => {}
            404 | 410 => return Err(ResolveError::NotFound(origin.clone())),
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            _ => return Ok(None),
        }
        Ok(detail_state(&fetched.text(), id))
    }

    /// The mobile share page's photo record.
    async fn mobile(
        &self,
        id: &str,
        landing: Option<&Url>,
        origin: &Url,
    ) -> Result<Value, ResolveError> {
        let url = match landing {
            Some(landing) => landing.clone(),
            None => Url::parse(&format!("{MOBILE_SHARE}{id}")).expect("valid"),
        };
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
        match mobile_state(&fetched.text()) {
            Some(Ok(record)) => Ok(record),
            Some(Err((223, _))) => Err(ResolveError::NotFound(origin.clone())),
            Some(Err((code, message))) => Err(ResolveError::unavailable(
                origin,
                format!("{message} (result {code})"),
            )),
            None => Err(self.verification(origin)),
        }
    }

    async fn video(
        &self,
        id: &str,
        landing: Option<&Url>,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let mut verification = false;
        if let Some(state) = self.desktop(id, origin).await? {
            match (state.status, state.photo) {
                (_, Some(photo)) => {
                    return desktop_resolution(id, &photo, state.author.as_ref(), origin);
                }
                (Some(STATUS_VERIFY), None) | (Some(1041), None) => verification = true,
                (Some(2), None) => return Err(ResolveError::NotFound(origin.clone())),
                (Some(status), None) if status != 1 => {
                    return Err(ResolveError::unavailable(
                        origin,
                        format!("the page answered status {status}"),
                    ));
                }
                (_, None) => {}
            }
        }
        match self.mobile(id, landing, origin).await {
            Ok(record) => mobile_resolution(id, &record, origin),
            Err(ResolveError::LoginRequired { .. } | ResolveError::Unavailable { .. })
                if verification =>
            {
                Err(self.verification(origin))
            }
            Err(error) => Err(error),
        }
    }
}

fn headers() -> Vec<(String, String)> {
    vec![("referer".to_string(), SITE.to_string())]
}

/// Whether two links name the same file: the CDN serves one file from several hosts
/// under one path, each with its own signature.
fn same_file(a: &Url, b: &Url) -> bool {
    a.path() == b.path()
}

/// The codec a representation's own note names, such as `ttExplain=HEVC_Turbo2_720P`.
fn codec_of(rep: &Value, fallback: &VideoCodec) -> VideoCodec {
    let note = rep["comment"]
        .as_str()
        .or_else(|| rep["codecs"].as_str())
        .unwrap_or("")
        .to_ascii_uppercase();
    if note.contains("HEVC") || note.contains("H265") || note.contains("HVC1") {
        VideoCodec::H265
    } else if note.contains("AV1") || note.contains("AV01") {
        VideoCodec::Av1
    } else if note.contains("AVC") || note.contains("H264") {
        VideoCodec::H264
    } else {
        fallback.clone()
    }
}

/// The variants a manifest's representations make.
fn manifest_variants(
    manifest: &Value,
    codec: VideoCodec,
    duration: Option<Duration>,
    out: &mut Vec<Variant>,
) {
    for set in manifest["adaptationSet"].as_array().into_iter().flatten() {
        for rep in set["representation"].as_array().into_iter().flatten() {
            let Some(url) = rep["url"]
                .as_str()
                .or_else(|| {
                    rep["backupUrl"]
                        .as_array()
                        .and_then(|b| b.iter().find_map(|u| u.as_str()))
                })
                .and_then(|u| Url::parse(u).ok())
            else {
                continue;
            };
            if out.iter().any(|v| same_file(&v.url, &url)) {
                continue;
            }
            let mut v = Variant::new(url, VariantKind::File);
            v.container = Some(Container::Mp4);
            v.video = Some(codec_of(rep, &codec));
            v.audio = Some(AudioCodec::Aac);
            v.width = rep["width"].as_u64().map(|w| w as u32);
            v.height = rep["height"].as_u64().map(|h| h as u32);
            v.fps = rep["frameRate"].as_f64().filter(|f| *f > 0.0);
            v.bitrate = rep["avgBitrate"]
                .as_u64()
                .or_else(|| rep["maxBitrate"].as_u64())
                .filter(|b| *b > 0)
                .map(|kbps| kbps * 1000);
            v.size = rep["fileSize"].as_u64().filter(|s| *s > 0);
            v.duration = duration.or_else(|| {
                set["duration"]
                    .as_u64()
                    .filter(|d| *d > 0)
                    .map(Duration::from_millis)
            });
            v.format_id = rep["id"].as_u64().map(|id| id.to_string());
            v.label = rep["qualityLabel"].as_str().map(String::from);
            v.headers = headers();
            out.push(v);
        }
    }
}

fn plain_variant(
    url: &str,
    codec: VideoCodec,
    duration: Option<Duration>,
    name: &str,
    out: &mut Vec<Variant>,
) {
    let Some(url) = Url::parse(url).ok() else {
        return;
    };
    if out.iter().any(|v| same_file(&v.url, &url)) {
        return;
    }
    let mut v = Variant::new(url, VariantKind::File);
    v.container = Some(Container::Mp4);
    v.video = Some(codec);
    v.audio = Some(AudioCodec::Aac);
    v.duration = duration;
    v.format_id = Some(name.to_string());
    v.headers = headers();
    out.push(v);
}

/// The variants the desktop page's photo record lists.
pub fn desktop_variants(photo: &Value, duration: Option<Duration>) -> Vec<Variant> {
    let mut out = Vec::new();
    if let Some(manifest) = manifest_of(&photo["manifest"]) {
        manifest_variants(&manifest, VideoCodec::H264, duration, &mut out);
    }
    if let Some(manifest) = manifest_of(&photo["manifestH265"]) {
        manifest_variants(&manifest, VideoCodec::H265, duration, &mut out);
    }
    if let Some(resource) = manifest_of(&photo["videoResource"]) {
        for (key, codec) in [("h264", VideoCodec::H264), ("hevc", VideoCodec::H265)] {
            if resource[key].is_object() {
                manifest_variants(&resource[key], codec, duration, &mut out);
            }
        }
    }
    if let Some(url) = photo["photoUrl"].as_str() {
        plain_variant(url, VideoCodec::H264, duration, "photoUrl", &mut out);
    }
    if let Some(url) = photo["photoH265Url"].as_str() {
        plain_variant(url, VideoCodec::H265, duration, "photoH265Url", &mut out);
    }
    out
}

fn desktop_resolution(
    id: &str,
    photo: &Value,
    author: Option<&Value>,
    origin: &Url,
) -> Result<Resolution, ResolveError> {
    let duration = photo["duration"]
        .as_u64()
        .or_else(|| photo["duration"].as_str().and_then(|d| d.parse().ok()))
        .filter(|d| *d > 0)
        .map(Duration::from_millis);
    let variants = desktop_variants(photo, duration);
    if variants.is_empty() {
        return Err(ResolveError::NotFound(origin.clone()));
    }
    let mut resolved = Resolved::new(PLATFORM);
    resolved.id = Some(id.to_string());
    resolved.title = photo["caption"].as_str().and_then(clean_title);
    resolved.description = photo["caption"].as_str().and_then(clean_title);
    resolved.uploader = author
        .and_then(|a| a["name"].as_str())
        .and_then(clean_title);
    resolved.uploader_url = author
        .and_then(|a| a["id"].as_str())
        .and_then(|uid| Url::parse(&format!("{SITE}profile/{uid}")).ok());
    resolved.uploaded_at = photo["timestamp"]
        .as_i64()
        .or_else(|| photo["timestamp"].as_str().and_then(|t| t.parse().ok()))
        .and_then(|t| Timestamp::from_millisecond(t).ok());
    resolved.duration = duration;
    resolved.thumbnail = photo["coverUrl"].as_str().and_then(|u| Url::parse(u).ok());
    resolved.webpage_url = Url::parse(&format!("{SITE}short-video/{id}")).ok();
    resolved.clip = timestamp_hint(origin).map(|start| ClipRange { start, end: None });
    resolved.variants = variants;
    Ok(Resolution::from(resolved))
}

/// The variants the mobile share page's photo record lists.
pub fn mobile_variants(photo: &Value, duration: Option<Duration>) -> Vec<Variant> {
    let mut out = Vec::new();
    if let Some(manifest) = manifest_of(&photo["manifest"]) {
        manifest_variants(&manifest, VideoCodec::H264, duration, &mut out);
    }
    if let Some(manifest) = manifest_of(&photo["manifestH265"]) {
        manifest_variants(&manifest, VideoCodec::H265, duration, &mut out);
    }
    for (index, item) in photo["mainMvUrls"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        if let Some(url) = item["url"].as_str() {
            plain_variant(
                url,
                VideoCodec::H264,
                duration,
                &format!("mainMvUrls-{index}"),
                &mut out,
            );
        }
    }
    out
}

fn mobile_resolution(id: &str, record: &Value, origin: &Url) -> Result<Resolution, ResolveError> {
    let photo = &record["photo"];
    let duration = photo["duration"]
        .as_u64()
        .filter(|d| *d > 0)
        .map(Duration::from_millis);
    let variants = mobile_variants(photo, duration);
    if variants.is_empty() {
        return Err(ResolveError::NotFound(origin.clone()));
    }
    let mut resolved = Resolved::new(PLATFORM);
    resolved.id = Some(id.to_string());
    resolved.title = photo["caption"].as_str().and_then(clean_title);
    resolved.description = photo["caption"].as_str().and_then(clean_title);
    resolved.uploader = photo["userName"].as_str().and_then(clean_title);
    resolved.uploader_url = photo["userEid"]
        .as_str()
        .or_else(|| photo["userId"].as_str())
        .and_then(|uid| Url::parse(&format!("{SITE}profile/{uid}")).ok());
    resolved.uploaded_at = photo["timestamp"]
        .as_i64()
        .and_then(|t| Timestamp::from_millisecond(t).ok());
    resolved.duration = duration;
    resolved.thumbnail = photo["coverUrls"]
        .as_array()
        .and_then(|list| list.iter().find_map(|c| c["url"].as_str()))
        .and_then(|u| Url::parse(u).ok());
    resolved.webpage_url = Url::parse(&format!("{SITE}short-video/{id}")).ok();
    resolved.clip = timestamp_hint(origin).map(|start| ClipRange { start, end: None });
    resolved.variants = variants;
    Ok(Resolution::from(resolved))
}

#[async_trait]
impl Resolver for KuaishouResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Kuaishou",
            hosts: &["kuaishou.com", "v.kuaishou.com", "chenzhongtech.com"],
            features: &["videos", "short links", "share pages"],
            formats: &["mp4"],
            session: SessionSupport::Optional,
            examples: &[
                "https://www.kuaishou.com/short-video/3x7qbj8xmmgbjbu",
                "https://v.kuaishou.com/OSuHs3",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Video(id) => self.video(&id, None, url).await,
            Link::Short(short) => {
                let target = self
                    .http
                    .unwrap_redirects(&short, Some(PLATFORM), MOBILE_UA)
                    .await?;
                match parse_link(&target) {
                    Some(Link::Video(id)) => self.video(&id, Some(&target), url).await,
                    _ => Err(ResolveError::NotFound(url.clone())),
                }
            }
        }
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        if !self.logged_in() {
            return Ok(SessionCheck::LoggedOut);
        }
        let body = json!({
            "operationName": "visionProfile",
            "variables": {"userId": ""},
            "query": "query visionProfile($userId: String) { visionProfile(userId: $userId) { result userProfile { profile { user_name user_id __typename } __typename } __typename } }"
        });
        let response = self
            .http
            .post(Url::parse(GRAPHQL).expect("valid"))
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("origin", "https://www.kuaishou.com")
            .header("referer", SITE)
            .json(&body)
            .send()
            .await?;
        if !response.is_success() {
            return Ok(SessionCheck::LoggedOut);
        }
        let value: Value = response.json(MAX_PAGE).await.map_err(|e| {
            ResolveError::malformed(&Url::parse(GRAPHQL).expect("valid"), e.to_string())
        })?;
        let profile = &value["data"]["visionProfile"];
        Ok(
            match profile["userProfile"]["profile"]["user_name"].as_str() {
                Some(name) if profile["result"].as_i64() == Some(1) && !name.is_empty() => {
                    SessionCheck::LoggedIn {
                        account: name.to_string(),
                    }
                }
                _ => SessionCheck::LoggedOut,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

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

    fn manifest() -> Value {
        json!({"mediaType": 1, "version": "1.0", "adaptationSet": [{"id": 1, "duration": 12345, "representation": [
            {"id": 1, "url": "https://v2.kwaicdn.com/720.mp4", "backupUrl": ["https://v3.kwaicdn.com/720.mp4"], "maxBitrate": 2000, "avgBitrate": 1500, "height": 1280, "width": 720, "frameRate": 30.0, "qualityType": "720p", "qualityLabel": "高清"},
            {"id": 2, "url": "https://v2.kwaicdn.com/1080.mp4", "maxBitrate": 4000, "avgBitrate": 3000, "height": 1920, "width": 1080, "frameRate": 30.0, "qualityType": "1080p", "qualityLabel": "超清", "comment": "videoId=1/ttExplain=HEVC_Turbo2_1080P/tt=hd15", "fileSize": 7654321}
        ]}]})
    }

    fn desktop_page(status: i64, with_photo: bool) -> String {
        let mut client = json!({"ROOT_QUERY": {"visionVideoDetail({\"page\":\"detail\",\"photoId\":\"3xtdbxt3pj5a4xc\"})": {"status": status, "photo": null, "author": null}}});
        if with_photo {
            // The manifest is normalized the way Apollo stores it: references to entries,
            // and JSON scalars wrapped.
            client["VisionVideoDetailPhoto:3xtdbxt3pj5a4xc"] = json!({"id": "3xtdbxt3pj5a4xc", "duration": 12345, "caption": "今天的猫 #猫", "coverUrl": "https://p2.a.yximgs.com/cover.jpg",
                "photoUrl": "https://v2.kwaicdn.com/plain.mp4", "photoH265Url": "https://v3.kwaicdn.com/1080.mp4?pkey=other",
                "manifest": {"type": "id", "generated": true, "id": "$VisionVideoDetailPhoto:3xtdbxt3pj5a4xc.manifest", "typename": "VisionVideoManifest"},
                "manifestH265": {"type": "json", "json": null},
                "timestamp": "1700000000000"});
            client["$VisionVideoDetailPhoto:3xtdbxt3pj5a4xc.manifest"] = json!({"mediaType": 1, "version": "1.0",
                "adaptationSet": [{"type": "id", "generated": true, "id": "$VisionVideoDetailPhoto:3xtdbxt3pj5a4xc.manifest.adaptationSet.0"}]});
            client["$VisionVideoDetailPhoto:3xtdbxt3pj5a4xc.manifest.adaptationSet.0"] = json!({"id": 1, "duration": 12345,
                "representation": manifest()["adaptationSet"][0]["representation"]});
            client["VisionVideoDetailAuthor:3xabc"] = json!({"id": "3xabc", "name": "某人", "headerUrl": "https://p.yximgs.com/head.jpg"});
        }
        let state = json!({"defaultClient": client, "clients": {}});
        format!(
            "<html><head><script>window.__APOLLO_STATE__={state};(function(){{var s;}})();</script></head><body></body></html>"
        )
    }

    fn mobile_page(value: Value) -> String {
        let state =
            json!({"tusjoh.abc": {"result": 1, "activityInfoList": []}, "tusjoh.def": value});
        format!("<html><script>window.INIT_STATE = {state}</script></html>")
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://www.kuaishou.com/short-video/3xtdbxt3pj5a4xc?streamSource=theater"),
            Some(Link::Video("3xtdbxt3pj5a4xc".into()))
        );
        assert_eq!(
            link(
                "https://kphk9mez.m.chenzhongtech.com/fw/photo/3xr4rd4rxfsh6jc?cc=share_copylink&photoId=3xr4rd4rxfsh6jc"
            ),
            Some(Link::Video("3xr4rd4rxfsh6jc".into()))
        );
        assert!(matches!(
            link("https://v.kuaishou.com/2wH7DR"),
            Some(Link::Short(_))
        ));
        assert!(matches!(
            link("https://www.kuaishou.com/f/X4abc"),
            Some(Link::Short(_))
        ));
        assert_eq!(link("https://www.kuaishou.com/profile/3xabc"), None);
        assert_eq!(link("https://www.kuaishou.com/short-video/bad"), None);
    }

    #[tokio::test]
    async fn desktop_state_gives_every_representation() {
        let mut fixture = Fixture::new("kuaishou", None);
        fixture.exchanges.push(exchange(
            "https://www.kuaishou.com/short-video/3xtdbxt3pj5a4xc",
            200,
            &desktop_page(1, true),
            &[("set-cookie", "did=web_abc; domain=kuaishou.com; path=/")],
        ));
        let http = Http::replay(fixture);
        let resolver = KuaishouResolver::new(http.clone());
        let url = Url::parse("https://www.kuaishou.com/short-video/3xtdbxt3pj5a4xc").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("3xtdbxt3pj5a4xc"));
        assert_eq!(resolved.title.as_deref(), Some("今天的猫 #猫"));
        assert_eq!(resolved.uploader.as_deref(), Some("某人"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://www.kuaishou.com/profile/3xabc"
        );
        assert_eq!(resolved.duration, Some(Duration::from_millis(12345)));
        assert!(resolved.uploaded_at.is_some());
        // Two representations, the plain H.264 file, and the plain H.265 file left out for
        // naming a representation's file on another host.
        assert_eq!(resolved.variants.len(), 3);
        assert_eq!(resolved.variants[0].video, Some(VideoCodec::H264));
        assert_eq!(resolved.variants[1].height, Some(1920));
        assert_eq!(resolved.variants[1].bitrate, Some(3_000_000));
        assert_eq!(resolved.variants[1].size, Some(7654321));
        assert_eq!(resolved.variants[1].label.as_deref(), Some("超清"));
        assert_eq!(resolved.variants[1].video, Some(VideoCodec::H265));
        assert_eq!(resolved.variants[2].format_id.as_deref(), Some("photoUrl"));
        assert_eq!(http.jar(PLATFORM).get("did").unwrap().value, "web_abc");
    }

    #[tokio::test]
    async fn verification_falls_through_to_the_mobile_page_and_is_reported() {
        let photo = json!({"result": 1, "photo": {"caption": "分享", "duration": 5000, "userName": "小明", "userEid": "3xming", "timestamp": 1700000000000_u64,
            "coverUrls": [{"url": "https://p.yximgs.com/c.jpg"}], "mainMvUrls": [{"url": "https://v.kwaicdn.com/main.mp4"}], "manifest": manifest()}});
        let mut fixture = Fixture::new("kuaishou", None);
        fixture.exchanges.push(exchange(
            "https://www.kuaishou.com/short-video/3xtdbxt3pj5a4xc",
            200,
            &desktop_page(1040, false),
            &[],
        ));
        fixture.exchanges.push(exchange(
            "https://v.m.chenzhongtech.com/fw/photo/3xtdbxt3pj5a4xc",
            200,
            &mobile_page(photo),
            &[],
        ));
        fixture.exchanges.push(exchange(
            "https://www.kuaishou.com/short-video/3xtdbxt3pj5a4xc",
            200,
            &desktop_page(1040, false),
            &[],
        ));
        fixture.exchanges.push(exchange(
            "https://v.m.chenzhongtech.com/fw/photo/3xtdbxt3pj5a4xc",
            200,
            &mobile_page(json!({"result": 226, "error_msg": "作品在审核中，请稍后观看。"})),
            &[],
        ));
        fixture.exchanges.push(exchange(
            "https://www.kuaishou.com/short-video/3xtdbxt3pj5a4xc",
            200,
            &desktop_page(2, false),
            &[],
        ));
        fixture.exchanges.push(exchange(
            "https://www.kuaishou.com/short-video/3xtdbxt3pj5a4xc",
            200,
            "<html>verify</html>",
            &[],
        ));
        fixture.exchanges.push(exchange(
            "https://v.m.chenzhongtech.com/fw/photo/3xtdbxt3pj5a4xc",
            200,
            &mobile_page(
                json!({"result": 223, "error_msg": "获取失败，作品可能已被删除或尚未上传"}),
            ),
            &[],
        ));
        let resolver = KuaishouResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.kuaishou.com/short-video/3xtdbxt3pj5a4xc").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.uploader.as_deref(), Some("小明"));
        assert_eq!(resolved.variants.len(), 3);
        assert_eq!(
            resolved.variants[2].format_id.as_deref(),
            Some("mainMvUrls-0")
        );
        let error = resolver.resolve(&url).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::LoginRequired { reason, .. } if reason.contains("verify")),
            "{error}"
        );
        assert!(matches!(
            resolver.resolve(&url).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
        assert!(matches!(
            resolver.resolve(&url).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn short_links_land_on_the_mobile_share_page() {
        let landing = "https://kphk9mez.m.chenzhongtech.com/fw/photo/3xr4rd4rxfsh6jc?cc=share_copylink&photoId=3xr4rd4rxfsh6jc";
        let mut fixture = Fixture::new("kuaishou", None);
        fixture.exchanges.push(exchange(
            "https://v.kuaishou.com/2wH7DR",
            302,
            "",
            &[("location", landing)],
        ));
        fixture
            .exchanges
            .push(exchange(landing, 200, "<html></html>", &[]));
        fixture.exchanges.push(exchange(
            "https://www.kuaishou.com/short-video/3xr4rd4rxfsh6jc",
            200,
            "<html>verify</html>",
            &[],
        ));
        fixture.exchanges.push(exchange(
            landing,
            200,
            &mobile_page(json!({"result": 1, "photo": {"caption": "x", "duration": 1000, "mainMvUrls": [{"url": "https://v.kwaicdn.com/m.mp4"}]}})),
            &[],
        ));
        let resolver = KuaishouResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://v.kuaishou.com/2wH7DR").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("3xr4rd4rxfsh6jc"));
        assert_eq!(
            resolved.variants[0].url.as_str(),
            "https://v.kwaicdn.com/m.mp4"
        );
    }
}
