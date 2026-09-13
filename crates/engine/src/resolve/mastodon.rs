//! Mastodon posts with video, on any server speaking the Mastodon API: the status is
//! read through `/api/v1/statuses/{id}`, which Mastodon, Pleroma, Akkoma, GoToSocial and
//! the other servers of the fediverse answer alike. A page shaped like a Mastodon post on
//! a server that turns out not to speak the API is handed on to the next resolver.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    ClipRange, MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionSupport, Variant, VariantKind, clean_title, timestamp_hint,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "mastodon";

static RE_STATUS_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9A-Za-z]{6,}$").unwrap());
static RE_ACCOUNT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^@[A-Za-z0-9_.-]+(?:@[A-Za-z0-9.-]+)?$").unwrap());

/// A status on a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusRef {
    pub server: Url,
    pub id: String,
}

/// Whether the path names a status the way Mastodon and its kin write their links.
pub fn parse_link(url: &Url) -> Option<StatusRef> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    url.host_str()?;
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let id = match segments.as_slice() {
        [account, id] if RE_ACCOUNT.is_match(account) && RE_STATUS_ID.is_match(id) => id,
        ["users", _, "statuses", id] if RE_STATUS_ID.is_match(id) => id,
        ["web", "statuses", id] if RE_STATUS_ID.is_match(id) => id,
        ["web", account, id] if RE_ACCOUNT.is_match(account) && RE_STATUS_ID.is_match(id) => id,
        ["notice", id] if RE_STATUS_ID.is_match(id) => id,
        ["statuses", id] if RE_STATUS_ID.is_match(id) => id,
        ["deck", account, id] if RE_ACCOUNT.is_match(account) && RE_STATUS_ID.is_match(id) => id,
        _ => return None,
    };
    if !id.chars().all(|c| c.is_ascii_digit()) && segments.first() != Some(&"notice") {
        return None;
    }
    let mut server = url.clone();
    server.set_path("/");
    server.set_query(None);
    server.set_fragment(None);
    Some(StatusRef {
        server,
        id: id.to_string(),
    })
}

pub struct MastodonResolver {
    http: Http,
    /// Servers found not to speak the API, so their links are handed on at once.
    declined: Mutex<HashMap<String, Timestamp>>,
}

/// How long a server stays remembered as not speaking the API.
const DECLINE_MEMORY: Duration = Duration::from_secs(6 * 60 * 60);

impl MastodonResolver {
    pub fn new(http: Http) -> Self {
        Self {
            http,
            declined: Mutex::new(HashMap::new()),
        }
    }

    fn remembered_decline(&self, host: &str) -> bool {
        let mut declined = self.declined.lock().unwrap_or_else(|e| e.into_inner());
        let now = Timestamp::now();
        declined.retain(|_, at| {
            now.duration_since(*at) < jiff::SignedDuration::try_from(DECLINE_MEMORY).unwrap_or_default()
        });
        declined.contains_key(host)
    }

    fn remember_decline(&self, host: &str) {
        self.declined
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(host.to_string(), Timestamp::now());
    }

    async fn status(&self, status: &StatusRef, origin: &Url) -> Result<Value, ResolveError> {
        let host = status.server.host_str().unwrap_or("").to_string();
        if self.remembered_decline(&host) {
            return Err(ResolveError::Unsupported(origin.clone()));
        }
        let api = status
            .server
            .join(&format!("api/v1/statuses/{}", status.id))
            .map_err(|e| ResolveError::malformed(origin, e.to_string()))?;
        let response = self
            .http
            .get(api.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "application/json")
            .send()
            .await?;
        let code = response.status.as_u16();
        let content_type = response.content_type().map(str::to_owned).unwrap_or_default();
        let text = response.text(MAX_PAGE).await?;
        let json: Option<Value> = serde_json::from_str(&text).ok();
        let Some(json) = json.filter(|j| j.is_object()) else {
            // No JSON at all: this is not a server speaking the API.
            if !content_type.contains("json") {
                self.remember_decline(&host);
            }
            return Err(ResolveError::Unsupported(origin.clone()));
        };
        match code {
            200..=299 => {}
            404 => return Err(ResolveError::NotFound(origin.clone())),
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            401 | 403 => {
                let message = json["error"].as_str().unwrap_or("").to_string();
                // Servers that want a login for every status, such as Pixelfed, are
                // handed on to the page readers rather than declared broken.
                if message.to_ascii_lowercase().contains("unauthenticated")
                    || message.to_ascii_lowercase().contains("authenticated user")
                {
                    self.remember_decline(&host);
                    return Err(ResolveError::Unsupported(origin.clone()));
                }
                return Err(ResolveError::unavailable(
                    origin,
                    if message.is_empty() {
                        format!("the server answered HTTP {code}")
                    } else {
                        message
                    },
                ));
            }
            _ => {
                return Err(ResolveError::unavailable(
                    origin,
                    json["error"]
                        .as_str()
                        .map(String::from)
                        .unwrap_or_else(|| format!("the server answered HTTP {code}")),
                ));
            }
        }
        if json["id"].is_null() || json["account"].is_null() {
            self.remember_decline(&host);
            return Err(ResolveError::Unsupported(origin.clone()));
        }
        Ok(json)
    }
}

