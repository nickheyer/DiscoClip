//! AcFun (acfun.cn) videos and bangumi episodes: each page carries the player's setup,
//! whose `ksPlayJson` lists one HLS playlist per rendition, played with the site as
//! referer.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Variant,
    clean_title, fetch, navigation_headers, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "acfun";
const SITE: &str = "https://www.acfun.cn/";

/// `/v/ac{id}` with an optional `_{part}`.
static RE_VIDEO: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/v/ac(\d+(?:_\d+)?)").unwrap());
/// `/bangumi/aa{id}` with optional `_{season}_{episode}` parts.
static RE_BANGUMI: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/bangumi/(aa\d+(?:_\d+)*)").unwrap());
static RE_VIDEO_INFO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"window\.videoInfo\s*=\s*").unwrap());
static RE_BANGUMI_DATA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"window\.bangumiData\s*=\s*").unwrap());
static RE_BANGUMI_LIST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"window\.bangumiList\s*=\s*").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Video {
        id: String,
    },
    /// A bangumi episode; `ac` picks a highlight clip of the page instead.
    Bangumi {
        id: String,
        ac: Option<String>,
    },
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "www.acfun.cn" && host != "acfun.cn" && host != "m.acfun.cn" {
        return None;
    }
    let path = url.path();
    if let Some(caps) = RE_VIDEO.captures(path) {
        return Some(Link::Video {
            id: caps[1].to_string(),
        });
    }
    let caps = RE_BANGUMI.captures(path)?;
    Some(Link::Bangumi {
        id: caps[1].to_string(),
        ac: util::query_param(url, "ac").filter(|ac| !ac.is_empty()),
    })
}

/// The JSON object assigned right after `assignment` in a page.
fn assigned_json(html: &str, assignment: &Regex) -> Option<Value> {
    let start = assignment.find(html)?.end();
    let rest = &html[start..];
    let end = util::balanced_js_end(rest)?;
    let text = &rest[..end];
    serde_json::from_str(text)
        .ok()
        .or_else(|| util::parse_js(text))
}

/// The renditions a player setup's `ksPlayJson` lists, as HLS variants played with the
/// site as referer.
pub fn representations(video_info: &Value) -> Vec<Variant> {
    let play_json = match &video_info["ksPlayJson"] {
        Value::String(text) => serde_json::from_str::<Value>(text).unwrap_or(Value::Null),
        other => other.clone(),
    };
    let mut variants = Vec::new();
    for set in play_json["adaptationSet"].as_array().into_iter().flatten() {
        for rendition in set["representation"].as_array().into_iter().flatten() {
            let Some(url) = util::url_of(&rendition["url"], None) else {
                continue;
            };
            let mut variant = Variant::hls(url).with_header("referer", SITE);
            variant.container = Some(Container::Mp4);
            variant.width = util::u32_of(&rendition["width"]).filter(|w| *w > 0);
            variant.height = util::u32_of(&rendition["height"]).filter(|h| *h > 0);
            variant.fps = util::float(&rendition["frameRate"]).filter(|f| *f > 0.0);
            variant.bitrate = util::uint(&rendition["avgBitrate"])
                .filter(|b| *b > 0)
                .map(|kbps| kbps * 1000);
            let codecs = rendition["codecs"].as_str().unwrap_or("").trim();
            if !codecs.is_empty() {
                variant = variant.with_codecs(codecs);
            }
            if variant.video.is_none() {
                variant.video = Some(VideoCodec::H264);
            }
            if variant.audio.is_none() {
                variant.audio = Some(AudioCodec::Aac);
            }
            variant.label = rendition["qualityLabel"]
                .as_str()
                .or(rendition["qualityType"].as_str())
                .map(str::to_string)
                .or_else(|| variant.height.map(|h| format!("{h}p")));
            variant.format_id = util::text(&rendition["id"]).map(|id| format!("hls-{id}"));
            variants.push(variant);
        }
    }
    variants
}

/// What a player setup says about its video, beyond the renditions.
fn fill_from_video_info(resolved: &mut Resolved, video_info: &Value) {
    resolved.duration = util::millis(&video_info["durationMillis"]);
    // Upload times are in milliseconds.
    resolved.uploaded_at = util::int(&video_info["uploadTime"])
        .and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok());
}

pub struct AcfunResolver {
    http: Http,
}

impl AcfunResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    async fn page(&self, url: &Url) -> Result<String, ResolveError> {
        let fetched = fetch(
            &self.http,
            url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        Ok(fetched.text())
    }

