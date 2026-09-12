//! Twitch recordings, clips and live channels, through the GQL API the web site uses,
//! with playback tokens turned into HLS playlists.

use std::sync::{LazyLock, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};
use url::Url;

use super::{
    ClipRange, MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionCheck,
    SessionSupport, Variant, VariantKind, clean_title, fetch, hls, status_error, timestamp_hint,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "twitch";
const GQL: &str = "https://gql.twitch.tv/gql";
const SITE: &str = "https://www.twitch.tv/";
/// The client id the web site sent when this was written; the site's current one takes
/// over as soon as GQL rejects it.
pub const CLIENT_ID: &str = "kimne78kx3ncx6brgo4mv6wki5h1ko";
const USHER: &str = "https://usher.ttvnw.net";

static RE_CLIP_SLUG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]{6,}$").unwrap());
/// How the site's script names its client id.
static RE_CLIENT_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"clientId\s*[:=]\s*"([a-z0-9]{20,40})""#).unwrap());

/// What a link names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Video(String),
    Clip(String),
    Channel(String),
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
    if host == "clips.twitch.tv" {
        return segments
            .first()
            .filter(|s| RE_CLIP_SLUG.is_match(s))
            .map(|s| Link::Clip(s.to_string()));
    }
    if !(host == "twitch.tv" || host.ends_with(".twitch.tv")) {
        return None;
    }
    match segments.as_slice() {
        ["videos", id] | [_, "video", id] | [_, "v", id]
            if id.chars().all(|c| c.is_ascii_digit()) =>
        {
            Some(Link::Video(id.to_string()))
        }
        [_, "clip", slug] if RE_CLIP_SLUG.is_match(slug) => Some(Link::Clip(slug.to_string())),
        [channel]
            if channel
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_') =>
        {
            Some(Link::Channel(channel.to_ascii_lowercase()))
        }
        _ => None,
    }
}

pub struct TwitchResolver {
    http: Http,
    client_id: RwLock<String>,
}

impl TwitchResolver {
    pub fn new(http: Http) -> Self {
        Self {
            http,
            client_id: RwLock::new(CLIENT_ID.to_string()),
        }
    }

