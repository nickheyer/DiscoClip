//! Loom recordings, through the API the share page calls: the recording's metadata from
//! the site's GraphQL, and its stream from the raw URL endpoint, whose CloudFront
//! credentials are kept as cookies so every segment of the stream is let through.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::{Value, json};
use url::Url;

use super::{
    ClipRange, MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionCheck,
    SessionSupport, Variant, VariantKind, clean_title, hls, timestamp_hint,
};
use crate::http::{BROWSER_UA, Cookie, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "loom";
const SITE: &str = "https://www.loom.com/";
const GRAPHQL: &str = "https://www.loom.com/graphql";
/// The cookie a logged-in loom.com session carries.
const SESSION_COOKIE: &str = "connect.sid";
const METADATA_QUERY: &str = "query GetVideoSSR($videoId: ID!, $password: String) { getVideo(id: $videoId, password: $password) { __typename ... on RegularUserVideo { id name description createdAt owner { display_name __typename } video_properties { width height duration __typename } } ... on PrivateVideo { id status message __typename } ... on VideoPasswordMissingOrIncorrect { id message __typename } } }";

static RE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{32}$").unwrap());

/// A recording by its id, with the password the link carries for a protected one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoRef {
    pub id: String,
    pub password: Option<String>,
}

pub fn parse_link(url: &Url) -> Option<VideoRef> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !(host == "loom.com" || host.ends_with(".loom.com")) {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let id = match segments.as_slice() {
        ["share", id, ..] | ["embed", id, ..] | ["i", id, ..] | ["v", id, ..] if RE_ID.is_match(id) => *id,
        [id] if RE_ID.is_match(id) => *id,
        _ => return None,
    };
    Some(VideoRef {
        id: id.to_string(),
        password: url
            .query_pairs()
            .find(|(k, _)| k == "password" || k == "pwd")
            .map(|(_, v)| v.into_owned())
            .filter(|p| !p.is_empty()),
    })
}

pub struct LoomResolver {
    http: Http,
}