    async fn resolve_video(&self, id: &str, url: &Url) -> Result<Resolution, ResolveError> {
        let html = self.page(url).await?;
        let info = assigned_json(&html, &RE_VIDEO_INFO)
            .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let current = &info["currentVideoInfo"];
        let variants = representations(current);
        if variants.is_empty() {
            return Err(ResolveError::unavailable(
                url,
                "the player lists no renditions",
            ));
        }
        let mut title = info["title"].as_str().and_then(clean_title);
        let parts = info["videoList"].as_array().cloned().unwrap_or_default();
        if parts.len() > 1
            && let Some(current_id) = util::text(&current["id"])
            && let Some((index, part)) = parts
                .iter()
                .enumerate()
                .find(|(_, part)| util::text(&part["id"]).as_deref() == Some(&current_id))
        {
            let part_title = part["title"]
                .as_str()
                .and_then(clean_title)
                .unwrap_or_default();
            title = title.map(|t| {
                format!("{t} P{:02} {part_title}", index + 1)
                    .trim()
                    .to_string()
            });
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.to_string());
        resolved.title = title;
        resolved.description = info["description"].as_str().and_then(clean_title);
        resolved.thumbnail = util::url_of(&info["coverUrl"], None);
        resolved.uploader = info["user"]["name"].as_str().and_then(clean_title);
        resolved.uploader_url = info["user"]["href"]
            .as_str()
            .and_then(|href| Url::parse(SITE).ok()?.join(href).ok());
        fill_from_video_info(&mut resolved, current);
        resolved.webpage_url = Some(url.clone());
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn resolve_bangumi(
        &self,
        id: &str,
        ac: Option<&str>,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let html = self.page(url).await?;
        let data = assigned_json(&html, &RE_BANGUMI_DATA)
            .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let mut resolved = Resolved::new(PLATFORM);
        let video_info = if ac.is_some() {
            let highlight = &data["hlVideoInfo"];
            resolved.title = highlight["title"].as_str().and_then(clean_title);
            highlight.clone()
        } else {
            let show = data["showTitle"].as_str().and_then(clean_title);
            let episode = data["title"].as_str().and_then(clean_title);
            resolved.title = match (show, episode) {
                (Some(show), Some(episode)) if !show.contains(&episode) => {
                    Some(format!("{show} {episode}"))
                }
                (Some(show), _) => Some(show),
                (None, episode) => episode,
            };
            data["currentVideoInfo"].clone()
        };
        let variants = representations(&video_info);
        if variants.is_empty() {
            return Err(ResolveError::unavailable(
                url,
                "the player lists no renditions",
            ));
        }
        resolved.id = Some(match ac {
            Some(ac) => format!("{id}__{ac}"),
            None => id.to_string(),
        });
        resolved.thumbnail = util::url_of(&data["image"], None);
        resolved.description = data["intro"]
            .as_str()
            .or(data["description"].as_str())
            .and_then(clean_title);
        resolved.uploader = data["bangumiTitle"].as_str().and_then(clean_title);
        fill_from_video_info(&mut resolved, &video_info);
        if resolved.duration.is_none()
            && let Some(list) = assigned_json(&html, &RE_BANGUMI_LIST)
            && let Some(current) = util::uint(&video_info["id"])
        {
            resolved.duration = list["items"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|item| util::uint(&item["videoId"]) == Some(current))
                .and_then(|item| util::millis(&item["durationMillis"]));
        }
        resolved.webpage_url = Some(url.clone());
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }
}

#[async_trait]
impl Resolver for AcfunResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "AcFun",
            hosts: &["acfun.cn"],
            features: &["videos", "bangumi"],
            formats: &["hls"],
            session: SessionSupport::None,
            examples: &["https://www.acfun.cn/v/ac35457073"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Video { id } => self.resolve_video(&id, url).await,
            Link::Bangumi { id, ac } => self.resolve_bangumi(&id, ac.as_deref(), url).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};
    use crate::resolve::VariantKind;
    use serde_json::json;

