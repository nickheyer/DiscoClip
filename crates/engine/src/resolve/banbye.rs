//! BanBye videos, channels and playlists through the JSON API behind the site

use std::collections::HashSet;
use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, Tag, Variant, VariantKind, clean_title, fetch_ok, hls, path_extension, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "banbye";
const SITE: &str = "https://banbye.com";
const API: &str = "https://banbye.com/api";
/// The most items the API hands out per listing page
const PAGE_SIZE: usize = 50;
/// Channel and playlist entries stop here
const MAX_ENTRIES: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A watch or embed link, with the playlist it plays in when playlistId is set
    Video {
        id: String,
        playlist: Option<String>,
    },
    /// A channel id or slug link, with one of its playlists when playlist is set
    Channel {
        id: String,
        playlist: Option<String>,
    },
}

static RE_VIDEO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(?:en/)?(?:watch|embed)/([\w-]+)").unwrap());
static RE_CHANNEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(?:en/)?(?:channel|c)/([\w-]+)").unwrap());

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "banbye.com" && host != "www.banbye.com" {
        return None;
    }
    if let Some(caps) = RE_VIDEO.captures(url.path()) {
        return Some(Link::Video {
            id: caps[1].to_string(),
            playlist: util::query_param(url, "playlistId"),
        });
    }
    if let Some(caps) = RE_CHANNEL.captures(url.path()) {
        return Some(Link::Channel {
            id: caps[1].to_string(),
            playlist: util::query_param(url, "playlist"),
        });
    }
    None
}

/// The watch link of a video id
fn watch_url(id: &str) -> Option<Url> {
    Url::parse(&format!("{SITE}/watch/{id}")).ok()
}

/// The channel link the site uses, by slug when it has one
fn channel_url(channel: &Value) -> Option<Url> {
    let path = match (channel["slug"].as_str(), channel["id"].as_str()) {
        (Some(slug), _) => format!("/c/{slug}"),
        (None, Some(id)) => format!("/channel/{id}"),
        (None, None) => return None,
    };
    Url::parse(&format!("{SITE}{path}")).ok()
}

/// The widest image a src and srcSet pair names
fn largest_image(image: &Value) -> Option<Url> {
    let widest = image["srcSet"]
        .as_str()
        .into_iter()
        .flat_map(|set| set.split(','))
        .filter_map(|entry| {
            let mut parts = entry.split_whitespace();
            let link = parts.next()?;
            let width: u32 = parts.next()?.strip_suffix('w')?.parse().ok()?;
            Some((width, link))
        })
        .max_by_key(|(width, _)| *width)
        .and_then(|(_, link)| Url::parse(link).ok());
    widest.or_else(|| util::url_of(&image["src"], None))
}

/// An HLS variant for a playlist link and an MP4 file variant for anything else
fn stream_variant(url: Url) -> Variant {
    let extension = path_extension(&url).unwrap_or_default();
    if extension == "m3u8" {
        return Variant::hls(url);
    }
    let mut variant = Variant::file(url);
    variant.container = Container::from_extension(&extension).or(Some(Container::Mp4));
    variant.video = Some(VideoCodec::H264);
    variant.audio = Some(AudioCodec::Aac);
    variant
}

/// The variants a playback block's qualities name, one per short side and link
pub fn quality_variants(playback: &Value) -> Vec<Variant> {
    let mut variants = Vec::new();
    for quality in playback["qualities"].as_array().into_iter().flatten() {
        let height = util::u32_of(&quality["shortSide"]);
        for link in quality["urls"].as_array().into_iter().flatten() {
            let Some(url) = util::url_of(link, None) else {
                continue;
            };
            let mut variant = stream_variant(url);
            variant.height = height;
            variant.label = quality["label"]
                .as_str()
                .map(str::to_string)
                .or_else(|| height.map(|h| format!("{h}p")));
            variant.format_id = quality["name"].as_str().map(|name| match variant.kind {
                VariantKind::Hls => format!("hls-{name}"),
                _ => name.to_string(),
            });
            variants.push(variant);
        }
    }
    variants
}

