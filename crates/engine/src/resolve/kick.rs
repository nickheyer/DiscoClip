//! Kick live channels, recordings and clips, through the API the web site reads.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionCheck, SessionSupport,
    clean_title, hls,
};
use crate::http::{BROWSER_UA, Http};

pub const PLATFORM: &str = "kick";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Channel(String),
    Video(String),
    Clip(String),
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "kick.com" && host != "www.kick.com" {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    if let Some(clip) = url
        .query_pairs()
        .find(|(k, _)| k == "clip")
        .map(|(_, v)| v.into_owned())
    {
        return Some(Link::Clip(clip));
    }
    let slug_like = |s: &str| {
        s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    };
    match segments.as_slice() {
        ["video", id] | [_, "videos", id] => Some(Link::Video(id.to_string())),
        [_, "clips", id] => Some(Link::Clip(id.to_string())),
        [slug] if slug_like(slug) => Some(Link::Channel(slug.to_ascii_lowercase())),
        _ => None,
    }
}

pub struct KickResolver {
    http: Http,
}

impl KickResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    fn bearer(&self) -> Option<String> {
        self.http.jar(PLATFORM).get("session_token").map(|c| {
            percent_encoding::percent_decode_str(&c.value)
                .decode_utf8_lossy()
                .into_owned()
        })
    }

    async fn api(&self, path: &str, origin: &Url) -> Result<Value, ResolveError> {
        let url = Url::parse(&format!("https://kick.com{path}")).expect("valid");
        let mut request = self
            .http
            .get(url.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "application/json")
            .header("referer", "https://kick.com/");
        if let Some(token) = self.bearer() {
            request = request.header("authorization", &format!("Bearer {token}"));
        }
        let response = request.send().await?;
        let status = response.status;
        match status.as_u16() {
            200..=299 => {}
            404 => return Err(ResolveError::NotFound(origin.clone())),
            403 => {
                return Err(ResolveError::unavailable(
                    origin,
                    "Kick's bot protection refused the request",
                ));
            }
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            _ => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the API answered HTTP {status}"),
                ));
            }
        }
        response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, format!("API JSON: {e}")))
    }

    async fn playlist(
        &self,
        manifest: &str,
        origin: &Url,
        live: bool,
    ) -> Result<hls::Expanded, ResolveError> {
        let master =
            Url::parse(manifest).map_err(|e| ResolveError::malformed(origin, e.to_string()))?;
        let mut expanded = hls::expand(&self.http, &master, PLATFORM, BROWSER_UA, &[]).await?;
        for v in &mut expanded.variants {
            v.live |= live;
        }
        expanded.live |= live;
        Ok(expanded)
    }
}

