//! Patreon: a post is read through the site's JSON API, which lists its own video as
//! an HLS stream or a file, its audio, its images with their originals, and the files
//! attached to it, and names the YouTube or Vimeo video an embed post plays, which is
//! handed on to that platform's resolver. A post with several pieces of media is a
//! playlist of them, each picked out by a fragment. A creator's page lists the
//! campaign's posts, newest first, and a collection lists the posts the creator put in
//! it. Public posts are read without an account; a stored `session_id` cookie reads the
//! posts of the memberships the account holds.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionCheck, SessionSupport, Tag, Variant, VariantKind, clean_title, hls, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "patreon";
const SITE: &str = "https://www.patreon.com";
const API: &str = "https://www.patreon.com/api/";
/// The app's user agent, which the API serves the higher renditions of a member's
/// videos to. Sent only with a session, since the API refuses it without one.
const APP_UA: &str = "Patreon/126.9.0.15 (Android; Android 14; Scale/2.10)";
/// The cookie a logged-in browser session carries.
const SESSION_COOKIE: &str = "session_id";
/// How many posts a creator's listing is read up to.
const LISTING_LIMIT: usize = 100;
const PAGE_SIZE: usize = 50;

/// `/posts/{slug}-{id}`, `/posts/{id}`, `/{creator}/posts/{slug}-{id}`.
static RE_POST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(?:[^/]+/)?posts/(?:[\w-]+-)?(\d+)/?$").unwrap());
/// `/collection/{id}`.
static RE_COLLECTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/collection/(\d+)/?$").unwrap());
/// `/m/{campaign id}` and `/api/campaigns/{campaign id}`, with or without `/posts`.
static RE_CAMPAIGN_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(?:m|api/campaigns)/(\d+)(?:/posts)?/?$").unwrap());
/// `/{vanity}`, `/c/{vanity}`, `/cw/{vanity}`, each with or without `/posts`.
static RE_VANITY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(?:cw?/)?([\w.-]+)(?:/posts)?/?$").unwrap());
/// `data-media-id="…"`: media inlined in a post's text.
static RE_INLINE_MEDIA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-media-id="(\d+)""#).unwrap());

/// Paths on the site that are pages of its own rather than a creator's.
const SITE_PAGES: &[&str] = &[
    "about",
    "api",
    "apps",
    "bePatron",
    "c",
    "checkout",
    "collection",
    "create",
    "creation",
    "cw",
    "explore",
    "home",
    "invite",
    "join",
    "login",
    "m",
    "messages",
    "notifications",
    "policy",
    "posts",
    "pricing",
    "product",
    "rss",
    "search",
    "settings",
    "signup",
    "user",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A post, and which of its pieces of media when it has several.
    Post {
        id: String,
        item: Option<usize>,
    },
    /// A creator's page, by the campaign's vanity name or its id.
    Creator(CreatorRef),
    Collection {
        id: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CreatorRef {
    Vanity(String),
    CampaignId(String),
}

/// Which piece of a post's media a link picks out, from an `#item-N` fragment.
fn item_index(url: &Url) -> Option<usize> {
    url.fragment()?
        .strip_prefix("item-")?
        .parse::<usize>()
        .ok()
        .filter(|n| *n >= 1)
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "patreon.com" && host != "www.patreon.com" {
        return None;
    }
    let path = url.path();
    if path.trim_end_matches('/') == "/creation" {
        let id = util::query_param(url, "hid")?;
        if !id.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        return Some(Link::Post {
            id,
            item: item_index(url),
        });
    }
    if let Some(caps) = RE_POST.captures(path) {
        return Some(Link::Post {
            id: caps[1].to_string(),
            item: item_index(url),
        });
    }
    if let Some(caps) = RE_COLLECTION.captures(path) {
        return Some(Link::Collection {
            id: caps[1].to_string(),
        });
    }
    if let Some(caps) = RE_CAMPAIGN_ID.captures(path) {
        return Some(Link::Creator(CreatorRef::CampaignId(caps[1].to_string())));
    }
    let caps = RE_VANITY.captures(path)?;
    let vanity = caps[1].to_string();
    if SITE_PAGES.contains(&vanity.as_str()) {
        return None;
    }
    Some(Link::Creator(CreatorRef::Vanity(vanity)))
}

/// What a file is and the format it is in, from the type the API names, else from the
/// file's name, else from the broad type.
pub fn classify(name: &str, mimetype: &str) -> (MediaKind, Option<Container>) {
    if let Some(container) = Container::from_mime(mimetype) {
        return (container.kind(), Some(container));
    }
    if let Some(container) = Container::from_name(name) {
        return (container.kind(), Some(container));
    }
    (MediaKind::from_mime(mimetype), None)
}

fn audio_codec(container: &Container) -> Option<AudioCodec> {
    Some(match container {
        Container::Mp3 => AudioCodec::Mp3,
        Container::M4a => AudioCodec::Aac,
        Container::Ogg => AudioCodec::Vorbis,
        Container::Opus => AudioCodec::Opus,
        Container::Flac => AudioCodec::Flac,
        Container::Wav => AudioCodec::Other("pcm".into()),
        _ => return None,
    })
}

/// A variant for a file of any kind.
fn file_variant(
    url: Url,
    kind: MediaKind,
    container: Option<Container>,
    size: Option<u64>,
) -> Variant {
    let mut v = Variant::new(url, VariantKind::File);
    match kind {
        MediaKind::Audio => {
            v.audio_only = true;
            v.audio = container.as_ref().and_then(audio_codec);
        }
        MediaKind::Video if container == Some(Container::Mp4) => {
            v.video = Some(VideoCodec::H264);
            v.audio = Some(AudioCodec::Aac);
        }
        _ => {}
    }
    v.container = container;
    v.size = size;
    v
}

/// The name of a file without its extension.
fn stem(name: &str) -> &str {
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

/// One piece of a post's media.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    /// The post's own video as an HLS stream.
    Stream {
        url: Url,
        name: Option<String>,
        duration: Option<Duration>,
    },
    /// A file the post carries: its video, audio, an image, or an attachment.
    File {
        url: Url,
        name: Option<String>,
        kind: MediaKind,
        container: Option<Container>,
        size: Option<u64>,
        width: Option<u32>,
        height: Option<u32>,
        duration: Option<Duration>,
    },
    /// A video on another platform the post embeds.
    Embed { url: Url, name: Option<String> },
}

impl Item {
    fn name(&self) -> Option<&str> {
        match self {
            Item::Stream { name, .. } | Item::File { name, .. } | Item::Embed { name, .. } => {
                name.as_deref()
            }
        }
    }

    fn duration(&self) -> Option<Duration> {
        match self {
            Item::Stream { duration, .. } | Item::File { duration, .. } => *duration,
            Item::Embed { .. } => None,
        }
    }
}

/// The included record of `kind` with `id`.
fn included<'a>(answer: &'a Value, kind: &str, id: &str) -> Option<&'a Value> {
    answer["included"]
        .as_array()?
        .iter()
        .find(|item| item["type"].as_str() == Some(kind) && item["id"].as_str() == Some(id))
}