impl LoomResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    fn logged_in(&self) -> bool {
        self.http.jar(PLATFORM).get(SESSION_COOKIE).is_some()
    }

    /// The recording's metadata, with the site's answer about a private or locked one.
    async fn metadata(&self, video: &VideoRef, origin: &Url) -> Result<Value, ResolveError> {
        let body = json!({
            "operationName": "GetVideoSSR",
            "variables": {"videoId": video.id, "password": video.password},
            "query": METADATA_QUERY
        });
        let response = self
            .http
            .post(Url::parse(GRAPHQL).expect("valid"))
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("x-loom-request-source", "loom_web_1")
            .header("apollographql-client-name", "web")
            .header("apollographql-client-version", "1")
            .header("referer", &format!("{SITE}share/{}", video.id))
            .json(&body)
            .send()
            .await?;
        let status = response.status;
        if status.as_u16() == 429 {
            return Err(ResolveError::RateLimited(origin.clone()));
        }
        let answer: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, format!("GraphQL: {e}")))?;
        let record = &answer["data"]["getVideo"];
        match record["__typename"].as_str() {
            Some("RegularUserVideo") => Ok(record.clone()),
            Some("PrivateVideo") => Err(if self.logged_in() {
                ResolveError::unavailable(
                    origin,
                    record["message"]
                        .as_str()
                        .unwrap_or("the recording is private")
                        .to_string(),
                )
            } else {
                ResolveError::login_required(
                    origin,
                    PLATFORM,
                    record["message"]
                        .as_str()
                        .unwrap_or("the recording is private")
                        .to_string(),
                )
            }),
            Some("VideoPasswordMissingOrIncorrect") => Err(ResolveError::unavailable(
                origin,
                "the recording is password protected; add ?password= to the link",
            )),
            None if record.is_null() => {
                let message = answer["errors"][0]["message"].as_str().unwrap_or("");
                Err(if message.to_ascii_lowercase().contains("not found") || message.is_empty() {
                    ResolveError::NotFound(origin.clone())
                } else {
                    ResolveError::unavailable(origin, message.to_string())
                })
            }
            Some(other) => Err(ResolveError::unavailable(
                origin,
                format!("the site describes the recording as {other}"),
            )),
            None => Err(ResolveError::malformed(origin, "the GraphQL answer names no type")),
        }
    }

    /// Asks one of the session URL endpoints for the recording's file: the transcoded MP4
    /// or the raw HLS stream with its CloudFront credentials.
    async fn url_of(&self, video: &VideoRef, endpoint: &str, origin: &Url) -> Result<Option<Value>, ResolveError> {
        let api = Url::parse(&format!("{SITE}api/campaigns/sessions/{}/{endpoint}", video.id)).expect("valid");
        let body = json!({
            "anonID": uuid::Uuid::now_v7().to_string(),
            "deviceID": null,
            "force_original": false,
            "password": video.password
        });
        let response = self
            .http
            .post(api)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "*/*")
            .header("origin", "https://www.loom.com")
            .header("referer", &format!("{SITE}share/{}", video.id))
            .json(&body)
            .send()
            .await?;
        match response.status.as_u16() {
            204 => Ok(None),
            200..=299 => {
                let value: Value = response
                    .json(MAX_PAGE)
                    .await
                    .map_err(|e| ResolveError::malformed(origin, format!("{endpoint}: {e}")))?;
                Ok(Some(value))
            }
            404 => Err(ResolveError::NotFound(origin.clone())),
            429 => Err(ResolveError::RateLimited(origin.clone())),
            401 | 403 => Err(if self.logged_in() {
                ResolveError::unavailable(origin, format!("the {endpoint} endpoint refused the request"))
            } else {
                ResolveError::login_required(origin, PLATFORM, "the recording is not public")
            }),
            status => Err(ResolveError::unavailable(
                origin,
                format!("the {endpoint} endpoint answered HTTP {status}"),
            )),
        }
    }

    /// Keeps CloudFront credentials as cookies on the stream's host, so the playlist's
    /// segments, which the credentials' query would not follow, are let through.
    fn keep_credentials(&self, stream: &Url, credentials: &Value) {
        let credentials: Value = match credentials {
            Value::String(text) => serde_json::from_str(text).unwrap_or(Value::Null),
            other => other.clone(),
        };
        let Some(host) = stream.host_str() else {
            return;
        };
        let Some(map) = credentials.as_object() else {
            return;
        };
        self.http.with_jar(PLATFORM, |jar| {
            for (key, value) in map {
                if let Some(value) = value.as_str() {
                    let name = format!("CloudFront-{key}");
                    let mut cookie = Cookie::new(name, value, host);
                    cookie.secure = true;
                    jar.insert(cookie);
                }
            }
        });
    }
}

