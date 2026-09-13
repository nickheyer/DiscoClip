//! Niconico videos, through the data the watch page hands its player and the access
//! rights the delivery API grants for an HLS stream, with `nico.ms` short links
//! unwrapped; mylists, series and user pages become playlists through the site's API.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::{Value, json};
use url::Url;

use super::page::Page;
use super::{
    ClipRange, MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionCheck, SessionSupport, Variant, clean_title, fetch, hls, timestamp_hint,
};
use crate::http::{BROWSER_UA, Http};

pub const PLATFORM: &str = "niconico";
const SITE: &str = "https://www.nicovideo.jp/";
const NVAPI: &str = "https://nvapi.nicovideo.jp";
/// The cookie a logged-in nicovideo.jp session carries.
const SESSION_COOKIE: &str = "user_session";
const PAGE_SIZE: usize = 100;
const MAX_PAGES: usize = 5;

static RE_VIDEO_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:(?:sm|nm|so)\d+|\d+)$").unwrap());

/// What a link names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Video(String),
    Mylist(String),
    Series(String),
    /// A user's uploads.
    User(String),
}

fn digits(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_digit())
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
    if host == "nico.ms" {
        return match segments.as_slice() {
            [id] if RE_VIDEO_ID.is_match(id) => Some(Link::Video(id.to_string())),
            [id] if id.starts_with("mylist/") => None,
            _ => None,
        };
    }
    if !(host == "nicovideo.jp" || host.ends_with(".nicovideo.jp")) {
        return None;
    }
    match segments.as_slice() {
        ["watch", id, ..] if RE_VIDEO_ID.is_match(id) => Some(Link::Video(id.to_string())),
        ["mylist", id] | ["user", _, "mylist", id] if digits(id) => {
            Some(Link::Mylist(id.to_string()))
        }
        ["series", id] | ["user", _, "series", id] if digits(id) => {
            Some(Link::Series(id.to_string()))
        }
        ["user", id] | ["user", id, "video"] if digits(id) => Some(Link::User(id.to_string())),
        _ => None,
    }
}

/// The `server-response` record the watch page carries, as JSON.
pub fn server_response(page: &Page) -> Option<Value> {
    let content = page.meta("server-response")?;
    serde_json::from_str(&content).ok()
}

pub struct NiconicoResolver {
    http: Http,
}

