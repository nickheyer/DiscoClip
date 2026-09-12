//! TikTok videos, read from the data the web app renders its pages from, with short
//! links unwrapped first.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::page::Page;
use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionCheck, SessionSupport,
    SubtitleFormat, SubtitleTrack, Variant, VariantKind, clean_title, fetch_ok,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "tiktok";
const REFERER: &str = "https://www.tiktok.com/";
const ACCOUNT_INFO: &str = "https://www.tiktok.com/passport/web/account/info/?aid=1459";

static RE_VIDEO: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^/(?:@[^/]+/video/|v/|embed/v2/|player/v1/|embed/)(\d{15,})").unwrap()
});
static RE_PHOTO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/@[^/]+/photo/(\d{15,})").unwrap());

fn is_short_link(url: &Url) -> bool {
    match url.host_str() {
        Some("vm.tiktok.com") | Some("vt.tiktok.com") => true,
        Some(host) if host.ends_with("tiktok.com") => url.path().starts_with("/t/"),
        _ => false,
    }
}

fn is_tiktok_host(host: &str) -> bool {
    host == "tiktok.com" || host.ends_with(".tiktok.com")
}

/// The video id a page link names.
pub fn video_id(url: &Url) -> Option<String> {
    RE_VIDEO.captures(url.path()).map(|c| c[1].to_string())
}

pub struct TiktokResolver {
    http: Http,
}

impl TiktokResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// The video's page data, from the web app's rehydration blob.
    async fn item(&self, id: &str, origin: &Url) -> Result<Value, ResolveError> {
        let page_url = Url::parse(&format!("https://www.tiktok.com/@_/video/{id}")).expect("valid");
        let fetched = fetch_ok(&self.http, &page_url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        let html = fetched.text();
        let page = Page::parse(&html, &page_url);
        let blob = page
            .document()
            .select(
                &scraper::Selector::parse("script#__UNIVERSAL_DATA_FOR_REHYDRATION__")
                    .expect("valid"),
            )
            .next()
            .map(|s| s.text().collect::<String>())
            .ok_or_else(|| ResolveError::malformed(origin, "the page has no rehydration data"))?;
        let data: Value = serde_json::from_str(&blob)
            .map_err(|e| ResolveError::malformed(origin, format!("rehydration JSON: {e}")))?;
        let detail = data
            .pointer("/__DEFAULT_SCOPE__/webapp.video-detail")
            .cloned()
            .ok_or_else(|| ResolveError::malformed(origin, "the page data has no video detail"))?;
        match detail["statusCode"].as_i64().unwrap_or(0) {
            0 => {}
            10204 | 10216 | 10222 => return Err(ResolveError::NotFound(origin.clone())),
            10231 | 10229 => {
                return Err(ResolveError::login_required(
                    origin,
                    PLATFORM,
                    detail["statusMsg"]
                        .as_str()
                        .unwrap_or("the video is private"),
                ));
            }
            code => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!(
                        "status {code}: {}",
                        detail["statusMsg"].as_str().unwrap_or("no message")
                    ),
                ));
            }
        }
        detail
            .pointer("/itemInfo/itemStruct")
            .cloned()
            .ok_or_else(|| ResolveError::NotFound(origin.clone()))
    }
}

fn codec_of(name: &str) -> Option<VideoCodec> {
    let lower = name.to_ascii_lowercase();
    if lower.starts_with("h264") {
        Some(VideoCodec::H264)
    } else if lower.starts_with("h265") || lower.starts_with("hevc") {
        Some(VideoCodec::H265)
    } else if lower.contains("av1") {
        Some(VideoCodec::Av1)
    } else if lower.is_empty() {
        None
    } else {
        Some(VideoCodec::Other(lower))
    }
}

