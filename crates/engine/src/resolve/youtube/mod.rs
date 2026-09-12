//! YouTube: videos, shorts, live streams, premieres, age-gated videos, playlists,
//! channels and clips, through the InnerTube API as YouTube's own apps use it, with the
//! player script's ciphers run in the JavaScript interpreter.

pub mod formats;
pub mod innertube;
pub mod player;
pub mod playlist;

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};
use url::Url;

pub use innertube::PLATFORM;
use innertube::{CLIENTS, Client, InnerTube, ORIGIN, TV_EMBEDDED, WEB};
use player::{Player, PlayerCache};

use super::page::json_after;
use super::{
    ClipRange, MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionCheck,
    SessionSupport, Variant, VariantKind, fetch_ok, hls, timestamp_hint,
};
use crate::http::{BROWSER_UA, Cookie, Http};

static RE_VIDEO_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]{11}$").unwrap());
static RE_LIST_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]{13,}$").unwrap());

/// What a link names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Video {
        id: String,
        start: Option<Duration>,
    },
    Playlist(String),
    /// A channel page, whose uploads are the playlist.
    Channel(Url),
    Clip(String),
}

fn is_youtube_host(host: &str) -> bool {
    matches!(
        host,
        "youtube.com"
            | "youtu.be"
            | "youtube-nocookie.com"
            | "www.youtube-nocookie.com"
            | "youtube.googleapis.com"
    ) || host.ends_with(".youtube.com")
}

/// What `url` names, when it is a YouTube link this resolver takes.
pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !is_youtube_host(&host) {
        return None;
    }
    let query = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let video = |id: &str| {
        RE_VIDEO_ID.is_match(id).then(|| Link::Video {
            id: id.to_string(),
            start: timestamp_hint(url),
        })
    };
    if host == "youtu.be" {
        return segments.first().and_then(|id| video(id));
    }
    match segments.as_slice() {
        ["watch"] => query("v").and_then(|id| video(&id)),
        ["playlist"] => query("list")
            .filter(|l| RE_LIST_ID.is_match(l))
            .map(Link::Playlist),
        ["embed", "videoseries"] => query("list")
            .filter(|l| RE_LIST_ID.is_match(l))
            .map(Link::Playlist),
        ["shorts", id] | ["embed", id] | ["v", id] | ["live", id] | ["e", id] => video(id),
        ["clip", id] => Some(Link::Clip(id.to_string())),
        [first, ..] if first.starts_with('@') => Some(Link::Channel(url.clone())),
        ["channel", _, ..] | ["c", _, ..] | ["user", _, ..] => Some(Link::Channel(url.clone())),
        _ => None,
    }
}

pub struct YoutubeResolver {
    http: Http,
    tube: InnerTube,
    player: PlayerCache,
}

impl YoutubeResolver {
    pub fn new(http: Http) -> Self {
        Self {
            tube: InnerTube::new(http.clone()),
            player: PlayerCache::new(http.clone()),
            http,
        }
    }

    async fn player_for(
        &self,
        watch: &Url,
        cached: &mut Option<Arc<Player>>,
    ) -> Result<Arc<Player>, ResolveError> {
        if let Some(player) = cached {
            return Ok(player.clone());
        }
        let player = self.player.get(watch).await?;
        *cached = Some(player.clone());
        Ok(player)
    }

