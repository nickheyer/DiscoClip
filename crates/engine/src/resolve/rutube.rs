//! Rutube videos, shorts, broadcasts, playlists and channels: a video's details come from
//! the video API and its streams from the play options API, which names the HLS playlists
//! of a recording and the live playlists of a broadcast, along with its captions. A
//! private link carries a `p` token that both APIs take. Playlists and channels are read
//! page by page from the listing APIs, and a channel named by its slug is looked up on its
//! page, where the site's state maps the slug to the channel id.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, SubtitleFormat, SubtitleTrack, Tag, Variant, clean_title, fetch, hls,
    navigation_headers, path_extension, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "rutube";
const API: &str = "https://rutube.ru/api";
/// How many entries a playlist or channel is read up to.
const LISTING_LIMIT: usize = 100;

static RE_VIDEO_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{32}$").unwrap());
static RE_NUMBER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9]+$").unwrap());
static RE_SLUG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_.-]+$").unwrap());
/// `channelIdBySlug({"slug":"x"})` … `"channel_id":123`: the slug's channel in the page's
/// state.
static RE_CHANNEL_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"channelIdBySlug[\s\S]{0,3000}?"channel_id":\s*(\d+)"#).unwrap());

/// Which of a channel's uploads are listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    /// Everything the channel published.
    All,
    Videos,
    Shorts,
}

impl Section {
    /// The `origin__type` filter the listing API takes.
    fn origin_types(self) -> Option<&'static str> {
        match self {
            Section::All => None,
            Section::Videos => Some("rtb,rst,ifrm,rspa"),
            Section::Shorts => Some("rshorts"),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Section::All => "all",
            Section::Videos => "videos",
            Section::Shorts => "shorts",
        }
    }
}

/// A channel, by its number or by the slug of its `/u/` page.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelId {
    Number(String),
    Slug(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A video, short or broadcast, by its 32-character id, with the `p` token of a
    /// private link.
    Video {
        id: String,
        token: Option<String>,
    },
    /// An old numeric embed, which the options API maps to the video's id.
    Embed {
        number: String,
        token: Option<String>,
    },
    Playlist {
        id: String,
    },
    Channel {
        id: ChannelId,
        section: Section,
    },
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "rutube.ru" && host != "www.rutube.ru" {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let token = util::query_param(url, "p").filter(|p| !p.is_empty());
    let is_video = |id: &str| RE_VIDEO_ID.is_match(id);
    let section = |name: Option<&&str>| match name {
        None => Some(Section::All),
        Some(&"videos") => Some(Section::Videos),
        Some(&"shorts") => Some(Section::Shorts),
        Some(_) => None,
    };
    match segments.as_slice() {
        ["video", id]
        | ["video", "private", id]
        | ["shorts", id]
        | ["live", "video", id]
        | ["live", "video", "private", id]
        | ["play", "embed", id]
        | ["video", "embed", id]
        | ["embed", id]
            if is_video(id) =>
        {
            Some(Link::Video {
                id: id.to_string(),
                token,
            })
        }
        ["play", "embed", number] | ["video", "embed", number] if RE_NUMBER.is_match(number) => {
            Some(Link::Embed {
                number: number.to_string(),
                token,
            })
        }
        ["plst", id] if RE_NUMBER.is_match(id) => Some(Link::Playlist { id: id.to_string() }),
        ["video", "person", id] if RE_NUMBER.is_match(id) => Some(Link::Channel {
            id: ChannelId::Number(id.to_string()),
            section: Section::All,
        }),
        ["channel", id, rest @ ..] if RE_NUMBER.is_match(id) && rest.len() <= 1 => {
            Some(Link::Channel {
                id: ChannelId::Number(id.to_string()),
                section: section(rest.first())?,
            })
        }
        ["u", slug, rest @ ..] if RE_SLUG.is_match(slug) && rest.len() <= 1 => {
            Some(Link::Channel {
                id: ChannelId::Slug(slug.to_string()),
                section: section(rest.first())?,
            })
        }
        _ => None,
    }
}

/// The API link for `path` under the API root, with `extra` query parameters,
/// `format=json`, and the private token when the link carries one.
fn api_url(path: &str, token: Option<&str>, extra: &[(&str, &str)]) -> Url {
    let mut url = Url::parse(&format!("{API}/{path}")).expect("valid");
    {
        let mut pairs = url.query_pairs_mut();
        for (name, value) in extra {
            pairs.append_pair(name, value);
        }
        pairs.append_pair("format", "json");
        if let Some(token) = token {
            pairs.append_pair("p", token);
        }
    }
    url
}

/// The reason the options API gives for withholding a video, in English when it has
/// one, else in whatever language it has.
fn detail_reason(detail: &Value) -> Option<String> {
    let languages = detail["languages"].as_array()?;
    languages
        .iter()
        .find(|l| l["lang"].as_str() == Some("eng"))
        .or_else(|| languages.first())
        .and_then(|l| util::text(&l["title"]))
}

/// The subtitle format a caption file's name says it is in.
fn caption_format(url: &Url) -> SubtitleFormat {
    match path_extension(url).as_deref() {
        Some("srt") => SubtitleFormat::Srt,
        Some("ttml") | Some("dfxp") | Some("xml") => SubtitleFormat::Ttml,
        Some("ass") | Some("ssa") => SubtitleFormat::Ass,
        _ => SubtitleFormat::Vtt,
    }
}

/// The captions the options API lists, one track per language.
pub fn captions_of(options: &Value) -> Vec<SubtitleTrack> {
    options["captions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|caption| {
            let url = util::url_of(&caption["file"], None)?;
            let language = util::text(&caption["code"]).unwrap_or_else(|| "ru".to_string());
            Some(SubtitleTrack {
                format: caption_format(&url),
                url,
                language,
                name: util::text(&caption["langTitle"]),
                auto: false,
                headers: Vec::new(),
            })
        })
        .collect()
}

