//! Odysee (LBRY) videos and channels, through the API the site itself calls: a claim is
//! resolved by its LBRY URL, its stream fetched by the same API, and a channel's videos
//! listed as a playlist.

use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use serde_json::{Value, json};
use url::Url;

use super::{
    ClipRange, MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionSupport, Variant, VariantKind, clean_title, timestamp_hint,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "odysee";
const SITE: &str = "https://odysee.com/";
const API: &str = "https://api.na-backend.odysee.com/api/v1/proxy";
const PAGE_SIZE: usize = 50;
const MAX_PAGES: usize = 10;

/// What a link names, as an LBRY URL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A stream: `lbry://@channel#id/name#id` or `lbry://name#id`.
    Stream(String),
    /// A channel: `lbry://@channel#id`.
    Channel(String),
    /// A stream or channel by its 40 hex digit claim id alone.
    Claim(String),
}

fn is_claim_id(text: &str) -> bool {
    text.len() == 40 && text.chars().all(|c| c.is_ascii_hexdigit())
}

/// A path segment such as `@lbry:3f` or `odysee:7a` as the LBRY URL writes it.
fn lbry_segment(segment: &str) -> Option<String> {
    let decoded = percent_encoding::percent_decode_str(segment)
        .decode_utf8()
        .ok()?;
    let (name, id) = match decoded.split_once(':') {
        Some((name, id)) => (name.to_string(), Some(id.to_string())),
        None => match decoded.split_once('#') {
            Some((name, id)) => (name.to_string(), Some(id.to_string())),
            None => (decoded.to_string(), None),
        },
    };
    if name.is_empty() || name.contains('/') {
        return None;
    }
    Some(match id {
        Some(id) if !id.is_empty() => format!("{name}#{id}"),
        _ => name,
    })
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let odysee = host == "odysee.com"
        || host.ends_with(".odysee.com")
        || host == "lbry.tv"
        || host.ends_with(".lbry.tv")
        || host == "open.lbry.com";
    if !odysee {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    match segments.as_slice() {
        ["$", "embed", channel, name] | ["$", "download", channel, name]
            if channel.starts_with('@') =>
        {
            Some(Link::Stream(format!("{}/{}", lbry_segment(channel)?, lbry_segment(name)?)))
        }
        ["$", "embed", name, id] | ["$", "download", name, id] if is_claim_id(id) => {
            Some(Link::Stream(format!("{name}#{id}")))
        }
        ["$", "embed", name] | ["$", "download", name] => {
            if is_claim_id(name) {
                Some(Link::Claim(name.to_string()))
            } else {
                Some(Link::Stream(lbry_segment(name)?))
            }
        }
        [channel, name] if channel.starts_with('@') => Some(Link::Stream(format!(
            "{}/{}",
            lbry_segment(channel)?,
            lbry_segment(name)?
        ))),
        [channel] if channel.starts_with('@') => Some(Link::Channel(lbry_segment(channel)?)),
        [name] if is_claim_id(name) => Some(Link::Claim(name.to_string())),
        [name] if !["$", "search", "following", "discover", "settings", "library", "wallet", "rewards", "signin", "signup", "help", "about", "featured", "popular", "trending"].contains(name) => {
            Some(Link::Stream(lbry_segment(name)?))
        }
        _ => None,
    }
}

pub struct OdyseeResolver {
    http: Http,
}

impl OdyseeResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// Calls the LBRY SDK method `method` through the site's proxy.
    async fn call(&self, method: &str, params: Value, origin: &Url) -> Result<Value, ResolveError> {
        let response = self
            .http
            .post(Url::parse(API).expect("valid"))
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("origin", "https://odysee.com")
            .header("referer", SITE)
            .json(&json!({"jsonrpc": "2.0", "method": method, "params": params, "id": 1}))
            .send()
            .await?;
        let status = response.status;
        if status.as_u16() == 429 {
            return Err(ResolveError::RateLimited(origin.clone()));
        }
        if !status.is_success() {
            return Err(ResolveError::unavailable(
                origin,
                format!("the API answered HTTP {status}"),
            ));
        }
        let answer: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, format!("{method}: {e}")))?;
        if let Some(error) = answer.get("error").filter(|e| !e.is_null()) {
            let message = error["message"]
                .as_str()
                .or_else(|| error["text"].as_str())
                .unwrap_or("the API answered with an error");
            return Err(ResolveError::unavailable(origin, message.to_string()));
        }
        Ok(answer["result"].clone())
    }

    /// The claim an LBRY URL names.
    async fn resolve_claim(&self, lbry_url: &str, origin: &Url) -> Result<Value, ResolveError> {
        let full = format!("lbry://{lbry_url}");
        let result = self
            .call("resolve", json!({"urls": [full]}), origin)
            .await?;
        let claim = result
            .as_object()
            .and_then(|map| map.values().next())
            .cloned()
            .ok_or_else(|| ResolveError::malformed(origin, "the API resolved nothing"))?;
        if let Some(error) = claim.get("error").filter(|e| !e.is_null()) {
            let name = error["name"].as_str().unwrap_or("");
            let text = error["text"].as_str().unwrap_or("the claim could not be resolved");
            return Err(if name == "NOT_FOUND" || text.contains("Could not find") {
                ResolveError::NotFound(origin.clone())
            } else {
                ResolveError::unavailable(origin, text.to_string())
            });
        }
        Ok(claim)
    }

    /// The claim with `claim_id`, whatever it is called.
    async fn claim_by_id(&self, claim_id: &str, origin: &Url) -> Result<Value, ResolveError> {
        let result = self
            .call(
                "claim_search",
                json!({"claim_ids": [claim_id], "page_size": 1, "no_totals": true}),
                origin,
            )
            .await?;
        result["items"]
            .as_array()
            .and_then(|items| items.first())
            .cloned()
            .ok_or_else(|| ResolveError::NotFound(origin.clone()))
    }

    async fn stream(&self, claim: Value, origin: &Url) -> Result<Resolution, ResolveError> {
        let value = &claim["value"];
        if claim["value_type"].as_str() == Some("channel") {
            return self.channel(claim, origin).await;
        }
        if claim["value_type"].as_str() == Some("repost") {
            let target = claim["reposted_claim"].clone();
            if target.is_object() {
                return Box::pin(self.stream(target, origin)).await;
            }
        }
        if value["stream_type"].as_str().is_some_and(|t| t != "video") {
            return Err(ResolveError::unavailable(
                origin,
                format!(
                    "the claim is {}, not a video",
                    value["stream_type"].as_str().unwrap_or("something else")
                ),
            ));
        }
        if value["fee"].is_object() && value["fee"]["amount"].as_str().is_some_and(|a| a != "0") {
            return Err(ResolveError::unavailable(origin, "the video is paid"));
        }
        let permanent = claim["permanent_url"]
            .as_str()
            .or_else(|| claim["canonical_url"].as_str())
            .ok_or_else(|| ResolveError::malformed(origin, "the claim has no URL"))?;
        let fetched = self
            .call("get", json!({"uri": permanent, "save_file": false}), origin)
            .await?;
        let streaming = fetched["streaming_url"]
            .as_str()
            .and_then(|u| Url::parse(u).ok())
            .ok_or_else(|| ResolveError::unavailable(origin, "the API handed out no stream"))?;
        let duration = value["video"]["duration"]
            .as_u64()
            .filter(|d| *d > 0)
            .map(Duration::from_secs);
        let variants = if streaming.path().ends_with(".m3u8") {
            let expanded =
                super::hls::expand(&self.http, &streaming, PLATFORM, BROWSER_UA, &[]).await?;
            let mut streams = expanded.variants;
            for v in &mut streams {
                v.duration = duration.or(v.duration);
            }
            streams
        } else {
            let mut v = Variant::new(streaming, VariantKind::File);
            v.container = match value["source"]["media_type"].as_str() {
                Some(mime) => Container::from_mime(mime).or(Some(Container::Mp4)),
                None => Some(Container::Mp4),
            };
            v.video = Some(VideoCodec::H264);
            v.audio = Some(AudioCodec::Aac);
            v.width = value["video"]["width"].as_u64().map(|w| w as u32);
            v.height = value["video"]["height"].as_u64().map(|h| h as u32);
            v.size = value["source"]["size"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .or_else(|| value["source"]["size"].as_u64())
                .filter(|s| *s > 0);
            v.duration = duration;
            vec![v]
        };
        let channel = &claim["signing_channel"];
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = claim["claim_id"].as_str().map(String::from);
        resolved.title = value["title"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| claim["name"].as_str().and_then(clean_title));
        resolved.description = value["description"].as_str().and_then(clean_title);
        resolved.uploader = channel["value"]["title"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| channel["name"].as_str().and_then(clean_title));
        resolved.uploader_url = channel["canonical_url"]
            .as_str()
            .and_then(site_url);
        resolved.uploaded_at = value["release_time"]
            .as_str()
            .and_then(|t| t.parse::<i64>().ok())
            .or_else(|| claim["timestamp"].as_i64())
            .and_then(|t| Timestamp::from_second(t).ok());
        resolved.duration = duration.or_else(|| variants.iter().find_map(|v| v.duration));
        resolved.thumbnail = value["thumbnail"]["url"]
            .as_str()
            .and_then(|u| Url::parse(u).ok());
        resolved.webpage_url = claim["canonical_url"]
            .as_str()
            .and_then(site_url);
        resolved.clip = timestamp_hint(origin).map(|start| ClipRange { start, end: None });
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn channel(&self, claim: Value, origin: &Url) -> Result<Resolution, ResolveError> {
        let channel_id = claim["claim_id"]
            .as_str()
            .ok_or_else(|| ResolveError::malformed(origin, "the channel has no claim id"))?;
        let mut entries = Vec::new();
        let mut total = None;
        for page in 1..=MAX_PAGES {
            let result = self
                .call(
                    "claim_search",
                    json!({
                        "channel_ids": [channel_id],
                        "claim_type": ["stream"],
                        "stream_types": ["video"],
                        "order_by": ["release_time"],
                        "page": page,
                        "page_size": PAGE_SIZE,
                        "no_totals": false
                    }),
                    origin,
                )
                .await?;
            if total.is_none() {
                total = result["total_items"].as_u64().map(|t| t as usize);
            }
            let items: Vec<&Value> = result["items"].as_array().into_iter().flatten().collect();
            for item in &items {
                let Some(url) = item["canonical_url"].as_str().and_then(site_url) else {
                    continue;
                };
                entries.push(PlaylistEntry {
                    url,
                    title: item["value"]["title"]
                        .as_str()
                        .and_then(clean_title)
                        .or_else(|| item["name"].as_str().and_then(clean_title)),
                    duration: item["value"]["video"]["duration"]
                        .as_u64()
                        .filter(|d| *d > 0)
                        .map(Duration::from_secs),
                });
            }
            if items.len() < PAGE_SIZE || total.is_some_and(|t| entries.len() >= t) {
                break;
            }
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.into(),
            id: Some(channel_id.to_string()),
            title: claim["value"]["title"]
                .as_str()
                .and_then(clean_title)
                .or_else(|| claim["name"].as_str().and_then(clean_title)),
            total: total.filter(|t| *t >= entries.len()).or(Some(entries.len())),
            entries,
        }))
    }
}