/// The variants a video's `bitrateInfo` and play address make.
pub fn variants_of(item: &Value, user_agent: &str) -> Vec<Variant> {
    let video = &item["video"];
    let duration = video["duration"]
        .as_f64()
        .filter(|d| *d > 0.0)
        .map(Duration::from_secs_f64);
    let headers = vec![
        ("referer".to_string(), REFERER.to_string()),
        ("user-agent".to_string(), user_agent.to_string()),
    ];
    let mut variants = Vec::new();
    for info in video["bitrateInfo"].as_array().into_iter().flatten() {
        let Some(url) = info["PlayAddr"]["UrlList"]
            .as_array()
            .and_then(|list| list.iter().rev().find_map(|u| u.as_str()))
            .and_then(|u| Url::parse(u).ok())
        else {
            continue;
        };
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(Container::Mp4);
        v.video = codec_of(info["CodecType"].as_str().unwrap_or(""));
        v.audio = Some(AudioCodec::Aac);
        v.bitrate = info["Bitrate"].as_u64();
        v.size = info["PlayAddr"]["DataSize"].as_u64().or_else(|| {
            info["PlayAddr"]["DataSize"]
                .as_str()
                .and_then(|s| s.parse().ok())
        });
        v.width = info["PlayAddr"]["Width"].as_u64().map(|w| w as u32);
        v.height = info["PlayAddr"]["Height"].as_u64().map(|h| h as u32);
        v.duration = duration;
        v.format_id = info["GearName"].as_str().map(String::from);
        v.label = info["QualityType"].as_u64().map(|q| format!("quality {q}"));
        v.headers = headers.clone();
        variants.push(v);
    }
    if variants.is_empty()
        && let Some(url) = video["playAddr"].as_str().and_then(|u| Url::parse(u).ok())
    {
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(Container::Mp4);
        v.video = codec_of(video["codecType"].as_str().unwrap_or(""));
        v.audio = Some(AudioCodec::Aac);
        v.bitrate = video["bitrate"].as_u64();
        v.width = video["width"].as_u64().map(|w| w as u32);
        v.height = video["height"].as_u64().map(|h| h as u32);
        v.duration = duration;
        v.headers = headers;
        variants.push(v);
    }
    variants
}