/// The video and animated image attachments of a status.
fn video_attachments(status: &Value) -> Vec<&Value> {
    status["media_attachments"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| matches!(a["type"].as_str(), Some("video") | Some("gifv")))
        .collect()
}

fn variant_of(attachment: &Value) -> Option<Variant> {
    let url = attachment["url"]
        .as_str()
        .or_else(|| attachment["remote_url"].as_str())
        .and_then(|u| Url::parse(u).ok())?;
    let meta = &attachment["meta"]["original"];
    let playlist = url.path().ends_with(".m3u8");
    let mut v = Variant::new(
        url,
        if playlist {
            VariantKind::Hls
        } else {
            VariantKind::File
        },
    );
    if !playlist {
        v.container = Some(
            super::path_extension(&v.url)
                .as_deref()
                .and_then(Container::from_extension)
                .unwrap_or(Container::Mp4),
        );
        v.video = Some(VideoCodec::H264);
        v.audio = (attachment["type"].as_str() != Some("gifv")).then_some(AudioCodec::Aac);
    }
    v.width = meta["width"].as_u64().map(|w| w as u32);
    v.height = meta["height"].as_u64().map(|h| h as u32);
    v.fps = meta["frame_rate"]
        .as_str()
        .and_then(super::parse_rate)
        .filter(|f| *f > 0.0);
    v.bitrate = meta["bitrate"].as_u64().filter(|b| *b > 0);
    v.duration = meta["duration"]
        .as_f64()
        .filter(|d| *d > 0.0)
        .map(Duration::from_secs_f64);
    Some(v)
}

/// A status's text without its HTML.
fn plain_text(html: &str) -> Option<String> {
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
    let text = out
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&amp;", "&");
    clean_title(&text)
}

fn base_of(status: &Value, status_ref: &StatusRef) -> Resolved {
    let account = &status["account"];
    let acct = account["acct"].as_str().unwrap_or("");
    let mut resolved = Resolved::new(PLATFORM);
    resolved.id = status["id"].as_str().map(String::from);
    resolved.title = status["content"]
        .as_str()
        .and_then(plain_text)
        .or_else(|| status["spoiler_text"].as_str().and_then(clean_title))
        .or_else(|| (!acct.is_empty()).then(|| format!("Post by @{acct}")));
    resolved.description = status["content"].as_str().and_then(plain_text);
    resolved.uploader = account["display_name"]
        .as_str()
        .and_then(clean_title)
        .or_else(|| (!acct.is_empty()).then(|| format!("@{acct}")));
    resolved.uploader_url = account["url"].as_str().and_then(|u| Url::parse(u).ok());
    resolved.uploaded_at = status["created_at"]
        .as_str()
        .and_then(|t| t.parse::<Timestamp>().ok());
    resolved.webpage_url = status["url"]
        .as_str()
        .and_then(|u| Url::parse(u).ok())
        .or_else(|| status_ref.server.join(&format!("@{acct}/{}", status_ref.id)).ok());
    resolved.age_limit = status["sensitive"].as_bool().filter(|s| *s).map(|_| 18);
    resolved
}

