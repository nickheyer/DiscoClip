//! Stripchat live rooms and the listings of who is online. A room's username is turned
//! into its model id by the site's front API, whose cam endpoint says whether the model
//! is streaming, in a private show, or offline. The stream is the HLS master playlist
//! the site's player reads from its edge host. The playlists come under the "Mouflon"
//! scheme the player implements: a media playlist has to be asked with the scheme and
//! key id the master offers, and each of its segment lines is a placeholder whose real
//! address stands in the `EXT-X-MOUFLON:URI` tag before it, with one part of that name
//! encrypted under a key the player carries. The variants here carry the keyed media
//! playlists, and [`restore_media_playlist`] turns a fetched media playlist into one
//! with plain segment addresses, as the player does before it plays.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, Tag, clean_title, fetch, hls, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::MediaKind;

pub const PLATFORM: &str = "stripchat";
const SITE: &str = "https://stripchat.com/";
const API: &str = "https://stripchat.com/api/front/";
/// The edge host the player reads playlists from.
const PRIMARY_HLS_HOST: &str = "doppiocdn.com";
/// How many rooms a listing is read up to.
const LISTING_LIMIT: usize = 60;
/// The Mouflon scheme version the player speaks.
const MOUFLON_SCHEME: &str = "v2";
/// The key ids the player knows, each with the secret the segment names are encrypted
/// under: `KEY_ID:SECRET` pairs as the player's playlist handler carries them.
const MOUFLON_KEYS: &[(&str, &str)] = &[("Ook7quaiNgiyuhai", "EQueeGh2kaewa3ch")];

static RE_USERNAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_@.-]{3,}$").unwrap());
/// `#EXT-X-MOUFLON:PSCH:v2:Ook7quaiNgiyuhai`: a scheme and key id a master offers.
static RE_SCHEME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^#EXT-X-MOUFLON:PSCH:([^:\r\n]+):([^\r\n]+)$").unwrap());
/// `_<encrypted>_<sequence>.mp4`: the encrypted part of a segment's name and its tail.
static RE_SEGMENT_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"_([^_]+)_(\d+(?:_part\d+)?)\.mp4(?:[?#].*)?$").unwrap());

/// Paths on the site that are pages of their own, never a room.
const RESERVED: &[&str] = &[
    "girls",
    "couples",
    "men",
    "trans",
    "cams",
    "tags",
    "videos",
    "discover",
    "model",
    "user",
    "api",
    "login",
    "signup",
    "register",
    "terms",
    "privacy",
    "about",
    "support",
    "blog",
    "contest",
    "categories",
    "search",
    "favorites",
    "my",
    "settings",
    "recordings",
    "dmca",
    "sitemap",
    "static",
    "help",
    "faq",
    "affiliates",
    "studio",
    "models",
    "users",
    "chat",
    "top",
    "new",
    "vr",
    "mobile",
    "notification",
    "payment",
    "premium",
    "feed",
    "wallet",
];

/// The four listings the site groups rooms by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    Girls,
    Couples,
    Men,
    Trans,
}

