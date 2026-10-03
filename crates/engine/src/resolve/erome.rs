//! Erome albums and profiles: an album page lays out its videos and images in media
//! groups, each a `<video>` with its MP4 source and poster or an image with its full-size
//! file, and names the album's title and uploader. An album of one item is that item; an
//! album of several is a playlist whose entries name one item each by fragment. A
//! profile page lists the user's albums, a page at a time. The media host serves videos
//! only to requests that name the site as their referer.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionSupport, Tag, Variant, clean_title, fetch, navigation_headers, status_error,
    util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "erome";
const SITE: &str = "https://www.erome.com";
/// How many pages of a profile are read.
const PROFILE_PAGES: u32 = 3;

static RE_HOST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[a-z0-9-]+\.)*erome\.com$").unwrap());
/// `/a/{id}`.
static RE_ALBUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/a/([A-Za-z0-9]+)/?$").unwrap());
/// `/{user}`.
static RE_PROFILE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/([A-Za-z0-9_.-]+)/?$").unwrap());
/// `#item-3`: one item of an album.
static RE_ITEM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^item-(\d+)$").unwrap());
/// `<h1 class="album-title-page">…</h1>`.
static RE_TITLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<h1[^>]+class=["']album-title-page["'][^>]*>(.*?)</h1>"#).unwrap()
});
/// `<a href="https://www.erome.com/{user}" id="user_name">`: the uploader.
static RE_UPLOADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<a[^>]+href=["'](https?://(?:www\.)?erome\.com/([A-Za-z0-9_.-]+))["'][^>]*id=["']user_name["']"#)
        .unwrap()
});
/// `<source src="…" type='video/mp4' label='HD' res='720'>`.
static RE_SOURCE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<source\s[^>]*src=["']([^"']+)["'][^>]*>"#).unwrap());
static RE_RES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"res=["'](\d+)["']"#).unwrap());
static RE_POSTER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"poster=["']([^"']+)["']"#).unwrap());
/// `<span … class="duration">02:26</span>`.
static RE_DURATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"class=["']duration["'][^>]*>\s*([\d:]+)\s*<"#).unwrap());
/// `<div class="img" data-src="…">`: an image item.
static RE_IMAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<div\s[^>]*class=["']img["'][^>]*data-src=["']([^"']+)["']"#).unwrap()
});
/// `<img … class="img-front">`: the image as shown, with its size.
static RE_IMAGE_TAG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<img\s[^>]*class=["']img-front["'][^>]*>"#).unwrap());
/// `<a class="album-title" href="https://www.erome.com/a/{id}">{title}</a>`.
static RE_CARD: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<a[^>]+class=["']album-title["'][^>]*href=["']([^"']+)["'][^>]*>(.*?)</a>"#)
        .unwrap()
});
/// `<a class="album-link" href="https://www.erome.com/a/{id}">`: a card without a title.
static RE_CARD_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<a[^>]+class=["']album-link["'][^>]*href=["']([^"']+)["']"#).unwrap()
});
/// `href="/{user}?page=3"`: a page of the profile.
static RE_PAGE_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"href=["'][^"']*\?page=(\d+)["']"#).unwrap());
/// `3095 POSTS`: how many albums the profile holds.
static RE_POSTS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([\d,]+)\s*POSTS").unwrap());
/// `<h1 class="username">…</h1>`.
static RE_USERNAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?s)<h1[^>]+class=["']username["'][^>]*>(.*?)</h1>"#).unwrap());

/// Paths on the site that are not profiles.
const RESERVED: &[&str] = &[
    "a",
    "about",
    "admin",
    "api",
    "cams",
    "categories",
    "contact",
    "dmca",
    "explore",
    "faq",
    "favorites",
    "help",
    "i",
    "live",
    "login",
    "logout",
    "messages",
    "notifications",
    "password",
    "premium",
    "privacy",
    "profile",
    "register",
    "rules",
    "search",
    "settings",
    "tags",
    "terms",
    "top",
    "trending",
    "upload",
    "user",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// An album, or one item of it when the link names one.
    Album { id: String, item: Option<usize> },
    /// A user's albums, from `page` on.
    Profile { user: String, page: u32 },
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !RE_HOST.is_match(&host) {
        return None;
    }
    let path = url.path();
    if let Some(caps) = RE_ALBUM.captures(path) {
        let item = url
            .fragment()
            .and_then(|f| RE_ITEM.captures(f))
            .and_then(|c| c[1].parse::<usize>().ok())
            .filter(|n| *n >= 1);
        return Some(Link::Album {
            id: caps[1].to_string(),
            item,
        });
    }
    let caps = RE_PROFILE.captures(path)?;
    let user = caps[1].to_string();
    if RESERVED.contains(&user.to_ascii_lowercase().as_str()) || user.contains('.') {
        return None;
    }
    let page = util::query_param(url, "page")
        .and_then(|p| p.parse::<u32>().ok())
        .filter(|p| *p >= 1)
        .unwrap_or(1);
    Some(Link::Profile { user, page })
}

/// One item of an album.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Video {
        url: Url,
        poster: Option<Url>,
        height: Option<u32>,
        duration: Option<Duration>,
    },
    Image {
        url: Url,
        width: Option<u32>,
        height: Option<u32>,
    },
}