/// The ids a relationship of the post names, in the API's order.
fn relation_ids(post: &Value, relation: &str) -> Vec<String> {
    match &post["relationships"][relation]["data"] {
        Value::Array(items) => items
            .iter()
            .filter_map(|item| item["id"].as_str().map(String::from))
            .collect(),
        Value::Object(item) => item
            .get("id")
            .and_then(Value::as_str)
            .map(|id| vec![id.to_string()])
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// A file item from a media record's attributes and the link to fetch it by.
fn media_file(url: Url, media: &Value) -> Item {
    let name = util::text(&media["file_name"]);
    let mimetype = media["mimetype"].as_str().unwrap_or("");
    let (kind, container) = classify(name.as_deref().unwrap_or(""), mimetype);
    let kind = match media["media_type"].as_str() {
        Some("video") => MediaKind::Video,
        Some("audio") => MediaKind::Audio,
        Some("image") => MediaKind::Image,
        _ => kind,
    };
    Item::File {
        url,
        name,
        kind,
        container,
        size: util::uint(&media["size_bytes"]),
        width: util::u32_of(&media["metadata"]["dimensions"]["w"])
            .or_else(|| util::u32_of(&media["display"]["width"])),
        height: util::u32_of(&media["metadata"]["dimensions"]["h"])
            .or_else(|| util::u32_of(&media["display"]["height"])),
        duration: util::seconds(&media["display"]["duration"]),
    }
}

/// Whether a link is an HLS playlist: a `.m3u8` path, or a Mux stream link.
fn is_playlist(url: &Url) -> bool {
    url.path().ends_with(".m3u8")
        || url
            .query()
            .is_some_and(|q| q.contains("token=") && url.path().contains(".m3u8"))
}

/// The pieces of media a post carries, in the order the post shows them: its own video
/// or audio, the video it embeds, its images (the cover of a video or audio post is not
/// one), then its attachments. `inline` are the media inlined in its text and not among
/// those, for the caller to look up.
pub fn items_of(answer: &Value) -> (Vec<Item>, Vec<String>) {
    let post = &answer["data"];
    let attributes = &post["attributes"];
    let mut items = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let image_ids = relation_ids(post, "images");
    let audio_ids = relation_ids(post, "audio");
    let post_file = &attributes["post_file"];
    let post_file_media = util::uint(&post_file["media_id"]).map(|id| id.to_string());
    // A video or audio post's images are its cover, not its content.
    let mut own_media = false;
    if let Some(url) = util::url_of(&post_file["url"], None) {
        let name = util::text(&post_file["name"]);
        let duration = util::seconds(&post_file["duration"]);
        let is_image = post_file_media
            .as_ref()
            .is_some_and(|id| image_ids.contains(id));
        if is_playlist(&url) || name.as_deref() == Some("video") {
            own_media = true;
            items.push(Item::Stream {
                url,
                name: None,
                duration,
            });
        } else if !is_image {
            own_media = true;
            let audio = audio_ids
                .first()
                .and_then(|id| included(answer, "media", id));
            let file_name = audio
                .and_then(|media| util::text(&media["attributes"]["file_name"]))
                .or(name);
            let (kind, container) = classify(
                file_name.as_deref().unwrap_or(url.path()),
                audio
                    .and_then(|media| media["attributes"]["mimetype"].as_str())
                    .unwrap_or(""),
            );
            let kind = if audio.is_some() && kind == MediaKind::File {
                MediaKind::Audio
            } else {
                kind
            };
            items.push(Item::File {
                url,
                name: file_name,
                kind,
                container,
                size: audio.and_then(|media| util::uint(&media["attributes"]["size_bytes"])),
                width: util::u32_of(&post_file["width"]),
                height: util::u32_of(&post_file["height"]),
                duration,
            });
            seen.extend(audio_ids.iter().cloned());
        }
        if let Some(id) = &post_file_media {
            seen.push(id.clone());
        }
    }
    if let Some(url) = util::url_of(&attributes["embed"]["url"], None) {
        items.push(Item::Embed {
            url,
            name: attributes["embed"]["subject"]
                .as_str()
                .and_then(clean_title)
                .or_else(|| {
                    attributes["embed"]["provider"]
                        .as_str()
                        .and_then(clean_title)
                }),
        });
    }
    let mut ordered = attributes["post_metadata"]["image_order"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(util::text)
        .filter(|id| image_ids.contains(id))
        .collect::<Vec<_>>();
    for id in &image_ids {
        if !ordered.contains(id) {
            ordered.push(id.clone());
        }
    }
    if own_media {
        seen.append(&mut ordered);
    }
    for id in ordered {
        if let Some(media) = included(answer, "media", &id)
            && let Some(url) = util::url_of(&media["attributes"]["download_url"], None)
                .or_else(|| util::url_of(&media["attributes"]["image_urls"]["original"], None))
        {
            items.push(media_file(url, &media["attributes"]));
        }
        seen.push(id);
    }
    for id in audio_ids {
        if seen.contains(&id) {
            continue;
        }
        if let Some(media) = included(answer, "media", &id)
            && let Some(url) = util::url_of(&media["attributes"]["download_url"], None)
        {
            items.push(media_file(url, &media["attributes"]));
        }
        seen.push(id);
    }
    for id in relation_ids(post, "attachments_media") {
        if let Some(media) = included(answer, "media", &id)
            && let Some(url) = util::url_of(&media["attributes"]["download_url"], None)
        {
            items.push(media_file(url, &media["attributes"]));
        }
        seen.push(id);
    }
    let inline = RE_INLINE_MEDIA
        .captures_iter(attributes["content"].as_str().unwrap_or(""))
        .map(|caps| caps[1].to_string())
        .filter(|id| !seen.contains(id))
        .fold(Vec::new(), |mut ids: Vec<String>, id| {
            if !ids.contains(&id) {
                ids.push(id);
            }
            ids
        });
    (items, inline)
}

/// The item a media record makes on its own: the stream it plays, or the file it is.
pub fn media_item(answer: &Value) -> Option<Item> {
    let attributes = &answer["data"]["attributes"];
    let playback = util::url_of(&attributes["display"]["url"], None)
        .or_else(|| util::url_of(&attributes["display"]["viewer_playback_data"]["url"], None));
    let download = util::url_of(&attributes["download_url"], None);
    let mimetype = attributes["mimetype"].as_str().unwrap_or("");
    if let Some(url) = &playback
        && (mimetype.contains("mpegurl") || is_playlist(url))
    {
        return Some(Item::Stream {
            url: url.clone(),
            name: util::text(&attributes["file_name"]),
            duration: util::seconds(&attributes["display"]["duration"]),
        });
    }
    let url = playback.or(download)?;
    Some(media_file(url, attributes))
}

pub struct PatreonResolver {
    http: Http,
}

impl PatreonResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    fn has_session(&self) -> bool {
        self.http.jar(PLATFORM).get(SESSION_COOKIE).is_some()
    }

    /// A JSON answer from the API, as the browser without a session and as the app
    /// with one.
    async fn api(
        &self,
        path: &str,
        query: &[(&str, &str)],
        origin: &Url,
    ) -> Result<Value, ResolveError> {
        let mut pairs: Vec<(&str, &str)> = query.to_vec();
        pairs.push(("json-api-version", "1.0"));
        let api = util::with_query(&Url::parse(&format!("{API}{path}")).expect("valid"), &pairs);
        let request = self
            .http
            .get(api.clone())
            .platform(PLATFORM)
            .header("accept", "application/json");
        let request = if self.has_session() {
            request.user_agent(APP_UA)
        } else {
            request.impersonate()
        };
        let response = request.send().await?;
        let status = response.status.as_u16();
        let body = response.bytes_up_to(MAX_PAGE).await?.0;
        let answer: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let detail = answer["errors"][0]["detail"]
            .as_str()
            .or_else(|| answer["errors"][0]["title"].as_str())
            .map(str::to_string);
        match status {
            200..=299 => Ok(answer),
            404 => Err(ResolveError::NotFound(origin.clone())),
            429 => Err(ResolveError::RateLimited(origin.clone())),
            401 | 403 if !self.has_session() => Err(ResolveError::login_required(
                origin,
                PLATFORM,
                detail.unwrap_or_else(|| "the API asks for a logged-in session".to_string()),
            )),
            _ => Err(ResolveError::unavailable(
                origin,
                detail.unwrap_or_else(|| format!("the API answered HTTP {status}")),
            )),
        }
    }

    async fn post(&self, id: &str, origin: &Url) -> Result<Value, ResolveError> {
        self.api(
            &format!("posts/{id}"),
            &[
                ("fields[post]", "title,content,post_type,embed,image,post_file,published_at,current_user_can_view,post_metadata,url,patreon_url,teaser_text"),
                ("fields[media]", "download_url,image_urls,mimetype,size_bytes,file_name,media_type,display,metadata"),
                ("fields[user]", "full_name,url"),
                ("fields[campaign]", "url,name,is_nsfw"),
                ("include", "images,audio,attachments_media,media,user,campaign"),
                ("json-api-use-default-includes", "false"),
            ],
            origin,
        )
        .await
    }

    /// The variants an item plays as.
    async fn variants_of(
        &self,
        item: &Item,
        origin: &Url,
    ) -> Result<(Vec<Variant>, Vec<super::SubtitleTrack>), ResolveError> {
        match item {
            Item::Stream { url, .. } => {
                let headers = vec![("referer".to_string(), format!("{SITE}/"))];
                let expanded = hls::expand(&self.http, url, PLATFORM, BROWSER_UA, &headers)
                    .await
                    .map_err(|e| e.at(origin))?;
                Ok((expanded.variants, expanded.subtitles))
            }
            Item::File {
                url,
                name,
                kind,
                container,
                size,
                width,
                height,
                duration,
            } => {
                let mut variant = file_variant(url.clone(), *kind, container.clone(), *size);
                variant.width = *width;
                variant.height = *height;
                variant.duration = *duration;
                variant.label = name.clone();
                Ok((vec![variant], Vec::new()))
            }
            Item::Embed { url, .. } => Err(ResolveError::Redirect(url.clone())),
        }
    }

    async fn resolve_post(
        &self,
        id: &str,
        item: Option<usize>,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let answer = self.post(id, url).await?;
        let post = &answer["data"];
        let attributes = &post["attributes"];
        if post["id"].is_null() {
            return Err(ResolveError::malformed(
                url,
                "the API answered without a post",
            ));
        }
        let (mut items, inline) = items_of(&answer);
        for media_id in inline {
            let media = self
                .api(
                    &format!("media/{media_id}"),
                    &[("json-api-use-default-includes", "false")],
                    url,
                )
                .await?;
            if let Some(item) = media_item(&media) {
                items.push(item);
            }
        }
        if items.is_empty() {
            return Err(match attributes["current_user_can_view"].as_bool() {
                Some(false) if !self.has_session() => ResolveError::login_required(
                    url,
                    PLATFORM,
                    "the post is for patrons; a member's session_id cookie reads it",
                ),
                Some(false) => ResolveError::unavailable(
                    url,
                    "the post is locked for the account's membership",
                ),
                _ => ResolveError::unavailable(url, "the post carries no media"),
            });
        }
        let user = relation_ids(post, "user")
            .first()
            .and_then(|id| included(&answer, "user", id));
        let campaign = relation_ids(post, "campaign")
            .first()
            .and_then(|id| included(&answer, "campaign", id));
        let webpage = attributes["patreon_url"]
            .as_str()
            .and_then(|path| Url::parse(SITE).ok()?.join(path).ok())
            .or_else(|| util::url_of(&attributes["url"], None))
            .unwrap_or_else(|| Url::parse(&format!("{SITE}/posts/{id}")).expect("valid"));
        let title = attributes["title"].as_str().and_then(clean_title);
        let mut base = Resolved::new(PLATFORM);
        base.id = Some(id.to_string());
        base.title = title.clone();
        base.description = attributes["content"]
            .as_str()
            .map(util::clean_html)
            .and_then(|t| clean_title(&t))
            .or_else(|| attributes["teaser_text"].as_str().and_then(clean_title));
        base.uploader = user
            .and_then(|u| u["attributes"]["full_name"].as_str())
            .or_else(|| campaign.and_then(|c| c["attributes"]["name"].as_str()))
            .and_then(clean_title);
        base.uploader_url = campaign
            .and_then(|c| util::url_of(&c["attributes"]["url"], None))
            .or_else(|| user.and_then(|u| util::url_of(&u["attributes"]["url"], None)));
        base.uploaded_at = attributes["published_at"]
            .as_str()
            .and_then(|t| t.parse::<Timestamp>().ok());
        base.thumbnail = util::url_of(&attributes["image"]["large_url"], None)
            .or_else(|| util::url_of(&attributes["image"]["url"], None));
        base.webpage_url = Some(webpage.clone());
        base.age_limit = campaign
            .and_then(|c| c["attributes"]["is_nsfw"].as_bool())
            .filter(|nsfw| *nsfw)
            .map(|_| 18);
        if items.len() > 1 && item.is_none() {
            let entries = items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let mut entry_url = match item {
                        Item::Embed { url, .. } => url.clone(),
                        _ => webpage.clone(),
                    };
                    if !matches!(item, Item::Embed { .. }) {
                        entry_url.set_fragment(Some(&format!("item-{}", index + 1)));
                    }
                    PlaylistEntry {
                        url: entry_url,
                        title: item
                            .name()
                            .and_then(clean_title)
                            .or_else(|| title.as_ref().map(|t| format!("{t} ({})", index + 1))),
                        duration: item.duration(),
                    }
                })
                .collect::<Vec<_>>();
            return Ok(Resolution::Playlist(Playlist {
                resolver: PLATFORM.into(),
                id: base.id,
                title: base.title,
                total: Some(entries.len()),
                entries,
            }));
        }
        let index = item.unwrap_or(1);
        let picked = items
            .get(index - 1)
            .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let (variants, subtitles) = self.variants_of(picked, url).await?;
        let mut resolved = base;
        if items.len() > 1 {
            resolved.id = Some(format!("{id}-{index}"));
            if let Some(name) = picked.name().and_then(clean_title) {
                resolved.title = Some(name);
            }
        } else if resolved.title.is_none() {
            resolved.title = picked.name().map(stem).and_then(clean_title);
        }
        if let Item::File { kind, .. } = picked {
            resolved.media = *kind;
        }
        resolved.duration = picked
            .duration()
            .or_else(|| variants.iter().find_map(|v| v.duration));
        resolved.subtitles = subtitles;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    /// The campaign id a vanity name belongs to, through the site's search.
    async fn campaign_id(&self, vanity: &str, origin: &Url) -> Result<String, ResolveError> {
        let answer = self
            .api("search", &[("q", vanity), ("page[size]", "5")], origin)
            .await?;
        let wanted = format!("/{}", vanity.to_ascii_lowercase());
        answer["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|hit| hit["type"].as_str() == Some("campaign-document"))
            .find(|hit| {
                hit["attributes"]["url"]
                    .as_str()
                    .is_some_and(|u| u.to_ascii_lowercase().ends_with(&wanted))
            })
            .and_then(|hit| hit["id"].as_str())
            .and_then(|id| id.strip_prefix("campaign_"))
            .map(String::from)
            .ok_or_else(|| ResolveError::NotFound(origin.clone()))
    }

    async fn resolve_creator(
        &self,
        creator: &CreatorRef,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let campaign_id = match creator {
            CreatorRef::Vanity(vanity) => self.campaign_id(vanity, url).await?,
            CreatorRef::CampaignId(id) => id.clone(),
        };
        let campaign = self
            .api(
                &format!("campaigns/{campaign_id}"),
                &[
                    ("include", "creator"),
                    (
                        "fields[campaign]",
                        "name,summary,url,creation_count,is_nsfw,vanity",
                    ),
                    ("fields[user]", "full_name,url"),
                    ("json-api-use-default-includes", "false"),
                ],
                url,
            )
            .await?;
        let name = campaign["data"]["attributes"]["name"]
            .as_str()
            .and_then(clean_title);
        let mut entries = Vec::new();
        let mut total = None;
        let mut cursor: Option<String> = None;
        let page_size = PAGE_SIZE.to_string();
        loop {
            let mut query = vec![
                ("filter[campaign_id]", campaign_id.as_str()),
                ("filter[is_draft]", "false"),
                ("sort", "-published_at"),
                (
                    "fields[post]",
                    "title,patreon_url,url,published_at,current_user_can_view,post_type",
                ),
                ("json-api-use-default-includes", "false"),
                ("page[size]", page_size.as_str()),
            ];
            if let Some(cursor) = &cursor {
                query.push(("page[cursor]", cursor.as_str()));
            }
            let page = self.api("posts", &query, url).await?;
            total = total
                .or_else(|| util::uint(&page["meta"]["pagination"]["total"]).map(|n| n as usize));
            let posts = page["data"].as_array().cloned().unwrap_or_default();
            if posts.is_empty() {
                break;
            }
            for post in &posts {
                let attributes = &post["attributes"];
                if matches!(
                    attributes["post_type"].as_str(),
                    Some("text_only") | Some("poll")
                ) {
                    continue;
                }
                let Some(entry_url) = attributes["patreon_url"]
                    .as_str()
                    .and_then(|path| Url::parse(SITE).ok()?.join(path).ok())
                    .or_else(|| util::url_of(&attributes["url"], None))
                else {
                    continue;
                };
                entries.push(PlaylistEntry {
                    url: entry_url,
                    title: attributes["title"].as_str().and_then(clean_title),
                    duration: None,
                });
            }
            cursor = page["meta"]["pagination"]["cursors"]["next"]
                .as_str()
                .map(String::from);
            if cursor.is_none() || entries.len() >= LISTING_LIMIT {
                break;
            }
        }
        entries.truncate(LISTING_LIMIT);
        if entries.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.into(),
            id: Some(campaign_id),
            title: name,
            total: total.or(Some(entries.len())),
            entries,
        }))
    }

    async fn resolve_collection(&self, id: &str, url: &Url) -> Result<Resolution, ResolveError> {
        let answer = self
            .api(
                &format!("collection/{id}"),
                &[
                    ("include", "posts"),
                    (
                        "fields[post]",
                        "title,patreon_url,url,published_at,current_user_can_view,post_type",
                    ),
                    ("fields[collection]", "title,description,num_posts"),
                    ("json-api-use-default-includes", "false"),
                ],
                url,
            )
            .await?;
        let collection = &answer["data"];
        let entries = relation_ids(collection, "posts")
            .iter()
            .filter_map(|post_id| {
                let post = included(&answer, "post", post_id)?;
                let attributes = &post["attributes"];
                let entry_url = attributes["patreon_url"]
                    .as_str()
                    .and_then(|path| Url::parse(SITE).ok()?.join(path).ok())
                    .or_else(|| util::url_of(&attributes["url"], None))?;
                Some(PlaylistEntry {
                    url: entry_url,
                    title: attributes["title"].as_str().and_then(clean_title),
                    duration: None,
                })
            })
            .collect::<Vec<_>>();
        if entries.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.into(),
            id: Some(id.to_string()),
            title: collection["attributes"]["title"]
                .as_str()
                .and_then(clean_title),
            total: util::uint(&collection["attributes"]["num_posts"])
                .map(|n| n as usize)
                .or(Some(entries.len())),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for PatreonResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Patreon",
            hosts: &["patreon.com"],
            features: &[
                "posts",
                "video",
                "audio",
                "images",
                "attachments",
                "embeds",
                "creators",
                "collections",
            ],
            formats: &["hls", "mp4", "mp3", "jpg", "png", "any file"],
            media: &[
                MediaKind::Video,
                MediaKind::Audio,
                MediaKind::Image,
                MediaKind::File,
            ],
            tags: &[Tag::Video, Tag::Podcasts, Tag::Images],
            session: SessionSupport::Optional,
            examples: &[
                "https://www.patreon.com/posts/video-sketchbook-32452882",
                "https://www.patreon.com/posts/1073-unsafe-and-170223332",
                "https://www.patreon.com/posts/im-going-to-next-169244631",
                "https://www.patreon.com/loish",
                "https://www.patreon.com/c/loish/posts",
                "https://www.patreon.com/m/1641751/posts",
                "https://www.patreon.com/collection/96265",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Post { id, item } => self.resolve_post(&id, item, url).await,
            Link::Creator(creator) => self.resolve_creator(&creator, url).await,
            Link::Collection { id } => self.resolve_collection(&id, url).await,
        }
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        if !self.has_session() {
            return Ok(SessionCheck::LoggedOut);
        }
        let origin = Url::parse(SITE).expect("valid");
        match self
            .api(
                "current_user",
                &[
                    ("fields[user]", "full_name,vanity,url"),
                    ("json-api-use-default-includes", "false"),
                ],
                &origin,
            )
            .await
        {
            Ok(answer) => {
                let attributes = &answer["data"]["attributes"];
                Ok(
                    match attributes["full_name"]
                        .as_str()
                        .and_then(clean_title)
                        .or_else(|| attributes["vanity"].as_str().and_then(clean_title))
                    {
                        Some(account) => SessionCheck::LoggedIn { account },
                        None => SessionCheck::LoggedOut,
                    },
                )
            }
            Err(ResolveError::LoginRequired { .. })
            | Err(ResolveError::Unavailable { .. })
            | Err(ResolveError::NotFound(_)) => Ok(SessionCheck::LoggedOut),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Cookie;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };
    use serde_json::json;

    fn get(url: &str, status: u16, body: String) -> Exchange {
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
                headers: vec![("content-type".into(), "application/vnd.api+json".into())],
                body: RecordedBody::Text(body),
                truncated: false,
            },
        }
    }

    fn text(url: &str, content_type: &str, body: &str) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: "GET".into(),
                url: url.into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status: 200,
                url: url.into(),
                headers: vec![("content-type".into(), content_type.into())],
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    fn post_api(id: &str) -> String {
        format!("https://www.patreon.com/api/posts/{id}")
    }

    fn loish() -> Vec<Value> {
        vec![
            json!({"type": "campaign", "id": "1641751", "attributes": {"is_nsfw": false, "name": "Loish", "url": "https://www.patreon.com/loish"}}),
            json!({"type": "user", "id": "4301314", "attributes": {"full_name": "Loish", "url": "https://www.patreon.com/loish"}}),
        ]
    }

    fn relationships(images: Vec<&str>, audio: Option<&str>, attachments: Vec<&str>) -> Value {
        json!({
            "images": {"data": images.iter().map(|id| json!({"id": id, "type": "media"})).collect::<Vec<_>>()},
            "audio": {"data": audio.map(|id| json!({"id": id, "type": "media"}))},
            "attachments_media": {"data": attachments.iter().map(|id| json!({"id": id, "type": "media"})).collect::<Vec<_>>()},
            "campaign": {"data": {"id": "1641751", "type": "campaign"}},
            "user": {"data": {"id": "4301314", "type": "user"}}
        })
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let post = |id: &str, item: Option<usize>| {
            Some(Link::Post {
                id: id.into(),
                item,
            })
        };
        assert_eq!(
            link("https://www.patreon.com/posts/video-sketchbook-32452882"),
            post("32452882", None)
        );
        assert_eq!(
            link("https://patreon.com/posts/32452882#item-2"),
            post("32452882", Some(2))
        );
        assert_eq!(
            link("https://www.patreon.com/Insanimate/posts/meatcanyon-in-142663524"),
            post("142663524", None)
        );
        assert_eq!(
            link("http://www.patreon.com/creation?hid=743933"),
            post("743933", None)
        );
        assert_eq!(
            link("https://www.patreon.com/loish"),
            Some(Link::Creator(CreatorRef::Vanity("loish".into())))
        );
        assert_eq!(
            link("https://www.patreon.com/c/loish/posts"),
            Some(Link::Creator(CreatorRef::Vanity("loish".into())))
        );
        assert_eq!(
            link("https://www.patreon.com/cw/anythingelse"),
            Some(Link::Creator(CreatorRef::Vanity("anythingelse".into())))
        );
        assert_eq!(
            link("https://www.patreon.com/m/4767637/posts"),
            Some(Link::Creator(CreatorRef::CampaignId("4767637".into())))
        );
        assert_eq!(
            link("https://www.patreon.com/api/campaigns/4243769"),
            Some(Link::Creator(CreatorRef::CampaignId("4243769".into())))
        );
        assert_eq!(
            link("https://www.patreon.com/collection/96265"),
            Some(Link::Collection { id: "96265".into() })
        );
        assert_eq!(link("https://www.patreon.com/"), None);
        assert_eq!(link("https://www.patreon.com/login"), None);
        assert_eq!(link("https://www.patreon.com/posts/"), None);
        assert_eq!(link("https://www.patreon.com/search?q=x"), None);
        assert_eq!(link("https://www.patreon.com/settings/apps"), None);
        assert_eq!(link("https://example.com/posts/1"), None);
    }

    #[tokio::test]
    async fn a_native_video_post_resolves_through_its_stream() {
        let mut fixture = Fixture::new(PLATFORM, None);
        let mut included = loish();
        included.push(json!({"type": "post_tag", "id": "user_defined;video", "attributes": {"value": "video"}}));
        fixture.exchanges.push(get(&post_api("32452882"), 200, json!({
            "data": {"id": "32452882", "type": "post", "attributes": {
                "title": "VIDEO // sketchbook flipthrough", "content": "<p>Here\u{2019}s a little <b>flipthrough</b> video</p>",
                "post_type": "video_external_file", "embed": null, "current_user_can_view": true, "published_at": "2019-12-18T19:22:42.000+00:00",
                "image": {"url": "https://c10.patreonusercontent.com/4/post/32452882/4.jpg", "large_url": "https://c10.patreonusercontent.com/4/post/32452882/large.jpg"},
                "post_file": {"duration": 163.16, "url": "https://stream.mux.com/00Rc.m3u8?token=t", "media_id": 555},
                "patreon_url": "/posts/video-sketchbook-32452882", "post_metadata": {"image_order": []}},
                "relationships": relationships(vec![], None, vec![])},
            "included": included
        }).to_string()));
        fixture.exchanges.push(text(
            "https://stream.mux.com/00Rc.m3u8?token=t",
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2352900,CODECS=\"avc1.640020,mp4a.40.2\",RESOLUTION=1280x720\nhttps://manifest.mux.com/720/rendition.m3u8?cdn=fastly\n",
        ));
        fixture.exchanges.push(text(
            "https://manifest.mux.com/720/rendition.m3u8?cdn=fastly",
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\n0.ts\n#EXT-X-ENDLIST\n",
        ));
        let resolver = PatreonResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.patreon.com/posts/video-sketchbook-32452882").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("32452882"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("VIDEO // sketchbook flipthrough")
        );
        assert_eq!(
            resolved.description.as_deref(),
            Some("Here\u{2019}s a little flipthrough video")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("Loish"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://www.patreon.com/loish"
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(163.16)));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://c10.patreonusercontent.com/4/post/32452882/large.jpg"
        );
        assert_eq!(
            resolved.webpage_url.as_ref().unwrap().as_str(),
            "https://www.patreon.com/posts/video-sketchbook-32452882"
        );
        assert_eq!(resolved.media, MediaKind::Video);
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].kind, VariantKind::Hls);
        assert_eq!(resolved.variants[0].height, Some(720));
        assert!(
            resolved.variants[0]
                .headers
                .iter()
                .any(|(k, v)| k == "referer" && v == "https://www.patreon.com/")
        );
    }

    #[tokio::test]
    async fn audio_images_attachments_and_embeds_come_back_as_what_they_are() {
        let mut fixture = Fixture::new(PLATFORM, None);
        let mut with_audio = loish();
        with_audio.push(json!({"type": "media", "id": "749814443", "attributes": {"file_name": "chapo_mixdown.mp3", "mimetype": "audio/mpeg", "size_bytes": 66321771, "media_type": "audio", "download_url": "https://c10.patreonusercontent.com/4/post/170223332/1.mp3?token-hash=d"}}));
        fixture.exchanges.push(get(&post_api("170223332"), 200, json!({
            "data": {"id": "170223332", "type": "post", "attributes": {
                "title": "1073 - Unsafe, Illegal, and Common", "content": null, "teaser_text": "A show.", "post_type": "podcast", "embed": null, "current_user_can_view": true,
                "published_at": "2026-09-22T00:00:00.000+00:00", "image": {"url": "https://c10.patreonusercontent.com/4/post/170223332/1.jpg"},
                "post_file": {"duration": 4449.0, "url": "https://c10.patreonusercontent.com/4/post/170223332/1.mp3?token-hash=p", "media_id": 749814443},
                "patreon_url": "/chapotraphouse/posts/1073-unsafe-and-170223332", "post_metadata": {}},
                "relationships": relationships(vec![], Some("749814443"), vec![])},
            "included": with_audio
        }).to_string()));
        let mut with_images = loish();
        with_images.push(json!({"type": "media", "id": "743115996", "attributes": {"file_name": "playgrounds copy.png", "mimetype": "image/png", "size_bytes": 2505175, "media_type": "image",
            "download_url": "https://c10.patreonusercontent.com/4/post/169244631/1.png?token-hash=d", "image_urls": {"original": "https://c10.patreonusercontent.com/4/post/169244631/1.png?token-hash=o"}, "metadata": {"dimensions": {"h": 969, "w": 1658}}}}));
        with_images.push(json!({"type": "media", "id": "743115997", "attributes": {"file_name": "second.jpg", "mimetype": "image/jpeg", "size_bytes": 1000, "media_type": "image",
            "download_url": "https://c10.patreonusercontent.com/4/post/169244631/2.jpg?token-hash=d", "metadata": {"dimensions": {"h": 100, "w": 200}}}}));
        with_images.push(json!({"type": "media", "id": "800", "attributes": {"file_name": "brushes.zip", "mimetype": "application/zip", "size_bytes": 4096, "download_url": "https://c10.patreonusercontent.com/4/post/169244631/brushes.zip?token-hash=d"}}));
        let image_post = json!({
            "data": {"id": "169244631", "type": "post", "attributes": {
                "title": "I'm going to London next week!", "content": "<p>See you there <img data-media-id=\"743115996\" src=\"x\"></p>", "post_type": "image_file", "embed": null, "current_user_can_view": true,
                "published_at": "2026-09-10T12:00:00.000+00:00", "image": {"url": "https://c10.patreonusercontent.com/4/post/169244631/1.png?token-hash=t"},
                "post_file": {"url": "https://c10.patreonusercontent.com/4/post/169244631/1.png?token-hash=p", "width": 1658, "height": 969, "media_id": 743115996},
                "patreon_url": "/loish/posts/im-going-to-next-169244631", "post_metadata": {"image_order": ["743115997", "743115996"]}},
                "relationships": relationships(vec!["743115996", "743115997"], None, vec!["800"])},
            "included": with_images
        }).to_string();
        fixture
            .exchanges
            .push(get(&post_api("169244631"), 200, image_post.clone()));
        fixture
            .exchanges
            .push(get(&post_api("169244631"), 200, image_post.clone()));
        fixture
            .exchanges
            .push(get(&post_api("169244631"), 200, image_post));
        let mut with_embed_image = loish();
        with_embed_image.push(json!({"type": "media", "id": "26577127", "attributes": {"file_name": "Untitled", "mimetype": "image/jpeg", "size_bytes": 33625, "download_url": "https://c10.patreonusercontent.com/4/post/1682498/1.jpg?token-hash=d"}}));
        fixture.exchanges.push(get(&post_api("1682498"), 200, json!({
            "data": {"id": "1682498", "type": "post", "attributes": {
                "title": "I'm on Patreon!", "content": "<p>Visit</p>", "post_type": "link", "current_user_can_view": true, "published_at": "2015-02-11T21:21:06.000+00:00",
                "embed": {"url": "https://www.youtube.com/watch?v=SU4fj_aEMVw", "provider": "www.youtube.com", "subject": "I'm on Patreon!"},
                "image": {"url": "https://c10.patreonusercontent.com/4/post/1682498/1.jpg"},
                "post_file": {"url": "https://c10.patreonusercontent.com/4/post/1682498/1.jpg?token-hash=p", "width": 640, "height": 480, "media_id": 26577127},
                "patreon_url": "/posts/im-on-patreon-1682498", "post_metadata": {}},
                "relationships": relationships(vec!["26577127"], None, vec![])},
            "included": with_embed_image
        }).to_string()));
        let resolver = PatreonResolver::new(Http::replay(fixture));
        let resolve = |link: &str| {
            let url = Url::parse(link).unwrap();
            let resolver = &resolver;
            async move { resolver.resolve(&url).await }
        };
        let audio = resolve("https://www.patreon.com/posts/1073-unsafe-and-170223332")
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(audio.media, MediaKind::Audio);
        assert_eq!(audio.description.as_deref(), Some("A show."));
        assert_eq!(audio.duration, Some(Duration::from_secs(4449)));
        assert_eq!(audio.variants.len(), 1);
        let track = &audio.variants[0];
        assert!(track.audio_only);
        assert_eq!(track.container, Some(Container::Mp3));
        assert_eq!(track.audio, Some(AudioCodec::Mp3));
        assert_eq!(track.size, Some(66321771));
        assert_eq!(track.label.as_deref(), Some("chapo_mixdown.mp3"));
        assert!(track.url.as_str().ends_with("1.mp3?token-hash=p"));

        let Resolution::Playlist(pieces) =
            resolve("https://www.patreon.com/posts/im-going-to-next-169244631")
                .await
                .unwrap()
        else {
            panic!("several pieces of media make a playlist");
        };
        assert_eq!(pieces.entries.len(), 3, "two images and an attachment");
        assert_eq!(
            pieces.entries[0].url.as_str(),
            "https://www.patreon.com/loish/posts/im-going-to-next-169244631#item-1"
        );
        assert_eq!(pieces.entries[0].title.as_deref(), Some("second.jpg"));
        assert_eq!(pieces.entries[2].title.as_deref(), Some("brushes.zip"));
        let image = resolve("https://www.patreon.com/posts/169244631#item-2")
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(image.media, MediaKind::Image);
        assert_eq!(image.id.as_deref(), Some("169244631-2"));
        assert_eq!(image.title.as_deref(), Some("playgrounds copy.png"));
        assert_eq!(image.variants[0].container, Some(Container::Png));
        assert_eq!(image.variants[0].size, Some(2505175));
        assert_eq!(
            (image.variants[0].width, image.variants[0].height),
            (Some(1658), Some(969))
        );
        assert!(
            image.variants[0]
                .url
                .as_str()
                .ends_with("1.png?token-hash=d")
        );
        let attachment = resolve("https://www.patreon.com/posts/169244631#item-3")
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(attachment.media, MediaKind::File);
        assert_eq!(
            attachment.variants[0].container,
            Some(Container::Other("zip".into()))
        );
        assert_eq!(attachment.variants[0].size, Some(4096));

        let Resolution::Playlist(embed) = resolve("https://www.patreon.com/posts/1682498")
            .await
            .unwrap()
        else {
            panic!("an embed beside an image makes a playlist");
        };
        assert_eq!(embed.entries.len(), 2);
        assert_eq!(
            embed.entries[0].url.as_str(),
            "https://www.youtube.com/watch?v=SU4fj_aEMVw"
        );
        assert_eq!(embed.entries[0].title.as_deref(), Some("I'm on Patreon!"));
    }

    #[tokio::test]
    async fn locked_posts_ask_for_a_session_and_inline_media_is_looked_up() {
        let mut fixture = Fixture::new(PLATFORM, None);
        let locked = json!({
            "data": {"id": "51706779", "type": "post", "attributes": {"title": "KITCHEN", "content": null, "post_type": "video_external_file", "embed": null,
                "current_user_can_view": false, "image": {"url": "https://c10.patreonusercontent.com/x.jpg"}, "post_file": null, "patreon_url": "/posts/kitchen-51706779"},
                "relationships": relationships(vec![], None, vec![])},
            "included": loish()
        }).to_string();
        fixture
            .exchanges
            .push(get(&post_api("51706779"), 200, locked.clone()));
        fixture
            .exchanges
            .push(get(&post_api("51706779"), 200, locked));
        fixture.exchanges.push(get(
            &post_api("1"),
            404,
            json!({"errors": [{"code_name": "NotFound", "detail": "Not Found", "status": "404"}]})
                .to_string(),
        ));
        fixture.exchanges.push(get(&post_api("146966245"), 200, json!({
            "data": {"id": "146966245", "type": "post", "attributes": {"title": "scottfalco 1080", "content": "<p><div data-media-id=\"640\"></div></p>", "post_type": "text_only",
                "embed": null, "current_user_can_view": true, "image": null, "post_file": null, "patreon_url": "/posts/scottfalco-146966245", "published_at": "2025-12-30T02:30:00.000+00:00"},
                "relationships": relationships(vec![], None, vec![])},
            "included": loish()
        }).to_string()));
        fixture.exchanges.push(get("https://www.patreon.com/api/media/640", 200, json!({
            "data": {"id": "640", "type": "media", "attributes": {"file_name": "scottfalco.mp4", "mimetype": "video/mp4", "media_type": "video", "size_bytes": 5000000,
                "download_url": "https://c10.patreonusercontent.com/4/media/640/scottfalco.mp4?token-hash=d",
                "display": {"url": "https://c10.patreonusercontent.com/4/media/640/display.mp4?token-hash=p", "duration": 7.833333, "width": 1920, "height": 1080}}}
        }).to_string()));
        let resolver = PatreonResolver::new(Http::replay(fixture));
        let locked_url =
            Url::parse("https://www.patreon.com/posts/kitchen-as-seen-51706779").unwrap();
        let error = resolver.resolve(&locked_url).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::LoginRequired { platform, .. } if *platform == PLATFORM),
            "{error}"
        );
        resolver.http.with_jar(PLATFORM, |jar| {
            jar.insert(Cookie::new(SESSION_COOKIE, "abc", "patreon.com"));
        });
        let error = resolver.resolve(&locked_url).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("membership")),
            "{error}"
        );
        resolver.http.clear_jar(PLATFORM);
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.patreon.com/posts/1").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let inlined = resolver
            .resolve(&Url::parse("https://www.patreon.com/posts/scottfalco-146966245").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(inlined.media, MediaKind::Video);
        assert_eq!(inlined.title.as_deref(), Some("scottfalco 1080"));
        assert_eq!(inlined.duration, Some(Duration::from_secs_f64(7.833333)));
        assert_eq!(inlined.variants.len(), 1);
        assert!(inlined.variants[0].url.as_str().contains("display.mp4"));
        assert_eq!(inlined.variants[0].container, Some(Container::Mp4));
        assert_eq!(inlined.variants[0].height, Some(1080));
    }

    #[tokio::test]
    async fn creators_and_collections_list_their_posts() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get("https://www.patreon.com/api/search?q=loish", 200, json!({
            "data": [
                {"id": "campaign_1091323", "type": "campaign-document", "attributes": {"url": "https://www.patreon.com/leighellexson", "name": "Leigh Ellexson"}},
                {"id": "campaign_1641751", "type": "campaign-document", "attributes": {"url": "https://www.patreon.com/loish", "name": "Loish"}}
            ]
        }).to_string()));
        fixture.exchanges.push(get("https://www.patreon.com/api/campaigns/1641751", 200, json!({
            "data": {"id": "1641751", "type": "campaign", "attributes": {"name": "Loish", "creation_count": 1302, "is_nsfw": false, "url": "https://www.patreon.com/loish", "vanity": "loish"}},
            "included": [{"type": "user", "id": "4301314", "attributes": {"full_name": "Loish"}}]
        }).to_string()));
        fixture.exchanges.push(get("https://www.patreon.com/api/posts?filter%5Bcampaign_id%5D=1641751", 200, json!({
            "data": [
                {"id": "170283037", "type": "post", "attributes": {"post_type": "video_external_file", "current_user_can_view": false, "patreon_url": "/loish/posts/process-video-2-170283037", "title": "PROCESS VIDEO // tree - part 2"}},
                {"id": "170341772", "type": "post", "attributes": {"post_type": "text_only", "current_user_can_view": true, "patreon_url": "/loish/posts/catching-up-my-170341772", "title": "Catching up"}},
                {"id": "169244631", "type": "post", "attributes": {"post_type": "image_file", "current_user_can_view": true, "patreon_url": "/loish/posts/im-going-to-next-169244631", "title": "I'm going to London next week!"}}
            ],
            "meta": {"pagination": {"total": 1289, "cursors": {"next": "03:abc"}}}
        }).to_string()));
        fixture.exchanges.push(get("https://www.patreon.com/api/posts?filter%5Bcampaign_id%5D=1641751&page%5Bcursor%5D=03%3Aabc", 200, json!({
            "data": [
                {"id": "168417104", "type": "post", "attributes": {"post_type": "image_file", "current_user_can_view": true, "patreon_url": "/loish/posts/coming-soon-art-168417104", "title": "COMING SOON"}}
            ],
            "meta": {"pagination": {"total": 1289, "cursors": {"next": null}}}
        }).to_string()));
        fixture.exchanges.push(get("https://www.patreon.com/api/collection/96265", 200, json!({
            "data": {"id": "96265", "type": "collection", "attributes": {"title": "Tutorials", "description": "All of my video tutorials", "num_posts": 76},
                "relationships": {"posts": {"data": [{"id": "168513175", "type": "post"}, {"id": "168417942", "type": "post"}]}}},
            "included": [
                {"type": "post", "id": "168417942", "attributes": {"title": "TUTORIAL // creating flowy & streamlined art", "patreon_url": "/loish/posts/tutorial-flowy-168417942", "current_user_can_view": false}},
                {"type": "post", "id": "168513175", "attributes": {"title": "Early access: Loish's Water Pack", "patreon_url": "/loish/posts/early-access-168513175", "current_user_can_view": false}}
            ]
        }).to_string()));
        let resolver = PatreonResolver::new(Http::replay(fixture));
        let Resolution::Playlist(creator) = resolver
            .resolve(&Url::parse("https://www.patreon.com/loish").unwrap())
            .await
            .unwrap()
        else {
            panic!("a creator page is a playlist");
        };
        assert_eq!(creator.id.as_deref(), Some("1641751"));
        assert_eq!(creator.title.as_deref(), Some("Loish"));
        assert_eq!(creator.total, Some(1289));
        assert_eq!(creator.entries.len(), 3, "the text post is left out");
        assert_eq!(
            creator.entries[0].url.as_str(),
            "https://www.patreon.com/loish/posts/process-video-2-170283037"
        );
        assert_eq!(creator.entries[2].title.as_deref(), Some("COMING SOON"));
        let Resolution::Playlist(collection) = resolver
            .resolve(&Url::parse("https://www.patreon.com/collection/96265").unwrap())
            .await
            .unwrap()
        else {
            panic!("a collection is a playlist");
        };
        assert_eq!(collection.title.as_deref(), Some("Tutorials"));
        assert_eq!(collection.total, Some(76));
        assert_eq!(collection.entries.len(), 2);
        assert_eq!(
            collection.entries[0].title.as_deref(),
            Some("Early access: Loish's Water Pack")
        );
    }

    #[tokio::test]
    async fn sessions_are_checked_through_the_current_user() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get("https://www.patreon.com/api/current_user", 200, json!({
            "data": {"id": "1", "type": "user", "attributes": {"full_name": "Nick", "vanity": "nick"}}
        }).to_string()));
        fixture.exchanges.push(get("https://www.patreon.com/api/current_user", 401, json!({
            "errors": [{"code_name": "LoginRequired", "detail": "This route is restricted to logged in users.", "status": "401"}]
        }).to_string()));
        let resolver = PatreonResolver::new(Http::replay(fixture));
        assert_eq!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedOut
        );
        resolver.http.with_jar(PLATFORM, |jar| {
            jar.insert(Cookie::new(SESSION_COOKIE, "abc", "patreon.com"));
        });
        assert_eq!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedIn {
                account: "Nick".into()
            }
        );
        assert_eq!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedOut
        );
    }

    /// Every example link resolves live without a session: the public video, audio and
    /// image posts, and the creator and collection listings.
    #[ignore = "reaches the live site: cargo test -- --ignored"]
    #[tokio::test]

    async fn live_examples_resolve() {
        let resolver = PatreonResolver::new(Http::new(crate::http::HttpConfig::default()));
        let mut kinds = Vec::new();
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
                        "{link}: no playable variant"
                    );
                    println!(
                        "{link}: {:?} {:?} with {} variants",
                        resolved.media,
                        resolved.title,
                        resolved.variants.len()
                    );
                    kinds.push(resolved.media);
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
        assert!(kinds.contains(&MediaKind::Video));
        assert!(kinds.contains(&MediaKind::Audio));
        assert!(kinds.contains(&MediaKind::Image));
    }
}
