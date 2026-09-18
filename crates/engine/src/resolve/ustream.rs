//! Ustream, now IBM Video Streaming (video.ibm.com, where ustream.tv links redirect):
//! recordings, channels live and off air, and the player embeds of both. Records come
//! from the videos and channels API; streams come from the UMS media server through the
//! viewer handshake the player makes, which names the HLS playlist of a recording, or of
//! a channel while it broadcasts. An off-air channel is the list of its recordings.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionSupport, Tag, Variant, clean_title, fetch, fetch_ok, hls, navigation_headers,
    status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "ustream";
const SITE: &str = "https://video.ibm.com";
const API: &str = "https://api.video.ibm.com";
/// The media server's viewer application the web player identifies itself as: id and
/// version, answered with HLS.
const APP: (u32, u32) = (11, 2);
/// Recordings per page of the channel videos API, the most it pages by.
const PAGE_SIZE: usize = 50;
/// A playlist stops growing here.
const MAX_ENTRIES: usize = 500;
/// How many answers of one handshake connection are read for the stream: the server
/// sends the module info first, the stream in a later answer, then pings.
const MAX_POLLS: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// `/recorded/{id}`: one recording.
    Recorded(String),
    /// `/embed/{id}`: the player of a channel.
    Embed(String),
    /// `/embed/recorded/{id}`: the player of one recording.
    EmbedRecorded(String),
    /// `/channel/{slug}`: a channel's page.
    Channel(String),
}

static RE_RECORDED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/recorded/(\d+)").unwrap());
static RE_EMBED_RECORDED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/embed/recorded/(\d+)").unwrap());
static RE_EMBED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/embed/(\d+)").unwrap());
static RE_CHANNEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/channel/([^/?#]+)").unwrap());
/// The player frames another site embeds.
static RE_EMBED_FRAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"<iframe[^>]+?src=(?:"(https?://(?:www\.)?(?:ustream\.tv|video\.ibm\.com)/embed/[^"]+)"|'(https?://(?:www\.)?(?:ustream\.tv|video\.ibm\.com)/embed/[^']+)')"#,
    )
    .unwrap()
});
/// `image_{width}x{height}`, as the API names a record's thumbnail sizes.
static RE_IMAGE_SIZE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^image_(\d+)x(\d+)$").unwrap());

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    if host != "ustream.tv" && host != "video.ibm.com" {
        return None;
    }
    let path = url.path();
    if let Some(caps) = RE_EMBED_RECORDED.captures(path) {
        return Some(Link::EmbedRecorded(caps[1].to_string()));
    }
    if let Some(caps) = RE_RECORDED.captures(path) {
        return Some(Link::Recorded(caps[1].to_string()));
    }
    if let Some(caps) = RE_EMBED.captures(path) {
        return Some(Link::Embed(caps[1].to_string()));
    }
    if let Some(caps) = RE_CHANNEL.captures(path) {
        return Some(Link::Channel(caps[1].to_string()));
    }
    None
}

/// The player frames `html` embeds: what a site that shows Ustream video hands on.
pub fn embed_urls(html: &str) -> Vec<Url> {
    RE_EMBED_FRAME
        .captures_iter(html)
        .filter_map(|caps| caps.get(1).or_else(|| caps.get(2)))
        .filter_map(|m| Url::parse(&util::html_unescape(m.as_str())).ok())
        .collect()
}

fn recorded_url(id: &str) -> Url {
    Url::parse(&format!("{SITE}/recorded/{id}")).expect("valid")
}

fn channel_url(slug: &str) -> Url {
    Url::parse(&format!("{SITE}/channel/{slug}")).expect("valid")
}

/// The random values the viewer handshake carries, fresh for every handshake as the
/// player makes them: the number in the handshake host, the session id and the pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handshake {
    pub host_number: u64,
    pub rsid: String,
    pub rpin: String,
}