impl Item {
    fn url(&self) -> &Url {
        match self {
            Item::Video { url, .. } | Item::Image { url, .. } => url,
        }
    }

    fn duration(&self) -> Option<Duration> {
        match self {
            Item::Video { duration, .. } => *duration,
            Item::Image { .. } => None,
        }
    }
}

/// An album as its page lays it out.
#[derive(Debug, Clone, PartialEq)]
pub struct Album {
    pub title: Option<String>,
    pub uploader: Option<String>,
    pub uploader_url: Option<Url>,
    pub items: Vec<Item>,
}

/// The album a page carries: its media groups in order, each once.
pub fn parse_album(html: &str, page_url: &Url) -> Album {
    let page = Page::parse(html, page_url);
    let title = RE_TITLE
        .captures(html)
        .and_then(|caps| clean_title(&util::clean_html(&caps[1])))
        .or_else(|| page.meta("og:title").and_then(|t| clean_title(&t)));
    let (uploader, uploader_url) = RE_UPLOADER
        .captures(html)
        .map(|caps| (clean_title(&caps[2]), Url::parse(&caps[1]).ok()))
        .unwrap_or((None, None));
    let mut items: Vec<Item> = Vec::new();
    let starts: Vec<usize> = html
        .match_indices("class=\"media-group\"")
        .map(|(i, _)| i)
        .collect();
    for (index, start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(html.len());
        let block = &html[*start..end];
        let item = if let Some(caps) = RE_SOURCE.captures(block) {
            let Some(url) = Url::parse(&util::html_unescape(&caps[1])).ok() else {
                continue;
            };
            let tag = &caps[0];
            Item::Video {
                url,
                poster: RE_POSTER
                    .captures(block)
                    .and_then(|c| Url::parse(&util::html_unescape(&c[1])).ok()),
                height: RE_RES.captures(tag).and_then(|c| c[1].parse().ok()),
                duration: RE_DURATION
                    .captures(block)
                    .and_then(|c| super::parse_time_stamp(&c[1])),
            }
        } else if let Some(caps) = RE_IMAGE.captures(block) {
            let Some(url) = Url::parse(&util::html_unescape(&caps[1])).ok() else {
                continue;
            };
            let tag = RE_IMAGE_TAG.find(block).map(|m| m.as_str().to_string());
            let dimension = |name: &str| {
                tag.as_deref()
                    .and_then(|t| util::attribute(t, name))
                    .and_then(|v| v.parse::<u32>().ok())
                    .filter(|v| *v > 0)
            };
            Item::Image {
                url,
                width: dimension("width"),
                height: dimension("height"),
            }
        } else {
            continue;
        };
        if !items.iter().any(|known| known.url() == item.url()) {
            items.push(item);
        }
    }
    Album {
        title,
        uploader,
        uploader_url,
        items,
    }
}

/// The albums a profile page lists, each once, with their titles.
pub fn parse_profile(html: &str) -> Vec<(Url, Option<String>)> {
    let mut albums: Vec<(Url, Option<String>)> = Vec::new();
    for caps in RE_CARD.captures_iter(html) {
        if let Ok(url) = Url::parse(&util::html_unescape(&caps[1]))
            && !albums.iter().any(|(known, _)| *known == url)
        {
            albums.push((url, clean_title(&util::clean_html(&caps[2]))));
        }
    }
    for caps in RE_CARD_LINK.captures_iter(html) {
        if let Ok(url) = Url::parse(&util::html_unescape(&caps[1]))
            && !albums.iter().any(|(known, _)| *known == url)
        {
            albums.push((url, None));
        }
    }
    albums
}