    /// Asks the apps for the video in turn until one plays it; an age gate sends the
    /// embedded app, which gets past it for embeddable videos, and a logged-in session
    /// goes with the web app for the rest.
    async fn video(
        &self,
        id: &str,
        clip: Option<ClipRange>,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let watch = Url::parse(&format!("{ORIGIN}/watch?v={id}")).expect("valid");
        let mut player: Option<Arc<Player>> = None;
        let mut gated: Option<String> = None;
        let mut login: Option<String> = None;
        let mut refusals: Vec<String> = Vec::new();
        let mut queue: Vec<Client> = CLIENTS.to_vec();
        let mut index = 0;
        while index < queue.len() {
            let client = queue[index];
            index += 1;
            let sts = if client.needs_player {
                Some(self.player_for(&watch, &mut player).await?.sts)
            } else {
                None
            };
            let response = self.tube.player(&client, id, sts, &watch).await?;
            match formats::playability(&response) {
                formats::Playability::Ok => {
                    let answered = response
                        .pointer("/videoDetails/videoId")
                        .and_then(Value::as_str);
                    if answered.is_some_and(|a| a != id) {
                        refusals.push(format!("{}: answered for another video", client.id));
                        continue;
                    }
                    match self
                        .build(&response, &client, player.as_deref(), clip, gated.is_some())
                        .await
                    {
                        Ok(resolved) => return Ok(resolved.into()),
                        Err(problems) => refusals.push(format!("{}: {problems}", client.id)),
                    }
                }
                formats::Playability::AgeGated(reason) => {
                    if gated.is_none() && !queue.contains(&TV_EMBEDDED) {
                        queue.push(TV_EMBEDDED);
                    }
                    gated = Some(reason);
                }
                formats::Playability::LoginRequired(reason) => login = Some(reason),
                formats::Playability::Upcoming(at) => {
                    return Err(ResolveError::unavailable(
                        url,
                        match at {
                            Some(at) => format!("premieres at {at}"),
                            None => "has not started yet".to_string(),
                        },
                    ));
                }
                formats::Playability::Unavailable(reason) => {
                    refusals.push(format!("{}: {reason}", client.id));
                }
            }
        }
        if let Some(reason) = gated {
            return Err(ResolveError::login_required(
                url,
                PLATFORM,
                format!("age-restricted: {reason}"),
            ));
        }
        if let Some(reason) = login {
            return Err(ResolveError::login_required(url, PLATFORM, reason));
        }
        Err(ResolveError::unavailable(url, refusals.join("; ")))
    }

    /// What one app's answer resolves to, or why it resolves to nothing.
    async fn build(
        &self,
        response: &Value,
        client: &Client,
        player: Option<&Player>,
        clip: Option<ClipRange>,
        gated: bool,
    ) -> Result<Resolved, String> {
        let mut resolved = Resolved::new(PLATFORM);
        formats::details(response, &mut resolved);
        let (mut variants, mut problems) = formats::variants(response, client, player).await;
        let streaming = &response["streamingData"];
        if let Some(manifest) = streaming["hlsManifestUrl"]
            .as_str()
            .and_then(|u| Url::parse(u).ok())
            && (resolved.live || variants.is_empty())
        {
            let headers = [("user-agent".to_string(), client.user_agent.to_string())];
            match hls::expand(&self.http, &manifest, PLATFORM, client.user_agent, &headers).await {
                Ok(expanded) => {
                    resolved.live |= expanded.live;
                    if resolved.duration.is_none() {
                        resolved.duration = expanded.duration;
                    }
                    variants.extend(expanded.variants);
                    resolved.subtitles.extend(expanded.subtitles);
                }
                Err(error) => problems.push(format!("HLS manifest: {error}")),
            }
        }
        if resolved.live
            && let Some(manifest) = streaming["dashManifestUrl"]
                .as_str()
                .and_then(|u| Url::parse(u).ok())
        {
            let mut variant = Variant::new(manifest, VariantKind::Dash);
            variant.live = true;
            variant.headers = vec![("user-agent".to_string(), client.user_agent.to_string())];
            variants.push(variant);
        }
        if variants.is_empty() {
            return Err(if problems.is_empty() {
                "no playable format".to_string()
            } else {
                problems.join(", ")
            });
        }
        for problem in problems {
            tracing::debug!(client = client.id, "{problem}");
        }
        resolved.subtitles.extend(formats::captions(response));
        resolved.variants = variants;
        resolved.clip = clip;
        if gated {
            resolved.age_limit = Some(18);
        }
        Ok(resolved)
    }

