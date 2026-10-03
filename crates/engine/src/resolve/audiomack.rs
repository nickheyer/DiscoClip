//! Audiomack songs, albums, playlists and artists through the API the web app reads.
//! Every request is signed as OAuth 1.0 with the consumer key and secret the web app
//! carries, the way its own scripts sign theirs. A song's page names it by artist and
//! slug, the API answers with its record, and the play endpoint hands out the signed MP3
//! link. Albums and playlists list their tracks, each a song page of its own, and an
//! artist's page lists their uploads.

use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, Tag, Variant, VariantKind, clean_title, probe_file, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind};

pub const PLATFORM: &str = "audiomack";
const API: &str = "https://api.audiomack.com/v1/";
const SITE: &str = "https://audiomack.com/";
/// The OAuth consumer the web app signs its requests as.
const CONSUMER_KEY: &str = "audiomack-web";
const CONSUMER_SECRET: &str = "bd8a07e9f23fbe9d808646b730f89b8e";
/// How many uploads an artist's page is read up to.
const LISTING_LIMIT: usize = 100;

static RE_SLUG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\w.-]+$").unwrap());

/// The characters OAuth 1.0 leaves unencoded in a signature base string.
const OAUTH_SET: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

fn oauth_encode(text: &str) -> String {
    percent_encoding::utf8_percent_encode(text, OAUTH_SET).to_string()
}

/// `url` with `params`, signed as OAuth 1.0 HMAC-SHA1 with the web app's consumer
/// secret and no token, `nonce` and `timestamp` being the request's.
pub fn sign_request(url: &Url, params: &[(&str, &str)], nonce: &str, timestamp: u64) -> Url {
    let timestamp = timestamp.to_string();
    let mut all: Vec<(String, String)> = params
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    all.push(("oauth_consumer_key".into(), CONSUMER_KEY.into()));
    all.push(("oauth_signature_method".into(), "HMAC-SHA1".into()));
    all.push(("oauth_timestamp".into(), timestamp));
    all.push(("oauth_nonce".into(), nonce.to_string()));
    all.push(("oauth_version".into(), "1.0".into()));
    let mut encoded: Vec<(String, String)> = all
        .iter()
        .map(|(k, v)| (oauth_encode(k), oauth_encode(v)))
        .collect();
    encoded.sort();
    let normalized = encoded
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    let mut bare = url.clone();
    bare.set_query(None);
    bare.set_fragment(None);
    let base = format!(
        "GET&{}&{}",
        oauth_encode(bare.as_str()),
        oauth_encode(&normalized)
    );
    let key = format!("{}&", oauth_encode(CONSUMER_SECRET));
    let signature = util::b64_encode(&util::hmac_sha1(key.as_bytes(), base.as_bytes()));
    let mut signed = bare;
    {
        let mut pairs = signed.query_pairs_mut();
        for (k, v) in &all {
            pairs.append_pair(k, v);
        }
        pairs.append_pair("oauth_signature", &signature);
    }
    signed
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Song {
        artist: String,
        slug: String,
    },
    Album {
        artist: String,
        slug: String,
    },
    Playlist {
        artist: String,
        slug: String,
    },
    /// An artist's page: their uploads.
    Artist {
        artist: String,
    },
}