impl Handshake {
    pub fn fresh() -> Self {
        use rand::RngExt;
        let mut rng = rand::rng();
        Self {
            host_number: rng.random_range(0..100_000_000u64),
            rsid: format!(
                "{:x}:{:x}",
                rng.random_range(0..100_000_000u64),
                rng.random_range(0..100_000_000u64)
            ),
            rpin: format!("_rpin.{}", rng.random_range(0..1_000_000_000_000_000u64)),
        }
    }
}

/// What the media server is asked for: a recording, or a channel's broadcast.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Broadcast {
    Recorded,
    Channel,
}

impl Broadcast {
    fn application(self) -> &'static str {
        match self {
            Broadcast::Recorded => "recorded",
            Broadcast::Channel => "channel",
        }
    }

    /// The handshake host names the application too.
    fn host_kind(self) -> &'static str {
        match self {
            Broadcast::Recorded => "recorded-lp-live",
            Broadcast::Channel => "channel-live",
        }
    }
}

async fn api_json(http: &Http, origin: &Url, url: &Url) -> Result<Value, ResolveError> {
    fetch_ok(http, url, PLATFORM, BROWSER_UA, &[], MAX_PAGE)
        .await?
        .json(origin)
}

/// The HLS playlist the media server names for `media_id` through one viewer handshake:
/// the connection it assigns is polled until an answer carries the stream. A locked
/// broadcast is refused, with the lock named.
pub async fn ums_stream(
    http: &Http,
    origin: &Url,
    broadcast: Broadcast,
    media_id: &str,
    handshake: &Handshake,
) -> Result<Url, ResolveError> {
    let connect = Url::parse(&format!(
        "http://r{}-1-{media_id}-{}.ums.ustream.tv/1/ustream",
        handshake.host_number,
        broadcast.host_kind()
    ))
    .map_err(|e| ResolveError::malformed(origin, format!("handshake host: {e}")))?;
    let connect = util::with_query(
        &connect,
        &[
            ("type", "viewer"),
            ("appId", &APP.0.to_string()),
            ("appVersion", &APP.1.to_string()),
            ("rsid", &handshake.rsid),
            ("rpin", &handshake.rpin),
            ("referrer", origin.as_str()),
            ("media", media_id),
            ("application", broadcast.application()),
        ],
    );
    let connection = api_json(http, origin, &connect).await?;
    let args = &connection[0]["args"][0];
    let host = args["host"]
        .as_str()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| ResolveError::malformed(origin, "the handshake names no media host"))?;
    let connection_id = util::text(&args["connectionId"])
        .ok_or_else(|| ResolveError::malformed(origin, "the handshake names no connection"))?;
    let poll = Url::parse(&format!(
        "http://{host}/1/ustream?connectionId={connection_id}"
    ))
    .map_err(|e| ResolveError::malformed(origin, format!("handshake media host: {e}")))?;
    for _ in 0..MAX_POLLS {
        let answer = api_json(http, origin, &poll).await?;
        for command in answer.as_array().into_iter().flatten() {
            let args: Vec<&Value> = command["args"].as_array().into_iter().flatten().collect();
            if command["cmd"] == "reject" {
                let locks: Vec<&str> = args
                    .iter()
                    .filter_map(|a| a.as_object())
                    .flat_map(|o| o.keys())
                    .map(String::as_str)
                    .collect();
                let reason = if locks.is_empty() {
                    "no reason given".to_string()
                } else {
                    locks.join(", ")
                };
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the media server refused the viewer: {reason}"),
                ));
            }
            if let Some(stream) = args
                .iter()
                .filter_map(|a| a["stream"].as_array())
                .flatten()
                .find_map(|s| s["url"].as_str())
                .and_then(|u| util::join_url(None, u))
            {
                return Ok(stream);
            }
        }
    }
    Err(ResolveError::unavailable(
        origin,
        format!("the media server named no stream in {MAX_POLLS} answers"),
    ))
}

/// The locks an API answer names on a record: a password, viewer registration, the
/// pages allowed to embed it...
fn locked(record: &Value) -> Option<String> {
    let names: Vec<&str> = record["locks"]
        .as_object()?
        .keys()
        .map(String::as_str)
        .collect();
    (!names.is_empty()).then(|| names.join(", "))
}