    /// A clip: its page names the video and the portion of it.
    async fn clip(&self, clip_id: &str, url: &Url) -> Result<Resolution, ResolveError> {
        let page_url = Url::parse(&format!("{ORIGIN}/clip/{clip_id}")).expect("valid");
        let fetched = fetch_ok(&self.http, &page_url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        let html = fetched.text();
        let response = json_after(&html, "ytInitialPlayerResponse = ")
            .or_else(|| json_after(&html, "ytInitialPlayerResponse="))
            .ok_or_else(|| ResolveError::malformed(url, "the clip page has no player response"))?;
        let video_id = response
            .pointer("/videoDetails/videoId")
            .and_then(Value::as_str)
            .ok_or_else(|| ResolveError::malformed(url, "the clip page names no video"))?
            .to_string();
        let millis = |key: &str| {
            let value = &response["clipConfig"][key];
            value
                .as_str()
                .and_then(|s| s.parse::<u64>().ok())
                .or_else(|| value.as_u64())
        };
        let (start, end) = match (millis("startTimeMs"), millis("endTimeMs")) {
            (Some(start), Some(end)) if end > start => (start, end),
            _ => {
                return Err(ResolveError::malformed(
                    url,
                    "the clip page has no clip range",
                ));
            }
        };
        self.video(
            &video_id,
            Some(ClipRange {
                start: Duration::from_millis(start),
                end: Some(Duration::from_millis(end)),
            }),
            url,
        )
        .await
    }
}

#[async_trait]
impl Resolver for YoutubeResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "YouTube",
            hosts: &["youtube.com", "youtu.be", "youtube-nocookie.com"],
            features: &[
                "videos",
                "shorts",
                "live",
                "premieres",
                "age-gated",
                "playlists",
                "channels",
                "clips",
                "subtitles",
            ],
            formats: &["mp4", "webm", "hls", "dash"],
            session: SessionSupport::Optional,
            examples: &[
                "https://www.youtube.com/watch?v=jNQXAC9IVRw",
                "https://youtu.be/dQw4w9WgXcQ",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    /// The consent choice the EU cookie prompt records, and English names for everything.
    fn consent_cookies(&self) -> Vec<Cookie> {
        vec![
            Cookie::new("SOCS", "CAI", "youtube.com"),
            Cookie::new("PREF", "hl=en&tz=UTC", "youtube.com"),
        ]
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Video { id, start } => {
                self.video(&id, start.map(|start| ClipRange { start, end: None }), url)
                    .await
            }
            Link::Playlist(list) => Ok(Resolution::Playlist(
                playlist::resolve_playlist(&self.tube, &list, url).await?,
            )),
            Link::Channel(page) => {
                let list = playlist::channel_playlist(&self.tube, &page).await?;
                Ok(Resolution::Playlist(
                    playlist::resolve_playlist(&self.tube, &list, url).await?,
                ))
            }
            Link::Clip(id) => self.clip(&id, url).await,
        }
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        if !self.tube.logged_in() {
            return Ok(SessionCheck::LoggedOut);
        }
        let origin = Url::parse(ORIGIN).expect("valid");
        let answer = self
            .tube
            .call("account/account_menu", &WEB, json!({}), &origin)
            .await?;
        let mut found = Vec::new();
        playlist::walk(&answer, "activeAccountHeaderRenderer", &mut found);
        Ok(
            match found
                .into_iter()
                .find_map(|header| header["accountName"]["simpleText"].as_str())
            {
                Some(account) => SessionCheck::LoggedIn {
                    account: account.to_string(),
                },
                None => SessionCheck::LoggedOut,
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
    use crate::resolve::youtube::player::tests::SCRIPT;

    fn link(s: &str) -> Option<Link> {
        parse_link(&Url::parse(s).unwrap())
    }

    #[test]
    fn links_of_every_shape_are_read() {
        let video = |id: &str| Link::Video {
            id: id.into(),
            start: None,
        };
        assert_eq!(
            link("https://www.youtube.com/watch?v=jNQXAC9IVRw"),
            Some(video("jNQXAC9IVRw"))
        );
        assert_eq!(
            link("https://youtu.be/jNQXAC9IVRw?si=abc"),
            Some(video("jNQXAC9IVRw"))
        );
        assert_eq!(
            link("https://www.youtube.com/shorts/jNQXAC9IVRw"),
            Some(video("jNQXAC9IVRw"))
        );
        assert_eq!(
            link("https://m.youtube.com/watch?v=jNQXAC9IVRw&list=PLx"),
            Some(video("jNQXAC9IVRw"))
        );
        assert_eq!(
            link("https://www.youtube-nocookie.com/embed/jNQXAC9IVRw"),
            Some(video("jNQXAC9IVRw"))
        );
        assert_eq!(
            link("https://www.youtube.com/live/jNQXAC9IVRw"),
            Some(video("jNQXAC9IVRw"))
        );
        assert_eq!(
            link("https://www.youtube.com/watch?v=jNQXAC9IVRw&t=1m5s"),
            Some(Link::Video {
                id: "jNQXAC9IVRw".into(),
                start: Some(Duration::from_secs(65))
            })
        );
        assert_eq!(
            link("https://www.youtube.com/playlist?list=PLbpi6ZahtOH6Bl0t4sniIz1n2fPJ4sZG1"),
            Some(Link::Playlist("PLbpi6ZahtOH6Bl0t4sniIz1n2fPJ4sZG1".into()))
        );
        assert_eq!(
            link("https://www.youtube.com/clip/UgkxABCDEF"),
            Some(Link::Clip("UgkxABCDEF".into()))
        );
        assert!(matches!(
            link("https://www.youtube.com/@jawed/videos"),
            Some(Link::Channel(_))
        ));
        assert!(matches!(
            link("https://www.youtube.com/channel/UC4QobU6STFB0P71PMvOGN5A"),
            Some(Link::Channel(_))
        ));
        assert_eq!(link("https://www.youtube.com/watch?v=short"), None);
        assert_eq!(link("https://www.youtube.com/feed/subscriptions"), None);
        assert_eq!(link("https://vimeo.com/123"), None);
    }

    fn get(url: &str, content_type: &str, body: &str) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: "GET".into(),
                url: url.into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status: 200,
                url: url.into(),
                headers: vec![("content-type".into(), content_type.into())],
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    fn post(url: &str, body: Value) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: "POST".into(),
                url: url.into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status: 200,
                url: url.into(),
                headers: vec![("content-type".into(), "application/json".into())],
                body: RecordedBody::Text(body.to_string()),
                truncated: false,
            },
        }
    }

