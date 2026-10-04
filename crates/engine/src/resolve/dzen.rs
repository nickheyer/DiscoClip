//! Dzen (the former Yandex Zen) videos, shorts, embeds, channels and articles: a video
//! page carries the player's data in the page's `_params`, with the MP4 files by quality
//! and the HLS playlist the player picks from; the site first sends a visitor through its
//! sign-on page, which names the page to come back to, so that page is fetched again. An
//! embed page carries the same streams without the detour. A channel's videos come from
//! the launcher export API, page by page. An article's body is a Draft.js document whose
//! embed blocks name the videos it carries, which are listed for the resolvers that read
//! them.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, Tag, Variant, clean_title, fetch, hls, navigation_headers, page, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "dzen";
const SITE: &str = "https://dzen.ru";
const EXPORT_API: &str = "https://dzen.ru/api/v3/launcher/export";
/// How many videos a channel is read up to.
const LISTING_LIMIT: usize = 100;

static RE_PUBLICATION_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-f]{24}$").unwrap());
/// `slug-60c7c443da18892ebfe85ed7`: an old media link ending in the publication id.
static RE_TRAILING_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:^|-)([0-9a-f]{24})$").unwrap());
static RE_UUID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]{8,}$").unwrap());
static RE_ARTICLE_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]{10,}$").unwrap());
static RE_CHANNEL_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9_.-]{1,}$").unwrap());
/// `var it = {…}`: the sign-on page's state, which names the page to come back to.
static RE_IT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:var|let|const)\s+it\s*=\s*(\{[^;]*\})\s*;").unwrap());
/// `_params = ({…})`: a page's data, one of which carries `ssrData`.
static RE_PARAMS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:var|let|const)\s+_params\s*=\s*\(").unwrap());

/// Paths on the site that are pages of their own, not channels.
const RESERVED: &[&str] = &[
    "a", "api", "auth", "brief", "embed", "help", "id", "list", "login", "media", "news",
    "profile", "search", "shorts", "topic", "video", "videos", "watch", "zen",
];

/// A channel, by the id of its `/id/` page or by the name of its own page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelId {
    Id(String),
    Name(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A video or a short, by its publication id.
    Video {
        id: String,
    },
    /// An embedded player, by the video's content id.
    Embed {
        uuid: String,
    },
    Article {
        id: String,
    },
    Channel {
        id: ChannelId,
    },
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !matches!(
        host.as_str(),
        "dzen.ru" | "www.dzen.ru" | "zen.yandex.ru" | "www.zen.yandex.ru"
    ) {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let publication = |slug: &str| {
        RE_TRAILING_ID
            .captures(slug)
            .map(|c| c[1].to_string())
            .filter(|id| RE_PUBLICATION_ID.is_match(id))
    };
    match segments.as_slice() {
        ["video", "watch", slug] | ["shorts", slug] | ["watch", slug] => Some(Link::Video {
            id: publication(slug)?,
        }),
        ["media", "id", _, slug] | ["media", _, slug] | ["media", slug] => Some(Link::Video {
            id: publication(slug)?,
        }),
        ["embed", uuid] if RE_UUID.is_match(uuid) => Some(Link::Embed {
            uuid: uuid.to_string(),
        }),
        ["a", id] if RE_ARTICLE_ID.is_match(id) => Some(Link::Article { id: id.to_string() }),
        ["id", id] if RE_PUBLICATION_ID.is_match(id) => Some(Link::Channel {
            id: ChannelId::Id(id.to_string()),
        }),
        [name] if RE_CHANNEL_NAME.is_match(name) && !RESERVED.contains(name) => {
            Some(Link::Channel {
                id: ChannelId::Name(name.to_string()),
            })
        }
        _ => None,
    }
}

/// The `ssrData` a page's `_params` carry.
pub fn ssr_data(html: &str) -> Option<Value> {
    for found in RE_PARAMS.find_iter(html) {
        let rest = &html[found.end()..];
        let Some(brace) = rest.find('{') else {
            continue;
        };
        if rest[..brace].trim().is_empty()
            && let Some((value, _)) = page::leading_json(&rest[brace..])
            && value.get("ssrData").is_some_and(Value::is_object)
        {
            return Some(value["ssrData"].clone());
        }
    }
    None
}

/// The page the sign-on page sends a visitor back to, when `html` is one.
pub fn sign_on_return(html: &str) -> Option<Url> {
    let state: Value = serde_json::from_str(&util::search(&RE_IT, html)?).ok()?;
    util::url_of(&state["retpath"], None)
        .filter(|u| u.host_str().is_some_and(|h| h.ends_with("dzen.ru")))
}

/// The height a stream's quality name stands for in the player's ladder.
fn quality_height(name: &str) -> Option<u32> {
    Some(match name {
        "tiny" | "mobile" => 144,
        "lowest" => 240,
        "low" => 360,
        "medium" | "sd" => 480,
        "high" | "hd" => 720,
        "fullhd" | "full" => 1080,
        "quad" => 1440,
        "ultra" => 2160,
        _ => return None,
    })
}

/// The height the `type` of a file link stands for.
fn type_height(url: &Url) -> Option<u32> {
    Some(match util::query_param(url, "type")?.as_str() {
        "4" => 144,
        "0" => 240,
        "1" => 360,
        "2" => 480,
        "3" => 720,
        "5" => 1080,
        "6" => 1440,
        "7" => 2160,
        _ => return None,
    })
}

/// The typed streams a player lists: `[{url, type}]`.
fn typed_streams(list: &Value) -> Vec<(String, Url)> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|stream| {
            Some((
                util::text(&stream["type"]).unwrap_or_default(),
                util::url_of(&stream["url"], None)?,
            ))
        })
        .collect()
}

