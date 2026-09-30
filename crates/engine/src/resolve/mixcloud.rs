//! Mixcloud shows, users and playlists through the GraphQL API the web app reads, asked
//! as Chrome since the host turns other clients away. A show's record names its stream
//! links (a progressive file, an HLS playlist and a DASH manifest) as base64 text XORed
//! with the key the player uses, which is undone here. A user's uploads, favorites,
//! listens and stream, and a playlist's items, are read page by page.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionCheck, SessionSupport, Tag, Variant, VariantKind, clean_title, dash, hls, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind};

pub const PLATFORM: &str = "mixcloud";
const GRAPHQL: &str = "https://app.mixcloud.com/graphql";
const SITE: &str = "https://www.mixcloud.com/";
/// The key the player XORs stream links with.
const STREAM_KEY: &[u8] = b"IFYOUWANTTHEARTISTSTOGETPAIDDONOTDOWNLOADFROMMIXCLOUD";
/// How many shows a listing is read up to.
const LISTING_LIMIT: usize = 200;
/// How many items one page of a listing asks for.
const PAGE_SIZE: usize = 100;

static RE_SLUG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\w.-]+$").unwrap());
/// `/m4a/64/`: the bitrate a progressive stream link names.
static RE_STREAM_BITRATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/(?:m4a|mp3|aac)/(\d+)/").unwrap());

/// The listings of a user's profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserPage {
    Uploads,
    Favorites,
    Listens,
    Stream,
}

impl UserPage {
    fn from_segment(segment: &str) -> Option<Self> {
        Some(match segment {
            "uploads" => UserPage::Uploads,
            "favorites" => UserPage::Favorites,
            "listens" => UserPage::Listens,
            "stream" => UserPage::Stream,
            _ => return None,
        })
    }