#[async_trait]
impl Resolver for MastodonResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Mastodon and the fediverse",
            hosts: &["*"],
            features: &["posts", "boosts", "multiple attachments", "any server"],
            formats: &["mp4", "webm", "hls"],
            session: SessionSupport::None,
            examples: &["https://mastodon.social/@sonic_hedgeblog/117256980960208736"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let status_ref = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let mut status = self.status(&status_ref, url).await?;
        if video_attachments(&status).is_empty() && status["reblog"].is_object() {
            status = status["reblog"].clone();
        }
        let attachments = video_attachments(&status);
        if attachments.is_empty() {
            return Err(if status["media_attachments"]
                .as_array()
                .is_some_and(|a| !a.is_empty())
            {
                ResolveError::unavailable(url, "the post's attachments are not videos")
            } else {
                ResolveError::NotFound(url.clone())
            });
        }
        let base = base_of(&status, &status_ref);
        if attachments.len() > 1 {
            let entries = attachments
                .iter()
                .enumerate()
                .filter_map(|(index, attachment)| {
                    let mut entry_url = base.webpage_url.clone()?;
                    entry_url.set_fragment(Some(&format!("attachment-{}", index + 1)));
                    Some(PlaylistEntry {
                        url: entry_url,
                        title: attachment["description"]
                            .as_str()
                            .and_then(clean_title)
                            .or_else(|| base.title.as_ref().map(|t| format!("{t} ({})", index + 1))),
                        duration: attachment["meta"]["original"]["duration"]
                            .as_f64()
                            .filter(|d| *d > 0.0)
                            .map(Duration::from_secs_f64),
                    })
                })
                .collect::<Vec<_>>();
            if attachment_index(url).is_none() {
                return Ok(Resolution::Playlist(Playlist {
                    resolver: PLATFORM.into(),
                    id: base.id,
                    title: base.title,
                    total: Some(entries.len()),
                    entries,
                }));
            }
        }
        let index = attachment_index(url)
            .unwrap_or(1)
            .clamp(1, attachments.len());
        let attachment = attachments[index - 1];
        let variant = variant_of(attachment)
            .ok_or_else(|| ResolveError::malformed(url, "the attachment has no URL"))?;
        let mut resolved = base;
        if attachments.len() > 1 {
            resolved.id = resolved.id.map(|id| format!("{id}-{index}"));
            if let Some(description) = attachment["description"].as_str().and_then(clean_title) {
                resolved.title = Some(description);
            }
        }
        resolved.duration = variant.duration;
        resolved.thumbnail = attachment["preview_url"]
            .as_str()
            .and_then(|u| Url::parse(u).ok());
        resolved.clip = timestamp_hint(url).map(|start| ClipRange { start, end: None });
        resolved.variants = vec![variant];
        Ok(Resolution::from(resolved))
    }
}

