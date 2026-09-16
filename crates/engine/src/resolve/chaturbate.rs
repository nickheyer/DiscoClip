//! Chaturbate rooms and listings: a room's HLS playlist comes from the endpoint the
//! room page polls, and the listing pages (the front page and the gender tabs) come from
//! the room-list API the site renders them with. Both are read as Chrome, the way the
//! site expects its own scripts to call them.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, clean_title, hls, status_error, util, Variant,
};
use crate::http::{BROWSER_UA, Http};

pub const PLATFORM: &str = "chaturbate";
/// How many rooms a listing is read up to.
const LISTING_LIMIT: usize = 90;

static RE_HOST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[a-z0-9-]+\.)?chaturbate\.(com|eu|global)$").unwrap());
/// `/{room}/`, or `/fullvideo/?b={room}`.
static RE_ROOM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/([A-Za-z0-9_]+)/?$").unwrap());
/// `/female-cams/`, `/couple-cams/`… the gender listings.
static RE_LISTING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(female|male|couple|trans)-cams/?$").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Room { tld: String, room: String },
    /// A listing of rooms online: every one, or one gender.
    Listing { tld: String, gender: Option<String> },
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let tld = RE_HOST.captures(&host)?[1].to_string();
    let path = url.path();
    if path.is_empty() || path == "/" {
        return Some(Link::Listing { tld, gender: None });
    }
    if let Some(caps) = RE_LISTING.captures(path) {
        return Some(Link::Listing {
            tld,
            gender: Some(caps[1].to_string()),
        });
    }
    if path.trim_end_matches('/') == "/fullvideo" {
        let room = util::query_param(url, "b").filter(|b| !b.is_empty())?;
        return Some(Link::Room { tld, room });
    }
    let caps = RE_ROOM.captures(path)?;
    let room = caps[1].to_string();
    if matches!(
        room.as_str(),
        "login" | "signup" | "tags" | "affiliates" | "contest" | "apps" | "terms" | "privacy"
            | "dmca" | "security" | "careers" | "billing" | "support" | "discover" | "api"
            | "accounts" | "auth" | "tipping" | "followed-cams" | "spy-on-cams"
    ) {
        return None;
    }
    Some(Link::Room { tld, room })
}

/// The gender code the room-list API filters by.
fn gender_code(gender: &str) -> &'static str {
    match gender {
        "female" => "f",
        "male" => "m",
        "couple" => "c",
        _ => "t",
    }
}

/// What a room's status means for the caller.
fn status_error_of(status: &str, room: &str, url: &Url) -> ResolveError {
    match status {
        "offline" => ResolveError::unavailable(url, format!("{room} is not streaming right now")),
        "private" | "hidden" | "away" | "group" => {
            ResolveError::unavailable(url, format!("{room} is in a {status} show right now"))
        }
        "password protected" => ResolveError::unavailable(url, format!("{room} is password protected")),
        "public" => ResolveError::unavailable(url, "the stream is withheld for this location"),
        "" => ResolveError::NotFound(url.clone()),
        other => ResolveError::unavailable(url, format!("the room answered with status {other}")),
    }
}

/// `initialRoomDossier = "{…}"`: the room's state on its page, JSON in a JS string.
static RE_DOSSIER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"initialRoomDossier\s*=\s*(?:"((?:[^"\]|\.)+)"|'((?:[^'\]|\.)+)')"#).unwrap()
});
/// `'https://….m3u8…'`: a stream link in the page's escaped scripts.
static RE_ESCAPED_M3U8: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\u002[27](https?://.+?\.m3u8.*?)\u002[27]").unwrap());
/// `"https://….m3u8…"`: a stream link in the page's scripts.
static RE_PLAIN_M3U8: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"["'](https?://[^"']+?\.m3u8[^"']*?)["']"#).unwrap());
/// The reason a room page gives for showing no stream.
static RE_PAGE_ERROR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<span[^>]+class=["']desc_span["'][^>]*>([^<]+)</span>|<div[^>]+id=["']defchat["'][^>]*>\s*<p><strong>([^<]+)<"#)
        .unwrap()
});