    const PLAYER: &str = "https://www.youtube.com/youtubei/v1/player?prettyPrint=false";
    const BROWSE: &str = "https://www.youtube.com/youtubei/v1/browse?prettyPrint=false";

    fn player_script_exchanges() -> Vec<Exchange> {
        vec![
            get(
                "https://www.youtube.com/iframe_api",
                "text/javascript",
                r#"var scriptUrl = 'https:\/\/www.youtube.com\/s\/player\/abcdef12\/www-widgetapi.vflset\/www-widgetapi.js';"#,
            ),
            get(
                "https://www.youtube.com/s/player/abcdef12/player_ias.vflset/en_US/base.js",
                "text/javascript",
                SCRIPT,
            ),
        ]
    }

    fn playable(id: &str) -> Value {
        json!({
            "playabilityStatus": {"status": "OK"},
            "videoDetails": {
                "videoId": id, "title": "Me at the zoo", "author": "jawed", "lengthSeconds": "19",
                "channelId": "UC4QobU6STFB0P71PMvOGN5A", "isLive": false,
                "thumbnail": {"thumbnails": [{"url": "https://i.ytimg.com/vi/x/hq.jpg", "width": 480}]}
            },
            "streamingData": {
                "formats": [{
                    "itag": 18, "url": "https://rr1.googlevideo.com/videoplayback?id=1&n=xyz",
                    "mimeType": "video/mp4; codecs=\"avc1.42001E, mp4a.40.2\"", "width": 640, "height": 360,
                    "bitrate": 500000, "contentLength": "1000", "approxDurationMs": "19000", "qualityLabel": "360p"
                }],
                "adaptiveFormats": [
                    {
                        "itag": 137, "signatureCipher": "s=abcdefgh&sp=sig&url=https%3A%2F%2Frr1.googlevideo.com%2Fvideoplayback%3Fid%3D2%26n%3Dxyz",
                        "mimeType": "video/mp4; codecs=\"avc1.640028\"", "width": 1920, "height": 1080, "fps": 30,
                        "bitrate": 4000000, "contentLength": "5000", "qualityLabel": "1080p"
                    },
                    {
                        "itag": 140, "url": "https://rr1.googlevideo.com/videoplayback?id=3&n=xyz",
                        "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"", "bitrate": 128000, "contentLength": "300",
                        "audioQuality": "AUDIO_QUALITY_MEDIUM", "audioTrack": {"id": "en.4", "displayName": "English"}
                    },
                    {"itag": 999, "type": "FORMAT_STREAM_TYPE_OTF", "url": "https://rr1.googlevideo.com/otf", "mimeType": "video/mp4"}
                ]
            },
            "captions": {"playerCaptionsTracklistRenderer": {"captionTracks": [
                {"baseUrl": "https://www.youtube.com/api/timedtext?v=x&lang=en", "languageCode": "en", "name": {"simpleText": "English"}}
            ]}}
        })
    }