/// The last page a profile links to.
pub fn last_page(html: &str) -> u32 {
    RE_PAGE_LINK
        .captures_iter(html)
        .filter_map(|c| c[1].parse::<u32>().ok())
        .max()
        .unwrap_or(1)
}

fn referer() -> (String, String) {
    ("referer".to_string(), format!("{SITE}/"))
}

/// The variant an item plays as, sent with the site as referer, since the media host
/// refuses requests without it.
fn variant_of(item: &Item) -> Variant {
    match item {
        Item::Video {
            url,
            height,
            duration,
            ..
        } => {
            let mut variant = Variant::file(url.clone());
            variant.container = Some(Container::Mp4);
            variant.video = Some(VideoCodec::H264);
            variant.audio = Some(AudioCodec::Aac);
            variant.height = *height;
            variant.duration = *duration;
            variant.headers = vec![referer()];
            variant.format_id = Some(match height {
                Some(h) => format!("mp4-{h}p"),
                None => "mp4".to_string(),
            });
            variant.label = height.map(|h| format!("{h}p"));
            variant
        }
        Item::Image { url, width, height } => {
            let mut variant = Variant::file(url.clone());
            variant.container = Some(
                super::path_extension(url)
                    .as_deref()
                    .and_then(Container::from_extension)
                    .unwrap_or(Container::Jpeg),
            );
            variant.width = *width;
            variant.height = *height;
            variant.headers = vec![referer()];
            variant.format_id = Some("image".to_string());
            variant
        }
    }
}

fn kind_of(item: &Item) -> MediaKind {
    match item {
        Item::Video { .. } => MediaKind::Video,
        Item::Image { url, .. } => super::path_extension(url)
            .as_deref()
            .and_then(Container::from_extension)
            .map(|c| c.kind())
            .unwrap_or(MediaKind::Image),
    }
}

pub struct EromeResolver {
    http: Http,
}

