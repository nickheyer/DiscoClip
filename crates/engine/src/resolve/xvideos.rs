//! XVideos videos, channels and profiles. A video page hands its player the HLS playlist
//! in an `html5player.setVideoHLS` call, the title, thumbnails and uploader in the calls
//! beside it, and the upload date, length and description in the page's JSON-LD.
//! Channels, profiles, pornstar and model pages list their uploads through the JSON the
//! site's own listing pages page through. XNXX runs the same player on its own pages and
//! reads them with the helpers here.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionSupport, Tag, Variant, clean_title, fetch, hls, navigation_headers,
    status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::MediaKind;

pub const PLATFORM: &str = "xvideos";
const SITE: &str = "https://www.xvideos.com";
/// How many videos a channel or profile listing is read up to: three listing pages.
pub const LISTING_LIMIT: usize = 108;

/// `xvideos.com` with its country subdomains, `xvideos2.com`, `xvideos3.com`,
/// `xvideos.es` and the premium `xvideos.red`.
static RE_HOST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[a-z0-9-]+\.)?xvideos[23]?\.(?:com|es|red)$").unwrap());
/// `/video{digits}/slug` and `/video.{encoded id}/slug`.
static RE_VIDEO_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/video\.?([0-9a-z]+)(?:/|$)").unwrap());
/// `/embedframe/{id}`.
static RE_EMBED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/embedframe/([0-9a-z]+)/?$").unwrap());
/// `/prof-video-click/upload/{name}/{id}/slug`: how listings link their videos.
static RE_PROF_CLICK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/prof-video-click/[a-z]+/[^/]+/([0-9a-z]+)(?:/|$)").unwrap());
/// `#quickies/a/{id}`: a short video opened on a profile page.
static RE_QUICKIES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^quickies/a/([0-9a-z]+)").unwrap());
/// `/channels/{name}`, `/profiles/{name}`, and the pornstar, model and amateur channel
/// paths that all redirect to `/channels/{name}`.
static RE_LISTING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^/(?:channels|profiles|amateur-channels|pornstar-channels|model-channels|pornstars|models)/([A-Za-z0-9_.-]+)/?$",
    )
    .unwrap()
});
/// `/{name}`: the short form of a channel link, as the site writes it.
static RE_BARE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/([A-Za-z0-9_-]{3,})/?$").unwrap());

/// `html5player.setVideoHLS('…')`: the playlist the player loads.
static RE_SET_HLS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"html5player\.setVideoHLS\s*\(\s*['"]((?:https?:)?//[^'"]+)['"]"#).unwrap()
});
/// `html5player.setX('…')`: the player's other settings, by name.
static RE_SET_STRING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"html5player\.set([A-Za-z0-9]+)\s*\(\s*'((?:[^'\\]|\\.)*)'\s*\)"#).unwrap()
});
/// `<h1 class="inlineError">…</h1>`: why a page shows no video.
static RE_INLINE_ERROR: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?s)<h1 class="inlineError">(.+?)</h1>"#).unwrap());

/// Site sections whose single-segment paths are not channel names.
const SITE_SECTIONS: &[&str] = &[
    "new",
    "best",
    "verified",
    "tags",
    "channels",
    "pornstars",
    "models",
    "profiles",
    "lang",
    "login",
    "account",
    "search",
    "upload",
    "webmaster",
    "premium",
    "legal",
    "terms",
    "privacy",
    "dmca",
    "contact",
    "faq",
    "gay",
    "shemale",
    "amateur",
    "history",
    "favorites",
    "quickies",
    "top",
    "index",
    "categories",
    "change-country",
    "ads",
    "blog",
    "stats",
    "tools",
    "help",
    "sitemap",
    "info",
    "signup",
    "todays-selection",
    "best-of",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Video {
        id: String,
    },
    /// A channel's or a profile's uploads, by the name in its path.
    Listing {
        name: String,
    },
}

/// Whether a single path segment names a channel rather than a section of the site.
fn is_channel_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    !SITE_SECTIONS.contains(&lower.as_str())
        && !lower.starts_with("porn-in-")
        && !lower.ends_with("-index")
        && !lower.starts_with("video")
}