/// A file the options API names outright, rather than a playlist.
fn file_variant(url: Url, format_id: &str) -> Variant {
    let mut variant = Variant::file(url);
    let container = path_extension(&variant.url)
        .as_deref()
        .and_then(Container::from_extension)
        .unwrap_or(Container::Mp4);
    if container == Container::Mp4 {
        variant.video = Some(VideoCodec::H264);
        variant.audio = Some(AudioCodec::Aac);
    }
    variant.container = Some(container);
    variant.format_id = Some(format_id.to_string());
    variant
}

pub struct RutubeResolver {
    http: Http,
}

impl RutubeResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// An API answer as JSON, with its status: the APIs answer errors as JSON too.
    async fn api(&self, url: &Url, origin: &Url) -> Result<(u16, Value), ResolveError> {
        let fetched = fetch(
            &self.http,
            url,
            PLATFORM,
            BROWSER_UA,
            &[("accept".to_string(), "application/json".to_string())],
            MAX_PAGE,
        )
        .await?;
        let status = fetched.status.as_u16();
        match status {
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            500..=599 => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the API answered HTTP {status}"),
                ));
            }
            _ => {}
        }
        let json: Value = serde_json::from_slice(&fetched.body)
            .map_err(|e| ResolveError::malformed(origin, format!("API JSON: {e}")))?;
        Ok((status, json))
    }

    /// The play options of a video, or the reason it is withheld.
    async fn options(
        &self,
        id: &str,
        token: Option<&str>,
        origin: &Url,
    ) -> Result<Value, ResolveError> {
        let (status, options) = self
            .api(&api_url(&format!("play/options/{id}/"), token, &[]), origin)
            .await?;
        if status == 404 || status == 410 {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        if options["detail"].is_object() {
            return Err(ResolveError::unavailable(
                origin,
                detail_reason(&options["detail"])
                    .unwrap_or_else(|| "the video is withheld".to_string()),
            ));
        }
        if !(200..300).contains(&status) {
            return Err(ResolveError::unavailable(
                origin,
                util::text(&options["detail"])
                    .unwrap_or_else(|| format!("the options API answered HTTP {status}")),
            ));
        }
        if options["acl_access"]["allowed"] == Value::Bool(false) {
            return Err(ResolveError::unavailable(
                origin,
                util::text(&options["acl_access"]["err_text"])
                    .unwrap_or_else(|| "the video is withheld for this location".to_string()),
            ));
        }
        Ok(options)
    }

    /// The streams the options name: the recording's playlists, the broadcast's live
    /// playlists, and any file listed outright.
    async fn variants_of(
        &self,
        options: &Value,
        origin: &Url,
    ) -> Result<(Vec<Variant>, Vec<SubtitleTrack>, bool), ResolveError> {
        let mut playlists: Vec<(String, Url, bool)> = Vec::new();
        let mut variants: Vec<Variant> = Vec::new();
        for (key, value) in options["video_balancer"].as_object().into_iter().flatten() {
            let Some(url) = util::url_of(value, None) else {
                continue;
            };
            if playlists.iter().any(|(_, u, _)| *u == url) {
                continue;
            }
            if path_extension(&url).as_deref() == Some("m3u8") {
                playlists.push((key.clone(), url, false));
            } else {
                variants.push(file_variant(url, key));
            }
        }
        for stream in options["live_streams"]["hls"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(url) = util::url_of(&stream["url"], None)
                && !playlists.iter().any(|(_, u, _)| *u == url)
            {
                playlists.push(("live".to_string(), url, true));
            }
        }
        let mut subtitles = captions_of(options);
        let mut live = false;
        let mut failure = None;
        for (key, playlist, is_live) in &playlists {
            match hls::expand(&self.http, playlist, PLATFORM, BROWSER_UA, &[]).await {
                Ok(expanded) => {
                    live |= *is_live || expanded.live;
                    for mut variant in expanded.variants {
                        variant.live = *is_live || expanded.live;
                        variant.format_id = Some(match &variant.label {
                            Some(label) => format!("{key}-{label}"),
                            None => key.clone(),
                        });
                        if !variants.iter().any(|v| v.url == variant.url) {
                            variants.push(variant);
                        }
                    }
                    for track in expanded.subtitles {
                        if !subtitles.iter().any(|t| t.url == track.url) {
                            subtitles.push(track);
                        }
                    }
                }
                Err(error) => failure = Some(error),
            }
        }
        if variants.is_empty() {
            return Err(failure.unwrap_or_else(|| {
                ResolveError::unavailable(
                    origin,
                    if util::boolean(&options["is_paid"]).unwrap_or(false) {
                        "the video is paid"
                    } else {
                        "the options API names no stream"
                    },
                )
            }));
        }
        Ok((variants, subtitles, live))
    }

    async fn resolve_video(
        &self,
        id: &str,
        token: Option<&str>,
        options: Option<Value>,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let options = match options {
            Some(options) => options,
            None => self.options(id, token, origin).await?,
        };
        let (status, video) = self
            .api(&api_url(&format!("video/{id}/"), token, &[]), origin)
            .await?;
        if status == 404 || status == 410 {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        if !(200..300).contains(&status) {
            return Err(ResolveError::unavailable(
                origin,
                util::text(&video["detail"])
                    .unwrap_or_else(|| format!("the video API answered HTTP {status}")),
            ));
        }
        if util::boolean(&video["is_deleted"]).unwrap_or(false) {
            return Err(ResolveError::unavailable(origin, "the video was deleted"));
        }
        let (variants, subtitles, live) = self.variants_of(&options, origin).await?;
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.to_string());
        resolved.title = video["title"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| options["title"].as_str().and_then(clean_title));
        resolved.description = video["description"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| options["description"].as_str().and_then(clean_title));
        resolved.uploader = video["author"]["name"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| options["author"]["name"].as_str().and_then(clean_title));
        resolved.uploader_url = util::uint(&video["author"]["id"])
            .or_else(|| util::uint(&options["author"]["id"]))
            .and_then(|author| Url::parse(&format!("https://rutube.ru/channel/{author}/")).ok());
        resolved.uploaded_at =
            util::time(&video["publication_ts"]).or_else(|| util::time(&video["created_ts"]));
        resolved.live = live
            || util::boolean(&video["is_livestream"]).unwrap_or(false)
            || util::boolean(&video["is_on_air"]).unwrap_or(false);
        resolved.duration = if resolved.live {
            None
        } else {
            util::seconds(&video["duration"])
                .filter(|d| !d.is_zero())
                .or_else(|| util::millis(&options["duration"]).filter(|d| !d.is_zero()))
                .or_else(|| variants.iter().find_map(|v| v.duration))
        };
        resolved.thumbnail = util::url_of(&video["thumbnail_url"], None)
            .or_else(|| util::url_of(&options["thumbnail_url"], None));
        resolved.webpage_url = util::url_of(&video["video_url"], None)
            .or_else(|| Url::parse(&format!("https://rutube.ru/video/{id}/")).ok());
        resolved.age_limit = util::boolean(&video["is_adult"])
            .or_else(|| util::boolean(&options["is_adult"]))
            .filter(|adult| *adult)
            .map(|_| 18);
        resolved.subtitles = subtitles;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    /// An old numeric embed: the options API names the video it stands for.
    async fn resolve_embed(
        &self,
        number: &str,
        token: Option<&str>,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let options = self.options(number, token, origin).await?;
        let id = util::text(&options["effective_video"])
            .filter(|id| RE_VIDEO_ID.is_match(id))
            .ok_or_else(|| ResolveError::malformed(origin, "the embed names no video"))?;
        self.resolve_video(&id, token, Some(options), origin).await
    }

    /// The entries of a paged listing API, read page by page up to the limit, with how
    /// many the platform reports when there are more.
    async fn listing(
        &self,
        page_url: &(dyn Fn(usize) -> Url + Sync),
        origin: &Url,
    ) -> Result<(Vec<PlaylistEntry>, Option<usize>), ResolveError> {
        let mut entries: Vec<PlaylistEntry> = Vec::new();
        let mut total = None;
        for page in 1.. {
            let (status, listing) = self.api(&page_url(page), origin).await?;
            if status == 404 && page == 1 {
                return Err(ResolveError::NotFound(origin.clone()));
            }
            if !(200..300).contains(&status) {
                if page == 1 {
                    return Err(ResolveError::unavailable(
                        origin,
                        format!("the listing API answered HTTP {status}"),
                    ));
                }
                break;
            }
            let results = listing["results"].as_array().cloned().unwrap_or_default();
            for result in &results {
                let Some(url) = util::url_of(&result["video_url"], None) else {
                    continue;
                };
                if entries.iter().any(|e| e.url == url) {
                    continue;
                }
                entries.push(PlaylistEntry {
                    url,
                    title: result["title"].as_str().and_then(clean_title),
                    duration: util::seconds(&result["duration"]).filter(|d| !d.is_zero()),
                });
            }
            let has_next =
                util::boolean(&listing["has_next"]).unwrap_or(false) && !results.is_empty();
            if has_next
                && let (Some(pages), Some(per_page)) = (
                    util::uint(&listing["num_pages"]),
                    util::uint(&listing["per_page"]),
                )
            {
                total = Some((pages * per_page) as usize);
            }
            if !has_next || entries.len() >= LISTING_LIMIT {
                break;
            }
        }
        entries.truncate(LISTING_LIMIT);
        Ok((entries, total))
    }

    async fn resolve_playlist(&self, id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let (status, playlist) = self
            .api(
                &api_url(&format!("playlist/custom/{id}/"), None, &[]),
                origin,
            )
            .await?;
        if status == 404 {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        if !(200..300).contains(&status) {
            return Err(ResolveError::unavailable(
                origin,
                format!("the playlist API answered HTTP {status}"),
            ));
        }
        let (entries, total) = self
            .listing(
                &|page| {
                    api_url(
                        &format!("playlist/custom/{id}/videos"),
                        None,
                        &[("page", &page.to_string())],
                    )
                },
                origin,
            )
            .await?;
        if entries.is_empty() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(id.to_string()),
            title: playlist["title"].as_str().and_then(clean_title),
            total: util::uint(&playlist["videos_count"])
                .map(|n| n as usize)
                .or(total)
                .or(Some(entries.len())),
            entries,
        }))
    }

    /// The number of the channel a `/u/` slug names, from the page's state.
    async fn channel_number(&self, slug: &str, origin: &Url) -> Result<String, ResolveError> {
        let page_url = Url::parse(&format!("https://rutube.ru/u/{slug}/")).expect("valid");
        let fetched = fetch(
            &self.http,
            &page_url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
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
                    format!("the channel page answered HTTP {status}"),
                ));
            }
        }
        util::search(&RE_CHANNEL_ID, &fetched.text())
            .ok_or_else(|| ResolveError::malformed(origin, "the channel page names no channel id"))
    }

    async fn resolve_channel(
        &self,
        id: &ChannelId,
        section: Section,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let number = match id {
            ChannelId::Number(number) => number.clone(),
            ChannelId::Slug(slug) => self.channel_number(slug, origin).await?,
        };
        let (status, profile) = self
            .api(
                &api_url(&format!("profile/user/{number}/"), None, &[]),
                origin,
            )
            .await?;
        if status == 404 {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        let (entries, total) = self
            .listing(
                &|page| {
                    let page = page.to_string();
                    let mut query: Vec<(&str, &str)> = vec![("page", &page)];
                    if let Some(origin_types) = section.origin_types() {
                        query.push(("origin__type", origin_types));
                    }
                    api_url(&format!("video/person/{number}/"), None, &query)
                },
                origin,
            )
            .await?;
        if entries.is_empty() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        let name = profile["name"].as_str().and_then(clean_title);
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(match section {
                Section::All => number.clone(),
                other => format!("{number}_{}", other.name()),
            }),
            title: name.map(|name| match section {
                Section::All => name,
                Section::Videos => format!("{name}: videos"),
                Section::Shorts => format!("{name}: shorts"),
            }),
            total: match section {
                Section::All => util::uint(&profile["video_count"]).map(|n| n as usize),
                _ => None,
            }
            .or(total)
            .or(Some(entries.len())),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for RutubeResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Rutube",
            hosts: &["rutube.ru"],
            features: &[
                "videos",
                "shorts",
                "live",
                "playlists",
                "channels",
                "embeds",
                "private links",
            ],
            formats: &["hls", "mp4"],
            media: &[MediaKind::Video],
            tags: &[Tag::Video, Tag::Live],
            session: SessionSupport::None,
            examples: &[
                "https://rutube.ru/video/3eac3b4561676c17df9132a9a1e62e3e/",
                "https://rutube.ru/shorts/d23980aafd7b0cbd936c23b1f95c9bab/",
                "https://rutube.ru/live/video/c58f502c7bb34a8fcdd976b221fca292/",
                "https://rutube.ru/play/embed/03a9cb54bac3376af4c5cb0f18444e01/",
                "https://rutube.ru/plst/308547/",
                "https://rutube.ru/u/rutube/",
                "https://rutube.ru/channel/23704195/shorts/",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Video { id, token } => self.resolve_video(&id, token.as_deref(), None, url).await,
            Link::Embed { number, token } => {
                self.resolve_embed(&number, token.as_deref(), url).await
            }
            Link::Playlist { id } => self.resolve_playlist(&id, url).await,
            Link::Channel { id, section } => self.resolve_channel(&id, section, url).await,
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

    const ID: &str = "3eac3b4561676c17df9132a9a1e62e3e";
    const LIVE: &str = "c58f502c7bb34a8fcdd976b221fca292";

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let video = |id: &str, token: Option<&str>| {
            Some(Link::Video {
                id: id.into(),
                token: token.map(String::from),
            })
        };
        assert_eq!(
            link(&format!("https://rutube.ru/video/{ID}/")),
            video(ID, None)
        );
        assert_eq!(
            link(&format!("https://rutube.ru/video/{ID}/?pl_id=4252")),
            video(ID, None)
        );
        assert_eq!(
            link(&format!(
                "https://rutube.ru/video/private/{ID}/?p=x2QojCumHTS3rsKHWXN8Lg"
            )),
            video(ID, Some("x2QojCumHTS3rsKHWXN8Lg"))
        );
        assert_eq!(
            link(&format!("https://rutube.ru/shorts/{ID}/")),
            video(ID, None)
        );
        assert_eq!(
            link(&format!("https://rutube.ru/live/video/{LIVE}/")),
            video(LIVE, None)
        );
        assert_eq!(
            link(&format!("https://rutube.ru/live/video/private/{LIVE}/?p=t")),
            video(LIVE, Some("t"))
        );
        assert_eq!(
            link(&format!("https://www.rutube.ru/play/embed/{ID}")),
            video(ID, None)
        );
        assert_eq!(
            link(&format!("https://rutube.ru/embed/{ID}")),
            video(ID, None)
        );
        assert_eq!(
            link("https://rutube.ru/play/embed/10631925?p=IbAigKqWd1do4mjaM5XLIQ"),
            Some(Link::Embed {
                number: "10631925".into(),
                token: Some("IbAigKqWd1do4mjaM5XLIQ".into())
            })
        );
        assert_eq!(
            link("https://rutube.ru/video/embed/6722881?vk_puid37="),
            Some(Link::Embed {
                number: "6722881".into(),
                token: None
            })
        );
        assert_eq!(
            link("https://rutube.ru/plst/308547/"),
            Some(Link::Playlist {
                id: "308547".into()
            })
        );
        assert_eq!(
            link("https://rutube.ru/channel/23704195/"),
            Some(Link::Channel {
                id: ChannelId::Number("23704195".into()),
                section: Section::All
            })
        );
        assert_eq!(
            link("https://rutube.ru/channel/23704195/shorts/"),
            Some(Link::Channel {
                id: ChannelId::Number("23704195".into()),
                section: Section::Shorts
            })
        );
        assert_eq!(
            link("https://rutube.ru/video/person/313878/"),
            Some(Link::Channel {
                id: ChannelId::Number("313878".into()),
                section: Section::All
            })
        );
        assert_eq!(
            link("https://rutube.ru/u/rutube/videos/"),
            Some(Link::Channel {
                id: ChannelId::Slug("rutube".into()),
                section: Section::Videos
            })
        );
        assert_eq!(link("https://rutube.ru/u/rutube/playlists/"), None);
        assert_eq!(link("https://rutube.ru/"), None);
        assert_eq!(link("https://rutube.ru/video/notanid/"), None);
        assert_eq!(link("https://rutube.ru/tags/video/1800/"), None);
        assert_eq!(link(&format!("https://example.com/video/{ID}/")), None);
    }

    fn options(id: &str, balancer: Value, live: Value) -> String {
        json!({
            "acl_access": {"allowed": true, "err_code": null, "err_text": ""},
            "author": {"id": 29790, "name": "NTDRussian"},
            "captions": [{"code": "ru", "file": "https://cdn.rutube.ru/captions/x.vtt", "langTitle": "Русский"}],
            "detail": null, "duration": 81000, "effective_video": id, "is_adult": false,
            "live_streams": live, "thumbnail_url": "https://pic.rtbcdn.ru/video/d2/a0/x.jpg",
            "title": "Раненный кенгуру забежал в аптеку", "video_balancer": balancer
        })
        .to_string()
    }

    fn video(id: &str, live: bool) -> String {
        json!({
            "id": id, "title": "Раненный кенгуру забежал в аптеку", "description": "http://www.ntdtv.ru ",
            "thumbnail_url": "https://pic.rtbcdn.ru/video/d2/a0/d2a0aec998494a396deafc7ba2c82add.jpg",
            "created_ts": "2013-10-16T17:13:22", "publication_ts": "2013-10-16T17:13:22",
            "video_url": format!("https://rutube.ru/video/{id}/"), "duration": if live { 0 } else { 81 },
            "is_livestream": live, "is_on_air": live, "author": {"id": 29790, "name": "NTDRussian"},
            "is_adult": false, "is_deleted": false, "is_paid": false
        })
        .to_string()
    }

    const MASTER: &str = "#EXTM3U\n#EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=662000,FRAME-RATE=25,CODECS=\"avc1.42c01e, mp4a.40.2\",RESOLUTION=480x368\nhttps://river-1.rutube.ru/hls-vod/a/480.m3u8?i=480x368_662\n";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n0.ts\n#EXT-X-ENDLIST\n";

    #[tokio::test]
    async fn videos_resolve_with_their_playlists_and_captions() {
        let master = format!("https://bl.rutube.ru/route/{ID}.m3u8?guids=x&sign=s");
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            &format!("{API}/play/options/{ID}/?format=json"),
            200,
            "application/json",
            options(ID, json!({"default": master, "m3u8": master}), json!({})),
        ));
        fixture.exchanges.push(get(
            &format!("{API}/video/{ID}/?format=json"),
            200,
            "application/json",
            video(ID, false),
        ));
        fixture.exchanges.push(get(
            &master,
            200,
            "application/vnd.apple.mpegurl",
            MASTER.into(),
        ));
        fixture.exchanges.push(get(
            "https://river-1.rutube.ru/hls-vod/a/480.m3u8?i=480x368_662",
            200,
            "application/vnd.apple.mpegurl",
            MEDIA.into(),
        ));
        let resolver = RutubeResolver::new(Http::replay(fixture));
        let url = Url::parse(&format!("https://rutube.ru/video/{ID}/")).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some(ID));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Раненный кенгуру забежал в аптеку")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("NTDRussian"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://rutube.ru/channel/29790/"
        );
        assert_eq!(resolved.duration, Some(std::time::Duration::from_secs(81)));
        assert!(resolved.uploaded_at.is_some());
        assert!(!resolved.live);
        assert_eq!(resolved.age_limit, None);
        assert_eq!(resolved.variants.len(), 1, "the two keys name one playlist");
        assert_eq!(resolved.variants[0].height, Some(368));
        assert_eq!(resolved.variants[0].video, Some(VideoCodec::H264));
        assert_eq!(
            resolved.variants[0].format_id.as_deref(),
            Some("default-368p")
        );
        assert!(!resolved.variants[0].live);
        assert_eq!(resolved.subtitles.len(), 1);
        assert_eq!(resolved.subtitles[0].language, "ru");
        assert_eq!(resolved.subtitles[0].name.as_deref(), Some("Русский"));
        assert_eq!(resolved.subtitles[0].format, SubtitleFormat::Vtt);
    }

    #[tokio::test]
    async fn broadcasts_and_numeric_embeds_resolve() {
        let live_master = format!("https://bl.rutube.ru/livestream/{LIVE}/index.m3u8?s=k");
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            &format!("{API}/play/options/{LIVE}/?format=json"),
            200,
            "application/json",
            options(
                LIVE,
                json!({}),
                json!({"hls": [{"is_audio": true, "is_video": true, "url": live_master}]}),
            ),
        ));
        fixture.exchanges.push(get(
            &format!("{API}/video/{LIVE}/?format=json"),
            200,
            "application/json",
            video(LIVE, true),
        ));
        fixture.exchanges.push(get(
            &live_master,
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:RESOLUTION=640x360,CODECS=\"avc1.64001f,mp4a.40.2\",BANDWIDTH=1000000\nhttps://river-1.rutube.ru/stream/x/360p_stream.m3u8\n".into(),
        ));
        fixture.exchanges.push(get(
            "https://river-1.rutube.ru/stream/x/360p_stream.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n0.ts\n".into(),
        ));
        fixture.exchanges.push(get(
            &format!("{API}/play/options/6722881/?format=json&p=tok"),
            200,
            "application/json",
            options(
                ID,
                json!({"default": format!("https://bl.rutube.ru/route/{ID}.m3u8?guids=x&sign=s")}),
                json!({}),
            ),
        ));
        fixture.exchanges.push(get(
            &format!("{API}/video/{ID}/?format=json&p=tok"),
            200,
            "application/json",
            video(ID, false),
        ));
        fixture.exchanges.push(get(
            &format!("https://bl.rutube.ru/route/{ID}.m3u8?guids=x&sign=s"),
            200,
            "application/vnd.apple.mpegurl",
            MASTER.into(),
        ));
        fixture.exchanges.push(get(
            "https://river-1.rutube.ru/hls-vod/a/480.m3u8?i=480x368_662",
            200,
            "application/vnd.apple.mpegurl",
            MEDIA.into(),
        ));
        let resolver = RutubeResolver::new(Http::replay(fixture));
        let live = resolver
            .resolve(&Url::parse(&format!("https://rutube.ru/live/video/{LIVE}/")).unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert!(live.live);
        assert_eq!(live.duration, None);
        assert_eq!(live.variants.len(), 1);
        assert!(live.variants[0].live);
        assert_eq!(live.variants[0].height, Some(360));
        assert_eq!(live.variants[0].format_id.as_deref(), Some("live-360p"));
        let embed = resolver
            .resolve(&Url::parse("https://rutube.ru/play/embed/6722881?p=tok").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(embed.id.as_deref(), Some(ID));
        assert_eq!(embed.variants.len(), 1);
    }

    #[tokio::test]
    async fn withheld_and_missing_videos_say_so() {
        let hidden = "884fb55f07a97ab673c7d654553e0f48";
        let gone = "ffffffffffffffffffffffffffffffff";
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            &format!("{API}/play/options/{hidden}/?format=json"),
            200,
            "application/json",
            json!({"detail": {"id": 12, "name": "default_hidden_video", "languages": [
                {"lang": "rus", "title": "Автор скрыл это видео"},
                {"lang": "eng", "title": "This video has been hidden from public access by its author"}
            ]}})
            .to_string(),
        ));
        fixture.exchanges.push(get(
            &format!("{API}/play/options/{gone}/?format=json"),
            404,
            "application/json",
            json!({"detail": "Страница не найдена."}).to_string(),
        ));
        fixture.exchanges.push(get(
            &format!("{API}/playlist/custom/1/?format=json"),
            404,
            "application/json",
            json!({"detail": "Страница не найдена."}).to_string(),
        ));
        let resolver = RutubeResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse(&format!("https://rutube.ru/video/{hidden}/")).unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("hidden from public access")),
            "{error}"
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse(&format!("https://rutube.ru/video/{gone}/")).unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://rutube.ru/plst/1/").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    fn listing(ids: &[&str], page: usize, has_next: bool) -> String {
        json!({
            "has_next": has_next, "page": page, "per_page": 20, "num_pages": 2,
            "results": ids.iter().map(|id| json!({
                "id": id, "title": format!("Clip {id}"), "video_url": format!("https://rutube.ru/video/{id}/"),
                "duration": 489, "thumbnail_url": "https://pic.rtbcdn.ru/video/x.jpg"
            })).collect::<Vec<_>>()
        })
        .to_string()
    }

    #[tokio::test]
    async fn playlists_and_channels_list_their_videos() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            &format!("{API}/playlist/custom/308547/?format=json"),
            200,
            "application/json",
            json!({"id": 308547, "title": "музыка для расслабления", "videos_count": 23})
                .to_string(),
        ));
        fixture.exchanges.push(get(
            &format!("{API}/playlist/custom/308547/videos?page=1&format=json"),
            200,
            "application/json",
            listing(&["a1", "a2"], 1, true),
        ));
        fixture.exchanges.push(get(
            &format!("{API}/playlist/custom/308547/videos?page=2&format=json"),
            200,
            "application/json",
            listing(&["a3"], 2, false),
        ));
        fixture.exchanges.push(get(
            "https://rutube.ru/u/rutube/",
            200,
            "text/html",
            r#"<html><script>window.reduxState = {"api":{"queries":{"channelIdBySlug({\"slug\":\"rutube\"})":{"status":"fulfilled","data":{"channel_id":23704195,"name":"RUTUBE"}}}}};</script></html>"#.into(),
        ));
        fixture.exchanges.push(get(
            &format!("{API}/profile/user/23704195/?format=json"),
            200,
            "application/json",
            json!({"id": 23704195, "name": "RUTUBE", "video_count": 213}).to_string(),
        ));
        fixture.exchanges.push(get(
            &format!("{API}/video/person/23704195/?page=1&origin__type=rshorts&format=json"),
            200,
            "application/json",
            listing(&["s1", "s2"], 1, false),
        ));
        let resolver = RutubeResolver::new(Http::replay(fixture));
        let Resolution::Playlist(playlist) = resolver
            .resolve(&Url::parse("https://rutube.ru/plst/308547/").unwrap())
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(playlist.title.as_deref(), Some("музыка для расслабления"));
        assert_eq!(playlist.total, Some(23));
        assert_eq!(playlist.entries.len(), 3);
        assert_eq!(
            playlist.entries[2].url.as_str(),
            "https://rutube.ru/video/a3/"
        );
        assert_eq!(playlist.entries[0].title.as_deref(), Some("Clip a1"));
        assert_eq!(
            playlist.entries[0].duration,
            Some(std::time::Duration::from_secs(489))
        );
        let Resolution::Playlist(shorts) = resolver
            .resolve(&Url::parse("https://rutube.ru/u/rutube/shorts/").unwrap())
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(shorts.id.as_deref(), Some("23704195_shorts"));
        assert_eq!(shorts.title.as_deref(), Some("RUTUBE: shorts"));
        assert_eq!(shorts.entries.len(), 2);
        assert_eq!(shorts.total, Some(2));
    }

    /// Every example link resolves live: videos, the broadcast and the embed to media
    /// with playable streams, the playlist and channels to entries.
    #[ignore = "reaches the live site: cargo test -- --ignored"]
    #[tokio::test]

    async fn live_examples_resolve() {
        use std::time::Duration;

        let resolver = RutubeResolver::new(Http::new(crate::http::HttpConfig::default()));
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
                    assert!(resolved.title.is_some(), "{link}: no title");
                    println!(
                        "{link}: {} variants, live {}, {:?}",
                        resolved.variants.len(),
                        resolved.live,
                        resolved.title
                    );
                }
                Resolution::Playlist(playlist) => {
                    assert!(!playlist.entries.is_empty(), "{link}: no entries");
                    println!(
                        "{link}: {} entries of {:?}, {:?}",
                        playlist.entries.len(),
                        playlist.total,
                        playlist.title
                    );
                }
            }
        }
    }
}