    /// The GraphQL field the listing is read from.
    fn field(self) -> &'static str {
        match self {
            UserPage::Uploads => "uploads",
            UserPage::Favorites => "favorites",
            UserPage::Listens => "listens",
            UserPage::Stream => "stream",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Show { user: String, slug: String },
    User { user: String, page: UserPage },
    Playlist { user: String, slug: String },
}

/// First path segments that are pages of the app rather than a user's.
const RESERVED: &[&str] = &[
    "discover",
    "upload",
    "search",
    "live",
    "select",
    "pro",
    "about",
    "settings",
    "categories",
    "tag",
    "jobs",
    "login",
    "signup",
    "signin",
    "logout",
    "help",
    "terms",
    "privacy",
    "api",
    "static",
    "popular",
    "trending",
    "explore",
    "genres",
    "notifications",
    "messages",
    "premium",
    "feed",
    "widget",
    "oembed",
    "developers",
    "press",
    "creators",
];

/// Second path segments under a user that are neither a show nor a listing read here.
const USER_PAGES: &[&str] = &[
    "following",
    "followers",
    "about",
    "playlists",
    "live",
    "select",
    "tracks",
];

fn segments(url: &Url) -> Vec<String> {
    url.path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .map(|s| {
            percent_encoding::percent_decode_str(s)
                .decode_utf8_lossy()
                .into_owned()
        })
        .collect()
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !matches!(
        host.as_str(),
        "mixcloud.com" | "www.mixcloud.com" | "beta.mixcloud.com" | "m.mixcloud.com"
    ) {
        return None;
    }
    let segments = segments(url);
    let user = segments.first()?.clone();
    if !RE_SLUG.is_match(&user) || RESERVED.contains(&user.as_str()) {
        return None;
    }
    match segments.as_slice() {
        [_] => Some(Link::User {
            user,
            page: UserPage::Uploads,
        }),
        [_, second] => {
            if let Some(page) = UserPage::from_segment(second) {
                return Some(Link::User { user, page });
            }
            if USER_PAGES.contains(&second.as_str()) || !RE_SLUG.is_match(second) {
                return None;
            }
            Some(Link::Show {
                user,
                slug: second.clone(),
            })
        }
        [_, second, third] if second == "playlists" => {
            RE_SLUG.is_match(third).then(|| Link::Playlist {
                user,
                slug: third.clone(),
            })
        }
        _ => None,
    }
}

/// A stream link as the API writes it: base64 of the link XORed with the player's key.
pub fn decrypt_stream(encoded: &str) -> Option<Url> {
    let bytes = util::b64_decode(encoded)?;
    let plain: Vec<u8> = bytes
        .iter()
        .zip(STREAM_KEY.iter().cycle())
        .map(|(b, k)| b ^ k)
        .collect();
    let text = String::from_utf8(plain).ok()?;
    Url::parse(text.trim())
        .ok()
        .filter(|u| u.scheme().starts_with("http"))
}

/// A GraphQL string literal.
fn literal(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// The fields a show is listed with.
const NODE: &str = "slug url name audioLength owner { username }";

/// A playlist entry for a show the API lists.
fn entry_of(node: &Value) -> Option<PlaylistEntry> {
    let cloudcast = if node["cloudcast"].is_object() {
        &node["cloudcast"]
    } else {
        node
    };
    let url = util::url_of(&cloudcast["url"], None).or_else(|| {
        let owner = cloudcast["owner"]["username"].as_str()?;
        let slug = cloudcast["slug"].as_str()?;
        Url::parse(&format!("{SITE}{owner}/{slug}/")).ok()
    })?;
    Some(PlaylistEntry {
        url,
        title: cloudcast["name"].as_str().and_then(clean_title),
        duration: util::seconds(&cloudcast["audioLength"]),
    })
}

pub struct MixcloudResolver {
    http: Http,
}

impl MixcloudResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// One GraphQL query, answered as its `data`.
    async fn graphql(&self, query: &str, origin: &Url) -> Result<Value, ResolveError> {
        let url = util::with_query(&Url::parse(GRAPHQL).expect("valid"), &[("query", query)]);
        let response = self
            .http
            .get(url)
            .platform(PLATFORM)
            .impersonate()
            .header("accept", "application/json")
            .header("referer", SITE)
            .send()
            .await?;
        let status = response.status.as_u16();
        match status {
            200..=299 => {}
            404 => return Err(ResolveError::NotFound(origin.clone())),
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            _ => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the API answered HTTP {status}"),
                ));
            }
        }
        let answer: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, format!("GraphQL JSON: {e}")))?;
        if let Some(errors) = answer["errors"].as_array().filter(|e| !e.is_empty()) {
            let messages: Vec<String> = errors
                .iter()
                .filter_map(|e| e["message"].as_str().map(String::from))
                .collect();
            return Err(ResolveError::malformed(
                origin,
                format!("GraphQL errors: {}", messages.join("; ")),
            ));
        }
        Ok(answer["data"].clone())
    }

    /// The variants a show's stream links make: the progressive file, the HLS renditions
    /// and the DASH representations.
    async fn variants_of(&self, stream_info: &Value, duration: Option<Duration>) -> Vec<Variant> {
        let mut variants: Vec<Variant> = Vec::new();
        if let Some(file) = stream_info["url"].as_str().and_then(decrypt_stream) {
            let container = super::path_extension(&file)
                .as_deref()
                .and_then(Container::from_extension)
                .unwrap_or(Container::M4a);
            let mut variant = Variant::new(file.clone(), VariantKind::File);
            variant.audio_only = true;
            variant.audio = Some(match container {
                Container::Mp3 => AudioCodec::Mp3,
                _ => AudioCodec::Aac,
            });
            variant.container = Some(container);
            variant.bitrate = util::search(&RE_STREAM_BITRATE, file.as_str())
                .and_then(|k| k.parse::<u64>().ok())
                .map(|k| k * 1000);
            variant.duration = duration;
            variant.format_id = Some("http".into());
            variant.label = variant.bitrate.map(|b| format!("{}k", b / 1000));
            variants.push(variant);
        }
        if let Some(playlist) = stream_info["hlsUrl"].as_str().and_then(decrypt_stream) {
            match hls::expand(&self.http, &playlist, PLATFORM, BROWSER_UA, &[]).await {
                Ok(expanded) => {
                    for mut variant in expanded.variants {
                        variant.audio_only = true;
                        if variant.audio.is_none() {
                            variant.audio = Some(AudioCodec::Aac);
                        }
                        if variant.duration.is_none() {
                            variant.duration = expanded.duration.or(duration);
                        }
                        variant.format_id = Some(match &variant.label {
                            Some(label) => format!("hls-{label}"),
                            None => "hls".into(),
                        });
                        variants.push(variant);
                    }
                }
                Err(error) => {
                    tracing::debug!(%playlist, "Mixcloud HLS playlist not read: {error}");
                }
            }
        }
        if let Some(manifest) = stream_info["dashUrl"].as_str().and_then(decrypt_stream) {
            match dash::expand(&self.http, &manifest, PLATFORM, BROWSER_UA, &[]).await {
                Ok(expanded) => {
                    for mut variant in expanded.variants {
                        variant.audio_only = true;
                        if variant.audio.is_none() {
                            variant.audio = Some(AudioCodec::Aac);
                        }
                        if variant.duration.is_none() {
                            variant.duration = expanded.duration.or(duration);
                        }
                        variant.format_id = Some(match &variant.format_id {
                            Some(id) => format!("dash-{id}"),
                            None => "dash".into(),
                        });
                        variants.push(variant);
                    }
                }
                Err(error) => {
                    tracing::debug!(%manifest, "Mixcloud DASH manifest not read: {error}");
                }
            }
        }
        variants
    }

    async fn resolve_show(
        &self,
        user: &str,
        slug: &str,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let query = format!(
            "{{ cloudcastLookup(lookup: {{username: {}, slug: {}}}) {{ id slug url name description audioLength publishDate isExclusive restrictedReason owner {{ displayName url username }} picture(width: 1024, height: 1024) {{ url }} streamInfo {{ dashUrl hlsUrl url }} }} }}",
            literal(user),
            literal(slug)
        );
        let data = self.graphql(&query, origin).await?;
        let show = &data["cloudcastLookup"];
        if !show.is_object() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        match show["restrictedReason"].as_str() {
            None | Some("") => {}
            Some("tracklist") => {
                return Err(ResolveError::unavailable(
                    origin,
                    "the show is withheld in this region by its licensing",
                ));
            }
            Some("repeat_play") => {
                return Err(ResolveError::unavailable(
                    origin,
                    "the play limit for the show is reached",
                ));
            }
            Some(reason) => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the show is restricted ({reason})"),
                ));
            }
        }
        let duration = util::seconds(&show["audioLength"]);
        let variants = self.variants_of(&show["streamInfo"], duration).await;
        if variants.is_empty() {
            if util::boolean(&show["isExclusive"]).unwrap_or(false) {
                return Err(ResolveError::login_required(
                    origin,
                    PLATFORM,
                    "the show is exclusive to the creator's subscribers",
                ));
            }
            return Err(ResolveError::unavailable(origin, "the show has no stream"));
        }
        let owner = &show["owner"];
        let mut resolved = Resolved::of(PLATFORM, MediaKind::Audio);
        resolved.id = Some(format!(
            "{}_{}",
            owner["username"].as_str().unwrap_or(user),
            show["slug"].as_str().unwrap_or(slug)
        ));
        resolved.title = show["name"].as_str().and_then(clean_title);
        resolved.description = util::text(&show["description"]);
        resolved.uploader = owner["displayName"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| owner["username"].as_str().and_then(clean_title));
        resolved.uploader_url = util::url_of(&owner["url"], None);
        resolved.uploaded_at = util::time(&show["publishDate"]);
        resolved.duration = duration.or_else(|| variants.iter().find_map(|v| v.duration));
        resolved.thumbnail = util::url_of(&show["picture"]["url"], None);
        resolved.webpage_url = util::url_of(&show["url"], None).or_else(|| Some(origin.clone()));
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    /// A listing read page by page: `root` is the lookup that holds it, `field` the
    /// connection, `node` the fields of each item.
    async fn listing(
        &self,
        root: &str,
        head: &str,
        field: &str,
        node: &str,
        origin: &Url,
    ) -> Result<(Value, Vec<PlaylistEntry>, Option<usize>), ResolveError> {
        let mut entries: Vec<PlaylistEntry> = Vec::new();
        let mut after: Option<String> = None;
        let mut record = Value::Null;
        let mut total: Option<usize> = None;
        loop {
            let cursor = after
                .as_deref()
                .map(|c| format!(", after: {}", literal(c)))
                .unwrap_or_default();
            let query = format!(
                "{{ {root} {{ {head} {field}(first: {PAGE_SIZE}{cursor}) {{ totalCount edges {{ node {{ {node} }} }} pageInfo {{ endCursor hasNextPage }} }} }} }}"
            );
            let data = self.graphql(&query, origin).await?;
            let Some(lookup) = data.as_object().and_then(|d| d.values().next()) else {
                return Err(ResolveError::malformed(
                    origin,
                    "the API answered without data",
                ));
            };
            if !lookup.is_object() {
                return Err(ResolveError::NotFound(origin.clone()));
            }
            let connection = &lookup[field];
            if total.is_none() {
                total = util::uint(&connection["totalCount"]).map(|n| n as usize);
            }
            for edge in connection["edges"].as_array().into_iter().flatten() {
                if let Some(entry) = entry_of(&edge["node"])
                    && !entries.iter().any(|e| e.url == entry.url)
                {
                    entries.push(entry);
                }
            }
            if record.is_null() {
                record = lookup.clone();
            }
            let page_info = &connection["pageInfo"];
            let more = util::boolean(&page_info["hasNextPage"]).unwrap_or(false);
            after = page_info["endCursor"].as_str().map(String::from);
            if !more || after.is_none() || entries.len() >= LISTING_LIMIT {
                break;
            }
        }
        entries.truncate(LISTING_LIMIT);
        Ok((record, entries, total))
    }

    async fn resolve_user(
        &self,
        user: &str,
        page: UserPage,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let root = format!("userLookup(lookup: {{username: {}}})", literal(user));
        let (record, entries, total) = self
            .listing(&root, "username displayName", page.field(), NODE, origin)
            .await?;
        if entries.is_empty() {
            return Err(ResolveError::unavailable(
                origin,
                format!("the user's {} are empty", page.field()),
            ));
        }
        let username = record["username"].as_str().unwrap_or(user);
        let name = record["displayName"]
            .as_str()
            .and_then(clean_title)
            .unwrap_or_else(|| username.to_string());
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(format!("{username}_{}", page.field())),
            title: Some(format!("{name} ({})", page.field())),
            total: total
                .filter(|n| *n >= entries.len())
                .or(Some(entries.len())),
            entries,
        }))
    }

    async fn resolve_playlist(
        &self,
        user: &str,
        slug: &str,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let root = format!(
            "playlistLookup(lookup: {{username: {}, slug: {}}})",
            literal(user),
            literal(slug)
        );
        let node = format!("cloudcast {{ {NODE} }}");
        let (record, entries, total) = self
            .listing(&root, "name slug", "items", &node, origin)
            .await?;
        if entries.is_empty() {
            return Err(ResolveError::unavailable(origin, "the playlist is empty"));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(format!(
                "{user}_{}",
                record["slug"].as_str().unwrap_or(slug)
            )),
            title: record["name"].as_str().and_then(clean_title),
            total: total
                .filter(|n| *n >= entries.len())
                .or(Some(entries.len())),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for MixcloudResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Mixcloud",
            hosts: &["mixcloud.com"],
            features: &["shows", "users", "favorites", "listens", "playlists"],
            formats: &["m4a", "hls", "dash"],
            media: &[MediaKind::Audio],
            tags: &[Tag::Music],
            session: SessionSupport::Optional,
            examples: &[
                "https://www.mixcloud.com/dholbach/cryptkeeper/",
                "https://www.mixcloud.com/dholbach/",
                "https://www.mixcloud.com/dholbach/favorites/",
                "https://www.mixcloud.com/maxvibes/playlists/jazzcat-on-ness-radio/",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Show { user, slug } => self.resolve_show(&user, &slug, url).await,
            Link::User { user, page } => self.resolve_user(&user, page, url).await,
            Link::Playlist { user, slug } => self.resolve_playlist(&user, &slug, url).await,
        }
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        if self.http.jar(PLATFORM).is_empty() {
            return Ok(SessionCheck::LoggedOut);
        }
        let origin = Url::parse(SITE).expect("valid");
        let data = self
            .graphql("{ viewer { me { username displayName } } }", &origin)
            .await?;
        Ok(
            match data["viewer"]["me"]["username"]
                .as_str()
                .filter(|u| !u.is_empty())
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

    /// A stream link as the API writes it.
    fn encrypt(link: &str) -> String {
        let bytes: Vec<u8> = link
            .bytes()
            .zip(STREAM_KEY.iter().cycle())
            .map(|(b, k)| b ^ k)
            .collect();
        util::b64_encode(&bytes)
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://www.mixcloud.com/dholbach/cryptkeeper/"),
            Some(Link::Show {
                user: "dholbach".into(),
                slug: "cryptkeeper".into()
            })
        );
        assert_eq!(
            link("http://beta.mixcloud.com/RedLightRadio/nosedrip-15-red-light-radio-01-18-2016/"),
            Some(Link::Show {
                user: "RedLightRadio".into(),
                slug: "nosedrip-15-red-light-radio-01-18-2016".into()
            })
        );
        assert_eq!(
            link("https://www.mixcloud.com/dholbach/"),
            Some(Link::User {
                user: "dholbach".into(),
                page: UserPage::Uploads
            })
        );
        assert_eq!(
            link("https://www.mixcloud.com/dholbach/favorites/"),
            Some(Link::User {
                user: "dholbach".into(),
                page: UserPage::Favorites
            })
        );
        assert_eq!(
            link("https://www.mixcloud.com/FirstEar/stream/"),
            Some(Link::User {
                user: "FirstEar".into(),
                page: UserPage::Stream
            })
        );
        assert_eq!(
            link("https://www.mixcloud.com/maxvibes/playlists/jazzcat-on-ness-radio/"),
            Some(Link::Playlist {
                user: "maxvibes".into(),
                slug: "jazzcat-on-ness-radio".into()
            })
        );
        assert_eq!(link("https://www.mixcloud.com/discover/jazz/"), None);
        assert_eq!(link("https://www.mixcloud.com/dholbach/followers/"), None);
        assert_eq!(link("https://www.mixcloud.com/"), None);
        assert_eq!(link("https://example.com/dholbach/cryptkeeper/"), None);
    }

    #[test]
    fn stream_links_are_decrypted() {
        let link = "https://dl.mixcloud.stream/secure/c/m4a/64/6/f/c/d/d610.m4a?sig=ZoVzO9AXXMDKP4gxB6y3uQ";
        assert_eq!(decrypt_stream(&encrypt(link)).unwrap().as_str(), link);
        assert_eq!(decrypt_stream("not base64!"), None);
    }

    #[tokio::test]
    async fn shows_resolve_to_their_decrypted_streams() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://app.mixcloud.com/graphql?query=x",
            200,
            "application/json",
            json!({"data": {"cloudcastLookup": {
                "id": "Q2xvdWRjYXN0OjEyMw==", "slug": "cryptkeeper", "url": "https://www.mixcloud.com/dholbach/cryptkeeper/",
                "name": "Cryptkeeper", "description": "After quite a long silence from myself, finally another Drum'n'Bass mix.",
                "audioLength": 3723, "publishDate": "2011-11-15T12:19:38Z", "isExclusive": false, "restrictedReason": null,
                "owner": {"displayName": "dholbach", "url": "https://www.mixcloud.com/dholbach/", "username": "dholbach"},
                "picture": {"url": "https://thumbnailer.mixcloud.com/unsafe/1024x1024/extaudio/a/4/1/3/7e83.jpg"},
                "streamInfo": {
                    "url": encrypt("https://dl.mixcloud.stream/secure/c/m4a/64/6/f/c/d/d610.m4a?sig=Zo"),
                    "hlsUrl": encrypt("https://aod.mixcloud.stream/secure/hls/6/f/c/d/d610.m4a/streamindex-a1.m3u8"),
                    "dashUrl": null
                }
            }}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://aod.mixcloud.stream/secure/hls/6/f/c/d/d610.m4a/streamindex-a1.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-TARGETDURATION:10\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:10.031,\nseg-1-a1.ts\n#EXTINF:9.985,\nseg-2-a1.ts\n#EXT-X-ENDLIST\n".into(),
        ));
        fixture.exchanges.push(get(
            "https://app.mixcloud.com/graphql?query=y",
            200,
            "application/json",
            json!({"data": {"cloudcastLookup": null}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://app.mixcloud.com/graphql?query=z",
            200,
            "application/json",
            json!({"data": {"cloudcastLookup": {
                "id": "x", "slug": "members-only", "url": "https://www.mixcloud.com/dholbach/members-only/", "name": "Members only",
                "audioLength": 100, "isExclusive": true, "restrictedReason": null, "owner": {"username": "dholbach"},
                "streamInfo": {"url": null, "hlsUrl": null, "dashUrl": null}
            }}}).to_string(),
        ));
        let resolver = MixcloudResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.mixcloud.com/dholbach/cryptkeeper/").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.media, MediaKind::Audio);
        assert_eq!(resolved.id.as_deref(), Some("dholbach_cryptkeeper"));
        assert_eq!(resolved.title.as_deref(), Some("Cryptkeeper"));
        assert_eq!(resolved.uploader.as_deref(), Some("dholbach"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(3723)));
        assert!(resolved.uploaded_at.is_some());
        assert!(resolved.thumbnail.is_some());
        assert_eq!(resolved.variants.len(), 2, "the file and the HLS playlist");
        let file = &resolved.variants[0];
        assert_eq!(
            file.url.as_str(),
            "https://dl.mixcloud.stream/secure/c/m4a/64/6/f/c/d/d610.m4a?sig=Zo"
        );
        assert_eq!(file.container, Some(Container::M4a));
        assert_eq!(file.audio, Some(AudioCodec::Aac));
        assert_eq!(file.bitrate, Some(64_000));
        assert!(file.audio_only);
        assert_eq!(file.format_id.as_deref(), Some("http"));
        let stream = &resolved.variants[1];
        assert_eq!(stream.kind, VariantKind::Hls);
        assert!(stream.audio_only);
        assert!(
            (stream.duration.unwrap().as_secs_f64() - 20.016).abs() < 0.001,
            "{:?}",
            stream.duration
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.mixcloud.com/dholbach/gone/").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(&Url::parse("https://www.mixcloud.com/dholbach/members-only/").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(error, ResolveError::LoginRequired { .. }),
            "{error}"
        );
    }

    #[tokio::test]
    async fn users_and_playlists_list_their_shows_page_by_page() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://app.mixcloud.com/graphql?query=1",
            200,
            "application/json",
            json!({"data": {"userLookup": {"username": "dholbach", "displayName": "Daniel Holbach", "uploads": {
                "totalCount": 3,
                "edges": [
                    {"node": {"slug": "cryptkeeper", "url": "https://www.mixcloud.com/dholbach/cryptkeeper/", "name": "Cryptkeeper", "audioLength": 3723, "owner": {"username": "dholbach"}}},
                    {"node": {"slug": "second", "url": "https://www.mixcloud.com/dholbach/second/", "name": "Second", "audioLength": 100, "owner": {"username": "dholbach"}}}
                ],
                "pageInfo": {"endCursor": "YXJyYXljb25uZWN0aW9uOjE=", "hasNextPage": true}
            }}}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://app.mixcloud.com/graphql?query=2",
            200,
            "application/json",
            json!({"data": {"userLookup": {"username": "dholbach", "displayName": "Daniel Holbach", "uploads": {
                "totalCount": 3,
                "edges": [
                    {"node": {"slug": "third", "url": null, "name": "Third", "audioLength": 50, "owner": {"username": "dholbach"}}}
                ],
                "pageInfo": {"endCursor": "YXJyYXljb25uZWN0aW9uOjI=", "hasNextPage": false}
            }}}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://app.mixcloud.com/graphql?query=3",
            200,
            "application/json",
            json!({"data": {"playlistLookup": {"name": "Ness Radio sessions", "slug": "jazzcat-on-ness-radio", "items": {
                "totalCount": 58,
                "edges": [
                    {"node": {"cloudcast": {"slug": "jazzcat-on-ness-radio-programme-01", "url": "https://www.mixcloud.com/maxvibes/jazzcat-on-ness-radio-programme-01/", "name": "Jazzcat on Ness Radio programme 01", "audioLength": 3600, "owner": {"username": "maxvibes"}}}}
                ],
                "pageInfo": {"endCursor": "x", "hasNextPage": false}
            }}}}).to_string(),
        ));
        let resolver = MixcloudResolver::new(Http::replay(fixture));
        let Resolution::Playlist(uploads) = resolver
            .resolve(&Url::parse("https://www.mixcloud.com/dholbach/").unwrap())
            .await
            .unwrap()
        else {
            panic!("uploads are a playlist");
        };
        assert_eq!(uploads.id.as_deref(), Some("dholbach_uploads"));
        assert_eq!(uploads.title.as_deref(), Some("Daniel Holbach (uploads)"));
        assert_eq!(uploads.total, Some(3));
        assert_eq!(uploads.entries.len(), 3);
        assert_eq!(uploads.entries[0].title.as_deref(), Some("Cryptkeeper"));
        assert_eq!(uploads.entries[0].duration, Some(Duration::from_secs(3723)));
        assert_eq!(
            uploads.entries[2].url.as_str(),
            "https://www.mixcloud.com/dholbach/third/"
        );
        let Resolution::Playlist(playlist) = resolver
            .resolve(
                &Url::parse("https://www.mixcloud.com/maxvibes/playlists/jazzcat-on-ness-radio/")
                    .unwrap(),
            )
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(
            playlist.id.as_deref(),
            Some("maxvibes_jazzcat-on-ness-radio")
        );
        assert_eq!(playlist.title.as_deref(), Some("Ness Radio sessions"));
        assert_eq!(playlist.total, Some(58));
        assert_eq!(playlist.entries.len(), 1);
    }

    /// Every example link resolves live: shows to audio with a playable variant, lists
    /// to entries.
    #[ignore = "reaches the live site: cargo test -- --ignored"]
    #[tokio::test]

    async fn live_examples_resolve() {
        let resolver = MixcloudResolver::new(Http::new(crate::http::HttpConfig::default()));
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
            let resolution = tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&url))
                .await
                .expect("resolution timed out")
                .unwrap_or_else(|e| panic!("{link}: {e}"));
            match resolution {
                Resolution::Media(resolved) => {
                    assert_eq!(resolved.media, MediaKind::Audio, "{link}");
                    assert!(
                        resolved.variants.iter().any(|v| v.is_playable()),
                        "{link}: no playable variant"
                    );
                    println!(
                        "{link}: audio {:?} with {} variants {:?}",
                        resolved.title,
                        resolved.variants.len(),
                        resolved
                            .variants
                            .iter()
                            .map(|v| v.format_id.clone())
                            .collect::<Vec<_>>()
                    );
                }
                Resolution::Playlist(playlist) => {
                    assert!(!playlist.entries.is_empty(), "{link}: no entries");
                    println!(
                        "{link}: playlist {:?} with {} entries (total {:?})",
                        playlist.title,
                        playlist.entries.len(),
                        playlist.total
                    );
                }
            }
        }
        // The session query is one the schema knows: without cookies the viewer has no
        // account, and the check reports as much.
        let origin = Url::parse(SITE).unwrap();
        let data = resolver
            .graphql("{ viewer { me { username displayName } } }", &origin)
            .await
            .unwrap();
        assert!(data["viewer"].is_object(), "{data}");
        assert!(data["viewer"]["me"].is_null(), "{data}");
        let session = resolver.check_session().await.unwrap();
        assert_eq!(session, SessionCheck::LoggedOut);
    }
}
