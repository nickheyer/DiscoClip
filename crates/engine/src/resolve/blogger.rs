//! Blogger video embeds, through the `WcwnYd` call the player page's app makes for a
//! token's streams: `videoplayback` files like YouTube's, one per itag, with the player
//! response that describes them.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Tag,
    Variant, clean_title, page, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "blogger";

/// The player app's call that answers a token with the video's streams.
const RPC_ID: &str = "WcwnYd";
/// Where the player app makes that call.
const RPC_URL: &str = "https://www.blogger.com/_/BloggerVideoPlayerUi/data/batchexecute?rpcids=WcwnYd&source-path=%2Fvideo.g&hl=en-US&rt=c";
/// The error code the app answers a token it does not know with.
const UNKNOWN_TOKEN: i64 = 3;

/// Player frames other pages embed.
static RE_EMBED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<iframe[^>]+src=["']((?:https?:)?//(?:www\.)?blogger\.com/video\.g\?token=[^"']+)["']"#)
        .unwrap()
});

/// The token a `blogger.com/video.g?token=…` link carries.
pub fn video_token(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "blogger.com" && host != "www.blogger.com" {
        return None;
    }
    if url.path() != "/video.g" {
        return None;
    }
    url.query_pairs()
        .find(|(name, _)| name == "token")
        .map(|(_, value)| value.into_owned())
        .filter(|token| !token.is_empty())
}

/// The `f.req` form field of a `batchexecute` request: one call of `rpc` with
/// `arguments`, JSON in a string as the app writes it.
pub fn rpc_request(rpc: &str, arguments: &Value) -> String {
    json!([[[rpc, arguments.to_string(), Value::Null, "generic"]]]).to_string()
}

/// What a `batchexecute` answer says for one call.
#[derive(Debug, Clone, PartialEq)]
pub enum RpcAnswer {
    /// The call's result.
    Payload(Value),
    /// The app refused the call: no result, with the error code it gave.
    Refused(Value),
}

/// Reads the `wrb.fr` entry for `rpc` out of a `batchexecute` answer: the anti-hijacking
/// prefix is dropped, then the chunks follow, each a byte count on its own line and a
/// JSON array of entries; the entry's result is JSON in a string. `None` when no chunk
/// carries the call's entry, or a chunk is not JSON.
pub fn rpc_answer(text: &str, rpc: &str) -> Option<RpcAnswer> {
    let mut rest = text.strip_prefix(")]}'").unwrap_or(text);
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return None;
        }
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits > 0 && rest[digits..].starts_with(['\n', '\r']) {
            rest = &rest[digits..];
            continue;
        }
        let (chunk, end) = page::leading_json(rest)?;
        rest = &rest[end..];
        for entry in chunk.as_array().into_iter().flatten() {
            if entry[0].as_str() != Some("wrb.fr") || entry[1].as_str() != Some(rpc) {
                continue;
            }
            return Some(match entry[2].as_str() {
                Some(payload) => RpcAnswer::Payload(serde_json::from_str(payload).ok()?),
                None => RpcAnswer::Refused(entry[5].clone()),
            });
        }
    }
}

/// The `codecs="…"` parameter of a MIME type.
fn codecs_of(mime: &str) -> Option<String> {
    mime.split(';')
        .skip(1)
        .map(str::trim)
        .find_map(|parameter| parameter.strip_prefix("codecs="))
        .map(|codecs| codecs.trim_matches('"').trim().to_string())
        .filter(|codecs| !codecs.is_empty())
}

/// `76.068` or `1:16` seconds.
fn parse_seconds(text: &str) -> Option<Duration> {
    text.trim()
        .parse::<f64>()
        .ok()
        .filter(|s| *s >= 0.0 && s.is_finite())
        .map(Duration::from_secs_f64)
        .or_else(|| util::parse_duration(text))
}