impl NiconicoResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    fn logged_in(&self) -> bool {
        self.http.jar(PLATFORM).get(SESSION_COOKIE).is_some()
    }

    /// Refusals the watch page states in its record, by the reason it gives.
    fn refusal(&self, response: &Value, status: u16, origin: &Url) -> ResolveError {
        let reason = response["reasonCode"]
            .as_str()
            .or_else(|| response["errorCode"].as_str())
            .or_else(|| response["okReason"].as_str())
            .unwrap_or("")
            .to_string();
        let upper = reason.to_ascii_uppercase();
        if status == 404 || upper.contains("DELETE") || upper.contains("NOT_FOUND") {
            return ResolveError::NotFound(origin.clone());
        }
        let needs_account = upper.contains("PREMIUM")
            || upper.contains("PPV")
            || upper.contains("ADMISSION")
            || upper.contains("LOGIN")
            || upper.contains("AUTHENTICATION")
            || upper.contains("CHANNEL_MEMBER")
            || upper.contains("HIDDEN")
            || upper.contains("DOMESTIC")
            || upper.contains("FORBIDDEN") && !self.logged_in();
        let message = if reason.is_empty() {
            format!("the watch page answered status {status}")
        } else {
            reason.clone()
        };
        if needs_account && !self.logged_in() {
            ResolveError::login_required(origin, PLATFORM, message)
        } else {
            ResolveError::unavailable(origin, message)
        }
    }

    /// The watch page's record of a video.
    async fn watch(&self, id: &str, origin: &Url) -> Result<Value, ResolveError> {
        let page_url = Url::parse(&format!("{SITE}watch/{id}")).expect("valid");
        let fetched = fetch(&self.http, &page_url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        let status = fetched.status.as_u16();
        if status == 429 {
            return Err(ResolveError::RateLimited(origin.clone()));
        }
        let html = fetched.text();
        let page = Page::parse(&html, &page_url);
        let Some(record) = server_response(&page) else {
            return Err(match status {
                404 | 410 => ResolveError::NotFound(origin.clone()),
                200..=299 => ResolveError::malformed(origin, "the watch page has no player data"),
                _ => ResolveError::unavailable(
                    origin,
                    format!("the watch page answered HTTP {status}"),
                ),
            });
        };
        let response = &record["data"]["response"];
        let meta_status = record["meta"]["status"].as_u64().unwrap_or(status as u64) as u16;
        if !(200..300).contains(&meta_status) || response["video"].is_null() {
            return Err(self.refusal(response, meta_status, origin));
        }
        Ok(response.clone())
    }

    /// Asks the delivery API for an HLS stream of every video quality with the best audio,
    /// which answers with one master playlist and the cookie its segments want.
    async fn access_rights(
        &self,
        id: &str,
        response: &Value,
        origin: &Url,
    ) -> Result<Url, ResolveError> {
        let domand = &response["media"]["domand"];
        if domand.is_null() {
            let payment = &response["payment"]["video"];
            let reason = if payment["isPremium"].as_bool() == Some(true) {
                "the video is for premium members"
            } else if payment["isPpv"].as_bool() == Some(true) {
                "the video is pay-per-view"
            } else if payment["isAdmission"].as_bool() == Some(true) {
                "the video is for channel members"
            } else {
                "the player was given no stream"
            };
            return Err(if self.logged_in() {
                ResolveError::unavailable(origin, reason)
            } else {
                ResolveError::login_required(origin, PLATFORM, reason)
            });
        }
        let key = domand["accessRightKey"]
            .as_str()
            .filter(|k| !k.is_empty())
            .ok_or_else(|| {
                ResolveError::malformed(origin, "the player data has no access right key")
            })?;
        let track = response["client"]["watchTrackId"]
            .as_str()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| ResolveError::malformed(origin, "the player data has no track id"))?;
        let audio = domand["audios"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|a| a["isAvailable"].as_bool() == Some(true))
            .max_by_key(|a| a["qualityLevel"].as_i64().unwrap_or(0))
            .and_then(|a| a["id"].as_str())
            .ok_or_else(|| ResolveError::malformed(origin, "the player data has no audio"))?;
        let outputs: Vec<Value> = domand["videos"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|v| v["isAvailable"].as_bool() == Some(true))
            .filter_map(|v| v["id"].as_str())
            .map(|video| json!([video, audio]))
            .collect();
        if outputs.is_empty() {
            return Err(ResolveError::malformed(
                origin,
                "the player data has no video",
            ));
        }
        let mut url =
            Url::parse(&format!("{NVAPI}/v1/watch/{id}/access-rights/hls")).expect("valid");
        url.query_pairs_mut().append_pair("actionTrackId", track);
        let response = self
            .http
            .post(url)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("x-access-right-key", key)
            .header("x-frontend-id", "6")
            .header("x-frontend-version", "0")
            .header("x-request-with", "https://www.nicovideo.jp")
            .header("referer", &format!("{SITE}watch/{id}"))
            .header("origin", "https://www.nicovideo.jp")
            .json(&json!({"outputs": outputs}))
            .send()
            .await?;
        let status = response.status;
        let answer: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, format!("access rights: {e}")))?;
        if !status.is_success() {
            let code = answer["meta"]["errorCode"]
                .as_str()
                .unwrap_or("")
                .to_string();
            return Err(match status.as_u16() {
                429 => ResolveError::RateLimited(origin.clone()),
                401 | 403 if !self.logged_in() => ResolveError::login_required(
                    origin,
                    PLATFORM,
                    format!("the delivery API refused the stream ({code})"),
                ),
                _ => ResolveError::unavailable(
                    origin,
                    format!("the delivery API answered HTTP {status} {code}"),
                ),
            });
        }
        answer["data"]["contentUrl"]
            .as_str()
            .and_then(|u| Url::parse(u).ok())
            .ok_or_else(|| ResolveError::malformed(origin, "the access rights carry no stream"))
    }

    async fn video(&self, id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let response = self.watch(id, origin).await?;
        let video = &response["video"];
        let master = self.access_rights(id, &response, origin).await?;
        let expanded = hls::expand(&self.http, &master, PLATFORM, BROWSER_UA, &[]).await?;
        let duration = video["duration"]
            .as_f64()
            .filter(|d| *d > 0.0)
            .map(Duration::from_secs_f64)
            .or(expanded.duration);
        let mut variants: Vec<Variant> = expanded.variants;
        for v in &mut variants {
            v.duration = duration;
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = video["id"]
            .as_str()
            .map(String::from)
            .or_else(|| Some(id.to_string()));
        resolved.title = video["title"].as_str().and_then(clean_title);
        resolved.description = video["description"]
            .as_str()
            .map(strip_tags)
            .and_then(|d| clean_title(&d));
        let owner = &response["owner"];
        let channel = &response["channel"];
        resolved.uploader = owner["nickname"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| channel["name"].as_str().and_then(clean_title));
        resolved.uploader_url = owner["id"]
            .as_u64()
            .and_then(|uid| Url::parse(&format!("{SITE}user/{uid}")).ok())
            .or_else(|| {
                channel["id"]
                    .as_str()
                    .and_then(|cid| Url::parse(&format!("https://ch.nicovideo.jp/{cid}")).ok())
            });
        resolved.uploaded_at = video["registeredAt"]
            .as_str()
            .and_then(|t| t.parse::<Timestamp>().ok());
        resolved.duration = duration;
        resolved.thumbnail = ["ogp", "player", "largeUrl", "middleUrl", "url"]
            .iter()
            .find_map(|k| video["thumbnail"][k].as_str())
            .and_then(|u| Url::parse(u).ok());
        resolved.webpage_url = Url::parse(&format!("{SITE}watch/{id}")).ok();
        resolved.age_limit = video["rating"]["isAdult"]
            .as_bool()
            .filter(|a| *a)
            .map(|_| 18);
        resolved.clip = timestamp_hint(origin).map(|start| ClipRange { start, end: None });
        resolved.subtitles = expanded.subtitles;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    /// GETs an nvapi path as the web front end.
    async fn nvapi(&self, path: &str, origin: &Url) -> Result<Value, ResolveError> {
        let url = Url::parse(&format!("{NVAPI}{path}"))
            .map_err(|e| ResolveError::malformed(origin, e.to_string()))?;
        let headers = [
            ("x-frontend-id".to_string(), "6".to_string()),
            ("x-frontend-version".to_string(), "0".to_string()),
            ("referer".to_string(), SITE.to_string()),
        ];
        let fetched = fetch(&self.http, &url, PLATFORM, BROWSER_UA, &headers, MAX_PAGE).await?;
        match fetched.status.as_u16() {
            200..=299 => fetched.json(origin),
            404 => Err(ResolveError::NotFound(origin.clone())),
            429 => Err(ResolveError::RateLimited(origin.clone())),
            401 | 403 if !self.logged_in() => Err(ResolveError::login_required(
                origin,
                PLATFORM,
                "the list is not public",
            )),
            status => Err(ResolveError::unavailable(
                origin,
                format!("the API answered HTTP {status}"),
            )),
        }
    }

    fn entry(video: &Value) -> Option<PlaylistEntry> {
        let id = video["id"].as_str()?;
        Some(PlaylistEntry {
            url: Url::parse(&format!("{SITE}watch/{id}")).ok()?,
            title: video["title"].as_str().and_then(clean_title),
            duration: video["duration"]
                .as_u64()
                .filter(|d| *d > 0)
                .map(Duration::from_secs),
        })
    }

    async fn mylist(&self, id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let mut entries = Vec::new();
        let mut title = None;
        let mut total = None;
        for page in 1..=MAX_PAGES {
            let answer = self
                .nvapi(
                    &format!("/v2/mylists/{id}?pageSize={PAGE_SIZE}&page={page}"),
                    origin,
                )
                .await?;
            let mylist = &answer["data"]["mylist"];
            if title.is_none() {
                title = mylist["name"].as_str().and_then(clean_title);
                total = mylist["totalItemCount"].as_u64().map(|t| t as usize);
            }
            let items: Vec<&Value> = mylist["items"].as_array().into_iter().flatten().collect();
            entries.extend(items.iter().filter_map(|item| Self::entry(&item["video"])));
            if mylist["hasNext"].as_bool() != Some(true) || items.is_empty() {
                break;
            }
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.into(),
            id: Some(format!("mylist{id}")),
            title,
            total: total
                .filter(|t| *t >= entries.len())
                .or(Some(entries.len())),
            entries,
        }))
    }

    async fn series(&self, id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let mut entries = Vec::new();
        let mut title = None;
        let mut total = None;
        for page in 1..=MAX_PAGES {
            let answer = self
                .nvapi(
                    &format!("/v2/series/{id}?pageSize={PAGE_SIZE}&page={page}"),
                    origin,
                )
                .await?;
            let data = &answer["data"];
            if title.is_none() {
                title = data["detail"]["title"].as_str().and_then(clean_title);
                total = data["totalCount"].as_u64().map(|t| t as usize);
            }
            let items: Vec<&Value> = data["items"].as_array().into_iter().flatten().collect();
            entries.extend(items.iter().filter_map(|item| Self::entry(&item["video"])));
            if items.len() < PAGE_SIZE || total.is_some_and(|t| entries.len() >= t) {
                break;
            }
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.into(),
            id: Some(format!("series{id}")),
            title,
            total: total
                .filter(|t| *t >= entries.len())
                .or(Some(entries.len())),
            entries,
        }))
    }

    async fn user(&self, id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let mut entries = Vec::new();
        let mut total = None;
        for page in 1..=MAX_PAGES {
            let answer = self
                .nvapi(
                    &format!(
                        "/v3/users/{id}/videos?sortKey=registeredAt&sortOrder=desc&pageSize={PAGE_SIZE}&page={page}"
                    ),
                    origin,
                )
                .await?;
            let data = &answer["data"];
            if total.is_none() {
                total = data["totalCount"].as_u64().map(|t| t as usize);
            }
            let items: Vec<&Value> = data["items"].as_array().into_iter().flatten().collect();
            entries.extend(
                items
                    .iter()
                    .filter_map(|item| Self::entry(&item["essential"])),
            );
            if items.len() < PAGE_SIZE || total.is_some_and(|t| entries.len() >= t) {
                break;
            }
        }
        let title = self
            .nvapi(&format!("/v1/users/{id}"), origin)
            .await
            .ok()
            .and_then(|answer| {
                answer["data"]["user"]["nickname"]
                    .as_str()
                    .and_then(clean_title)
            })
            .map(|name| format!("{name}'s videos"));
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.into(),
            id: Some(format!("user{id}")),
            title,
            total: total
                .filter(|t| *t >= entries.len())
                .or(Some(entries.len())),
            entries,
        }))
    }
}

