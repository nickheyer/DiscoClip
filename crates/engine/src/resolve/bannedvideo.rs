//! Banned.video videos and live streams, through the InfoWars GraphQL API the site's
//! player calls.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Variant,
    check_status, clean_title, hls, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "bannedvideo";
const API: &str = "https://api.infowarsmedia.com/graphql";

/// The query the player sends for a video and its comments.
const QUERY: &str = "
query GetVideoAndComments($id: String!) {
    getVideo(id: $id) {
        streamUrl
        directUrl
        unlisted
        live
        tags {
            name
        }
        title
        summary
        playCount
        largeImage
        videoDuration
        channel {
            _id
            title
        }
        createdAt
    }
    getVideoComments(id: $id, limit: 999999, offset: 0) {
        _id
        content
        user {
            _id
            username
        }
        voteCount {
            positive
        }
        createdAt
        replyCount
    }
}";

/// The 24-character id of `/watch?id=…`, which must open the query.
pub fn video_id(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "banned.video" && host != "www.banned.video" {
        return None;
    }
    if url.path() != "/watch" {
        return None;
    }
    let rest = url.query()?.strip_prefix("id=")?;
    let id: String = rest.chars().take(24).collect();
    if id.chars().count() != 24 || !id.chars().all(|c| ('0'..='f').contains(&c)) {
        return None;
    }
    Some(id)
}

pub struct BannedVideoResolver {
    http: Http,
}

impl BannedVideoResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// The `getVideo` object of the API's answer for `id`.
    async fn video(&self, id: &str, origin: &Url) -> Result<Value, ResolveError> {
        let api = Url::parse(API).expect("valid");
        let response = self
            .http
            .post(api.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .json(&serde_json::json!({
                "variables": {"id": id},
                "operationName": "GetVideoAndComments",
                "query": QUERY,
            }))
            .header("content-type", "application/json; charset=utf-8")
            .send()
            .await?;
        check_status(&response, origin)?;
        let answer: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, format!("API JSON: {e}")))?;
        if let Some(message) = answer["errors"][0]["message"].as_str() {
            return Err(ResolveError::unavailable(origin, message));
        }
        let video = answer["data"]["getVideo"].clone();
        if !video.is_object() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        Ok(video)
    }
}