/// A listing item as a playlist entry
fn entry_of(item: &Value) -> Option<PlaylistEntry> {
    let url = item["id"].as_str().and_then(watch_url)?;
    Some(PlaylistEntry {
        url,
        title: item["title"].as_str().and_then(clean_title),
        duration: util::seconds(&item["durationSeconds"]),
    })
}

pub struct BanByeResolver {
    http: Http,
}

impl BanByeResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    async fn api(&self, path: &str, origin: &Url) -> Result<Value, ResolveError> {
        let api = Url::parse(&format!("{API}{path}")).expect("valid");
        let fetched = fetch_ok(&self.http, &api, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        fetched.json(origin)
    }

    /// Every page of a listing that takes limit and offset, with the first page's JSON
    async fn paged(
        &self,
        path: &str,
        origin: &Url,
    ) -> Result<(Value, Vec<PlaylistEntry>), ResolveError> {
        let separator = if path.contains('?') { '&' } else { '?' };
        let mut entries = Vec::new();
        let mut offset = 0;
        let mut first = None;
        loop {
            let page = self
                .api(
                    &format!("{path}{separator}limit={PAGE_SIZE}&offset={offset}"),
                    origin,
                )
                .await?;
            let items = page["items"].as_array().map_or(0, Vec::len);
            let more = page["hasMore"].as_bool().unwrap_or(false) && items > 0;
            offset += items;
            entries.extend(
                page["items"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(entry_of),
            );
            if first.is_none() {
                first = Some(page);
            }
            if !more || entries.len() >= MAX_ENTRIES {
                break;
            }
        }
        entries.truncate(MAX_ENTRIES);
        Ok((first.expect("at least one page"), entries))
    }

    async fn playlist(&self, id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let (first, entries) = self.paged(&format!("/playlists/{id}"), origin).await?;
        if entries.is_empty() {
            return Err(ResolveError::unavailable(origin, "the playlist is empty"));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(id.to_string()),
            title: first["name"].as_str().and_then(clean_title),
            entries,
            total: None,
        }))
    }

    async fn video(&self, id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let data = self.api(&format!("/videos/{id}"), origin).await?;
        let playback = &data["playback"];
        if let Some(blocker) = playback["blocker"].as_str() {
            let reason = match blocker {
                "preparing" => "the video is still being prepared".to_string(),
                "unavailable" => "the video's streams are unavailable".to_string(),
                other => format!("the site blocks playback: {other}"),
            };
            return Err(ResolveError::unavailable(origin, reason));
        }
        let mut variants = quality_variants(playback);
        let mut expanded_duration = None;
        let mut master_error = None;
        for link in playback["urls"].as_array().into_iter().flatten() {
            let Some(url) = util::url_of(link, None) else {
                continue;
            };
            let variant = stream_variant(url);
            if variant.kind != VariantKind::Hls {
                variants.push(variant);
                continue;
            }
            match hls::expand(&self.http, &variant.url, PLATFORM, BROWSER_UA, &[]).await {
                Ok(expanded) => {
                    expanded_duration = expanded.duration;
                    variants.extend(expanded.variants);
                }
                Err(ResolveError::NotFound(_)) => {
                    master_error = Some(ResolveError::unavailable(
                        origin,
                        "the HLS master playlist is missing",
                    ));
                }
                Err(error) => master_error = Some(error.at(origin)),
            }
        }
        let mut seen = HashSet::new();
        variants.retain(|v| seen.insert(v.url.to_string()));
        let live_state = data["live"]["state"].as_str();
        if variants.is_empty() {
            return Err(match (master_error, live_state) {
                (Some(error), _) => error,
                (None, Some("scheduled")) => {
                    ResolveError::unavailable(origin, "the live stream has not started yet")
                }
                (None, _) => {
                    ResolveError::unavailable(origin, "the API lists no streams for the video")
                }
            });
        }
        if let Some(error) = master_error {
            tracing::debug!(%error, "master playlist skipped beside other streams");
        }
        let restrictions = &data["restrictions"];
        let minimum_age = util::uint(&restrictions["minimumAge"])
            .filter(|a| *a > 0)
            .map(|a| a.min(21) as u8);
        let adults_only = restrictions["adultsOnly"]
            .as_bool()
            .unwrap_or(false)
            .then_some(18u8);
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.to_string());
        resolved.title = data["title"].as_str().and_then(clean_title);
        resolved.description = data["description"].as_str().and_then(clean_title);
        resolved.uploader = data["channel"]["name"].as_str().and_then(clean_title);
        resolved.uploader_url = channel_url(&data["channel"]);
        resolved.uploaded_at = util::time(&data["publishedAt"]);
        resolved.duration = util::seconds(&data["durationSeconds"]).or(expanded_duration);
        resolved.thumbnail = largest_image(&data["thumbnail"]);
        resolved.webpage_url = watch_url(id);
        resolved.live = live_state == Some("live");
        resolved.age_limit = minimum_age.max(adults_only);
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn channel(&self, key: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let channel = self.api(&format!("/channels/{key}"), origin).await?;
        let id = channel["id"]
            .as_str()
            .ok_or_else(|| ResolveError::malformed(origin, "the channel reports no id"))?;
        let video_count = util::uint(&channel["videoCount"])
            .ok_or_else(|| ResolveError::malformed(origin, "the channel reports no video count"))?
            as usize;
        let (_, entries) = self
            .paged(&format!("/videos?channelId={id}&sort=new"), origin)
            .await?;
        if entries.is_empty() {
            return Err(ResolveError::unavailable(
                origin,
                "the channel has no videos",
            ));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(id.to_string()),
            title: channel["name"].as_str().and_then(clean_title),
            total: (video_count > entries.len()).then_some(video_count),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for BanByeResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "BanBye",
            hosts: &["banbye.com"],
            features: &["videos", "channels", "playlists"],
            formats: &["mp4", "hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::Video],
            session: SessionSupport::None,
            on_by_default: true,
            examples: &[
                "https://banbye.com/watch/v_ytfmvkVYLE8T",
                "https://banbye.com/watch/v_4fGN1gP_RKM8",
                "https://banbye.com/embed/v_ytfmvkVYLE8T",
                "https://banbye.com/watch/v_2JjQtqjKUE_F?playlistId=p_Ld82N6gBw_OJ",
                "https://banbye.com/channel/ch_wrealu24",
                "https://banbye.com/c/wRealu24",
                "https://banbye.com/channel/ch_wrealu24?playlist=p_Ld82N6gBw_OJ",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Video {
                playlist: Some(playlist),
                ..
            }
            | Link::Channel {
                playlist: Some(playlist),
                ..
            } => self.playlist(&playlist, url).await,
            Link::Video { id, playlist: None } => self.video(&id, url).await,
            Link::Channel { id, playlist: None } => self.channel(&id, url).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};
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

    fn json_get(url: &str, body: Value) -> Exchange {
        get(url, 200, "application/json", body.to_string())
    }

    fn image(path: &str) -> Value {
        json!({
            "src": format!("https://media.banbye.net/{path}/480.webp"),
            "srcSet": format!(
                "https://media.banbye.net/{path}/144.webp 256w, https://media.banbye.net/{path}/480.webp 854w, https://media.banbye.net/{path}/1080.webp 1920w"
            )
        })
    }

    fn channel() -> Value {
        json!({
            "id": "ch_wrealu24", "name": "wRealu24", "slug": "wRealu24",
            "avatar": image("channel/ch_wrealu24"), "subscriptionsCount": 26611
        })
    }

    fn video_data(id: &str, playback: Value) -> Value {
        json!({
            "id": id, "title": "Chiny skupują zboże! Czy czeka nas głód? C. Wincenciak",
            "publishedAt": "2022-03-18T12:00:00.403Z", "durationSeconds": 1931, "views": 7599,
            "thumbnail": image(&format!("video/{id}")), "channel": channel(), "live": null,
            "premiere": null, "restrictions": {"adultsOnly": false, "minimumAge": null},
            "moderation": {"level": "none", "reason": null}, "status": "public",
            "description": "Telewizja wRealu24 potrzebuje Twojej pomocy!", "tags": ["Chiny"],
            "categories": [], "likes": 210, "dislikes": 1, "commentCount": 36,
            "playback": playback, "chat": null
        })
    }

    fn mp4_playback(id: &str) -> Value {
        let file = |height: u32| format!("https://media.banbye.net/video/{id}/{height}.mp4");
        json!({
            "urls": [file(480)], "format": "mp4",
            "qualities": [
                {"name": "480", "label": "480p", "shortSide": 480, "urls": [file(480)]},
                {"name": "144", "label": "144p", "shortSide": 144, "urls": [file(144)]}
            ],
            "blocker": null, "seeksprite": format!("https://media.banbye.net/video/{id}/seeksprite.jpg"),
            "incomplete": false
        })
    }

    fn hls_playback(id: &str) -> Value {
        json!({
            "urls": [format!("https://media.banbye.net/video/{id}/live/g1/master.m3u8")],
            "format": "hls", "qualities": null, "blocker": null,
            "seeksprite": format!("https://media.banbye.net/video/{id}/seeksprite.jpg"),
            "incomplete": false
        })
    }

    fn listing(items: Vec<Value>, has_more: bool) -> Value {
        json!({"items": items, "hasMore": has_more})
    }

    fn item(id: &str, title: &str, duration: f64) -> Value {
        json!({
            "id": id, "title": title, "publishedAt": "2022-05-29T15:30:00.809Z",
            "durationSeconds": duration, "views": 1405, "thumbnail": image(&format!("video/{id}")),
            "channel": channel(), "live": null, "premiere": null,
            "restrictions": {"adultsOnly": false, "minimumAge": null},
            "moderation": {"level": "none", "reason": null}
        })
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://banbye.com/watch/v_ytfmvkVYLE8T"),
            Some(Link::Video {
                id: "v_ytfmvkVYLE8T".into(),
                playlist: None
            })
        );
        assert_eq!(
            link("https://banbye.com/embed/v_ytfmvkVYLE8T"),
            Some(Link::Video {
                id: "v_ytfmvkVYLE8T".into(),
                playlist: None
            })
        );
        assert_eq!(
            link("https://banbye.com/watch/v_2JjQtqjKUE_F?playlistId=p_Ld82N6gBw_OJ"),
            Some(Link::Video {
                id: "v_2JjQtqjKUE_F".into(),
                playlist: Some("p_Ld82N6gBw_OJ".into())
            })
        );
        assert_eq!(
            link("https://www.banbye.com/en/watch/v_kb6_o1Kyq-CD"),
            Some(Link::Video {
                id: "v_kb6_o1Kyq-CD".into(),
                playlist: None
            })
        );
        assert_eq!(
            link("https://banbye.com/channel/ch_wrealu24"),
            Some(Link::Channel {
                id: "ch_wrealu24".into(),
                playlist: None
            })
        );
        assert_eq!(
            link("https://banbye.com/c/wRealu24"),
            Some(Link::Channel {
                id: "wRealu24".into(),
                playlist: None
            })
        );
        assert_eq!(
            link("https://banbye.com/en/channel/ch_wrealu24?playlist=p_Ld82N6gBw_OJ"),
            Some(Link::Channel {
                id: "ch_wrealu24".into(),
                playlist: Some("p_Ld82N6gBw_OJ".into())
            })
        );
        assert_eq!(link("https://banbye.com/"), None);
        assert_eq!(link("https://banbye.com/watch/"), None);
        assert_eq!(link("https://banbye.com/playlist/p_Ld82N6gBw_OJ"), None);
        assert_eq!(link("https://example.com/watch/v_ytfmvkVYLE8T"), None);
    }

    #[tokio::test]
    async fn videos_resolve_with_mp4_qualities() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(json_get(
            "https://banbye.com/api/videos/v_ytfmvkVYLE8T",
            video_data("v_ytfmvkVYLE8T", mp4_playback("v_ytfmvkVYLE8T")),
        ));
        let resolver = BanByeResolver::new(Http::replay(fixture));
        let url = Url::parse("https://banbye.com/watch/v_ytfmvkVYLE8T").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("v_ytfmvkVYLE8T"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Chiny skupują zboże! Czy czeka nas głód? C. Wincenciak")
        );
        assert_eq!(
            resolved.description.as_deref(),
            Some("Telewizja wRealu24 potrzebuje Twojej pomocy!")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("wRealu24"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://banbye.com/c/wRealu24"
        );
        assert_eq!(resolved.uploaded_at.unwrap().as_second(), 1647604800);
        assert_eq!(
            resolved.duration,
            Some(std::time::Duration::from_secs(1931))
        );
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://media.banbye.net/video/v_ytfmvkVYLE8T/1080.webp"
        );
        assert_eq!(
            resolved.webpage_url.as_ref().unwrap().as_str(),
            "https://banbye.com/watch/v_ytfmvkVYLE8T"
        );
        assert_eq!(resolved.age_limit, None);
        assert!(!resolved.live);
        // The default stream and the 480 quality share a link so each height appears once
        assert_eq!(resolved.variants.len(), 2);
        let heights: Vec<Option<u32>> = resolved.variants.iter().map(|v| v.height).collect();
        assert_eq!(heights, vec![Some(480), Some(144)]);
        let best = &resolved.variants[0];
        assert_eq!(best.kind, VariantKind::File);
        assert_eq!(best.container, Some(Container::Mp4));
        assert_eq!(best.format_id.as_deref(), Some("480"));
        assert_eq!(best.label.as_deref(), Some("480p"));
        assert_eq!(
            best.url.as_str(),
            "https://media.banbye.net/video/v_ytfmvkVYLE8T/480.mp4"
        );
    }

    #[tokio::test]
    async fn videos_resolve_with_hls_master() {
        let mut data = video_data("v_6nfOuKFlQVsE", hls_playback("v_6nfOuKFlQVsE"));
        data["live"] = json!({
            "state": "ended", "startsAt": "2026-10-08T18:00:31.679Z",
            "endedAt": "2026-10-08T18:35:04.357Z", "interrupted": false
        });
        data["restrictions"] = json!({"adultsOnly": true, "minimumAge": 12});
        data["durationSeconds"] = json!(2065.2);
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(json_get(
            "https://banbye.com/api/videos/v_6nfOuKFlQVsE",
            data,
        ));
        fixture.exchanges.push(get(
            "https://media.banbye.net/video/v_6nfOuKFlQVsE/live/g1/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=3515600,RESOLUTION=1280x720,CODECS=\"avc1.64001f,mp4a.40.2\"\nindex_720.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=910800,RESOLUTION=640x360,CODECS=\"avc1.64001e,mp4a.40.2\"\nindex_360.m3u8\n".into(),
        ));
        for height in [720, 360] {
            fixture.exchanges.push(get(
                &format!(
                    "https://media.banbye.net/video/v_6nfOuKFlQVsE/live/g1/index_{height}.m3u8"
                ),
                200,
                "application/vnd.apple.mpegurl",
                "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\n0.ts\n#EXT-X-ENDLIST\n".into(),
            ));
        }
        let resolver = BanByeResolver::new(Http::replay(fixture));
        let url = Url::parse("https://banbye.com/watch/v_6nfOuKFlQVsE").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.variants.len(), 2);
        assert!(resolved.variants.iter().all(|v| v.kind == VariantKind::Hls));
        assert_eq!(resolved.variants[0].height, Some(720));
        assert_eq!(resolved.variants[1].height, Some(360));
        assert_eq!(
            resolved.variants[1].url.as_str(),
            "https://media.banbye.net/video/v_6nfOuKFlQVsE/live/g1/index_360.m3u8"
        );
        assert!(!resolved.live);
        assert_eq!(resolved.age_limit, Some(18));
        assert_eq!(
            resolved.duration,
            Some(std::time::Duration::from_secs_f64(2065.2))
        );

        // A missing master playlist leaves nothing to play
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(json_get(
            "https://banbye.com/api/videos/v_6nfOuKFlQVsE",
            video_data("v_6nfOuKFlQVsE", hls_playback("v_6nfOuKFlQVsE")),
        ));
        fixture.exchanges.push(get(
            "https://media.banbye.net/video/v_6nfOuKFlQVsE/live/g1/master.m3u8",
            404,
            "text/plain",
            "".into(),
        ));
        let resolver = BanByeResolver::new(Http::replay(fixture));
        let error = resolver.resolve(&url).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { url: at, reason } if at == &url && reason.contains("master playlist")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn live_streams_are_flagged() {
        let mut data = video_data("v_live", hls_playback("v_live"));
        data["live"] = json!({"state": "live", "startsAt": "2026-10-08T18:00:31.679Z", "endedAt": null, "interrupted": false});
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture
            .exchanges
            .push(json_get("https://banbye.com/api/videos/v_live", data));
        fixture.exchanges.push(get(
            "https://media.banbye.net/video/v_live/live/g1/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=3515600,RESOLUTION=1280x720\nindex_720.m3u8\n"
                .into(),
        ));
        fixture.exchanges.push(get(
            "https://media.banbye.net/video/v_live/live/g1/index_720.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:10\n#EXTINF:6.0,\n10.ts\n"
                .into(),
        ));
        let resolver = BanByeResolver::new(Http::replay(fixture));
        let url = Url::parse("https://banbye.com/watch/v_live").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert!(resolved.live);
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].height, Some(720));
    }

    #[tokio::test]
    async fn playlists_page_through_their_videos() {
        let page = |items: Vec<Value>, more: bool| json!({"id": "p_Ld82N6gBw_OJ", "name": "Krzysztof Karoń", "channel": channel(), "items": items, "hasMore": more});
        let mut fixture = Fixture::new(PLATFORM, None);
        for _ in 0..2 {
            fixture.exchanges.push(json_get(
                "https://banbye.com/api/playlists/p_Ld82N6gBw_OJ?limit=50&offset=0",
                page(
                    vec![
                        item("v_Vwhzy-jD50_0", "Dwa marksizmy", 4095.0),
                        item("v_acAkN8WR_2e8", "Nauka a wiara", 100.0),
                    ],
                    true,
                ),
            ));
            fixture.exchanges.push(json_get(
                "https://banbye.com/api/playlists/p_Ld82N6gBw_OJ?limit=50&offset=2",
                page(vec![item("v_2JjQtqjKUE_F", "Trzeci", 5.5)], false),
            ));
        }
        let resolver = BanByeResolver::new(Http::replay(fixture));
        for link in [
            "https://banbye.com/watch/v_2JjQtqjKUE_F?playlistId=p_Ld82N6gBw_OJ",
            "https://banbye.com/channel/ch_wrealu24?playlist=p_Ld82N6gBw_OJ",
        ] {
            let url = Url::parse(link).unwrap();
            let Resolution::Playlist(playlist) = resolver.resolve(&url).await.unwrap() else {
                panic!("{link} is a playlist");
            };
            assert_eq!(playlist.id.as_deref(), Some("p_Ld82N6gBw_OJ"));
            assert_eq!(playlist.title.as_deref(), Some("Krzysztof Karoń"));
            assert_eq!(playlist.total, None);
            assert_eq!(playlist.entries.len(), 3);
            assert_eq!(playlist.entries[0].title.as_deref(), Some("Dwa marksizmy"));
            assert_eq!(
                playlist.entries[0].duration,
                Some(std::time::Duration::from_secs(4095))
            );
            assert_eq!(
                playlist.entries[2].url.as_str(),
                "https://banbye.com/watch/v_2JjQtqjKUE_F"
            );
        }
    }

    #[tokio::test]
    async fn channels_page_through_their_videos() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(json_get(
            "https://banbye.com/api/channels/wRealu24",
            json!({
                "id": "ch_wrealu24", "name": "wRealu24", "slug": "wRealu24", "description": "Redakcja",
                "tags": ["news"], "promoVideoId": null, "videoCount": 7451, "subscriptionsCount": 26611,
                "postsCount": 977, "createdAt": "2021-05-14T16:14:54.642Z", "links": {}
            }),
        ));
        fixture.exchanges.push(json_get(
            "https://banbye.com/api/videos?channelId=ch_wrealu24&sort=new&limit=50&offset=0",
            listing(
                vec![
                    item("v_FtW18Q1bMAR4", "Pierwszy", 2749.52),
                    item("v_IajbOIvQmsjf", "Drugi", 100.0),
                ],
                true,
            ),
        ));
        fixture.exchanges.push(json_get(
            "https://banbye.com/api/videos?channelId=ch_wrealu24&sort=new&limit=50&offset=2",
            listing(vec![item("v_third", "Trzeci", 5.0)], false),
        ));
        let resolver = BanByeResolver::new(Http::replay(fixture));
        let url = Url::parse("https://banbye.com/c/wRealu24").unwrap();
        let Resolution::Playlist(playlist) = resolver.resolve(&url).await.unwrap() else {
            panic!("a channel is a playlist");
        };
        assert_eq!(playlist.id.as_deref(), Some("ch_wrealu24"));
        assert_eq!(playlist.title.as_deref(), Some("wRealu24"));
        assert_eq!(playlist.total, Some(7451));
        assert_eq!(playlist.entries.len(), 3);
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://banbye.com/watch/v_FtW18Q1bMAR4"
        );
        assert_eq!(playlist.entries[0].title.as_deref(), Some("Pierwszy"));
        assert_eq!(
            playlist.entries[0].duration,
            Some(std::time::Duration::from_secs_f64(2749.52))
        );
        assert_eq!(
            playlist.entries[2].url.as_str(),
            "https://banbye.com/watch/v_third"
        );
    }

    #[tokio::test]
    async fn missing_blocked_and_unstarted_videos_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://banbye.com/api/videos/v_gone",
            404,
            "application/json",
            json!({"error": "not_found"}).to_string(),
        ));
        let preparing = json!({
            "urls": [], "format": null, "qualities": null, "blocker": "preparing",
            "seeksprite": "https://media.banbye.net/video/v_preparing/seeksprite.jpg", "incomplete": false
        });
        fixture.exchanges.push(json_get(
            "https://banbye.com/api/videos/v_preparing",
            video_data("v_preparing", preparing),
        ));
        let mut scheduled = video_data(
            "v_scheduled",
            json!({"urls": [], "format": null, "qualities": null, "blocker": null, "seeksprite": null, "incomplete": false}),
        );
        scheduled["live"] = json!({"state": "scheduled", "startsAt": "2026-10-09T18:00:00.000Z", "endedAt": null, "interrupted": false});
        fixture.exchanges.push(json_get(
            "https://banbye.com/api/videos/v_scheduled",
            scheduled,
        ));
        let resolver = BanByeResolver::new(Http::replay(fixture));
        let gone = Url::parse("https://banbye.com/watch/v_gone").unwrap();
        assert!(matches!(
            resolver.resolve(&gone).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
        for (id, expected) in [
            ("v_preparing", "being prepared"),
            ("v_scheduled", "not started"),
        ] {
            let url = Url::parse(&format!("https://banbye.com/watch/{id}")).unwrap();
            let error = resolver.resolve(&url).await.unwrap_err();
            assert!(
                matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains(expected)),
                "{id}: {error}"
            );
        }
    }

    /// Every example link resolves live with a playable stream or listing entries
    #[ignore = "reaches the live site: cargo test -- --ignored"]
    #[tokio::test]
    async fn live_examples_resolve() {
        use std::time::Duration;

        let resolver = BanByeResolver::new(Http::new(crate::http::HttpConfig::default()));
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
            assert!(resolver.matches(&url), "{link}");
            let resolution = tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&url))
                .await
                .expect("resolution timed out")
                .unwrap_or_else(|e| panic!("{link}: {e}"));
            match resolution {
                Resolution::Media(resolved) => {
                    assert!(
                        resolved.variants.iter().any(|v| v.is_playable()),
                        "{link}: no playable stream"
                    );
                    println!(
                        "{link}: {:?} with {} variants",
                        resolved.title,
                        resolved.variants.len()
                    );
                }
                Resolution::Playlist(playlist) => {
                    assert!(!playlist.entries.is_empty(), "{link}: no entries");
                    println!(
                        "{link}: {:?} with {} entries",
                        playlist.title,
                        playlist.entries.len()
                    );
                }
            }
        }
    }
}
