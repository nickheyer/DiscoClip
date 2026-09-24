//! ZDF: the Mediathek's GraphQL API names a video by the canonical id its link ends with
//! and lists its media, each a document the tmd API describes with MP4 and WebM files by
//! height, HLS playlists and captions. Live channels are such documents too, and the live
//! TV page lists the channels. A series, magazine or film page lists its episodes per
//! season through the same API, and the news sites' videos come from the Mediathek's
//! document API when the GraphQL API does not carry them. Every API call carries the
//! bearer token the token service hands out.

use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::{Value, json};
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, SubtitleFormat, SubtitleTrack, Tag, Variant, clean_title, fetch, manifests,
    navigation_headers, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "zdf";
const API: &str = "https://api.zdf.de";
const GRAPHQL: &str = "https://api.zdf.de/graphql";
const TOKEN_API: &str = "https://zdf-prod-futura.zdf.de/mediathekV2/token";
const DOCUMENT_API: &str = "https://zdf-prod-futura.zdf.de/mediathekV2/document/";
const LIVE_PAGE: &str = "https://www.zdf.de/live-tv";
/// The player the tmd API is asked for, as the Android app asks.
const PLAYER_ID: &str = "android_native_6";
/// How many episodes of a season a collection lists.
const PAGE_SIZE: usize = 100;
/// How long before a token expires it is fetched again.
const TOKEN_MARGIN: i64 = 60;

static RE_CANONICAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9._-]+$").unwrap());
/// `_3328k_` in a file's name: its bitrate.
static RE_BITRATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"_(\d+)k_").unwrap());
/// `zdfneo-live-beitrag-100`: a live channel's canonical on the live TV page.
static RE_LIVE_CHANNEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([a-z0-9]+)-live-beitrag-(\d+)\b").unwrap());

const VIDEO_QUERY: &str = "query VideoByCanonical($canonical: String!) { videoByCanonical(canonical: $canonical) { canonical title sharingUrl editorialDate leadParagraph teaser { title description image { list } } episodeInfo { episodeNumber seasonNumber } smartCollection { canonical title sharingUrl } currentMedia { nodes { id ptmdTemplate ... on VodMedia { duration aspectRatio vodMediaType label } ... on LiveMedia { start stop encryption liveMediaType label } } } } }";

const COLLECTION_QUERY: &str = "query CollectionByCanonical($canonical: String!, $pageSize: Int!) { smartCollectionByCanonical(canonical: $canonical) { __typename ... on ISmartCollection { canonical title infoText sharingUrl } ... on MovieSmartCollection { video { canonical sharingUrl title } } ... on HybridBingeSeriesSmartCollection { seasons { totalCount nodes { number episodes(first: $pageSize) { totalCount nodes { canonical sharingUrl title editorialDate currentMedia { nodes { ... on VodMedia { duration } } } } } } } } ... on DefaultWithSectionsSmartCollection { seasons { totalCount nodes { number episodes(first: $pageSize) { totalCount nodes { canonical sharingUrl title editorialDate currentMedia { nodes { ... on VodMedia { duration } } } } } } } } } }";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A video or a live channel, by the canonical id its link ends with.
    Video { canonical: String },
    /// A series, magazine or film page, or one season of it.
    Collection {
        canonical: String,
        season: Option<u32>,
    },
    /// The live TV page, listing the channels.
    LiveChannels,
}

fn canonical_of(segment: &str) -> Option<String> {
    let stem = segment.strip_suffix(".html").unwrap_or(segment);
    (RE_CANONICAL.is_match(stem) && stem.len() > 1).then(|| stem.to_string())
}

/// zdf.de `/video/…/{canonical}` and `/play/…/{canonical}` pages, live channels under
/// `/live-tv`, collection pages `/{section}/{canonical}`, and the `.html` pages of the
/// site's older layout and of its news and children's sites.
pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let last = *segments.last()?;
    let is_page = last.ends_with(".html");
    match host.as_str() {
        "zdf.de" | "www.zdf.de" => {
            if segments == ["live-tv"] {
                return Some(Link::LiveChannels);
            }
            if is_page || matches!(segments.first(), Some(&"video" | &"play" | &"live-tv")) {
                return canonical_of(last).map(|canonical| Link::Video { canonical });
            }
            if segments.len() >= 2 {
                return canonical_of(last).map(|canonical| Link::Collection {
                    canonical,
                    season: util::query_param(url, "staffel").and_then(|s| s.parse().ok()),
                });
            }
            None
        }
        "zdfheute.de" | "www.zdfheute.de" | "zdftivi.de" | "www.zdftivi.de" | "logo.de"
        | "www.logo.de" => is_page
            .then(|| canonical_of(last))
            .flatten()
            .map(|canonical| Link::Video { canonical }),
        _ => None,
    }
}

/// A bearer token from the token service, and when it stops working.
struct Token {
    header: String,
    expires: i64,
}

/// The streams one tmd document describes.
struct Streams {
    files: Vec<Variant>,
    playlists: Vec<Variant>,
    subtitles: Vec<SubtitleTrack>,
    duration: Option<Duration>,
}

/// The two-letter code of a language the API names in three letters, as ISO 639-2
/// writes the languages ZDF broadcasts in.
fn language_of(code: &str) -> Option<String> {
    let code = code.trim().to_ascii_lowercase();
    if code.is_empty() {
        return None;
    }
    let short = match code.as_str() {
        "deu" | "ger" => "de",
        "eng" => "en",
        "fra" | "fre" => "fr",
        "spa" => "es",
        "ita" => "it",
        "tur" => "tr",
        "pol" => "pl",
        "rus" => "ru",
        "ara" => "ar",
        "ukr" => "uk",
        "por" => "pt",
        "nld" | "dut" => "nl",
        "ell" | "gre" => "el",
        other => return Some(util::iso639_short(other).unwrap_or(other).to_string()),
    };
    Some(short.to_string())
}

/// The captions a document lists, as subtitle tracks.
fn subtitles_of(captions: &Value) -> Vec<SubtitleTrack> {
    let mut tracks: Vec<SubtitleTrack> = Vec::new();
    for caption in captions.as_array().into_iter().flatten() {
        let Some(track_url) = util::url_of(&caption["uri"], None) else {
            continue;
        };
        if tracks.iter().any(|t| t.url == track_url) {
            continue;
        }
        let format = match caption["format"].as_str().unwrap_or("") {
            "webvtt" => SubtitleFormat::Vtt,
            f if f.starts_with("ebu-tt") || f.contains("ttml") => SubtitleFormat::Ttml,
            _ => match super::path_extension(&track_url).as_deref() {
                Some("vtt") => SubtitleFormat::Vtt,
                Some("srt") => SubtitleFormat::Srt,
                _ => SubtitleFormat::Ttml,
            },
        };
        tracks.push(SubtitleTrack {
            url: track_url,
            language: language_of(caption["language"].as_str().unwrap_or("deu"))
                .unwrap_or_else(|| "de".into()),
            name: match caption["class"].as_str() {
                Some("hoh") => Some("Untertitel für Hörgeschädigte".into()),
                Some(class) if !class.is_empty() => Some(class.to_string()),
                _ => None,
            },
            format,
            auto: false,
            headers: Vec::new(),
        });
    }
    tracks
}

