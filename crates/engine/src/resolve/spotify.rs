//! Spotify podcasts: the embed page of an episode or a show carries the anonymous web
//! token the player uses and the episode's details, and the sound finder the player
//! asks with that token names the episode's audio: Spotify's own AAC in MP4, and the
//! podcast host's file when the show passes it through. A show's episodes are listed
//! through the player's GraphQL query, whose persisted hash is read from the web
//! player's script. Music is locked with Widevine and refused as such.

use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::{Value, json};
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, Tag, Variant, VariantKind, clean_title, essence, fetch, navigation_headers,
    status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind};

pub const PLATFORM: &str = "spotify";
const SITE: &str = "https://open.spotify.com";
const SCRIPT_CDN: &str = "https://open.spotifycdn.com/cdn/build/";
const SOUND_FINDER: &str = "https://spclient.wg.spotify.com/soundfinder/v1/unauth/episode/";
const PATHFINDER: &str = "https://api-partner.spotify.com/pathfinder/v2/query";
/// The player's query for a show's episodes, whose persisted hash the script carries.
const EPISODES_QUERY: &str = "queryPodcastEpisodes";
/// How many episodes a show listing is read up to, and how many one query page holds.
const LISTING_LIMIT: usize = 100;
const PAGE_SIZE: usize = 50;
/// How long a podcast host gets to answer for its file.
const HOST_PROBE: Duration = Duration::from_secs(10);

/// `/episode/{id}`, `/show/{id}` and the music links, behind an optional locale or
/// embed prefix.
static RE_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^/(?:intl-[a-z]{2}(?:-[a-z]+)?/)?(?:embed(?:-podcast)?/)?(episode|show|track|album|playlist|artist)/([0-9A-Za-z]{22})/?$",
    )
    .unwrap()
});
/// The page's Next.js data.
static RE_NEXT_DATA: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<script id="__NEXT_DATA__" type="application/json">(.*?)</script>"#).unwrap()
});
/// The web player's script, named by its build hash.
static RE_PLAYER_SCRIPT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(web-player/web-player\.[0-9a-f]+\.js)").unwrap());
/// `MP4_128`: the bitrate in a format name.
static RE_FORMAT_BITRATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"_(\d{2,3})").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Episode {
        id: String,
    },
    Show {
        id: String,
    },
    /// A track, album, playlist or artist: music, which is locked.
    Music {
        kind: String,
        id: String,
    },
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "open.spotify.com" {
        return None;
    }
    let caps = RE_PATH.captures(url.path())?;
    let id = caps[2].to_string();
    Some(match &caps[1] {
        "episode" => Link::Episode { id },
        "show" => Link::Show { id },
        kind => Link::Music {
            kind: kind.to_string(),
            id,
        },
    })
}

/// The Next.js data of an embed page.
pub fn next_data(html: &str) -> Option<Value> {
    let caps = RE_NEXT_DATA.captures(html)?;
    serde_json::from_str(caps[1].trim()).ok()
}

/// The persisted hash of `operation` in the web player's script.
pub fn query_hash(script: &str, operation: &str) -> Option<String> {
    let re = Regex::new(&format!(
        r#"{}","query","([0-9a-f]{{64}})""#,
        regex::escape(operation)
    ))
    .ok()?;
    util::search(&re, script)
}

/// The largest of a list of images with `maxWidth` or `width`.
fn largest_image(images: &Value) -> Option<Url> {
    images
        .as_array()?
        .iter()
        .filter_map(|image| {
            let width = util::uint(&image["maxWidth"]).or_else(|| util::uint(&image["width"]))?;
            Some((width, util::url_of(&image["url"], None)?))
        })
        .max_by_key(|(width, _)| *width)
        .map(|(_, url)| url)
}

/// What the embed page of an entity carries: the anonymous token and the entity itself.
struct EmbedState {
    token: String,
    entity: Value,
}

pub struct SpotifyResolver {
    http: Http,
    /// The persisted hash of the episodes query, once read from the player's script.
    episodes_query: Mutex<Option<String>>,
}

impl SpotifyResolver {
    pub fn new(http: Http) -> Self {
        Self {
            http,
            episodes_query: Mutex::new(None),
        }
    }