/// The link's meaning on a host `hosts` matches (a regex over the lower-cased host).
pub fn parse_link_on(url: &Url, hosts: &Regex) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !hosts.is_match(&host) {
        return None;
    }
    let path = url.path();
    if let Some(fragment) = url.fragment()
        && let Some(caps) = RE_QUICKIES.captures(fragment)
    {
        return Some(Link::Video {
            id: caps[1].to_string(),
        });
    }
    if let Some(caps) = RE_VIDEO_PATH
        .captures(path)
        .or_else(|| RE_EMBED.captures(path))
        .or_else(|| RE_PROF_CLICK.captures(path))
    {
        return Some(Link::Video {
            id: caps[1].to_string(),
        });
    }
    if path.starts_with("/swf/")
        && let Some(id) = util::query_param(url, "id_video")
        && id.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return Some(Link::Video { id });
    }
    if let Some(caps) = RE_LISTING.captures(path) {
        return Some(Link::Listing {
            name: caps[1].to_string(),
        });
    }
    if let Some(caps) = RE_BARE.captures(path)
        && is_channel_name(&caps[1])
    {
        return Some(Link::Listing {
            name: caps[1].to_string(),
        });
    }
    None
}

pub fn parse_link(url: &Url) -> Option<Link> {
    parse_link_on(url, &RE_HOST)
}

/// The page of a video: `/video.{id}/_` for encoded ids, `/video{id}/_` for the older
/// numeric ones, which the site redirects to the encoded form.
fn video_page(site: &str, id: &str) -> Url {
    let separator = if id.chars().all(|c| c.is_ascii_digit()) {
        ""
    } else {
        "."
    };
    Url::parse(&format!("{site}/video{separator}{id}/_")).expect("valid")
}

/// What a video page tells its player.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PlayerData {
    /// The video's encoded id, from `setEncodedIdVideo`.
    pub id: Option<String>,
    /// From `setVideoTitle`, HTML entities decoded.
    pub title: Option<String>,
    /// The HLS playlist, from `setVideoHLS`.
    pub hls: Option<Url>,
    /// The 16:9 thumbnail, else the 4:3 one.
    pub thumbnail: Option<Url>,
    /// From `setUploaderName`.
    pub uploader: Option<String>,
}

/// A JS single-quoted string's escapes undone.
fn js_unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(other) => out.push(other),
                None => {}
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// The player's settings on a page.
pub fn player_data(html: &str) -> PlayerData {
    let mut data = PlayerData {
        hls: RE_SET_HLS
            .captures(html)
            .and_then(|caps| util::join_url(None, &caps[1])),
        ..PlayerData::default()
    };
    let mut thumb_43 = None;
    for caps in RE_SET_STRING.captures_iter(html) {
        let value = js_unescape(&caps[2]);
        match &caps[1] {
            "EncodedIdVideo" => data.id = clean_title(&value),
            "VideoTitle" => data.title = clean_title(&util::html_unescape(&value)),
            "UploaderName" => data.uploader = clean_title(&value),
            "ThumbUrl169" => data.thumbnail = util::join_url(None, &value),
            "ThumbUrl" => thumb_43 = util::join_url(None, &value),
            _ => {}
        }
    }
    if data.thumbnail.is_none() {
        data.thumbnail = thumb_43;
    }
    data
}

/// The reason a page shows in place of a video, when it shows one.
pub fn page_error(html: &str) -> Option<String> {
    let caps = RE_INLINE_ERROR.captures(html)?;
    clean_title(&util::clean_html(&caps[1]))
}

/// The variants a video page's player is handed: the playlist's renditions, tallest
/// first.
pub async fn player_variants(
    http: &Http,
    platform: &str,
    playlist: &Url,
    origin: &Url,
) -> Result<Vec<Variant>, ResolveError> {
    let expanded = hls::expand(http, playlist, platform, BROWSER_UA, &[])
        .await
        .map_err(|error| error.at(origin))?;
    let mut variants = expanded.variants;
    for variant in &mut variants {
        variant.format_id = Some(match &variant.label {
            Some(label) => format!("hls-{label}"),
            None => "hls".to_string(),
        });
    }
    variants.sort_by_key(|v| std::cmp::Reverse(v.height));
    Ok(variants)
}