    fn gated(id: &str) -> Value {
        json!({
            "playabilityStatus": {"status": "LOGIN_REQUIRED", "reason": "Sign in to confirm your age"},
            "videoDetails": {"videoId": id}
        })
    }

    #[tokio::test]
    async fn a_video_resolves_with_its_formats_unlocked() {
        let mut fixture = Fixture::new("youtube", None);
        fixture.exchanges.extend(player_script_exchanges());
        fixture
            .exchanges
            .push(post(PLAYER, playable("jNQXAC9IVRw")));
        let resolver = YoutubeResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://www.youtube.com/watch?v=jNQXAC9IVRw&t=5").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.title.as_deref(), Some("Me at the zoo"));
        assert_eq!(resolved.id.as_deref(), Some("jNQXAC9IVRw"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(19)));
        assert_eq!(resolved.clip.unwrap().start, Duration::from_secs(5));
        assert_eq!(resolved.subtitles.len(), 1);
        assert_eq!(resolved.variants.len(), 3);
        let full = &resolved.variants[0];
        assert_eq!(full.format_id.as_deref(), Some("18"));
        assert!(!full.video_only && !full.audio_only);
        assert!(full.url.as_str().contains("n=zyx_w8_split"), "{}", full.url);
        let video = &resolved.variants[1];
        assert!(video.video_only);
        assert_eq!(video.height, Some(1080));
        assert_eq!(video.size, Some(5000));
        assert!(video.url.as_str().contains("sig=cedfba"), "{}", video.url);
        assert!(
            video.url.as_str().contains("n=zyx_w8_split"),
            "{}",
            video.url
        );
        assert_eq!(video.headers[0].0, "user-agent");
        let audio = &resolved.variants[2];
        assert!(audio.audio_only);
        assert_eq!(audio.language.as_deref(), Some("en"));
        assert_eq!(audio.audio, Some(crate::media::AudioCodec::Aac));
    }

