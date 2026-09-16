//! Coub loops, through the API the site's player reads: the shareable MP4 with sound
//! when the site has rendered one, else the silent loop paired with its looped audio
//! track, with the coub's title, author, time and picture.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Variant,
    VariantKind, clean_title, fetch,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "coub";
const SITE: &str = "https://coub.com/";
const API: &str = "https://coub.com/api/v2/coubs/";

static RE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9]{3,12}$").unwrap());
/// Top level pages of the site that are not coubs.
const RESERVED: [&str; 14] = [
    "api", "explore", "hot", "rising", "fresh", "feed", "community", "tags", "search", "embed",
    "view", "login", "signup", "about",
];

pub fn coub_id(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "coub.com" && host != "www.coub.com" {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let id = match segments.as_slice() {
        ["view" | "embed" | "coubs", id, ..] => id,
        [id] if !RESERVED.contains(id) => id,
        _ => return None,
    };
    RE_ID.is_match(id).then(|| id.to_string())
}

/// `c-cdn.coub.com/fb-player.swf?coubID={id}`: the Flash player other pages embedded.
pub fn player_coub_id(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") || url.host_str()? != "c-cdn.coub.com" {
        return None;
    }
    if !url.path().ends_with("fb-player.swf") {
        return None;
    }
    let id = url
        .query_pairs()
        .find(|(k, _)| k == "coubID")
        .map(|(_, v)| v.into_owned())?;
    RE_ID.is_match(&id).then_some(id)
}

fn dimensions(coub: &Value) -> (Option<u32>, Option<u32>) {
    let pair = coub["dimensions"]["big"]
        .as_array()
        .or_else(|| coub["size"].as_array())
        .or_else(|| coub["dimensions"]["med"].as_array());
    match pair.map(|p| p.as_slice()) {
        Some([w, h, ..]) => (
            w.as_u64().map(|w| w as u32),
            h.as_u64().map(|h| h as u32),
        ),
        _ => (None, None),
    }
}

/// The variants the API describes: the shareable file with sound first, then the silent
/// loop in each size paired with the looped audio.
/// Every file the coub is served as: the share file (the loop with its sound), each
/// HTML5 video quality (video alone, paired with the best HTML5 audio), each HTML5 audio
/// quality on its own, the iPhone file, and the mobile video and audio.
pub fn variants_of(coub: &Value) -> Vec<Variant> {
    let duration = coub["duration"]
        .as_f64()
        .filter(|d| *d > 0.0)
        .map(Duration::from_secs_f64);
    let (width, height) = dimensions(coub);
    let versions = &coub["file_versions"];
    let mut variants = Vec::new();
    if let Some(url) = versions["share"]["default"]
        .as_str()
        .and_then(|u| Url::parse(u).ok())
    {
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(Container::Mp4);
        v.video = Some(VideoCodec::H264);
        v.audio = Some(AudioCodec::Aac);
        v.width = width;
        v.height = height;
        v.duration = duration;
        v.format_id = Some("share".into());
        v.label = Some("looped with sound".into());
        variants.push(v);
    }
    let audio = ["higher", "high", "med", "low"]
        .iter()
        .find_map(|q| versions["html5"]["audio"][q]["url"].as_str())
        .or_else(|| coub["audio_file_url"].as_str())
        .and_then(|u| Url::parse(u).ok());
    for quality in ["higher", "high", "med", "low"] {
        let entry = &versions["html5"]["video"][quality];
        let Some(url) = entry["url"].as_str().and_then(|u| Url::parse(u).ok()) else {
            continue;
        };
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(Container::Mp4);
        v.video = Some(VideoCodec::H264);
        v.size = entry["size"].as_u64().filter(|s| *s > 0);
        v.duration = duration;
        v.format_id = Some(format!("html5-video-{quality}"));
        v.label = Some(quality.to_string());
        match (quality, width, height) {
            ("med", Some(w), Some(h)) => {
                v.width = Some(w / 2);
                v.height = Some(h / 2);
            }
            ("low", Some(w), Some(h)) => {
                v.width = Some(w / 4);
                v.height = Some(h / 4);
            }
            (_, w, h) => {
                v.width = w;
                v.height = h;
            }
        }
        v.video_only = true;
        if let Some(audio) = &audio {
            v.audio_url = Some(audio.clone());
            v.audio = Some(AudioCodec::Mp3);
        }
        variants.push(v);
    }
    for quality in ["higher", "high", "med", "low"] {
        let entry = &versions["html5"]["audio"][quality];
        let Some(url) = entry["url"].as_str().and_then(|u| Url::parse(u).ok()) else {
            continue;
        };
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(Container::Other("mp3".into()));
        v.audio = Some(AudioCodec::Mp3);
        v.audio_only = true;
        v.size = entry["size"].as_u64().filter(|s| *s > 0);
        v.duration = duration;
        v.format_id = Some(format!("html5-audio-{quality}"));
        v.label = Some(format!("audio {quality}"));
        variants.push(v);
    }
    if let Some(url) = versions["iphone"]["url"]
        .as_str()
        .and_then(|u| Url::parse(u).ok())
    {
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(Container::Mp4);
        v.video = Some(VideoCodec::H264);
        v.audio = Some(AudioCodec::Aac);
        v.duration = duration;
        v.format_id = Some("iphone".into());
        v.label = Some("iphone".into());
        variants.push(v);
    }
    if let Some(url) = versions["mobile"]["video"]
        .as_str()
        .and_then(|u| Url::parse(u).ok())
    {
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(Container::Mp4);
        v.video = Some(VideoCodec::H264);
        v.duration = duration;
        v.format_id = Some("mobile".into());
        v.label = Some("mobile".into());
        v.video_only = true;
        if let Some(audio) = versions["mobile"]["audio_url"]
            .as_str()
            .and_then(|u| Url::parse(u).ok())
            .or_else(|| audio.clone())
        {
            v.audio_url = Some(audio);
            v.audio = Some(AudioCodec::Mp3);
        }
        variants.push(v);
    }
    if let Some(url) = versions["mobile"]["audio_url"]
        .as_str()
        .and_then(|u| Url::parse(u).ok())
    {
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(Container::Other("mp3".into()));
        v.audio = Some(AudioCodec::Mp3);
        v.audio_only = true;
        v.duration = duration;
        v.format_id = Some("mobile-audio".into());
        v.label = Some("mobile audio".into());
        variants.push(v);
    }
    variants
}