/// The JSON-LD `VideoObject` on the page, when it carries one.
fn ld_video(page: &Page) -> Option<Value> {
    let blocks = page.ld_json();
    super::page::ld_objects_of_type(&blocks, "VideoObject")
        .into_iter()
        .next()
        .cloned()
}

/// Reads the player page at `page_url` for `platform`, filling everything a video's
/// resolution carries but the uploader's link, which `uploader_url` makes from the
/// uploader's name.
pub async fn resolve_player_page(
    http: &Http,
    platform: &str,
    page_url: &Url,
    origin: &Url,
    uploader_url: &(dyn Fn(&str) -> Option<Url> + Sync),
) -> Result<Resolved, ResolveError> {
    let fetched = fetch(
        http,
        page_url,
        platform,
        BROWSER_UA,
        &navigation_headers(),
        MAX_PAGE,
    )
    .await?;
    if let Some(error) = status_error(fetched.status, origin) {
        return Err(error);
    }
    let html = fetched.text();
    let data = player_data(&html);
    let Some(playlist) = &data.hls else {
        return Err(match page_error(&html) {
            Some(reason) => ResolveError::unavailable(origin, reason),
            None => ResolveError::unavailable(origin, "the page hands its player no video"),
        });
    };
    let variants = player_variants(http, platform, playlist, origin).await?;
    let page = Page::parse(&html, &fetched.url);
    let ld = ld_video(&page);
    let mut resolved = Resolved::new(platform);
    resolved.id = data.id.clone();
    resolved.title = page
        .meta("og:title")
        .and_then(|t| clean_title(&t))
        .or_else(|| data.title.clone());
    resolved.description = ld
        .as_ref()
        .and_then(|ld| util::text(&ld["description"]))
        .and_then(|d| clean_title(&util::html_unescape(&d)))
        .filter(|d| Some(d) != resolved.title.as_ref());
    resolved.uploader = data.uploader.clone();
    resolved.uploader_url = data.uploader.as_deref().and_then(uploader_url);
    resolved.uploaded_at = ld
        .as_ref()
        .and_then(|ld| util::text(&ld["uploadDate"]))
        .and_then(|date| util::parse_timestamp(&date));
    resolved.duration = page
        .meta("og:duration")
        .and_then(|d| d.trim().parse::<f64>().ok())
        .filter(|d| *d > 0.0)
        .map(Duration::from_secs_f64)
        .or_else(|| {
            ld.as_ref()
                .and_then(|ld| util::text(&ld["duration"]))
                .and_then(|d| util::parse_duration(&d))
        });
    resolved.thumbnail = data.thumbnail.clone().or_else(|| page.poster());
    resolved.webpage_url = page.canonical().or_else(|| Some(fetched.url.clone()));
    resolved.age_limit = Some(18);
    resolved.variants = variants;
    Ok(resolved)
}

/// One page of a listing's JSON: `/profiles/{name}/videos/new/{page}`, which answers
/// for channels and profiles alike.
async fn listing_page(
    http: &Http,
    platform: &str,
    site: &str,
    name: &str,
    page: usize,
    origin: &Url,
) -> Result<Value, ResolveError> {
    let api = Url::parse(&format!("{site}/profiles/{name}/videos/new/{page}")).expect("valid");
    let response = http
        .get(api)
        .platform(platform)
        .user_agent(BROWSER_UA)
        .header("accept", "application/json, text/plain, */*")
        .header("x-requested-with", "XMLHttpRequest")
        .send()
        .await?;
    let status = response.status;
    let body = response.text(MAX_PAGE).await?;
    let json: Option<Value> = serde_json::from_str(&body).ok();
    match (status.as_u16(), json) {
        (200..=299, Some(json)) if json["result"].as_bool() != Some(false) => Ok(json),
        (404, _) => Err(ResolveError::NotFound(origin.clone())),
        (429, _) => Err(ResolveError::RateLimited(origin.clone())),
        (_, Some(json)) if json["result"].as_bool() == Some(false) => {
            Err(ResolveError::NotFound(origin.clone()))
        }
        (code, _) if !status.is_success() => Err(ResolveError::unavailable(
            origin,
            format!("the listing answered HTTP {code}"),
        )),
        _ => Err(ResolveError::malformed(origin, "the listing is not JSON")),
    }
}