/// The site's page for an LBRY URL such as `lbry://@lbry#3f/odysee#7a`.
fn site_url(lbry: &str) -> Option<Url> {
    let rest = lbry.trim_start_matches("lbry://");
    let path: Vec<String> = rest
        .split('/')
        .map(|segment| segment.replacen('#', ":", 1))
        .collect();
    Url::parse(&format!("{SITE}{}", path.join("/"))).ok()
}

#[async_trait]
impl Resolver for OdyseeResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Odysee",
            hosts: &["odysee.com", "lbry.tv", "open.lbry.com"],
            features: &["videos", "channels", "embeds", "lbry links", "reposts"],
            formats: &["mp4", "hls"],
            session: SessionSupport::None,
            examples: &[
                "https://odysee.com/@lbry:3f/odysee:7a",
                "https://odysee.com/@lbry:3f",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Stream(lbry) => {
                let claim = self.resolve_claim(&lbry, url).await?;
                self.stream(claim, url).await
            }
            Link::Channel(lbry) => {
                let claim = self.resolve_claim(&lbry, url).await?;
                if claim["value_type"].as_str() != Some("channel") {
                    return self.stream(claim, url).await;
                }
                self.channel(claim, url).await
            }
            Link::Claim(id) => {
                let claim = self.claim_by_id(&id, url).await?;
                self.stream(claim, url).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

    fn post(body: Value) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: "POST".into(),
                url: API.into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status: 200,
                url: API.into(),
                headers: vec![("content-type".into(), "application/json".into())],
                body: RecordedBody::Text(body.to_string()),
                truncated: false,
            },
        }
    }

    fn claim() -> Value {
        json!({
            "claim_id": "7a416c44a6888d94fe045241bbac055c726332aa", "name": "odysee", "value_type": "stream", "timestamp": 1606848500,
            "canonical_url": "lbry://@lbry#3f/odysee#7a", "permanent_url": "lbry://odysee#7a416c44a6888d94fe045241bbac055c726332aa",
            "signing_channel": {"name": "@lbry", "claim_id": "3fda836a92faaceedfe398225fb9b2ee2ed1f01a", "canonical_url": "lbry://@lbry#3f", "value": {"title": "LBRY"}},
            "value": {"title": "Introducing Odysee: A Short Video", "description": "Big thanks to @MH for this ❤️", "thumbnail": {"url": "https://spee.ch/6/c10a143c7372d48c.jpg"},
                "release_time": "1606848480", "stream_type": "video", "source": {"media_type": "video/mp4", "sd_hash": "a27e60cc", "size": "44532814"}, "video": {"duration": 143, "height": 1080, "width": 1920}, "fee": null}
        })
    }

    #[test]
    fn links_of_every_shape_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://odysee.com/@lbry:3f/odysee:7a"),
            Some(Link::Stream("@lbry#3f/odysee#7a".into()))
        );
        assert_eq!(
            link("https://odysee.com/@lbry:3f/odysee:7a?t=10"),
            Some(Link::Stream("@lbry#3f/odysee#7a".into()))
        );
        assert_eq!(
            link("https://odysee.com/$/embed/@lbry:3f/odysee:7a"),
            Some(Link::Stream("@lbry#3f/odysee#7a".into()))
        );
        assert_eq!(
            link("https://odysee.com/$/embed/odysee/7a416c44a6888d94fe045241bbac055c726332aa"),
            Some(Link::Stream("odysee#7a416c44a6888d94fe045241bbac055c726332aa".into()))
        );
        assert_eq!(
            link("https://odysee.com/$/embed/odysee:7a416c44a6888d94fe045241bbac055c726332aa"),
            Some(Link::Stream("odysee#7a416c44a6888d94fe045241bbac055c726332aa".into()))
                .or_else(|| link("https://odysee.com/$/embed/odysee:7a416c44a6888d94fe045241bbac055c726332aa"))
        );
        assert_eq!(link("https://odysee.com/@lbry:3f"), Some(Link::Channel("@lbry#3f".into())));
        assert_eq!(
            link("https://odysee.com/7a416c44a6888d94fe045241bbac055c726332aa"),
            Some(Link::Claim("7a416c44a6888d94fe045241bbac055c726332aa".into()))
        );
        assert_eq!(link("https://odysee.com/odysee"), Some(Link::Stream("odysee".into())));
        assert_eq!(link("https://odysee.com/$/search?q=x"), None);
        assert_eq!(link("https://odysee.com/"), None);
        assert_eq!(
            site_url("lbry://@lbry#3f/odysee#7a").unwrap().as_str(),
            "https://odysee.com/@lbry:3f/odysee:7a"
        );
    }

    #[tokio::test]
    async fn streams_come_through_the_api() {
        let mut fixture = Fixture::new("odysee", None);
        fixture.exchanges.push(post(json!({"jsonrpc": "2.0", "result": {"lbry://@lbry#3f/odysee#7a": claim()}, "id": 1})));
        fixture.exchanges.push(post(json!({"jsonrpc": "2.0", "result": {"streaming_url": "https://player.odycdn.com/v6/streams/7a416c44a6888d94fe045241bbac055c726332aa/a27e60.mp4"}, "id": 1})));
        let resolver = OdyseeResolver::new(Http::replay(fixture));
        let url = Url::parse("https://odysee.com/@lbry:3f/odysee:7a?t=20").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("7a416c44a6888d94fe045241bbac055c726332aa"));
        assert_eq!(resolved.title.as_deref(), Some("Introducing Odysee: A Short Video"));
        assert_eq!(resolved.uploader.as_deref(), Some("LBRY"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://odysee.com/@lbry:3f"
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(143)));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.clip.unwrap().start, Duration::from_secs(20));
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].size, Some(44532814));
        assert_eq!(resolved.variants[0].height, Some(1080));
        assert_eq!(resolved.variants[0].container, Some(Container::Mp4));
    }

    #[tokio::test]
    async fn channels_list_their_videos_and_missing_claims_are_missing() {
        let mut fixture = Fixture::new("odysee", None);
        let channel = json!({"claim_id": "3fda836a92faaceedfe398225fb9b2ee2ed1f01a", "name": "@lbry", "value_type": "channel", "value": {"title": "LBRY"}});
        fixture.exchanges.push(post(json!({"result": {"lbry://@lbry#3f": channel}})));
        fixture.exchanges.push(post(json!({"result": {"total_items": 2, "items": [
            {"name": "odysee", "claim_id": "7a", "canonical_url": "lbry://@lbry#3f/odysee#7a", "value": {"title": "Introducing Odysee", "video": {"duration": 143}}},
            {"name": "why", "claim_id": "97", "canonical_url": "lbry://@lbry#3f/odyseewhatandwhy#9", "value": {"title": "What is Odysee?", "video": {"duration": 209}}}
        ]}})));
        fixture.exchanges.push(post(json!({"result": {"lbry://@nobodyxyz#1/nothing#2": {"error": {"name": "NOT_FOUND", "text": "Could not find channel in \"lbry://@nobodyxyz#1/nothing#2\"."}}}})));
        let resolver = OdyseeResolver::new(Http::replay(fixture));
        let playlist = match resolver
            .resolve(&Url::parse("https://odysee.com/@lbry:3f").unwrap())
            .await
            .unwrap()
        {
            Resolution::Playlist(playlist) => playlist,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(playlist.title.as_deref(), Some("LBRY"));
        assert_eq!(playlist.total, Some(2));
        assert_eq!(
            playlist.entries[1].url.as_str(),
            "https://odysee.com/@lbry:3f/odyseewhatandwhy:9"
        );
        assert_eq!(playlist.entries[1].duration, Some(Duration::from_secs(209)));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://odysee.com/@nobodyxyz:1/nothing:2").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