impl Category {
    fn tag(self) -> &'static str {
        match self {
            Category::Girls => "girls",
            Category::Couples => "couples",
            Category::Men => "men",
            Category::Trans => "trans",
        }
    }

    fn of(segment: &str) -> Option<Category> {
        Some(match segment {
            "girls" => Category::Girls,
            "couples" => Category::Couples,
            "men" => Category::Men,
            "trans" => Category::Trans,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Room {
        username: String,
    },
    /// A listing of rooms online: the front page, or one of the four categories.
    Listing {
        category: Category,
    },
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "stripchat.com" && host != "www.stripchat.com" {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let room = |name: &str| {
        (RE_USERNAME.is_match(name) && !RESERVED.contains(&name.to_ascii_lowercase().as_str()))
            .then(|| Link::Room {
                username: name.to_string(),
            })
    };
    match segments.as_slice() {
        [] => Some(Link::Listing {
            category: Category::Girls,
        }),
        [category] if Category::of(category).is_some() => Some(Link::Listing {
            category: Category::of(category)?,
        }),
        ["cams", name] => room(name),
        [name] => room(name),
        [name, tab]
            if matches!(
                *tab,
                "profile" | "videos" | "photos" | "gallery" | "schedule"
            ) =>
        {
            room(name)
        }
        _ => None,
    }
}

/// The secret the segment names under `key_id` are encrypted with, when the player
/// knows the key.
pub fn mouflon_secret(key_id: &str) -> Option<&'static str> {
    MOUFLON_KEYS
        .iter()
        .find(|(id, _)| *id == key_id)
        .map(|(_, secret)| *secret)
}

/// The scheme and key id a master playlist offers that the player knows.
pub fn known_scheme(master: &str) -> Option<(String, String)> {
    RE_SCHEME
        .captures_iter(master)
        .map(|c| (c[1].to_string(), c[2].trim().to_string()))
        .find(|(scheme, key)| scheme == MOUFLON_SCHEME && mouflon_secret(key).is_some())
}

/// `url` with the scheme and key the media playlists are asked with.
pub fn keyed(url: &str, scheme: &str, key_id: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}psch={scheme}&pkey={key_id}")
}

/// The master playlist with every media playlist link keyed, so the media playlists
/// answer with their segments rather than the placeholder advert.
pub fn keyed_master(master: &str, scheme: &str, key_id: &str) -> String {
    master
        .lines()
        .map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with('#') || trimmed.is_empty() {
                line.to_string()
            } else {
                keyed(trimmed, scheme, key_id)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The plain name of an encrypted segment name part: the token reversed, read as
/// base64, and each byte XORed with the SHA-256 digest of `secret`, repeating.
pub fn decrypt_segment_name(token: &str, secret: &str) -> Option<String> {
    let reversed: String = token.chars().rev().collect();
    let bytes = util::b64_decode(&reversed)?;
    let digest = util::sha256(secret.as_bytes());
    let plain: Vec<u8> = bytes
        .iter()
        .enumerate()
        .map(|(index, byte)| byte ^ digest[index % digest.len()])
        .collect();
    let name = String::from_utf8(plain).ok()?;
    (!name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    .then_some(name)
}

/// A segment address with its encrypted name part restored.
pub fn restore_segment_url(url: &str, secret: &str) -> Option<String> {
    let caps = RE_SEGMENT_NAME.captures(url)?;
    let plain = decrypt_segment_name(&caps[1], secret)?;
    let start = caps.get(0)?.start();
    Some(format!("{}_{plain}_{}.mp4", &url[..start], &caps[2]))
}

/// A fetched media playlist with plain segment addresses: each placeholder line is
/// replaced by the address the `EXT-X-MOUFLON:URI` tag before it names, decrypted.
pub fn restore_media_playlist(playlist: &str, secret: &str) -> String {
    let mut out = Vec::new();
    let mut pending: Option<String> = None;
    for line in playlist.lines() {
        let trimmed = line.trim();
        if let Some(address) = trimmed.strip_prefix("#EXT-X-MOUFLON:URI:") {
            pending = Some(address.trim().to_string());
            continue;
        }
        if let Some(address) = pending.as_deref() {
            let restored =
                restore_segment_url(address, secret).unwrap_or_else(|| address.to_string());
            if let Some(rest) = trimmed.strip_prefix("#EXT-X-PART:") {
                static RE_PART_URI: LazyLock<Regex> =
                    LazyLock::new(|| Regex::new(r#"URI="[^"]+""#).unwrap());
                out.push(format!(
                    "#EXT-X-PART:{}",
                    RE_PART_URI.replace(rest, format!("URI=\"{restored}\"").as_str())
                ));
                pending = None;
                continue;
            }
            if !trimmed.starts_with('#') && !trimmed.is_empty() {
                out.push(restored);
                pending = None;
                continue;
            }
        }
        out.push(line.to_string());
    }
    out.join("\n")
}

/// What a room's status, or the mode of the show it is in, means for the caller: the
/// public stream is withheld while the model is in a private or group show.
fn status_error_of(status: &str, username: &str, url: &Url) -> Option<ResolveError> {
    Some(match status {
        "off" | "idle" => {
            ResolveError::unavailable(url, format!("{username} is not streaming right now"))
        }
        "private" | "p2p" | "virtualPrivate" => {
            ResolveError::unavailable(url, format!("{username} is in a private show right now"))
        }
        "groupShow" | "ticketShow" => {
            ResolveError::unavailable(url, format!("{username} is in a group show right now"))
        }
        _ => return None,
    })
}

pub struct StripchatResolver {
    http: Http,
}

impl StripchatResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// A JSON answer of the front API.
    async fn api(&self, path: &str, origin: &Url) -> Result<super::Fetched, ResolveError> {
        let api_url = Url::parse(&format!("{API}{path}")).expect("valid");
        fetch(
            &self.http,
            &api_url,
            PLATFORM,
            BROWSER_UA,
            &[("accept".to_string(), "application/json".to_string())],
            MAX_PAGE,
        )
        .await
        .map_err(|error| error.at(origin))
    }

    async fn resolve_room(&self, username: &str, url: &Url) -> Result<Resolution, ResolveError> {
        let lookup = self
            .api(
                &format!("users/user-ids/{}", util::url_encode(username)),
                url,
            )
            .await?;
        match lookup.status.as_u16() {
            200..=299 => {}
            404 => return Err(ResolveError::NotFound(url.clone())),
            429 => return Err(ResolveError::RateLimited(url.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    url,
                    format!("the site answered HTTP {status}"),
                ));
            }
        }
        let id = util::uint(&lookup.json(url)?["id"])
            .ok_or_else(|| ResolveError::malformed(url, "the user lookup names no id"))?;
        let answer = self.api(&format!("v2/models/{id}/cam"), url).await?;
        if let Some(error) = status_error(answer.status, url) {
            return Err(error);
        }
        let cam = answer.json(url)?;
        let user = &cam["user"]["user"];
        let name = util::text(&user["username"]).unwrap_or_else(|| username.to_string());
        if util::boolean(&user["isDeleted"]) == Some(true) {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let status = util::text(&user["status"]).unwrap_or_default();
        if util::boolean(&user["isLive"]) != Some(true) {
            return Err(ResolveError::unavailable(
                url,
                format!("{name} is not streaming right now"),
            ));
        }
        if let Some(error) = status_error_of(&status, &name, url) {
            return Err(error);
        }
        if let Some(mode) = cam["cam"]["show"]["mode"].as_str()
            && let Some(error) = status_error_of(mode, &name, url)
        {
            return Err(error);
        }
        let master_url = Url::parse(&format!(
            "https://edge-hls.{PRIMARY_HLS_HOST}/hls/{id}/master/{id}_auto.m3u8"
        ))
        .expect("valid");
        let fetched = fetch(&self.http, &master_url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        let master = fetched.text();
        if !master.trim_start().starts_with("#EXTM3U") {
            return Err(ResolveError::malformed(
                url,
                format!("{PRIMARY_HLS_HOST} answered no playlist"),
            ));
        }
        let (scheme, key_id) = known_scheme(&master).ok_or_else(|| {
            ResolveError::malformed(url, "the playlist offers no key the player knows")
        })?;
        let expanded = hls::expand_playlist(
            &self.http,
            &master_url,
            &master_url,
            keyed_master(&master, &scheme, &key_id).as_bytes(),
            PLATFORM,
            BROWSER_UA,
            &[],
        )
        .await?;
        let mut variants = expanded.variants;
        for variant in &mut variants {
            variant.live = true;
            variant.duration = None;
            variant.format_id = Some(match &variant.label {
                Some(label) => format!("hls-{label}"),
                None => "hls".to_string(),
            });
        }
        let room = Url::parse(&format!("{SITE}{name}")).expect("valid");
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(name.clone());
        resolved.title = util::text(&cam["cam"]["topic"])
            .and_then(|t| clean_title(&t))
            .map(|topic| format!("{name}: {topic}"))
            .or_else(|| clean_title(&name));
        resolved.uploader = clean_title(&name);
        resolved.uploader_url = Some(room.clone());
        resolved.thumbnail = util::url_of(&user["previewUrl"], None)
            .or_else(|| util::url_of(&user["avatarUrl"], None));
        resolved.webpage_url = Some(room);
        resolved.age_limit = Some(18);
        resolved.live = true;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn resolve_listing(
        &self,
        category: Category,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let answer = self
            .api(
                &format!(
                    "models?limit={LISTING_LIMIT}&offset=0&primaryTag={}&sortBy=viewersRating",
                    category.tag()
                ),
                url,
            )
            .await?;
        if let Some(error) = status_error(answer.status, url) {
            return Err(error);
        }
        let listing: Value = answer.json(url)?;
        let entries: Vec<PlaylistEntry> = listing["models"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|model| util::boolean(&model["isLive"]) == Some(true))
            .filter_map(|model| {
                let username = util::text(&model["username"])?;
                Some(PlaylistEntry {
                    url: Url::parse(&format!("{SITE}{username}")).ok()?,
                    title: util::text(&model["groupShowTopic"])
                        .and_then(|t| clean_title(&t))
                        .map(|topic| format!("{username}: {topic}"))
                        .or_else(|| clean_title(&username)),
                    duration: None,
                })
            })
            .collect();
        if entries.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(category.tag().to_string()),
            title: Some(format!("Stripchat {} online", category.tag())),
            total: util::uint(&listing["totalCount"])
                .map(|n| n as usize)
                .or(Some(entries.len())),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for StripchatResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Stripchat",
            hosts: &["stripchat.com"],
            features: &["live", "rooms", "listings"],
            formats: &["hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::Nsfw, Tag::Live],
            session: SessionSupport::None,
            on_by_default: true,
            examples: &[
                "https://stripchat.com/",
                "https://stripchat.com/girls",
                "https://stripchat.com/couples",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Room { username } => self.resolve_room(&username, url).await,
            Link::Listing { category } => self.resolve_listing(category, url).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };
    use serde_json::json;

    fn exchange(url: &str, status: u16, content_type: &str, body: &str) -> Exchange {
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
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let room = |name: &str| {
            Some(Link::Room {
                username: name.into(),
            })
        };
        assert_eq!(link("https://stripchat.com/dirtypub"), room("dirtypub"));
        assert_eq!(
            link("https://www.stripchat.com/Joselin_Flower/"),
            room("Joselin_Flower")
        );
        assert_eq!(
            link("https://stripchat.com/Rakhijaan@xh"),
            room("Rakhijaan@xh")
        );
        assert_eq!(
            link("https://stripchat.com/cams/dirtypub"),
            room("dirtypub")
        );
        assert_eq!(
            link("https://stripchat.com/dirtypub/profile"),
            room("dirtypub")
        );
        assert_eq!(
            link("https://stripchat.com/"),
            Some(Link::Listing {
                category: Category::Girls
            })
        );
        assert_eq!(
            link("https://stripchat.com/couples"),
            Some(Link::Listing {
                category: Category::Couples
            })
        );
        assert_eq!(link("https://stripchat.com/girls/asian"), None);
        assert_eq!(link("https://stripchat.com/login"), None);
        assert_eq!(link("https://stripchat.com/api/front/models"), None);
        assert_eq!(link("https://stripchat.com/dirtypub/knights"), None);
        assert_eq!(link("https://example.com/dirtypub"), None);
    }

    const MASTER: &str = "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-MOUFLON:PSCH:v2:1Dzcc6OjP73LKbtI\n#EXT-X-MOUFLON:PSCH:v2:Ook7quaiNgiyuhai\n\
        #EXT-X-STREAM-INF:BANDWIDTH=2401484,CODECS=\"avc1.42001f,mp4a.40.2\",RESOLUTION=1280x720,FRAME-RATE=29.000,NAME=\"source\"\n\
        https://media-hls.doppiocdn.com/b-hls-08/266549456/266549456.m3u8?playlistType=standard&preferredVideoCodec=h264\n\
        #EXT-X-STREAM-INF:BANDWIDTH=654438,CODECS=\"avc1.4d0015,mp4a.40.2\",RESOLUTION=426x240,FRAME-RATE=30.000,NAME=\"240p\"\n\
        https://media-hls.doppiocdn.com/b-hls-08/266549456/266549456_240p.m3u8?playlistType=standard&preferredVideoCodec=h264\n";

    const MEDIA: &str = "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-MOUFLON:PSCH:v2:Ook7quaiNgiyuhai\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:686\n\
        #EXT-X-MAP:URI=\"https://media-hls.doppiocdn.com/b-hls-08/266549456/266549456_240p_h264_init_MZrcUNdWl6G8l0WY.mp4\"\n\
        #EXTINF:1.999\n#EXT-X-MOUFLON:URI:https://media-hls.doppiocdn.com/b-hls-08/266549456/266549456_240p_h264_686_Qvcn595Ezx+q9lSg6i9v0z_1790271449.mp4\n\
        https://media-hls.doppiocdn.com/b-hls-08/media.mp4\n\
        #EXTINF:2.000\n#EXT-X-MOUFLON:URI:https://media-hls.doppiocdn.com/b-hls-08/266549456/266549456_240p_h264_687_g+c+Noj8AoszsvGxpvD3Vx_1790271451.mp4\n\
        https://media-hls.doppiocdn.com/b-hls-08/media.mp4\n";

    #[test]
    fn mouflon_playlists_are_keyed_and_their_segments_restored() {
        assert_eq!(
            known_scheme(MASTER),
            Some(("v2".to_string(), "Ook7quaiNgiyuhai".to_string()))
        );
        assert_eq!(
            known_scheme("#EXTM3U\n#EXT-X-MOUFLON:PSCH:v2:unknown\n"),
            None
        );
        let keyed_text = keyed_master(MASTER, "v2", "Ook7quaiNgiyuhai");
        assert!(keyed_text.contains(
            "266549456_240p.m3u8?playlistType=standard&preferredVideoCodec=h264&psch=v2&pkey=Ook7quaiNgiyuhai"
        ));
        assert_eq!(
            keyed("https://h/p.m3u8", "v2", "k"),
            "https://h/p.m3u8?psch=v2&pkey=k"
        );
        assert_eq!(
            decrypt_segment_name("Qvcn595Ezx+q9lSg6i9v0z", "EQueeGh2kaewa3ch").as_deref(),
            Some("aZwVGcbQe2xUd1rp")
        );
        assert_eq!(
            decrypt_segment_name("g+c+Noj8AoszsvGxpvD3Vx", "EQueeGh2kaewa3ch").as_deref(),
            Some("kLIcs7KG7UFO3w27")
        );
        assert_eq!(
            restore_segment_url(
                "https://media-hls.doppiocdn.com/b-hls-26/41991456/41991456_5904_wr9H/9UsRuC27lzwbi7zX7_1790273000.mp4",
                "EQueeGh2kaewa3ch"
            )
            .as_deref(),
            Some("https://media-hls.doppiocdn.com/b-hls-26/41991456/41991456_5904_CmqTSBb6YLRxdYSb_1790273000.mp4")
        );
        assert_eq!(
            decrypt_segment_name("Qvcn595Ezx+q9lSg6i9v0z", "wrong"),
            None
        );
        let restored = restore_media_playlist(MEDIA, "EQueeGh2kaewa3ch");
        assert!(!restored.contains("media.mp4"));
        assert!(!restored.contains("EXT-X-MOUFLON:URI"));
        assert!(restored.contains(
            "\n#EXTINF:1.999\nhttps://media-hls.doppiocdn.com/b-hls-08/266549456/266549456_240p_h264_686_aZwVGcbQe2xUd1rp_1790271449.mp4\n"
        ));
        assert!(restored.contains("266549456_240p_h264_687_kLIcs7KG7UFO3w27_1790271451.mp4"));
        assert!(restored.contains("#EXT-X-MAP:URI=\"https://media-hls.doppiocdn.com/b-hls-08/266549456/266549456_240p_h264_init_MZrcUNdWl6G8l0WY.mp4\""));
        let part = "#EXTINF:2.0\n#EXT-X-MOUFLON:URI:https://h/1_Qvcn595Ezx+q9lSg6i9v0z_5_part1.mp4\n#EXT-X-PART:DURATION=0.5,URI=\"https://h/media.mp4\"\n";
        assert!(
            restore_media_playlist(part, "EQueeGh2kaewa3ch").contains(
                "#EXT-X-PART:DURATION=0.5,URI=\"https://h/1_aZwVGcbQe2xUd1rp_5_part1.mp4\""
            )
        );
    }

    fn cam(status: &str, live: bool, topic: &str) -> String {
        json!({
            "cam": {"streamName": "266549456", "topic": topic, "show": if matches!(status, "private" | "groupShow") { json!({"mode": status}) } else { Value::Null }},
            "user": {"user": {"id": 266549456, "username": "realdoll", "status": status, "isLive": live, "isOnline": live,
                "previewUrl": "https://static-proxy.strpst.com/previews/1/6/0/16092d80-full", "avatarUrl": "https://static-proxy.strpst.com/avatars/0/a/f/0af62d21-full"}}
        })
        .to_string()
    }

    #[tokio::test]
    async fn live_rooms_resolve_to_keyed_playlists() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "https://stripchat.com/api/front/users/user-ids/realdoll",
            200,
            "application/json",
            r#"{"id":266549456}"#,
        ));
        fixture.exchanges.push(exchange(
            "https://stripchat.com/api/front/v2/models/266549456/cam",
            200,
            "application/json",
            &cam("public", true, "ENJOY MY STREAM :)"),
        ));
        fixture.exchanges.push(exchange(
            "https://edge-hls.doppiocdn.com/hls/266549456/master/266549456_auto.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MASTER,
        ));
        fixture.exchanges.push(exchange(
            "https://media-hls.doppiocdn.com/b-hls-08/266549456/266549456.m3u8?playlistType=standard&preferredVideoCodec=h264&psch=v2&pkey=Ook7quaiNgiyuhai",
            200,
            "application/vnd.apple.mpegurl",
            MEDIA,
        ));
        let resolver = StripchatResolver::new(Http::replay(fixture));
        let url = Url::parse("https://stripchat.com/realdoll").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        crate::resolve::assert_one_family(&resolved.variants);
        assert_eq!(resolved.id.as_deref(), Some("realdoll"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("realdoll: ENJOY MY STREAM :)")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("realdoll"));
        assert!(resolved.live);
        assert_eq!(resolved.age_limit, Some(18));
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://static-proxy.strpst.com/previews/1/6/0/16092d80-full"
        );
        assert_eq!(resolved.variants.len(), 2);
        let best = &resolved.variants[0];
        assert!(best.live);
        assert_eq!(best.height, Some(720));
        assert_eq!(best.format_id.as_deref(), Some("hls-720p"));
        assert_eq!(
            best.url.as_str(),
            "https://media-hls.doppiocdn.com/b-hls-08/266549456/266549456.m3u8?playlistType=standard&preferredVideoCodec=h264&psch=v2&pkey=Ook7quaiNgiyuhai"
        );
        assert_eq!(resolved.variants[1].height, Some(240));
    }

    #[tokio::test]
    async fn offline_private_and_missing_rooms_say_why() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "https://stripchat.com/api/front/users/user-ids/Joselin_Flower",
            200,
            "application/json",
            r#"{"id":76673634}"#,
        ));
        fixture.exchanges.push(exchange(
            "https://stripchat.com/api/front/v2/models/76673634/cam",
            200,
            "application/json",
            &cam("off", false, "").replace("realdoll", "Joselin_Flower"),
        ));
        fixture.exchanges.push(exchange(
            "https://stripchat.com/api/front/users/user-ids/busy",
            200,
            "application/json",
            r#"{"id":1}"#,
        ));
        fixture.exchanges.push(exchange(
            "https://stripchat.com/api/front/v2/models/1/cam",
            200,
            "application/json",
            &cam("private", true, "").replace("realdoll", "busy"),
        ));
        // A ticket show: the model is public by status while the show's mode withholds
        // the stream.
        fixture.exchanges.push(exchange(
            "https://stripchat.com/api/front/users/user-ids/party",
            200,
            "application/json",
            r#"{"id":2}"#,
        ));
        fixture.exchanges.push(exchange(
            "https://stripchat.com/api/front/v2/models/2/cam",
            200,
            "application/json",
            &cam("public", true, "")
                .replace("realdoll", "party")
                .replace(r#""show":null"#, r#""show":{"mode":"groupShow"}"#),
        ));
        fixture.exchanges.push(exchange(
            "https://stripchat.com/api/front/users/user-ids/zzqqnotexist12345",
            404,
            "application/json",
            r#"{"title":"An error occurred","description":"User zzqqnotexist12345 not found"}"#,
        ));
        let resolver = StripchatResolver::new(Http::replay(fixture));
        let url = |s: &str| Url::parse(s).unwrap();
        let error = resolver
            .resolve(&url("https://stripchat.com/Joselin_Flower"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "Joselin_Flower is not streaming right now"),
            "{error}"
        );
        let error = resolver
            .resolve(&url("https://stripchat.com/busy"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("private show")),
            "{error}"
        );
        let error = resolver
            .resolve(&url("https://stripchat.com/party"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "party is in a group show right now"),
            "{error}"
        );
        assert!(matches!(
            resolver
                .resolve(&url("https://stripchat.com/zzqqnotexist12345"))
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn listings_resolve_to_the_rooms_online() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "https://stripchat.com/api/front/models?limit=60&offset=0&primaryTag=couples&sortBy=viewersRating",
            200,
            "application/json",
            &json!({"models": [
                {"username": "dirtypub", "isLive": true, "status": "groupShow", "groupShowTopic": "All girls fuck Kate"},
                {"username": "realdoll", "isLive": true, "status": "public", "groupShowTopic": ""},
                {"username": "sleeper", "isLive": false, "status": "off"}
            ], "totalCount": 12461})
            .to_string(),
        ));
        let resolver = StripchatResolver::new(Http::replay(fixture));
        let Resolution::Playlist(playlist) = resolver
            .resolve(&Url::parse("https://stripchat.com/couples").unwrap())
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(playlist.title.as_deref(), Some("Stripchat couples online"));
        assert_eq!(playlist.total, Some(12461));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://stripchat.com/dirtypub"
        );
        assert_eq!(
            playlist.entries[0].title.as_deref(),
            Some("dirtypub: All girls fuck Kate")
        );
        assert_eq!(playlist.entries[1].title.as_deref(), Some("realdoll"));
    }

    /// Every example link resolves live: listings with entries, the room with a live
    /// keyed playlist whose first media playlist restores to real segment addresses.
    #[ignore = "reaches the live site: cargo test -- --ignored"]
    #[tokio::test]

    async fn live_examples_resolve() {
        use std::time::Duration;

        let http = Http::new(crate::http::HttpConfig::default());
        let resolver = StripchatResolver::new(http.clone());
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
            assert!(resolver.matches(&url), "{link}");
            let resolution = tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&url))
                .await
                .expect("resolution timed out")
                .unwrap_or_else(|e| panic!("{link}: {e}"));
            match resolution {
                Resolution::Media(resolved) => {
                    assert!(resolved.live, "{link}: not live");
                    let best = resolved
                        .variants
                        .iter()
                        .find(|v| v.is_playable())
                        .unwrap_or_else(|| panic!("{link}: no playable variant"));
                    let media = fetch(&http, &best.url, PLATFORM, BROWSER_UA, &[], MAX_PAGE)
                        .await
                        .unwrap()
                        .text();
                    assert!(media.contains("#EXT-X-MOUFLON:URI:"), "{link}: {media}");
                    let restored =
                        restore_media_playlist(&media, mouflon_secret("Ook7quaiNgiyuhai").unwrap());
                    let segment = restored
                        .lines()
                        .rev()
                        .find(|l| l.starts_with("http"))
                        .unwrap()
                        .to_string();
                    assert!(!segment.contains("media.mp4"), "{segment}");
                    let probed = super::super::probe_file(
                        &http,
                        &Url::parse(&segment).unwrap(),
                        PLATFORM,
                        BROWSER_UA,
                        &[],
                    )
                    .await
                    .unwrap();
                    assert!(
                        probed.status.is_success(),
                        "{segment}: HTTP {}",
                        probed.status
                    );
                    println!(
                        "{link}: {:?} with {} variants; segment {segment} is HTTP {}",
                        resolved.title,
                        resolved.variants.len(),
                        probed.status
                    );
                }
                Resolution::Playlist(playlist) => {
                    assert!(!playlist.entries.is_empty(), "{link}: no entries");
                    println!(
                        "{link}: {:?} with {} entries of {:?}",
                        playlist.title,
                        playlist.entries.len(),
                        playlist.total
                    );
                }
            }
        }
    }

    /// A room online right now is captured through the HLS downloader: the keyed media
    /// playlist is restored to the stream's own segments and the capture plays.
    #[ignore = "reaches the live site: cargo test -- --ignored"]
    #[tokio::test]

    async fn live_rooms_capture_through_the_downloader() {
        use std::time::Duration;

        use crate::download::{DownloadContext, Downloader, hls::HlsDownloader};
        use crate::ffmpeg::Ffmpeg;

        let http = Http::new(crate::http::HttpConfig::default());
        let resolver = StripchatResolver::new(http.clone());
        let Resolution::Playlist(rooms) = resolver
            .resolve(&Url::parse("https://stripchat.com/girls").unwrap())
            .await
            .unwrap()
        else {
            panic!("a listing");
        };
        let mut chosen = None;
        for entry in rooms.entries.iter().take(5) {
            let Ok(Resolution::Media(room)) = resolver.resolve(&entry.url).await else {
                continue;
            };
            let variant = room
                .variants
                .iter()
                .min_by_key(|v| v.height.unwrap_or(u32::MAX))
                .cloned()
                .expect("a live room has variants");
            chosen = Some((entry.url.clone(), variant));
            break;
        }
        let (room_url, variant) = chosen.expect("an online room among the first five listed");
        let dir =
            std::env::temp_dir().join(format!("discoclip-stripchat-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let mut context = DownloadContext::new(200 * 1024 * 1024);
        context.max_live = Duration::from_secs(8);
        context.platform = PLATFORM.to_string();
        let (progress, _watched) = tokio::sync::watch::channel(crate::event::Progress::default());
        let downloaded = HlsDownloader::new(http, ffmpeg.clone())
            .download(&variant, &dir, &context, progress)
            .await
            .unwrap();
        let info = ffmpeg.probe(&downloaded.file.path).await.unwrap();
        println!(
            "{room_url}: captured {:?} from {} ({} bytes)",
            info.duration, variant.url, downloaded.file.size
        );
        assert!(info.video.is_some(), "{info:?}");
        assert!(
            info.duration.is_some_and(|d| d >= Duration::from_secs(3)),
            "{info:?}"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
