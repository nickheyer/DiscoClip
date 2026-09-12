//! Redgifs clips, through the API the site reads with a temporary token.

use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Variant,
    VariantKind, clean_title,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "redgifs";
const API: &str = "https://api.redgifs.com/v2/";
const SITE: &str = "https://www.redgifs.com/";

static RE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z]{3,}$").unwrap());

/// The clip id a link names: `/watch/{id}`, `/ifr/{id}`, or a media file named after it.
pub fn gif_id(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "redgifs.com" && !host.ends_with(".redgifs.com") {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let candidate = match segments.as_slice() {
        ["watch" | "ifr", id] => id.to_string(),
        ["i", file] | [file] => {
            let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file);
            ["-mobile", "-silent", "-poster", "-large"]
                .iter()
                .fold(stem.to_string(), |s, suffix| {
                    s.strip_suffix(suffix).map(String::from).unwrap_or(s)
                })
        }
        _ => return None,
    };
    RE_ID
        .is_match(&candidate)
        .then(|| candidate.to_ascii_lowercase())
}

pub struct RedgifsResolver {
    http: Http,
    token: Mutex<Option<String>>,
}

impl RedgifsResolver {
    pub fn new(http: Http) -> Self {
        Self {
            http,
            token: Mutex::new(None),
        }
    }

    fn cached_token(&self) -> Option<String> {
        self.token.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// A temporary token, bound to this client's address and user agent.
    async fn fetch_token(&self, origin: &Url) -> Result<String, ResolveError> {
        let url = Url::parse(&format!("{API}auth/temporary")).expect("valid");
        let response = self
            .http
            .get(url.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("referer", SITE)
            .send()
            .await?;
        let status = response.status;
        if !status.is_success() {
            return Err(match status.as_u16() {
                429 => ResolveError::RateLimited(origin.clone()),
                _ => ResolveError::unavailable(
                    origin,
                    format!("Redgifs gave no temporary token (HTTP {status})"),
                ),
            });
        }
        let value: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, format!("token JSON: {e}")))?;
        let token = value["token"]
            .as_str()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| ResolveError::malformed(origin, "the token answer has no token"))?
            .to_string();
        *self.token.lock().unwrap_or_else(|e| e.into_inner()) = Some(token.clone());
        Ok(token)
    }

    /// GETs `path` with the token, fetching a fresh one when the old one is refused.
    async fn api(&self, path: &str, id: &str, origin: &Url) -> Result<Value, ResolveError> {
        let url = Url::parse(&format!("{API}{path}")).expect("valid");
        let mut refreshed = false;
        loop {
            let token = match self.cached_token() {
                Some(token) => token,
                None => {
                    refreshed = true;
                    self.fetch_token(origin).await?
                }
            };
            let response = self
                .http
                .get(url.clone())
                .platform(PLATFORM)
                .user_agent(BROWSER_UA)
                .header("authorization", &format!("Bearer {token}"))
                .header("referer", SITE)
                .header("origin", "https://www.redgifs.com")
                .header("x-customheader", &format!("{SITE}watch/{id}"))
                .send()
                .await?;
            let status = response.status;
            match status.as_u16() {
                200..=299 => {
                    return response
                        .json(MAX_PAGE)
                        .await
                        .map_err(|e| ResolveError::malformed(origin, format!("API JSON: {e}")));
                }
                401 if !refreshed => {
                    *self.token.lock().unwrap_or_else(|e| e.into_inner()) = None;
                    refreshed = true;
                    continue;
                }
                404 | 410 => return Err(ResolveError::NotFound(origin.clone())),
                429 => return Err(ResolveError::RateLimited(origin.clone())),
                _ => {
                    let detail = response
                        .json::<Value>(MAX_PAGE)
                        .await
                        .ok()
                        .and_then(|v| v["error"]["description"].as_str().map(String::from))
                        .unwrap_or_else(|| format!("the API answered HTTP {status}"));
                    return Err(ResolveError::unavailable(origin, detail));
                }
            }
        }
    }
}

/// The MP4 renditions a clip's `urls` name: `hd` at full size, `sd` at most 480 tall.
pub fn variants_of(gif: &Value) -> Vec<Variant> {
    let width = gif["width"].as_f64().filter(|w| *w > 0.0);
    let height = gif["height"].as_f64().filter(|h| *h > 0.0);
    let aspect = match (width, height) {
        (Some(w), Some(h)) => Some(w / h),
        _ => None,
    };
    let duration = gif["duration"]
        .as_f64()
        .filter(|d| *d > 0.0)
        .map(Duration::from_secs_f64);
    let has_audio = gif["hasAudio"].as_bool() == Some(true);
    let mut variants = Vec::new();
    for (key, cap) in [("hd", None), ("sd", Some(480.0))] {
        let Some(url) = gif["urls"][key].as_str().and_then(|u| Url::parse(u).ok()) else {
            continue;
        };
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(Container::Mp4);
        v.video = Some(VideoCodec::H264);
        v.audio = has_audio.then_some(AudioCodec::Aac);
        v.height = height.map(|h| cap.map_or(h, |c: f64| h.min(c)) as u32);
        v.width = match (v.height, aspect) {
            (Some(h), Some(aspect)) => Some((f64::from(h) * aspect).round() as u32),
            _ => width.map(|w| w as u32),
        };
        v.duration = duration;
        v.format_id = Some(key.to_string());
        v.label = v.height.map(|h| format!("{h}p"));
        variants.push(v);
    }
    variants
}

