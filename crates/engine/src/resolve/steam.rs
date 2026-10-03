//! Steam: a store page's trailers come from the storefront's app details API, which names
//! each trailer with its HLS master playlist. The community's shared files are
//! screenshots, read from their page as images, and videos, which are YouTube embeds
//! handed on to the YouTube resolver. A game's community hub lists its screenshots and
//! videos through the endpoint the hub scrolls with. The store and community age gates
//! are passed with the birth-date cookies the site sets.

use std::sync::LazyLock;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionSupport, Tag, Variant, VariantKind, clean_title, essence, fetch, manifests,
    navigation_headers, probe_file, status_error, util,
};
use crate::http::{BROWSER_UA, Cookie, Http};
use crate::media::{Container, MediaKind};

pub const PLATFORM: &str = "steam";
const STORE: &str = "https://store.steampowered.com";
const COMMUNITY: &str = "https://steamcommunity.com";
const APP_DETAILS: &str = "https://store.steampowered.com/api/appdetails";
/// How many shared files a hub listing is read up to.
const LISTING_LIMIT: usize = 40;

/// `/app/{id}` and `/agecheck/app/{id}`, with or without the game's name after the id.
static RE_APP: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(?:agecheck/)?app/(\d+)(?:/[^/]*)?/?$").unwrap());
/// `/sharedfiles/filedetails/{id}`: the id in the path rather than the query.
static RE_SHARED_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/sharedfiles/filedetails/(\d+)/?$").unwrap());
/// `/app/{id}/screenshots` and `/app/{id}/videos`: a community hub section.
static RE_HUB: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/app/(\d+)/(screenshots|videos)/?$").unwrap());
/// A YouTube embed on a community video page.
static RE_YOUTUBE_EMBED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"youtube\.com/embed/([A-Za-z0-9_-]{11})").unwrap());
/// `data-publishedfileid="…"`: a shared file card in a hub listing.
static RE_CARD_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-publishedfileid="(\d+)""#).unwrap());
/// The title on a hub card.
static RE_CARD_TITLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"apphub_CardContentTitle[^"]*"[^>]*>([^<]*)<"#).unwrap());
/// A hidden field of the hub's "more content" form.
static RE_HIDDEN_INPUT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<input[^>]+name="([^"]+)"[^>]+value="([^"]*)""#).unwrap());
/// `1600 x 1200`: a screenshot's dimensions among its details.
static RE_DIMENSIONS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(\d+)\s*x\s*(\d+)$").unwrap());
/// `0.276 MB`: a screenshot's size among its details.
static RE_FILE_SIZE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([\d.]+)\s*(KB|MB|GB)$").unwrap());
/// `Sep 18 @ 9:05am` or `Sep 18, 2024 @ 9:05am`: when a shared file was posted.
static RE_POSTED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([A-Z][a-z]{2}) (\d{1,2})(?:, (\d{4}))? @ (\d{1,2}:\d{2}[ap]m)$").unwrap()
});
/// The creator block of a shared file page: the profile link and the name in it.
static RE_CREATOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)class="creatorsBlock".*?<a[^>]+href="(https://steamcommunity\.com/(?:profiles|id)/[^"]+)"[^>]*>(.*?)</a>"#)
        .unwrap()
});
/// The `class="detailsStatRight"` values of a shared file page.
static RE_DETAIL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"detailsStatRight"[^>]*>([^<]*)<"#).unwrap());

/// Which trailer a store link picks out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    /// The trailer with this store id.
    Id(String),
    /// The n-th trailer, counted from one.
    Index(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Screenshots,
    Videos,
}

impl Section {
    fn path(self) -> &'static str {
        match self {
            Section::Screenshots => "screenshots",
            Section::Videos => "videos",
        }
    }

    /// The hub's number for the section, in the form it scrolls with.
    fn hub_subsection(self) -> &'static str {
        match self {
            Section::Screenshots => "2",
            Section::Videos => "3",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A store page: every trailer, or the one `movie` picks.
    App {
        app_id: String,
        movie: Option<Selector>,
    },
    /// A community shared file: a screenshot or a video.
    SharedFile { id: String },
    /// A community hub listing of an app's screenshots or videos.
    Hub { app_id: String, section: Section },
}

