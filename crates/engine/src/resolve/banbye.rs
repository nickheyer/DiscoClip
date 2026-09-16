//! BanBye videos, channels and playlists, through the JSON API the site reads: a video's
//! streams come from the `url` endpoint as an HLS master playlist and per-height MP4 or
//! HLS links.

use std::collections::HashSet;
use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, Variant, check_status, clean_title, fetch_ok, hls, path_extension, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "banbye";
const API: &str = "https://api.banbye.com";
const CDN: &str = "https://cdn.banbye.com";
const VIDEO_BASE: &str = "https://banbye.com/watch";
/// Videos per channel page, as the site asks for them.
const PAGE_SIZE: usize = 100;
/// Channel entries stop here.
const MAX_ENTRIES: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// `/watch/{id}`; with `?playlistId=` the playlist the video plays in.
    Video {
        id: String,
        playlist: Option<String>,
    },
    /// `/channel/{id}`; with `?playlist=` one of the channel's playlists.
    Channel {
        id: String,
        playlist: Option<String>,
    },
}

static RE_VIDEO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(?:en/)?watch/([\w-]+)").unwrap());
static RE_CHANNEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(?:en/)?channel/(\w+)").unwrap());

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

/// The watch link of a video id.
fn watch_url(id: &str) -> Option<Url> {
    Url::parse(&format!("{VIDEO_BASE}/{id}")).ok()
}