    async fn page(&self, url: &Url, origin: &Url) -> Result<String, ResolveError> {
        let fetched = fetch(
            &self.http,
            url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(fetched.status, origin) {
            return Err(error);
        }
        Ok(fetched.text())
    }

    /// The embed page of an episode or a show: the token and the entity it shows,
    /// which for a show is its latest episode.
    async fn embed(&self, kind: &str, id: &str, origin: &Url) -> Result<EmbedState, ResolveError> {
        let embed_url = Url::parse(&format!("{SITE}/embed/{kind}/{id}")).expect("valid");
        let html = self.page(&embed_url, origin).await?;
        let data = next_data(&html)
            .ok_or_else(|| ResolveError::malformed(origin, "the embed page carries no data"))?;
        let props = &data["props"]["pageProps"];
        if props["status"].as_u64() == Some(404) {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        let state = &props["state"];
        let token = state["settings"]["session"]["accessToken"]
            .as_str()
            .filter(|t| !t.is_empty())
            .ok_or_else(|| ResolveError::malformed(origin, "the embed page carries no token"))?
            .to_string();
        let entity = state["data"]["entity"].clone();
        if !entity.is_object() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        Ok(EmbedState { token, entity })
    }

    /// A JSON answer from one of the player's APIs, with the token.
    async fn api(
        &self,
        url: Url,
        token: &str,
        body: Option<Value>,
        origin: &Url,
    ) -> Result<Value, ResolveError> {
        let request = match &body {
            Some(body) => self.http.post(url.clone()).json(body),
            None => self.http.get(url.clone()),
        };
        let response = request
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("authorization", &format!("Bearer {token}"))
            .header("accept", "application/json")
            .header("app-platform", "WebPlayer")
            .send()
            .await?;
        if let Some(error) = status_error(response.status, origin) {
            return Err(error);
        }
        response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, format!("{}: {e}", url.path())))
    }