/// The trailer a store link picks: `?movie={id}`, `?movieid={id}`, `#movie_{id}` or
/// `#trailer-{n}`.
fn selector_of(url: &Url) -> Option<Selector> {
    if let Some(id) = util::query_param(url, "movie").or_else(|| util::query_param(url, "movieid"))
        && id.chars().all(|c| c.is_ascii_digit())
    {
        return Some(Selector::Id(id));
    }
    let fragment = url.fragment()?;
    if let Some(id) = fragment
        .strip_prefix("movie_")
        .or_else(|| fragment.strip_prefix("movie-"))
        && !id.is_empty()
        && id.chars().all(|c| c.is_ascii_digit())
    {
        return Some(Selector::Id(id.to_string()));
    }
    fragment
        .strip_prefix("trailer-")
        .and_then(|n| n.parse::<usize>().ok())
        .filter(|n| *n >= 1)
        .map(Selector::Index)
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    match host.as_str() {
        "store.steampowered.com" => {
            let caps = RE_APP.captures(url.path())?;
            Some(Link::App {
                app_id: caps[1].to_string(),
                movie: selector_of(url),
            })
        }
        "steamcommunity.com" | "www.steamcommunity.com" => {
            if let Some(caps) = RE_HUB.captures(url.path()) {
                return Some(Link::Hub {
                    app_id: caps[1].to_string(),
                    section: if &caps[2] == "videos" {
                        Section::Videos
                    } else {
                        Section::Screenshots
                    },
                });
            }
            if let Some(caps) = RE_SHARED_PATH.captures(url.path()) {
                return Some(Link::SharedFile {
                    id: caps[1].to_string(),
                });
            }
            if url.path().trim_end_matches('/') == "/sharedfiles/filedetails" {
                let id = util::query_param(url, "id")?;
                if id.chars().all(|c| c.is_ascii_digit()) {
                    return Some(Link::SharedFile { id });
                }
            }
            None
        }
        _ => None,
    }
}

/// A trailer as the app details API lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trailer {
    pub id: String,
    pub name: Option<String>,
    pub thumbnail: Option<Url>,
    pub hls: Option<Url>,
}

/// The app's name and trailers from the app details answer, which is keyed by an id
/// of the store's choosing rather than the one asked for.
pub fn app_of(answer: &Value) -> Option<(String, Vec<Trailer>)> {
    let entry = answer.as_object()?.values().next()?;
    if !entry["success"].as_bool().unwrap_or(false) {
        return None;
    }
    let data = &entry["data"];
    let name = util::text(&data["name"])?;
    let trailers = data["movies"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|movie| {
            Some(Trailer {
                id: util::uint(&movie["id"])?.to_string(),
                name: movie["name"].as_str().and_then(clean_title),
                thumbnail: util::url_of(&movie["thumbnail"], None),
                hls: util::url_of(&movie["hls_h264"], None),
            })
        })
        .collect();
    Some((name, trailers))
}

/// A store link to one trailer of an app.
fn trailer_link(app_id: &str, movie_id: &str) -> Url {
    Url::parse(&format!("{STORE}/app/{app_id}/?movie={movie_id}")).expect("valid")
}

fn shared_file_link(id: &str) -> Url {
    Url::parse(&format!("{COMMUNITY}/sharedfiles/filedetails/?id={id}")).expect("valid")
}

/// The name in `Steam Community :: Screenshot :: Shroomship`, and what kind of shared
/// file the page says it is.
pub fn shared_file_title(og_title: &str) -> (Option<String>, Option<String>) {
    let parts: Vec<&str> = og_title.split(" :: ").collect();
    match parts.as_slice() {
        [_, kind, name @ ..] => (clean_title(&name.join(" :: ")), clean_title(kind)),
        _ => (clean_title(og_title), None),
    }
}

/// `0.276 MB` in bytes.
pub fn parse_file_size(text: &str) -> Option<u64> {
    let caps = RE_FILE_SIZE.captures(text.trim())?;
    let number: f64 = caps[1].parse().ok()?;
    let factor = match &caps[2] {
        "KB" => 1024.0,
        "MB" => 1024.0 * 1024.0,
        _ => 1024.0 * 1024.0 * 1024.0,
    };
    Some((number * factor).round() as u64)
}

/// `Sep 18 @ 9:05am` (this year) or `Sep 18, 2024 @ 9:05am`, as the page shows it.
pub fn parse_posted(text: &str) -> Option<Timestamp> {
    let caps = RE_POSTED.captures(text.trim())?;
    let year = match caps.get(3) {
        Some(year) => year.as_str().to_string(),
        None => jiff::Zoned::now().year().to_string(),
    };
    let time = caps[4].replace("am", " AM").replace("pm", " PM");
    util::parse_timestamp_month_first(&format!("{} {}, {year} {time}", &caps[1], &caps[2]))
}