/// One variant per stream the payload lists: every format of the player response's
/// `streamingData` (muxed `formats`, then video-only and audio-only `adaptiveFormats`)
/// with its size, codecs, bitrate and length, then every stream of the plain list whose
/// itag those did not name, with what its link says.
pub fn variants_of(payload: &Value) -> Vec<Variant> {
    let mut variants: Vec<Variant> = Vec::new();
    let player: Value = payload[7]
        .as_str()
        .and_then(|text| serde_json::from_str(text).ok())
        .unwrap_or(Value::Null);
    let streaming = &player["streamingData"];
    for (key, adaptive) in [("formats", false), ("adaptiveFormats", true)] {
        for format in streaming[key].as_array().into_iter().flatten() {
            let Some(url) = util::url_of(&format["url"], None) else {
                continue;
            };
            let mime = format["mimeType"].as_str().unwrap_or("");
            let mut variant = Variant::file(url);
            variant.container = Container::from_mime(mime);
            match codecs_of(mime) {
                Some(codecs) => variant = variant.with_codecs(&codecs),
                None if variant.container == Some(Container::Mp4) => {
                    variant.video = Some(VideoCodec::H264);
                    variant.audio = Some(AudioCodec::Aac);
                }
                None => {}
            }
            variant.width = util::u32_of(&format["width"]);
            variant.height = util::u32_of(&format["height"]);
            variant.fps = util::float(&format["fps"]);
            variant.bitrate = util::uint(&format["bitrate"]);
            variant.size = util::uint(&format["contentLength"]);
            variant.duration = util::millis(&format["approxDurationMs"]);
            variant.format_id = util::text(&format["itag"]);
            variant.label = util::text(&format["qualityLabel"]);
            if adaptive {
                variant.audio_only = mime.starts_with("audio/");
                variant.video_only = !variant.audio_only;
            }
            variants.push(variant);
        }
    }
    for stream in payload[2].as_array().into_iter().flatten() {
        let Some(url) = util::url_of(&stream[0], None) else {
            continue;
        };
        let itag = util::text(&stream[1][0]);
        if itag.is_some() && variants.iter().any(|v| v.format_id == itag) {
            continue;
        }
        let container = util::query_param(&url, "mime")
            .as_deref()
            .and_then(Container::from_mime);
        let mut variant = Variant::file(url);
        if container == Some(Container::Mp4) {
            variant.video = Some(VideoCodec::H264);
            variant.audio = Some(AudioCodec::Aac);
        }
        variant.container = container;
        variant.duration = util::query_param(&variant.url, "dur")
            .as_deref()
            .and_then(parse_seconds);
        variant.size = util::query_param(&variant.url, "clen").and_then(|clen| clen.parse().ok());
        variant.format_id = itag;
        variants.push(variant);
    }
    variants
}

pub struct BloggerResolver {
    http: Http,
}