    /// The persisted hash of the episodes query, from the player's script, kept once
    /// read.
    async fn episodes_query_hash(
        &self,
        refresh: bool,
        origin: &Url,
    ) -> Result<String, ResolveError> {
        if !refresh
            && let Some(hash) = self
                .episodes_query
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone()
        {
            return Ok(hash);
        }
        let home = self.page(&Url::parse(SITE).expect("valid"), origin).await?;
        let script_path = util::search(&RE_PLAYER_SCRIPT, &home).ok_or_else(|| {
            ResolveError::malformed(origin, "the player page names no web player script")
        })?;
        let script_url = Url::parse(&format!("{SCRIPT_CDN}{script_path}")).expect("valid");
        let fetched = fetch(&self.http, &script_url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        if let Some(error) = status_error(fetched.status, origin) {
            return Err(error);
        }
        let hash = query_hash(&fetched.text(), EPISODES_QUERY).ok_or_else(|| {
            ResolveError::malformed(
                origin,
                format!("the player script carries no {EPISODES_QUERY} hash"),
            )
        })?;
        *self
            .episodes_query
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(hash.clone());
        Ok(hash)
    }

    /// One page of a show's episodes through the player's query. A hash the API no
    /// longer knows is read again from the script once.
    async fn episodes_page(
        &self,
        token: &str,
        show_id: &str,
        offset: usize,
        origin: &Url,
    ) -> Result<Value, ResolveError> {
        let mut refresh = false;
        loop {
            let hash = self.episodes_query_hash(refresh, origin).await?;
            let body = json!({
                "operationName": EPISODES_QUERY,
                "variables": {"uri": format!("spotify:show:{show_id}"), "offset": offset, "limit": PAGE_SIZE},
                "extensions": {"persistedQuery": {"version": 1, "sha256Hash": hash}}
            });
            let answer = self
                .api(
                    Url::parse(PATHFINDER).expect("valid"),
                    token,
                    Some(body),
                    origin,
                )
                .await?;
            let stale = answer["errors"].as_array().into_iter().flatten().any(|e| {
                e["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("PersistedQueryNotFound"))
            });
            if stale && !refresh {
                refresh = true;
                continue;
            }
            if let Some(message) = answer["errors"][0]["message"].as_str() {
                return Err(ResolveError::unavailable(origin, message.to_string()));
            }
            return Ok(answer);
        }
    }

    /// The podcast host's own file, when the host answers a first-byte request within
    /// [`HOST_PROBE`]: the redirect chains podcast hosts measure downloads through stall
    /// or refuse ranged requests, and such a file is left out rather than offered.
    async fn host_file(&self, original: Url, origin: &Url) -> Option<Variant> {
        let response = self
            .http
            .get(original.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("range", "bytes=0-0")
            .header("accept-encoding", "identity")
            .timeout(HOST_PROBE)
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                tracing::debug!(url = %origin, "the podcast host did not answer for its file: {error}");
                return None;
            }
        };
        if !response.status.is_success() {
            tracing::debug!(url = %origin, status = %response.status, "the podcast host refused its file");
            return None;
        }
        let size = response
            .header("content-range")
            .and_then(|v| v.rsplit('/').next())
            .and_then(|v| v.trim().parse::<u64>().ok())
            .or_else(|| response.content_length().filter(|n| *n > 1));
        let content_type = essence(response.content_type());
        let container = Container::from_mime(&content_type).or_else(|| {
            super::path_extension(&response.url)
                .as_deref()
                .and_then(Container::from_extension)
        });
        let mut variant = Variant::new(original, VariantKind::File);
        variant.audio = match &container {
            Some(Container::Mp3) => Some(AudioCodec::Mp3),
            Some(Container::M4a) => Some(AudioCodec::Aac),
            Some(Container::Ogg) => Some(AudioCodec::Vorbis),
            Some(Container::Opus) => Some(AudioCodec::Opus),
            _ => None,
        };
        variant.container = container;
        variant.audio_only = true;
        variant.size = size;
        variant.format_id = Some("passthrough".into());
        variant.label = Some("podcast host".into());
        Some(variant)
    }

    async fn resolve_episode(&self, id: &str, url: &Url) -> Result<Resolution, ResolveError> {
        let EmbedState { token, entity } = self.embed("episode", id, url).await?;
        if entity["isPlayable"].as_bool() == Some(false) {
            return Err(ResolveError::unavailable(
                url,
                match entity["playabilityReason"].as_str() {
                    Some(reason) if !reason.is_empty() => {
                        format!(
                            "the episode is not playable: {}",
                            reason.to_ascii_lowercase()
                        )
                    }
                    _ => "the episode is not playable".to_string(),
                },
            ));
        }
        let finder = Url::parse(&format!(
            "{SOUND_FINDER}{id}/com.widevine.alpha?market=from_token"
        ))
        .expect("valid");
        let audio = self.api(finder, &token, None, url).await?;
        let format = audio["format"].as_str().unwrap_or("").to_string();
        let locked = format.contains("CBCS") || format.contains("CENC");
        let mut variants = Vec::new();
        if !locked
            && let Some(file) = audio["url"]
                .as_array()
                .and_then(|urls| urls.iter().find_map(|u| util::url_of(u, None)))
        {
            let mut variant = Variant::new(file, VariantKind::File);
            variant.container = Some(Container::M4a);
            variant.audio = Some(AudioCodec::Aac);
            variant.audio_only = true;
            variant.bitrate = util::search(&RE_FORMAT_BITRATE, &format)
                .and_then(|k| k.parse::<u64>().ok())
                .map(|k| k * 1000);
            variant.format_id = Some(format.to_ascii_lowercase());
            variant.label = Some(match variant.bitrate {
                Some(bitrate) => format!("AAC {}k", bitrate / 1000),
                None => "AAC".to_string(),
            });
            variants.push(variant);
        }
        if audio["passthrough"].as_str() == Some("ALLOWED")
            && let Some(original) = util::url_of(&audio["passthroughUrl"], None)
            && let Some(variant) = self.host_file(original, url).await
        {
            variants.push(variant);
        }
        if variants.is_empty() {
            return Err(if locked {
                ResolveError::drm(url, "Widevine")
            } else {
                ResolveError::unavailable(url, "the sound finder named no audio")
            });
        }
        let show_id = entity["relatedEntityUri"]
            .as_str()
            .and_then(|uri| uri.strip_prefix("spotify:show:"))
            .map(String::from);
        let mut resolved = Resolved::of(PLATFORM, MediaKind::Audio);
        resolved.id = Some(id.to_string());
        resolved.title = entity["name"]
            .as_str()
            .or_else(|| entity["title"].as_str())
            .and_then(clean_title);
        resolved.uploader = entity["subtitle"].as_str().and_then(clean_title);
        resolved.uploader_url = show_id
            .as_deref()
            .and_then(|show| Url::parse(&format!("{SITE}/show/{show}")).ok());
        resolved.uploaded_at = entity["releaseDate"]["isoString"]
            .as_str()
            .and_then(|t| t.parse::<Timestamp>().ok());
        resolved.duration = util::millis(&entity["duration"]);
        resolved.thumbnail = largest_image(&entity["visualIdentity"]["image"])
            .or_else(|| largest_image(&entity["relatedEntityCoverArt"]));
        resolved.webpage_url = Url::parse(&format!("{SITE}/episode/{id}")).ok();
        resolved.age_limit = entity["isExplicit"].as_bool().filter(|e| *e).map(|_| 18);
        for variant in &mut variants {
            variant.duration = resolved.duration;
        }
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn resolve_show(&self, id: &str, url: &Url) -> Result<Resolution, ResolveError> {
        let EmbedState { token, entity } = self.embed("show", id, url).await?;
        let name = entity["subtitle"].as_str().and_then(clean_title);
        let mut entries = Vec::new();
        let mut total = None;
        let mut offset = 0;
        loop {
            let page = self.episodes_page(&token, id, offset, url).await?;
            let episodes = &page["data"]["podcastUnionV2"]["episodesV2"];
            if episodes.is_null() {
                return Err(ResolveError::NotFound(url.clone()));
            }
            total = total.or_else(|| util::uint(&episodes["totalCount"]).map(|n| n as usize));
            let items = episodes["items"].as_array().cloned().unwrap_or_default();
            if items.is_empty() {
                break;
            }
            offset += items.len();
            for item in &items {
                let episode = &item["entity"]["data"];
                let Some(episode_id) = episode["id"]
                    .as_str()
                    .or_else(|| episode["uri"].as_str()?.strip_prefix("spotify:episode:"))
                else {
                    continue;
                };
                entries.push(PlaylistEntry {
                    url: Url::parse(&format!("{SITE}/episode/{episode_id}")).expect("valid"),
                    title: episode["name"].as_str().and_then(clean_title),
                    duration: util::millis(&episode["duration"]["totalMilliseconds"]),
                });
            }
            if entries.len() >= LISTING_LIMIT || total.is_some_and(|total| offset >= total) {
                break;
            }
        }
        entries.truncate(LISTING_LIMIT);
        if entries.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.into(),
            id: Some(id.to_string()),
            title: name,
            total: total.or(Some(entries.len())),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for SpotifyResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Spotify podcasts",
            hosts: &["open.spotify.com"],
            features: &["episodes", "shows", "embeds"],
            formats: &["m4a", "mp3"],
            media: &[MediaKind::Audio],
            tags: &[Tag::Podcasts],
            session: SessionSupport::None,
            examples: &[
                "https://open.spotify.com/episode/3crGsCZzzR8znh9HPh8qR5",
                "https://open.spotify.com/episode/6f50x9eHVtGGNFsRSfHpnF",
                "https://open.spotify.com/show/4rOoJ6Egrf8K2IrywzwOMk",
                "https://open.spotify.com/embed/show/3IM0lmZxpFAY7CwMuv9H4g",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Episode { id } => self.resolve_episode(&id, url).await,
            Link::Show { id } => self.resolve_show(&id, url).await,
            Link::Music { .. } => Err(ResolveError::drm(url, "Widevine")),
        }
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

    fn probe(url: &str, status: u16, content_type: &str, size: u64) -> Exchange {
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
                headers: vec![
                    ("content-type".into(), content_type.into()),
                    ("content-range".into(), format!("bytes 0-0/{size}")),
                ],
                body: RecordedBody::Empty,
                truncated: false,
            },
        }
    }

    fn embed_page(entity: Value) -> String {
        let data = json!({"props": {"pageProps": {"state": {
            "data": {"entity": entity, "defaultAudioFileObject": {"format": "MP4_128_CBCS", "passthrough": "NONE"}},
            "settings": {"session": {"accessToken": "BQBtoken", "isAnonymous": true}}
        }}}});
        format!(
            r#"<html><body><script id="__NEXT_DATA__" type="application/json">{data}</script></body></html>"#
        )
    }

    fn episode_entity(id: &str, name: &str) -> Value {
        json!({"type": "episode", "name": name, "title": name, "id": id, "uri": format!("spotify:episode:{id}"),
            "subtitle": "The Joe Rogan Experience", "releaseDate": {"isoString": "2026-09-24T17:00:00Z"}, "duration": 8622975,
            "isPlayable": true, "playabilityReason": "PLAYABLE", "isExplicit": true, "relatedEntityUri": "spotify:show:4rOoJ6Egrf8K2IrywzwOMk",
            "relatedEntityCoverArt": [{"url": "https://image-cdn-fa.spotifycdn.com/image/show300", "maxHeight": 300, "maxWidth": 300}],
            "visualIdentity": {"image": [{"url": "https://image-cdn-fa.spotifycdn.com/image/ep300", "maxWidth": 300}, {"url": "https://image-cdn-fa.spotifycdn.com/image/ep640", "maxWidth": 640}]}})
    }

    const FINDER: &str = "https://spclient.wg.spotify.com/soundfinder/v1/unauth/episode/3crGsCZzzR8znh9HPh8qR5/com.widevine.alpha?market=from_token";

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://open.spotify.com/episode/3crGsCZzzR8znh9HPh8qR5?si=abc"),
            Some(Link::Episode {
                id: "3crGsCZzzR8znh9HPh8qR5".into()
            })
        );
        assert_eq!(
            link("https://open.spotify.com/intl-de/show/4rOoJ6Egrf8K2IrywzwOMk"),
            Some(Link::Show {
                id: "4rOoJ6Egrf8K2IrywzwOMk".into()
            })
        );
        assert_eq!(
            link("https://open.spotify.com/embed/episode/3crGsCZzzR8znh9HPh8qR5"),
            Some(Link::Episode {
                id: "3crGsCZzzR8znh9HPh8qR5".into()
            })
        );
        assert_eq!(
            link("https://open.spotify.com/embed-podcast/show/4rOoJ6Egrf8K2IrywzwOMk"),
            Some(Link::Show {
                id: "4rOoJ6Egrf8K2IrywzwOMk".into()
            })
        );
        assert_eq!(
            link("https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC"),
            Some(Link::Music {
                kind: "track".into(),
                id: "4uLU6hMCjMI75M1A2tKUQC".into()
            })
        );
        assert_eq!(
            link("https://open.spotify.com/playlist/37i9dQZF1DXcBWIGoYBM5M"),
            Some(Link::Music {
                kind: "playlist".into(),
                id: "37i9dQZF1DXcBWIGoYBM5M".into()
            })
        );
        assert_eq!(link("https://open.spotify.com/"), None);
        assert_eq!(link("https://open.spotify.com/episode/short"), None);
        assert_eq!(link("https://open.spotify.com/search/rogan"), None);
        assert_eq!(
            link("https://example.com/episode/3crGsCZzzR8znh9HPh8qR5"),
            None
        );
        assert_eq!(
            query_hash(
                r#"gA=(0,r.C)("queryPodcastEpisodes","query","06046f9b939d56c8eb7cdbb687da938de1164c006871aec91dc26e4dc7d8eb08",null)"#,
                "queryPodcastEpisodes"
            )
            .as_deref(),
            Some("06046f9b939d56c8eb7cdbb687da938de1164c006871aec91dc26e4dc7d8eb08")
        );
    }

    #[tokio::test]
    async fn episodes_resolve_to_their_audio_and_music_is_refused() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "GET",
            "https://open.spotify.com/embed/episode/3crGsCZzzR8znh9HPh8qR5",
            200,
            "text/html",
            &embed_page(episode_entity(
                "3crGsCZzzR8znh9HPh8qR5",
                "#2558 - Tyler Engle",
            )),
        ));
        fixture.exchanges.push(exchange("GET", FINDER, 200, "application/json", &json!({
            "url": ["https://audio4-fa.scdn.co/audio/468e?1790357066_k", "https://audio4-ak.spotifycdn.com/audio/468e?__token__=exp"],
            "format": "MP4_128", "passthrough": "ALLOWED", "passthroughUrl": "https://dts.podtrac.com/redirect.mp3/host.example/ep.mp3", "fileId": "468e"
        }).to_string()));
        fixture.exchanges.push(probe(
            "https://dts.podtrac.com/redirect.mp3/host.example/ep.mp3",
            206,
            "audio/mpeg",
            38607459,
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://open.spotify.com/embed/episode/1111111111111111111111",
            200,
            "text/html",
            r#"<html><script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{"status":404,"title":"Page not found"}}}</script></html>"#,
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://open.spotify.com/embed/episode/2222222222222222222222",
            200,
            "text/html",
            &embed_page(episode_entity("2222222222222222222222", "Locked")),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://spclient.wg.spotify.com/soundfinder/v1/unauth/episode/2222222222222222222222/com.widevine.alpha?market=from_token",
            200,
            "application/json",
            &json!({"url": ["https://audio4-fa.scdn.co/audio/43e1"], "format": "MP4_128_CBCS", "passthrough": "NONE", "passthroughUrl": ""}).to_string(),
        ));
        let resolver = SpotifyResolver::new(Http::replay(fixture));
        let url = Url::parse("https://open.spotify.com/episode/3crGsCZzzR8znh9HPh8qR5").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.media, MediaKind::Audio);
        assert_eq!(resolved.id.as_deref(), Some("3crGsCZzzR8znh9HPh8qR5"));
        assert_eq!(resolved.title.as_deref(), Some("#2558 - Tyler Engle"));
        assert_eq!(
            resolved.uploader.as_deref(),
            Some("The Joe Rogan Experience")
        );
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://open.spotify.com/show/4rOoJ6Egrf8K2IrywzwOMk"
        );
        assert_eq!(resolved.duration, Some(Duration::from_millis(8622975)));
        assert_eq!(resolved.age_limit, Some(18));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://image-cdn-fa.spotifycdn.com/image/ep640"
        );
        assert_eq!(resolved.variants.len(), 2);
        let spotify = &resolved.variants[0];
        assert_eq!(
            spotify.url.as_str(),
            "https://audio4-fa.scdn.co/audio/468e?1790357066_k"
        );
        assert_eq!(spotify.container, Some(Container::M4a));
        assert_eq!(spotify.audio, Some(AudioCodec::Aac));
        assert_eq!(spotify.bitrate, Some(128_000));
        assert!(spotify.audio_only);
        assert_eq!(spotify.format_id.as_deref(), Some("mp4_128"));
        let host = &resolved.variants[1];
        assert_eq!(host.container, Some(Container::Mp3));
        assert_eq!(host.size, Some(38607459));
        assert_eq!(host.format_id.as_deref(), Some("passthrough"));
        assert!(matches!(
            resolver
                .resolve(
                    &Url::parse("https://open.spotify.com/episode/1111111111111111111111").unwrap()
                )
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let locked = resolver
            .resolve(
                &Url::parse("https://open.spotify.com/episode/2222222222222222222222").unwrap(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&locked, ResolveError::Drm { system, .. } if system == "Widevine"),
            "{locked}"
        );
        let music = resolver
            .resolve(&Url::parse("https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC").unwrap())
            .await
            .unwrap_err();
        assert!(matches!(&music, ResolveError::Drm { .. }), "{music}");
    }

    fn episodes_answer(names: &[(&str, &str)], total: u64) -> String {
        json!({"data": {"podcastUnionV2": {"__typename": "Podcast", "episodesV2": {
            "items": names.iter().map(|(id, name)| json!({"entity": {"_uri": format!("spotify:episode:{id}"), "data": {
                "__typename": "Episode", "id": id, "uri": format!("spotify:episode:{id}"), "name": name, "duration": {"totalMilliseconds": 8622975},
                "podcastV2": {"data": {"name": "The Joe Rogan Experience"}}}}})).collect::<Vec<_>>(),
            "totalCount": total}}}}).to_string()
    }

    #[tokio::test]
    async fn shows_list_their_episodes_through_the_players_query() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "GET",
            "https://open.spotify.com/embed/show/4rOoJ6Egrf8K2IrywzwOMk",
            200,
            "text/html",
            &embed_page(episode_entity(
                "3crGsCZzzR8znh9HPh8qR5",
                "#2558 - Tyler Engle",
            )),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://open.spotify.com/",
            200,
            "text/html",
            r#"<html><head><script src="https://open.spotifycdn.com/cdn/build/web-player/web-player.c424f30a.js"></script></head></html>"#,
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://open.spotifycdn.com/cdn/build/web-player/web-player.c424f30a.js",
            200,
            "application/javascript",
            r#"var a=1;gA=(0,r.C)("queryPodcastEpisodes","query","06046f9b939d56c8eb7cdbb687da938de1164c006871aec91dc26e4dc7d8eb08",null);"#,
        ));
        fixture.exchanges.push(exchange(
            "POST",
            PATHFINDER,
            200,
            "application/json",
            &episodes_answer(
                &[
                    ("3crGsCZzzR8znh9HPh8qR5", "#2558 - Tyler Engle"),
                    ("082a1V6nazH9ZZqskV1vfz", "#2557 - Someone"),
                ],
                2756,
            ),
        ));
        fixture.exchanges.push(exchange(
            "POST",
            PATHFINDER,
            200,
            "application/json",
            &episodes_answer(&[("1111111111111111111111", "#2556 - Another")], 2756),
        ));
        fixture.exchanges.push(exchange(
            "POST",
            PATHFINDER,
            200,
            "application/json",
            &episodes_answer(&[], 2756),
        ));
        let resolver = SpotifyResolver::new(Http::replay(fixture));
        let Resolution::Playlist(show) = resolver
            .resolve(&Url::parse("https://open.spotify.com/show/4rOoJ6Egrf8K2IrywzwOMk").unwrap())
            .await
            .unwrap()
        else {
            panic!("a show is a playlist");
        };
        assert_eq!(show.title.as_deref(), Some("The Joe Rogan Experience"));
        assert_eq!(show.total, Some(2756));
        assert_eq!(show.entries.len(), 3);
        assert_eq!(
            show.entries[1].url.as_str(),
            "https://open.spotify.com/episode/082a1V6nazH9ZZqskV1vfz"
        );
        assert_eq!(show.entries[1].title.as_deref(), Some("#2557 - Someone"));
        assert_eq!(
            show.entries[0].duration,
            Some(Duration::from_millis(8622975))
        );
        assert_eq!(
            resolver.episodes_query.lock().unwrap().as_deref(),
            Some("06046f9b939d56c8eb7cdbb687da938de1164c006871aec91dc26e4dc7d8eb08"),
            "the hash is kept for the next show"
        );
    }

    /// Every example link resolves live: the episodes with Spotify's own audio and the
    /// shows with their episodes.
    #[tokio::test]

    async fn live_examples_resolve() {
        let resolver = SpotifyResolver::new(Http::new(crate::http::HttpConfig::default()));
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
            assert!(resolver.matches(&url), "{link}");
            let resolution = tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&url))
                .await
                .expect("resolution timed out")
                .unwrap_or_else(|e| panic!("{link}: {e}"));
            match resolution {
                Resolution::Media(resolved) => {
                    assert_eq!(resolved.media, MediaKind::Audio);
                    assert!(
                        resolved.variants.iter().any(|v| v.is_playable()),
                        "{link}: no playable variant"
                    );
                    println!(
                        "{link}: {:?} by {:?} with {} variants ({:?})",
                        resolved.title,
                        resolved.uploader,
                        resolved.variants.len(),
                        resolved
                            .variants
                            .iter()
                            .map(|v| v.format_id.clone().unwrap_or_default())
                            .collect::<Vec<_>>()
                    );
                }
                Resolution::Playlist(playlist) => {
                    assert!(!playlist.entries.is_empty(), "{link}: no entries");
                    println!(
                        "{link}: playlist {:?} with {} entries of {:?}",
                        playlist.title,
                        playlist.entries.len(),
                        playlist.total
                    );
                }
            }
        }
        let music = resolver
            .resolve(&Url::parse("https://open.spotify.com/track/4uLU6hMCjMI75M1A2tKUQC").unwrap())
            .await
            .unwrap_err();
        assert!(matches!(music, ResolveError::Drm { .. }));
    }
}