/// The value of a tmd attribute: `{"value": …}`, or the value itself.
fn attribute<'a>(ptmd: &'a Value, name: &str) -> &'a Value {
    let value = &ptmd["attributes"][name];
    if value["value"].is_null() {
        value
    } else {
        &value["value"]
    }
}

/// The streams a tmd document lists: its files by height, the HLS master of each
/// adaptive listing, its captions and its length. `sign_language` marks a German Sign
/// Language edition.
fn streams_of(ptmd: &Value, aspect_ratio: Option<f64>, sign_language: bool) -> Streams {
    let mut files: Vec<Variant> = Vec::new();
    let mut playlists: Vec<Variant> = Vec::new();
    let duration = util::millis(attribute(ptmd, "duration"));
    let formitaeten = ptmd["priorityList"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|p| p["formitaeten"].as_array().into_iter().flatten());
    for formitaet in formitaeten {
        // The files behind the user agent restriction are the same files on another
        // host, served only to the site's own player.
        if formitaet["facets"].as_array().is_some_and(|f| {
            f.iter()
                .any(|v| v.as_str() == Some("restriction_useragent"))
        }) {
            continue;
        }
        let mut best_playlist: Option<(i64, Variant)> = None;
        for quality in formitaet["qualities"].as_array().into_iter().flatten() {
            let height = util::u32_of(&quality["highestVerticalResolution"]);
            let name = quality["quality"].as_str().unwrap_or("").to_string();
            for track in quality["audio"]["tracks"].as_array().into_iter().flatten() {
                let Some(link) = util::url_of(&track["uri"], None) else {
                    continue;
                };
                let class = track["class"].as_str().unwrap_or("main");
                let language = track["language"].as_str().and_then(language_of);
                let extension = super::path_extension(&link).unwrap_or_default();
                if extension == "m3u8" {
                    // One master per listing: the one with every rendition.
                    let rank = if name == "auto" {
                        i64::MAX
                    } else {
                        i64::from(height.unwrap_or(0))
                    };
                    if best_playlist.as_ref().is_none_or(|(r, _)| rank > *r) {
                        let mut variant = Variant::hls(link);
                        variant.language = language;
                        variant.format_id = Some(if sign_language {
                            "hls-dgs".into()
                        } else {
                            "hls".into()
                        });
                        best_playlist = Some((rank, variant));
                    }
                    continue;
                }
                if !matches!(extension.as_str(), "mp4" | "webm") {
                    continue;
                }
                if files.iter().any(|v| v.url == link) {
                    continue;
                }
                let mut variant = Variant::file(link);
                variant.container = Some(if extension == "webm" {
                    Container::Webm
                } else {
                    Container::Mp4
                });
                if let Some(codecs) = quality["mimeCodec"].as_str() {
                    variant = variant.with_codecs(codecs);
                }
                if variant.video.is_none() {
                    variant.video = Some(if extension == "webm" {
                        VideoCodec::Vp9
                    } else {
                        VideoCodec::H264
                    });
                }
                if variant.audio.is_none() {
                    variant.audio = Some(if extension == "webm" {
                        AudioCodec::Opus
                    } else {
                        AudioCodec::Aac
                    });
                }
                variant.height = height;
                variant.width = match (aspect_ratio, height) {
                    (Some(ratio), Some(h)) => Some((ratio * f64::from(h)).round() as u32),
                    _ => None,
                };
                variant.size = util::uint(&track["filesize"]);
                variant.bitrate = util::search(&RE_BITRATE, variant.url.as_str())
                    .and_then(|k| k.parse::<u64>().ok())
                    .map(|k| k * 1000);
                variant.duration = duration;
                variant.language = language;
                let mut label = height
                    .map(|h| format!("{h}p"))
                    .unwrap_or_else(|| name.clone());
                let mut format_id = format!("{extension}-{label}");
                if class == "ad" {
                    label.push_str(" AD");
                    format_id.push_str("-ad");
                }
                if sign_language {
                    label.push_str(" DGS");
                    format_id.push_str("-dgs");
                }
                variant.label = Some(label);
                variant.format_id = Some(format_id);
                files.push(variant);
            }
        }
        if let Some((_, playlist)) = best_playlist
            && !playlists.iter().any(|v| v.url == playlist.url)
        {
            playlists.push(playlist);
        }
    }
    files.sort_by_key(|v| std::cmp::Reverse(v.height.unwrap_or(0)));
    Streams {
        files,
        playlists,
        subtitles: subtitles_of(&ptmd["captions"]),
        duration,
    }
}

/// The largest picture in a teaser's image list, by its `dim{w}x{h}` key.
fn teaser_image(list: &Value) -> Option<Url> {
    let map = list.as_object()?;
    for key in ["dim1920x1080", "dim1280x720", "dim2400x1350", "original"] {
        if let Some(url) = map.get(key).and_then(|v| util::url_of(v, None)) {
            return Some(url);
        }
    }
    map.values().find_map(|v| util::url_of(v, None))
}

/// The largest picture in a document's `teaserBild`, by width.
fn document_image(bild: &Value) -> Option<Url> {
    bild.as_object()?
        .values()
        .filter_map(|v| Some((util::uint(&v["width"])?, util::url_of(&v["url"], None)?)))
        .max_by_key(|(width, _)| *width)
        .map(|(_, url)| url)
}

/// The name of a live channel, from its canonical.
fn channel_name(prefix: &str) -> String {
    match prefix {
        "zdf" => "ZDF".into(),
        "zdfneo" => "ZDFneo".into(),
        "zdfinfo" => "ZDFinfo".into(),
        "3sat" => "3sat".into(),
        "phoenix" => "phoenix".into(),
        "kika" => "KiKA".into(),
        "arte" => "arte".into(),
        other => other.to_string(),
    }
}

/// The media a video's GraphQL record or fallback document lists: the tmd document to
/// read, whether it is a sign language edition, and whether it is live.
struct MediaNode {
    ptmd: String,
    sign_language: bool,
    live: bool,
    aspect_ratio: Option<f64>,
}

fn aspect_ratio_of(text: &Value) -> Option<f64> {
    let (w, h) = text.as_str()?.split_once(':')?;
    let (w, h): (f64, f64) = (w.trim().parse().ok()?, h.trim().parse().ok()?);
    (h > 0.0).then(|| w / h)
}

pub struct ZdfResolver {
    http: Http,
    token: Mutex<Option<Token>>,
}

impl ZdfResolver {
    pub fn new(http: Http) -> Self {
        Self {
            http,
            token: Mutex::new(None),
        }
    }

    /// The `Api-Auth` header the APIs want, fetched when none is held or it is about
    /// to expire.
    async fn token(&self, origin: &Url) -> Result<String, ResolveError> {
        let now = Timestamp::now().as_second();
        if let Some(token) = self
            .token
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            && token.expires > now + TOKEN_MARGIN
        {
            return Ok(token.header.clone());
        }
        let api = Url::parse(TOKEN_API).expect("valid");
        let fetched = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        if let Some(error) = status_error(fetched.status, origin) {
            return Err(error);
        }
        let answer = fetched.json(origin)?;
        let (Some(kind), Some(value)) = (util::text(&answer["type"]), util::text(&answer["token"]))
        else {
            return Err(ResolveError::malformed(
                origin,
                "the token service named no token",
            ));
        };
        let header = format!("{kind} {value}");
        let expires = util::int(&answer["expires"]).unwrap_or(now + 3600);
        *self.token.lock().unwrap_or_else(|e| e.into_inner()) = Some(Token {
            header: header.clone(),
            expires,
        });
        Ok(header)
    }