/// A description's text without its HTML tags, with breaks as spaces.
fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
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
    out.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&")
}

#[async_trait]
impl Resolver for NiconicoResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Niconico",
            hosts: &["nicovideo.jp", "nico.ms"],
            features: &["videos", "short links", "mylists", "series", "user pages"],
            formats: &["hls"],
            session: SessionSupport::Optional,
            examples: &[
                "https://www.nicovideo.jp/watch/sm9",
                "https://nico.ms/sm9",
                "https://www.nicovideo.jp/series/110226",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Video(id) => self.video(&id, url).await,
            Link::Mylist(id) => self.mylist(&id, url).await,
            Link::Series(id) => self.series(&id, url).await,
            Link::User(id) => self.user(&id, url).await,
        }
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        if !self.logged_in() {
            return Ok(SessionCheck::LoggedOut);
        }
        let url = Url::parse(&format!("{NVAPI}/v1/users/me")).expect("valid");
        let headers = [
            ("x-frontend-id".to_string(), "6".to_string()),
            ("x-frontend-version".to_string(), "0".to_string()),
        ];
        let fetched = fetch(&self.http, &url, PLATFORM, BROWSER_UA, &headers, MAX_PAGE).await?;
        if !fetched.status.is_success() {
            return Ok(SessionCheck::LoggedOut);
        }
        let value = fetched.json(&url)?;
        Ok(match value["data"]["user"]["nickname"].as_str() {
            Some(name) if !name.is_empty() => SessionCheck::LoggedIn {
                account: name.to_string(),
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

    fn exchange(
        method: &str,
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

    fn watch_page(response: Value, status: u64) -> String {
        let record = json!({"meta": {"status": status}, "data": {"response": response}});
        let escaped = record
            .to_string()
            .replace('&', "&amp;")
            .replace('"', "&quot;");
        format!(
            r#"<html><head><meta name="server-response" content="{escaped}"></head><body></body></html>"#
        )
    }

    fn response() -> Value {
        json!({
            "video": {"id": "sm9", "title": "新・豪血寺一族 -煩悩解放 - レッツゴー！陰陽師", "description": "レッツゴー！<br>陰陽師（フルコーラス&amp;バージョン）",
                "duration": 320, "registeredAt": "2007-03-06T00:33:00+09:00", "rating": {"isAdult": false},
                "thumbnail": {"url": "https://nicovideo.cdn.nimg.jp/thumbnails/9/9", "ogp": "https://img.cdn.nimg.jp/s/nicovideo/thumbnails/9/9.original/r1280x720l"}},
            "owner": {"id": 4, "nickname": "中の"},
            "channel": null,
            "client": {"watchId": "sm9", "watchTrackId": "TRACK_1"},
            "payment": {"video": {"isPremium": false, "isPpv": false, "isAdmission": false}},
            "media": {"domand": {
                "videos": [
                    {"id": "video-h264-360p", "isAvailable": true, "label": "360p", "bitRate": 602592, "width": 320, "height": 240, "qualityLevel": 1},
                    {"id": "video-h264-360p-lowest", "isAvailable": true, "label": "低画質", "bitRate": 302594, "qualityLevel": 0},
                    {"id": "video-h264-720p", "isAvailable": false, "label": "720p", "qualityLevel": 2}
                ],
                "audios": [
                    {"id": "audio-aac-128kbps", "isAvailable": true, "bitRate": 104747, "qualityLevel": 1},
                    {"id": "audio-aac-64kbps", "isAvailable": true, "bitRate": 78056, "qualityLevel": 0}
                ],
                "accessRightKey": "KEY_1"
            }}
        })
    }

    const MASTER: &str = "#EXTM3U\n\
        #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"audio-aac-128kbps\",DEFAULT=YES,URI=\"audio-aac-128kbps.m3u8\"\n\
        #EXT-X-STREAM-INF:BANDWIDTH=707339,RESOLUTION=320x240,CODECS=\"avc1.4d401e,mp4a.40.2\",AUDIO=\"audio\"\n\
        video-h264-360p.m3u8\n\
        #EXT-X-STREAM-INF:BANDWIDTH=407341,RESOLUTION=480x360,CODECS=\"avc1.4d401e,mp4a.40.2\",AUDIO=\"audio\"\n\
        video-h264-360p-lowest.m3u8\n";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\ns1.cmfv\n#EXTINF:6.0,\ns2.cmfv\n#EXT-X-ENDLIST\n";

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://www.nicovideo.jp/watch/sm9?ref=x"),
            Some(Link::Video("sm9".into()))
        );
        assert_eq!(
            link("https://sp.nicovideo.jp/watch/so44236913"),
            Some(Link::Video("so44236913".into()))
        );
        assert_eq!(
            link("https://embed.nicovideo.jp/watch/nm14893971"),
            Some(Link::Video("nm14893971".into()))
        );
        assert_eq!(link("https://nico.ms/sm9"), Some(Link::Video("sm9".into())));
        assert_eq!(
            link("https://www.nicovideo.jp/user/805797/mylist/27411728"),
            Some(Link::Mylist("27411728".into()))
        );
        assert_eq!(
            link("https://www.nicovideo.jp/mylist/27411728"),
            Some(Link::Mylist("27411728".into()))
        );
        assert_eq!(
            link("https://www.nicovideo.jp/series/110226"),
            Some(Link::Series("110226".into()))
        );
        assert_eq!(
            link("https://www.nicovideo.jp/user/4/video"),
            Some(Link::User("4".into()))
        );
        assert_eq!(link("https://www.nicovideo.jp/ranking"), None);
        assert_eq!(link("https://www.nicovideo.jp/watch/xyz"), None);
    }

    #[tokio::test]
    async fn videos_get_an_hls_stream_through_the_access_rights() {
        let mut fixture = Fixture::new("niconico", None);
        fixture.exchanges.push(exchange(
            "GET",
            "https://www.nicovideo.jp/watch/sm9",
            200,
            "text/html",
            &watch_page(response(), 200),
            &[],
        ));
        fixture.exchanges.push(exchange(
            "POST",
            "https://nvapi.nicovideo.jp/v1/watch/sm9/access-rights/hls?actionTrackId=TRACK_1",
            201,
            "application/json",
            &json!({"meta": {"status": 201}, "data": {"contentUrl": "https://delivery.domand.nicovideo.jp/hlsbid/x/master.m3u8?session=1"}}).to_string(),
            &[("set-cookie", "domand_bid=BID; Path=/; Domain=nicovideo.jp; Secure; HttpOnly")],
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://delivery.domand.nicovideo.jp/hlsbid/x/master.m3u8?session=1",
            200,
            "application/vnd.apple.mpegurl",
            MASTER,
            &[],
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://delivery.domand.nicovideo.jp/hlsbid/x/video-h264-360p.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MEDIA,
            &[],
        ));
        let http = Http::replay(fixture);
        let resolver = NiconicoResolver::new(http.clone());
        let url = Url::parse("https://nico.ms/sm9?from=30").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("sm9"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("新・豪血寺一族 -煩悩解放 - レッツゴー！陰陽師")
        );
        assert_eq!(
            resolved.description.as_deref(),
            Some("レッツゴー！ 陰陽師（フルコーラス&バージョン）")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("中の"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://www.nicovideo.jp/user/4"
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(320)));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.variants.len(), 2);
        assert_eq!(resolved.variants[0].height, Some(240));
        assert_eq!(
            resolved.variants[0].duration,
            Some(Duration::from_secs(320))
        );
        assert!(
            resolved.variants[0]
                .audio_url
                .as_ref()
                .unwrap()
                .as_str()
                .ends_with("audio-aac-128kbps.m3u8")
        );
        assert_eq!(http.jar(PLATFORM).get("domand_bid").unwrap().value, "BID");
    }

    #[tokio::test]
    async fn gone_premium_and_listed_videos() {
        let mut fixture = Fixture::new("niconico", None);
        fixture.exchanges.push(exchange(
            "GET",
            "https://www.nicovideo.jp/watch/sm1",
            400,
            "text/html",
            &watch_page(json!({"reasonCode": "DELETED_VIDEO"}), 400),
            &[],
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://www.nicovideo.jp/watch/sm2",
            400,
            "text/html",
            &watch_page(json!({"reasonCode": "ADMINISTRATOR_DELETE_VIDEO"}), 400),
            &[],
        ));
        let mut premium = response();
        premium["media"]["domand"] = Value::Null;
        premium["payment"]["video"]["isPremium"] = json!(true);
        fixture.exchanges.push(exchange(
            "GET",
            "https://www.nicovideo.jp/watch/so44236913",
            200,
            "text/html",
            &watch_page(premium, 200),
            &[],
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://www.nicovideo.jp/watch/sm999",
            404,
            "text/html",
            "<html>not found</html>",
            &[],
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://nvapi.nicovideo.jp/v2/series/110226?pageSize=100&page=1",
            200,
            "application/json",
            &json!({"meta": {"status": 200}, "data": {"detail": {"id": 110226, "title": "ご立派ァ！のシリーズ"}, "totalCount": 2, "items": [
                {"video": {"id": "sm36584256", "title": "ご立派ァ！.sugita", "duration": 2626}},
                {"video": {"id": "sm36585883", "title": "やったぜ。", "duration": 2632}}
            ]}}).to_string(),
            &[],
        ));
        let resolver = NiconicoResolver::new(Http::replay(fixture));
        let url = |s: &str| Url::parse(s).unwrap();
        assert!(matches!(
            resolver
                .resolve(&url("https://www.nicovideo.jp/watch/sm1"))
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        assert!(matches!(
            resolver
                .resolve(&url("https://www.nicovideo.jp/watch/sm2"))
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(&url("https://www.nicovideo.jp/watch/so44236913"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::LoginRequired { reason, .. } if reason.contains("premium")),
            "{error}"
        );
        assert!(matches!(
            resolver
                .resolve(&url("https://www.nicovideo.jp/watch/sm999"))
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let playlist = match resolver
            .resolve(&url("https://www.nicovideo.jp/series/110226"))
            .await
            .unwrap()
        {
            Resolution::Playlist(playlist) => playlist,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(playlist.title.as_deref(), Some("ご立派ァ！のシリーズ"));
        assert_eq!(playlist.total, Some(2));
        assert_eq!(
            playlist.entries[1].url.as_str(),
            "https://www.nicovideo.jp/watch/sm36585883"
        );
        assert_eq!(
            playlist.entries[1].duration,
            Some(Duration::from_secs(2632))
        );
    }

    #[tokio::test]
    async fn mylists_page_through_and_sessions_are_checked() {
        let mut fixture = Fixture::new("niconico", None);
        fixture.exchanges.push(exchange(
            "GET",
            "https://nvapi.nicovideo.jp/v2/mylists/27411728?pageSize=100&page=1",
            200,
            "application/json",
            &json!({"data": {"mylist": {"id": 27411728, "name": "AKB48のオールナイトニッポン", "totalItemCount": 3, "hasNext": true,
                "items": [{"watchId": "sm27718606", "video": {"id": "sm27718606", "title": "2015.12.02", "duration": 6269}},
                          {"watchId": "sm27681428", "video": {"id": "sm27681428", "title": "2015.11.25", "duration": 6270}}]}}}).to_string(),
            &[],
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://nvapi.nicovideo.jp/v2/mylists/27411728?pageSize=100&page=2",
            200,
            "application/json",
            &json!({"data": {"mylist": {"id": 27411728, "name": "AKB48のオールナイトニッポン", "totalItemCount": 3, "hasNext": false,
                "items": [{"watchId": "sm27631506", "video": {"id": "sm27631506", "title": "2015.11.18", "duration": 6270}}]}}}).to_string(),
            &[],
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://nvapi.nicovideo.jp/v1/users/me",
            200,
            "application/json",
            &json!({"data": {"user": {"id": 4, "nickname": "中の"}}}).to_string(),
            &[],
        ));
        let http = Http::replay(fixture);
        let resolver = NiconicoResolver::new(http.clone());
        let playlist = match resolver
            .resolve(&Url::parse("https://www.nicovideo.jp/user/805797/mylist/27411728").unwrap())
            .await
            .unwrap()
        {
            Resolution::Playlist(playlist) => playlist,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(playlist.entries.len(), 3);
        assert_eq!(playlist.total, Some(3));
        assert_eq!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedOut
        );
        http.with_jar(PLATFORM, |jar| {
            jar.insert(crate::http::Cookie::new(
                SESSION_COOKIE,
                "s",
                ".nicovideo.jp",
            ));
        });
        assert_eq!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedIn {
                account: "中の".into()
            }
        );
    }
}