/// The `class="detailsStatRight"` values of a shared file page: its size, when it was
/// posted, and its dimensions, in the order the page lists them.
pub fn details_of(html: &str) -> Vec<String> {
    RE_DETAIL
        .captures_iter(html)
        .map(|caps| util::html_unescape(caps[1].trim()))
        .collect()
}

/// The cards of a hub listing: each shared file's id and title, each once.
pub fn cards_of(html: &str) -> Vec<(String, Option<String>)> {
    let positions: Vec<(usize, String)> = RE_CARD_ID
        .captures_iter(html)
        .map(|caps| (caps.get(0).expect("match").start(), caps[1].to_string()))
        .collect();
    let mut cards: Vec<(String, Option<String>)> = Vec::new();
    for (index, (start, id)) in positions.iter().enumerate() {
        if cards.iter().any(|(seen, _)| seen == id) {
            continue;
        }
        let end = positions
            .get(index + 1)
            .map(|(next, _)| *next)
            .unwrap_or(html.len());
        let title = RE_CARD_TITLE
            .captures(&html[*start..end])
            .and_then(|caps| clean_title(&util::html_unescape(&caps[1])));
        cards.push((id.clone(), title));
    }
    cards
}

/// `id="MoreContentForm{n}"`: the hub's "more content" form, numbered by page.
static RE_MORE_FORM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"id="MoreContentForm\d+""#).unwrap());

/// The hidden fields of the hub's "more content" form, which name the next page.
pub fn more_content_form(html: &str) -> Option<Vec<(String, String)>> {
    let start = RE_MORE_FORM.find(html)?.start();
    let end = html[start..].find("</form>")? + start;
    let fields: Vec<(String, String)> = RE_HIDDEN_INPUT
        .captures_iter(&html[start..end])
        .map(|caps| (caps[1].to_string(), util::html_unescape(&caps[2])))
        .collect();
    (!fields.is_empty()).then_some(fields)
}

pub struct SteamResolver {
    http: Http,
}