    fn forget_token(&self) {
        *self.token.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// The `data` a GraphQL query answers with. A refused token is fetched again once.
    async fn graphql(
        &self,
        query: &str,
        variables: Value,
        origin: &Url,
    ) -> Result<Value, ResolveError> {
        for attempt in 0..2 {
            let token = self.token(origin).await?;
            let response = self
                .http
                .post(Url::parse(GRAPHQL).expect("valid"))
                .platform(PLATFORM)
                .user_agent(BROWSER_UA)
                .header("api-auth", &token)
                .header("apollo-require-preflight", "true")
                .header("accept", "application/json")
                .json(&json!({"query": query, "variables": variables}))
                .send()
                .await?;
            let status = response.status.as_u16();
            if matches!(status, 401 | 403) && attempt == 0 {
                self.forget_token();
                continue;
            }
            if status == 429 {
                return Err(ResolveError::RateLimited(origin.clone()));
            }
            let answer: Value = response
                .json(MAX_PAGE)
                .await
                .map_err(|e| ResolveError::malformed(origin, format!("GraphQL answer: {e}")))?;
            if answer["data"].is_object() {
                return Ok(answer["data"].clone());
            }
            let message = answer["errors"][0]["message"]
                .as_str()
                .map(String::from)
                .unwrap_or_else(|| format!("the API answered HTTP {status}"));
            return Err(ResolveError::malformed(
                origin,
                format!("GraphQL: {message}"),
            ));
        }
        Err(ResolveError::unavailable(
            origin,
            "the API refused its own token twice",
        ))
    }

    /// A tmd document, by the template the GraphQL API names or the link a document
    /// carries.
    async fn ptmd(&self, template: &str, origin: &Url) -> Result<Value, ResolveError> {
        let link = if template.starts_with("http") {
            template.to_string()
        } else {
            format!("{API}{}", template.replace("{playerId}", PLAYER_ID))
        };
        let api = Url::parse(&link)
            .map_err(|e| ResolveError::malformed(origin, format!("tmd link {link}: {e}")))?;
        let token = self.token(origin).await?;
        let headers = vec![("api-auth".to_string(), token)];
        let fetched = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &headers, MAX_PAGE).await?;
        if let Some(error) = status_error(fetched.status, origin) {
            return Err(error);
        }
        fetched.json(origin)
    }

    /// The video's streams, from every tmd document it lists, with the HLS masters
    /// expanded into their renditions.
    async fn resolve_media(
        &self,
        nodes: &[MediaNode],
        mut resolved: Resolved,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        if nodes.is_empty() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        let mut files = Vec::new();
        let mut playlists = Vec::new();
        let mut subtitles = Vec::new();
        let mut duration = None;
        let live = nodes.iter().any(|n| n.live);
        for node in nodes {
            let ptmd = self.ptmd(&node.ptmd, origin).await?;
            let streams = streams_of(&ptmd, node.aspect_ratio, node.sign_language);
            duration = duration.or(streams.duration);
            files.extend(streams.files);
            playlists.extend(streams.playlists);
            for track in streams.subtitles {
                if !subtitles.iter().any(|t: &SubtitleTrack| t.url == track.url) {
                    subtitles.push(track);
                }
            }
        }
        let mut variants = files;
        for mut variant in
            manifests::expand_all(&self.http, PLATFORM, playlists, &mut subtitles, duration).await
        {
            if live {
                variant.live = true;
            }
            if variant.duration.is_none() {
                variant.duration = duration;
            }
            variants.push(variant);
        }
        if variants.is_empty() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        if live {
            variants.iter_mut().for_each(|v| v.live = true);
        }
        resolved.live = live;
        resolved.duration = if live {
            None
        } else {
            duration.or_else(|| variants.iter().find_map(|v| v.duration))
        };
        resolved.subtitles = subtitles;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn resolve_video(
        &self,
        canonical: &str,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let data = self
            .graphql(VIDEO_QUERY, json!({"canonical": canonical}), origin)
            .await?;
        let video = &data["videoByCanonical"];
        if !video.is_object() {
            return self.resolve_document(canonical, origin).await;
        }
        let nodes: Vec<MediaNode> = video["currentMedia"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|node| {
                Some(MediaNode {
                    ptmd: util::text(&node["ptmdTemplate"])?,
                    sign_language: node["vodMediaType"].as_str() == Some("DGS"),
                    live: !node["liveMediaType"].is_null(),
                    aspect_ratio: aspect_ratio_of(&node["aspectRatio"]),
                })
            })
            .collect();
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(canonical.to_string());
        resolved.title = video["title"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| video["teaser"]["title"].as_str().and_then(clean_title));
        resolved.description = video["leadParagraph"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| {
                video["teaser"]["description"]
                    .as_str()
                    .and_then(clean_title)
            });
        resolved.uploaded_at = util::time(&video["editorialDate"]);
        resolved.thumbnail = teaser_image(&video["teaser"]["image"]["list"]);
        resolved.webpage_url =
            util::url_of(&video["sharingUrl"], None).or_else(|| origin.clone().into());
        let collection = &video["smartCollection"];
        resolved.uploader = collection["title"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| Some("ZDF".into()));
        resolved.uploader_url = util::url_of(&collection["sharingUrl"], None)
            .or_else(|| Url::parse("https://www.zdf.de/").ok());
        self.resolve_media(&nodes, resolved, origin).await
    }

    /// A video the GraphQL API does not carry, as the news and children's sites publish
    /// them: the Mediathek's document API describes it and names its tmd documents.
    async fn resolve_document(
        &self,
        canonical: &str,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let api = Url::parse(&format!("{DOCUMENT_API}{canonical}")).expect("valid");
        let fetched = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        if let Some(error) = status_error(fetched.status, origin) {
            return Err(error);
        }
        let answer = fetched.json(origin)?;
        let document = &answer["document"];
        if !document.is_object() || document["type"].as_str().is_some_and(|t| t != "video") {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        let mut nodes: Vec<MediaNode> = document["streams"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|stream| {
                let ptmd = util::text(&stream["streamApiUrlAndroid"])?;
                let ext_id = stream["extId"].as_str().unwrap_or("");
                Some(MediaNode {
                    sign_language: stream["sourceVariant"].as_str() == Some("dgs")
                        || ext_id.ends_with("_dgs")
                        || stream["label"]
                            .as_str()
                            .is_some_and(|l| l.contains("Gebärden")),
                    live: ptmd.contains("/live/"),
                    aspect_ratio: None,
                    ptmd,
                })
            })
            .collect();
        if nodes.is_empty()
            && let Some(ptmd) = util::text(&document["streamApiUrlAndroid"])
        {
            nodes.push(MediaNode {
                sign_language: false,
                live: ptmd.contains("/live/"),
                aspect_ratio: None,
                ptmd,
            });
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(canonical.to_string());
        resolved.title = document["titel"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| document["headline"].as_str().and_then(clean_title));
        resolved.description = document["beschreibung"].as_str().and_then(clean_title);
        resolved.uploaded_at =
            util::time(&answer["meta"]["editorialDate"]).or_else(|| util::time(&document["date"]));
        resolved.thumbnail = document_image(&document["teaserBild"]);
        resolved.webpage_url =
            util::url_of(&document["sharingUrl"], None).or_else(|| origin.clone().into());
        resolved.uploader = document["brandTitle"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| Some("ZDF".into()));
        resolved.uploader_url = Url::parse("https://www.zdf.de/").ok();
        let mut subtitles = subtitles_of(&document["captions"]);
        let outcome = self.resolve_media(&nodes, resolved, origin).await?;
        Ok(match outcome {
            Resolution::Media(mut media) => {
                subtitles.retain(|t| !media.subtitles.iter().any(|m| m.url == t.url));
                media.subtitles.extend(subtitles);
                Resolution::Media(media)
            }
            other => other,
        })
    }

    async fn resolve_collection(
        &self,
        canonical: &str,
        season: Option<u32>,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let variables = json!({"canonical": canonical, "pageSize": PAGE_SIZE});
        let mut data = self.graphql(COLLECTION_QUERY, variables, origin).await?;
        let mut canonical = canonical.to_string();
        if !data["smartCollectionByCanonical"].is_object() {
            // The site's older links name a collection without its number: the page
            // redirects to the link that carries it.
            let fetched = fetch(
                &self.http,
                origin,
                PLATFORM,
                BROWSER_UA,
                &navigation_headers(),
                MAX_PAGE,
            )
            .await?;
            if let Some(error) = status_error(fetched.status, origin) {
                return Err(error);
            }
            let redirected = fetched
                .url
                .path_segments()
                .and_then(|mut s| s.rfind(|s| !s.is_empty()).map(String::from))
                .and_then(|last| canonical_of(&last))
                .filter(|c| *c != canonical)
                .ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
            canonical = redirected;
            data = self
                .graphql(
                    COLLECTION_QUERY,
                    json!({"canonical": canonical, "pageSize": PAGE_SIZE}),
                    origin,
                )
                .await?;
        }
        let collection = &data["smartCollectionByCanonical"];
        if !collection.is_object() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        if let Some(video) = util::text(&collection["video"]["canonical"]) {
            return self.resolve_video(&video, origin).await;
        }
        let mut seasons: Vec<&Value> = collection["seasons"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| season.is_none_or(|n| util::uint(&s["number"]) == Some(u64::from(n))))
            .collect();
        seasons.sort_by_key(|s| util::uint(&s["number"]).unwrap_or(0));
        let mut entries: Vec<PlaylistEntry> = Vec::new();
        let mut total = 0usize;
        for season in seasons {
            total += util::uint(&season["episodes"]["totalCount"]).unwrap_or(0) as usize;
            for episode in season["episodes"]["nodes"].as_array().into_iter().flatten() {
                let Some(link) = util::url_of(&episode["sharingUrl"], None).or_else(|| {
                    util::text(&episode["canonical"])
                        .and_then(|c| Url::parse(&format!("https://www.zdf.de/video/{c}")).ok())
                }) else {
                    continue;
                };
                if entries.iter().any(|e| e.url == link) {
                    continue;
                }
                entries.push(PlaylistEntry {
                    url: link,
                    title: episode["title"].as_str().and_then(clean_title),
                    duration: util::seconds(&episode["currentMedia"]["nodes"][0]["duration"]),
                });
            }
        }
        if entries.is_empty() {
            return Err(match season {
                Some(n) => {
                    ResolveError::unavailable(origin, format!("the collection has no season {n}"))
                }
                None => ResolveError::NotFound(origin.clone()),
            });
        }
        let title = collection["title"].as_str().and_then(clean_title);
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(match season {
                Some(n) => format!("{canonical}-s{n}"),
                None => canonical.clone(),
            }),
            title: match (title, season) {
                (Some(title), Some(n)) => Some(format!("{title} - Staffel {n}")),
                (title, _) => title,
            },
            total: Some(total.max(entries.len())),
            entries,
        }))
    }