/// First path segments that are pages of the app rather than an artist's.
const RESERVED: &[&str] = &[
    "trending",
    "top",
    "world",
    "search",
    "charts",
    "playlists",
    "albums",
    "songs",
    "my-library",
    "login",
    "signup",
    "join",
    "about",
    "premium",
    "supporters",
    "artists",
    "browse",
    "discover",
    "feed",
    "settings",
    "upload",
    "edit",
    "podcasts",
    "amp",
    "creators",
    "world-chart",
    "sitemap",
    "terms",
    "privacy",
    "contact",
    "genre",
    "tags",
    "embed",
    "api",
    "app",
    "_next",
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
    if host != "audiomack.com" && host != "www.audiomack.com" {
        return None;
    }
    let segments = segments(url);
    let typed = |kind: &str, artist: &String, slug: &String| -> Option<Link> {
        if !RE_SLUG.is_match(artist) || !RE_SLUG.is_match(slug) {
            return None;
        }
        Some(match kind {
            "song" => Link::Song {
                artist: artist.clone(),
                slug: slug.clone(),
            },
            "album" => Link::Album {
                artist: artist.clone(),
                slug: slug.clone(),
            },
            "playlist" => Link::Playlist {
                artist: artist.clone(),
                slug: slug.clone(),
            },
            _ => return None,
        })
    };
    match segments.as_slice() {
        [kind, artist, slug] if matches!(kind.as_str(), "song" | "album" | "playlist") => {
            typed(kind, artist, slug)
        }
        [artist, kind, slug] if matches!(kind.as_str(), "song" | "album" | "playlist") => {
            if RESERVED.contains(&artist.as_str()) {
                return None;
            }
            typed(kind, artist, slug)
        }
        [kind, artist] if kind == "artist" => RE_SLUG.is_match(artist).then(|| Link::Artist {
            artist: artist.clone(),
        }),
        [artist] => {
            if RESERVED.contains(&artist.as_str()) || !RE_SLUG.is_match(artist) {
                return None;
            }
            Some(Link::Artist {
                artist: artist.clone(),
            })
        }
        _ => None,
    }
}

/// The page of a song, album or playlist on the site.
fn page_url(kind: &str, artist: &str, slug: &str) -> Option<Url> {
    Url::parse(&format!("{SITE}{artist}/{kind}/{slug}")).ok()
}

/// A playlist entry for a track an album or playlist lists: albums name the uploader by
/// `uploader_url_slug`, playlists by the `uploader` record.
fn track_entry(track: &Value, fallback_artist: &str) -> Option<PlaylistEntry> {
    let slug = util::text(&track["url_slug"])?;
    let uploader = util::text(&track["uploader_url_slug"])
        .or_else(|| util::text(&track["uploader"]["url_slug"]))
        .unwrap_or_else(|| fallback_artist.to_string());
    Some(PlaylistEntry {
        url: page_url("song", &uploader, &slug)?,
        title: match (util::text(&track["artist"]), util::text(&track["title"])) {
            (Some(artist), Some(title)) => Some(format!("{artist} - {title}")),
            (_, title) => title,
        },
        duration: util::seconds(&track["duration"]),
    })
}

pub struct AudiomackResolver {
    http: Http,
}