#[async_trait]
impl Resolver for TiktokResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "TikTok",
            hosts: &["tiktok.com", "vm.tiktok.com", "vt.tiktok.com"],
            features: &["videos", "short links", "embeds", "subtitles"],
            formats: &["mp4"],
            session: SessionSupport::Optional,
            examples: &["https://www.tiktok.com/@tiktok/video/7106594312292453675"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some_and(|h| {
                is_tiktok_host(h)
                    && (is_short_link(url)
                        || video_id(url).is_some()
                        || RE_PHOTO.is_match(url.path()))
            })
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let target = if is_short_link(url) {
            self.http
                .unwrap_redirects(url, Some(PLATFORM), BROWSER_UA)
                .await?
        } else {
            url.clone()
        };
        if RE_PHOTO.is_match(target.path()) {
            return Err(ResolveError::unavailable(url, "a photo post has no video"));
        }
        let id = video_id(&target).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let item = self.item(&id, url).await?;
        if item["imagePost"].is_object() {
            return Err(ResolveError::unavailable(url, "a photo post has no video"));
        }
        let variants = variants_of(&item, BROWSER_UA);
        if variants.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let author = &item["author"];
        let handle = author["uniqueId"].as_str().unwrap_or("");
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.clone());
        resolved.title = item["desc"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| clean_title(&format!("TikTok by @{handle}")));
        resolved.description = item["desc"].as_str().map(String::from);
        resolved.uploader = author["nickname"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| (!handle.is_empty()).then(|| format!("@{handle}")));
        resolved.uploader_url = (!handle.is_empty())
            .then(|| Url::parse(&format!("https://www.tiktok.com/@{handle}")).ok())
            .flatten();
        resolved.uploaded_at = item["createTime"]
            .as_str()
            .and_then(|s| s.parse::<i64>().ok())
            .or_else(|| item["createTime"].as_i64())
            .and_then(|t| jiff::Timestamp::from_second(t).ok());
        resolved.duration = variants[0].duration;
        resolved.thumbnail = item["video"]["cover"]
            .as_str()
            .and_then(|c| Url::parse(c).ok());
        resolved.webpage_url = (!handle.is_empty())
            .then(|| Url::parse(&format!("https://www.tiktok.com/@{handle}/video/{id}")).ok())
            .flatten();
        resolved.subtitles = item["video"]["subtitleInfos"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|track| {
                let url = Url::parse(track["Url"].as_str()?).ok()?;
                let format = match track["Format"]
                    .as_str()
                    .unwrap_or("")
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "webvtt" | "vtt" => SubtitleFormat::Vtt,
                    "srt" => SubtitleFormat::Srt,
                    _ => return None,
                };
                Some(SubtitleTrack {
                    url,
                    language: track["LanguageCodeName"]
                        .as_str()
                        .unwrap_or("und")
                        .to_string(),
                    name: None,
                    format,
                    auto: track["Source"].as_str() == Some("ASR"),
                    headers: vec![("referer".to_string(), REFERER.to_string())],
                })
            })
            .collect();
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        if self.http.jar(PLATFORM).get("sessionid").is_none() {
            return Ok(SessionCheck::LoggedOut);
        }
        let info = Url::parse(ACCOUNT_INFO).expect("valid");
        let response = self
            .http
            .get(info.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("referer", REFERER)
            .send()
            .await?;
        if !response.is_success() {
            return Ok(SessionCheck::LoggedOut);
        }
        let value: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(&info, e.to_string()))?;
        Ok(match value["data"]["username"].as_str() {
            Some(name) if !name.is_empty() => SessionCheck::LoggedIn {
                account: format!("@{name}"),
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

    fn page(status: i64, item: Value) -> String {
        let data = json!({"__DEFAULT_SCOPE__": {"webapp.video-detail": {"statusCode": status, "statusMsg": "", "itemInfo": {"itemStruct": item}}}});
        format!(
            r#"<html><head><title>TikTok</title></head><body><script id="__UNIVERSAL_DATA_FOR_REHYDRATION__" type="application/json">{data}</script></body></html>"#
        )
    }

    fn get(
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
            "id": "7106594312292453675", "desc": "A clip #fyp", "createTime": "1654000000",
            "author": {"uniqueId": "tiktok", "nickname": "TikTok"},
            "video": {
                "duration": 12, "cover": "https://p16.tiktokcdn.com/cover.jpg", "playAddr": "https://v16.tiktokcdn.com/play.mp4",
                "bitrateInfo": [
                    {"Bitrate": 1200000, "CodecType": "h264", "GearName": "normal_720", "QualityType": 10, "PlayAddr": {"UrlList": ["https://v16-webapp-prime.tiktok.com/a.mp4"], "DataSize": "1800000", "Width": 720, "Height": 1280}},
                    {"Bitrate": 2000000, "CodecType": "h265_hvc1", "GearName": "adapt_1080", "QualityType": 20, "PlayAddr": {"UrlList": ["https://v16-webapp-prime.tiktok.com/b.mp4"], "DataSize": 3000000, "Width": 1080, "Height": 1920}}
                ],
                "subtitleInfos": [{"LanguageCodeName": "eng-US", "Url": "https://v16.tiktokcdn.com/sub.vtt", "Format": "webvtt", "Source": "ASR"}]
            }
        })
    }

    #[tokio::test]
    async fn videos_come_from_the_page_data_with_short_links_unwrapped() {
        let mut fixture = Fixture::new("tiktok", None);
        fixture.exchanges.push(get(
            "https://vm.tiktok.com/ZMabc/",
            301,
            "text/html",
            "",
            &[(
                "location",
                "https://www.tiktok.com/@tiktok/video/7106594312292453675?_r=1",
            )],
        ));
        fixture.exchanges.push(get(
            "https://www.tiktok.com/@tiktok/video/7106594312292453675?_r=1",
            200,
            "text/html",
            "",
            &[],
        ));
        fixture.exchanges.push(get(
            "https://www.tiktok.com/@_/video/7106594312292453675",
            200,
            "text/html",
            &page(0, item()),
            &[(
                "set-cookie",
                "tt_chain_token=abc; Path=/; Domain=.tiktok.com",
            )],
        ));
        let http = Http::replay(fixture);
        let resolver = TiktokResolver::new(http.clone());
        let short = Url::parse("https://vm.tiktok.com/ZMabc/").unwrap();
        assert!(resolver.matches(&short));
        let resolved = resolver.resolve(&short).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("7106594312292453675"));
        assert_eq!(resolved.title.as_deref(), Some("A clip #fyp"));
        assert_eq!(resolved.uploader.as_deref(), Some("TikTok"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(12)));
        assert_eq!(resolved.variants.len(), 2);
        assert_eq!(resolved.variants[1].video, Some(VideoCodec::H265));
        assert_eq!(resolved.variants[1].height, Some(1920));
        assert_eq!(resolved.variants[0].size, Some(1800000));
        assert_eq!(
            resolved.variants[0].headers[0],
            ("referer".into(), REFERER.into())
        );
        assert_eq!(resolved.subtitles.len(), 1);
        assert!(resolved.subtitles[0].auto);
        assert_eq!(
            http.jar(PLATFORM).get("tt_chain_token").unwrap().value,
            "abc"
        );
    }

    #[tokio::test]
    async fn gone_and_private_videos_say_so() {
        let mut fixture = Fixture::new("tiktok", None);
        fixture.exchanges.push(get(
            "https://www.tiktok.com/@_/video/7106594312292453675",
            200,
            "text/html",
            &page(10204, json!({})),
            &[],
        ));
        let resolver = TiktokResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.tiktok.com/@tiktok/video/7106594312292453675").unwrap();
        assert!(matches!(
            resolver.resolve(&url).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let mut fixture = Fixture::new("tiktok", None);
        fixture.exchanges.push(get(
            "https://www.tiktok.com/@_/video/7106594312292453675",
            200,
            "text/html",
            &page(10231, json!({})),
            &[],
        ));
        let resolver = TiktokResolver::new(Http::replay(fixture));
        assert!(matches!(
            resolver.resolve(&url).await.unwrap_err(),
            ResolveError::LoginRequired { .. }
        ));
        assert!(!resolver.matches(&Url::parse("https://www.tiktok.com/@tiktok").unwrap()));
        assert!(resolver.matches(&Url::parse("https://www.tiktok.com/t/ZTabc/").unwrap()));
    }
}