impl BloggerResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for BloggerResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Blogger",
            hosts: &["blogger.com"],
            features: &["video embeds"],
            formats: &["mp4"],
            media: &[MediaKind::Video],
            tags: &[Tag::Players],
            session: SessionSupport::None,
            examples: &[
                "https://www.blogger.com/video.g?token=AD6v5dzEe9hfcARr5Hlq1WTkYy6t-fXH3BBahVhGvVHe5szdEUBEloSEDSTA8-b111089KbfWuBvTN7fnbxMtymsHhXAXwVvyzHH4Qch2cfLQdGxKQrrEuFpC1amSl_9GuLWODjPgw",
                "https://www.blogger.com/video.g?token=AD6v5dx9TdMlBW4zSBTx8we3NdZkcnXpeEnaiL1qr0_IDyC9wTVrt8W4iTdzKQSDUGKl60SkkYwminoaFYiah26YtShVZd4Ph6umwzwiT1_hCgNrbcrtZt0bSiiezJT0XP3wODwj7aA&origin=blog.tomeuvizoso.net",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        video_token(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let token = video_token(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let rpc = Url::parse(RPC_URL).expect("the player app's call is a valid URL");
        let request = rpc_request(RPC_ID, &json!([token]));
        let response = self
            .http
            .post(rpc)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .form(&[("f.req", request.as_str())])
            .header("origin", "https://www.blogger.com")
            .header("referer", "https://www.blogger.com/")
            .header("x-same-domain", "1")
            .send()
            .await?;
        if let Some(error) = status_error(response.status, url) {
            return Err(error);
        }
        let text = response.text(MAX_PAGE).await?;
        let payload = match rpc_answer(&text, RPC_ID) {
            Some(RpcAnswer::Payload(payload)) => payload,
            Some(RpcAnswer::Refused(code)) => {
                return Err(match util::int(&code[0]) {
                    Some(UNKNOWN_TOKEN) => ResolveError::NotFound(url.clone()),
                    _ => ResolveError::unavailable(
                        url,
                        format!("the player app refused the token: {code}"),
                    ),
                });
            }
            None => {
                return Err(ResolveError::malformed(
                    url,
                    "the player app's answer has no result for the streams call",
                ));
            }
        };
        let variants = variants_of(&payload);
        if variants.is_empty() {
            return Err(ResolveError::unavailable(
                url,
                "the player app lists no streams",
            ));
        }
        let id = util::text(&payload[4])
            .or_else(|| util::text(&payload[5]))
            .unwrap_or(token);
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.clone());
        resolved.title = clean_title(&id);
        resolved.thumbnail = util::url_of(&payload[3], None);
        resolved.duration = variants.iter().find_map(|v| v.duration);
        resolved.webpage_url = Some(url.clone());
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    fn embeds_in(&self, page: &Page) -> Vec<Url> {
        RE_EMBED
            .captures_iter(page.html())
            .filter_map(|caps| util::join_url(Some(page.url()), &util::html_unescape(&caps[1])))
            .filter(|embed| video_token(embed).is_some())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};

    fn post(url: &str, status: u16, content_type: &str, body: String) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: "POST".into(),
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

    const PLAYER: &str = "https://www.blogger.com/video.g?token=AD6v5dzEe9hfcARr5Hlq1WTkYy6t";
    const MP4_360: &str = "https://rr1---sn-x.googlevideo.com/videoplayback?expire=1789543908&id=3c740e3a49197e16&itag=18&source=blogger&mime=video%2Fmp4&dur=76.068";
    const MP4_720: &str = "https://rr1---sn-x.googlevideo.com/videoplayback?expire=1789543908&id=3c740e3a49197e16&itag=22&source=blogger&mime=video%2Fmp4&dur=76.068";
    const THREEGP: &str = "https://rr1---sn-x.googlevideo.com/videoplayback?expire=1789543908&id=3c740e3a49197e16&itag=13&source=blogger&mime=video%2F3gpp&clen=438496&dur=76.006";
    const AUDIO: &str = "https://rr1---sn-x.googlevideo.com/videoplayback?expire=1789543908&id=3c740e3a49197e16&itag=140&source=blogger&mime=audio%2Fmp4&dur=76.068";

    /// The app's answer for a video with three muxed files, one audio-only adaptive
    /// stream, and a 3GP file only the plain list names.
    fn answer() -> String {
        let player = json!({
            "streamingData": {
                "formats": [
                    {"itag": 18, "url": MP4_360, "mimeType": "video/mp4; codecs=\"avc1.42001E, mp4a.40.2\"",
                     "bitrate": 320611, "width": 480, "height": 360, "contentLength": "3045810",
                     "quality": "medium", "fps": 6, "qualityLabel": "360p", "approxDurationMs": "76068"},
                    {"itag": 22, "url": MP4_720, "mimeType": "video/mp4; codecs=\"avc1.64001F, mp4a.40.2\"",
                     "bitrate": 1032331, "width": 960, "height": 720, "quality": "hd720", "fps": 6,
                     "qualityLabel": "720p", "approxDurationMs": "76068"}
                ],
                "adaptiveFormats": [
                    {"itag": 140, "url": AUDIO, "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"",
                     "bitrate": 130000, "contentLength": "1236000", "approxDurationMs": "76068"}
                ],
                "serverAbrStreamingUrl": "https://rr1---sn-x.googlevideo.com/videoplayback?sabr=1"
            },
            "videoDetails": {"videoId": "PHQOOkkZfhY"}
        });
        let payload = json!([
            1,
            null,
            [[THREEGP, [13]], [MP4_360, [18]], [MP4_720, [22]]],
            "https://i9.ytimg.com/vi_blogger/PHQOOkkZfhY/1.jpg?sqp=abc",
            "BLOGGER-video-3c740e3a49197e16-3566",
            "3c740e3a49197e16",
            false,
            player.to_string()
        ]);
        let entry = json!([[
            "wrb.fr",
            "WcwnYd",
            payload.to_string(),
            null,
            null,
            null,
            "generic"
        ]])
        .to_string();
        format!(
            ")]}}'\n\n{}\n{entry}\n56\n[[\"di\",151],[\"af.httprm\",150,\"-35296437412991160\",12]]\n27\n[[\"e\",4,null,null,12992]]\n",
            entry.len() + 1
        )
    }

    #[test]
    fn links_are_read() {
        let token = |s: &str| video_token(&Url::parse(s).unwrap());
        assert_eq!(token(PLAYER), Some("AD6v5dzEe9hfcARr5Hlq1WTkYy6t".into()));
        assert_eq!(
            token("http://blogger.com/video.g?token=abc&origin=blog.example"),
            Some("abc".into())
        );
        assert_eq!(
            token("https://www.blogger.com/video.g?x=1&token=abc"),
            Some("abc".into())
        );
        assert_eq!(token("https://www.blogger.com/video.g?token="), None);
        assert_eq!(token("https://www.blogger.com/blog/posts/1"), None);
        assert_eq!(token("https://example.com/video.g?token=abc"), None);
    }

    #[test]
    fn requests_and_answers_follow_the_app() {
        assert_eq!(
            rpc_request("WcwnYd", &json!(["tok"])),
            r#"[[["WcwnYd","[\"tok\"]",null,"generic"]]]"#
        );
        let found = rpc_answer(&answer(), "WcwnYd").unwrap();
        let RpcAnswer::Payload(payload) = found else {
            panic!("{found:?}");
        };
        assert_eq!(
            payload[4].as_str(),
            Some("BLOGGER-video-3c740e3a49197e16-3566")
        );
        assert_eq!(rpc_answer(&answer(), "Other"), None);
        assert_eq!(
            rpc_answer(
                ")]}'\n\n104\n[[\"wrb.fr\",\"WcwnYd\",null,null,null,[3],\"generic\"],[\"di\",78]]\n25\n[[\"e\",4,null,null,140]]\n",
                "WcwnYd"
            ),
            Some(RpcAnswer::Refused(json!([3])))
        );
        assert_eq!(rpc_answer("<html>busy</html>", "WcwnYd"), None);
        assert_eq!(
            rpc_answer(
                ")]}'\n\n40\n[[\"wrb.fr\",\"WcwnYd\",\"not json\"]]\n",
                "WcwnYd"
            ),
            None
        );
        assert_eq!(
            codecs_of("video/mp4; codecs=\"avc1.42001E, mp4a.40.2\"").as_deref(),
            Some("avc1.42001E, mp4a.40.2")
        );
        assert_eq!(codecs_of("video/3gpp"), None);
    }

    #[tokio::test]
    async fn videos_resolve_with_every_stream() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(post(
            RPC_URL,
            200,
            "application/json; charset=utf-8",
            answer(),
        ));
        let resolver = BloggerResolver::new(Http::replay(fixture));
        let url = Url::parse(PLAYER).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(
            resolved.id.as_deref(),
            Some("BLOGGER-video-3c740e3a49197e16-3566")
        );
        assert_eq!(
            resolved.title.as_deref(),
            Some("BLOGGER-video-3c740e3a49197e16-3566")
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(76.068)));
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://i9.ytimg.com/vi_blogger/PHQOOkkZfhY/1.jpg?sqp=abc"
        );
        assert_eq!(resolved.webpage_url.as_ref(), Some(&url));
        assert_eq!(resolved.variants.len(), 4);
        let mp4 = &resolved.variants[0];
        assert_eq!(mp4.url.as_str(), MP4_360);
        assert_eq!(mp4.container, Some(Container::Mp4));
        assert_eq!(mp4.video, Some(VideoCodec::H264));
        assert_eq!(mp4.audio, Some(AudioCodec::Aac));
        assert_eq!(mp4.codecs.as_deref(), Some("avc1.42001E, mp4a.40.2"));
        assert_eq!(mp4.width, Some(480));
        assert_eq!(mp4.height, Some(360));
        assert_eq!(mp4.bitrate, Some(320611));
        assert_eq!(mp4.size, Some(3045810));
        assert_eq!(mp4.format_id.as_deref(), Some("18"));
        assert_eq!(mp4.label.as_deref(), Some("360p"));
        assert!(!mp4.video_only && !mp4.audio_only);
        assert_eq!(resolved.variants[1].height, Some(720));
        assert_eq!(resolved.variants[1].size, None);
        let audio = &resolved.variants[2];
        assert_eq!(audio.url.as_str(), AUDIO);
        assert!(audio.audio_only);
        assert!(!audio.video_only);
        assert_eq!(audio.audio, Some(AudioCodec::Aac));
        assert_eq!(audio.video, None);
        let threegp = &resolved.variants[3];
        assert_eq!(threegp.url.as_str(), THREEGP);
        assert_eq!(threegp.format_id.as_deref(), Some("13"));
        assert_eq!(threegp.container, None);
        assert_eq!(threegp.size, Some(438496));
        assert_eq!(threegp.duration, Some(Duration::from_secs_f64(76.006)));
    }

    #[tokio::test]
    async fn missing_videos_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(post(
            RPC_URL,
            200,
            "application/json; charset=utf-8",
            ")]}'\n\n104\n[[\"wrb.fr\",\"WcwnYd\",null,null,null,[3],\"generic\"],[\"di\",78]]\n25\n[[\"e\",4,null,null,140]]\n".into(),
        ));
        fixture.exchanges.push(post(
            RPC_URL,
            200,
            "application/json; charset=utf-8",
            ")]}'\n\n60\n[[\"wrb.fr\",\"WcwnYd\",null,null,null,[7],\"generic\"]]\n".into(),
        ));
        fixture.exchanges.push(post(
            RPC_URL,
            200,
            "text/html",
            "<html><body>nothing</body></html>".into(),
        ));
        fixture.exchanges.push(post(
            RPC_URL,
            200,
            "application/json; charset=utf-8",
            ")]}'\n\n70\n[[\"wrb.fr\",\"WcwnYd\",\"[1,null,[],null,\\\"BLOGGER-video-x\\\"]\",null,null,null,\"generic\"]]\n".into(),
        ));
        fixture.exchanges.push(post(
            RPC_URL,
            429,
            "text/html",
            "<html>slow down</html>".into(),
        ));
        let resolver = BloggerResolver::new(Http::replay(fixture));
        let resolve = |token: &str| {
            let url =
                Url::parse(&format!("https://www.blogger.com/video.g?token={token}")).unwrap();
            let resolver = &resolver;
            async move { resolver.resolve(&url).await.unwrap_err() }
        };
        assert!(matches!(
            resolve("unknown").await,
            ResolveError::NotFound(_)
        ));
        let error = resolve("refused").await;
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("refused the token: [7]")),
            "{error}"
        );
        let error = resolve("shell").await;
        assert!(matches!(error, ResolveError::Malformed { .. }), "{error}");
        let error = resolve("empty").await;
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("no streams")),
            "{error}"
        );
        assert!(matches!(
            resolve("busy").await,
            ResolveError::RateLimited(_)
        ));
    }

    #[test]
    fn embedded_players_are_found() {
        let html = concat!(
            "<p><iframe src='//www.blogger.com/video.g?token=abc' width=320></iframe>",
            "<iframe allowfullscreen=\"allowfullscreen\" class=\"b-hbp-video b-uploaded\" id=\"BLOGGER-video-3c740e3a49197e16-3566\" src=\"https://www.blogger.com/video.g?token=def&amp;origin=blog.example\"></iframe>",
            "<iframe src=\"https://www.youtube.com/embed/x\"></iframe></p>"
        );
        let page = Page::parse(html, &Url::parse("https://blog.example/post").unwrap());
        let resolver = BloggerResolver::new(Http::replay(Fixture::new(PLATFORM, None)));
        let embeds = resolver.embeds_in(&page);
        assert_eq!(embeds.len(), 2);
        assert_eq!(
            embeds[0].as_str(),
            "https://www.blogger.com/video.g?token=abc"
        );
        assert_eq!(
            embeds[1].as_str(),
            "https://www.blogger.com/video.g?token=def&origin=blog.example"
        );
        assert!(resolver.matches(&embeds[1]));
        assert_eq!(video_token(&embeds[1]).as_deref(), Some("def"));
    }
}