    #[tokio::test]
    async fn an_age_gate_is_passed_by_the_embedded_app() {
        let mut fixture = Fixture::new("youtube", None);
        fixture.exchanges.extend(player_script_exchanges());
        for _ in 0..3 {
            fixture.exchanges.push(post(PLAYER, gated("jNQXAC9IVRw")));
        }
        fixture
            .exchanges
            .push(post(PLAYER, playable("jNQXAC9IVRw")));
        let resolver = YoutubeResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://youtu.be/jNQXAC9IVRw").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.age_limit, Some(18));
        assert_eq!(resolved.variants.len(), 3);

        // With no app getting past it, the video needs a logged-in session.
        let mut fixture = Fixture::new("youtube", None);
        fixture.exchanges.extend(player_script_exchanges());
        for _ in 0..4 {
            fixture.exchanges.push(post(PLAYER, gated("jNQXAC9IVRw")));
        }
        let resolver = YoutubeResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://youtu.be/jNQXAC9IVRw").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::LoginRequired { platform: "youtube", reason, .. } if reason.contains("age")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn premieres_and_unavailable_videos_say_so() {
        let mut fixture = Fixture::new("youtube", None);
        fixture.exchanges.extend(player_script_exchanges());
        fixture.exchanges.push(post(
            PLAYER,
            json!({"playabilityStatus": {"status": "LIVE_STREAM_OFFLINE", "liveStreamability": {"liveStreamabilityRenderer": {"offlineSlate": {"liveStreamOfflineSlateRenderer": {"scheduledStartTime": "1700000000"}}}}}}),
        ));
        let resolver = YoutubeResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://www.youtube.com/watch?v=jNQXAC9IVRw").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("premieres at 2023-11-14")),
            "{error}"
        );