/// The `kind` record (`video`, `channel`) the API answers at `path`, telling a lock, a
/// missing record and an API error apart.
async fn api_record(
    http: &Http,
    origin: &Url,
    path: &str,
    kind: &str,
) -> Result<Value, ResolveError> {
    let api = Url::parse(&format!("{API}/{path}"))
        .map_err(|e| ResolveError::malformed(origin, format!("API path {path}: {e}")))?;
    let fetched = fetch(http, &api, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
    let answer: Value = serde_json::from_slice(&fetched.body).unwrap_or(Value::Null);
    if let Some(locks) = locked(&answer).or_else(|| locked(&answer[kind])) {
        return Err(ResolveError::unavailable(
            origin,
            format!("the {kind} is locked: {locks}"),
        ));
    }
    if let Some(error) = status_error(fetched.status, origin) {
        return Err(error);
    }
    let answer = fetched.json(origin)?;
    let error = &answer["error"];
    if !error.is_null() && error != &Value::Bool(false) {
        let message = error
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| error.to_string());
        return Err(ResolveError::unavailable(
            origin,
            format!("ustream returned error: {message}"),
        ));
    }
    let record = answer[kind].clone();
    if !record.is_object() {
        return Err(ResolveError::malformed(
            origin,
            format!("the API answers no {kind} record"),
        ));
    }
    Ok(record)
}

/// The largest of the thumbnails a record lists by size: `image_{width}x{height}`
/// entries first, then the named sizes.
fn thumbnail_of(thumbnails: &Value) -> Option<Url> {
    let map = thumbnails.as_object()?;
    let sized = map
        .iter()
        .filter_map(|(name, value)| {
            let caps = RE_IMAGE_SIZE.captures(name)?;
            let width: u64 = caps[1].parse().ok()?;
            Some((width, value))
        })
        .max_by_key(|(width, _)| *width)
        .map(|(_, value)| value);
    sized
        .or_else(|| {
            ["live", "large", "big", "medium", "default", "small"]
                .iter()
                .find_map(|name| map.get(*name))
        })
        .or_else(|| map.values().next())
        .and_then(|v| util::url_of(v, None))
}

/// Resolves recording `video_id`: its files when the API lists any, else the HLS playlist
/// the media server names through the handshake with the values given.
pub async fn resolve_recorded(
    http: &Http,
    origin: &Url,
    video_id: &str,
    handshake: &Handshake,
) -> Result<Resolved, ResolveError> {
    let video = api_record(http, origin, &format!("videos/{video_id}.json"), "video").await?;
    let filesize = util::float(&video["file_size"])
        .filter(|s| *s > 0.0)
        .map(|s| s as u64);
    let mut variants = Vec::new();
    for (format_id, media_url) in video["media_urls"].as_object().into_iter().flatten() {
        let Some(media_url) = media_url.as_str().and_then(|u| util::join_url(None, u)) else {
            continue;
        };
        let mut variant = Variant::file(media_url);
        variant.container = Container::from_extension(format_id);
        if variant.container == Some(Container::Mp4) {
            variant.video = Some(VideoCodec::H264);
            variant.audio = Some(AudioCodec::Aac);
        }
        variant.size = filesize;
        variant.format_id = Some(format_id.clone());
        variants.push(variant);
    }
    let mut subtitles = Vec::new();
    let mut stream_duration = None;
    let mut live = false;
    if variants.is_empty() {
        let stream = ums_stream(http, origin, Broadcast::Recorded, video_id, handshake).await?;
        let expanded = hls::expand(http, &stream, PLATFORM, BROWSER_UA, &[]).await?;
        variants = expanded.variants;
        subtitles = expanded.subtitles;
        stream_duration = expanded.duration;
        live = expanded.live;
    }
    let mut resolved = Resolved::new(PLATFORM);
    resolved.id = Some(video_id.to_string());
    resolved.title = video["title"].as_str().and_then(clean_title);
    resolved.description = video["description"]
        .as_str()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(str::to_string);
    resolved.uploader = video["owner"]["username"].as_str().and_then(clean_title);
    resolved.uploaded_at = util::epoch(&video["created_at"]);
    resolved.duration = util::float(&video["length"])
        .filter(|d| *d > 0.0)
        .map(Duration::from_secs_f64)
        .or(stream_duration);
    resolved.thumbnail = thumbnail_of(&video["thumbnail"]);
    resolved.webpage_url =
        Some(util::url_of(&video["url"], None).unwrap_or_else(|| recorded_url(video_id)));
    resolved.live = live;
    resolved.subtitles = subtitles;
    resolved.variants = variants;
    Ok(resolved)
}