impl SteamResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
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

    /// The app's name and trailers.
    async fn app(
        &self,
        app_id: &str,
        origin: &Url,
    ) -> Result<(String, Vec<Trailer>), ResolveError> {
        let api = util::with_query(
            &Url::parse(APP_DETAILS).expect("valid"),
            &[("appids", app_id), ("cc", "us"), ("l", "english")],
        );
        let fetched = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        if let Some(error) = status_error(fetched.status, origin) {
            return Err(error);
        }
        let answer = fetched.json(origin)?;
        app_of(&answer).ok_or_else(|| ResolveError::NotFound(origin.clone()))
    }

    /// One trailer's streams: its HLS master playlist expanded into its renditions.
    async fn resolve_trailer(
        &self,
        app_id: &str,
        app_name: &str,
        trailer: &Trailer,
    ) -> Result<Resolution, ResolveError> {
        let webpage = trailer_link(app_id, &trailer.id);
        let Some(hls) = &trailer.hls else {
            return Err(ResolveError::unavailable(
                &webpage,
                "the trailer has no streams",
            ));
        };
        let mut master = Variant::hls(hls.clone());
        master.format_id = Some("hls".into());
        let mut subtitles = Vec::new();
        let variants =
            manifests::expand_all(&self.http, PLATFORM, vec![master], &mut subtitles, None).await;
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(format!("{app_id}-{}", trailer.id));
        resolved.title = trailer
            .name
            .clone()
            .or_else(|| clean_title(&format!("{app_name} trailer {}", trailer.id)));
        resolved.uploader = clean_title(app_name);
        resolved.uploader_url = Url::parse(&format!("{STORE}/app/{app_id}/")).ok();
        resolved.duration = variants.iter().find_map(|v| v.duration);
        resolved.thumbnail = trailer.thumbnail.clone();
        resolved.webpage_url = Some(webpage);
        resolved.subtitles = subtitles;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn resolve_app(
        &self,
        app_id: &str,
        movie: Option<&Selector>,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let (app_name, trailers) = self.app(app_id, url).await?;
        if trailers.is_empty() {
            return Err(ResolveError::unavailable(
                url,
                format!("the store page of {app_name} has no trailers"),
            ));
        }
        let picked = match movie {
            Some(Selector::Id(id)) => Some(
                trailers
                    .iter()
                    .find(|t| t.id == *id)
                    .ok_or_else(|| ResolveError::NotFound(url.clone()))?,
            ),
            Some(Selector::Index(n)) => Some(
                trailers
                    .get(n - 1)
                    .ok_or_else(|| ResolveError::NotFound(url.clone()))?,
            ),
            None if trailers.len() == 1 => trailers.first(),
            None => None,
        };
        if let Some(trailer) = picked {
            return self.resolve_trailer(app_id, &app_name, trailer).await;
        }
        let entries = trailers
            .iter()
            .map(|trailer| PlaylistEntry {
                url: trailer_link(app_id, &trailer.id),
                title: trailer
                    .name
                    .clone()
                    .or_else(|| clean_title(&format!("{app_name} trailer {}", trailer.id))),
                duration: None,
            })
            .collect::<Vec<_>>();
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.into(),
            id: Some(app_id.to_string()),
            title: clean_title(&format!("{app_name} trailers")),
            total: Some(entries.len()),
            entries,
        }))
    }

    /// A shared file: a video is a YouTube embed handed on, a screenshot or artwork is
    /// the image the page shows in full.
    async fn resolve_shared_file(&self, id: &str, url: &Url) -> Result<Resolution, ResolveError> {
        let page_url = shared_file_link(id);
        let html = self.page(&page_url, url).await?;
        if let Some(caps) = RE_YOUTUBE_EMBED.captures(&html) {
            return Err(ResolveError::Redirect(
                Url::parse(&format!("https://www.youtube.com/watch?v={}", &caps[1]))
                    .expect("valid"),
            ));
        }
        let (title, kind, og_image) = {
            let page = Page::parse(&html, &page_url);
            let (title, kind) = page
                .meta("og:title")
                .map(|t| shared_file_title(&t))
                .unwrap_or((None, None));
            (
                title,
                kind,
                page.meta("og:image").and_then(|u| Url::parse(&u).ok()),
            )
        };
        let image_tag = util::tags_where(&html, "img", &|attrs| {
            attrs.iter().any(|(k, v)| k == "id" && v == "ActualMedia")
        });
        let Some(image) = image_tag
            .first()
            .and_then(|tag| util::attribute(tag, "src"))
            .and_then(|src| Url::parse(&src).ok())
        else {
            return Err(ResolveError::unavailable(
                url,
                match kind {
                    Some(kind) => {
                        format!("the shared file is a {kind}, not a screenshot or a video")
                    }
                    None => "the shared file is not a screenshot or a video".to_string(),
                },
            ));
        };
        // The image host serves the original at the bare path, without the sizing query.
        let mut original = image.clone();
        original.set_query(None);
        let probed = probe_file(&self.http, &original, PLATFORM, BROWSER_UA, &[]).await?;
        if let Some(error) = status_error(probed.status, url) {
            return Err(error);
        }
        let details = details_of(&html);
        let mut variant = Variant::new(original, VariantKind::File);
        variant.container = Container::from_mime(&essence(probed.content_type.as_deref()));
        variant.size = probed
            .size
            .or_else(|| details.iter().find_map(|d| parse_file_size(d)));
        if let Some((width, height)) = details.iter().find_map(|d| {
            let caps = RE_DIMENSIONS.captures(d)?;
            Some((caps[1].parse::<u32>().ok()?, caps[2].parse::<u32>().ok()?))
        }) {
            variant.width = Some(width);
            variant.height = Some(height);
        }
        variant.format_id = Some("original".into());
        let mut resolved = Resolved::of(PLATFORM, MediaKind::Image);
        resolved.id = Some(id.to_string());
        resolved.title = title.or_else(|| clean_title(&format!("Screenshot {id}")));
        resolved.description = util::element_by_class(&html, "screenshotDescription")
            .map(|d| util::clean_html(&d))
            .and_then(|d| clean_title(&d));
        if let Some(caps) = RE_CREATOR.captures(&html) {
            resolved.uploader = clean_title(&util::clean_html(&caps[2]));
            resolved.uploader_url = Url::parse(&util::html_unescape(&caps[1])).ok();
        }
        resolved.uploaded_at = details.iter().find_map(|d| parse_posted(d));
        resolved.thumbnail = og_image;
        resolved.webpage_url = Some(page_url);
        resolved.variants = vec![variant];
        Ok(Resolution::from(resolved))
    }

    /// A hub listing: the first page as the hub shows it, then the pages it scrolls to.
    async fn resolve_hub(
        &self,
        app_id: &str,
        section: Section,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let hub_url =
            Url::parse(&format!("{COMMUNITY}/app/{app_id}/{}/", section.path())).expect("valid");
        let html = self.page(&hub_url, url).await?;
        let app_name = util::element_by_class(&html, "apphub_AppName")
            .map(|n| util::clean_html(&n))
            .and_then(|n| clean_title(&n));
        let mut cards = cards_of(&html);
        let mut form = more_content_form(&html);
        while cards.len() < LISTING_LIMIT
            && let Some(fields) = form.take()
        {
            let more =
                Url::parse(&format!("{COMMUNITY}/app/{app_id}/homecontent/")).expect("valid");
            let mut pairs: Vec<(&str, &str)> = fields
                .iter()
                .filter(|(name, _)| name != "appHubSubSection")
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect();
            pairs.push(("appHubSubSection", section.hub_subsection()));
            let more = util::with_query(&more, &pairs);
            let fetched = fetch(&self.http, &more, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
            if let Some(error) = status_error(fetched.status, url) {
                return Err(error);
            }
            let page = fetched.text();
            let found = cards_of(&page);
            if found.is_empty() {
                break;
            }
            for card in found {
                if !cards.iter().any(|(id, _)| *id == card.0) {
                    cards.push(card);
                }
            }
            form = more_content_form(&page);
        }
        cards.truncate(LISTING_LIMIT);
        if cards.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let entries = cards
            .into_iter()
            .map(|(id, title)| PlaylistEntry {
                url: shared_file_link(&id),
                title,
                duration: None,
            })
            .collect::<Vec<_>>();
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.into(),
            id: Some(format!("{app_id}-{}", section.path())),
            title: Some(match app_name {
                Some(name) => format!("{name} community {}", section.path()),
                None => format!("Steam community {}", section.path()),
            }),
            total: None,
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for SteamResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Steam",
            hosts: &["store.steampowered.com", "steamcommunity.com"],
            features: &[
                "store trailers",
                "community videos",
                "community screenshots",
                "hub listings",
            ],
            formats: &["hls", "jpg", "png"],
            media: &[MediaKind::Video, MediaKind::Image],
            tags: &[Tag::Video, Tag::Images],
            session: SessionSupport::None,
            on_by_default: true,
            examples: &[
                "https://store.steampowered.com/app/105600/Terraria/",
                "https://store.steampowered.com/app/105600/?movie=81300",
                "https://steamcommunity.com/sharedfiles/filedetails/?id=3803813067",
                "https://steamcommunity.com/app/105600/screenshots/",
                "https://steamcommunity.com/app/105600/videos/",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::App { app_id, movie } => self.resolve_app(&app_id, movie.as_ref(), url).await,
            Link::SharedFile { id } => self.resolve_shared_file(&id, url).await,
            Link::Hub { app_id, section } => self.resolve_hub(&app_id, section, url).await,
        }
    }

    /// The store and the community ask for a birth date before showing mature games.
    fn consent_cookies(&self) -> Vec<Cookie> {
        ["store.steampowered.com", "steamcommunity.com"]
            .into_iter()
            .flat_map(|domain| {
                [
                    Cookie::new("birthtime", "946652401", domain),
                    Cookie::new("lastagecheckage", "1-January-2000", domain),
                    Cookie::new("wants_mature_content", "1", domain),
                ]
            })
            .collect()
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

    const API: &str = "https://store.steampowered.com/api/appdetails?appids=105600&cc=us&l=english";
    const CDN: &str =
        "https://video.akamai.steamstatic.com/store_trailers/105600/6446/abc/1750498443";

    fn app_details() -> String {
        json!({"1323320": {"success": true, "data": {"name": "Terraria", "steam_appid": 105600, "movies": [
            {"id": 257274214, "name": "Terraria: Bigger & Boulder Trailer", "thumbnail": "https://shared.akamai.steamstatic.com/store_item_assets/steam/apps/257274214/movie_600x337.jpg",
             "hls_h264": "https://video.akamai.steamstatic.com/store_trailers/105600/1114648825/x/1769541214/hls_264_master.m3u8", "highlight": true},
            {"id": 81300, "name": "Terraria 1.1 Trailer", "thumbnail": "https://shared.akamai.steamstatic.com/store_item_assets/steam/apps/81300/movie.293x165.jpg",
             "hls_h264": format!("{CDN}/hls_264_master.m3u8"), "highlight": false}
        ]}}}).to_string()
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://store.steampowered.com/app/105600/Terraria/"),
            Some(Link::App {
                app_id: "105600".into(),
                movie: None
            })
        );
        assert_eq!(
            link("https://store.steampowered.com/agecheck/app/1091500"),
            Some(Link::App {
                app_id: "1091500".into(),
                movie: None
            })
        );
        assert_eq!(
            link("https://store.steampowered.com/app/105600/?movie=81300"),
            Some(Link::App {
                app_id: "105600".into(),
                movie: Some(Selector::Id("81300".into()))
            })
        );
        assert_eq!(
            link("https://store.steampowered.com/app/105600/Terraria/#movie_81300"),
            Some(Link::App {
                app_id: "105600".into(),
                movie: Some(Selector::Id("81300".into()))
            })
        );
        assert_eq!(
            link("https://store.steampowered.com/app/105600#trailer-2"),
            Some(Link::App {
                app_id: "105600".into(),
                movie: Some(Selector::Index(2))
            })
        );
        assert_eq!(
            link("https://steamcommunity.com/sharedfiles/filedetails/?id=3803813067"),
            Some(Link::SharedFile {
                id: "3803813067".into()
            })
        );
        assert_eq!(
            link("https://steamcommunity.com/sharedfiles/filedetails/2717708756"),
            Some(Link::SharedFile {
                id: "2717708756".into()
            })
        );
        assert_eq!(
            link("https://steamcommunity.com/app/105600/videos/"),
            Some(Link::Hub {
                app_id: "105600".into(),
                section: Section::Videos
            })
        );
        assert_eq!(
            link("https://www.steamcommunity.com/app/105600/screenshots"),
            Some(Link::Hub {
                app_id: "105600".into(),
                section: Section::Screenshots
            })
        );
        assert_eq!(link("https://store.steampowered.com/"), None);
        assert_eq!(
            link("https://store.steampowered.com/search/?term=terraria"),
            None
        );
        assert_eq!(
            link("https://steamcommunity.com/id/someone/screenshots/"),
            None
        );
        assert_eq!(
            link("https://steamcommunity.com/app/105600/discussions/"),
            None
        );
        assert_eq!(
            link("https://steamcommunity.com/workshop/filedetails/?id=1"),
            None
        );
        assert_eq!(link("https://example.com/app/105600"), None);
    }

    #[test]
    fn page_details_are_read() {
        assert_eq!(parse_file_size("0.276 MB"), Some(289407));
        assert_eq!(parse_file_size("12 KB"), Some(12288));
        assert_eq!(parse_file_size("large"), None);
        let posted = parse_posted("Sep 18, 2024 @ 9:05am").unwrap();
        assert_eq!(posted.to_string(), "2024-09-18T09:05:00Z");
        let this_year = parse_posted("Sep 18 @ 9:05pm").unwrap();
        assert_eq!(
            this_year.to_zoned(jiff::tz::TimeZone::UTC).year(),
            jiff::Zoned::now().year()
        );
        assert_eq!(parse_posted("yesterday"), None);
        assert_eq!(
            shared_file_title("Steam Community :: Screenshot :: Shroomship"),
            (Some("Shroomship".into()), Some("Screenshot".into()))
        );
        assert_eq!(
            shared_file_title("Steam Community :: Video :: PSA: Teleportation Potion"),
            (
                Some("PSA: Teleportation Potion".into()),
                Some("Video".into())
            )
        );
        assert_eq!(
            shared_file_title("Steam Community :: Guide :: A :: B"),
            (Some("A :: B".into()), Some("Guide".into()))
        );
    }

    #[tokio::test]
    async fn one_trailer_resolves_with_its_renditions() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture
            .exchanges
            .push(get(API, 200, "application/json", &app_details()));
        fixture.exchanges.push(get(
            &format!("{CDN}/hls_264_master.m3u8"),
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2600000,CODECS=\"avc1.640029,mp4a.40.2\",RESOLUTION=1280x720\nhls_264_1_video.m3u8\n",
        ));
        fixture.exchanges.push(get(
            &format!("{CDN}/hls_264_1_video.m3u8"),
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:3\n#EXTINF:3.0,\n0.m4s\n#EXTINF:2.5,\n1.m4s\n#EXT-X-ENDLIST\n",
        ));
        let resolver = SteamResolver::new(Http::replay(fixture));
        let url = Url::parse("https://store.steampowered.com/app/105600/?movie=81300").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        crate::resolve::assert_one_family(&resolved.variants);
        assert_eq!(resolved.id.as_deref(), Some("105600-81300"));
        assert_eq!(resolved.title.as_deref(), Some("Terraria 1.1 Trailer"));
        assert_eq!(resolved.uploader.as_deref(), Some("Terraria"));
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(5.5)));
        assert!(resolved.thumbnail.is_some());
        assert_eq!(
            resolved.variants.len(),
            1,
            "the one rendition the master lists"
        );
        let rendition = &resolved.variants[0];
        assert_eq!(rendition.kind, VariantKind::Hls);
        assert_eq!(rendition.height, Some(720));
        assert_eq!(
            rendition.url.as_str(),
            format!("{CDN}/hls_264_1_video.m3u8")
        );
    }

    #[tokio::test]
    async fn several_trailers_make_a_playlist_and_missing_apps_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture
            .exchanges
            .push(get(API, 200, "application/json", &app_details()));
        fixture.exchanges.push(get(
            "https://store.steampowered.com/api/appdetails?appids=1&cc=us&l=english",
            200,
            "application/json",
            &json!({"1": {"success": false}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://store.steampowered.com/api/appdetails?appids=2&cc=us&l=english",
            200,
            "application/json",
            &json!({"2": {"success": true, "data": {"name": "Soundtrack", "movies": []}}})
                .to_string(),
        ));
        let resolver = SteamResolver::new(Http::replay(fixture));
        let Resolution::Playlist(playlist) = resolver
            .resolve(&Url::parse("https://store.steampowered.com/app/105600/Terraria/").unwrap())
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(playlist.title.as_deref(), Some("Terraria trailers"));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[1].url.as_str(),
            "https://store.steampowered.com/app/105600/?movie=81300"
        );
        assert_eq!(
            playlist.entries[0].title.as_deref(),
            Some("Terraria: Bigger & Boulder Trailer")
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://store.steampowered.com/app/1/").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(&Url::parse("https://store.steampowered.com/app/2/").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("no trailers")),
            "{error}"
        );
    }

    const SCREENSHOT_PAGE: &str = r#"<html><head><meta property="og:title" content="Steam Community :: Screenshot :: Shroomship"><meta property="og:image" content="https://images.steamusercontent.com/ugc/1571/F55F/?imw=512&amp;imh=384"></head><body>