/// Which attachment a link picks out, from an `#attachment-N` fragment.
fn attachment_index(url: &Url) -> Option<usize> {
    url.fragment()?
        .strip_prefix("attachment-")?
        .parse::<usize>()
        .ok()
        .filter(|n| *n >= 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };
    use serde_json::json;

    fn get(url: &str, status: u16, content_type: &str, body: &str) -> Exchange {
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
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    fn status(attachments: Vec<Value>) -> Value {
        json!({
            "id": "117256980960208736", "url": "https://mastodon.social/@sonic_hedgeblog/117256980960208736", "created_at": "2026-09-12T07:52:18.582Z",
            "content": "<p>&#39;Promba&#39; by MikeWad<br />Get ready for a treacherous integrity test!</p>", "sensitive": false, "reblog": null,
            "account": {"acct": "sonic_hedgeblog", "display_name": "Sonic The Hedgeblog", "url": "https://mastodon.social/@sonic_hedgeblog"},
            "media_attachments": attachments
        })
    }

    fn gifv() -> Value {
        json!({"type": "gifv", "url": "https://files.mastodon.social/media_attachments/files/117/original/31e99f4c7bfe7009.mp4",
            "preview_url": "https://files.mastodon.social/media_attachments/files/117/small/31e99f4c7bfe7009.png", "description": "Promba gameplay",
            "meta": {"original": {"width": 640, "height": 360, "frame_rate": "30/1", "duration": 8.0, "bitrate": 708327}}})
    }

    #[test]
    fn links_shaped_like_statuses_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let expected = StatusRef {
            server: Url::parse("https://mastodon.social/").unwrap(),
            id: "117256980960208736".into(),
        };
        assert_eq!(
            link("https://mastodon.social/@sonic_hedgeblog/117256980960208736"),
            Some(expected.clone())
        );
        assert_eq!(
            link("https://mastodon.social/@someone@other.social/117256980960208736"),
            Some(expected.clone())
        );
        assert_eq!(
            link("https://mastodon.social/users/sonic_hedgeblog/statuses/117256980960208736"),
            Some(expected.clone())
        );
        assert_eq!(
            link("https://mastodon.social/web/statuses/117256980960208736"),
            Some(expected)
        );
        assert!(link("https://pleroma.example/notice/AbCdEf123456").is_some());
        assert_eq!(link("https://mastodon.social/@sonic_hedgeblog"), None);
        assert_eq!(link("https://example.com/@user/not-a-status"), None);
        assert_eq!(link("https://www.youtube.com/@channel/videos"), None);
        assert_eq!(
            attachment_index(&Url::parse("https://m.social/@a/1#attachment-2").unwrap()),
            Some(2)
        );
    }

    #[tokio::test]
    async fn statuses_with_one_video_resolve() {
        let mut fixture = Fixture::new("mastodon", None);
        fixture.exchanges.push(get(
            "https://mastodon.social/api/v1/statuses/117256980960208736",
            200,
            "application/json",
            &status(vec![gifv()]).to_string(),
        ));
        let resolver = MastodonResolver::new(Http::replay(fixture));
        let url = Url::parse("https://mastodon.social/@sonic_hedgeblog/117256980960208736").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("117256980960208736"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("'Promba' by MikeWad Get ready for a treacherous integrity test!")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("Sonic The Hedgeblog"));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.duration, Some(Duration::from_secs(8)));
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].height, Some(360));
        assert_eq!(resolved.variants[0].fps, Some(30.0));
        assert_eq!(resolved.variants[0].bitrate, Some(708327));
        assert!(resolved.variants[0].audio.is_none());
        assert!(resolved.thumbnail.is_some());
    }

    #[tokio::test]
    async fn several_videos_make_a_playlist_and_one_is_picked_by_fragment() {
        let mut second = gifv();
        second["type"] = json!("video");
        second["url"] = json!("https://files.mastodon.social/second.mp4");
        second["description"] = json!("Second clip");
        let mut fixture = Fixture::new("mastodon", None);
        fixture.exchanges.push(get(
            "https://mastodon.social/api/v1/statuses/117256980960208736",
            200,
            "application/json",
            &status(vec![gifv(), second.clone()]).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://mastodon.social/api/v1/statuses/117256980960208736",
            200,
            "application/json",
            &status(vec![gifv(), second]).to_string(),
        ));
        let resolver = MastodonResolver::new(Http::replay(fixture));
        let url = Url::parse("https://mastodon.social/@sonic_hedgeblog/117256980960208736").unwrap();
        let playlist = match resolver.resolve(&url).await.unwrap() {
            Resolution::Playlist(playlist) => playlist,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[1].url.as_str(),
            "https://mastodon.social/@sonic_hedgeblog/117256980960208736#attachment-2"
        );
        assert_eq!(playlist.entries[1].title.as_deref(), Some("Second clip"));
        let second = resolver
            .resolve(&playlist.entries[1].url)
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(second.id.as_deref(), Some("117256980960208736-2"));
        assert_eq!(second.title.as_deref(), Some("Second clip"));
        assert_eq!(second.variants[0].url.as_str(), "https://files.mastodon.social/second.mp4");
        assert_eq!(second.variants[0].audio, Some(AudioCodec::Aac));
    }

    #[tokio::test]
    async fn servers_not_speaking_the_api_are_passed_on_and_remembered() {
        let mut fixture = Fixture::new("mastodon", None);
        fixture.exchanges.push(get(
            "https://blog.example/api/v1/statuses/123456789",
            404,
            "text/html",
            "<html>not here</html>",
        ));
        fixture.exchanges.push(get(
            "https://pixelfed.social/api/v1/statuses/1004264982774109168",
            401,
            "application/json",
            r#"{"message":"Unauthenticated."}"#,
        ));
        fixture.exchanges.push(get(
            "https://mastodon.social/api/v1/statuses/999999999999999999",
            404,
            "application/json",
            r#"{"error":"Not Found"}"#,
        ));
        fixture.exchanges.push(get(
            "https://mastodon.social/api/v1/statuses/117256980960208736",
            200,
            "application/json",
            &status(vec![]).to_string(),
        ));
        let resolver = MastodonResolver::new(Http::replay(fixture));
        let url = |s: &str| Url::parse(s).unwrap();
        assert!(matches!(
            resolver.resolve(&url("https://blog.example/@someone/123456789")).await.unwrap_err(),
            ResolveError::Unsupported(_)
        ));
        // Remembered: no request is made the second time.
        assert!(matches!(
            resolver.resolve(&url("https://blog.example/@someone/123456789")).await.unwrap_err(),
            ResolveError::Unsupported(_)
        ));
        let missing = resolver
            .resolve(&url("https://pixelfed.social/p/TiredMa1d/1004264982774109168"))
            .await
            .unwrap_err();
        assert!(matches!(missing, ResolveError::NotFound(_)), "{missing}");
        assert!(matches!(
            resolver.resolve(&url("https://mastodon.social/@x/999999999999999999")).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
        assert!(matches!(
            resolver.resolve(&url("https://mastodon.social/@sonic_hedgeblog/117256980960208736")).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