/// Resolves channel `channel_id`: its broadcast while it is live, else the list of its
/// recordings, newest first.
pub async fn resolve_channel(
    http: &Http,
    origin: &Url,
    channel_id: &str,
    handshake: &Handshake,
) -> Result<Resolution, ResolveError> {
    let channel = api_record(
        http,
        origin,
        &format!("channels/{channel_id}.json"),
        "channel",
    )
    .await?;
    let title = channel["title"].as_str().and_then(clean_title);
    let page_url = channel["url"]
        .as_str()
        .map(str::trim)
        .filter(|slug| !slug.is_empty())
        .map(channel_url)
        .unwrap_or_else(|| origin.clone());
    if channel["status"].as_str() == Some("live") {
        let stream = ums_stream(http, origin, Broadcast::Channel, channel_id, handshake).await?;
        let expanded = hls::expand(http, &stream, PLATFORM, BROWSER_UA, &[]).await?;
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(channel_id.to_string());
        resolved.title = title;
        resolved.description = channel["description"]
            .as_str()
            .map(util::clean_html)
            .and_then(|d| clean_title(&d));
        resolved.uploader = channel["owner"]["username"].as_str().and_then(clean_title);
        resolved.thumbnail =
            thumbnail_of(&channel["thumbnail"]).or_else(|| thumbnail_of(&channel["picture"]));
        resolved.webpage_url = Some(page_url);
        resolved.live = true;
        resolved.subtitles = expanded.subtitles;
        resolved.variants = expanded
            .variants
            .into_iter()
            .map(|mut v| {
                v.live = true;
                v
            })
            .collect();
        return Ok(Resolution::from(resolved));
    }
    let mut entries: Vec<PlaylistEntry> = Vec::new();
    let mut total = None;
    let mut next = Some(
        Url::parse(&format!(
            "{API}/channels/{channel_id}/videos.json?page=1&pagesize={PAGE_SIZE}"
        ))
        .map_err(|e| ResolveError::malformed(origin, format!("channel id {channel_id}: {e}")))?,
    );
    while let Some(page) = next.take() {
        let answer = api_json(http, origin, &page).await?;
        for video in answer["videos"].as_array().into_iter().flatten() {
            let Some(id) = util::text(&video["id"]) else {
                continue;
            };
            entries.push(PlaylistEntry {
                url: recorded_url(&id),
                title: video["title"].as_str().and_then(clean_title),
                duration: util::float(&video["length"])
                    .filter(|d| *d > 0.0)
                    .map(Duration::from_secs_f64),
            });
            if entries.len() >= MAX_ENTRIES {
                break;
            }
        }
        total = util::uint(&answer["paging"]["item_count"])
            .map(|n| n as usize)
            .or(total);
        if entries.len() >= MAX_ENTRIES {
            break;
        }
        next = answer["paging"]["next"]
            .as_str()
            .and_then(|n| util::join_url(Some(&page), n));
    }
    if entries.is_empty() {
        return Err(ResolveError::unavailable(
            origin,
            "the channel is off air and has no public recordings",
        ));
    }
    Ok(Resolution::Playlist(Playlist {
        resolver: PLATFORM.to_string(),
        id: Some(channel_id.to_string()),
        title,
        total: total.filter(|n| *n > entries.len()),
        entries,
    }))
}

pub struct UstreamResolver {
    http: Http,
    handshake: Option<Handshake>,
}

impl UstreamResolver {
    pub fn new(http: Http) -> Self {
        Self {
            http,
            handshake: None,
        }
    }