#[async_trait]
impl Resolver for KickResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Kick",
            hosts: &["kick.com"],
            features: &["live", "recordings", "clips"],
            formats: &["hls"],
            session: SessionSupport::Optional,
            examples: &["https://kick.com/xqc/clips/clip_01H811MXG4FBR62FXPE1AXABDH"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let mut resolved = Resolved::new(PLATFORM);
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Channel(slug) => {
                let channel = self.api(&format!("/api/v2/channels/{slug}"), url).await?;
                let stream = &channel["livestream"];
                if stream.is_null() || stream["is_live"].as_bool() != Some(true) {
                    return Err(ResolveError::unavailable(
                        url,
                        format!("{slug} is not live"),
                    ));
                }
                let manifest = channel["playback_url"].as_str().ok_or_else(|| {
                    ResolveError::malformed(url, "the channel has no playback URL")
                })?;
                let expanded = self.playlist(manifest, url, true).await?;
                resolved.id = stream["id"].as_u64().map(|i| i.to_string());
                resolved.title = stream["session_title"]
                    .as_str()
                    .and_then(clean_title)
                    .or_else(|| clean_title(&format!("{slug} live")));
                resolved.uploader = channel["user"]["username"].as_str().and_then(clean_title);
                resolved.uploader_url = Url::parse(&format!("https://kick.com/{slug}")).ok();
                resolved.uploaded_at = stream["created_at"].as_str().and_then(parse_time);
                resolved.thumbnail = stream["thumbnail"]["url"]
                    .as_str()
                    .and_then(|t| Url::parse(t).ok());
                resolved.webpage_url = Url::parse(&format!("https://kick.com/{slug}")).ok();
                resolved.live = true;
                resolved.age_limit = stream["is_mature"].as_bool().filter(|m| *m).map(|_| 18);
                resolved.variants = expanded.variants;
            }
            Link::Video(id) => {
                let video = self.api(&format!("/api/v1/video/{id}"), url).await?;
                let manifest = video["source"]
                    .as_str()
                    .ok_or_else(|| ResolveError::malformed(url, "the recording has no source"))?;
                let expanded = self.playlist(manifest, url, false).await?;
                let stream = &video["livestream"];
                let slug = stream["channel"]["slug"].as_str().unwrap_or("");
                resolved.id = Some(id.clone());
                resolved.title = stream["session_title"].as_str().and_then(clean_title);
                resolved.uploader = stream["channel"]["user"]["username"]
                    .as_str()
                    .and_then(clean_title);
                resolved.uploader_url = (!slug.is_empty())
                    .then(|| Url::parse(&format!("https://kick.com/{slug}")).ok())
                    .flatten();
                resolved.uploaded_at = video["created_at"].as_str().and_then(parse_time);
                resolved.duration = stream["duration"]
                    .as_u64()
                    .filter(|d| *d > 0)
                    .map(Duration::from_millis)
                    .or(expanded.duration);
                resolved.thumbnail = stream["thumbnail"]
                    .as_str()
                    .and_then(|t| Url::parse(t).ok());
                resolved.webpage_url = Url::parse(&format!("https://kick.com/video/{id}")).ok();
                resolved.live = expanded.live;
                resolved.age_limit = stream["is_mature"].as_bool().filter(|m| *m).map(|_| 18);
                resolved.variants = expanded.variants;
            }
            Link::Clip(id) => {
                let answer = self.api(&format!("/api/v2/clips/{id}"), url).await?;
                let clip = if answer["clip"].is_object() {
                    &answer["clip"]
                } else {
                    &answer
                };
                let manifest = clip["video_url"]
                    .as_str()
                    .or_else(|| clip["clip_url"].as_str())
                    .ok_or_else(|| ResolveError::malformed(url, "the clip has no video URL"))?;
                let expanded = self.playlist(manifest, url, false).await?;
                let slug = clip["channel"]["slug"].as_str().unwrap_or("");
                resolved.id = Some(id.clone());
                resolved.title = clip["title"].as_str().and_then(clean_title);
                resolved.uploader = clip["channel"]["username"].as_str().and_then(clean_title);
                resolved.uploader_url = (!slug.is_empty())
                    .then(|| Url::parse(&format!("https://kick.com/{slug}")).ok())
                    .flatten();
                resolved.uploaded_at = clip["created_at"].as_str().and_then(parse_time);
                resolved.duration = clip["duration"]
                    .as_f64()
                    .filter(|d| *d > 0.0)
                    .map(Duration::from_secs_f64)
                    .or(expanded.duration);
                resolved.thumbnail = clip["thumbnail_url"]
                    .as_str()
                    .and_then(|t| Url::parse(t).ok());
                resolved.webpage_url = (!slug.is_empty())
                    .then(|| Url::parse(&format!("https://kick.com/{slug}/clips/{id}")).ok())
                    .flatten();
                resolved.age_limit = clip["is_mature"].as_bool().filter(|m| *m).map(|_| 18);
                resolved.variants = expanded.variants;
            }
        }
        if resolved.variants.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        Ok(Resolution::from(resolved))
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        if self.bearer().is_none() {
            return Ok(SessionCheck::LoggedOut);
        }
        let origin = Url::parse("https://kick.com/").expect("valid");
        match self.api("/api/v1/user", &origin).await {
            Ok(user) => Ok(match user["username"].as_str() {
                Some(name) => SessionCheck::LoggedIn {
                    account: name.to_string(),
                },
                None => SessionCheck::LoggedOut,
            }),
            Err(ResolveError::Unavailable { .. }) | Err(ResolveError::NotFound(_)) => {
                Ok(SessionCheck::LoggedOut)
            }
            Err(error) => Err(error),
        }
    }
}