    pub fn client_id(&self) -> String {
        self.client_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn session_token(&self) -> Option<String> {
        self.http
            .jar(PLATFORM)
            .get("auth-token")
            .map(|c| c.value.clone())
    }

    /// Reads the client id the site currently sends, for when GQL stops taking ours.
    async fn refresh_client_id(&self, origin: &Url) -> Result<(), ResolveError> {
        let home = Url::parse(SITE).expect("valid");
        let fetched = fetch(&self.http, &home, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        let html = fetched.text();
        let found = RE_CLIENT_ID
            .captures(&html)
            .map(|c| c[1].to_string())
            .ok_or_else(|| {
                ResolveError::malformed(origin, "the Twitch site no longer shows its client id")
            })?;
        tracing::info!(client_id = %found, "twitch client id read from the site");
        *self.client_id.write().unwrap_or_else(|e| e.into_inner()) = found;
        Ok(())
    }

    /// POSTs `operations` to the GQL API, with the session's token when there is one.
    async fn gql(&self, operations: Value, origin: &Url) -> Result<Value, ResolveError> {
        let mut refreshed = false;
        loop {
            let client_id = self.client_id();
            let mut request = self
                .http
                .post(Url::parse(GQL).expect("valid"))
                .platform(PLATFORM)
                .user_agent(BROWSER_UA)
                .header("client-id", &client_id)
                .header("origin", "https://www.twitch.tv")
                .header("referer", SITE)
                .json(&operations);
            if let Some(token) = self.session_token() {
                request = request.header("authorization", &format!("OAuth {token}"));
            }
            let response = request.send().await?;
            let status = response.status;
            if status.is_success() {
                return response
                    .json(MAX_PAGE)
                    .await
                    .map_err(|e| ResolveError::malformed(origin, format!("GQL JSON: {e}")));
            }
            let body = response.text(MAX_PAGE).await.unwrap_or_default();
            if status.as_u16() == 400 && !refreshed && body.contains("Client-ID") {
                self.refresh_client_id(origin).await?;
                refreshed = true;
                continue;
            }
            return Err(match status.as_u16() {
                429 => ResolveError::RateLimited(origin.clone()),
                _ => ResolveError::unavailable(
                    origin,
                    format!(
                        "GQL answered HTTP {status}: {}",
                        body.chars().take(200).collect::<String>()
                    ),
                ),
            });
        }
    }

    /// The master playlist usher serves for a signed token, expanded; a refusal names
    /// subscriber-only content.
    async fn playlists(
        &self,
        path: &str,
        value: &str,
        signature: &str,
        url: &Url,
    ) -> Result<hls::Expanded, ResolveError> {
        let mut master = Url::parse(&format!("{USHER}/{path}.m3u8")).expect("valid");
        master
            .query_pairs_mut()
            .append_pair("sig", signature)
            .append_pair("token", value)
            .append_pair("allow_source", "true")
            .append_pair("allow_audio_only", "true")
            .append_pair("allow_spectre", "true")
            .append_pair("platform", "web")
            .append_pair("player", "twitchweb")
            .append_pair("supported_codecs", "h265,h264")
            .append_pair("playlist_include_framerate", "true")
            .append_pair(
                "p",
                &(jiff::Timestamp::now().as_millisecond() % 9_000_000 + 1_000_000).to_string(),
            );
        let fetched = fetch(&self.http, &master, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        if fetched.status.as_u16() == 403 {
            let error: Value = serde_json::from_slice(&fetched.body).unwrap_or(Value::Null);
            let code = error[0]["error_code"].as_str().unwrap_or("");
            let message = error[0]["error"].as_str().unwrap_or("");
            if matches!(
                code,
                "vod_manifest_restricted" | "unauthorized_entitlements"
            ) {
                return Err(if self.session_token().is_some() {
                    ResolveError::unavailable(
                        url,
                        "the session's account has no access to this subscriber-only content",
                    )
                } else {
                    ResolveError::login_required(url, PLATFORM, "subscriber-only content")
                });
            }
            let reason = [code, message]
                .iter()
                .filter(|s| !s.is_empty())
                .copied()
                .collect::<Vec<_>>()
                .join(": ");
            return Err(ResolveError::unavailable(
                url,
                if reason.is_empty() {
                    "usher refused the playlist (HTTP 403)".to_string()
                } else {
                    reason
                },
            ));
        }
        if let Some(error) = status_error(fetched.status, &master) {
            return Err(error);
        }
        hls::expand_playlist(
            &self.http,
            &master,
            &fetched.url,
            &fetched.body,
            PLATFORM,
            BROWSER_UA,
            &[],
        )
        .await
    }

    async fn video(&self, id: &str, url: &Url) -> Result<Resolved, ResolveError> {
        let answer = self
            .gql(
                json!([
                    {
                        "operationName": "PlaybackAccessToken_Template",
                        "query": "query PlaybackAccessToken_Template($login: String!, $isLive: Boolean!, $vodID: ID!, $isVod: Boolean!, $playerType: String!) {  streamPlaybackAccessToken(channelName: $login, params: {platform: \"web\", playerBackend: \"mediaplayer\", playerType: $playerType}) @include(if: $isLive) {    value    signature    __typename  }  videoPlaybackAccessToken(id: $vodID, params: {platform: \"web\", playerBackend: \"mediaplayer\", playerType: $playerType}) @include(if: $isVod) {    value    signature    __typename  }}",
                        "variables": {"isLive": false, "login": "", "isVod": true, "vodID": id, "playerType": "site"}
                    },
                    {
                        "query": format!("query {{ video(id: \"{id}\") {{ id title lengthSeconds createdAt previewThumbnailURL(height: 480, width: 854) description owner {{ login displayName }} }} }}"),
                        "variables": {}
                    }
                ]),
                url,
            )
            .await?;
        let token = &answer[0]["data"]["videoPlaybackAccessToken"];
        let video = &answer[1]["data"]["video"];
        if video.is_null() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let (value, signature) = match (token["value"].as_str(), token["signature"].as_str()) {
            (Some(value), Some(signature)) => (value, signature),
            _ => {
                let message = answer[0]["errors"][0]["message"]
                    .as_str()
                    .unwrap_or("no playback token was given");
                let lower = message.to_lowercase();
                return Err(
                    if lower.contains("subscri")
                        || self.session_token().is_none() && lower.contains("auth")
                    {
                        ResolveError::login_required(url, PLATFORM, message)
                    } else {
                        ResolveError::unavailable(url, message)
                    },
                );
            }
        };
        let expanded = self
            .playlists(&format!("vod/{id}"), value, signature, url)
            .await?;
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.to_string());
        resolved.title = video["title"].as_str().and_then(clean_title);
        resolved.description = video["description"].as_str().and_then(clean_title);
        resolved.uploader = video["owner"]["displayName"].as_str().and_then(clean_title);
        resolved.uploader_url = video["owner"]["login"]
            .as_str()
            .and_then(|l| Url::parse(&format!("{SITE}{l}")).ok());
        resolved.uploaded_at = video["createdAt"].as_str().and_then(|t| t.parse().ok());
        resolved.duration = video["lengthSeconds"]
            .as_u64()
            .filter(|s| *s > 0)
            .map(Duration::from_secs)
            .or(expanded.duration);
        resolved.thumbnail = video["previewThumbnailURL"]
            .as_str()
            .and_then(|t| Url::parse(t).ok());
        resolved.webpage_url = Url::parse(&format!("{SITE}videos/{id}")).ok();
        resolved.live = expanded.live;
        resolved.clip = timestamp_hint(url).map(|start| ClipRange { start, end: None });
        resolved.subtitles = expanded.subtitles;
        resolved.variants = expanded.variants;
        Ok(resolved)
    }

    async fn clip(&self, slug: &str, url: &Url) -> Result<Resolved, ResolveError> {
        let answer = self
            .gql(
                json!([{
                    "operationName": "VideoAccessToken_Clip",
                    "variables": {"slug": slug, "platform": "web"},
                    "extensions": {"persistedQuery": {"version": 1, "sha256Hash": "6fd3af2b22989506269b9ac02dd87eb4a6688392d67d94e41a6886f1e9f5c00f"}}
                }, {
                    "query": format!("query {{ clip(slug: \"{slug}\") {{ id title durationSeconds createdAt thumbnailURL viewCount broadcaster {{ login displayName }} video {{ id }} videoOffsetSeconds }} }}"),
                    "variables": {}
                }]),
                url,
            )
            .await?;
        let access = &answer[0]["data"]["clip"];
        let clip = &answer[1]["data"]["clip"];
        if clip.is_null() && access.is_null() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let token = &access["playbackAccessToken"];
        let mut variants = Vec::new();
        for quality in access["videoQualities"].as_array().into_iter().flatten() {
            let Some(mut source) = quality["sourceURL"]
                .as_str()
                .and_then(|u| Url::parse(u).ok())
            else {
                continue;
            };
            if let (Some(sig), Some(value)) = (token["signature"].as_str(), token["value"].as_str())
            {
                source
                    .query_pairs_mut()
                    .append_pair("sig", sig)
                    .append_pair("token", value);
            }
            let mut v = Variant::new(source, VariantKind::File);
            v.container = Some(Container::Mp4);
            v.video = Some(VideoCodec::H264);
            v.audio = Some(AudioCodec::Aac);
            v.height = quality["quality"].as_str().and_then(|q| q.parse().ok());
            v.fps = quality["frameRate"].as_f64().filter(|f| *f > 0.0);
            v.format_id = quality["quality"].as_str().map(String::from);
            v.label = quality["quality"].as_str().map(|q| format!("{q}p"));
            variants.push(v);
        }
        if variants.is_empty() {
            return Err(ResolveError::unavailable(
                url,
                "the clip has no video qualities",
            ));
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = clip["id"]
            .as_str()
            .map(String::from)
            .or_else(|| Some(slug.to_string()));
        resolved.title = clip["title"].as_str().and_then(clean_title);
        resolved.uploader = clip["broadcaster"]["displayName"]
            .as_str()
            .and_then(clean_title);
        resolved.uploader_url = clip["broadcaster"]["login"]
            .as_str()
            .and_then(|l| Url::parse(&format!("{SITE}{l}")).ok());
        resolved.uploaded_at = clip["createdAt"].as_str().and_then(|t| t.parse().ok());
        resolved.duration = clip["durationSeconds"]
            .as_f64()
            .filter(|d| *d > 0.0)
            .map(Duration::from_secs_f64);
        resolved.thumbnail = clip["thumbnailURL"]
            .as_str()
            .and_then(|t| Url::parse(t).ok());
        resolved.webpage_url = Url::parse(&format!("https://clips.twitch.tv/{slug}")).ok();
        for v in &mut variants {
            v.duration = resolved.duration;
        }
        resolved.variants = variants;
        Ok(resolved)
    }

    async fn channel(&self, login: &str, url: &Url) -> Result<Resolved, ResolveError> {
        let answer = self
            .gql(
                json!([
                    {
                        "operationName": "PlaybackAccessToken_Template",
                        "query": "query PlaybackAccessToken_Template($login: String!, $isLive: Boolean!, $vodID: ID!, $isVod: Boolean!, $playerType: String!) {  streamPlaybackAccessToken(channelName: $login, params: {platform: \"web\", playerBackend: \"mediaplayer\", playerType: $playerType}) @include(if: $isLive) {    value    signature    __typename  }  videoPlaybackAccessToken(id: $vodID, params: {platform: \"web\", playerBackend: \"mediaplayer\", playerType: $playerType}) @include(if: $isVod) {    value    signature    __typename  }}",
                        "variables": {"isLive": true, "login": login, "isVod": false, "vodID": "", "playerType": "site"}
                    },
                    {
                        "query": format!("query {{ user(login: \"{login}\") {{ id displayName login stream {{ id title type createdAt previewImageURL(height: 480, width: 854) game {{ displayName }} }} }} }}"),
                        "variables": {}
                    }
                ]),
                url,
            )
            .await?;
        let user = &answer[1]["data"]["user"];
        if user.is_null() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let stream = &user["stream"];
        if stream.is_null() {
            return Err(ResolveError::unavailable(
                url,
                format!("{login} is not live"),
            ));
        }
        let token = &answer[0]["data"]["streamPlaybackAccessToken"];
        let (value, signature) = match (token["value"].as_str(), token["signature"].as_str()) {
            (Some(value), Some(signature)) => (value, signature),
            _ => {
                return Err(ResolveError::unavailable(
                    url,
                    "no playback token was given",
                ));
            }
        };
        let expanded = self
            .playlists(&format!("api/channel/hls/{login}"), value, signature, url)
            .await?;
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = stream["id"].as_str().map(String::from);
        resolved.title = stream["title"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| clean_title(&format!("{login} live")));
        resolved.uploader = user["displayName"].as_str().and_then(clean_title);
        resolved.uploader_url = Url::parse(&format!("{SITE}{login}")).ok();
        resolved.uploaded_at = stream["createdAt"].as_str().and_then(|t| t.parse().ok());
        resolved.thumbnail = stream["previewImageURL"]
            .as_str()
            .and_then(|t| Url::parse(t).ok());
        resolved.webpage_url = Url::parse(&format!("{SITE}{login}")).ok();
        resolved.live = true;
        let mut variants = expanded.variants;
        for v in &mut variants {
            v.live = true;
        }
        resolved.variants = variants;
        Ok(resolved)
    }
}

#[async_trait]
impl Resolver for TwitchResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Twitch",
            hosts: &["twitch.tv", "clips.twitch.tv"],
            features: &[
                "recordings",
                "highlights",
                "clips",
                "live",
                "subscriber-only with a session",
            ],
            formats: &["hls", "mp4"],
            session: SessionSupport::Optional,
            examples: &[
                "https://www.twitch.tv/videos/40791111",
                "https://clips.twitch.tv/FaintLightGullWholeWheat",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let resolved = match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Video(id) => self.video(&id, url).await?,
            Link::Clip(slug) => self.clip(&slug, url).await?,
            Link::Channel(login) => self.channel(&login, url).await?,
        };
        Ok(Resolution::from(resolved))
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        if self.session_token().is_none() {
            return Ok(SessionCheck::LoggedOut);
        }
        let origin = Url::parse(SITE).expect("valid");
        let answer = self
            .gql(
                json!([{"query": "query { currentUser { login displayName } }", "variables": {}}]),
                &origin,
            )
            .await?;
        Ok(match answer[0]["data"]["currentUser"]["login"].as_str() {
            Some(login) => SessionCheck::LoggedIn {
                account: login.to_string(),
            },
            None => SessionCheck::LoggedOut,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

    fn exchange(method: &str, url: &str, content_type: &str, body: String) -> Exchange {
        exchange_with(method, url, 200, content_type, body)
    }

    fn exchange_with(
        method: &str,
        url: &str,
        status: u16,
        content_type: &str,
        body: String,
    ) -> Exchange {
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
                body: RecordedBody::Text(body),
                truncated: false,
            },
        }
    }

    const MASTER: &str = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=6000000,RESOLUTION=1920x1080,CODECS=\"avc1.64002A,mp4a.40.2\",FRAME-RATE=60.000\nhttps://d2vjef5jvl6bfs.cloudfront.net/x/chunked/index-dvr.m3u8\n";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n0.ts\n#EXTINF:5.0,\n1.ts\n#EXT-X-ENDLIST\n";

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://www.twitch.tv/videos/1234567"),
            Some(Link::Video("1234567".into()))
        );
        assert_eq!(
            link("https://clips.twitch.tv/AwkwardHelplessSalamanderSwiftRage"),
            Some(Link::Clip("AwkwardHelplessSalamanderSwiftRage".into()))
        );
        assert_eq!(
            link("https://www.twitch.tv/someone/clip/AwkwardHelplessSalamanderSwiftRage?x=1"),
            Some(Link::Clip("AwkwardHelplessSalamanderSwiftRage".into()))
        );
        assert_eq!(
            link("https://twitch.tv/SomeOne"),
            Some(Link::Channel("someone".into()))
        );
        assert_eq!(link("https://www.twitch.tv/directory/game/x"), None);
        assert_eq!(link("https://www.twitch.tv/someone/videos"), None);
    }