#[async_trait]
impl Resolver for RedgifsResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Redgifs",
            hosts: &["redgifs.com"],
            features: &["clips", "embeds", "direct links"],
            formats: &["mp4"],
            session: SessionSupport::None,
            examples: &["https://www.redgifs.com/watch/messytrustingnutria"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        gif_id(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let id = gif_id(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let answer = self
            .api(&format!("gifs/{id}?views=yes&users=yes"), &id, url)
            .await?;
        let gif = &answer["gif"];
        if !gif.is_object() {
            return Err(ResolveError::malformed(url, "the API answer has no gif"));
        }
        if gif["type"].as_u64() == Some(2) {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let variants = variants_of(gif);
        if variants.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let user = &answer["user"];
        let user_name = gif["userName"]
            .as_str()
            .or_else(|| user["username"].as_str())
            .filter(|u| !u.is_empty());
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(gif["id"].as_str().unwrap_or(&id).to_string());
        resolved.title = gif["description"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| {
                let tags: Vec<&str> = gif["tags"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|t| t.as_str())
                    .collect();
                clean_title(&tags.join(" "))
            })
            .or_else(|| user_name.map(|u| format!("Redgifs clip by {u}")));
        resolved.uploader = user["name"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| user_name.map(String::from));
        resolved.uploader_url = user["url"]
            .as_str()
            .and_then(|u| Url::parse(u).ok())
            .or_else(|| user_name.and_then(|u| Url::parse(&format!("{SITE}users/{u}")).ok()));
        resolved.uploaded_at = gif["createDate"]
            .as_i64()
            .and_then(|t| Timestamp::from_second(t).ok());
        resolved.duration = variants[0].duration;
        resolved.thumbnail = gif["urls"]["poster"]
            .as_str()
            .or_else(|| gif["urls"]["thumbnail"].as_str())
            .and_then(|t| Url::parse(t).ok());
        resolved.webpage_url = Url::parse(&format!("{SITE}watch/{id}")).ok();
        resolved.age_limit = Some(18);
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

    fn gif() -> Value {
        json!({
            "gif": {
                "id": "pricklyflippanttaruca", "createDate": 1752167543, "duration": 59.993, "width": 1080, "height": 1448,
                "type": 1, "hasAudio": true, "userName": "feet_toes", "tags": ["Tag One", "Tag Two"], "description": "",
                "urls": {"hd": "https://media.redgifs.com/PricklyFlippantTaruca.mp4", "sd": "https://media.redgifs.com/PricklyFlippantTaruca-mobile.mp4",
                         "poster": "https://media.redgifs.com/PricklyFlippantTaruca-poster.jpg", "silent": "https://media.redgifs.com/PricklyFlippantTaruca-silent.mp4"}
            },
            "user": {"username": "feet_toes", "name": "Feet", "url": "https://www.redgifs.com/users/feet_toes"}
        })
    }

    #[test]
    fn links_are_read() {
        let id = |s: &str| gif_id(&Url::parse(s).unwrap());
        assert_eq!(
            id("https://www.redgifs.com/watch/PricklyFlippantTaruca#rel=user"),
            Some("pricklyflippanttaruca".into())
        );
        assert_eq!(
            id("https://redgifs.com/ifr/pricklyflippanttaruca"),
            Some("pricklyflippanttaruca".into())
        );
        assert_eq!(
            id("https://thumbs2.redgifs.com/PricklyFlippantTaruca-mobile.mp4"),
            Some("pricklyflippanttaruca".into())
        );
        assert_eq!(
            id("https://media.redgifs.com/PricklyFlippantTaruca.mp4"),
            Some("pricklyflippanttaruca".into())
        );
        assert_eq!(
            id("https://i.redgifs.com/i/pricklyflippanttaruca.jpg"),
            Some("pricklyflippanttaruca".into())
        );
        assert_eq!(id("https://www.redgifs.com/users/feet_toes"), None);
        assert_eq!(id("https://www.redgifs.com/"), None);
    }

    #[tokio::test]
    async fn clips_resolve_with_a_token_that_is_renewed_when_refused() {
        let mut fixture = Fixture::new("redgifs", None);
        fixture.exchanges.push(get(
            "https://api.redgifs.com/v2/gifs/pricklyflippanttaruca?views=yes&users=yes",
            401,
            json!({"error": {"code": "Unauthorized"}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.redgifs.com/v2/auth/temporary",
            200,
            json!({"token": "second"}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.redgifs.com/v2/gifs/pricklyflippanttaruca?views=yes&users=yes",
            200,
            gif().to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.redgifs.com/v2/gifs/notreal?views=yes&users=yes",
            404,
            json!({"error": {"code": "GifNotFound"}}).to_string(),
        ));
        let resolver = RedgifsResolver::new(Http::replay(fixture));
        *resolver.token.lock().unwrap() = Some("stale".into());
        let url = Url::parse("https://www.redgifs.com/watch/pricklyflippanttaruca").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolver.cached_token().as_deref(), Some("second"));
        assert_eq!(resolved.title.as_deref(), Some("Tag One Tag Two"));
        assert_eq!(resolved.uploader.as_deref(), Some("Feet"));
        assert_eq!(resolved.age_limit, Some(18));
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(59.993)));
        assert_eq!(resolved.variants.len(), 2);
        assert_eq!(resolved.variants[0].height, Some(1448));
        assert_eq!(resolved.variants[0].width, Some(1080));
        assert_eq!(resolved.variants[0].audio, Some(AudioCodec::Aac));
        assert_eq!(resolved.variants[1].height, Some(480));
        assert_eq!(resolved.variants[1].width, Some(358));
        assert_eq!(
            resolved.variants[1].url.as_str(),
            "https://media.redgifs.com/PricklyFlippantTaruca-mobile.mp4"
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.redgifs.com/watch/notreal").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