/// Kick's times: RFC 3339, or `2024-01-01 12:00:00` in UTC.
fn parse_time(text: &str) -> Option<jiff::Timestamp> {
    text.parse::<jiff::Timestamp>().ok().or_else(|| {
        text.parse::<jiff::civil::DateTime>()
            .ok()
            .and_then(|dt| dt.to_zoned(jiff::tz::TimeZone::UTC).ok())
            .map(|z| z.timestamp())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };
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

    const MASTER: &str = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1280x720\nhttps://stream.kick.com/x/720p30/playlist.m3u8\n";

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://kick.com/xqc"),
            Some(Link::Channel("xqc".into()))
        );
        assert_eq!(
            link("https://kick.com/video/abc-123"),
            Some(Link::Video("abc-123".into()))
        );
        assert_eq!(
            link("https://kick.com/xqc/videos/abc-123"),
            Some(Link::Video("abc-123".into()))
        );
        assert_eq!(
            link("https://kick.com/xqc/clips/clip_01"),
            Some(Link::Clip("clip_01".into()))
        );
        assert_eq!(
            link("https://kick.com/xqc?clip=clip_02"),
            Some(Link::Clip("clip_02".into()))
        );
        assert_eq!(link("https://kick.com/browse/categories"), None);
    }

    #[tokio::test]
    async fn channels_recordings_and_clips_resolve_to_hls() {
        let mut fixture = Fixture::new("kick", None);
        fixture.exchanges.push(get("https://kick.com/api/v2/channels/xqc", 200, "application/json", json!({
            "playback_url": "https://stream.kick.com/x/master.m3u8", "user": {"username": "xQc"},
            "livestream": {"id": 9, "is_live": true, "session_title": "gaming", "created_at": "2024-01-01 12:00:00", "thumbnail": {"url": "https://images.kick.com/t.jpg"}}
        }).to_string()));
        fixture.exchanges.push(get(
            "https://stream.kick.com/x/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MASTER.into(),
        ));
        fixture.exchanges.push(get(
            "https://stream.kick.com/x/720p30/playlist.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2.0,\n0.ts\n".into(),
        ));
        let resolver = KickResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://kick.com/xqc").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert!(resolved.live);
        assert_eq!(resolved.title.as_deref(), Some("gaming"));
        assert_eq!(resolved.uploader.as_deref(), Some("xQc"));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.variants[0].height, Some(720));

        let mut fixture = Fixture::new("kick", None);
        fixture.exchanges.push(get("https://kick.com/api/v1/video/abc-123", 200, "application/json", json!({
            "source": "https://stream.kick.com/x/master.m3u8", "created_at": "2024-01-02T00:00:00.000000Z",
            "livestream": {"session_title": "yesterday", "duration": 5400000, "thumbnail": "https://images.kick.com/v.jpg", "channel": {"slug": "xqc", "user": {"username": "xQc"}}}
        }).to_string()));
        fixture.exchanges.push(get(
            "https://stream.kick.com/x/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MASTER.into(),
        ));
        fixture.exchanges.push(get(
            "https://stream.kick.com/x/720p30/playlist.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2.0,\n0.ts\n#EXT-X-ENDLIST\n".into(),
        ));
        let resolver = KickResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://kick.com/video/abc-123").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert!(!resolved.live);
        assert_eq!(resolved.duration, Some(Duration::from_secs(5400)));
        assert_eq!(
            resolved.uploader_url.unwrap().as_str(),
            "https://kick.com/xqc"
        );

        let mut fixture = Fixture::new("kick", None);
        fixture.exchanges.push(get("https://kick.com/api/v2/clips/clip_01", 200, "application/json", json!({
            "clip": {"title": "Clipped", "duration": 20, "is_mature": true, "video_url": "https://clips.kick.com/c/master.m3u8", "thumbnail_url": "https://images.kick.com/c.jpg", "channel": {"slug": "xqc", "username": "xQc"}}
        }).to_string()));
        fixture.exchanges.push(get(
            "https://clips.kick.com/c/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2.0,\n0.ts\n#EXT-X-ENDLIST\n".into(),
        ));
        let resolver = KickResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://kick.com/xqc/clips/clip_01").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.title.as_deref(), Some("Clipped"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(20)));
        assert_eq!(resolved.age_limit, Some(18));

        let mut fixture = Fixture::new("kick", None);
        fixture.exchanges.push(get(
            "https://kick.com/api/v2/channels/xqc",
            403,
            "text/html",
            "<html>Just a moment...</html>".into(),
        ));
        let resolver = KickResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://kick.com/xqc").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("bot protection")),
            "{error}"
        );
    }
}