/// The variants a video's `src` block names: `mp4.levels` and `hls.levels` map a height
/// to a file or a media playlist.
pub fn level_variants(src: &Value) -> Vec<Variant> {
    let mut variants = Vec::new();
    for kind in ["mp4", "hls"] {
        for (level, link) in src[kind]["levels"].as_object().into_iter().flatten() {
            let Some(url) = util::url_of(link, None) else {
                continue;
            };
            let extension = path_extension(&url).unwrap_or_default();
            let height = level.parse::<u32>().ok();
            let mut variant = if extension == "m3u8" {
                let mut v = Variant::hls(url);
                v.format_id = Some(format!("hls-{level}"));
                v
            } else {
                let mut v = Variant::file(url);
                v.container = Container::from_extension(&extension).or(Some(Container::Mp4));
                v.video = Some(VideoCodec::H264);
                v.audio = Some(AudioCodec::Aac);
                v.format_id = Some(level.clone());
                v
            };
            variant.height = height;
            variant.label = height.map(|h| format!("{h}p"));
            variants.push(variant);
        }
    }
    variants
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

    /// The `url` endpoint wants an empty POST.
    async fn stream_sources(&self, id: &str, origin: &Url) -> Result<Value, ResolveError> {
        let api = Url::parse(&format!("{API}/videos/{id}/url")).expect("valid");
        let response = self
            .http
            .post(api.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .body(Vec::new())
            .send()
            .await?;
        check_status(&response, origin)?;
        response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, format!("stream JSON: {e}")))
    }

    async fn playlist(&self, id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let data = self.api(&format!("/playlists/{id}"), origin).await?;
        let entries: Vec<PlaylistEntry> = data["videoIds"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
            .filter_map(watch_url)
            .map(|url| PlaylistEntry {
                url,
                title: None,
                duration: None,
            })
            .collect();
        if entries.is_empty() {
            return Err(ResolveError::unavailable(origin, "the playlist is empty"));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(id.to_string()),
            title: data["name"].as_str().and_then(clean_title),
            entries,
            total: None,
        }))
    }

    async fn video(&self, id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let data = self.api(&format!("/videos/{id}"), origin).await?;
        let sources = self.stream_sources(id, origin).await?;
        let mut variants = Vec::new();
        let mut expanded_duration = None;
        if let Some(master) = util::url_of(&sources["src"]["hls"]["masterPlaylist"], None) {
            // The master playlist is missing for some videos whose per-height playlists exist.
            match hls::expand(&self.http, &master, PLATFORM, BROWSER_UA, &[]).await {
                Ok(expanded) => {
                    expanded_duration = expanded.duration;
                    variants.extend(expanded.variants);
                }
                Err(error) => tracing::debug!(%master, %error, "master playlist unavailable"),
            }
        }
        variants.extend(level_variants(&sources["src"]));
        let mut seen = HashSet::new();
        variants.retain(|v| seen.insert(v.url.to_string()));
        if variants.is_empty() {
            return Err(ResolveError::unavailable(
                origin,
                "the API lists no streams for the video",
            ));
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.to_string());
        resolved.title = data["title"].as_str().and_then(clean_title);
        resolved.description = data["desc"].as_str().and_then(clean_title);
        resolved.uploader = data["channel"]["name"].as_str().and_then(clean_title);
        resolved.uploader_url = data["channelId"]
            .as_str()
            .and_then(|c| Url::parse(&format!("https://banbye.com/channel/{c}")).ok());
        resolved.uploaded_at = util::time(&data["publishedAt"]);
        resolved.duration = util::seconds(&data["duration"]).or(expanded_duration);
        resolved.thumbnail = Url::parse(&format!("{CDN}/video/{id}/1080.webp")).ok();
        resolved.webpage_url = watch_url(id);
        resolved.live = data["isLiveNow"].as_bool().unwrap_or(false);
        resolved.age_limit = util::uint(&data["ageRestriction"])
            .filter(|a| *a > 0)
            .map(|a| a.min(21) as u8);
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn channel(&self, id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let channel = self.api(&format!("/channels/{id}"), origin).await?;
        let video_count = util::uint(&channel["videoCount"])
            .ok_or_else(|| ResolveError::malformed(origin, "the channel reports no video count"))?
            as usize;
        let pages = video_count.div_ceil(PAGE_SIZE);
        let mut entries = Vec::new();
        for page in 0..pages {
            if entries.len() >= MAX_ENTRIES {
                break;
            }
            let path = format!(
                "/videos?channelId={id}&sort=new&limit={PAGE_SIZE}&offset={}",
                page * PAGE_SIZE
            );
            let listing = self.api(&path, origin).await?;
            for item in listing["items"].as_array().into_iter().flatten() {
                let Some(url) = item["_id"].as_str().and_then(watch_url) else {
                    continue;
                };
                entries.push(PlaylistEntry {
                    url,
                    title: item["title"].as_str().and_then(clean_title),
                    duration: util::seconds(&item["duration"]),
                });
            }
        }
        entries.truncate(MAX_ENTRIES);
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
            session: SessionSupport::None,
            examples: &[
                "https://banbye.com/watch/v_ytfmvkVYLE8T",
                "https://banbye.com/watch/v_2JjQtqjKUE_F?playlistId=p_Ld82N6gBw_OJ",
                "https://banbye.com/watch/v_kb6_o1Kyq-CD",
                "https://banbye.com/watch/v_a_gPFuC9LoW5",
                "https://banbye.com/watch/v_B0rsKWsr-aaa",
                "https://banbye.com/channel/ch_wrealu24",
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
    use crate::resolve::VariantKind;
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

    fn get(url: &str, status: u16, content_type: &str, body: String) -> Exchange {
        exchange("GET", url, status, content_type, body)
    }

    fn video_data(id: &str) -> Value {
        json!({
            "_id": id, "channelId": "ch_QgWnHvDG2fo5", "desc": "Telewizja wRealu24 potrzebuje Twojej pomocy!",
            "likes": 314, "dislikes": 7, "publishedAt": "2023-07-06T11:24:16.338Z",
            "tags": ["Paryż", "Francja"], "title": "Co tak naprawdę dzieje się we Francji?!",
            "quality": [480, 144], "views": 3368, "duration": 597, "isLiveNow": false, "ageRestriction": 12,
            "channel": {"_id": "ch_QgWnHvDG2fo5", "name": "Marcin Rola - MOIM ZDANIEM!🇵🇱"},
            "commentCount": 3
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
    async fn videos_resolve_with_mp4_levels() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://api.banbye.com/videos/v_kb6_o1Kyq-CD",
            200,
            "application/json",
            video_data("v_kb6_o1Kyq-CD").to_string(),
        ));
        fixture.exchanges.push(exchange("POST", "https://api.banbye.com/videos/v_kb6_o1Kyq-CD/url", 200, "application/json", json!({
            "rid": "vw_1", "src": {"mp4": {"base": "https://tc2g.banbye.com/edge/video/v_kb6_o1Kyq-CD", "levels": {
                "144": "https://tc2g.banbye.com/edge/video/v_kb6_o1Kyq-CD/144.mp4",
                "480": "https://tc2g.banbye.com/edge/video/v_kb6_o1Kyq-CD/480.mp4"
            }}}
        }).to_string()));
        let resolver = BanByeResolver::new(Http::replay(fixture));
        let url = Url::parse("https://banbye.com/watch/v_kb6_o1Kyq-CD").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("v_kb6_o1Kyq-CD"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Co tak naprawdę dzieje się we Francji?!")
        );
        assert_eq!(
            resolved.uploader.as_deref(),
            Some("Marcin Rola - MOIM ZDANIEM!🇵🇱")
        );
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://banbye.com/channel/ch_QgWnHvDG2fo5"
        );
        assert_eq!(resolved.uploaded_at.unwrap().as_second(), 1688642656);
        assert_eq!(resolved.duration, Some(std::time::Duration::from_secs(597)));
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://cdn.banbye.com/video/v_kb6_o1Kyq-CD/1080.webp"
        );
        assert_eq!(resolved.age_limit, Some(12));
        assert!(!resolved.live);
        assert_eq!(resolved.variants.len(), 2);
        let heights: Vec<Option<u32>> = resolved.variants.iter().map(|v| v.height).collect();
        assert!(heights.contains(&Some(144)) && heights.contains(&Some(480)));
        let best = resolved
            .variants
            .iter()
            .find(|v| v.height == Some(480))
            .unwrap();
        assert_eq!(best.kind, VariantKind::File);
        assert_eq!(best.container, Some(Container::Mp4));
        assert_eq!(best.format_id.as_deref(), Some("480"));
        assert_eq!(best.label.as_deref(), Some("480p"));
        assert_eq!(
            best.url.as_str(),
            "https://tc2g.banbye.com/edge/video/v_kb6_o1Kyq-CD/480.mp4"
        );
    }

    #[tokio::test]
    async fn videos_resolve_with_hls_master_and_levels() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://api.banbye.com/videos/v_B0rsKWsr-aaa",
            200,
            "application/json",
            video_data("v_B0rsKWsr-aaa").to_string(),
        ));
        fixture.exchanges.push(exchange("POST", "https://api.banbye.com/videos/v_B0rsKWsr-aaa/url", 200, "application/json", json!({
            "src": {"hls": {
                "masterPlaylist": "https://tc2g.banbye.com/edge/video/v_B0rsKWsr-aaa/master.m3u8",
                "levels": {
                    "720": "https://tc2g.banbye.com/edge/video/v_B0rsKWsr-aaa/720/index.m3u8",
                    "360": "https://tc2g.banbye.com/edge/video/v_B0rsKWsr-aaa/360/index.m3u8"
                }
            }}
        }).to_string()));
        fixture.exchanges.push(get(
            "https://tc2g.banbye.com/edge/video/v_B0rsKWsr-aaa/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1280x720\nhttps://tc2g.banbye.com/edge/video/v_B0rsKWsr-aaa/720/index.m3u8\n".into(),
        ));
        fixture.exchanges.push(get(
            "https://tc2g.banbye.com/edge/video/v_B0rsKWsr-aaa/720/index.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\n0.ts\n#EXT-X-ENDLIST\n".into(),
        ));
        let resolver = BanByeResolver::new(Http::replay(fixture));
        let url = Url::parse("https://banbye.com/watch/v_B0rsKWsr-aaa").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        // The master's 720p stream and the 720 level share a link: one variant, plus 360.
        assert_eq!(resolved.variants.len(), 2);
        assert!(resolved.variants.iter().all(|v| v.kind == VariantKind::Hls));
        assert_eq!(resolved.variants[0].height, Some(720));
        assert_eq!(resolved.variants[1].height, Some(360));
        assert_eq!(resolved.variants[1].format_id.as_deref(), Some("hls-360"));

        // A missing master playlist leaves the per-height playlists.
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://api.banbye.com/videos/v_a_gPFuC9LoW5",
            200,
            "application/json",
            video_data("v_a_gPFuC9LoW5").to_string(),
        ));
        fixture.exchanges.push(exchange("POST", "https://api.banbye.com/videos/v_a_gPFuC9LoW5/url", 200, "application/json", json!({
            "src": {"hls": {
                "masterPlaylist": "https://tc2g.banbye.com/edge/video/v_a_gPFuC9LoW5/master.m3u8",
                "levels": {"480": "https://tc2g.banbye.com/edge/video/v_a_gPFuC9LoW5/480/index.m3u8"}
            }}
        }).to_string()));
        fixture.exchanges.push(get(
            "https://tc2g.banbye.com/edge/video/v_a_gPFuC9LoW5/master.m3u8",
            404,
            "text/plain",
            "".into(),
        ));
        let resolver = BanByeResolver::new(Http::replay(fixture));
        let url = Url::parse("https://banbye.com/watch/v_a_gPFuC9LoW5").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].height, Some(480));
        assert_eq!(resolved.variants[0].kind, VariantKind::Hls);
    }

    #[tokio::test]
    async fn playlists_list_their_videos() {
        let playlist = json!({
            "_id": "p_Ld82N6gBw_OJ", "channelId": "ch_wrealu24", "type": "channel", "name": "Krzysztof Karoń",
            "videoIds": ["v_Vwhzy-jD50_0", "v_acAkN8WR_2e8", "v_2JjQtqjKUE_F"], "visibility": "public"
        });
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://api.banbye.com/playlists/p_Ld82N6gBw_OJ",
            200,
            "application/json",
            playlist.to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.banbye.com/playlists/p_Ld82N6gBw_OJ",
            200,
            "application/json",
            playlist.to_string(),
        ));
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
            assert_eq!(playlist.entries.len(), 3);
            assert_eq!(
                playlist.entries[2].url.as_str(),
                "https://banbye.com/watch/v_2JjQtqjKUE_F"
            );
        }
    }

    #[tokio::test]
    async fn channels_page_through_their_videos() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://api.banbye.com/channels/ch_wrealu24",
            200,
            "application/json",
            json!({"_id": "ch_wrealu24", "name": "wRealu24", "description": "Redakcja", "videoCount": 150}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.banbye.com/videos?channelId=ch_wrealu24&sort=new&limit=100&offset=0",
            200,
            "application/json",
            json!({"items": [
                {"_id": "v_FtW18Q1bMAR4", "title": "Pierwszy", "duration": 2749.52},
                {"_id": "v_IajbOIvQmsjf", "title": "Drugi", "duration": 100}
            ]})
            .to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.banbye.com/videos?channelId=ch_wrealu24&sort=new&limit=100&offset=100",
            200,
            "application/json",
            json!({"items": [{"_id": "v_third", "title": "Trzeci", "duration": 5}]}).to_string(),
        ));
        let resolver = BanByeResolver::new(Http::replay(fixture));
        let url = Url::parse("https://banbye.com/channel/ch_wrealu24").unwrap();
        let Resolution::Playlist(playlist) = resolver.resolve(&url).await.unwrap() else {
            panic!("a channel is a playlist");
        };
        assert_eq!(playlist.id.as_deref(), Some("ch_wrealu24"));
        assert_eq!(playlist.title.as_deref(), Some("wRealu24"));
        assert_eq!(playlist.total, Some(150));
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
    async fn missing_videos_and_streamless_videos_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://api.banbye.com/videos/v_gone",
            404,
            "application/json",
            json!({"message": "Not Found"}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.banbye.com/videos/v_empty",
            200,
            "application/json",
            video_data("v_empty").to_string(),
        ));
        fixture.exchanges.push(exchange(
            "POST",
            "https://api.banbye.com/videos/v_empty/url",
            200,
            "application/json",
            json!({"src": {}}).to_string(),
        ));
        let resolver = BanByeResolver::new(Http::replay(fixture));
        let gone = Url::parse("https://banbye.com/watch/v_gone").unwrap();
        assert!(matches!(
            resolver.resolve(&gone).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let empty = Url::parse("https://banbye.com/watch/v_empty").unwrap();
        let error = resolver.resolve(&empty).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("no streams")),
            "{error}"
        );
    }
}