    /// The channels the live TV page lists, each a live link.
    async fn resolve_live_channels(&self, origin: &Url) -> Result<Resolution, ResolveError> {
        let page = Url::parse(LIVE_PAGE).expect("valid");
        let fetched = fetch(
            &self.http,
            &page,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(fetched.status, origin) {
            return Err(error);
        }
        let html = fetched.text();
        let mut entries: Vec<PlaylistEntry> = Vec::new();
        for caps in RE_LIVE_CHANNEL.captures_iter(&html) {
            let canonical = caps[0].to_string();
            let Ok(link) = Url::parse(&format!("https://www.zdf.de/live-tv/{canonical}")) else {
                continue;
            };
            if entries.iter().any(|e| e.url == link) {
                continue;
            }
            entries.push(PlaylistEntry {
                url: link,
                title: Some(format!("{} Livestream", channel_name(&caps[1]))),
                duration: None,
            });
        }
        if entries.is_empty() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some("live-tv".to_string()),
            title: Some("ZDF Live TV".to_string()),
            total: Some(entries.len()),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for ZdfResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "ZDF",
            hosts: &["zdf.de", "zdfheute.de", "zdftivi.de", "logo.de"],
            features: &[
                "videos",
                "live",
                "series",
                "magazines",
                "films",
                "news",
                "subtitles",
            ],
            formats: &["mp4", "webm", "hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::News, Tag::Video, Tag::Live],
            session: SessionSupport::None,
            examples: &[
                "https://www.zdf.de/video/dokus/ein-tag-im-juli---ahrtalflut-2021-movie-100/terra-x-history-ein-tag-im-juli-ahrtalflut-2021-100",
                "https://www.zdf.de/dokumentation/dokumentation-sonstige/terra-x-history-ein-tag-im-juli-ahrtalflut-2021-100.html",
                "https://www.zdf.de/magazine/heute-journal-104",
                "https://www.zdf.de/dokus/ein-tag-im-juli---ahrtalflut-2021-movie-100",
                "https://www.zdf.de/live-tv",
                "https://www.zdf.de/play/live-tv/sender/zdf-live-beitrag-100",
                "https://www.zdfheute.de/video/heute-journal/heute-journal-vom-19-dezember-2025-100.html",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Video { canonical } => self.resolve_video(&canonical, url).await,
            Link::Collection { canonical, season } => {
                self.resolve_collection(&canonical, season, url).await
            }
            Link::LiveChannels => self.resolve_live_channels(url).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };
    use crate::resolve::VariantKind;

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

    fn token() -> Exchange {
        get(
            TOKEN_API,
            200,
            "application/json",
            json!({"type": "Bearer", "token": "087ee847a6d725180474cee3dcf62e3547702018", "expires": 4102444800i64}).to_string(),
        )
    }

    fn graphql(data: Value) -> Exchange {
        exchange(
            "POST",
            GRAPHQL,
            200,
            "application/json",
            json!({"data": data}).to_string(),
        )
    }

    fn track(uri: &str, class: &str, size: Option<u64>) -> Value {
        json!({"cdn": "akamai", "class": class, "language": "deu", "uri": uri, "filesize": size})
    }

    fn vod_ptmd() -> Value {
        json!({
            "attributes": {"duration": {"value": 2129000}, "geoLocation": {"value": "none"}},
            "basename": "260922_folge1_rehbraun_zei",
            "captions": [
                {"class": "hoh", "format": "ebu-tt-d-basic-de", "language": "deu", "uri": "https://utstreaming.zdf.de/mtt/x/Inside.xml"},
                {"class": "hoh", "format": "webvtt", "language": "deu", "uri": "https://utstreaming.zdf.de/mtt/x/Inside.vtt"}
            ],
            "priorityList": [{"formitaeten": [
                {"facets": ["progressive"], "mimeType": "video/webm", "type": "vp9_opus_webm_http_na_na", "qualities": [
                    {"highestVerticalResolution": 1080, "mimeCodec": "vp9, opus", "quality": "fhd", "audio": {"tracks": [track("https://nrodlzdf-a.akamaihd.net/x/rehbraun_4328k_p19v17.webm", "main", Some(1464293655))]}}
                ]},
                {"facets": [], "isAdaptive": true, "mimeType": "application/x-mpegURL", "type": "h264_aac_ts_http_m3u8_http", "qualities": [
                    {"highestVerticalResolution": 1080, "quality": "auto", "audio": {"tracks": [track("https://zdfvod.akamaized.net/x/all.csmil/master.m3u8", "main", None)]}},
                    {"highestVerticalResolution": 540, "quality": "med", "audio": {"tracks": [track("https://zdfvod.akamaized.net/x/med.csmil/master.m3u8", "main", None)]}}
                ]},
                {"facets": ["progressive"], "mimeType": "video/mp4", "type": "h264_aac_mp4_http_na_na", "qualities": [
                    {"highestVerticalResolution": 720, "mimeCodec": "avc1.640028, mp4a.40.2", "quality": "hd", "audio": {"tracks": [track("https://nrodlzdf-a.akamaihd.net/x/rehbraun_3328k_p15v17.mp4", "main", Some(684613358))]}},
                    {"highestVerticalResolution": 360, "mimeCodec": "avc1.4d401f, mp4a.40.2", "quality": "high", "audio": {"tracks": [track("https://nrodlzdf-a.akamaihd.net/x/rehbraun_808k_p11v17.mp4", "main", Some(167808027)), track("https://nrodlzdf-a.akamaihd.net/x/rehbraun_808k_p11v17_ad.mp4", "ad", Some(167808027))]}}
                ]},
                {"facets": ["restriction_useragent"], "mimeType": "video/mp4", "type": "h264_aac_mp4_http_na_na", "qualities": [
                    {"highestVerticalResolution": 540, "mimeCodec": "avc1.4d401f, mp4a.40.2", "quality": "veryhigh", "audio": {"tracks": [track("https://rodlzdf-a.akamaihd.net/x/rehbraun_1628k_p13v17.mp4", "main", Some(331223031))]}}
                ]}
            ]}]
        })
    }

    const MASTER: &str = "#EXTM3U\n#EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=424504,RESOLUTION=480x270,FRAME-RATE=25.000,CODECS=\"avc1.4d401f,mp4a.40.2\"\nhttps://zdfvod-rwrtr.akamaized.net/x/index-f1-v1-a1.m3u8\n#EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=3524000,RESOLUTION=1920x1080,FRAME-RATE=25.000,CODECS=\"avc1.64002a,mp4a.40.2\"\nhttps://zdfvod-rwrtr.akamaized.net/x/index-f5-v1-a1.m3u8\n";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n0.ts\n#EXTINF:5.0,\n1.ts\n#EXT-X-ENDLIST\n";
    const LIVE_MEDIA: &str =
        "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n0.ts\n#EXTINF:4.0,\n1.ts\n";

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let video = |c: &str| {
            Some(Link::Video {
                canonical: c.into(),
            })
        };
        assert_eq!(
            link(
                "https://www.zdf.de/video/dokus/inside-cdu-102/inside-cdu-staffel-2-folge-1-rehbraun-100"
            ),
            video("inside-cdu-staffel-2-folge-1-rehbraun-100")
        );
        assert_eq!(
            link(
                "https://www.zdf.de/play/dokus/inside-cdu-102/inside-cdu-staffel-2-folge-1-rehbraun-100"
            ),
            video("inside-cdu-staffel-2-folge-1-rehbraun-100")
        );
        assert_eq!(
            link(
                "https://www.zdf.de/dokumentation/dokumentation-sonstige/terra-x-history-ein-tag-im-juli-ahrtalflut-2021-100.html"
            ),
            video("terra-x-history-ein-tag-im-juli-ahrtalflut-2021-100")
        );
        assert_eq!(
            link("https://www.zdf.de/play/live-tv/sender/zdf-live-beitrag-100"),
            video("zdf-live-beitrag-100")
        );
        assert_eq!(
            link("https://www.zdf.de/live-tv/zdfneo-live-beitrag-100"),
            video("zdfneo-live-beitrag-100")
        );
        assert_eq!(link("https://www.zdf.de/live-tv"), Some(Link::LiveChannels));
        assert_eq!(
            link(
                "https://www.zdfheute.de/video/heute-journal/heute-journal-vom-19-dezember-2025-100.html"
            ),
            video("heute-journal-vom-19-dezember-2025-100")
        );
        assert_eq!(
            link(
                "https://www.zdfheute.de/politik/deutschland/wildberger-ki-einsatz-rede-texte-100.html"
            ),
            video("wildberger-ki-einsatz-rede-texte-100")
        );
        assert_eq!(
            link("https://www.logo.de/logo-vom-freitag-19-dezember-2025-102.html"),
            video("logo-vom-freitag-19-dezember-2025-102")
        );
        assert_eq!(
            link("https://www.zdftivi.de/kinder/tivi/logo-vom-freitag-100.html"),
            video("logo-vom-freitag-100")
        );
        assert_eq!(
            link("https://www.zdf.de/dokus/inside-cdu-102"),
            Some(Link::Collection {
                canonical: "inside-cdu-102".into(),
                season: None
            })
        );
        assert_eq!(
            link("https://www.zdf.de/magazine/heute-journal-104?staffel=2025"),
            Some(Link::Collection {
                canonical: "heute-journal-104".into(),
                season: Some(2025)
            })
        );
        assert_eq!(
            link("https://www.zdf.de/sport/das-aktuelle-sportstudio"),
            Some(Link::Collection {
                canonical: "das-aktuelle-sportstudio".into(),
                season: None
            })
        );
        assert_eq!(link("https://www.zdf.de/"), None);
        assert_eq!(link("https://www.zdf.de/kinder"), None);
        assert_eq!(link("https://www.zdfheute.de/"), None);
        assert_eq!(link("https://www.zdfheute.de/politik/"), None);
        assert_eq!(link("https://example.com/video/x/y"), None);
    }

    #[tokio::test]
    async fn videos_resolve_with_files_by_height_renditions_and_captions() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(token());
        fixture.exchanges.push(graphql(json!({"videoByCanonical": {
            "canonical": "inside-cdu-staffel-2-folge-1-rehbraun-100", "title": "Rehbraun",
            "sharingUrl": "https://www.zdf.de/video/dokus/inside-cdu-102/inside-cdu-staffel-2-folge-1-rehbraun-100",
            "leadParagraph": "Für die Partei stehen Kämpfe auf vielen Ebenen an.", "editorialDate": "2026-09-22T16:00:00.000000+00:00",
            "teaser": {"description": "Für die Partei beginnt ein Kampf.", "image": {"list": {"dim1140x120": "https://www.zdf.de/assets/x~1140x120", "dim1920x1080": "https://www.zdf.de/assets/x~1920x1080?cb=1"}}},
            "episodeInfo": {"episodeNumber": 1, "seasonNumber": 2},
            "smartCollection": {"canonical": "inside-cdu-102", "title": "Inside CDU", "sharingUrl": "https://www.zdf.de/dokus/inside-cdu-102"},
            "currentMedia": {"nodes": [
                {"id": "260922_folge1_rehbraun_zei", "ptmdTemplate": "/tmd/2/{playerId}/vod/ptmd/mediathek/260922_folge1_rehbraun_zei/2", "duration": 2129, "aspectRatio": "16:9", "vodMediaType": "DEFAULT", "label": "Normal"}
            ]}
        }})));
        fixture.exchanges.push(get(
            "https://api.zdf.de/tmd/2/android_native_6/vod/ptmd/mediathek/260922_folge1_rehbraun_zei/2",
            200,
            "application/json",
            vod_ptmd().to_string(),
        ));
        fixture.exchanges.push(get(
            "https://zdfvod.akamaized.net/x/all.csmil/master.m3u8",
            200,
            "application/x-mpegURL",
            MASTER.into(),
        ));
        fixture.exchanges.push(get(
            "https://zdfvod-rwrtr.akamaized.net/x/index-f5-v1-a1.m3u8",
            200,
            "application/x-mpegURL",
            MEDIA.into(),
        ));
        let resolver = ZdfResolver::new(Http::replay(fixture));
        let url = Url::parse(
            "https://www.zdf.de/video/dokus/inside-cdu-102/inside-cdu-staffel-2-folge-1-rehbraun-100",
        )
        .unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(
            resolved.id.as_deref(),
            Some("inside-cdu-staffel-2-folge-1-rehbraun-100")
        );
        assert_eq!(resolved.title.as_deref(), Some("Rehbraun"));
        assert_eq!(
            resolved.description.as_deref(),
            Some("Für die Partei stehen Kämpfe auf vielen Ebenen an.")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("Inside CDU"));
        assert_eq!(
            resolved.uploader_url.as_ref().map(|u| u.as_str()),
            Some("https://www.zdf.de/dokus/inside-cdu-102")
        );
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.duration, Some(Duration::from_secs(2129)));
        assert!(!resolved.live);
        assert_eq!(
            resolved.thumbnail.as_ref().map(|u| u.as_str()),
            Some("https://www.zdf.de/assets/x~1920x1080?cb=1")
        );
        assert_eq!(
            resolved.webpage_url.as_ref().map(|u| u.as_str()),
            Some(
                "https://www.zdf.de/video/dokus/inside-cdu-102/inside-cdu-staffel-2-folge-1-rehbraun-100"
            )
        );
        let files: Vec<&Variant> = resolved
            .variants
            .iter()
            .filter(|v| v.kind == VariantKind::File)
            .collect();
        assert_eq!(
            files.len(),
            4,
            "the WebM, two MP4 files and the audio description"
        );
        assert_eq!(files[0].container, Some(Container::Webm));
        assert_eq!(files[0].height, Some(1080));
        assert_eq!(files[0].width, Some(1920));
        assert_eq!(files[0].video, Some(VideoCodec::Vp9));
        assert_eq!(files[0].audio, Some(AudioCodec::Opus));
        assert_eq!(files[0].size, Some(1464293655));
        assert_eq!(files[0].bitrate, Some(4_328_000));
        assert_eq!(files[0].format_id.as_deref(), Some("webm-1080p"));
        assert_eq!(files[0].language.as_deref(), Some("de"));
        assert_eq!(files[1].container, Some(Container::Mp4));
        assert_eq!(files[1].height, Some(720));
        assert_eq!(files[1].video, Some(VideoCodec::H264));
        assert_eq!(files[1].audio, Some(AudioCodec::Aac));
        assert_eq!(files[1].label.as_deref(), Some("720p"));
        assert_eq!(files[1].duration, Some(Duration::from_secs(2129)));
        assert_eq!(files[3].format_id.as_deref(), Some("mp4-360p-ad"));
        assert_eq!(files[3].label.as_deref(), Some("360p AD"));
        let renditions: Vec<&Variant> = resolved
            .variants
            .iter()
            .filter(|v| v.kind == VariantKind::Hls)
            .collect();
        assert_eq!(renditions.len(), 2, "the full master's renditions, once");
        assert_eq!(renditions[1].height, Some(1080));
        assert_eq!(renditions[1].duration, Some(Duration::from_secs(15)));
        assert_eq!(resolved.subtitles.len(), 2);
        assert_eq!(resolved.subtitles[0].format, SubtitleFormat::Ttml);
        assert_eq!(resolved.subtitles[1].format, SubtitleFormat::Vtt);
        assert_eq!(resolved.subtitles[1].language, "de");
        assert_eq!(
            resolved.subtitles[1].name.as_deref(),
            Some("Untertitel für Hörgeschädigte")
        );
    }

