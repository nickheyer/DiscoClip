//! Pornhub videos, playlists and the video pages of users, channels, models and
//! pornstars. A video page carries its player's `flashvars`: the HLS master playlist of
//! each quality, and a `get_media` link that answers the page's own session with the MP4
//! file of each quality. Both are read here, and the HLS host is asked with the headers
//! a browser's player sends, which it insists on. Listing pages are the site's own HTML,
//! turned page by page, and a playlist's further pages come from the chunk endpoint its
//! page names with the token the page carries. The age and region gates are passed with
//! the site's own cookies. pornhub.org, pornhub.net, modelhub.com and thumbzilla.com
//! links name the same videos, and pornhubpremium.com is read with a stored session.

use std::sync::LazyLock;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    Fetched, MAX_PAGE, Page, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionCheck, SessionSupport, SubtitleFormat, SubtitleTrack, Tag, Variant,
    VariantKind, clean_title, fetch, navigation_headers, page, status_error, util,
};
use crate::http::{BROWSER_UA, Cookie, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "pornhub";
/// How many videos a listing is read up to.
const LISTING_LIMIT: usize = 200;
/// How many pages of a listing are turned.
const PAGE_LIMIT: u32 = 5;

/// `www.pornhub.com`, `de.pornhub.org`, `pornhubpremium.com`, `modelhub.com`,
/// `thumbzilla.com`: every host whose links name Pornhub videos.
static RE_HOST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:[a-z0-9-]+\.)*(pornhub(?:premium)?\.(?:com|net|org)|modelhub\.com|thumbzilla\.com)$",
    )
    .unwrap()
});
static RE_VIDEO_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\da-z]+$").unwrap());
static RE_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[\w.-]+$").unwrap());
/// `var flashvars_12345 = {…};`: the player's data on a video page.
static RE_FLASHVARS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"var\s+flashvars_\d+\s*=\s*(\{)").unwrap());
/// The reason a video page shows no player.
static RE_PAGE_ERROR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?s)<div[^>]+class="[^"]*\b(?:removed|userMessageSection)\b[^"]*"[^>]*>(.+?)</div>|<section[^>]+class="noVideo"[^>]*>(.+?)</section>"#,
    )
    .unwrap()
});
/// `1080P_4000K`: the height and bit rate a stream's file name carries.
static RE_STREAM_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+)P_(\d+)K").unwrap());
/// `/videos/202607/10/`: the day a video was uploaded, in its file path.
static RE_UPLOAD_DAY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/(\d{4})(\d{2})/(\d{2})/").unwrap());
/// The uploader named under a video: the first profile link of the user info block.
static RE_UPLOADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?s)<div class="userInfo">.*?<a[^>]+href="(/(?:users|channels|model|pornstar)/[^"?#]+)"[^>]*>\s*([^<]+?)\s*</a>"#,
    )
    .unwrap()
});
/// A video in a listing: `<li class="pcVideoListItem …" data-video-vkey="…">`.
static RE_ITEM: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<li\b[^>]*\bclass="[^"]*\bpcVideoListItem\b[^"]*"[^>]*>"#).unwrap()
});
static RE_ITEM_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-video-vkey="([\da-z]+)""#).unwrap());
static RE_ITEM_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<a\b[^>]+href="/view_video\.php\?viewkey=([\da-z]+)[^"]*"[^>]+title="([^"]*)""#)
        .unwrap()
});
static RE_ITEM_DURATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<var\b[^>]*\bclass="[^"]*\bduration\b[^"]*"[^>]*>([^<]+)</var>"#).unwrap()
});
static RE_NEXT_PAGE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"<li[^>]+\bclass="page_next|<link[^>]+\brel="next"|<button[^>]+\bid="moreDataBtn""#,
    )
    .unwrap()
});
static RE_H1: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<h1[^>]*>(.*?)</h1>").unwrap());
static RE_PLAYLIST_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"var\s+playlistId\s*=\s*"([^"]+)""#).unwrap());
static RE_PLAYLIST_COUNT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"var\s+itemsCount\s*=\s*(\d+)").unwrap());
static RE_PLAYLIST_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"var\s+token\s*=\s*"([^"]+)""#).unwrap());
/// A profile link in the signed-in header menu.
static RE_MENU_PROFILE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"href="/(?:users|model|channels|pornstar)/([^"/?#]+)"#).unwrap());

/// Which of the two sites a link belongs to: they share video ids but not sessions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Site {
    Pornhub,
    Premium,
}

impl Site {
    pub fn host(self) -> &'static str {
        match self {
            Site::Pornhub => "pornhub.com",
            Site::Premium => "pornhubpremium.com",
        }
    }

    fn base(self) -> String {
        format!("https://www.{}/", self.host())
    }

    fn of(host: &str) -> Option<Site> {
        let main = RE_HOST.captures(host)?[1].to_string();
        Some(if main.starts_with("pornhubpremium.") {
            Site::Premium
        } else {
            Site::Pornhub
        })
    }
}

/// The four kinds of profile the site lists videos under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileKind {
    Users,
    Channels,
    Model,
    Pornstar,
}

impl ProfileKind {
    fn segment(self) -> &'static str {
        match self {
            ProfileKind::Users => "users",
            ProfileKind::Channels => "channels",
            ProfileKind::Model => "model",
            ProfileKind::Pornstar => "pornstar",
        }
    }

    fn of(segment: &str) -> Option<ProfileKind> {
        Some(match segment {
            "users" => ProfileKind::Users,
            "channels" => ProfileKind::Channels,
            "model" => ProfileKind::Model,
            "pornstar" => ProfileKind::Pornstar,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Video {
        site: Site,
        id: String,
    },
    Playlist {
        site: Site,
        id: String,
    },
    /// The videos of a user, channel, model or pornstar, one page of them when the
    /// link names one.
    Profile {
        site: Site,
        kind: ProfileKind,
        name: String,
        page: Option<u32>,
    },
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let site = Site::of(&host)?;
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let video = |id: &str| {
        RE_VIDEO_ID.is_match(id).then(|| Link::Video {
            site,
            id: id.to_string(),
        })
    };
    if host.ends_with("modelhub.com") || host.ends_with("thumbzilla.com") {
        return match segments.as_slice() {
            ["video", id, ..] => video(id),
            [name] if host.ends_with("modelhub.com") && RE_NAME.is_match(name) => {
                Some(Link::Profile {
                    site,
                    kind: ProfileKind::Model,
                    name: name.to_string(),
                    page: None,
                })
            }
            _ => None,
        };
    }
    match segments.as_slice() {
        ["view_video.php"] | ["video", "show"] => video(&util::query_param(url, "viewkey")?),
        ["embed", id] => video(id),
        ["playlist", id] if id.chars().all(|c| c.is_ascii_digit()) => Some(Link::Playlist {
            site,
            id: id.to_string(),
        }),
        [kind, name, rest @ ..] if RE_NAME.is_match(name) => {
            let kind = ProfileKind::of(kind)?;
            let listing = match rest {
                [] | ["videos"] => true,
                ["videos", sub] => {
                    matches!(*sub, "upload" | "public" | "recent" | "paid" | "fanonly")
                }
                _ => false,
            };
            listing.then(|| Link::Profile {
                site,
                kind,
                name: name.to_string(),
                page: util::query_param(url, "page").and_then(|p| p.parse().ok()),
            })
        }
        _ => None,
    }
}

/// The player's `flashvars` object on a video page.
pub fn flashvars(html: &str) -> Option<Value> {
    let caps = RE_FLASHVARS.captures(html)?;
    let start = caps.get(1)?.start();
    page::leading_json(&html[start..]).map(|(value, _)| value)
}

/// The reason a video page shows no player, when it says.
pub fn page_error(html: &str) -> Option<String> {
    let caps = RE_PAGE_ERROR.captures(html)?;
    let raw = caps.get(1).or_else(|| caps.get(2))?.as_str();
    clean_title(&util::clean_html(raw).replace('\n', " "))
}

/// Whether the page says the video is withheld where the request came from.
pub fn is_geo_blocked(html: &str) -> bool {
    html.contains(r#"class="geoBlocked""#)
        || html.contains("This content is unavailable in your country")
}

/// The headers the video hosts want on every playlist, segment and file request: a
/// player on the site's own page.
fn media_headers(site: Site) -> Vec<(String, String)> {
    [
        ("origin", format!("https://www.{}", site.host())),
        ("referer", site.base()),
        ("sec-fetch-dest", "empty".to_string()),
        ("sec-fetch-mode", "cors".to_string()),
        ("sec-fetch-site", "cross-site".to_string()),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_string(), value))
    .collect()
}

/// The height a media definition's `quality` names, a number or a string of digits.
fn quality_height(value: &Value) -> Option<u32> {
    util::u32_of(value).or_else(|| util::text(value)?.trim().parse().ok())
}

/// The height and bit rate a stream's file name carries.
fn stream_name_facts(url: &Url) -> (Option<u32>, Option<u64>) {
    match RE_STREAM_NAME.captures(url.as_str()) {
        Some(caps) => (
            caps[1].parse().ok(),
            caps[2].parse::<u64>().ok().map(|k| k * 1000),
        ),
        None => (None, None),
    }
}

/// A variant for one quality of a video: an HLS master playlist or an MP4 file.
fn variant_of(url: Url, kind: VariantKind, quality: Option<u32>, site: Site) -> Variant {
    let (named_height, bitrate) = stream_name_facts(&url);
    let height = quality.filter(|h| *h > 0).or(named_height);
    let mut v = Variant::new(url, kind);
    v.height = height;
    v.bitrate = bitrate;
    v.video = Some(VideoCodec::H264);
    v.audio = Some(AudioCodec::Aac);
    v.headers = media_headers(site);
    let prefix = match kind {
        VariantKind::Hls => "hls",
        _ => "mp4",
    };
    if kind == VariantKind::File {
        v.container = Some(Container::Mp4);
    }
    v.label = height.map(|h| format!("{h}p"));
    v.format_id = Some(match height {
        Some(h) => format!("{prefix}-{h}"),
        None => prefix.to_string(),
    });
    v
}

/// The videos a listing page or chunk lists, each once: the part of the page after the
/// main container, so the header's drop-down menus of other videos are left out.
pub fn listing_entries(html: &str, site: Site) -> Vec<PlaylistEntry> {
    let text = match html.find("<div class=\"container") {
        Some(index) => &html[index..],
        None => html,
    };
    let starts: Vec<usize> = RE_ITEM.find_iter(text).map(|m| m.start()).collect();
    let mut entries: Vec<PlaylistEntry> = Vec::new();
    for (index, start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(text.len());
        let block = &text[*start..end];
        let link = RE_ITEM_LINK.captures(block);
        let key = match RE_ITEM_KEY
            .captures(block)
            .map(|c| c[1].to_string())
            .or_else(|| link.as_ref().map(|c| c[1].to_string()))
        {
            Some(key) => key,
            None => continue,
        };
        let url = match Url::parse(&format!("{}view_video.php?viewkey={key}", site.base())) {
            Ok(url) => url,
            Err(_) => continue,
        };
        if entries.iter().any(|e| e.url == url) {
            continue;
        }
        entries.push(PlaylistEntry {
            url,
            title: link
                .as_ref()
                .and_then(|c| clean_title(&util::html_unescape(&c[2]))),
            duration: RE_ITEM_DURATION
                .captures(block)
                .and_then(|c| util::parse_duration(&c[1])),
        });
    }
    entries
}

/// Whether a listing page links to a next page.
pub fn has_next_page(html: &str) -> bool {
    RE_NEXT_PAGE.is_match(html)
}

/// The page's heading, as the name of the listing.
fn heading(html: &str) -> Option<String> {
    RE_H1
        .captures(html)
        .and_then(|c| clean_title(&util::clean_html(&c[1]).replace('\n', " ")))
}

/// The account the signed-in header menu names when the page shows one; an error when
/// the menu is there but names nobody.
pub fn logged_in_account(html: &str) -> Result<Option<String>, String> {
    let signed_in =
        html.contains(r#"id="profileMenuDropdown""#) || html.contains(r#"class="ph-icon-logout""#);
    if !signed_in {
        return Ok(None);
    }
    let menu = util::element_by_id(html, "profileMenuDropdown");
    let account = RE_MENU_PROFILE
        .captures(menu.as_deref().unwrap_or(html))
        .map(|c| util::url_decode(&c[1]));
    match account {
        Some(account) => Ok(Some(account)),
        None => Err("the page shows a signed-in menu but names no account".to_string()),
    }
}

/// One page of a listing: its videos, its heading, and whether a page follows.
struct ListingPage {
    entries: Vec<PlaylistEntry>,
    title: Option<String>,
    more: bool,
}

fn listing_page(html: &str, site: Site) -> ListingPage {
    ListingPage {
        entries: listing_entries(html, site),
        title: heading(html),
        more: has_next_page(html),
    }
}

pub struct PornhubResolver {
    http: Http,
}

impl PornhubResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// A page of the site, read as a person browsing it: with the age gate cookies and
    /// the stored session. A page that turns into the login page needs a session.
    async fn page(
        &self,
        site: Site,
        page_url: &Url,
        origin: &Url,
    ) -> Result<Fetched, ResolveError> {
        let fetched = fetch(
            &self.http,
            page_url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        if fetched.url.path().contains("/login") {
            return Err(ResolveError::login_required(
                origin,
                PLATFORM,
                format!("{} shows this only to a signed-in member", site.host()),
            ));
        }
        Ok(fetched)
    }

    async fn resolve_video(
        &self,
        site: Site,
        id: &str,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let page_url =
            Url::parse(&format!("{}view_video.php?viewkey={id}", site.base())).expect("valid");
        let fetched = self.page(site, &page_url, url).await?;
        if fetched.status.is_success()
            && util::query_param(&fetched.url, "viewkey").as_deref() != Some(id)
        {
            return Err(match site {
                Site::Premium => ResolveError::login_required(
                    url,
                    PLATFORM,
                    "the video is shown only to a signed-in premium member",
                ),
                Site::Pornhub => {
                    ResolveError::unavailable(url, "the video was removed or is private")
                }
            });
        }
        let html = fetched.text();
        match fetched.status.as_u16() {
            200..=299 => {}
            404 | 410 => return Err(ResolveError::NotFound(url.clone())),
            429 => return Err(ResolveError::RateLimited(url.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    url,
                    page_error(&html).unwrap_or_else(|| format!("the site answered HTTP {status}")),
                ));
            }
        }
        if let Some(reason) = page_error(&html) {
            return Err(ResolveError::unavailable(url, reason));
        }
        if is_geo_blocked(&html) {
            return Err(ResolveError::unavailable(
                url,
                "the video is withheld in this country",
            ));
        }
        let flashvars = flashvars(&html)
            .ok_or_else(|| ResolveError::malformed(url, "the video page carries no player data"))?;
        let mut variants = Vec::new();
        for definition in flashvars["mediaDefinitions"]
            .as_array()
            .into_iter()
            .flatten()
        {
            let Some(media_url) = util::url_of(&definition["videoUrl"], None) else {
                continue;
            };
            let quality = quality_height(&definition["quality"]);
            match definition["format"].as_str() {
                Some("hls") => {
                    variants.push(variant_of(media_url, VariantKind::Hls, quality, site));
                }
                Some("mp4") if media_url.path().contains("/get_media") => {
                    variants.extend(self.media_files(&media_url, &page_url, url).await?);
                }
                Some("mp4") => {
                    variants.push(variant_of(media_url, VariantKind::File, quality, site));
                }
                _ => {}
            }
        }
        variants.dedup_by(|a, b| a.url == b.url);
        if variants.is_empty() {
            return Err(if html.contains(r#"id="lockedPlayer"#) {
                match site {
                    Site::Premium => ResolveError::login_required(
                        url,
                        PLATFORM,
                        "the video plays only for a signed-in premium member",
                    ),
                    Site::Pornhub => ResolveError::unavailable(
                        url,
                        "the video is locked: it plays only for paying members",
                    ),
                }
            } else {
                ResolveError::unavailable(url, "the player names no stream")
            });
        }
        let page = Page::parse(&html, &page_url);
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.to_string());
        resolved.title = util::text(&flashvars["video_title"])
            .and_then(|t| clean_title(&util::html_unescape(&t)))
            .or_else(|| page.meta("twitter:title").and_then(|t| clean_title(&t)))
            .or_else(|| page.meta("og:title").and_then(|t| clean_title(&t)));
        if let Some(caps) = RE_UPLOADER.captures(&html) {
            resolved.uploader = clean_title(&util::html_unescape(&caps[2]));
            resolved.uploader_url = Url::parse(&site.base())
                .ok()
                .and_then(|b| b.join(&caps[1]).ok());
        } else if let Some(profile) = page::json_after(&html, "var MODEL_PROFILE = ") {
            resolved.uploader = util::text(&profile["username"]).and_then(|u| clean_title(&u));
            resolved.uploader_url = util::text(&profile["modelProfileLink"])
                .and_then(|link| Url::parse(&site.base()).ok()?.join(&link).ok());
        }
        resolved.uploaded_at = page::ld_objects_of_type(&page.ld_json(), "VideoObject")
            .into_iter()
            .find_map(|object| util::time(&object["uploadDate"]))
            .or_else(|| {
                variants
                    .iter()
                    .find_map(|v| RE_UPLOAD_DAY.captures(v.url.as_str()))
                    .and_then(|c| util::parse_date(&format!("{}-{}-{}", &c[1], &c[2], &c[3])))
            });
        resolved.duration = util::seconds(&flashvars["video_duration"]);
        resolved.thumbnail = util::url_of(&flashvars["image_url"], None)
            .or_else(|| page.meta("og:image").and_then(|u| Url::parse(&u).ok()));
        resolved.webpage_url = Some(page_url.clone());
        resolved.age_limit = Some(18);
        if let Some(captions) = util::url_of(&flashvars["closedCaptionsFile"], None) {
            resolved.subtitles.push(SubtitleTrack {
                url: captions,
                language: "en".to_string(),
                name: None,
                format: SubtitleFormat::Srt,
                auto: false,
                headers: media_headers(site),
            });
        }
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    /// The MP4 file of each quality, from the `get_media` link the page's session may ask.
    async fn media_files(
        &self,
        api: &Url,
        page_url: &Url,
        origin: &Url,
    ) -> Result<Vec<Variant>, ResolveError> {
        let site = if api
            .host_str()
            .is_some_and(|h| h.ends_with("pornhubpremium.com"))
        {
            Site::Premium
        } else {
            Site::Pornhub
        };
        let response = self
            .http
            .get(api.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "application/json, text/javascript, */*; q=0.01")
            .header("referer", page_url.as_str())
            .header("x-requested-with", "XMLHttpRequest")
            .send()
            .await?;
        if let Some(error) = status_error(response.status, origin) {
            return Err(error);
        }
        let files: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, format!("media list: {e}")))?;
        Ok(files
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|file| {
                let file_url = util::url_of(&file["videoUrl"], None)?;
                Some(variant_of(
                    file_url,
                    VariantKind::File,
                    quality_height(&file["quality"]),
                    site,
                ))
            })
            .collect())
    }

    /// One page of a profile's videos, at `/videos` when the profile has that tab and
    /// on the profile page itself when it has not.
    async fn profile_page(
        &self,
        site: Site,
        listing: &Url,
        profile: &Url,
        number: u32,
        origin: &Url,
    ) -> Result<Option<ListingPage>, ResolveError> {
        let page_number = number.to_string();
        for (index, base) in [listing, profile].into_iter().enumerate() {
            let page_url = util::with_query(base, &[("page", &page_number)]);
            let fetched = self.page(site, &page_url, origin).await?;
            match fetched.status.as_u16() {
                200..=299 => return Ok(Some(listing_page(&fetched.text(), site))),
                404 | 410 if index == 0 && number == 1 => continue,
                404 | 410 if number == 1 => return Err(ResolveError::NotFound(origin.clone())),
                404 | 410 => return Ok(None),
                429 => return Err(ResolveError::RateLimited(origin.clone())),
                status => {
                    return Err(ResolveError::unavailable(
                        origin,
                        format!("the site answered HTTP {status}"),
                    ));
                }
            }
        }
        Err(ResolveError::NotFound(origin.clone()))
    }

    async fn resolve_profile(
        &self,
        site: Site,
        kind: ProfileKind,
        name: &str,
        page: Option<u32>,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let profile =
            Url::parse(&format!("{}{}/{name}", site.base(), kind.segment())).expect("valid");
        let listing = Url::parse(&format!("{profile}/videos")).expect("valid");
        let mut entries: Vec<PlaylistEntry> = Vec::new();
        let mut title = None;
        let first = page.unwrap_or(1);
        let last = match page {
            Some(only) => only,
            None => first + PAGE_LIMIT - 1,
        };
        for number in first..=last {
            let Some(found) = self
                .profile_page(site, &listing, &profile, number, url)
                .await?
            else {
                break;
            };
            title = title.or(found.title);
            if found.entries.is_empty() {
                break;
            }
            for entry in found.entries {
                if !entries.iter().any(|e| e.url == entry.url) {
                    entries.push(entry);
                }
            }
            if !found.more || entries.len() >= LISTING_LIMIT {
                break;
            }
        }
        entries.truncate(LISTING_LIMIT);
        if entries.is_empty() {
            return Err(ResolveError::unavailable(
                url,
                format!("{name} lists no videos"),
            ));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(format!("{}/{name}", kind.segment())),
            title: Some(title.unwrap_or_else(|| name.to_string())),
            total: None,
            entries,
        }))
    }

    async fn resolve_playlist(
        &self,
        site: Site,
        id: &str,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let page_url = Url::parse(&format!("{}playlist/{id}", site.base())).expect("valid");
        let fetched = self.page(site, &page_url, url).await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        let html = fetched.text();
        let mut entries = listing_entries(&html, site);
        let title = heading(&html);
        let count = RE_PLAYLIST_COUNT
            .captures(&html)
            .and_then(|c| c[1].parse::<usize>().ok());
        let playlist_id = util::search(&RE_PLAYLIST_ID, &html).unwrap_or_else(|| id.to_string());
        let token = util::search(&RE_PLAYLIST_TOKEN, &html);
        let chunks = Url::parse(&format!("{}playlist/viewChunked", site.base())).expect("valid");
        let mut number = 2;
        while let Some(token) = token.as_deref()
            && count.is_none_or(|n| entries.len() < n)
            && entries.len() < LISTING_LIMIT
            && number <= PAGE_LIMIT
        {
            let chunk_url = util::with_query(
                &chunks,
                &[
                    ("id", playlist_id.as_str()),
                    ("page", &number.to_string()),
                    ("token", token),
                ],
            );
            let response = self
                .http
                .get(chunk_url)
                .platform(PLATFORM)
                .user_agent(BROWSER_UA)
                .header("referer", page_url.as_str())
                .header("x-requested-with", "XMLHttpRequest")
                .send()
                .await?;
            if let Some(error) = status_error(response.status, url) {
                return Err(error);
            }
            let chunk = response.text(MAX_PAGE).await?;
            let found = listing_entries(&chunk, site);
            if found.is_empty() {
                break;
            }
            for entry in found {
                if !entries.iter().any(|e| e.url == entry.url) {
                    entries.push(entry);
                }
            }
            number += 1;
        }
        entries.truncate(LISTING_LIMIT);
        if entries.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(id.to_string()),
            title,
            total: count.or(Some(entries.len())),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for PornhubResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Pornhub",
            hosts: &[
                "pornhub.com",
                "pornhub.org",
                "pornhub.net",
                "pornhubpremium.com",
                "modelhub.com",
                "thumbzilla.com",
            ],
            features: &[
                "videos",
                "embeds",
                "playlists",
                "channels",
                "models",
                "pornstars",
                "users",
                "premium",
            ],
            formats: &["mp4", "hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::Nsfw, Tag::Video],
            session: SessionSupport::Optional,
            examples: &[
                "https://www.pornhub.com/view_video.php?viewkey=6a50ebd7cb3ff",
                "https://www.pornhub.com/embed/6a50ebd7cb3ff",
                "https://www.pornhub.org/view_video.php?viewkey=ph601dc30bae19a",
                "https://www.pornhub.com/playlist/44121572",
                "https://www.pornhub.com/channels/povd",
                "https://www.pornhub.com/model/zoe_ph/videos",
                "https://www.pornhub.com/pornstar/liz-vicious",
                "https://www.pornhub.com/users/babes-com/videos",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Video { site, id } => self.resolve_video(site, &id, url).await,
            Link::Playlist { site, id } => self.resolve_playlist(site, &id, url).await,
            Link::Profile {
                site,
                kind,
                name,
                page,
            } => self.resolve_profile(site, kind, &name, page, url).await,
        }
    }

    /// The age gate and the region disclaimer are passed with the cookies the site sets
    /// when a person clicks through them, on both sites.
    fn consent_cookies(&self) -> Vec<Cookie> {
        ["pornhub.com", "pornhubpremium.com"]
            .into_iter()
            .flat_map(|domain| {
                [
                    ("age_verified", "1"),
                    ("accessAgeDisclaimerPH", "1"),
                    ("accessAgeDisclaimerUK", "1"),
                    ("accessPH", "1"),
                    ("platform", "pc"),
                ]
                .into_iter()
                .map(move |(name, value)| Cookie::new(name, value, domain))
            })
            .collect()
    }

    /// Whether the stored cookies sign in on either site: the front page then shows the
    /// member menu with the account's profile link.
    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        let jar = self.http.jar(PLATFORM);
        for site in [Site::Pornhub, Site::Premium] {
            let home = Url::parse(&site.base()).expect("valid");
            if jar.header_for(&home, Timestamp::now()).is_none() {
                continue;
            }
            let fetched = fetch(
                &self.http,
                &home,
                PLATFORM,
                BROWSER_UA,
                &navigation_headers(),
                MAX_PAGE,
            )
            .await?;
            if let Some(error) = status_error(fetched.status, &home) {
                return Err(error);
            }
            match logged_in_account(&fetched.text()) {
                Ok(Some(account)) => return Ok(SessionCheck::LoggedIn { account }),
                Ok(None) => {}
                Err(detail) => return Err(ResolveError::malformed(&home, detail)),
            }
        }
        Ok(SessionCheck::LoggedOut)
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

    fn redirected(url: &str, final_url: &str, body: &str) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: "GET".into(),
                url: url.into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status: 200,
                url: final_url.into(),
                headers: vec![("content-type".into(), "text/html".into())],
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let video = |site: Site, id: &str| {
            Some(Link::Video {
                site,
                id: id.into(),
            })
        };
        assert_eq!(
            link("https://www.pornhub.com/view_video.php?viewkey=ph601dc30bae19a"),
            video(Site::Pornhub, "ph601dc30bae19a")
        );
        assert_eq!(
            link("http://www.pornhub.com/video/show?viewkey=648719015"),
            video(Site::Pornhub, "648719015")
        );
        assert_eq!(
            link("https://de.pornhub.org/embed/6a50ebd7cb3ff"),
            video(Site::Pornhub, "6a50ebd7cb3ff")
        );
        assert_eq!(
            link("https://www.pornhub.net/view_video.php?viewkey=203640933"),
            video(Site::Pornhub, "203640933")
        );
        assert_eq!(
            link("https://www.pornhubpremium.com/view_video.php?viewkey=ph5e4acdae54a82"),
            video(Site::Premium, "ph5e4acdae54a82")
        );
        assert_eq!(
            link("https://www.modelhub.com/video/ph601dc30bae19a"),
            video(Site::Pornhub, "ph601dc30bae19a")
        );
        assert_eq!(
            link("https://www.thumbzilla.com/video/ph56c6114abd99a/horny-girlfriend-sex"),
            video(Site::Pornhub, "ph56c6114abd99a")
        );
        assert_eq!(
            link("https://www.pornhub.com/playlist/44121572"),
            Some(Link::Playlist {
                site: Site::Pornhub,
                id: "44121572".into()
            })
        );
        assert_eq!(
            link("https://www.pornhub.com/model/zoe_ph/videos?page=3"),
            Some(Link::Profile {
                site: Site::Pornhub,
                kind: ProfileKind::Model,
                name: "zoe_ph".into(),
                page: Some(3)
            })
        );
        assert_eq!(
            link("https://www.pornhub.com/pornstar/liz-vicious"),
            Some(Link::Profile {
                site: Site::Pornhub,
                kind: ProfileKind::Pornstar,
                name: "liz-vicious".into(),
                page: None
            })
        );
        assert_eq!(
            link("https://www.pornhub.com/channels/povd/videos/upload"),
            Some(Link::Profile {
                site: Site::Pornhub,
                kind: ProfileKind::Channels,
                name: "povd".into(),
                page: None
            })
        );
        assert_eq!(
            link("https://www.pornhub.com/users/babes-com"),
            Some(Link::Profile {
                site: Site::Pornhub,
                kind: ProfileKind::Users,
                name: "babes-com".into(),
                page: None
            })
        );
        assert_eq!(
            link("https://www.modelhub.com/zoe_ph"),
            Some(Link::Profile {
                site: Site::Pornhub,
                kind: ProfileKind::Model,
                name: "zoe_ph".into(),
                page: None
            })
        );
        assert_eq!(link("https://www.pornhub.com/"), None);
        assert_eq!(link("https://www.pornhub.com/video/search?search=x"), None);
        assert_eq!(link("https://www.pornhub.com/categories/teen"), None);
        assert_eq!(link("https://www.pornhub.com/model/zoe_ph/about"), None);
        assert_eq!(link("https://www.pornhub.com/view_video.php"), None);
        assert_eq!(link("https://example.com/view_video.php?viewkey=abc"), None);
    }

    fn video_page(hls: &[(u32, &str)], get_media: &str) -> String {
        let mut definitions: Vec<Value> = hls
            .iter()
            .map(|(quality, url)| {
                json!({"group": "1", "height": "0", "width": "0", "defaultQuality": "False", "format": "hls", "videoUrl": url, "quality": quality.to_string()})
            })
            .collect();
        definitions.push(json!({"group": "1", "format": "mp4", "videoUrl": get_media, "quality": "[]", "remote": "True"}));
        let flashvars = json!({
            "video_title": "\"Welcome to My Pussy Mansion\" - CB Stream (02/03/21)",
            "image_url": "https://ei.phncdn.com/videos/202102/05/383080302/thumbs_60/7.jpg",
            "video_duration": 8173,
            "closedCaptionsFile": "https://cc.phncdn.com/captions/383080302.srt",
            "mediaDefinitions": definitions,
            "video_unavailable": false
        });
        format!(
            concat!(
                "<html><head><title>x</title>",
                "<script type=\"application/ld+json\">{{\"@type\": \"VideoObject\", \"uploadDate\": \"2021-02-05T22:42:12+00:00\"}}</script>",
                "</head><body><div class=\"container\"><script>var flashvars_383080302 = {};</script>",
                "<div class=\"video-detailed-info\"><div class=\"userInfo\"><div class=\"usernameWrap\">",
                "<a rel=\"\" href=\"/model/projekt-melody\" class=\"bolded\">Projekt Melody</a></div></div></div>",
                "</div></body></html>"
            ),
            flashvars
        )
    }

    #[tokio::test]
    async fn videos_resolve_their_hls_and_mp4_qualities() {
        let get_media = "https://www.pornhub.com/video/get_media?s=abc&v=ph601dc30bae19a&e=0&t=p";
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "https://www.pornhub.com/view_video.php?viewkey=ph601dc30bae19a",
            200,
            "text/html",
            &video_page(
                &[
                    (1080, "https://hv-h.phncdn.com/hls/videos/202102/05/383080302/1080P_8000K_383080302.mp4/master.m3u8?h=a"),
                    (720, "https://hv-h.phncdn.com/hls/videos/202102/05/383080302/720P_4000K_383080302.mp4/master.m3u8?h=b"),
                ],
                get_media,
            ),
        ));
        fixture.exchanges.push(exchange(
            get_media,
            200,
            "application/json",
            &json!([
                {"format": "mp4", "videoUrl": "https://ev.phncdn.com/videos/202102/05/383080302/720P_4000K_383080302.mp4?validfrom=1&hash=x", "quality": "720"},
                {"format": "mp4", "videoUrl": "https://ev.phncdn.com/videos/202102/05/383080302/1080P_8000K_383080302.mp4?validfrom=1&hash=y", "quality": "1080"}
            ])
            .to_string(),
        ));
        let resolver = PornhubResolver::new(Http::replay(fixture));
        let url =
            Url::parse("https://www.pornhub.com/view_video.php?viewkey=ph601dc30bae19a").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("ph601dc30bae19a"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("\"Welcome to My Pussy Mansion\" - CB Stream (02/03/21)")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("Projekt Melody"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://www.pornhub.com/model/projekt-melody"
        );
        assert_eq!(
            resolved.duration,
            Some(std::time::Duration::from_secs(8173))
        );
        assert_eq!(
            resolved.uploaded_at.unwrap().to_string(),
            "2021-02-05T22:42:12Z"
        );
        assert_eq!(resolved.age_limit, Some(18));
        assert_eq!(resolved.subtitles.len(), 1);
        assert_eq!(resolved.subtitles[0].format, SubtitleFormat::Srt);
        assert_eq!(resolved.variants.len(), 4);
        let hls = &resolved.variants[0];
        assert_eq!(hls.kind, VariantKind::Hls);
        assert_eq!(hls.height, Some(1080));
        assert_eq!(hls.bitrate, Some(8_000_000));
        assert_eq!(hls.format_id.as_deref(), Some("hls-1080"));
        assert!(
            hls.headers
                .iter()
                .any(|(k, v)| k == "origin" && v == "https://www.pornhub.com")
        );
        assert!(
            hls.headers
                .iter()
                .any(|(k, v)| k == "sec-fetch-mode" && v == "cors")
        );
        let mp4 = &resolved.variants[2];
        assert_eq!(mp4.kind, VariantKind::File);
        assert_eq!(mp4.container, Some(Container::Mp4));
        assert_eq!(mp4.height, Some(720));
        assert_eq!(mp4.bitrate, Some(4_000_000));
        assert_eq!(mp4.label.as_deref(), Some("720p"));
        assert_eq!(mp4.format_id.as_deref(), Some("mp4-720"));
        assert_eq!(resolved.variants[3].height, Some(1080));
    }

    #[tokio::test]
    async fn missing_removed_private_and_premium_videos_say_why() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "https://www.pornhub.com/view_video.php?viewkey=zzz1",
            404,
            "text/html",
            "<html><title>Page Not Found</title></html>",
        ));
        fixture.exchanges.push(exchange(
            "https://www.pornhub.com/view_video.php?viewkey=ph5a9813bfa7156",
            200,
            "text/html",
            r#"<html><section class="noVideo"><div class="removed"><p>This video has been disabled</p></div></section></html>"#,
        ));
        fixture.exchanges.push(redirected(
            "https://www.pornhub.com/view_video.php?viewkey=ph56fd731fce6b7",
            "https://www.pornhub.com/",
            "<html>home</html>",
        ));
        fixture.exchanges.push(redirected(
            "https://www.pornhubpremium.com/view_video.php?viewkey=ph5e4acdae54a82",
            "https://www.pornhubpremium.com/premium/login?redirect=x",
            "<html>login</html>",
        ));
        let resolver = PornhubResolver::new(Http::replay(fixture));
        let url = |s: &str| Url::parse(s).unwrap();
        assert!(matches!(
            resolver
                .resolve(&url("https://www.pornhub.com/view_video.php?viewkey=zzz1"))
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(&url(
                "https://www.pornhub.com/view_video.php?viewkey=ph5a9813bfa7156",
            ))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "This video has been disabled"),
            "{error}"
        );
        let error = resolver
            .resolve(&url(
                "https://www.pornhub.com/view_video.php?viewkey=ph56fd731fce6b7",
            ))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("removed or is private")),
            "{error}"
        );
        let error = resolver
            .resolve(&url(
                "https://www.pornhubpremium.com/view_video.php?viewkey=ph5e4acdae54a82",
            ))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::LoginRequired { platform, .. } if *platform == PLATFORM),
            "{error}"
        );
    }

    fn item(key: &str, title: &str, duration: &str) -> String {
        format!(
            concat!(
                r#"<li class="pcVideoListItem js-pop videoblock videoBox" id="v1" data-video-id="1" data-video-vkey="{key}">"#,
                r#"<div class="phimage"><a href="/view_video.php?viewkey={key}" title="{title}" class="linkVideoThumb">"#,
                r#"<img src="x.jpg" alt="{title}" title="{title}"/><div class="marker-overlays"><var class="duration">{duration}</var></div></a></div></li>"#
            ),
            key = key,
            title = title,
            duration = duration
        )
    }

    #[tokio::test]
    async fn profiles_list_their_videos_across_pages_without_the_menus() {
        let menu = format!(
            r#"<ul class="dropdownHottestVideos videos" id="hottestMenuSection">{}</ul>"#,
            item("menu1", "Hot in the menu", "1:00")
        );
        let page1 = format!(
            r#"<html>{menu}<div class="container"><h1>POVD</h1><span class="videosCount">83</span><ul id="showAllChanelVideos">{}{}</ul><li class="page_next"><a href="?page=2">Next</a></li></div></html>"#,
            item("6a92c17961adc", "My Wife Let Me &amp; Her", "11:53"),
            item("6a946be71430b", "Hot Egirl", "10:33")
        );
        let page2 = format!(
            r#"<html>{menu}<div class="container"><h1>POVD</h1><ul>{}</ul></div></html>"#,
            item("66dc02c87abaf", "Third", "12:47")
        );
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "https://www.pornhub.com/channels/povd/videos?page=1",
            200,
            "text/html",
            &page1,
        ));
        fixture.exchanges.push(exchange(
            "https://www.pornhub.com/channels/povd/videos?page=2",
            200,
            "text/html",
            &page2,
        ));
        let resolver = PornhubResolver::new(Http::replay(fixture));
        let Resolution::Playlist(playlist) = resolver
            .resolve(&Url::parse("https://www.pornhub.com/channels/povd").unwrap())
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(playlist.title.as_deref(), Some("POVD"));
        assert_eq!(playlist.total, None);
        assert_eq!(playlist.entries.len(), 3);
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://www.pornhub.com/view_video.php?viewkey=6a92c17961adc"
        );
        assert_eq!(
            playlist.entries[0].title.as_deref(),
            Some("My Wife Let Me & Her")
        );
        assert_eq!(
            playlist.entries[0].duration,
            Some(std::time::Duration::from_secs(713))
        );
        assert_eq!(playlist.entries[2].title.as_deref(), Some("Third"));
    }

    #[tokio::test]
    async fn playlists_read_their_further_chunks_with_the_page_token() {
        let page = format!(
            r#"<html><script>var playlistId = "44121572"; var itemsCount = 3 || 0; var token = "tok";</script><div class="container"><h1>Full Videos</h1><ul id="videoPlaylist">{}{}</ul></div></html>"#,
            item("68f71058504a6", "First", "12:31"),
            item("6aa5d599c93d2", "Second", "15:55")
        );
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "https://www.pornhub.com/playlist/44121572",
            200,
            "text/html",
            &page,
        ));
        fixture.exchanges.push(exchange(
            "https://www.pornhub.com/playlist/viewChunked?id=44121572&page=2&token=tok",
            200,
            "text/html",
            &item("6a92c17961adc", "Third", "2:49"),
        ));
        let resolver = PornhubResolver::new(Http::replay(fixture));
        let Resolution::Playlist(playlist) = resolver
            .resolve(&Url::parse("https://www.pornhub.com/playlist/44121572").unwrap())
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(playlist.id.as_deref(), Some("44121572"));
        assert_eq!(playlist.title.as_deref(), Some("Full Videos"));
        assert_eq!(playlist.total, Some(3));
        assert_eq!(playlist.entries.len(), 3);
        assert_eq!(playlist.entries[2].title.as_deref(), Some("Third"));
    }

    #[tokio::test]
    async fn sessions_are_checked_on_the_front_page() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "https://www.pornhub.com/",
            200,
            "text/html",
            r#"<html><div id="profileMenuDropdown"><a href="/users/someone-42?tab=x">My Profile</a></div></html>"#,
        ));
        let http = Http::replay(fixture);
        let resolver = PornhubResolver::new(http.clone());
        assert_eq!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedOut
        );
        http.with_jar(PLATFORM, |jar| {
            jar.insert(Cookie::new("il", "v1abc", "pornhub.com"));
        });
        assert_eq!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedIn {
                account: "someone-42".into()
            }
        );
        assert_eq!(resolver.consent_cookies().len(), 10);
        assert!(
            logged_in_account("<html>signed out</html>")
                .unwrap()
                .is_none()
        );
        assert!(logged_in_account(r#"<span class="ph-icon-logout"></span>"#).is_err());
    }

    /// Every example link resolves live: videos with a playable MP4 and HLS variant,
    /// listings with entries.
    #[tokio::test]
    #[ignore = "requires live Pornhub access"]
    async fn live_examples_resolve() {
        use std::time::Duration;

        let http = Http::new(crate::http::HttpConfig::default());
        let resolver = PornhubResolver::new(http.clone());
        http.seed_cookies(PLATFORM, resolver.consent_cookies());
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
                    assert!(
                        resolved
                            .variants
                            .iter()
                            .any(|v| v.kind == VariantKind::File),
                        "{link}: no MP4"
                    );
                    assert!(
                        resolved.variants.iter().any(|v| v.kind == VariantKind::Hls),
                        "{link}: no HLS"
                    );
                    println!(
                        "{link}: {:?} with {} variants",
                        resolved.title,
                        resolved.variants.len()
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
}