<div class="apphub_AppName ellipsis">Terraria</div>
<img id="ActualMedia" class="screenshotEnlargeable" src="https://images.steamusercontent.com/ugc/1571/F55F/?imw=1024&imh=768&ima=fit&impolicy=Letterbox&imcolor=%23000000&letterbox=true" width="100%">
<div class="screenshotDescription">"Shroomship"</div>
<div class="creatorsBlock"><div class="friendlyName"><a href="https://steamcommunity.com/profiles/76561198870423114">FaNNi</a></div></div>
<div class="detailsStatRight">0.276 MB</div><div class="detailsStatRight">Sep 18, 2024 @ 9:05am</div><div class="detailsStatRight">1600 x 1200</div>
</body></html>"#;

    #[tokio::test]
    async fn screenshots_are_images_and_videos_hand_on_to_youtube() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://steamcommunity.com/sharedfiles/filedetails/?id=3803813067",
            200,
            "text/html",
            SCREENSHOT_PAGE,
        ));
        fixture.exchanges.push(probe(
            "https://images.steamusercontent.com/ugc/1571/F55F/",
            206,
            "image/jpeg",
            289076,
        ));
        fixture.exchanges.push(get(
            "https://steamcommunity.com/sharedfiles/filedetails/?id=2502924646",
            200,
            "text/html",
            r#"<html><head><meta property="og:title" content="Steam Community :: Video :: PSA"></head><body><div class="movieFrame modal" id="N9bLO0XY3zs"><iframe src="https://www.youtube.com/embed/N9bLO0XY3zs?autoplay=1"></iframe></div></body></html>"#,
        ));
        fixture.exchanges.push(get(
            "https://steamcommunity.com/sharedfiles/filedetails/?id=1",
            200,
            "text/html",
            r#"<html><head><meta property="og:title" content="Steam Community :: Guide :: How to"></head><body>words</body></html>"#,
        ));
        fixture.exchanges.push(get(
            "https://steamcommunity.com/sharedfiles/filedetails/?id=2",
            404,
            "text/html",
            "",
        ));
        let resolver = SteamResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(
                &Url::parse("https://steamcommunity.com/sharedfiles/filedetails/?id=3803813067")
                    .unwrap(),
            )
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.media, MediaKind::Image);
        assert_eq!(resolved.title.as_deref(), Some("Shroomship"));
        assert_eq!(resolved.description.as_deref(), Some("\"Shroomship\""));
        assert_eq!(resolved.uploader.as_deref(), Some("FaNNi"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://steamcommunity.com/profiles/76561198870423114"
        );
        assert_eq!(
            resolved.uploaded_at.unwrap().to_string(),
            "2024-09-18T09:05:00Z"
        );
        assert_eq!(resolved.variants.len(), 1);
        let image = &resolved.variants[0];
        assert_eq!(
            image.url.as_str(),
            "https://images.steamusercontent.com/ugc/1571/F55F/"
        );
        assert_eq!(image.container, Some(Container::Jpeg));
        assert_eq!(image.size, Some(289076));
        assert_eq!((image.width, image.height), (Some(1600), Some(1200)));
        let redirect = resolver
            .resolve(
                &Url::parse("https://steamcommunity.com/sharedfiles/filedetails/?id=2502924646")
                    .unwrap(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&redirect, ResolveError::Redirect(to) if to.as_str() == "https://www.youtube.com/watch?v=N9bLO0XY3zs"),
            "{redirect}"
        );
        let error = resolver
            .resolve(
                &Url::parse("https://steamcommunity.com/sharedfiles/filedetails/?id=1").unwrap(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("Guide")),
            "{error}"
        );
        assert!(matches!(
            resolver
                .resolve(
                    &Url::parse("https://steamcommunity.com/sharedfiles/filedetails/?id=2")
                        .unwrap()
                )
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    fn card(id: u64, title: &str) -> String {
        format!(
            r#"<div class="apphub_Card modalContentLink interactable ugc" data-appid="105600" data-publishedfileid="{id}"><div class="apphub_CardContentType">Video</div><div class="apphub_CardContentTitle ellipsis">{title}&nbsp;</div></div>"#
        )
    }

    #[tokio::test]
    async fn hubs_list_their_shared_files_across_pages() {
        let first = format!(
            r#"<html><body><div class="apphub_AppName">Terraria</div>{}{}<form method="GET" id="MoreContentForm1" action="https://steamcommunity.com/app/105600/homecontent/"><input type="hidden" name="p" value="2"><input type="hidden" name="videospage" value="2"><input type="hidden" name="numperpage" value="10"><input type="hidden" name="browsefilter" value="trend"><input type="hidden" name="appHubSubSection" value="3"><input type="hidden" name="forceanon" value="1"></form></body></html>"#,
            card(1, "PSA: Teleportation Potion"),
            card(2, "PSA: Keybrand")
        );
        let second = format!(
            r#"{}<form method="GET" id="MoreContentForm2"><input type="hidden" name="p" value="3"><input type="hidden" name="appHubSubSection" value="3"></form>"#,
            card(3, "Shield of Cthulhu")
        );
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://steamcommunity.com/app/105600/videos/",
            200,
            "text/html",
            &first,
        ));
        fixture.exchanges.push(get(
            "https://steamcommunity.com/app/105600/homecontent/?p=2&videospage=2&numperpage=10&browsefilter=trend&forceanon=1&appHubSubSection=3",
            200,
            "text/html",
            &second,
        ));
        fixture.exchanges.push(get(
            "https://steamcommunity.com/app/105600/homecontent/?p=3&appHubSubSection=3",
            200,
            "text/html",
            "<div>nothing more</div>",
        ));
        let resolver = SteamResolver::new(Http::replay(fixture));
        let Resolution::Playlist(playlist) = resolver
            .resolve(&Url::parse("https://steamcommunity.com/app/105600/videos/").unwrap())
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(playlist.title.as_deref(), Some("Terraria community videos"));
        assert_eq!(playlist.entries.len(), 3);
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://steamcommunity.com/sharedfiles/filedetails/?id=1"
        );
        assert_eq!(
            playlist.entries[0].title.as_deref(),
            Some("PSA: Teleportation Potion")
        );
        assert_eq!(
            playlist.entries[2].title.as_deref(),
            Some("Shield of Cthulhu")
        );
    }

    /// Every example link resolves live: the trailers as a playlist and one trailer with
    /// its streams, the screenshot as an image, and both hub listings with entries.
    #[ignore = "reaches the live site: cargo test -- --ignored"]
    #[tokio::test]

    async fn live_examples_resolve() {
        let resolver = SteamResolver::new(Http::new(crate::http::HttpConfig::default()));
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
                    kinds.push((link.to_string(), resolved.media));
                }
                Resolution::Playlist(playlist) => {
                    assert!(!playlist.entries.is_empty(), "{link}: no entries");
                    println!(
                        "{link}: playlist {:?} with {} entries",
                        playlist.title,
                        playlist.entries.len()
                    );
                }
            }
        }
        assert!(kinds.iter().any(|(_, k)| *k == MediaKind::Video));
        assert!(kinds.iter().any(|(_, k)| *k == MediaKind::Image));
    }
}