#[async_trait]
impl Resolver for BannedVideoResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Banned.video",
            hosts: &["banned.video"],
            features: &["videos", "live"],
            formats: &["mp4", "hls"],
            session: SessionSupport::None,
            examples: &["https://banned.video/watch?id=5e7a859644e02200c6ef5f11"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        video_id(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let id = video_id(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let video = self.video(&id, url).await?;
        let live = video["live"].as_bool().unwrap_or(false);
        let mut variants = Vec::new();
        if let Some(direct) = util::url_of(&video["directUrl"], None) {
            let mut variant = Variant::file(direct);
            variant.container = Some(Container::Mp4);
            variant.video = Some(VideoCodec::H264);
            variant.audio = Some(AudioCodec::Aac);
            variant.format_id = Some("direct".to_string());
            variant.live = live;
            variants.push(variant);
        }
        let mut expanded_duration = None;
        if let Some(stream) = util::url_of(&video["streamUrl"], None) {
            let expanded = hls::expand(&self.http, &stream, PLATFORM, BROWSER_UA, &[]).await?;
            expanded_duration = expanded.duration;
            for mut variant in expanded.variants {
                variant.live |= live;
                variants.push(variant);
            }
        }
        if variants.is_empty() {
            return Err(ResolveError::unavailable(
                url,
                "the video has neither a direct file nor a stream",
            ));
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.clone());
        // The API's titles end in one extra character past the title text.
        resolved.title = video["title"].as_str().and_then(|title| {
            let mut chars = title.chars();
            chars.next_back();
            clean_title(chars.as_str())
        });
        resolved.description = video["summary"].as_str().and_then(clean_title);
        resolved.uploader = video["channel"]["title"].as_str().and_then(clean_title);
        resolved.uploaded_at = util::time(&video["createdAt"]);
        // A live playlist's length is only its window, not the stream's duration.
        resolved.duration = util::float(&video["videoDuration"])
            .filter(|d| *d > 0.0)
            .map(Duration::from_secs_f64)
            .or(expanded_duration.filter(|_| !live));
        resolved.thumbnail = util::url_of(&video["largeImage"], None);
        resolved.webpage_url = Url::parse(&format!("https://banned.video/watch?id={id}")).ok();
        resolved.live = live;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};
    use serde_json::json;

    fn exchange(
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

    const ID: &str = "5e7a859644e02200c6ef5f11";

    #[test]
    fn links_are_read() {
        let id = |s: &str| video_id(&Url::parse(s).unwrap());
        assert_eq!(
            id("https://banned.video/watch?id=5e7a859644e02200c6ef5f11"),
            Some(ID.into())
        );
        assert_eq!(
            id("http://www.banned.video/watch?id=5e7a859644e02200c6ef5f11&t=10"),
            Some(ID.into())
        );
        assert_eq!(
            id("https://banned.video/watch?v=5e7a859644e02200c6ef5f11"),
            None
        );
        assert_eq!(
            id("https://banned.video/watch?id=5e7a859644e02200c6ef5f"),
            None
        );
        assert_eq!(
            id("https://banned.video/watch?id=5e7a859644e02200c6ef5fzz"),
            None
        );
        assert_eq!(id("https://banned.video/channel/x"), None);
        assert_eq!(
            id("https://example.com/watch?id=5e7a859644e02200c6ef5f11"),
            None
        );
    }

    #[tokio::test]
    async fn videos_resolve_with_their_file_and_stream() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange("POST", API, 200, "application/json", json!({"data": {
            "getVideo": {
                "streamUrl": "https://stream.banned.video/x/master.m3u8",
                "directUrl": "https://assets.infowarsmedia.com/videos/x.mp4",
                "unlisted": false, "live": false, "tags": [{"name": "news"}],
                "title": "China Discovers Origin of Corona Virus: Issues Emergency Statement ",
                "summary": "The statement.", "playCount": 12, "largeImage": "https://assets.infowarsmedia.com/images/x.jpg",
                "videoDuration": 1290.5, "channel": {"_id": "c1", "title": "Banned.video"}, "createdAt": "2020-03-24T22:11:35.000Z"
            },
            "getVideoComments": []
        }}).to_string()));
        fixture.exchanges.push(exchange(
            "GET",
            "https://stream.banned.video/x/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720\nhttps://stream.banned.video/x/720p/playlist.m3u8\n".into(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://stream.banned.video/x/720p/playlist.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\n0.ts\n#EXT-X-ENDLIST\n".into(),
        ));
        let resolver = BannedVideoResolver::new(Http::replay(fixture));
        let url = Url::parse(&format!("https://banned.video/watch?id={ID}")).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some(ID));
        assert_eq!(
            resolved.title.as_deref(),
            Some("China Discovers Origin of Corona Virus: Issues Emergency Statement")
        );
        assert_eq!(resolved.description.as_deref(), Some("The statement."));
        assert_eq!(resolved.uploader.as_deref(), Some("Banned.video"));
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(1290.5)));
        assert_eq!(resolved.uploaded_at.unwrap().as_second(), 1585087895);
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://assets.infowarsmedia.com/images/x.jpg"
        );
        assert!(!resolved.live);
        assert_eq!(resolved.variants.len(), 2);
        assert_eq!(resolved.variants[0].format_id.as_deref(), Some("direct"));
        assert_eq!(
            resolved.variants[0].url.as_str(),
            "https://assets.infowarsmedia.com/videos/x.mp4"
        );
        assert_eq!(resolved.variants[0].container, Some(Container::Mp4));
        assert_eq!(resolved.variants[1].height, Some(720));
    }

    #[tokio::test]
    async fn live_streams_are_marked_live() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange("POST", API, 200, "application/json", json!({"data": {
            "getVideo": {
                "streamUrl": "https://stream.banned.video/live/master.m3u8", "directUrl": null, "live": true,
                "tags": [], "title": "Live show\n", "summary": null, "largeImage": null, "videoDuration": 0,
                "channel": {"_id": "c1", "title": "Alex"}, "createdAt": "2020-03-24T22:11:35.000Z"
            },
            "getVideoComments": []
        }}).to_string()));
        fixture.exchanges.push(exchange(
            "GET",
            "https://stream.banned.video/live/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\n0.ts\n".into(),
        ));
        let resolver = BannedVideoResolver::new(Http::replay(fixture));
        let url = Url::parse(&format!("https://banned.video/watch?id={ID}")).unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert!(resolved.live);
        assert_eq!(resolved.title.as_deref(), Some("Live show"));
        assert_eq!(resolved.variants.len(), 1);
        assert!(resolved.variants[0].live);
        assert_eq!(resolved.duration, None);
    }

    #[tokio::test]
    async fn missing_videos_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "POST",
            API,
            200,
            "application/json",
            json!({"data": {"getVideo": null, "getVideoComments": null}}).to_string(),
        ));
        fixture.exchanges.push(exchange(
            "POST",
            API,
            200,
            "application/json",
            json!({"errors": [{"message": "Video not available in your region"}], "data": null})
                .to_string(),
        ));
        fixture.exchanges.push(exchange(
            "POST",
            API,
            200,
            "application/json",
            json!({"data": {"getVideo": {"title": "Nothing ", "directUrl": null, "streamUrl": null}}}).to_string(),
        ));
        let resolver = BannedVideoResolver::new(Http::replay(fixture));
        let url = Url::parse(&format!("https://banned.video/watch?id={ID}")).unwrap();
        assert!(matches!(
            resolver.resolve(&url).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver.resolve(&url).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("region")),
            "{error}"
        );
        let error = resolver.resolve(&url).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("neither")),
            "{error}"
        );
    }
}