/// The MP4 files among `streams`, with their heights, and the HLS playlists among them.
pub fn split_streams(
    streams: &[(String, Url)],
    resolutions: &Value,
    width: Option<u32>,
    height: Option<u32>,
) -> (Vec<Variant>, Vec<Url>) {
    let mut files: Vec<Variant> = Vec::new();
    let mut playlists: Vec<Url> = Vec::new();
    for (kind, url) in streams {
        let content = util::query_param(url, "ct").unwrap_or_default();
        let is_playlist = kind == "hls"
            || content == "8"
            || super::path_extension(url).as_deref() == Some("m3u8");
        if is_playlist {
            if !playlists.contains(url) {
                playlists.push(url.clone());
            }
            continue;
        }
        if kind == "dash" || content == "6" || super::path_extension(url).as_deref() == Some("mpd")
        {
            continue;
        }
        if files.iter().any(|v| v.url == *url) {
            continue;
        }
        let mut variant = Variant::file(url.clone());
        variant.container = Some(Container::Mp4);
        variant.video = Some(VideoCodec::H264);
        variant.audio = Some(AudioCodec::Aac);
        let ladder = quality_height(kind).or_else(|| type_height(url));
        variant.height = match (ladder, height) {
            (Some(h), Some(max)) => Some(h.min(max)),
            (Some(h), None) => Some(h),
            (None, max) => max,
        };
        let listed = variant.height.and_then(|h| {
            resolutions
                .as_array()
                .into_iter()
                .flatten()
                .find(|r| util::u32_of(&r["height"]) == Some(h))
        });
        variant.width = listed.and_then(|r| util::u32_of(&r["width"])).or(
            match (variant.height, width, height) {
                (Some(h), Some(w), Some(mh)) if mh > 0 => Some((h * w / mh) & !1),
                _ => None,
            },
        );
        variant.size = listed.and_then(|r| util::uint(&r["sizeInBytes"]));
        variant.format_id = (!kind.is_empty()).then(|| kind.clone());
        variant.label = variant.height.map(|h| format!("{h}p"));
        files.push(variant);
    }
    (files, playlists)
}

/// The video links an article's Draft.js body embeds, with their titles and lengths:
/// the site's own videos, its publications that are videos, and other sites' players.
pub fn article_videos(content_state: &str) -> Vec<PlaylistEntry> {
    let Ok(state) = serde_json::from_str::<Value>(content_state) else {
        return Vec::new();
    };
    let document = if state["draftJsState"].is_object() {
        &state["draftJsState"]
    } else {
        &state
    };
    let mut entries: Vec<PlaylistEntry> = Vec::new();
    for block in document["blocks"].as_array().into_iter().flatten() {
        if block["type"].as_str() != Some("atomic:embed") {
            continue;
        }
        let embed = &block["data"]["embedData"];
        let kind = embed["type"].as_str().unwrap_or("");
        let link = match kind {
            "yandex-zen-video" => util::text(&embed["publicationId"])
                .filter(|id| RE_PUBLICATION_ID.is_match(id))
                .and_then(|id| Url::parse(&format!("{SITE}/video/watch/{id}")).ok())
                .or_else(|| {
                    util::text(&embed["uuid"])
                        .filter(|uuid| RE_UUID.is_match(uuid))
                        .and_then(|uuid| Url::parse(&format!("{SITE}/embed/{uuid}")).ok())
                }),
            "yandex-zen-publication" => util::url_of(&embed["originalUrl"], None)
                .filter(|url| matches!(parse_link(url), Some(Link::Video { .. }))),
            _ => ["originalUrl", "src", "url"]
                .iter()
                .find_map(|key| util::url_of(&embed[key], None))
                .filter(|url| {
                    let host = url.host_str().unwrap_or("").trim_start_matches("www.");
                    matches!(
                        host,
                        "youtube.com"
                            | "m.youtube.com"
                            | "youtu.be"
                            | "vk.com"
                            | "vkvideo.ru"
                            | "rutube.ru"
                            | "ok.ru"
                    )
                }),
        };
        let Some(link) = link else {
            continue;
        };
        if entries.iter().any(|e| e.url == link) {
            continue;
        }
        entries.push(PlaylistEntry {
            url: link,
            title: embed["title"].as_str().and_then(clean_title),
            duration: util::millis(&embed["duration"]).filter(|d| !d.is_zero()),
        });
    }
    entries
}