    #[tokio::test]
    async fn live_channels_are_live_and_the_live_page_lists_them() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(token());
        fixture.exchanges.push(graphql(json!({"videoByCanonical": {
            "canonical": "zdf-live-beitrag-100", "title": "ZDF Livestream", "sharingUrl": "https://www.zdf.de/live-tv/zdf-live-beitrag-100",
            "leadParagraph": "Das ZDF im Livestream.", "editorialDate": "2026-09-06T22:58:00.000000+00:00",
            "teaser": {"image": {"list": {"dim1280x720": "https://www.zdf.de/assets/2400-zdf-100~1280x720"}}},
            "smartCollection": null,
            "currentMedia": {"nodes": [{"id": "247onAir-201", "ptmdTemplate": "/tmd/2/{playerId}/live/ptmd/247onAir-201", "liveMediaType": "DEFAULT", "encryption": null, "label": "Normal"}]}
        }})));
        fixture.exchanges.push(get(
            "https://api.zdf.de/tmd/2/android_native_6/live/ptmd/247onAir-201",
            200,
            "application/json",
            json!({"attributes": {"geoLocation": {"value": "de"}}, "captions": [], "priorityList": [{"formitaeten": [
                {"facets": ["https"], "isAdaptive": true, "mimeType": "application/x-mpegURL", "type": "h264_aac_ts_http_m3u8_http", "qualities": [
                    {"mimeCodec": "avc1.42E01E, mp4a.40.2", "quality": "auto", "audio": {"tracks": [track("https://zdf-hls-15.akamaized.net/hls/live/2016498/de/high/master.m3u8", "main", None)]}},
                    {"mimeCodec": "avc1.42E01E, mp4a.40.2", "quality": "low", "audio": {"tracks": [track("https://zdf-hls-15.akamaized.net/hls/live/2016498/de/low/master.m3u8", "main", None)]}}
                ]}
            ]}]}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://zdf-hls-15.akamaized.net/hls/live/2016498/de/high/master.m3u8",
            200,
            "application/x-mpegURL",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=3500000,RESOLUTION=1280x720,CODECS=\"avc1.42E01E,mp4a.40.2\"\n720.m3u8\n".into(),
        ));
        fixture.exchanges.push(get(
            "https://zdf-hls-15.akamaized.net/hls/live/2016498/de/high/720.m3u8",
            200,
            "application/x-mpegURL",
            LIVE_MEDIA.into(),
        ));
        fixture.exchanges.push(get(
            LIVE_PAGE,
            200,
            "text/html",
            r#"<html><body><a href="/play/live-tv/sender/zdf-live-beitrag-100">ZDF</a><a href="/play/live-tv/sender/zdfneo-live-beitrag-100">neo</a><script>"zdf-live-beitrag-100","zdfinfo-live-beitrag-100"</script></body></html>"#.into(),
        ));
        let resolver = ZdfResolver::new(Http::replay(fixture));
        let live = resolver
            .resolve(
                &Url::parse("https://www.zdf.de/play/live-tv/sender/zdf-live-beitrag-100").unwrap(),
            )
            .await
            .unwrap()
            .media()
            .unwrap();
        assert!(live.live);
        assert_eq!(live.title.as_deref(), Some("ZDF Livestream"));
        assert_eq!(live.uploader.as_deref(), Some("ZDF"));
        assert_eq!(live.duration, None);
        assert_eq!(live.variants.len(), 1);
        assert!(live.variants[0].live);
        assert_eq!(live.variants[0].height, Some(720));
        assert_eq!(
            live.thumbnail.as_ref().map(|u| u.as_str()),
            Some("https://www.zdf.de/assets/2400-zdf-100~1280x720")
        );
        let Resolution::Playlist(channels) = resolver
            .resolve(&Url::parse("https://www.zdf.de/live-tv").unwrap())
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(channels.entries.len(), 3);
        assert_eq!(
            channels.entries[0].url.as_str(),
            "https://www.zdf.de/live-tv/zdf-live-beitrag-100"
        );
        assert_eq!(channels.entries[0].title.as_deref(), Some("ZDF Livestream"));
        assert_eq!(
            channels.entries[1].title.as_deref(),
            Some("ZDFneo Livestream")
        );
        assert_eq!(
            channels.entries[2].title.as_deref(),
            Some("ZDFinfo Livestream")
        );
    }