    #[tokio::test]
    async fn recordings_and_clips_and_live_channels_resolve() {
        let mut fixture = Fixture::new("twitch", None);
        fixture.exchanges.push(exchange("POST", GQL, "application/json", json!([
            {"data": {"videoPlaybackAccessToken": {"value": "{\"vod_id\":1}", "signature": "sig1"}}},
            {"data": {"video": {"id": "1", "title": "A long stream", "lengthSeconds": 3600, "createdAt": "2024-01-01T00:00:00Z", "previewThumbnailURL": "https://static-cdn.jtvnw.net/t.jpg", "owner": {"login": "someone", "displayName": "Someone"}}}}
        ]).to_string()));
        fixture.exchanges.push(exchange(
            "GET",
            "https://usher.ttvnw.net/vod/1.m3u8",
            "application/vnd.apple.mpegurl",
            MASTER.into(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://d2vjef5jvl6bfs.cloudfront.net/x/chunked/index-dvr.m3u8",
            "application/vnd.apple.mpegurl",
            MEDIA.into(),
        ));
        let resolver = TwitchResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://www.twitch.tv/videos/1?t=1h2m3s").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.title.as_deref(), Some("A long stream"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(3600)));
        assert_eq!(resolved.clip.unwrap().start, Duration::from_secs(3723));
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].kind, VariantKind::Hls);
        assert_eq!(resolved.variants[0].height, Some(1080));
        assert!(!resolved.live);