pub struct CoubResolver {
    http: Http,
}

impl CoubResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for CoubResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Coub",
            hosts: &["coub.com"],
            features: &["coubs", "embeds", "short links", "recoubs"],
            formats: &["mp4"],
            session: SessionSupport::None,
            examples: &["https://coub.com/view/5u5n1"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        coub_id(url).is_some() || player_coub_id(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let id = coub_id(url)
            .or_else(|| player_coub_id(url))
            .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let api = Url::parse(&format!("{API}{id}")).expect("valid");
        let fetched = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        match fetched.status.as_u16() {
            200..=299 => {}
            404 | 410 => return Err(ResolveError::NotFound(url.clone())),
            429 => return Err(ResolveError::RateLimited(url.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    url,
                    format!("the API answered HTTP {status}"),
                ));
            }
        }
        let coub = fetched.json(url)?;
        if let Some(error) = coub["error"].as_str().filter(|e| !e.trim().is_empty()) {
            return Err(ResolveError::unavailable(url, format!("Coub said: {error}")));
        }
        if coub["banned"].as_bool() == Some(true) {
            return Err(ResolveError::unavailable(url, "the coub was banned"));
        }
        let variants = variants_of(&coub);
        if variants.is_empty() {
            return Err(if coub["is_done"].as_bool() == Some(false) {
                ResolveError::unavailable(url, "the coub is still being processed")
            } else {
                ResolveError::NotFound(url.clone())
            });
        }
        let permalink = coub["permalink"].as_str().unwrap_or(&id).to_string();
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(permalink.clone());
        resolved.title = coub["title"].as_str().and_then(clean_title);
        resolved.uploader = coub["channel"]["title"].as_str().and_then(clean_title);
        resolved.uploader_url = coub["channel"]["permalink"]
            .as_str()
            .and_then(|p| Url::parse(&format!("{SITE}{p}")).ok());
        resolved.uploaded_at = coub["published_at"]
            .as_str()
            .or(coub["created_at"].as_str())
            .and_then(|t| t.parse::<Timestamp>().ok());
        resolved.duration = coub["duration"]
            .as_f64()
            .filter(|d| *d > 0.0)
            .map(Duration::from_secs_f64);
        resolved.thumbnail = coub["picture"].as_str().and_then(|u| Url::parse(u).ok());
        resolved.webpage_url = Url::parse(&format!("{SITE}view/{permalink}")).ok();
        resolved.age_limit = (coub["age_restricted"].as_bool() == Some(true)
            || coub["age_restricted_by_admin"].as_bool() == Some(true)
            || coub["not_safe_for_work"].as_bool() == Some(true))
        .then_some(18);
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
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

    fn coub() -> Value {
        json!({
            "id": 10558488, "permalink": "5u5n1", "title": "The Matrix Moonwalk", "duration": 4.6,
            "created_at": "2015-04-08T21:16:12Z", "published_at": "2015-04-08T21:16:12Z", "is_done": true,
            "dimensions": {"big": [1280, 536], "med": [640, 268]}, "size": [1280, 536], "age_restricted": false,
            "file_versions": {
                "html5": {
                    "video": {"high": {"url": "https://cdn.coub.test/muted_big.mp4", "size": 1838688}, "med": {"url": "https://cdn.coub.test/muted_med.mp4", "size": 678114}},
                    "audio": {"high": {"url": "https://cdn.coub.test/audio_high.mp3", "size": 7059956}, "med": {"url": "https://cdn.coub.test/audio_med.mp3", "size": 4706637}}
                },
                "mobile": {"video": "https://cdn.coub.test/muted_med.mp4", "audio": ["https://cdn.coub.test/audio_med.mp3"]},
                "share": {"default": "https://cdn.coub.test/looped.mp4"}
            },
            "channel": {"id": 1707478, "permalink": "artyom.loskutnikov", "title": "Artem Loskutnikov"},
            "picture": "https://cdn.coub.test/picture.jpg", "has_sound": true
        })
    }

    #[test]
    fn links_are_read() {
        let id = |s: &str| coub_id(&Url::parse(s).unwrap());
        assert_eq!(id("https://coub.com/view/5u5n1"), Some("5u5n1".into()));
        assert_eq!(id("https://coub.com/embed/5u5n1?muted=false"), Some("5u5n1".into()));
        assert_eq!(id("https://coub.com/5u5n1"), Some("5u5n1".into()));
        assert_eq!(id("https://coub.com/explore"), None);
        assert_eq!(id("https://coub.com/artyom.loskutnikov"), None);
        assert_eq!(id("https://coub.com/"), None);
    }

    #[tokio::test]
    async fn coubs_resolve_to_the_shareable_file_with_sound() {
        let mut fixture = Fixture::new("coub", None);
        fixture.exchanges.push(get(
            "https://coub.com/api/v2/coubs/5u5n1",
            200,
            coub().to_string(),
        ));
        let resolver = CoubResolver::new(Http::replay(fixture));
        let url = Url::parse("https://coub.com/view/5u5n1").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.title.as_deref(), Some("The Matrix Moonwalk"));
        assert_eq!(resolved.uploader.as_deref(), Some("Artem Loskutnikov"));
        assert_eq!(
            resolved.uploader_url.unwrap().as_str(),
            "https://coub.com/artyom.loskutnikov"
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(4.6)));
        assert!(resolved.uploaded_at.is_some());
        assert!(resolved.variants.len() > 1, "the share file and every HTML5 rendition");
        let share = &resolved.variants[0];
        assert_eq!(share.url.as_str(), "https://cdn.coub.test/looped.mp4");
        assert_eq!(share.audio, Some(AudioCodec::Aac));
        assert_eq!((share.width, share.height), (Some(1280), Some(536)));
        assert!(!share.video_only);
    }

    #[tokio::test]
    async fn without_a_shareable_file_the_loop_is_paired_with_its_audio() {
        let mut data = coub();
        data["file_versions"]["share"] = json!({});
        let mut fixture = Fixture::new("coub", None);
        fixture.exchanges.push(get(
            "https://coub.com/api/v2/coubs/5u5n1",
            200,
            data.to_string(),
        ));
        fixture.exchanges.push(get(
            "https://coub.com/api/v2/coubs/gone1",
            404,
            r#"{"error":"Not Found"}"#.into(),
        ));
        let resolver = CoubResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://coub.com/view/5u5n1").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert!(resolved.variants.len() >= 2, "{}", resolved.variants.len());
        assert!(resolved.variants.iter().all(|v| v.format_id.as_deref() != Some("share")));
        assert!(resolved.variants.iter().any(|v| v.audio_only), "the HTML5 audio on its own");
        let high = &resolved.variants[0];
        assert!(high.video_only);
        assert_eq!(
            high.audio_url.as_ref().unwrap().as_str(),
            "https://cdn.coub.test/audio_high.mp3"
        );
        assert_eq!(high.size, Some(1838688));
        assert_eq!(high.height, Some(536));
        assert_eq!(resolved.variants[1].height, Some(268));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://coub.com/view/gone1").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