pub struct DzenResolver {
    http: Http,
}

impl DzenResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// A page of the site, past the sign-on page it first sends a visitor to.
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
        let html = fetched.text();
        let Some(back) = sign_on_return(&html) else {
            return self.checked(fetched.status.as_u16(), html, origin);
        };
        let again = fetch(
            &self.http,
            &back,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        let status = again.status.as_u16();
        self.checked(status, again.text(), origin)
    }

    /// A page's text, unless its status says the page is missing or withheld.
    fn checked(&self, status: u16, html: String, origin: &Url) -> Result<String, ResolveError> {
        match status {
            200..=299 => Ok(html),
            404 | 410 => Err(ResolveError::NotFound(origin.clone())),
            429 => Err(ResolveError::RateLimited(origin.clone())),
            _ => Err(ResolveError::unavailable(
                origin,
                format!("the page answered HTTP {status}"),
            )),
        }
    }

    /// The MP4 files and the renditions of the playlists among `streams`.
    async fn variants_of(
        &self,
        streams: &[(String, Url)],
        resolutions: &Value,
        width: Option<u32>,
        height: Option<u32>,
        origin: &Url,
    ) -> Result<Vec<Variant>, ResolveError> {
        let (mut variants, playlists) = split_streams(streams, resolutions, width, height);
        let mut failure = None;
        for playlist in &playlists {
            match hls::expand(&self.http, playlist, PLATFORM, BROWSER_UA, &[]).await {
                Ok(expanded) => {
                    for mut variant in expanded.variants {
                        variant.format_id = Some(match &variant.label {
                            Some(label) => format!("hls-{label}"),
                            None => "hls".to_string(),
                        });
                        if !variants.iter().any(|v| v.url == variant.url) {
                            variants.push(variant);
                        }
                    }
                }
                Err(error) => failure = Some(error),
            }
        }
        if variants.is_empty() {
            return Err(failure.unwrap_or_else(|| {
                ResolveError::unavailable(origin, "the player names no stream")
            }));
        }
        Ok(variants)
    }

    async fn resolve_video(&self, id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let page_url = Url::parse(&format!("{SITE}/video/watch/{id}")).expect("valid");
        let html = self.page(&page_url, origin).await?;
        let data = ssr_data(&html)
            .ok_or_else(|| ResolveError::malformed(origin, "the page carries no data"))?;
        let meta = &data["videoMetaResponse"];
        if !meta.is_object() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        let video = &meta["video"];
        let mut streams = typed_streams(&video["oneVideoStreams"]);
        for url in video["streams"].as_array().into_iter().flatten() {
            if let Some(url) = util::url_of(url, None)
                && !streams.iter().any(|(_, u)| *u == url)
            {
                streams.push((String::new(), url));
            }
        }
        if let Some(url) = util::url_of(&video["id"], None)
            && !streams.iter().any(|(_, u)| *u == url)
        {
            streams.push((String::new(), url));
        }
        let width = util::u32_of(&video["width"]).filter(|w| *w > 0);
        let height = util::u32_of(&video["height"]).filter(|h| *h > 0);
        let variants = self
            .variants_of(&streams, &video["resolutions"], width, height, origin)
            .await?;
        let source = &meta["source"];
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.to_string());
        resolved.title = meta["title"].as_str().and_then(clean_title);
        resolved.description = meta["description"].as_str().and_then(clean_title);
        resolved.uploader = source["title"].as_str().and_then(clean_title);
        resolved.uploader_url = util::text(&source["url"])
            .filter(|name| RE_CHANNEL_NAME.is_match(name))
            .and_then(|name| Url::parse(&format!("{SITE}/{name}")).ok())
            .or_else(|| util::url_of(&source["feedShareLink"], None));
        resolved.uploaded_at = util::epoch(&meta["publicationDate"]);
        resolved.duration = util::seconds(&video["duration"])
            .filter(|d| !d.is_zero())
            .or_else(|| variants.iter().find_map(|v| v.duration));
        resolved.thumbnail = util::url_of(&meta["image"], None);
        resolved.webpage_url = util::url_of(&meta["link"], None).or(Some(page_url));
        resolved.age_limit = util::boolean(&meta["isAdult"])
            .filter(|adult| *adult)
            .map(|_| 18);
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn resolve_embed(&self, uuid: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let page_url = Url::parse(&format!("{SITE}/embed/{uuid}")).expect("valid");
        let html = self.page(&page_url, origin).await?;
        let data = ssr_data(&html)
            .ok_or_else(|| ResolveError::malformed(origin, "the embed carries no data"))?;
        let content = &data["exportResponse"]["content"];
        if !content.is_object() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        let streams = typed_streams(&content["streams"]);
        let variants = self
            .variants_of(&streams, &Value::Null, None, None, origin)
            .await?;
        let watch = util::url_of(&content["video_url"], None);
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = watch
            .as_ref()
            .and_then(|w| match parse_link(w) {
                Some(Link::Video { id }) => Some(id),
                _ => None,
            })
            .or_else(|| util::text(&content["content_id"]))
            .or_else(|| Some(uuid.to_string()));
        resolved.title = content["title"].as_str().and_then(clean_title);
        resolved.description = content["description"].as_str().and_then(clean_title);
        resolved.uploader = content["tag_attribute"].as_str().and_then(clean_title);
        resolved.uploaded_at = util::epoch(&content["publication_date"]);
        resolved.duration = util::seconds(&content["duration"])
            .filter(|d| !d.is_zero())
            .or_else(|| variants.iter().find_map(|v| v.duration));
        resolved.thumbnail = util::url_of(&content["thumbnail"], None);
        resolved.webpage_url = watch.or(Some(page_url));
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    /// One page of a channel's videos from the export API.
    async fn export(&self, url: &Url, origin: &Url) -> Result<Value, ResolveError> {
        let fetched = fetch(
            &self.http,
            url,
            PLATFORM,
            BROWSER_UA,
            &[("accept".to_string(), "application/json".to_string())],
            MAX_PAGE,
        )
        .await?;
        match fetched.status.as_u16() {
            200..=299 => {}
            404 | 410 => return Err(ResolveError::NotFound(origin.clone())),
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the export API answered HTTP {status}"),
                ));
            }
        }
        fetched.json(origin)
    }

    /// The channel's videos of one kind, page by page, and the channel's name.
    async fn channel_videos(
        &self,
        id: &ChannelId,
        content_type: &str,
        entries: &mut Vec<PlaylistEntry>,
        origin: &Url,
    ) -> Result<Option<String>, ResolveError> {
        let (key, value) = match id {
            ChannelId::Id(id) => ("channel_id", id.as_str()),
            ChannelId::Name(name) => ("channel_name", name.as_str()),
        };
        let mut next = Some(util::with_query(
            &Url::parse(EXPORT_API).expect("valid"),
            &[
                ("country_code", "ru"),
                ("lang", "ru"),
                ("clid", "1400"),
                (key, value),
                ("content_type", content_type),
            ],
        ));
        let mut name = None;
        while let Some(url) = next.take() {
            let feed = self.export(&url, origin).await?;
            let items = feed["items"].as_array().cloned().unwrap_or_default();
            let before = entries.len();
            for item in &items {
                let Some(link) = util::url_of(&item["link"], None) else {
                    continue;
                };
                let Some(Link::Video { id }) = parse_link(&link) else {
                    continue;
                };
                let Ok(url) = Url::parse(&format!("{SITE}/video/watch/{id}")) else {
                    continue;
                };
                if entries.iter().any(|e| e.url == url) {
                    continue;
                }
                if name.is_none() {
                    name = item["source"]["title"].as_str().and_then(clean_title);
                }
                entries.push(PlaylistEntry {
                    url,
                    title: item["title"].as_str().and_then(clean_title),
                    duration: util::seconds(&item["video"]["duration"]).filter(|d| !d.is_zero()),
                });
            }
            if entries.len() >= LISTING_LIMIT || entries.len() == before {
                break;
            }
            next = util::url_of(&feed["more"]["link"], None);
        }
        Ok(name)
    }

    async fn resolve_channel(
        &self,
        id: &ChannelId,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let mut entries = Vec::new();
        let mut name = self
            .channel_videos(id, "long_video", &mut entries, origin)
            .await?;
        if entries.len() < LISTING_LIMIT {
            let shorts = self
                .channel_videos(id, "short_video", &mut entries, origin)
                .await?;
            if name.is_none() {
                name = shorts;
            }
        }
        if entries.is_empty() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        entries.truncate(LISTING_LIMIT);
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(match id {
                ChannelId::Id(id) | ChannelId::Name(id) => id.clone(),
            }),
            title: name,
            total: Some(entries.len()),
            entries,
        }))
    }

    async fn resolve_article(&self, id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let page_url = Url::parse(&format!("{SITE}/a/{id}")).expect("valid");
        let html = self.page(&page_url, origin).await?;
        let data = ssr_data(&html)
            .ok_or_else(|| ResolveError::malformed(origin, "the article carries no data"))?;
        let article = &data["publishersResponse"]["data"]["data"];
        let publication = &article["publication"];
        if !publication.is_object() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        let entries = publication["content"]["articleContent"]["contentState"]
            .as_str()
            .map(article_videos)
            .unwrap_or_default();
        match entries.len() {
            0 => Err(ResolveError::unavailable(
                origin,
                "the article embeds no video",
            )),
            1 => Err(ResolveError::Redirect(entries[0].url.clone())),
            _ => Ok(Resolution::Playlist(Playlist {
                resolver: PLATFORM.to_string(),
                id: Some(id.to_string()),
                title: publication["title"]
                    .as_str()
                    .and_then(clean_title)
                    .or_else(|| article["og"]["title"].as_str().and_then(clean_title)),
                total: Some(entries.len()),
                entries,
            })),
        }
    }
}