        let mut fixture = Fixture::new("twitch", None);
        fixture.exchanges.push(exchange("POST", GQL, "application/json", json!([
            {"data": {"clip": {"playbackAccessToken": {"value": "{\"clip_uri\":\"x\"}", "signature": "sig2"}, "videoQualities": [
                {"quality": "720", "frameRate": 60.0, "sourceURL": "https://production.assets.clips.twitchcdn.net/x-720.mp4"},
                {"quality": "480", "frameRate": 0, "sourceURL": "https://production.assets.clips.twitchcdn.net/x-480.mp4"}
            ]}}},
            {"data": {"clip": {"id": "99", "title": "Wow", "durationSeconds": 30.5, "createdAt": "2024-02-02T00:00:00Z", "thumbnailURL": "https://clips-media-assets2.twitch.tv/t.jpg", "broadcaster": {"login": "someone", "displayName": "Someone"}}}}
        ]).to_string()));
        let resolver = TwitchResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(
                &Url::parse("https://clips.twitch.tv/AwkwardHelplessSalamanderSwiftRage").unwrap(),
            )
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.title.as_deref(), Some("Wow"));
        assert_eq!(resolved.variants.len(), 2);
        assert_eq!(resolved.variants[0].height, Some(720));
        assert_eq!(resolved.variants[0].fps, Some(60.0));
        assert_eq!(resolved.variants[1].fps, None);
        assert!(
            resolved.variants[0].url.as_str().contains("sig=sig2"),
            "{}",
            resolved.variants[0].url
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(30.5)));

        let mut fixture = Fixture::new("twitch", None);
        fixture.exchanges.push(exchange("POST", GQL, "application/json", json!([
            {"data": {"streamPlaybackAccessToken": {"value": "{\"channel\":\"someone\"}", "signature": "sig3"}}},
            {"data": {"user": {"id": "5", "displayName": "Someone", "login": "someone", "stream": {"id": "77", "title": "Live now", "type": "live", "createdAt": "2024-03-03T00:00:00Z", "previewImageURL": "https://static-cdn.jtvnw.net/p.jpg"}}}}
        ]).to_string()));
        fixture.exchanges.push(exchange(
            "GET",
            "https://usher.ttvnw.net/api/channel/hls/someone.m3u8",
            "application/vnd.apple.mpegurl",
            MASTER.into(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://d2vjef5jvl6bfs.cloudfront.net/x/chunked/index-dvr.m3u8",
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2.0,\n0.ts\n".into(),
        ));
        let resolver = TwitchResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://www.twitch.tv/someone").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert!(resolved.live);
        assert_eq!(resolved.title.as_deref(), Some("Live now"));
        assert!(resolved.variants[0].live);

        let mut fixture = Fixture::new("twitch", None);
        fixture.exchanges.push(exchange("POST", GQL, "application/json", json!([
            {"data": {}},
            {"data": {"user": {"id": "5", "displayName": "Someone", "login": "someone", "stream": null}}}
        ]).to_string()));
        let resolver = TwitchResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://www.twitch.tv/someone").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("not live")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_rejected_client_id_is_replaced_by_the_sites_own() {
        let mut fixture = Fixture::new("twitch", None);
        fixture.exchanges.push(exchange_with("POST", GQL, 400, "application/json", json!({"error": "Bad Request", "status": 400, "message": "The \"Client-ID\" header is invalid."}).to_string()));
        fixture.exchanges.push(exchange("GET", SITE, "text/html", r#"<html><script>var a=1;clientId="kimne78kx3ncx6brgo4mv6wki5h1ko",commonOptions={}</script></html>"#.into()));
        fixture.exchanges.push(exchange("POST", GQL, "application/json", json!([
            {"data": {"clip": {"playbackAccessToken": {"value": "v", "signature": "s"}, "videoQualities": [{"quality": "1080", "frameRate": 0, "sourceURL": "https://d1ndex63qxojbr.cloudfront.net/x/1080/index.mp4"}]}}},
            {"data": {"clip": {"id": "396245304", "title": "EA Play 2016", "durationSeconds": 32, "createdAt": "2016-06-12T21:36:33Z", "broadcaster": {"login": "ea", "displayName": "EA"}}}}
        ]).to_string()));
        let resolver = TwitchResolver::new(Http::replay(fixture));
        *resolver.client_id.write().unwrap() = "stale".into();
        let resolved = resolver
            .resolve(&Url::parse("https://clips.twitch.tv/FaintLightGullWholeWheat").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.title.as_deref(), Some("EA Play 2016"));
        assert_eq!(resolver.client_id(), "kimne78kx3ncx6brgo4mv6wki5h1ko");
    }

    #[tokio::test]
    async fn subscriber_only_recordings_ask_for_a_session() {
        let mut fixture = Fixture::new("twitch", None);
        fixture.exchanges.push(exchange("POST", GQL, "application/json", json!([
            {"data": {"videoPlaybackAccessToken": {"value": "{\"vod_id\":2}", "signature": "sig"}}},
            {"data": {"video": {"id": "2", "title": "Subs only", "lengthSeconds": 60, "owner": {"login": "someone", "displayName": "Someone"}}}}
        ]).to_string()));
        fixture.exchanges.push(exchange_with("GET", "https://usher.ttvnw.net/vod/2.m3u8", 403, "application/json", json!([{"error": "Content Restricted In Region", "error_code": "vod_manifest_restricted", "type": "error"}]).to_string()));
        let resolver = TwitchResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://www.twitch.tv/videos/2").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(
                &error,
                ResolveError::LoginRequired {
                    platform: "twitch",
                    ..
                }
            ),
            "{error}"
        );
    }
}