/// Whether an API status settles the room's state, so the page has nothing more to
/// say: offline, private, hidden, away, or a public room the API refuses.
fn is_final_status(status: &str) -> bool {
    matches!(
        status,
        "offline" | "private" | "hidden" | "away" | "password protected" | "public"
    )
}

/// The stream links a room page names: in the room dossier, in escaped script strings,
/// or in plain ones; each with its `_fast` twin and the plain one.
pub fn page_playlists(html: &str) -> Vec<Url> {
    let mut found: Vec<String> = Vec::new();
    if let Some(caps) = RE_DOSSIER.captures(html)
        && let Some(raw) = caps.get(1).or_else(|| caps.get(2))
        && let Some(dossier) = serde_json::from_str::<Value>(&super::page::unescape_json_string(raw.as_str())).ok()
        && let Some(source) = dossier["hls_source"].as_str().filter(|s| s.starts_with("http"))
    {
        found.push(source.to_string());
    }
    if found.is_empty() {
        for caps in RE_ESCAPED_M3U8.captures_iter(html) {
            found.push(super::page::unescape_json_string(&caps[1]));
        }
    }
    if found.is_empty() {
        for caps in RE_PLAIN_M3U8.captures_iter(html) {
            found.push(caps[1].to_string());
        }
    }
    let mut playlists: Vec<Url> = Vec::new();
    for link in found {
        for candidate in [link.clone(), link.replace("_fast", "")] {
            if let Ok(url) = Url::parse(&candidate)
                && !playlists.contains(&url)
            {
                playlists.push(url);
            }
        }
    }
    playlists
}

/// The reason a room page shows no stream, when it says.
pub fn page_error(html: &str) -> Option<String> {
    if let Some(caps) = RE_PAGE_ERROR.captures(html)
        && let Some(reason) = caps.get(1).or_else(|| caps.get(2))
    {
        return clean_title(&util::clean_html(reason.as_str()));
    }
    ["Room is currently offline", "offline_tipping", "tip_offline"]
        .iter()
        .any(|marker| html.contains(marker))
        .then(|| "Room is currently offline".to_string())
}

pub struct ChaturbateResolver {
    http: Http,
}