/// The uploads of a channel or profile as a playlist, each entry's link made by
/// `entry_url` from the video's encoded id and slug.
pub async fn resolve_listing(
    http: &Http,
    platform: &str,
    site: &str,
    name: &str,
    origin: &Url,
    entry_url: &(dyn Fn(&str, &str) -> Url + Sync),
) -> Result<Resolution, ResolveError> {
    let mut entries: Vec<PlaylistEntry> = Vec::new();
    let mut total = None;
    let mut title = None;
    let mut page = 0;
    loop {
        let json = listing_page(http, platform, site, name, page, origin).await?;
        total = util::uint(&json["nb_videos"]).map(|n| n as usize).or(total);
        let per_page = util::uint(&json["nb_per_page"]).unwrap_or(36).max(1) as usize;
        let videos = json["videos"].as_array().cloned().unwrap_or_default();
        for video in &videos {
            let Some(eid) = util::text(&video["eid"]) else {
                continue;
            };
            let slug = util::text(&video["u"])
                .and_then(|u| u.rsplit('/').next().map(str::to_string))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "_".to_string());
            if title.is_none() {
                title = util::text(&video["pn"]).and_then(|t| clean_title(&t));
            }
            entries.push(PlaylistEntry {
                url: entry_url(&eid, &slug),
                title: util::text(&video["tf"])
                    .or_else(|| util::text(&video["t"]))
                    .map(|t| util::html_unescape(&t))
                    .and_then(|t| clean_title(&t)),
                duration: util::text(&video["d"]).and_then(|d| util::parse_duration(&d)),
            });
            if entries.len() >= LISTING_LIMIT {
                break;
            }
        }
        page += 1;
        let more = videos.len() >= per_page && total.is_some_and(|t| page * per_page < t);
        if videos.is_empty() || entries.len() >= LISTING_LIMIT || !more {
            break;
        }
    }
    if entries.is_empty() {
        return Err(ResolveError::NotFound(origin.clone()));
    }
    Ok(Resolution::Playlist(Playlist {
        resolver: platform.to_string(),
        id: Some(name.to_string()),
        title: title.or_else(|| clean_title(name)),
        total: total.or(Some(entries.len())),
        entries,
    }))
}

pub struct XvideosResolver {
    http: Http,
}

impl XvideosResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

fn uploader_url(name: &str) -> Option<Url> {
    Url::parse(&format!("{SITE}/profiles/{name}")).ok()
}

fn entry_url(eid: &str, slug: &str) -> Url {
    Url::parse(&format!("{SITE}/video.{eid}/{slug}")).expect("valid")
}