    /// A resolver whose every handshake carries `handshake` instead of fresh random
    /// values: what replaying a recording needs, to ask for the hosts it recorded.
    pub fn with_handshake(http: Http, handshake: Handshake) -> Self {
        Self {
            http,
            handshake: Some(handshake),
        }
    }

    fn handshake(&self) -> Handshake {
        self.handshake.clone().unwrap_or_else(Handshake::fresh)
    }

    /// The channel id a channel page names.
    async fn channel_id(&self, url: &Url) -> Result<String, ResolveError> {
        let fetched = fetch_ok(
            &self.http,
            url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        let html = fetched.text();
        Page::parse(&html, &fetched.url)
            .meta("ustream:channel_id")
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()))
            .ok_or_else(|| ResolveError::malformed(url, "the channel page names no channel id"))
    }
}

#[async_trait]
impl Resolver for UstreamResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "IBM Video Streaming (Ustream)",
            hosts: &["video.ibm.com", "ustream.tv"],
            features: &["recordings", "live", "channels", "embeds"],
            formats: &["hls", "mp4", "flv"],
            media: &[MediaKind::Video],
            tags: &[Tag::Live],
            session: SessionSupport::None,
            examples: &[
                "https://video.ibm.com/recorded/134842643",
                "http://www.ustream.tv/recorded/134736279",
                "https://video.ibm.com/embed/recorded/134842643",
                "https://video.ibm.com/channel/Q4zW6rUvwPq",
                "http://www.ustream.tv/channel/amMKekLvTG8",
                "https://video.ibm.com/embed/23605217",
                "http://www.ustream.tv/embed/23952663",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::EmbedRecorded(id) => Err(ResolveError::Redirect(recorded_url(&id))),
            Link::Recorded(id) => Ok(Resolution::from(
                resolve_recorded(&self.http, url, &id, &self.handshake()).await?,
            )),
            Link::Embed(id) => resolve_channel(&self.http, url, &id, &self.handshake()).await,
            Link::Channel(_) => {
                let id = self.channel_id(url).await?;
                resolve_channel(&self.http, url, &id, &self.handshake()).await
            }
        }
    }

    fn embeds_in(&self, page: &Page) -> Vec<Url> {
        embed_urls(page.html())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};
    use serde_json::json;

    const RECORDED: &str = "https://video.ibm.com/recorded/134842643";
    const RECORDED_USTREAM: &str = "http://www.ustream.tv/recorded/134736279";
    const EMBED_RECORDED: &str = "https://video.ibm.com/embed/recorded/134842643";
    const LIVE_CHANNEL: &str = "https://video.ibm.com/channel/Q4zW6rUvwPq";
    const OFF_AIR_CHANNEL: &str = "http://www.ustream.tv/channel/amMKekLvTG8";
    const LIVE_EMBED: &str = "https://video.ibm.com/embed/23605217";
    const OFF_AIR_EMBED: &str = "http://www.ustream.tv/embed/23952663";

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn fixture() -> Fixture {
        Fixture::parse(include_str!("ustream_fixture.json")).unwrap()
    }

    /// The handshake values the recording made for `media_id`, read off its connect
    /// request.
    fn handshake_in(fixture: &Fixture, media_id: &str) -> Handshake {
        let marker = format!("-1-{media_id}-");
        let connect = fixture
            .exchanges
            .iter()
            .map(|e| url(&e.request.url))
            .find(|u| u.host_str().is_some_and(|h| h.contains(&marker)))
            .unwrap_or_else(|| panic!("the fixture holds no handshake for {media_id}"));
        let host = connect.host_str().unwrap();
        let host_number = host
            .strip_prefix('r')
            .and_then(|h| h.split('-').next())
            .and_then(|n| n.parse().ok())
            .unwrap();
        Handshake {
            host_number,
            rsid: util::query_param(&connect, "rsid").unwrap(),
            rpin: util::query_param(&connect, "rpin").unwrap(),
        }
    }

    fn replay(media_id: &str) -> UstreamResolver {
        let fixture = fixture();
        let handshake = handshake_in(&fixture, media_id);
        UstreamResolver::with_handshake(Http::replay(fixture), handshake)
    }

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

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&url(s));
        assert_eq!(link(RECORDED), Some(Link::Recorded("134842643".into())));
        assert_eq!(
            link(RECORDED_USTREAM),
            Some(Link::Recorded("134736279".into()))
        );
        assert_eq!(
            link("http://www.ustream.tv/embed/recorded/59307601?ub=ff0000&lc=ff0000&v=3"),
            Some(Link::EmbedRecorded("59307601".into()))
        );
        assert_eq!(
            link("https://video.ibm.com/embed/recorded/128240221?&autoplay=true&controls=true"),
            Some(Link::EmbedRecorded("128240221".into()))
        );
        assert_eq!(link(LIVE_EMBED), Some(Link::Embed("23605217".into())));
        assert_eq!(link(OFF_AIR_EMBED), Some(Link::Embed("23952663".into())));
        assert_eq!(
            link(LIVE_CHANNEL),
            Some(Link::Channel("Q4zW6rUvwPq".into()))
        );
        assert_eq!(
            link("http://www.ustream.tv/channel/nasa-tv-wallops?x=1"),
            Some(Link::Channel("nasa-tv-wallops".into()))
        );
        assert_eq!(link("https://www.ustream.tv/recorded/abc"), None);
        assert_eq!(link("https://video.ibm.com/channel/"), None);
        assert_eq!(link("https://video.ibm.com/"), None);
        assert_eq!(link("https://example.com/recorded/1"), None);
        assert_eq!(link("ftp://video.ibm.com/recorded/1"), None);
    }

    #[test]
    fn embedded_players_are_found() {
        let html = r#"<div><iframe width="640" src="http://www.ustream.tv/embed/recorded/58428542?v=3&amp;wmode=direct" frameborder="0"></iframe>
            <iframe src='https://video.ibm.com/embed/1234'></iframe><iframe src="https://example.com/embed/1"></iframe></div>"#;
        let found = embed_urls(html);
        assert_eq!(found.len(), 2);
        assert_eq!(
            found[0].as_str(),
            "http://www.ustream.tv/embed/recorded/58428542?v=3&wmode=direct"
        );
        assert_eq!(found[1].as_str(), "https://video.ibm.com/embed/1234");
        assert!(found.iter().all(|u| parse_link(u).is_some()));
    }

    #[test]
    fn the_largest_thumbnail_is_taken() {
        let sized = json!({
            "default": "https://cdn.test/d.jpg", "image_192x108": "https://cdn.test/s.jpg",
            "image_1920x1080": "https://cdn.test/l.jpg", "image_640x360": "https://cdn.test/m.jpg"
        });
        assert_eq!(
            thumbnail_of(&sized).unwrap().as_str(),
            "https://cdn.test/l.jpg"
        );
        let named = json!({"small": "https://cdn.test/s.jpg", "live": "https://cdn.test/live.jpg"});
        assert_eq!(
            thumbnail_of(&named).unwrap().as_str(),
            "https://cdn.test/live.jpg"
        );
        assert_eq!(thumbnail_of(&json!(null)), None);
    }

    #[tokio::test]
    async fn recordings_resolve_to_the_handshake_stream() {
        let resolver = replay("134842643");
        assert!(resolver.matches(&url(RECORDED)));
        let resolved = resolver
            .resolve(&url(RECORDED))
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("134842643"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("NVIDIA Financial Analyst Q&A")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("NVIDIA"));
        assert_eq!(resolved.uploaded_at.unwrap().as_second(), 1780373722);
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(7069.166)));
        assert!(
            resolved
                .thumbnail
                .as_ref()
                .is_some_and(|t| t.as_str().contains("1920x1080"))
        );
        assert_eq!(
            resolved.webpage_url.as_ref().map(Url::as_str),
            Some(RECORDED)
        );
        assert!(!resolved.live);
        assert!(resolved.variants.len() >= 2, "{:?}", resolved.variants);
        assert!(
            resolved
                .variants
                .iter()
                .all(|v| v.kind == super::super::VariantKind::Hls)
        );
        assert!(resolved.variants.iter().any(|v| v.height == Some(1080)));
        assert!(resolved.variants.iter().all(|v| !v.live));
    }

    #[tokio::test]
    async fn ustream_tv_recordings_resolve_the_same() {
        let resolver = replay("134736279");
        let resolved = resolver
            .resolve(&url(RECORDED_USTREAM))
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("134736279"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("2026 Grade 5 Migration Day")
        );
        assert_eq!(
            resolved.webpage_url.as_ref().map(Url::as_str),
            Some("https://video.ibm.com/recorded/134736279")
        );
        assert!(!resolved.variants.is_empty());
    }

    #[tokio::test]
    async fn recording_embeds_hand_on_to_the_recording() {
        let resolver = UstreamResolver::new(Http::replay(Fixture::new(PLATFORM, None)));
        assert!(matches!(
            resolver.resolve(&url(EMBED_RECORDED)).await.unwrap_err(),
            ResolveError::Redirect(u) if u.as_str() == RECORDED
        ));
    }

    #[tokio::test]
    async fn live_channels_resolve_to_their_broadcast() {
        for link in [LIVE_CHANNEL, LIVE_EMBED] {
            let resolver = replay("23605217");
            let resolved = resolver.resolve(&url(link)).await.unwrap().media().unwrap();
            assert_eq!(resolved.id.as_deref(), Some("23605217"));
            assert_eq!(
                resolved.title.as_deref(),
                Some("Sisters of Divine Providence")
            );
            assert_eq!(resolved.uploader.as_deref(), Some("vgm4u1ne0yy"));
            assert!(
                resolved
                    .description
                    .as_deref()
                    .is_some_and(|d| d.starts_with("Founded in Finthen"))
            );
            assert_eq!(
                resolved.webpage_url.as_ref().map(Url::as_str),
                Some(LIVE_CHANNEL)
            );
            assert!(resolved.thumbnail.is_some());
            assert!(resolved.live);
            assert!(!resolved.variants.is_empty(), "{link}");
            assert!(resolved.variants.iter().all(|v| v.live));
            assert!(resolved.variants.iter().any(|v| v.height == Some(720)));
        }
    }

    #[tokio::test]
    async fn off_air_channels_list_their_recordings() {
        for link in [OFF_AIR_CHANNEL, OFF_AIR_EMBED] {
            let resolver = UstreamResolver::new(Http::replay(fixture()));
            let Resolution::Playlist(playlist) = resolver.resolve(&url(link)).await.unwrap() else {
                panic!("{link} should list a playlist");
            };
            assert_eq!(playlist.id.as_deref(), Some("23952663"));
            assert_eq!(playlist.title.as_deref(), Some("IBM Data and AI Content"));
            assert_eq!(playlist.entries.len(), MAX_ENTRIES, "{link}");
            assert!(playlist.total.is_some_and(|t| t > MAX_ENTRIES));
            assert!(playlist.entries.iter().all(|e| {
                e.url
                    .as_str()
                    .starts_with("https://video.ibm.com/recorded/")
            }));
            assert!(playlist.entries.iter().all(|e| e.title.is_some()));
            assert!(playlist.entries.iter().all(|e| e.duration.is_some()));
        }
    }

    #[tokio::test]
    async fn recordings_with_files_resolve_to_them() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get("https://api.video.ibm.com/videos/20274954.json", 200, "application/json", json!({
            "video": {
                "id": "20274954", "title": "Young Americans for Liberty February 7, 2012 2:28 AM",
                "created_at": 1328577035, "length": "1234.5", "file_size": "55000000",
                "owner": {"username": "yaliberty", "id": 6780869},
                "thumbnail": {"default": "https://static.ustream.tv/s.jpg", "image_640x360": "https://static.ustream.tv/l.jpg"},
                "media_urls": {"flv": "https://tcdn.ustream.tv/video/20274954.flv", "mp4": "https://tcdn.ustream.tv/video/20274954.mp4", "webm": null},
                "locks": {}
            }
        }).to_string()));
        let resolver = UstreamResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&url("http://www.ustream.tv/recorded/20274954"))
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.uploader.as_deref(), Some("yaliberty"));
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(1234.5)));
        assert_eq!(
            resolved.thumbnail.unwrap().as_str(),
            "https://static.ustream.tv/l.jpg"
        );
        assert_eq!(
            resolved.webpage_url.unwrap().as_str(),
            "https://video.ibm.com/recorded/20274954"
        );
        assert_eq!(resolved.variants.len(), 2);
        let flv = resolved
            .variants
            .iter()
            .find(|v| v.format_id.as_deref() == Some("flv"))
            .unwrap();
        assert_eq!(flv.container, Some(Container::Flv));
        assert_eq!(flv.size, Some(55000000));
        let mp4 = resolved
            .variants
            .iter()
            .find(|v| v.format_id.as_deref() == Some("mp4"))
            .unwrap();
        assert_eq!(mp4.container, Some(Container::Mp4));
        assert_eq!(mp4.video, Some(VideoCodec::H264));
    }

    #[tokio::test]
    async fn locked_missing_and_refused_media_say_so() {
        let handshake = Handshake {
            host_number: 123,
            rsid: "abc:def".into(),
            rpin: "_rpin.42".into(),
        };
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://api.video.ibm.com/channels/13567824.json",
            401,
            "application/json",
            json!({"locks": {"password": []}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.video.ibm.com/videos/1.json",
            404,
            "application/json",
            json!({"error": "not_found"}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.video.ibm.com/videos/59307601.json",
            200,
            "application/json",
            json!({"error": "This Pro Broadcaster has chosen to remove this video from the ustream.tv site."}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.video.ibm.com/videos/133482044.json",
            200,
            "application/json",
            json!({"video": {"id": "133482044", "title": "Earn It", "media_urls": {"flv": null}, "locks": {}}}).to_string(),
        ));
        let connect = "http://r123-1-133482044-recorded-lp-live.ums.ustream.tv/1/ustream?type=viewer&appId=11&appVersion=2&rsid=abc%3Adef&rpin=_rpin.42&referrer=https%3A%2F%2Fvideo.ibm.com%2Frecorded%2F133482044&media=133482044&application=recorded";
        fixture.exchanges.push(get(
            connect,
            200,
            "application/json",
            json!([{"args": [{"host": "media.ums.ustream.tv", "connectionId": "c-1"}], "cmd": "tracking"}]).to_string(),
        ));
        fixture.exchanges.push(get(
            "http://media.ums.ustream.tv/1/ustream?connectionId=c-1",
            200,
            "application/json",
            json!([{"args": [{"referrerLock": {"redirectUrl": "https://video.ibm.com/channel/K27Pj3GwLNz"}}], "cmd": "reject"}, {"args": [], "cmd": "close"}]).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.video.ibm.com/channels/18058693.json",
            200,
            "application/json",
            json!({"channel": {"id": "18058693", "title": "NASA CSBF", "url": "nasa-csbf-ldsd", "status": "offair", "locks": {}}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.video.ibm.com/channels/18058693/videos.json?page=1&pagesize=50",
            200,
            "application/json",
            json!({"videos": [], "paging": {"page_count": 0, "item_count": 0}}).to_string(),
        ));
        let resolver = UstreamResolver::with_handshake(Http::replay(fixture), handshake);
        let error = resolver
            .resolve(&url("https://video.ibm.com/embed/13567824"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "the channel is locked: password"),
            "{error}"
        );
        let error = resolver
            .resolve(&url("https://video.ibm.com/recorded/1"))
            .await
            .unwrap_err();
        assert!(matches!(error, ResolveError::NotFound(_)), "{error}");
        let error = resolver
            .resolve(&url("http://www.ustream.tv/recorded/59307601"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("chosen to remove")),
            "{error}"
        );
        let error = resolver
            .resolve(&url("https://video.ibm.com/recorded/133482044"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "the media server refused the viewer: referrerLock"),
            "{error}"
        );
        let error = resolver
            .resolve(&url("https://video.ibm.com/embed/18058693"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("no public recordings")),
            "{error}"
        );
    }
}