impl ChaturbateResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// The stream links a room's page names, read the way a browser would.
    async fn playlists_from_page(&self, site: &str, room: &str, origin: &Url) -> Result<Vec<Url>, ResolveError> {
        let page_url = Url::parse(&format!("{site}{room}/")).expect("valid");
        let fetched = super::fetch_as_browser(&self.http, &page_url, PLATFORM, &[], MAX_PAGE).await?;
        if let Some(error) = status_error(fetched.status, origin) {
            return Err(error);
        }
        let html = fetched.text();
        let playlists = page_playlists(&html);
        if playlists.is_empty() {
            return Err(match page_error(&html) {
                Some(reason) => ResolveError::unavailable(origin, reason),
                None => ResolveError::unavailable(origin, format!("the page of {room} names no stream")),
            });
        }
        Ok(playlists)
    }

    async fn resolve_room(&self, tld: &str, room: &str, url: &Url) -> Result<Resolution, ResolveError> {
        let site = format!("https://chaturbate.{tld}/");
        let api = Url::parse(&format!("{site}get_edge_hls_url_ajax/")).expect("valid");
        let response = self
            .http
            .post(api)
            .platform(PLATFORM)
            .impersonate()
            .header("accept", "application/json")
            .header("x-requested-with", "XMLHttpRequest")
            .header("referer", &format!("{site}{room}/"))
            .form(&[("room_slug", room)])
            .send()
            .await?;
        if let Some(error) = status_error(response.status, url) {
            return Err(error);
        }
        let answer: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(url, format!("room JSON: {e}")))?;
        let playlists: Vec<Url> = match util::url_of(&answer["url"], None) {
            Some(playlist) => vec![playlist],
            None => {
                let status = answer["room_status"].as_str().unwrap_or("");
                if is_final_status(status) {
                    return Err(status_error_of(status, room, url));
                }
                // Any other status: the room page names the stream itself.
                tracing::debug!(room, status, "chaturbate API named no stream; reading the room page");
                self.playlists_from_page(&site, room, url).await?
            }
        };
        let mut expanded_variants = Vec::new();
        let mut subtitles = Vec::new();
        let mut failure = None;
        for playlist in &playlists {
            match hls::expand(&self.http, playlist, PLATFORM, BROWSER_UA, &[]).await {
                Ok(expanded) => {
                    let speed = if playlist.as_str().contains("_fast") {
                        "fast"
                    } else if playlist.as_str().contains("_slow") {
                        "slow"
                    } else {
                        "hls"
                    };
                    for mut variant in expanded.variants {
                        variant.live = true;
                        variant.format_id = Some(match &variant.label {
                            Some(label) => format!("{speed}-{label}"),
                            None => speed.to_string(),
                        });
                        if !expanded_variants.iter().any(|v: &Variant| v.url == variant.url) {
                            expanded_variants.push(variant);
                        }
                    }
                    subtitles.extend(expanded.subtitles);
                }
                Err(error) => failure = Some(error),
            }
        }
        if expanded_variants.is_empty() {
            return Err(failure.unwrap_or_else(|| {
                ResolveError::unavailable(url, format!("{room} is not streaming right now"))
            }));
        }
        let expanded = hls::Expanded {
            variants: expanded_variants,
            subtitles,
            duration: None,
            live: true,
        };
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(room.to_string());
        resolved.title = clean_title(room);
        resolved.uploader = clean_title(room);
        resolved.uploader_url = Url::parse(&format!("{site}{room}/")).ok();
        resolved.thumbnail = Url::parse(&format!("https://roomimg.stream.highwebmedia.com/ri/{room}.jpg")).ok();
        resolved.webpage_url = Url::parse(&format!("{site}{room}/")).ok();
        resolved.age_limit = Some(18);
        resolved.live = true;
        resolved.variants = expanded.variants;
        Ok(Resolution::from(resolved))
    }

    async fn resolve_listing(&self, tld: &str, gender: Option<&str>, url: &Url) -> Result<Resolution, ResolveError> {
        let site = format!("https://chaturbate.{tld}/");
        let mut query = vec![("limit", LISTING_LIMIT.to_string()), ("offset", "0".to_string())];
        if let Some(gender) = gender {
            query.push(("genders", gender_code(gender).to_string()));
        }
        let pairs: Vec<(&str, &str)> = query.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let api = util::with_query(
            &Url::parse(&format!("{site}api/ts/roomlist/room-list/")).expect("valid"),
            &pairs,
        );
        let response = self
            .http
            .get(api)
            .platform(PLATFORM)
            .impersonate()
            .header("accept", "application/json")
            .header("referer", &site)
            .send()
            .await?;
        if let Some(error) = status_error(response.status, url) {
            return Err(error);
        }
        let answer: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(url, format!("room list JSON: {e}")))?;
        let entries: Vec<PlaylistEntry> = answer["rooms"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|room| {
                let username = util::text(&room["username"])?;
                Some(PlaylistEntry {
                    url: Url::parse(&format!("{site}{username}/")).ok()?,
                    title: room["room_subject"]
                        .as_str()
                        .and_then(clean_title)
                        .map(|subject| format!("{username}: {subject}"))
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
            id: Some(gender.unwrap_or("all").to_string()),
            title: Some(match gender {
                Some(gender) => format!("Chaturbate {gender} cams"),
                None => "Chaturbate cams".to_string(),
            }),
            total: util::uint(&answer["total_count"]).map(|n| n as usize).or(Some(entries.len())),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for ChaturbateResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Chaturbate",
            hosts: &["chaturbate.com", "chaturbate.eu", "chaturbate.global"],
            features: &["live", "listings"],
            formats: &["hls"],
            session: SessionSupport::None,
            examples: &["https://chaturbate.com/", "https://chaturbate.com/female-cams/"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Room { tld, room } => self.resolve_room(&tld, &room, url).await,
            Link::Listing { tld, gender } => self.resolve_listing(&tld, gender.as_deref(), url).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};
    use serde_json::json;

    fn exchange(method: &str, url: &str, status: u16, content_type: &str, body: String) -> Exchange {
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

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let room = |tld: &str, room: &str| Some(Link::Room { tld: tld.into(), room: room.into() });
        assert_eq!(link("https://www.chaturbate.com/siswet19/"), room("com", "siswet19"));
        assert_eq!(link("https://chaturbate.com/fullvideo/?b=caylin"), room("com", "caylin"));
        assert_eq!(link("https://en.chaturbate.com/siswet19/"), room("com", "siswet19"));
        assert_eq!(link("https://chaturbate.eu/siswet19/"), room("eu", "siswet19"));
        assert_eq!(link("https://chaturbate.global/siswet19"), room("global", "siswet19"));
        assert_eq!(link("https://chaturbate.com/"), Some(Link::Listing { tld: "com".into(), gender: None }));
        assert_eq!(
            link("https://chaturbate.com/female-cams/"),
            Some(Link::Listing { tld: "com".into(), gender: Some("female".into()) })
        );
        assert_eq!(link("https://chaturbate.com/login/"), None);
        assert_eq!(link("https://chaturbate.com/tags/anal/"), None);
        assert_eq!(link("https://example.com/siswet19/"), None);
    }

    #[tokio::test]
    async fn rooms_resolve_to_their_live_playlist_or_say_why_not() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "POST",
            "https://chaturbate.com/get_edge_hls_url_ajax/",
            200,
            "application/json",
            json!({"success": true, "url": "https://edge9-sea.live.mmcdn.com/v1/edge/streams/origin.siswet19.x/llhls.m3u8?token=t", "room_status": "public"}).to_string(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://edge9-sea.live.mmcdn.com/v1/edge/streams/origin.siswet19.x/llhls.m3u8?token=t",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1920x1080\nhttps://edge9-sea.live.mmcdn.com/v1/edge/streams/origin.siswet19.x/1080.m3u8?token=t\n".into(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://edge9-sea.live.mmcdn.com/v1/edge/streams/origin.siswet19.x/1080.m3u8?token=t",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2.0,\n0.ts\n".into(),
        ));
        fixture.exchanges.push(exchange(
            "POST",
            "https://chaturbate.com/get_edge_hls_url_ajax/",
            200,
            "application/json",
            json!({"success": true, "url": "", "room_status": "offline", "hidden_message": ""}).to_string(),
        ));
        fixture.exchanges.push(exchange(
            "POST",
            "https://chaturbate.eu/get_edge_hls_url_ajax/",
            200,
            "application/json",
            json!({"success": true, "url": "", "room_status": "private"}).to_string(),
        ));
        let resolver = ChaturbateResolver::new(Http::replay(fixture));
        let url = Url::parse("https://chaturbate.com/siswet19/").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("siswet19"));
        assert!(resolved.live);
        assert_eq!(resolved.age_limit, Some(18));
        assert_eq!(resolved.variants.len(), 1);
        assert!(resolved.variants[0].live);
        assert_eq!(resolved.variants[0].height, Some(1080));
        let error = resolver
            .resolve(&Url::parse("https://chaturbate.com/sleeper/").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "sleeper is not streaming right now"),
            "{error}"
        );
        let error = resolver
            .resolve(&Url::parse("https://chaturbate.eu/busy/").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("private show")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn listings_resolve_to_the_rooms_online() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "GET",
            "https://chaturbate.com/api/ts/roomlist/room-list/?limit=90&offset=0&genders=f",
            200,
            "application/json",
            json!({"rooms": [
                {"username": "sierrasexy6", "room_subject": "Tuesday night #18", "gender": "f"},
                {"username": "emilychoi", "room_subject": "", "gender": "f"},
                {"gender": "f"}
            ], "total_count": 2500}).to_string(),
        ));
        let resolver = ChaturbateResolver::new(Http::replay(fixture));
        let Resolution::Playlist(playlist) = resolver
            .resolve(&Url::parse("https://chaturbate.com/female-cams/").unwrap())
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(playlist.title.as_deref(), Some("Chaturbate female cams"));
        assert_eq!(playlist.total, Some(2500));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(playlist.entries[0].url.as_str(), "https://chaturbate.com/sierrasexy6/");
        assert_eq!(playlist.entries[0].title.as_deref(), Some("sierrasexy6: Tuesday night #18"));
        assert_eq!(playlist.entries[1].title.as_deref(), Some("emilychoi"));
    }
}