#[async_trait]
impl Resolver for LoomResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Loom",
            hosts: &["loom.com"],
            features: &["recordings", "embeds", "password protected recordings"],
            formats: &["mp4", "hls"],
            session: SessionSupport::Optional,
            examples: &["https://www.loom.com/share/43d05f362f734614a2e81b4694a3a523"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let video = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let record = self.metadata(&video, url).await?;
        let properties = &record["video_properties"];
        let duration = properties["duration"]
            .as_f64()
            .filter(|d| *d > 0.0)
            .map(Duration::from_secs_f64);
        let width = properties["width"].as_u64().map(|w| w as u32);
        let height = properties["height"].as_u64().map(|h| h as u32);
        let mut variants: Vec<Variant> = Vec::new();
        if let Some(transcoded) = self.url_of(&video, "transcoded-url", url).await?
            && let Some(file) = transcoded["url"].as_str().and_then(|u| Url::parse(u).ok())
        {
            let mut v = Variant::new(file, VariantKind::File);
            v.container = Some(Container::Mp4);
            v.video = Some(VideoCodec::H264);
            v.audio = Some(AudioCodec::Aac);
            v.width = width;
            v.height = height;
            v.duration = duration;
            v.format_id = Some("transcoded".into());
            variants.push(v);
        }
        if let Some(raw) = self.url_of(&video, "raw-url", url).await?
            && let Some(stream) = raw["url"].as_str().and_then(|u| Url::parse(u).ok())
        {
            if let Some(credentials) = raw.get("part_credentials") {
                self.keep_credentials(&stream, credentials);
            }
            if stream.path().ends_with(".m3u8") {
                let expanded = hls::expand(&self.http, &stream, PLATFORM, BROWSER_UA, &[]).await?;
                for mut v in expanded.variants {
                    v.duration = duration.or(v.duration);
                    if v.width.is_none() {
                        v.width = width;
                        v.height = height;
                    }
                    v.format_id = Some("raw".into());
                    variants.push(v);
                }
            } else {
                let mut v = Variant::new(stream, VariantKind::File);
                v.container = Some(Container::Mp4);
                v.video = Some(VideoCodec::H264);
                v.audio = Some(AudioCodec::Aac);
                v.width = width;
                v.height = height;
                v.duration = duration;
                v.format_id = Some("raw".into());
                variants.push(v);
            }
        }
        if variants.is_empty() {
            return Err(ResolveError::unavailable(url, "the site handed out no stream for the recording"));
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(video.id.clone());
        resolved.title = record["name"].as_str().and_then(clean_title);
        resolved.description = record["description"].as_str().and_then(clean_title);
        resolved.uploader = record["owner"]["display_name"].as_str().and_then(clean_title);
        resolved.uploaded_at = record["createdAt"]
            .as_str()
            .and_then(|t| t.parse::<Timestamp>().ok());
        resolved.duration = duration;
        resolved.thumbnail = Url::parse(&format!("https://cdn.loom.com/sessions/thumbnails/{}-00001.jpg", video.id)).ok();
        resolved.webpage_url = Url::parse(&format!("{SITE}share/{}", video.id)).ok();
        resolved.clip = timestamp_hint(url).map(|start| ClipRange { start, end: None });
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        if !self.logged_in() {
            return Ok(SessionCheck::LoggedOut);
        }
        let body = json!({
            "operationName": "GetUser",
            "variables": {},
            "query": "query GetUser { getUser { __typename ... on RegularUser { id display_name email __typename } } }"
        });
        let response = self
            .http
            .post(Url::parse(GRAPHQL).expect("valid"))
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("x-loom-request-source", "loom_web_1")
            .header("apollographql-client-name", "web")
            .header("apollographql-client-version", "1")
            .json(&body)
            .send()
            .await?;
        if !response.is_success() {
            return Ok(SessionCheck::LoggedOut);
        }
        let answer: Value = match response.json(MAX_PAGE).await {
            Ok(answer) => answer,
            Err(_) => return Ok(SessionCheck::LoggedOut),
        };
        let user = &answer["data"]["getUser"];
        Ok(match user["display_name"].as_str().or_else(|| user["email"].as_str()) {
            Some(name) if user["__typename"].as_str() == Some("RegularUser") && !name.is_empty() => {
                SessionCheck::LoggedIn {
                    account: name.to_string(),
                }
            }
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

    fn exchange(method: &str, url: &str, status: u16, content_type: &str, body: &str) -> Exchange {
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
                headers: vec![("content-type".into(), content_type.into())],
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    const ID: &str = "43d05f362f734614a2e81b4694a3a523";
    const STREAM: &str = "https://luna.loom.com/id/43d05f362f734614a2e81b4694a3a523/rev/8edb/resource/hls/playlist-split.m3u8";
    const MASTER: &str = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",NAME=\"audio\",DEFAULT=YES,URI=\"mediaplaylist-audio.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=1500000,RESOLUTION=1280x720,AUDIO=\"audio\"\nmediaplaylist-video-bitrate1500.m3u8\n";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:5\n#EXTINF:4.16,\n43d05f-video-0.ts\n#EXTINF:4.03,\n43d05f-video-1.ts\n#EXT-X-ENDLIST\n";

    fn metadata(typename: &str) -> String {
        json!({"data": {"getVideo": {"__typename": typename, "id": ID, "name": "A Ruler for Windows - 28 March 2022", "description": null, "createdAt": "2022-03-28T07:57:18.337Z",
            "owner": {"display_name": "wILLIAM PIP"}, "video_properties": {"width": 1280, "height": 720, "duration": 27}, "message": "This video is private"}}}).to_string()
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link(&format!("https://www.loom.com/share/{ID}")),
            Some(VideoRef {
                id: ID.into(),
                password: None
            })
        );
        assert_eq!(
            link(&format!("https://www.loom.com/embed/{ID}?sid=abc&password=secret")),
            Some(VideoRef {
                id: ID.into(),
                password: Some("secret".into())
            })
        );
        assert_eq!(link("https://www.loom.com/share/not-an-id"), None);
        assert_eq!(link("https://www.loom.com/"), None);
    }

    #[tokio::test]
    async fn recordings_resolve_with_their_credentials_kept_as_cookies() {
        let mut fixture = Fixture::new("loom", None);
        fixture.exchanges.push(exchange("POST", GRAPHQL, 200, "application/json", &metadata("RegularUserVideo")));
        fixture.exchanges.push(exchange("POST", &format!("{SITE}api/campaigns/sessions/{ID}/transcoded-url"), 204, "text/plain", ""));
        fixture.exchanges.push(exchange(
            "POST",
            &format!("{SITE}api/campaigns/sessions/{ID}/raw-url"),
            200,
            "application/json",
            &json!({"url": STREAM, "part_credentials": "{\"Policy\":\"POLICY\",\"Key-Pair-Id\":\"KEYPAIR\",\"Signature\":\"SIG\"}"}).to_string(),
        ));
        fixture.exchanges.push(exchange("GET", STREAM, 200, "application/vnd.apple.mpegurl", MASTER));
        fixture.exchanges.push(exchange(
            "GET",
            "https://luna.loom.com/id/43d05f362f734614a2e81b4694a3a523/rev/8edb/resource/hls/mediaplaylist-video-bitrate1500.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MEDIA,
        ));
        let http = Http::replay(fixture);
        let resolver = LoomResolver::new(http.clone());
        let url = Url::parse(&format!("https://www.loom.com/share/{ID}")).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some(ID));
        assert_eq!(resolved.title.as_deref(), Some("A Ruler for Windows - 28 March 2022"));
        assert_eq!(resolved.uploader.as_deref(), Some("wILLIAM PIP"));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.duration, Some(Duration::from_secs(27)));
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].kind, VariantKind::Hls);
        assert_eq!(resolved.variants[0].height, Some(720));
        assert!(resolved.variants[0].audio_url.is_some());
        let jar = http.jar(PLATFORM);
        assert_eq!(jar.get("CloudFront-Policy").unwrap().value, "POLICY");
        assert_eq!(jar.get("CloudFront-Key-Pair-Id").unwrap().domain, "luna.loom.com");
        assert!(
            jar.header_for(&Url::parse("https://luna.loom.com/id/x/video-0.ts").unwrap(), Timestamp::now())
                .unwrap()
                .contains("CloudFront-Signature=SIG")
        );
    }

    #[tokio::test]
    async fn transcoded_files_private_and_locked_recordings() {
        let mut fixture = Fixture::new("loom", None);
        fixture.exchanges.push(exchange("POST", GRAPHQL, 200, "application/json", &metadata("RegularUserVideo")));
        fixture.exchanges.push(exchange(
            "POST",
            &format!("{SITE}api/campaigns/sessions/{ID}/transcoded-url"),
            200,
            "application/json",
            &json!({"url": "https://cdn.loom.com/sessions/transcoded/43d05f.mp4?Policy=p"}).to_string(),
        ));
        fixture.exchanges.push(exchange("POST", &format!("{SITE}api/campaigns/sessions/{ID}/raw-url"), 204, "text/plain", ""));
        fixture.exchanges.push(exchange("POST", GRAPHQL, 200, "application/json", &metadata("PrivateVideo")));
        fixture.exchanges.push(exchange("POST", GRAPHQL, 200, "application/json", &metadata("VideoPasswordMissingOrIncorrect")));
        fixture.exchanges.push(exchange("POST", GRAPHQL, 200, "application/json", r#"{"data":{"getVideo":null},"errors":[{"message":"Video not found"}]}"#));
        let resolver = LoomResolver::new(Http::replay(fixture));
        let url = Url::parse(&format!("https://www.loom.com/share/{ID}")).unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].kind, VariantKind::File);
        assert_eq!(resolved.variants[0].format_id.as_deref(), Some("transcoded"));
        assert!(matches!(
            resolver.resolve(&url).await.unwrap_err(),
            ResolveError::LoginRequired { .. }
        ));
        let error = resolver.resolve(&url).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("password")),
            "{error}"
        );
        assert!(matches!(
            resolver.resolve(&url).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