impl AudiomackResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// One signed GET of `endpoint` with `params`, answered as JSON: a record under
    /// `results`, or the play endpoint's signed link.
    async fn api(
        &self,
        endpoint: &str,
        params: &[(&str, &str)],
        origin: &Url,
    ) -> Result<Value, ResolveError> {
        let url = Url::parse(&format!("{API}{endpoint}"))
            .map_err(|e| ResolveError::malformed(origin, format!("API link: {e}")))?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let signed = sign_request(&url, params, &util::random_alphanumeric(32), timestamp);
        let response = self
            .http
            .get(signed)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "application/json")
            .header("origin", "https://audiomack.com")
            .header("referer", SITE)
            .send()
            .await?;
        let status = response.status.as_u16();
        let body = response.text(MAX_PAGE).await?;
        let json: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        match status {
            200..=299 => {}
            404 => return Err(ResolveError::NotFound(origin.clone())),
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            _ => {
                let message = json["message"]
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| format!("the API answered HTTP {status}"));
                return Err(ResolveError::unavailable(origin, message));
            }
        }
        if !json.is_object() {
            return Err(ResolveError::malformed(
                origin,
                "the API answered without JSON",
            ));
        }
        if let Some(message) = json["message"].as_str()
            && json["results"].is_null()
            && json["signedUrl"].is_null()
        {
            return Err(ResolveError::unavailable(origin, message.to_string()));
        }
        Ok(json)
    }

    /// The signed MP3 link the play endpoint hands out for a song.
    async fn play_variant(&self, id: u64, origin: &Url) -> Result<Variant, ResolveError> {
        let answer = self
            .api(
                &format!("music/play/{id}"),
                &[("hq", "true"), ("environment", "desktop-web")],
                origin,
            )
            .await?;
        let stream = util::url_of(&answer["signedUrl"], None)
            .ok_or_else(|| ResolveError::unavailable(origin, "the song has no stream"))?;
        let probed = probe_file(&self.http, &stream, PLATFORM, BROWSER_UA, &[]).await?;
        if !matches!(probed.status.as_u16(), 200 | 206) {
            return Err(ResolveError::unavailable(
                origin,
                format!("the stream host answered HTTP {}", probed.status),
            ));
        }
        let container = Container::from_name(probed.url.path())
            .or_else(|| {
                probed
                    .content_type
                    .as_deref()
                    .and_then(Container::from_mime)
            })
            .unwrap_or(Container::Mp3);
        let mut variant = Variant::new(stream, VariantKind::File);
        variant.audio_only = true;
        variant.audio = Some(match container {
            Container::M4a => AudioCodec::Aac,
            _ => AudioCodec::Mp3,
        });
        variant.container = Some(container);
        variant.size = probed.size;
        variant.format_id = Some("hq".into());
        Ok(variant)
    }

    async fn resolve_song(
        &self,
        artist: &str,
        slug: &str,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let answer = self
            .api(&format!("music/song/{artist}/{slug}"), &[], origin)
            .await?;
        let song = &answer["results"];
        let id = util::uint(&song["id"]).ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
        if util::boolean(&song["geo_restricted"]).unwrap_or(false) {
            return Err(ResolveError::unavailable(
                origin,
                "the song is withheld in this region",
            ));
        }
        if util::boolean(&song["private"]).unwrap_or(false) {
            return Err(ResolveError::unavailable(origin, "the song is private"));
        }
        let variant = self.play_variant(id, origin).await?;
        let mut resolved = Resolved::of(PLATFORM, MediaKind::Audio);
        resolved.id = Some(id.to_string());
        resolved.title = match (util::text(&song["artist"]), util::text(&song["title"])) {
            (Some(artist), Some(title)) => clean_title(&format!("{artist} - {title}")),
            (_, Some(title)) => clean_title(&title),
            _ => None,
        };
        resolved.description = util::text(&song["description"]);
        resolved.uploader = util::text(&song["artist"])
            .or_else(|| util::text(&song["uploader"]["name"]))
            .and_then(|a| clean_title(&a));
        resolved.uploader_url = util::text(&song["uploader"]["url_slug"])
            .and_then(|u| Url::parse(&format!("{SITE}{u}")).ok());
        resolved.uploaded_at =
            util::epoch(&song["released"]).or_else(|| util::epoch(&song["uploaded"]));
        resolved.duration = util::seconds(&song["duration"]);
        resolved.thumbnail = util::url_of(&song["image"], None);
        resolved.webpage_url = util::text(&song["uploader"]["url_slug"])
            .and_then(|u| page_url("song", &u, slug))
            .or_else(|| page_url("song", artist, slug));
        resolved.variants = vec![variant];
        Ok(Resolution::from(resolved))
    }

    /// An album or playlist as a list of its tracks.
    async fn resolve_collection(
        &self,
        kind: &str,
        artist: &str,
        slug: &str,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let endpoint = match kind {
            "album" => format!("music/album/{artist}/{slug}"),
            _ => format!("playlist/{artist}/{slug}"),
        };
        let answer = self.api(&endpoint, &[], origin).await?;
        let record = &answer["results"];
        let id = util::uint(&record["id"]).ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
        let entries: Vec<PlaylistEntry> = record["tracks"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|track| track_entry(track, artist))
            .collect();
        if entries.is_empty() {
            return Err(ResolveError::unavailable(
                origin,
                format!("the {kind} has no tracks"),
            ));
        }
        let title = util::text(&record["title"]);
        let owner = util::text(&record["artist"]).or_else(|| util::text(&record["artist"]["name"]));
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(id.to_string()),
            title: match (owner, title) {
                (Some(owner), Some(title)) if kind == "album" => {
                    clean_title(&format!("{owner} - {title}"))
                }
                (_, title) => title.and_then(|t| clean_title(&t)),
            },
            total: Some(entries.len()),
            entries,
        }))
    }

    async fn resolve_artist(&self, artist: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let answer = self.api(&format!("artist/{artist}"), &[], origin).await?;
        let record = &answer["results"];
        let id = util::uint(&record["id"]).ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
        let slug = util::text(&record["url_slug"]).unwrap_or_else(|| artist.to_string());
        let limit = LISTING_LIMIT.to_string();
        let uploads = self
            .api(
                &format!("artist/{slug}/uploads"),
                &[("page", "1"), ("limit", limit.as_str())],
                origin,
            )
            .await?;
        let entries: Vec<PlaylistEntry> = uploads["results"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let kind = util::text(&item["type"])?;
                let item_slug = util::text(&item["url_slug"])?;
                let uploader =
                    util::text(&item["uploader"]["url_slug"]).unwrap_or_else(|| slug.clone());
                Some(PlaylistEntry {
                    url: page_url(&kind, &uploader, &item_slug)?,
                    title: match (util::text(&item["artist"]), util::text(&item["title"])) {
                        (Some(artist), Some(title)) => Some(format!("{artist} - {title}")),
                        (_, title) => title,
                    },
                    duration: util::seconds(&item["duration"]),
                })
            })
            .collect();
        if entries.is_empty() {
            return Err(ResolveError::unavailable(
                origin,
                "the artist has no uploads",
            ));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(id.to_string()),
            title: util::text(&record["name"]).and_then(|n| clean_title(&format!("{n} (uploads)"))),
            total: util::uint(&record["upload_count"])
                .map(|n| n as usize)
                .filter(|n| *n >= entries.len())
                .or(Some(entries.len())),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for AudiomackResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Audiomack",
            hosts: &["audiomack.com"],
            features: &["songs", "albums", "playlists", "artists"],
            formats: &["mp3"],
            media: &[MediaKind::Audio],
            tags: &[Tag::Music],
            session: SessionSupport::None,
            on_by_default: true,
            examples: &[
                "https://audiomack.com/roosh-williams/song/extraordinary",
                "https://www.audiomack.com/song/roosh-williams/extraordinary",
                "https://audiomack.com/roosh-williams/album/fodafukuvvit",
                "https://audiomack.com/audiomack/playlist/00s-dance-hits",
                "https://audiomack.com/roosh-williams",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Song { artist, slug } => self.resolve_song(&artist, &slug, url).await,
            Link::Album { artist, slug } => {
                self.resolve_collection("album", &artist, &slug, url).await
            }
            Link::Playlist { artist, slug } => {
                self.resolve_collection("playlist", &artist, &slug, url)
                    .await
            }
            Link::Artist { artist } => self.resolve_artist(&artist, url).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

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

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let song = Some(Link::Song {
            artist: "roosh-williams".into(),
            slug: "extraordinary".into(),
        });
        assert_eq!(
            link("https://audiomack.com/roosh-williams/song/extraordinary"),
            song
        );
        assert_eq!(
            link("http://www.audiomack.com/song/roosh-williams/extraordinary?x=1"),
            song
        );
        assert_eq!(
            link("https://audiomack.com/roosh-williams/album/fodafukuvvit/"),
            Some(Link::Album {
                artist: "roosh-williams".into(),
                slug: "fodafukuvvit".into()
            })
        );
        assert_eq!(
            link("https://audiomack.com/playlist/audiomack/00s-dance-hits"),
            Some(Link::Playlist {
                artist: "audiomack".into(),
                slug: "00s-dance-hits".into()
            })
        );
        assert_eq!(
            link("https://audiomack.com/roosh-williams"),
            Some(Link::Artist {
                artist: "roosh-williams".into()
            })
        );
        assert_eq!(
            link("https://audiomack.com/artist/roosh-williams"),
            Some(Link::Artist {
                artist: "roosh-williams".into()
            })
        );
        assert_eq!(link("https://audiomack.com/trending"), None);
        assert_eq!(link("https://audiomack.com/search/x/song/y"), None);
        assert_eq!(link("https://audiomack.com/"), None);
        assert_eq!(link("https://example.com/roosh-williams/song/x"), None);
    }

    #[test]
    fn requests_are_signed_as_the_web_app_signs_them() {
        let url = Url::parse("https://api.audiomack.com/v1/music/play/310086").unwrap();
        let signed = sign_request(
            &url,
            &[("hq", "true"), ("environment", "desktop-web")],
            "abcdefghijklmnopqrstuvwxyz012345",
            1_790_000_000,
        );
        let pairs: Vec<(String, String)> = signed
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        assert!(pairs.contains(&("oauth_consumer_key".into(), "audiomack-web".into())));
        assert!(pairs.contains(&("oauth_signature_method".into(), "HMAC-SHA1".into())));
        assert!(pairs.contains(&("hq".into(), "true".into())));
        // The signature of this exact base string, checked against a reference signer.
        let signature = pairs
            .iter()
            .find(|(k, _)| k == "oauth_signature")
            .map(|(_, v)| v.clone())
            .unwrap();
        let base = "GET&https%3A%2F%2Fapi.audiomack.com%2Fv1%2Fmusic%2Fplay%2F310086&environment%3Ddesktop-web%26hq%3Dtrue%26oauth_consumer_key%3Daudiomack-web%26oauth_nonce%3Dabcdefghijklmnopqrstuvwxyz012345%26oauth_signature_method%3DHMAC-SHA1%26oauth_timestamp%3D1790000000%26oauth_version%3D1.0";
        let expected = util::b64_encode(&util::hmac_sha1(
            format!("{CONSUMER_SECRET}&").as_bytes(),
            base.as_bytes(),
        ));
        assert_eq!(signature, expected);
    }

    /// Replays match by host and path, so the signed query does not have to be known.
    #[tokio::test]
    async fn songs_resolve_through_the_play_endpoint() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://api.audiomack.com/v1/music/song/roosh-williams/extraordinary",
            200,
            "application/json",
            json!({"results": {
                "id": 310086, "uploaded": "1414075043", "duration": "249", "type": "song", "genre": "rap",
                "image": "https://i.audiomack.com/roosh-williams/b5a6e052f2.webp", "url_slug": "extraordinary",
                "featuring": "Emilio Rojas", "private": "no", "released": "1414075048", "title": "Extraordinary",
                "description": "The first single from Roosh's upcoming album.", "artist": "Roosh Williams", "geo_restricted": "no",
                "uploader": {"id": "8694", "name": "Roosh Williams", "url_slug": "roosh-williams"}
            }}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.audiomack.com/v1/music/play/310086",
            200,
            "application/json",
            json!({"signedUrl": "https://music.audiomack.com/tracks/roosh-williams/extraordinary-ft-emilio-rojas.mp3?Expires=1&Signature=s", "playUuid": "x"}).to_string(),
        ));
        let mut stream = get(
            "https://music.audiomack.com/tracks/roosh-williams/extraordinary-ft-emilio-rojas.mp3?Expires=1&Signature=s",
            206,
            "audio/mpeg",
            String::new(),
        );
        stream
            .response
            .headers
            .push(("content-range".into(), "bytes 0-0/9975000".into()));
        fixture.exchanges.push(stream);
        fixture.exchanges.push(get(
            "https://api.audiomack.com/v1/music/song/nobody/nothing",
            404,
            "application/json",
            json!({"errorcode": 1005, "message": "Music not found"}).to_string(),
        ));
        let resolver = AudiomackResolver::new(Http::replay(fixture));
        let url = Url::parse("https://audiomack.com/roosh-williams/song/extraordinary").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.media, MediaKind::Audio);
        assert_eq!(resolved.id.as_deref(), Some("310086"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Roosh Williams - Extraordinary")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("Roosh Williams"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://audiomack.com/roosh-williams"
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(249)));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://i.audiomack.com/roosh-williams/b5a6e052f2.webp"
        );
        assert_eq!(resolved.variants.len(), 1);
        let mp3 = &resolved.variants[0];
        assert!(mp3.audio_only);
        assert_eq!(mp3.container, Some(Container::Mp3));
        assert_eq!(mp3.audio, Some(AudioCodec::Mp3));
        assert_eq!(mp3.size, Some(9_975_000));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://audiomack.com/nobody/song/nothing").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn albums_playlists_and_artists_list_their_tracks() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://api.audiomack.com/v1/music/album/roosh-williams/fodafukuvvit",
            200,
            "application/json",
            json!({"results": {
                "id": 152642, "type": "album", "title": "FoDaFukUvvit", "artist": "Roosh Williams", "url_slug": "fodafukuvvit",
                "tracks": [
                    {"song_id": 548842, "url_slug": "outdoor-feat-kyle-hubbard-and-dex-kwasi", "artist": "Roosh Williams", "title": "Outdoor", "duration": "217", "uploader_url_slug": "roosh-williams"},
                    {"song_id": 548843, "url_slug": "57-hoes", "artist": "Roosh Williams", "title": "57 Hoes", "duration": "150", "uploader_url_slug": "roosh-williams"}
                ]
            }}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.audiomack.com/v1/playlist/audiomack/00s-dance-hits",
            200,
            "application/json",
            json!({"results": {
                "id": 78819519, "type": "playlist", "title": "00's Dance Hits", "url_slug": "00s-dance-hits",
                "artist": {"id": "74790", "name": "Audiomack", "url_slug": "audiomack"},
                "tracks": [
                    {"id": "77493061", "title": "Castles In The Sky", "artist": "Ian van Dahl feat. Marsha", "url_slug": "castles-in-the-sky", "duration": "228", "uploader": {"url_slug": "armada-music-albums"}},
                    {"id": "12652276", "title": "One More Time", "artist": "Daft Punk", "url_slug": "one-more-time", "duration": "321", "uploader": {"url_slug": "daftpunk"}}
                ]
            }}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.audiomack.com/v1/artist/roosh-williams",
            200,
            "application/json",
            json!({"results": {"id": 8694, "name": "Roosh Williams", "url_slug": "roosh-williams", "upload_count": 13}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.audiomack.com/v1/artist/roosh-williams/uploads",
            200,
            "application/json",
            json!({"results": [
                {"id": 955450, "type": "song", "url_slug": "whip-it", "title": "Whip It", "artist": "Roosh Williams", "duration": "216", "uploader": {"url_slug": "roosh-williams"}},
                {"id": 152642, "type": "album", "url_slug": "fodafukuvvit", "title": "FoDaFukUvvit", "artist": "Roosh Williams", "uploader": {"url_slug": "roosh-williams"}}
            ]}).to_string(),
        ));
        let resolver = AudiomackResolver::new(Http::replay(fixture));
        let Resolution::Playlist(album) = resolver
            .resolve(
                &Url::parse("https://audiomack.com/roosh-williams/album/fodafukuvvit").unwrap(),
            )
            .await
            .unwrap()
        else {
            panic!("an album is a playlist");
        };
        assert_eq!(album.id.as_deref(), Some("152642"));
        assert_eq!(
            album.title.as_deref(),
            Some("Roosh Williams - FoDaFukUvvit")
        );
        assert_eq!(album.total, Some(2));
        assert_eq!(
            album.entries[0].url.as_str(),
            "https://audiomack.com/roosh-williams/song/outdoor-feat-kyle-hubbard-and-dex-kwasi"
        );
        assert_eq!(
            album.entries[0].title.as_deref(),
            Some("Roosh Williams - Outdoor")
        );
        assert_eq!(album.entries[1].duration, Some(Duration::from_secs(150)));
        let Resolution::Playlist(playlist) = resolver
            .resolve(
                &Url::parse("https://audiomack.com/audiomack/playlist/00s-dance-hits").unwrap(),
            )
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(playlist.title.as_deref(), Some("00's Dance Hits"));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[1].url.as_str(),
            "https://audiomack.com/daftpunk/song/one-more-time"
        );
        let Resolution::Playlist(artist) = resolver
            .resolve(&Url::parse("https://audiomack.com/roosh-williams").unwrap())
            .await
            .unwrap()
        else {
            panic!("an artist page is a playlist");
        };
        assert_eq!(artist.title.as_deref(), Some("Roosh Williams (uploads)"));
        assert_eq!(artist.total, Some(13));
        assert_eq!(artist.entries.len(), 2);
        assert_eq!(
            artist.entries[1].url.as_str(),
            "https://audiomack.com/roosh-williams/album/fodafukuvvit"
        );
    }

    /// Every example link resolves live: songs to audio with a playable variant, lists
    /// to entries.
    #[ignore = "reaches the live site: cargo test -- --ignored"]
    #[tokio::test]
    async fn live_examples_resolve() {
        let resolver = AudiomackResolver::new(Http::new(crate::http::HttpConfig::default()));
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
                        "{link}: audio {:?} with {} variants",
                        resolved.title,
                        resolved.variants.len()
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
    }
}