impl EromeResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    async fn page(&self, page_url: &Url, origin: &Url) -> Result<String, ResolveError> {
        let fetched = fetch(
            &self.http,
            page_url,
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

    async fn resolve_album(
        &self,
        id: &str,
        item: Option<usize>,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let page_url = Url::parse(&format!("{SITE}/a/{id}")).expect("valid");
        let html = self.page(&page_url, url).await?;
        let album = parse_album(&html, &page_url);
        if album.items.is_empty() {
            return Err(ResolveError::unavailable(url, "the album lists no media"));
        }
        if album.items.len() > 1 && item.is_none() {
            let entries = album
                .items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    let mut entry_url = page_url.clone();
                    entry_url.set_fragment(Some(&format!("item-{}", index + 1)));
                    PlaylistEntry {
                        url: entry_url,
                        title: album.title.as_ref().map(|t| format!("{t} ({})", index + 1)),
                        duration: item.duration(),
                    }
                })
                .collect::<Vec<_>>();
            return Ok(Resolution::Playlist(Playlist {
                resolver: PLATFORM.to_string(),
                id: Some(id.to_string()),
                title: album.title.clone(),
                total: Some(entries.len()),
                entries,
            }));
        }
        let index = item.unwrap_or(1).clamp(1, album.items.len());
        let picked = &album.items[index - 1];
        let mut resolved = Resolved::of(PLATFORM, kind_of(picked));
        resolved.id = Some(if album.items.len() > 1 {
            format!("{id}-{index}")
        } else {
            id.to_string()
        });
        resolved.title = album.title.clone().map(|t| {
            if album.items.len() > 1 {
                format!("{t} ({index})")
            } else {
                t
            }
        });
        resolved.uploader = album.uploader.clone();
        resolved.uploader_url = album.uploader_url.clone();
        resolved.duration = picked.duration();
        resolved.thumbnail = match picked {
            Item::Video { poster, .. } => poster.clone(),
            Item::Image { url, .. } => Some(url.clone()),
        };
        resolved.webpage_url = Some(if album.items.len() > 1 {
            let mut link = page_url.clone();
            link.set_fragment(Some(&format!("item-{index}")));
            link
        } else {
            page_url.clone()
        });
        resolved.age_limit = Some(18);
        resolved.variants = vec![variant_of(picked)];
        Ok(Resolution::from(resolved))
    }

    async fn resolve_profile(
        &self,
        user: &str,
        first_page: u32,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let mut entries: Vec<PlaylistEntry> = Vec::new();
        let mut title = None;
        let mut total = None;
        let mut page_number = first_page;
        loop {
            let mut page_url = Url::parse(&format!("{SITE}/{user}")).expect("valid");
            if page_number > 1 {
                page_url
                    .query_pairs_mut()
                    .append_pair("page", &page_number.to_string());
            }
            let html = self.page(&page_url, url).await?;
            if title.is_none() {
                title = RE_USERNAME
                    .captures(&html)
                    .and_then(|c| clean_title(&util::clean_html(&c[1])))
                    .or_else(|| Some(user.to_string()));
            }
            total = total.or_else(|| {
                RE_POSTS
                    .captures(&html)
                    .and_then(|c| c[1].replace(',', "").parse::<usize>().ok())
            });
            let before = entries.len();
            for (album_url, album_title) in parse_profile(&html) {
                if !entries.iter().any(|e| e.url == album_url) {
                    entries.push(PlaylistEntry {
                        url: album_url,
                        title: album_title,
                        duration: None,
                    });
                }
            }
            let last = last_page(&html);
            page_number += 1;
            if entries.len() == before
                || page_number > last
                || page_number - first_page >= PROFILE_PAGES
            {
                break;
            }
        }
        if entries.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(user.to_string()),
            title,
            total: total.filter(|n| *n >= entries.len()),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for EromeResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Erome",
            hosts: &["erome.com"],
            features: &["albums", "videos", "images", "profiles"],
            formats: &["mp4", "jpg"],
            media: &[MediaKind::Video, MediaKind::Image],
            tags: &[Tag::Nsfw, Tag::Video, Tag::Images],
            session: SessionSupport::None,
            on_by_default: true,
            examples: &[
                "https://www.erome.com/a/cgalnn9K",
                "https://www.erome.com/a/z0VWMGch",
                "https://www.erome.com/a/z0VWMGch#item-2",
                "https://www.erome.com/a/zQyJDHkK",
                "https://www.erome.com/randyclips",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Album { id, item } => self.resolve_album(&id, item, url).await,
            Link::Profile { user, page } => self.resolve_profile(&user, page, url).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

    fn get(url: &str, status: u16, body: &str) -> Exchange {
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
                headers: vec![("content-type".into(), "text/html".into())],
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    const VIDEO_GROUP: &str = r##"<div id="juq7vljQ" ><div class="media-group" id="1" >
        <div class="img" data-html="#video160936656" style="display: none;" ></div>
        <div class="video-lg" id="video160936656" style="display: none;" >
            <video id="player-lg-160936656" loop class="lg-video-object" controls preload="none" poster="https://s15.erome.com/9139/0QAputHd/juq7vljQ.jpg" >
                <source src="https://v15.erome.com/9139/0QAputHd/juq7vljQ_720p.mp4" type='video/mp4' label='HD' res='720' >
            </video></div>
        <div class="video" ><video id="player-160936656" class="player" controls preload="none" poster="https://s15.erome.com/9139/0QAputHd/juq7vljQ.jpg" >
                <source src="https://v15.erome.com/9139/0QAputHd/juq7vljQ_720p.mp4" type='video/mp4' label='HD' res='720' >
            </video><span id="player-160936656-duration" class="duration" >02:26</span></div></div></div>"##;
    const IMAGE_GROUP: &str = r#"<div id="1vNSldID" ><div class="media-group" id="2" >
        <div class="img" data-src="https://s55.erome.com/9139/6U21A6aa/1vNSldID.jpg?v=1790268623" >
            <div class="img-box" style="width:min(100%,669px); aspect-ratio:669/432;">
                <img fetchpriority="high" width="669" height="432" alt="Latina #1vNSldID" class="img-front" src="https://s55.erome.com/9139/6U21A6aa/1vNSldID.jpg?v=1790268623" >
            </div>
            <div class="img-blur"><img width="669" height="432" alt="Latina #1vNSldID" class="img-back" src="https://s55.erome.com/9139/6U21A6aa/1vNSldID.jpg?v=1790268623" ></div>
        </div></div></div>"#;

    fn album_page(title: &str, groups: &[&str]) -> String {
        format!(
            r#"<html><head><meta property="og:title" content="{title}" /></head><body>
            <h1 class="album-title-page" >{title}</h1>
            <a href="https://www.erome.com/Spreadyourlips78" id="user_icon" ><img></a>
            <a href="https://www.erome.com/Spreadyourlips78" id="user_name" >Spreadyourlips78</a>
            <div id="album_x" class="col-sm-12 page-content" >{}</div></body></html>"#,
            groups.join("\n")
        )
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://www.erome.com/a/0QAputHd"),
            Some(Link::Album {
                id: "0QAputHd".into(),
                item: None
            })
        );
        assert_eq!(
            link("https://erome.com/a/0QAputHd#item-2"),
            Some(Link::Album {
                id: "0QAputHd".into(),
                item: Some(2)
            })
        );
        assert_eq!(
            link("https://www.erome.com/Spreadyourlips78"),
            Some(Link::Profile {
                user: "Spreadyourlips78".into(),
                page: 1
            })
        );
        assert_eq!(
            link("https://www.erome.com/Spreadyourlips78?page=3"),
            Some(Link::Profile {
                user: "Spreadyourlips78".into(),
                page: 3
            })
        );
        assert_eq!(link("https://www.erome.com/"), None);
        assert_eq!(link("https://www.erome.com/explore"), None);
        assert_eq!(link("https://www.erome.com/login"), None);
        assert_eq!(link("https://www.erome.com/search?q=x"), None);
        assert_eq!(link("https://www.erome.com/a/0QAputHd/edit"), None);
        assert_eq!(link("https://example.com/a/0QAputHd"), None);
    }

    #[test]
    fn album_pages_are_read_into_their_items() {
        let page_url = Url::parse("https://www.erome.com/a/x").unwrap();
        let album = parse_album(&album_page("Mixed", &[VIDEO_GROUP, IMAGE_GROUP]), &page_url);
        assert_eq!(album.title.as_deref(), Some("Mixed"));
        assert_eq!(album.uploader.as_deref(), Some("Spreadyourlips78"));
        assert_eq!(
            album.uploader_url.as_ref().unwrap().as_str(),
            "https://www.erome.com/Spreadyourlips78"
        );
        assert_eq!(album.items.len(), 2, "each source once");
        assert_eq!(
            album.items[0],
            Item::Video {
                url: Url::parse("https://v15.erome.com/9139/0QAputHd/juq7vljQ_720p.mp4").unwrap(),
                poster: Some(
                    Url::parse("https://s15.erome.com/9139/0QAputHd/juq7vljQ.jpg").unwrap()
                ),
                height: Some(720),
                duration: Some(Duration::from_secs(146)),
            }
        );
        assert_eq!(
            album.items[1],
            Item::Image {
                url: Url::parse("https://s55.erome.com/9139/6U21A6aa/1vNSldID.jpg?v=1790268623")
                    .unwrap(),
                width: Some(669),
                height: Some(432),
            }
        );
    }

    #[tokio::test]
    async fn albums_resolve_as_media_or_as_playlists_of_their_items() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.erome.com/a/0QAputHd",
            200,
            &album_page("XRecorder_20260925_02", &[VIDEO_GROUP]),
        ));
        for _ in 0..3 {
            fixture.exchanges.push(get(
                "https://www.erome.com/a/6U21A6aa",
                200,
                &album_page("Latina", &[VIDEO_GROUP, IMAGE_GROUP]),
            ));
        }
        fixture.exchanges.push(get(
            "https://www.erome.com/a/zzzzzzzz",
            404,
            "<html>gone</html>",
        ));
        let resolver = EromeResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.erome.com/a/0QAputHd").unwrap();
        assert!(resolver.matches(&url));
        let single = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(single.media, MediaKind::Video);
        assert_eq!(single.id.as_deref(), Some("0QAputHd"));
        assert_eq!(single.title.as_deref(), Some("XRecorder_20260925_02"));
        assert_eq!(single.uploader.as_deref(), Some("Spreadyourlips78"));
        assert_eq!(single.duration, Some(Duration::from_secs(146)));
        assert_eq!(single.age_limit, Some(18));
        assert_eq!(single.variants.len(), 1);
        let video = &single.variants[0];
        assert_eq!(video.height, Some(720));
        assert_eq!(video.container, Some(Container::Mp4));
        assert_eq!(
            video.headers,
            vec![("referer".to_string(), "https://www.erome.com/".to_string())]
        );
        assert!(single.thumbnail.is_some());

        let Resolution::Playlist(album) = resolver
            .resolve(&Url::parse("https://www.erome.com/a/6U21A6aa").unwrap())
            .await
            .unwrap()
        else {
            panic!("an album of two is a playlist");
        };
        assert_eq!(album.title.as_deref(), Some("Latina"));
        assert_eq!(album.entries.len(), 2);
        assert_eq!(
            album.entries[1].url.as_str(),
            "https://www.erome.com/a/6U21A6aa#item-2"
        );
        assert_eq!(album.entries[0].duration, Some(Duration::from_secs(146)));
        let first = resolver
            .resolve(&album.entries[0].url)
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(first.media, MediaKind::Video);
        assert_eq!(first.id.as_deref(), Some("6U21A6aa-1"));
        assert_eq!(first.title.as_deref(), Some("Latina (1)"));
        let second = resolver
            .resolve(&album.entries[1].url)
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(second.media, MediaKind::Image);
        assert_eq!(second.id.as_deref(), Some("6U21A6aa-2"));
        assert_eq!(second.variants[0].container, Some(Container::Jpeg));
        assert_eq!(second.variants[0].width, Some(669));
        assert_eq!(second.variants[0].height, Some(432));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.erome.com/a/zzzzzzzz").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    fn profile_page(albums: &[(&str, &str)], pages: u32, posts: u32) -> String {
        let cards: String = albums
            .iter()
            .map(|(id, title)| {
                format!(
                    r#"<div class="album"><div class="album-thumbnail-container"><a class="album-link" href="https://www.erome.com/a/{id}" ><img></a></div>
                    <div class="album-infos"><div><a class="album-title" href="https://www.erome.com/a/{id}" >{title}</a></div></div></div>"#
                )
            })
            .collect();
        let links: String = (1..=pages)
            .map(|p| format!(r#"<a class="page-link" href="/Spreadyourlips78?page={p}">{p}</a>"#))
            .collect();
        format!(
            r#"<html><body><h1 class="username" >Spreadyourlips78</h1>
            <a href="https://www.erome.com/Spreadyourlips78?t=posts" class="menu-tab " >{posts} POSTS</a>
            <div id="albums" class="page-content row user-profile" >{cards}</div><ul>{links}</ul></body></html>"#
        )
    }

    #[tokio::test]
    async fn profiles_list_their_albums_across_pages() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.erome.com/Spreadyourlips78",
            200,
            &profile_page(
                &[
                    ("vJnQulyr", "XRecorder_20260924_01"),
                    ("yml6gmL3", "XRecorder_20260924_02"),
                ],
                2,
                3095,
            ),
        ));
        fixture.exchanges.push(get(
            "https://www.erome.com/Spreadyourlips78?page=2",
            200,
            &profile_page(
                &[
                    ("yml6gmL3", "XRecorder_20260924_02"),
                    ("0QAputHd", "XRecorder_20260925_02"),
                ],
                2,
                3095,
            ),
        ));
        let resolver = EromeResolver::new(Http::replay(fixture));
        let Resolution::Playlist(profile) = resolver
            .resolve(&Url::parse("https://www.erome.com/Spreadyourlips78").unwrap())
            .await
            .unwrap()
        else {
            panic!("a profile is a playlist");
        };
        assert_eq!(profile.title.as_deref(), Some("Spreadyourlips78"));
        assert_eq!(profile.total, Some(3095));
        assert_eq!(profile.entries.len(), 3, "two pages, each album once");
        assert_eq!(
            profile.entries[0].url.as_str(),
            "https://www.erome.com/a/vJnQulyr"
        );
        assert_eq!(
            profile.entries[2].title.as_deref(),
            Some("XRecorder_20260925_02")
        );
    }

    /// Every example link resolves live: albums to their media, profiles to albums.
    #[ignore = "reaches the live site: cargo test -- --ignored"]
    #[tokio::test]

    async fn live_examples_resolve() {
        let resolver = EromeResolver::new(Http::new(crate::http::HttpConfig::default()));
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
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
                        "{link}: {:?} {} variants {:?}",
                        resolved.media,
                        resolved.variants.len(),
                        resolved.title
                    );
                }
                Resolution::Playlist(playlist) => {
                    assert!(!playlist.entries.is_empty(), "{link}: no entries");
                    println!(
                        "{link}: {} entries of {:?} {:?}",
                        playlist.entries.len(),
                        playlist.total,
                        playlist.title
                    );
                }
            }
        }
    }
}