    #[tokio::test]
    async fn collections_list_their_episodes_by_season_and_films_resolve_to_their_video() {
        let episode = |canonical: &str, title: &str, duration: u64| {
            json!({"canonical": canonical, "sharingUrl": format!("https://www.zdf.de/video/dokus/inside-cdu-102/{canonical}"), "title": title,
                "editorialDate": "2026-09-22T16:00:00.000000+00:00", "currentMedia": {"nodes": [{"duration": duration}]}})
        };
        let series = json!({"smartCollectionByCanonical": {
            "__typename": "HybridBingeSeriesSmartCollection", "canonical": "inside-cdu-102", "title": "Inside CDU",
            "infoText": "In zwei Staffeln blickt die Doku-Reihe hinter die Kulissen der CDU.", "sharingUrl": "https://www.zdf.de/dokus/inside-cdu-102",
            "seasons": {"totalCount": 2, "nodes": [
                {"number": 2, "episodes": {"totalCount": 4, "nodes": [episode("inside-cdu-staffel-2-folge-1-rehbraun-100", "Rehbraun", 2129), episode("inside-cdu-staffel-2-folge-2-retro-100", "Retro", 2044)]}},
                {"number": 1, "episodes": {"totalCount": 5, "nodes": [episode("-alphatiere-100", "Alphatiere", 2600)]}}
            ]}
        }});
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(token());
        fixture.exchanges.push(graphql(series.clone()));
        fixture.exchanges.push(graphql(series));
        fixture.exchanges.push(graphql(json!({"smartCollectionByCanonical": {
            "__typename": "MovieSmartCollection", "canonical": "ein-tag-im-juli---ahrtalflut-2021-movie-100", "title": "Ein Tag im Juli - Ahrtalflut 2021",
            "video": {"canonical": "terra-x-history-ein-tag-im-juli-ahrtalflut-2021-100", "sharingUrl": "https://www.zdf.de/video/dokus/ein-tag-im-juli---ahrtalflut-2021-movie-100/terra-x-history-ein-tag-im-juli-ahrtalflut-2021-100", "title": "Ein Tag im Juli - Ahrtalflut 2021"}
        }})));
        fixture.exchanges.push(graphql(json!({"videoByCanonical": {
            "canonical": "terra-x-history-ein-tag-im-juli-ahrtalflut-2021-100", "title": "Ein Tag im Juli - Ahrtalflut 2021",
            "sharingUrl": "https://www.zdf.de/video/dokus/ein-tag-im-juli---ahrtalflut-2021-movie-100/terra-x-history-ein-tag-im-juli-ahrtalflut-2021-100",
            "teaser": {"image": {"list": {}}}, "smartCollection": {"canonical": "ein-tag-im-juli---ahrtalflut-2021-movie-100", "title": "Ein Tag im Juli - Ahrtalflut 2021", "sharingUrl": "https://www.zdf.de/dokus/ein-tag-im-juli---ahrtalflut-2021-movie-100"},
            "currentMedia": {"nodes": [{"id": "260519_2015_sendung_his", "ptmdTemplate": "/tmd/2/{playerId}/vod/ptmd/mediathek/260519_2015_sendung_his/2", "duration": 5304, "aspectRatio": "16:9", "vodMediaType": "DEFAULT"}]}
        }})));
        fixture.exchanges.push(get(
            "https://api.zdf.de/tmd/2/android_native_6/vod/ptmd/mediathek/260519_2015_sendung_his/2",
            200,
            "application/json",
            json!({"attributes": {"duration": {"value": 5304000}}, "captions": [], "priorityList": [{"formitaeten": [
                {"facets": ["progressive"], "mimeType": "video/mp4", "type": "h264_aac_mp4_http_na_na", "qualities": [
                    {"highestVerticalResolution": 720, "mimeCodec": "avc1.640028, mp4a.40.2", "quality": "hd", "audio": {"tracks": [track("https://nrodlzdf-a.akamaihd.net/y/his_3328k_p15v17.mp4", "main", Some(1000))]}}
                ]}
            ]}]}).to_string(),
        ));
        let resolver = ZdfResolver::new(Http::replay(fixture));
        let Resolution::Playlist(all) = resolver
            .resolve(&Url::parse("https://www.zdf.de/dokus/inside-cdu-102").unwrap())
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(all.id.as_deref(), Some("inside-cdu-102"));
        assert_eq!(all.title.as_deref(), Some("Inside CDU"));
        assert_eq!(all.total, Some(9));
        assert_eq!(all.entries.len(), 3);
        assert_eq!(all.entries[0].title.as_deref(), Some("Alphatiere"));
        assert_eq!(
            all.entries[1].url.as_str(),
            "https://www.zdf.de/video/dokus/inside-cdu-102/inside-cdu-staffel-2-folge-1-rehbraun-100"
        );
        assert_eq!(all.entries[1].duration, Some(Duration::from_secs(2129)));
        let Resolution::Playlist(second) = resolver
            .resolve(&Url::parse("https://www.zdf.de/dokus/inside-cdu-102?staffel=2").unwrap())
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(second.id.as_deref(), Some("inside-cdu-102-s2"));
        assert_eq!(second.title.as_deref(), Some("Inside CDU - Staffel 2"));
        assert_eq!(second.total, Some(4));
        assert_eq!(second.entries.len(), 2);
        let film = resolver
            .resolve(
                &Url::parse("https://www.zdf.de/dokus/ein-tag-im-juli---ahrtalflut-2021-movie-100")
                    .unwrap(),
            )
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(
            film.id.as_deref(),
            Some("terra-x-history-ein-tag-im-juli-ahrtalflut-2021-100")
        );
        assert_eq!(
            film.title.as_deref(),
            Some("Ein Tag im Juli - Ahrtalflut 2021")
        );
        assert_eq!(film.duration, Some(Duration::from_secs(5304)));
        assert_eq!(film.variants.len(), 1);
        assert_eq!(film.variants[0].height, Some(720));
    }