#[async_trait]
impl Resolver for DzenResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Dzen",
            hosts: &["dzen.ru", "zen.yandex.ru"],
            features: &["videos", "shorts", "embeds", "channels", "articles"],
            formats: &["mp4", "hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::Video, Tag::Social],
            session: SessionSupport::None,
            on_by_default: true,
            examples: &[
                "https://dzen.ru/video/watch/6002240ff8b1af50bb2da5e3",
                "https://dzen.ru/shorts/6aabb0cc21a3b33d21018b48",
                "https://dzen.ru/embed/vtWLAXimtWS8",
                "https://dzen.ru/id/606fd806cc13cb3c58c05cf5",
                "https://dzen.ru/techinsider",
                "https://dzen.ru/a/apgr79izjnDkA_SX",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Video { id } => self.resolve_video(&id, url).await,
            Link::Embed { uuid } => self.resolve_embed(&uuid, url).await,
            Link::Article { id } => self.resolve_article(&id, url).await,
            Link::Channel { id } => self.resolve_channel(&id, url).await,
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
    use std::time::Duration;

    fn get(url: &str, status: u16, content_type: &str, body: &str) -> Exchange {
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

    const ID: &str = "6002240ff8b1af50bb2da5e3";
    const CDN: &str = "https://vd2.okcdn.ru";

    fn sign_on(back: &str) -> String {
        let back = back.replace('/', "\\u002F");
        format!(
            r#"<html><script>var it = {{"host":"https://sso.dzen.ru/install?uuid=x","retpath":"{back}","dzen":"1","root":"sso.passport.yandex.ru"}};</script></html>"#
        )
    }

    fn page_with(ssr: Value) -> String {
        format!(
            r#"<html><script>var _params = ({{"data":{{"cookieOfTheDay":"x"}},"namespaceKey":"y"}});</script><script>const _params = ({});</script></html>"#,
            json!({"ssrData": ssr})
        )
    }

    fn stream(kind: &str, kind_number: u8, content: u8) -> Value {
        json!({"url": format!("{CDN}/?expires=1&type={kind_number}&ct={content}&id=7315980946113"), "type": kind})
    }

    fn video_meta() -> Value {
        json!({
            "id": "-8268844160238087083", "title": "Извержение вулкана из спичек: зрелищный опыт",
            "description": "Канал Lavina собрал из сотен спичек внушительную модель вулкана",
            "image": "https://avatars.dzeninfra.ru/get-zen_doc/3006682/pub_x/smart_crop_516x290",
            "link": format!("https://dzen.ru/video/watch/{ID}"), "publicationDate": "1611378221", "tags": ["опыт"],
            "source": {"id": "3763559002235847595", "title": "TechInsider", "url": "techinsider"},
            "video": {
                "id": format!("{CDN}/video.m3u8?cmd=videoPlayerCdn&expires=1&type=2&ct=8&id=7315980946113"),
                "streams": [format!("{CDN}/video.m3u8?cmd=videoPlayerCdn&expires=1&type=2&ct=8&id=7315980946113")],
                "oneVideoStreams": [
                    {"url": format!("{CDN}/video.m3u8?cmd=videoPlayerCdn&expires=1&type=2&ct=8&id=7315980946113"), "type": "hls"},
                    {"url": format!("{CDN}/?expires=1&type=1&ct=6&id=7315980946113"), "type": "dash"},
                    stream("tiny", 4, 0), stream("lowest", 0, 0), stream("low", 1, 0), stream("medium", 2, 0), stream("high", 3, 0)
                ],
                "resolutions": [
                    {"width": 256, "height": 144}, {"width": 426, "height": 240, "sizeInBytes": 12115763},
                    {"width": 640, "height": 360}, {"width": 852, "height": 480, "sizeInBytes": 40423219},
                    {"width": 1280, "height": 720, "sizeInBytes": 70243150}
                ],
                "duration": 243, "views": 13561876, "width": 1280, "height": 720, "subtitles": [{"label": "Русский", "lang": "ru"}]
            }
        })
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let video = |id: &str| Some(Link::Video { id: id.into() });
        assert_eq!(
            link(&format!("https://dzen.ru/video/watch/{ID}")),
            video(ID)
        );
        assert_eq!(
            link(&format!(
                "https://dzen.ru/video/watch/{ID}?rid=1&referrer_clid=1400"
            )),
            video(ID)
        );
        assert_eq!(
            link(&format!("https://zen.yandex.ru/video/watch/{ID}")),
            video(ID)
        );
        assert_eq!(
            link("https://dzen.ru/shorts/6aabb0cc21a3b33d21018b48"),
            video("6aabb0cc21a3b33d21018b48")
        );
        assert_eq!(
            link(
                "https://dzen.ru/media/id/606fd806cc13cb3c58c05cf5/vot-eto-focus-dedy-morozy-na-gidrociklah-60c7c443da18892ebfe85ed7"
            ),
            video("60c7c443da18892ebfe85ed7")
        );
        assert_eq!(
            link("https://dzen.ru/embed/vtWLAXimtWS8"),
            Some(Link::Embed {
                uuid: "vtWLAXimtWS8".into()
            })
        );
        assert_eq!(
            link("https://dzen.ru/a/apgr79izjnDkA_SX"),
            Some(Link::Article {
                id: "apgr79izjnDkA_SX".into()
            })
        );
        assert_eq!(
            link("https://dzen.ru/id/606fd806cc13cb3c58c05cf5"),
            Some(Link::Channel {
                id: ChannelId::Id("606fd806cc13cb3c58c05cf5".into())
            })
        );
        assert_eq!(
            link("https://dzen.ru/techinsider?tab=articles"),
            Some(Link::Channel {
                id: ChannelId::Name("techinsider".into())
            })
        );
        assert_eq!(link("https://dzen.ru/"), None);
        assert_eq!(link("https://dzen.ru/video"), None);
        assert_eq!(link("https://dzen.ru/topic/baic-x75"), None);
        assert_eq!(link("https://dzen.ru/video/watch/notanid"), None);
        assert_eq!(link(&format!("https://example.com/video/watch/{ID}")), None);
    }

    #[test]
    fn article_bodies_name_their_videos() {
        let state = json!({"draftJsState": {"blocks": [
            {"type": "unstyled", "text": "Hello"},
            {"type": "atomic:image", "data": {"image": {"id": "1"}}},
            {"type": "atomic:embed", "data": {"embedData": {"type": "yandex-zen-video", "uuid": "oHkYKkKUPAAA", "publicationId": "6a982b57a6d9303399f80031", "title": "Дынька заскучала", "duration": 76000}}},
            {"type": "atomic:embed", "data": {"embedData": {"type": "yandex-zen-video", "uuid": "o-EbDycoOAAA", "title": "Магниты", "duration": 5000}}},
            {"type": "atomic:embed", "data": {"embedData": {"type": "yandex-zen-publication", "originalUrl": "https://dzen.ru/a/akzrLbkSuTJ_vC8J", "title": "An article"}}},
            {"type": "atomic:embed", "data": {"embedData": {"type": "yandex-zen-publication", "originalUrl": format!("https://dzen.ru/video/watch/{ID}"), "title": "A video publication"}}},
            {"type": "atomic:embed", "data": {"embedData": {"type": "yandex-music", "src": "https://music.yandex.ru/album/38909574", "title": "Рядом"}}},
            {"type": "atomic:embed", "data": {"embedData": {"type": "youtube", "src": "https://www.youtube.com/watch?v=BaW_jenozKc", "title": "A YouTube video"}}},
            {"type": "atomic:embed", "data": {"embedData": {"type": "yandex-zen-video", "uuid": "oHkYKkKUPAAA", "publicationId": "6a982b57a6d9303399f80031", "title": "Дынька заскучала"}}}
        ], "entityMap": {}}})
        .to_string();
        let videos = article_videos(&state);
        assert_eq!(videos.len(), 4);
        assert_eq!(
            videos[0].url.as_str(),
            "https://dzen.ru/video/watch/6a982b57a6d9303399f80031"
        );
        assert_eq!(videos[0].title.as_deref(), Some("Дынька заскучала"));
        assert_eq!(videos[0].duration, Some(Duration::from_secs(76)));
        assert_eq!(videos[1].url.as_str(), "https://dzen.ru/embed/o-EbDycoOAAA");
        assert_eq!(
            videos[2].url.as_str(),
            format!("https://dzen.ru/video/watch/{ID}")
        );
        assert_eq!(
            videos[3].url.as_str(),
            "https://www.youtube.com/watch?v=BaW_jenozKc"
        );
    }

    #[tokio::test]
    async fn videos_resolve_past_the_sign_on_page() {
        let watch = format!("https://dzen.ru/video/watch/{ID}");
        let back = format!("{watch}?is_autologin_ya=true");
        let master =
            format!("{CDN}/video.m3u8?cmd=videoPlayerCdn&expires=1&type=2&ct=8&id=7315980946113");
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture
            .exchanges
            .push(get(&watch, 200, "text/html", &sign_on(&back)));
        fixture.exchanges.push(get(
            &back,
            200,
            "text/html",
            &page_with(json!({"videoMetaResponse": video_meta(), "serverState": {}})),
        ));
        fixture.exchanges.push(get(
            &master,
            200,
            "application/vnd.apple.mpegurl",
            &format!("#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2312531,RESOLUTION=1280x720,CODECS=\"avc1.64001f,mp4a.40.2\"\n{CDN}/hls/high/index.m3u8\n"),
        ));
        fixture.exchanges.push(get(
            &format!("{CDN}/hls/high/index.m3u8"),
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n0.ts\n#EXT-X-ENDLIST\n",
        ));
        let resolver = DzenResolver::new(Http::replay(fixture));
        let url = Url::parse(&watch).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some(ID));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Извержение вулкана из спичек: зрелищный опыт")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("TechInsider"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://dzen.ru/techinsider"
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(243)));
        assert!(resolved.uploaded_at.is_some());
        assert!(resolved.thumbnail.is_some());
        assert_eq!(resolved.webpage_url.as_ref().unwrap().as_str(), watch);
        assert_eq!(
            resolved.variants.len(),
            6,
            "five files and one playlist rendition"
        );
        let high = resolved
            .variants
            .iter()
            .find(|v| v.format_id.as_deref() == Some("high"))
            .unwrap();
        assert_eq!(high.height, Some(720));
        assert_eq!(high.width, Some(1280));
        assert_eq!(high.size, Some(70243150));
        assert_eq!(high.container, Some(Container::Mp4));
        let tiny = resolved
            .variants
            .iter()
            .find(|v| v.format_id.as_deref() == Some("tiny"))
            .unwrap();
        assert_eq!(tiny.height, Some(144));
        assert_eq!(tiny.width, Some(256));
        assert!(
            resolved
                .variants
                .iter()
                .any(|v| v.format_id.as_deref() == Some("hls-720p"))
        );
        assert!(
            resolved
                .variants
                .iter()
                .all(|v| !v.url.as_str().contains("ct=6")),
            "DASH is left out"
        );
    }

    #[tokio::test]
    async fn embeds_and_channels_resolve_and_missing_videos_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://dzen.ru/embed/vtWLAXimtWS8",
            200,
            "text/html",
            &page_with(json!({"exportResponse": {"content": {
                "streams": [stream("tiny", 4, 0), stream("high", 3, 0)],
                "content_id": "vtWLAXimtWS8", "duration": "243", "publication_date": "1611378221",
                "title": "Извержение вулкана из спичек: зрелищный опыт", "description": "Канал Lavina",
                "thumbnail": "https://avatars.dzeninfra.ru/get-zen_doc/x/smart_crop_516x290",
                "video_url": format!("https://dzen.ru/video/watch/{ID}"), "tag_attribute": "TechInsider"
            }}})),
        ));
        let export =
            |query: &str| format!("{EXPORT_API}?country_code=ru&lang=ru&clid=1400&{query}");
        let item = |id: &str, title: &str, secs: u64| {
            json!({"type": "gif", "link": format!("https://dzen.ru/video/watch/{id}?rid=1"), "title": title,
                "source": {"title": "TechInsider"}, "video": {"duration": secs}})
        };
        fixture.exchanges.push(get(
            &export("channel_name=techinsider&content_type=long_video"),
            200,
            "application/json",
            &json!({"items": [item("6ab538611c3ac333abb01729", "5 глупых вопросов", 509), item("6aaa4e1ac7ead23311717af5", "Обновленный Haval H7", 227), {"type": "brief"}],
                "more": {"link": "https://dzen.ru/api/v3/launcher/channel-more?channel_name=techinsider&content_type=long_video&next_page_id=abc"}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://dzen.ru/api/v3/launcher/channel-more?channel_name=techinsider&content_type=long_video&next_page_id=abc",
            200,
            "application/json",
            &json!({"items": [item("6a1803595b5a5d7e55492901", "Third", 100)], "more": {"link": null}}).to_string(),
        ));
        fixture.exchanges.push(get(
            &export("channel_name=techinsider&content_type=short_video"),
            200,
            "application/json",
            &json!({"items": [{"type": "short_video_compact", "link": "https://dzen.ru/shorts/6aabb0cc21a3b33d21018b48?rid=2", "title": "Зачем в крекерах делают дырки", "source": {"title": "TechInsider"}, "video": {"duration": 84}}], "more": {}}).to_string(),
        ));
        fixture.exchanges.push(get(
            &export("channel_name=nobody&content_type=long_video"),
            200,
            "application/json",
            &json!({"items": [], "more": {}}).to_string(),
        ));
        fixture.exchanges.push(get(
            &export("channel_name=nobody&content_type=short_video"),
            200,
            "application/json",
            &json!({"items": [], "more": {}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://dzen.ru/video/watch/000000000000000000000000",
            404,
            "text/html",
            "<html><title>Смотреть видео от авторов и СМИ | Дзен</title></html>",
        ));
        let resolver = DzenResolver::new(Http::replay(fixture));
        let embed = resolver
            .resolve(&Url::parse("https://dzen.ru/embed/vtWLAXimtWS8").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(embed.id.as_deref(), Some(ID));
        assert_eq!(embed.uploader.as_deref(), Some("TechInsider"));
        assert_eq!(embed.duration, Some(Duration::from_secs(243)));
        assert_eq!(
            embed.webpage_url.as_ref().unwrap().as_str(),
            format!("https://dzen.ru/video/watch/{ID}")
        );
        assert_eq!(embed.variants.len(), 2);
        assert_eq!(embed.variants[1].height, Some(720));
        let Resolution::Playlist(channel) = resolver
            .resolve(&Url::parse("https://dzen.ru/techinsider").unwrap())
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(channel.title.as_deref(), Some("TechInsider"));
        assert_eq!(channel.entries.len(), 4, "three long videos and a short");
        assert_eq!(
            channel.entries[0].url.as_str(),
            "https://dzen.ru/video/watch/6ab538611c3ac333abb01729"
        );
        assert_eq!(channel.entries[0].duration, Some(Duration::from_secs(509)));
        assert_eq!(
            channel.entries[3].url.as_str(),
            "https://dzen.ru/video/watch/6aabb0cc21a3b33d21018b48"
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://dzen.ru/nobody").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        assert!(matches!(
            resolver
                .resolve(
                    &Url::parse("https://dzen.ru/video/watch/000000000000000000000000").unwrap()
                )
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn articles_list_their_videos_or_hand_the_one_on() {
        let article = |embeds: Vec<Value>| {
            let state = json!({"draftJsState": {"blocks": embeds.into_iter().map(|e| json!({"type": "atomic:embed", "data": {"embedData": e}})).collect::<Vec<_>>(), "entityMap": {}}});
            page_with(json!({"publishersResponse": {"data": {"data": {
                "publisher": {"name": "Мой удивительный Китай"},
                "publication": {"id": "6a982bffa6d9303399f80adb", "title": "Дынька ищет Арбузика", "content": {"type": "article", "articleContent": {"contentState": state.to_string()}}},
                "og": {"title": "Дынька ищет Арбузика и Тыковку"}
            }}}}))
        };
        let bear = |uuid: &str, id: &str, title: &str| json!({"type": "yandex-zen-video", "uuid": uuid, "publicationId": id, "title": title, "duration": 76000});
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://dzen.ru/a/apgr79izjnDkA_SX",
            200,
            "text/html",
            &article(vec![
                bear(
                    "oHkYKkKUPAAA",
                    "6a982b57a6d9303399f80031",
                    "Дынька заскучала",
                ),
                bear(
                    "oHkYKkKUPAAB",
                    "6a982b57a6d9303399f80032",
                    "Дынька потребовала вторую бутылочку",
                ),
                json!({"type": "yandex-music", "src": "https://music.yandex.ru/album/1"}),
            ]),
        ));
        fixture.exchanges.push(get(
            "https://dzen.ru/a/arQFATyuwHTvfskW",
            200,
            "text/html",
            &article(vec![
                json!({"type": "yandex-zen-video", "uuid": "o-EbDycoOAAA", "title": "Магниты"}),
            ]),
        ));
        fixture.exchanges.push(get(
            "https://dzen.ru/a/anNaXAum4S1iveuc",
            200,
            "text/html",
            &article(vec![json!({"type": "yandex-zen-publication", "originalUrl": "https://dzen.ru/a/akzrLbkSuTJ_vC8J"})]),
        ));
        let resolver = DzenResolver::new(Http::replay(fixture));
        let Resolution::Playlist(playlist) = resolver
            .resolve(&Url::parse("https://dzen.ru/a/apgr79izjnDkA_SX").unwrap())
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(playlist.title.as_deref(), Some("Дынька ищет Арбузика"));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[1].url.as_str(),
            "https://dzen.ru/video/watch/6a982b57a6d9303399f80032"
        );
        assert_eq!(playlist.entries[1].duration, Some(Duration::from_secs(76)));
        let error = resolver
            .resolve(&Url::parse("https://dzen.ru/a/arQFATyuwHTvfskW").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Redirect(to) if to.as_str() == "https://dzen.ru/embed/o-EbDycoOAAA"),
            "{error}"
        );
        let error = resolver
            .resolve(&Url::parse("https://dzen.ru/a/anNaXAum4S1iveuc").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("embeds no video")),
            "{error}"
        );
    }

    /// Every example link resolves live: the video, the short and the embed to media
    /// with playable streams, the channels and the article to entries.
    #[ignore = "reaches the live site: cargo test -- --ignored"]
    #[tokio::test]

    async fn live_examples_resolve() {
        let resolver = DzenResolver::new(Http::new(crate::http::HttpConfig::default()));
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
            assert!(resolver.matches(&url), "{link}");
            let began = std::time::Instant::now();
            let resolution = tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&url))
                .await
                .expect("resolution timed out")
                .unwrap_or_else(|e| panic!("{link}: {e}"));
            let took = began.elapsed();
            match resolution {
                Resolution::Media(resolved) => {
                    assert!(
                        resolved.variants.iter().any(|v| v.is_playable()),
                        "{link}: no playable variant"
                    );
                    assert!(resolved.title.is_some(), "{link}: no title");
                    println!(
                        "{link}: {} variants, {:?} in {took:?}",
                        resolved.variants.len(),
                        resolved.title
                    );
                }
                Resolution::Playlist(playlist) => {
                    assert!(!playlist.entries.is_empty(), "{link}: no entries");
                    println!(
                        "{link}: {} entries, {:?} in {took:?}",
                        playlist.entries.len(),
                        playlist.title
                    );
                }
            }
        }
    }
}