#[async_trait]
impl Resolver for XvideosResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "XVideos",
            hosts: &[
                "xvideos.com",
                "xvideos2.com",
                "xvideos3.com",
                "xvideos.es",
                "xvideos.red",
            ],
            features: &[
                "videos",
                "embeds",
                "quickies",
                "channels",
                "profiles",
                "pornstars",
            ],
            formats: &["hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::Nsfw, Tag::Video],
            session: SessionSupport::None,
            examples: &[
                "https://www.xvideos.com/video.keecekh888c/what_s_her_name_",
                "https://www.xvideos.com/video65982001/what_s_her_name",
                "https://www.xvideos.com/embedframe/keecekh888c",
                "https://www.xvideos.es/video.keecekh888c/what_s_her_name_",
                "https://www.xvideos.com/channels/brazzers",
                "https://www.xvideos.com/profiles/lili_love",
                "https://www.xvideos.com/pornstars/mia-khalifa",
                "https://www.xvideos.com/brazzers",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Video { id } => {
                let page_url = video_page(SITE, &id);
                let mut resolved =
                    resolve_player_page(&self.http, PLATFORM, &page_url, url, &uploader_url)
                        .await?;
                if resolved.id.is_none() {
                    resolved.id = Some(id);
                }
                Ok(Resolution::from(resolved))
            }
            Link::Listing { name } => {
                resolve_listing(&self.http, PLATFORM, SITE, &name, url, &entry_url).await
            }
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

    /// A video page as the site serves it: the player calls, the metadata and JSON-LD.
    fn video_html() -> String {
        r#"<html><head><title>what&apos;s her name&quest; - XVIDEOS.COM</title>
<meta property="og:title" content="what&apos;s her name&quest;" />
<meta property="og:url" content="https://www.xvideos.com/video.keecekh888c/what_s_her_name_" />
<meta property="og:duration" content="120" />
<meta property="og:image" content="https://thumb-cdn77.xvideos-cdn.com/7874b29e/0/xv_27_t.jpg" />
<script type="application/ld+json">{"@context":"https://schema.org","@type":"VideoObject","name":"what&apos;s her name&quest;","description":"A stranger at the door","uploadDate":"2021-10-20T04:44:26+00:00","duration":"PT00H02M00S"}</script>
</head><body><script>
html5player.setVideoTitle('what&#039;s her name?');
html5player.setEncodedIdVideo('keecekh888c');
html5player.setThumbUrl('https://thumb-cdn77.xvideos-cdn.com/7874b29e/0/xv_15_t.jpg');
html5player.setVideoUrlLow('https://mp4-cdn77.xvideos-cdn.com/7874b29e/0/video_240p.mp4?secure=a,1');
html5player.setVideoUrlHigh('https://mp4-cdn77.xvideos-cdn.com/7874b29e/0/video_360p.mp4?secure=b,1');
html5player.setVideoHLS('https://hls-cdn77.xvideos-cdn.com/c,1/7874b29e/0/hls.m3u8');
html5player.setThumbUrl169('https://thumb-cdn77.xvideos-cdn.com/7874b29e/0/xv_27_p.jpg');
html5player.setUploaderName('skakdjskdk');
</script></body></html>"#
            .to_string()
    }

    const MASTER: &str = "#EXTM3U\n#EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=763904,RESOLUTION=738x554,NAME=\"480p\"\nhls-480p-4023d.m3u8\n#EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=1327104,RESOLUTION=1110x832,NAME=\"720p\"\nhls-720p-64652.m3u8\n";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n0.ts\n#EXT-X-ENDLIST\n";

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let video = |id: &str| Some(Link::Video { id: id.into() });
        let listing = |name: &str| Some(Link::Listing { name: name.into() });
        assert_eq!(
            link("https://www.xvideos.com/video.keecekh888c/what_s_her_name_"),
            video("keecekh888c")
        );
        assert_eq!(
            link("http://xvideos.com/video65982001/what_s_her_name"),
            video("65982001")
        );
        assert_eq!(
            link("https://fr.xvideos.com/video4588838/biker"),
            video("4588838")
        );
        assert_eq!(
            link("https://www.xvideos.es/video4588838/biker"),
            video("4588838")
        );
        assert_eq!(
            link("https://www.xvideos2.com/video4588838/biker"),
            video("4588838")
        );
        assert_eq!(
            link("https://www.xvideos.red/video.abc123def45/x"),
            video("abc123def45")
        );
        assert_eq!(
            link("https://flashservice.xvideos.com/embedframe/4588838"),
            video("4588838")
        );
        assert_eq!(
            link("https://www.xvideos.com/embedframe/ucuvbkfda4e"),
            video("ucuvbkfda4e")
        );
        assert_eq!(
            link("http://static-hw.xvideos.com/swf/xv-player.swf?id_video=4588838"),
            video("4588838")
        );
        assert_eq!(
            link("https://www.xvideos.com/prof-video-click/upload/brazzers/hamdokf3427/moms"),
            video("hamdokf3427")
        );
        assert_eq!(
            link("https://www.xvideos.com/lili_love#quickies/a/ipdtikh1a4c"),
            video("ipdtikh1a4c")
        );
        assert_eq!(
            link("https://www.xvideos.com/channels/brazzers"),
            listing("brazzers")
        );
        assert_eq!(
            link("https://www.xvideos.com/profiles/lili_love/"),
            listing("lili_love")
        );
        assert_eq!(
            link("https://www.xvideos.com/pornstar-channels/mia-khalifa"),
            listing("mia-khalifa")
        );
        assert_eq!(
            link("https://www.xvideos.com/pornstars/mia-khalifa"),
            listing("mia-khalifa")
        );
        assert_eq!(
            link("https://www.xvideos.com/brazzers"),
            listing("brazzers")
        );
        assert_eq!(link("https://www.xvideos.com/tags/dildo"), None);
        assert_eq!(link("https://www.xvideos.com/new/3"), None);
        assert_eq!(link("https://www.xvideos.com/best"), None);
        assert_eq!(link("https://www.xvideos.com/"), None);
        assert_eq!(link("https://example.com/video65982001/x"), None);
        assert_eq!(link("https://www.xnxx.com/video-55awb78/x"), None);
    }

    #[test]
    fn player_calls_are_read() {
        let data = player_data(&video_html());
        assert_eq!(data.id.as_deref(), Some("keecekh888c"));
        assert_eq!(data.title.as_deref(), Some("what's her name?"));
        assert_eq!(data.uploader.as_deref(), Some("skakdjskdk"));
        assert!(data.hls.as_ref().unwrap().as_str().ends_with("/hls.m3u8"));
        assert!(data.thumbnail.unwrap().as_str().ends_with("xv_27_p.jpg"));
        assert_eq!(
            page_error(r#"<h1 class="inlineError">This video has been deleted.</h1>"#).as_deref(),
            Some("This video has been deleted.")
        );
        assert_eq!(page_error("<html></html>"), None);
    }

    #[tokio::test]
    async fn videos_resolve_with_the_playlists_renditions() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.xvideos.com/video.keecekh888c/_",
            200,
            "text/html",
            &video_html(),
        ));
        fixture.exchanges.push(get(
            "https://hls-cdn77.xvideos-cdn.com/c,1/7874b29e/0/hls.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MASTER,
        ));
        fixture.exchanges.push(get(
            "https://hls-cdn77.xvideos-cdn.com/c,1/7874b29e/0/hls-480p-4023d.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MEDIA,
        ));
        fixture.exchanges.push(get(
            "https://hls-cdn77.xvideos-cdn.com/c,1/7874b29e/0/hls-720p-64652.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MEDIA,
        ));
        let resolver = XvideosResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.xvideos.com/video.keecekh888c/what_s_her_name_").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        crate::resolve::assert_one_family(&resolved.variants);
        assert_eq!(resolved.id.as_deref(), Some("keecekh888c"));
        assert_eq!(resolved.title.as_deref(), Some("what's her name?"));
        assert_eq!(
            resolved.description.as_deref(),
            Some("A stranger at the door")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("skakdjskdk"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://www.xvideos.com/profiles/skakdjskdk"
        );
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.duration, Some(Duration::from_secs(120)));
        assert_eq!(resolved.age_limit, Some(18));
        assert_eq!(
            resolved.webpage_url.as_ref().unwrap().as_str(),
            "https://www.xvideos.com/video.keecekh888c/what_s_her_name_"
        );
        assert_eq!(resolved.variants.len(), 2, "the playlist's two renditions");
        assert!(resolved.variants.iter().all(|v| v.kind == VariantKind::Hls));
        assert_eq!(resolved.variants[0].height, Some(832));
        assert_eq!(resolved.variants[0].format_id.as_deref(), Some("hls-832p"));
        assert_eq!(resolved.variants[1].height, Some(554));
        assert_eq!(resolved.variants[1].format_id.as_deref(), Some("hls-554p"));
    }

    #[tokio::test]
    async fn missing_and_deleted_videos_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.xvideos.com/video.zzzzzzzzzzz/_",
            404,
            "text/html",
            "<html>Not found</html>",
        ));
        fixture.exchanges.push(get(
            "https://www.xvideos.com/video1/_",
            200,
            "text/html",
            r#"<html><h1 class="inlineError">This video has been deleted.</h1></html>"#,
        ));
        let resolver = XvideosResolver::new(Http::replay(fixture));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.xvideos.com/video.zzzzzzzzzzz/x").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(&Url::parse("https://www.xvideos.com/video1/x").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "This video has been deleted."),
            "{error}"
        );
    }

    #[tokio::test]
    async fn channels_list_their_uploads_across_pages() {
        let video = |eid: &str, title: &str| {
            json!({"id": 1, "eid": eid, "u": format!("/prof-video-click/upload/brazzers/{eid}/{}", title.to_lowercase().replace(' ', "_")),
                "tf": title, "t": title, "d": "8 min", "p": "brazzers", "pn": "Brazzers"})
        };
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.xvideos.com/profiles/brazzers/videos/new/0",
            200,
            "application/json",
            &json!({"nb_videos": 3, "nb_per_page": 2, "current_page": 0, "is_channel": true, "result": true,
                "videos": [video("hamdokf3427", "Moms In Control"), video("omckdeu61d3", "Second &amp; Third")]}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://www.xvideos.com/profiles/brazzers/videos/new/1",
            200,
            "application/json",
            &json!({"nb_videos": 3, "nb_per_page": 2, "current_page": 1, "is_channel": true, "result": true,
                "videos": [video("iefpdbdbf27", "Last One")]}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://www.xvideos.com/profiles/nosuchchannelzzz/videos/new/0",
            404,
            "application/json",
            r#"{"result":false,"code":-1,"message":"Unknown user"}"#,
        ));
        let resolver = XvideosResolver::new(Http::replay(fixture));
        let Resolution::Playlist(playlist) = resolver
            .resolve(&Url::parse("https://www.xvideos.com/channels/brazzers").unwrap())
            .await
            .unwrap()
        else {
            panic!("a channel is a playlist");
        };
        assert_eq!(playlist.title.as_deref(), Some("Brazzers"));
        assert_eq!(playlist.total, Some(3));
        assert_eq!(playlist.entries.len(), 3);
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://www.xvideos.com/video.hamdokf3427/moms_in_control"
        );
        assert_eq!(playlist.entries[0].duration, Some(Duration::from_secs(480)));
        assert_eq!(playlist.entries[1].title.as_deref(), Some("Second & Third"));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.xvideos.com/nosuchchannelzzz").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    /// Every example link resolves live: videos with renditions, listings with entries.
    #[ignore = "reaches the live site: cargo test -- --ignored"]
    #[tokio::test]

    async fn live_examples_resolve() {
        let resolver = XvideosResolver::new(Http::new(crate::http::HttpConfig::default()));
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
            assert!(resolver.matches(&url), "{link}: not matched");
            let resolution = tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&url))
                .await
                .expect("resolution timed out")
                .unwrap_or_else(|e| panic!("{link}: {e}"));
            match resolution {
                Resolution::Media(resolved) => {
                    assert!(
                        resolved.variants.iter().any(|v| v.is_playable()),
                        "{link}: no variants"
                    );
                    assert!(resolved.title.is_some(), "{link}: no title");
                    println!(
                        "{link}: {:?} ({} variants)",
                        resolved.title,
                        resolved.variants.len()
                    );
                }
                Resolution::Playlist(playlist) => {
                    assert!(!playlist.entries.is_empty(), "{link}: no entries");
                    println!(
                        "{link}: {:?} ({} of {:?} entries)",
                        playlist.title,
                        playlist.entries.len(),
                        playlist.total
                    );
                }
            }
        }
    }
}