        let mut fixture = Fixture::new("youtube", None);
        fixture.exchanges.extend(player_script_exchanges());
        for _ in 0..3 {
            fixture.exchanges.push(post(
                PLAYER,
                json!({"playabilityStatus": {"status": "ERROR", "reason": "Video unavailable"}}),
            ));
        }
        let resolver = YoutubeResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://www.youtube.com/watch?v=jNQXAC9IVRw").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("tv: Video unavailable")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn live_streams_come_as_hls() {
        let mut fixture = Fixture::new("youtube", None);
        fixture.exchanges.extend(player_script_exchanges());
        let mut live = playable("jNQXAC9IVRw");
        live["videoDetails"]["isLive"] = json!(true);
        live["streamingData"] = json!({"hlsManifestUrl": "https://manifest.googlevideo.com/api/manifest/hls_variant/x/master.m3u8"});
        fixture.exchanges.push(post(PLAYER, live));
        fixture.exchanges.push(get(
            "https://manifest.googlevideo.com/api/manifest/hls_variant/x/master.m3u8",
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720,CODECS=\"avc1.4d401f,mp4a.40.2\"\nhttps://manifest.googlevideo.com/api/manifest/hls_playlist/x/720.m3u8\n",
        ));
        fixture.exchanges.push(get(
            "https://manifest.googlevideo.com/api/manifest/hls_playlist/x/720.m3u8",
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:5\n#EXTINF:5.0,\nseg1.ts\n#EXTINF:5.0,\nseg2.ts\n",
        ));
        let resolver = YoutubeResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://www.youtube.com/live/jNQXAC9IVRw").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert!(resolved.live);
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].kind, VariantKind::Hls);
        assert!(resolved.variants[0].live);
        assert_eq!(resolved.variants[0].height, Some(720));
    }

    #[tokio::test]
    async fn playlists_and_channels_list_their_videos_page_by_page() {
        let mut fixture = Fixture::new("youtube", None);
        fixture.exchanges.push(post(
            "https://www.youtube.com/youtubei/v1/navigation/resolve_url?prettyPrint=false",
            json!({"endpoint": {"browseEndpoint": {"browseId": "UC4QobU6STFB0P71PMvOGN5A"}}}),
        ));
        fixture.exchanges.push(post(
            BROWSE,
            json!({
                "header": {"playlistHeaderRenderer": {"title": {"simpleText": "Uploads"}, "numVideosText": {"runs": [{"text": "3 videos"}]}}},
                "contents": [
                    {"playlistVideoRenderer": {"videoId": "aaaaaaaaaaa", "title": {"runs": [{"text": "One"}]}, "lengthSeconds": "10"}},
                    {"playlistVideoRenderer": {"videoId": "bbbbbbbbbbb", "title": {"runs": [{"text": "Two"}]}, "lengthSeconds": "20"}},
                    {"continuationItemRenderer": {"continuationEndpoint": {"continuationCommand": {"token": "PAGE2"}}}}
                ]
            }),
        ));
        fixture.exchanges.push(post(
            BROWSE,
            json!({"onResponseReceivedActions": [{"appendContinuationItemsAction": {"continuationItems": [
                {"playlistVideoRenderer": {"videoId": "ccccccccccc", "title": {"runs": [{"text": "Three"}]}, "lengthSeconds": "30"}}
            ]}}]}),
        ));
        let resolver = YoutubeResolver::new(Http::replay(fixture));
        let playlist = match resolver
            .resolve(&Url::parse("https://www.youtube.com/@jawed").unwrap())
            .await
            .unwrap()
        {
            Resolution::Playlist(playlist) => playlist,
            Resolution::Media(_) => panic!("a channel is a playlist"),
        };
        assert_eq!(playlist.id.as_deref(), Some("UU4QobU6STFB0P71PMvOGN5A"));
        assert_eq!(playlist.title.as_deref(), Some("Uploads"));
        assert_eq!(playlist.entries.len(), 3);
        assert_eq!(
            playlist.total, None,
            "the header's count is not more than what was read"
        );
        assert_eq!(
            playlist.entries[2].url.as_str(),
            "https://www.youtube.com/watch?v=ccccccccccc"
        );
        assert_eq!(playlist.entries[2].duration, Some(Duration::from_secs(30)));
    }

    #[tokio::test]
    async fn clips_name_their_video_and_range() {
        let mut fixture = Fixture::new("youtube", None);
        fixture.exchanges.push(get(
            "https://www.youtube.com/clip/UgkxABCDEF",
            "text/html",
            r#"<html><script>var ytInitialPlayerResponse = {"videoDetails":{"videoId":"jNQXAC9IVRw"},"clipConfig":{"postId":"UgkxABCDEF","startTimeMs":"5000","endTimeMs":"12000"}};</script></html>"#,
        ));
        fixture.exchanges.extend(player_script_exchanges());
        fixture
            .exchanges
            .push(post(PLAYER, playable("jNQXAC9IVRw")));
        let resolver = YoutubeResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://www.youtube.com/clip/UgkxABCDEF").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(
            resolved.clip,
            Some(ClipRange {
                start: Duration::from_secs(5),
                end: Some(Duration::from_secs(12))
            })
        );
        assert_eq!(resolved.id.as_deref(), Some("jNQXAC9IVRw"));
    }

    #[tokio::test]
    async fn sessions_are_checked_through_the_account_menu() {
        let mut fixture = Fixture::new("youtube", None);
        fixture.exchanges.push(post(
            "https://www.youtube.com/youtubei/v1/account/account_menu?prettyPrint=false",
            json!({"actions": [{"openPopupAction": {"popup": {"multiPageMenuRenderer": {"header": {"activeAccountHeaderRenderer": {"accountName": {"simpleText": "Nick"}}}}}}}]}),
        ));
        let http = Http::replay(fixture);
        let resolver = YoutubeResolver::new(http.clone());
        assert_eq!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedOut
        );
        http.with_jar(PLATFORM, |jar| {
            jar.insert(Cookie::new("SAPISID", "abc", "youtube.com"))
        });
        assert_eq!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedIn {
                account: "Nick".into()
            }
        );
        assert_eq!(resolver.consent_cookies()[0].name, "SOCS");
    }
}