    fn get(url: &str, status: u16, content_type: &str, body: String) -> Exchange {
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
                body: RecordedBody::Text(body),
                truncated: false,
            },
        }
    }

    fn play_json() -> String {
        json!({"adaptationSet": [{"representation": [
            {"id": 1, "url": "https://tx-safety-video.acfun.cn/x/hls_1080p.m3u8?pkey=a", "backupUrl": ["https://ali.acfun.cn/x/hls_1080p.m3u8"], "width": 1080, "height": 1920, "frameRate": 60.0, "avgBitrate": 2400, "codecs": "avc1.640028,mp4a.40.2", "qualityLabel": "1080P60", "qualityType": "1080p60"},
            {"id": 2, "url": "https://tx-safety-video.acfun.cn/x/hls_360p.m3u8?pkey=b", "width": 360, "height": 640, "frameRate": 29.98, "avgBitrate": 592, "codecs": "", "qualityLabel": "360P", "qualityType": "360p"},
            {"id": 3, "width": 1, "height": 1}
        ]}]}).to_string()
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://www.acfun.cn/v/ac35457073"),
            Some(Link::Video {
                id: "35457073".into()
            })
        );
        assert_eq!(
            link("https://www.acfun.cn/v/ac35468952_2"),
            Some(Link::Video {
                id: "35468952_2".into()
            })
        );
        assert_eq!(
            link("https://www.acfun.cn/bangumi/aa6002917_36188_1745457?ac=2"),
            Some(Link::Bangumi {
                id: "aa6002917_36188_1745457".into(),
                ac: Some("2".into())
            })
        );
        assert_eq!(
            link("https://www.acfun.cn/bangumi/aa5023171_36188_1750645"),
            Some(Link::Bangumi {
                id: "aa5023171_36188_1750645".into(),
                ac: None
            })
        );
        assert_eq!(link("https://www.acfun.cn/u/12345"), None);
        assert_eq!(link("https://example.com/v/ac35457073"), None);
    }

    #[test]
    fn renditions_become_hls_variants() {
        let info = json!({"ksPlayJson": play_json(), "durationMillis": 61234, "uploadTime": 1660000000000u64});
        let variants = representations(&info);
        assert_eq!(variants.len(), 2, "renditions without a link are left out");
        let best = &variants[0];
        assert_eq!(best.kind, VariantKind::Hls);
        assert_eq!((best.width, best.height), (Some(1080), Some(1920)));
        assert_eq!(best.fps, Some(60.0));
        assert_eq!(best.bitrate, Some(2_400_000));
        assert_eq!(best.video, Some(VideoCodec::H264));
        assert_eq!(best.label.as_deref(), Some("1080P60"));
        assert_eq!(
            best.headers,
            vec![("referer".to_string(), SITE.to_string())]
        );
        assert_eq!(
            variants[1].video,
            Some(VideoCodec::H264),
            "codec-less renditions are H.264"
        );
    }

    #[tokio::test]
    async fn videos_resolve_from_the_page_setup() {
        let info = json!({
            "title": "18 岁 现 状", "description": "A look back.", "coverUrl": "https://imgs.acfun.cn/cover.jpg",
            "user": {"name": "锤子game", "href": "/u/12345"},
            "currentVideoInfo": {"id": 555, "ksPlayJson": play_json(), "durationMillis": 61234, "uploadTime": 1660000000000u64},
            "videoList": [{"id": 555, "title": "上"}, {"id": 556, "title": "下"}]
        });
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.acfun.cn/v/ac35457073",
            200,
            "text/html",
            format!("<html><script>window.videoInfo = {info};\n</script></html>"),
        ));
        fixture.exchanges.push(get(
            "https://www.acfun.cn/v/ac1",
            200,
            "text/html",
            "<html><body>nothing</body></html>".into(),
        ));
        let resolver = AcfunResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.acfun.cn/v/ac35457073").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("35457073"));
        assert_eq!(resolved.title.as_deref(), Some("18 岁 现 状 P01 上"));
        assert_eq!(resolved.uploader.as_deref(), Some("锤子game"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://www.acfun.cn/u/12345"
        );
        assert_eq!(resolved.duration, Some(Duration::from_millis(61234)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1660000000)
        );
        assert_eq!(resolved.variants.len(), 2);
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.acfun.cn/v/ac1").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn bangumi_pages_resolve_episodes_and_highlights() {
        let data = json!({
            "showTitle": "Fruits Basket 第1话", "bangumiTitle": "Fruits Basket", "title": "第1话", "image": "https://imgs.acfun.cn/bangumi.jpg",
            "currentVideoInfo": {"id": 777, "ksPlayJson": play_json()},
            "hlVideoInfo": {"id": 778, "title": "Highlight 2", "ksPlayJson": play_json(), "durationMillis": 5000}
        });
        let list = json!({"items": [{"videoId": 777, "durationMillis": 1440000}, {"videoId": 778, "durationMillis": 5000}]});
        let page = format!(
            "<html><script>window.bangumiData = {data};\nwindow.bangumiList = {list};\n</script></html>"
        );
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.acfun.cn/bangumi/aa6002917_36188_1745457",
            200,
            "text/html",
            page.clone(),
        ));
        fixture.exchanges.push(get(
            "https://www.acfun.cn/bangumi/aa6002917_36188_1745457?ac=2",
            200,
            "text/html",
            page,
        ));
        let resolver = AcfunResolver::new(Http::replay(fixture));
        let episode = resolver
            .resolve(&Url::parse("https://www.acfun.cn/bangumi/aa6002917_36188_1745457").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(episode.title.as_deref(), Some("Fruits Basket 第1话"));
        assert_eq!(episode.uploader.as_deref(), Some("Fruits Basket"));
        assert_eq!(episode.duration, Some(Duration::from_millis(1440000)));
        assert_eq!(episode.variants.len(), 2);
        let highlight = resolver
            .resolve(
                &Url::parse("https://www.acfun.cn/bangumi/aa6002917_36188_1745457?ac=2").unwrap(),
            )
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(highlight.id.as_deref(), Some("aa6002917_36188_1745457__2"));
        assert_eq!(highlight.title.as_deref(), Some("Highlight 2"));
        assert_eq!(highlight.duration, Some(Duration::from_millis(5000)));
    }
}