    #[tokio::test]
    async fn news_videos_come_from_the_document_api_and_missing_ones_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(token());
        fixture
            .exchanges
            .push(graphql(json!({"videoByCanonical": null})));
        fixture.exchanges.push(get(
            &format!("{DOCUMENT_API}berlin-flughafen-ber-drohnen-alarm-100"),
            200,
            "application/json",
            json!({"document": {
                "id": "berlin-flughafen-ber-drohnen-alarm-100", "type": "video", "contentType": "news",
                "titel": "Drohnenalarm am Berliner Flughafen BER - Flüge umgeleitet", "beschreibung": "Am Flughafen BER wurden Drohnen gesichtet.",
                "date": "24.09.2026 19:20", "sharingUrl": "https://www.zdfheute.de/panorama/kriminalitaet/berlin-flughafen-ber-drohnen-alarm-100.html",
                "brandTitle": "ZDFheute", "streamApiUrlAndroid": "https://api.zdf.de/tmd/2/android_native_5/vod/ptmd/mediathek/260924_drohne_ber_x09/1",
                "streams": [{"sourceVariant": "default", "label": "Normal", "extId": "260924_drohne_ber_x09", "streamApiUrlAndroid": "https://api.zdf.de/tmd/2/android_native_5/vod/ptmd/mediathek/260924_drohne_ber_x09/1"}],
                "captions": [{"class": "hoh", "format": "webvtt", "language": "deu", "uri": "https://utstreaming.zdf.de/mtt/z/drohne.vtt"}],
                "teaserBild": {"1140": {"url": "https://www.zdfheute.de/assets/drohne-100~1140x240", "width": 1140, "height": 240}, "1920": {"url": "https://www.zdfheute.de/assets/drohne-100~1920x1080", "width": 1920, "height": 1080}}
            }, "meta": {"editorialDate": "2026-09-24T19:20:00.000+02:00"}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.zdf.de/tmd/2/android_native_5/vod/ptmd/mediathek/260924_drohne_ber_x09/1",
            200,
            "application/json",
            json!({"attributes": {"duration": {"value": 30000}}, "captions": [], "priorityList": [{"formitaeten": [
                {"facets": ["progressive"], "mimeType": "video/mp4", "type": "h264_aac_mp4_http_na_na", "qualities": [
                    {"highestVerticalResolution": 1080, "mimeCodec": "avc1.64002a, mp4a.40.2", "quality": "fhd", "audio": {"tracks": [track("https://nrodlzdf-a.akamaihd.net/z/drohne_6628k_p61v17.mp4", "main", Some(24000000))]}}
                ]}
            ]}]}).to_string(),
        ));
        fixture
            .exchanges
            .push(graphql(json!({"videoByCanonical": null})));
        fixture.exchanges.push(get(
            &format!("{DOCUMENT_API}gone-100"),
            404,
            "application/json",
            json!({"error": "not found"}).to_string(),
        ));
        let resolver = ZdfResolver::new(Http::replay(fixture));
        let news = resolver
            .resolve(&Url::parse("https://www.zdfheute.de/panorama/kriminalitaet/berlin-flughafen-ber-drohnen-alarm-100.html").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(
            news.id.as_deref(),
            Some("berlin-flughafen-ber-drohnen-alarm-100")
        );
        assert_eq!(
            news.title.as_deref(),
            Some("Drohnenalarm am Berliner Flughafen BER - Flüge umgeleitet")
        );
        assert_eq!(news.uploader.as_deref(), Some("ZDFheute"));
        assert!(news.uploaded_at.is_some());
        assert_eq!(news.duration, Some(Duration::from_secs(30)));
        assert_eq!(
            news.thumbnail.as_ref().map(|u| u.as_str()),
            Some("https://www.zdfheute.de/assets/drohne-100~1920x1080")
        );
        assert_eq!(news.variants.len(), 1);
        assert_eq!(news.variants[0].height, Some(1080));
        assert_eq!(news.variants[0].size, Some(24000000));
        assert_eq!(news.subtitles.len(), 1);
        assert_eq!(news.subtitles[0].format, SubtitleFormat::Vtt);
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.zdfheute.de/politik/gone-100.html").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    /// Every example link resolves live: videos with files by height, collections with
    /// episodes, the live TV page with its channels and a channel with its stream.
    #[tokio::test]
    #[ignore = "requires live ZDF access"]
    async fn live_examples_resolve() {
        let resolver = ZdfResolver::new(Http::new(crate::http::HttpConfig::default()));
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
            let resolution = tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&url))
                .await
                .expect("resolution timed out")
                .unwrap();
            match resolution {
                Resolution::Media(resolved) => {
                    assert!(
                        resolved.variants.iter().any(|v| v.is_playable()),
                        "{link}: no playable variant"
                    );
                    assert!(resolved.title.is_some(), "{link}: no title");
                    if !resolved.live {
                        assert!(
                            resolved
                                .variants
                                .iter()
                                .any(|v| v.kind == VariantKind::File && v.height.is_some()),
                            "{link}: no file by height"
                        );
                        assert!(!resolved.subtitles.is_empty(), "{link}: no subtitles");
                    }
                    println!(
                        "{link}: {:?}, live={}, {} variants, {} subtitles",
                        resolved.title,
                        resolved.live,
                        resolved.variants.len(),
                        resolved.subtitles.len()
                    );
                }
                Resolution::Playlist(playlist) => {
                    assert!(!playlist.entries.is_empty(), "{link}: no entries");
                    println!(
                        "{link}: {:?}, {} entries of {:?}",
                        playlist.title,
                        playlist.entries.len(),
                        playlist.total
                    );
                }
            }
        }
    }
}
