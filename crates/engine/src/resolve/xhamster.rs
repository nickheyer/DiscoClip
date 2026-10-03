//! xHamster videos, shorts, galleries, users, creators and channels: every page hands its
//! scripts a `window.initials` object. A video page carries the video model and the
//! player's sources, whose links are ciphered with the byte generator the player ships;
//! deciphered, the HLS masters list a rendition per height, as H.264 and, for newer
//! uploads, AV1. A short's player names no master but one MP4 file per height, and the
//! files stand in wherever the masters are missing. A gallery page lists its photos, a
//! photo page its image, and the user,
//! creator and channel pages list their videos a page at a time. The mirrors and the
//! language and mobile subdomains serve the same pages. Links to xhamsterlive.com, the
//! cam site xHamster fronts, are Stripchat rooms under another name and are handed on to
//! the Stripchat resolver.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, SubtitleTrack, Tag, Variant, VariantKind, clean_title, fetch, hls,
    navigation_headers, page, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "xhamster";
/// How many pages of a listing are read.
const LISTING_PAGES: u32 = 3;
/// How many photos of a gallery are listed.
const GALLERY_LIMIT: usize = 180;
/// Where xhamsterlive.com rooms live.
const STRIPCHAT: &str = "https://stripchat.com";

/// xhamster.com, its numbered and TLD mirrors, and the short domains, under any language
/// or mobile subdomain.
static RE_HOST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^(?:[a-z0-9-]+\.)*(xhamster\d*\.(?:com|one|desi|xxx)|xhms\.pro|xhvid\.com|xhday\.com)$",
    )
    .unwrap()
});
static RE_LIVE_HOST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[a-z0-9-]+\.)*xhamsterlive\.com$").unwrap());
/// `/videos/{slug}-{id}`, `/videos/{id}`.
static RE_VIDEO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/videos/(?:[^/?#]*-)?([A-Za-z0-9]+)/?$").unwrap());
/// `/movies/{id}/{slug}.html`, the older schema.
static RE_MOVIE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/movies/([A-Za-z0-9]+)/[^/?#]*\.html$").unwrap());
/// `/embed/{id}`.
static RE_EMBED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/embed/([A-Za-z0-9]+)/?$").unwrap());
/// `/shorts/{slug}-{id}`.
static RE_SHORT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/shorts/(?:[^/?#]*-)?([A-Za-z0-9]+)/?$").unwrap());
/// `/photos/gallery/{galleryId}/{photoId}`.
static RE_PHOTO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/photos/gallery/(\d+)/(\d+)/?$").unwrap());
/// `/photos/gallery/{slug}-{id}`, `/photos/gallery/{slug}-{id}/{page}`.
static RE_GALLERY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/photos/gallery/(?:[^/?#]*-)?(\d+)(?:/(\d+))?/?$").unwrap());
/// `/users/{name}`, `/users/profiles/{name}`, `/users/{name}/videos/{page}`.
static RE_USER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^/users/(?:profiles/)?([A-Za-z0-9_.-]+)(?:/videos(?:/(\d+))?)?/?$").unwrap()
});
/// `/creators/{name}`, `/creators/{name}/exclusive`, `/creators/{name}/{page}`.
static RE_CREATOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^/creators/([A-Za-z0-9_.-]+)(?:/(?:exclusive|videos|shorts))?(?:/(\d+))?/?$")
        .unwrap()
});
/// `/channels/{slug}`, `/channels/{slug}/{page}`.
static RE_CHANNEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/channels/([A-Za-z0-9_.-]+)(?:/(\d+))?/?$").unwrap());
/// `<div id="videoClosed">…</div>`: why a video shows no player.
static RE_CLOSED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<div[^>]+id=["']videoClosed["'][^>]*>(.+?)</div>"#).unwrap()
});
/// A ciphered link: twelve or more hex digits.
static RE_HEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-fA-F]{12,}$").unwrap());
/// A link whose first path segment is ciphered.
static RE_HEX_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/([0-9a-fA-F]{12,})([/,].+)$").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A video, by its numeric id or its hash slug.
    Video { site: String, id: String },
    /// A short, at the path its link names.
    Short {
        site: String,
        id: String,
        path: String,
    },
    /// A gallery of photos, from `page` on.
    Gallery { site: String, id: String, page: u32 },
    /// One photo of a gallery.
    Photo {
        site: String,
        gallery: String,
        photo: String,
    },
    /// A user's videos, from `page` on.
    User {
        site: String,
        name: String,
        page: u32,
    },
    /// A creator's videos and shorts, from `page` on.
    Creator {
        site: String,
        name: String,
        page: u32,
    },
    /// A channel's videos, from `page` on.
    Channel {
        site: String,
        slug: String,
        page: u32,
    },
    /// A room or listing on xhamsterlive.com, which is Stripchat.
    Live { path: String },
}

/// The site a link's host names, without the mobile prefix: `https://de.xhamster.com`.
fn site_of(host: &str) -> String {
    let host = host
        .strip_prefix("m.")
        .or_else(|| host.strip_prefix("www."))
        .unwrap_or(host);
    format!("https://{host}")
}

fn page_number(text: Option<regex::Match<'_>>) -> u32 {
    text.and_then(|m| m.as_str().parse().ok())
        .filter(|p| *p >= 1)
        .unwrap_or(1)
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if RE_LIVE_HOST.is_match(&host) {
        let mut path = url.path().to_string();
        if let Some(query) = url.query() {
            path.push('?');
            path.push_str(query);
        }
        return Some(Link::Live { path });
    }
    if !RE_HOST.is_match(&host) {
        return None;
    }
    let site = site_of(&host);
    let path = url.path();
    if path == "/xembed.php" {
        let id = util::query_param(url, "video").filter(|v| !v.is_empty())?;
        return Some(Link::Video { site, id });
    }
    if let Some(caps) = RE_VIDEO
        .captures(path)
        .or_else(|| RE_MOVIE.captures(path))
        .or_else(|| RE_EMBED.captures(path))
    {
        return Some(Link::Video {
            site,
            id: caps[1].to_string(),
        });
    }
    if let Some(caps) = RE_SHORT.captures(path) {
        return Some(Link::Short {
            site,
            id: caps[1].to_string(),
            path: path.trim_end_matches('/').to_string(),
        });
    }
    if let Some(caps) = RE_PHOTO.captures(path) {
        return Some(Link::Photo {
            site,
            gallery: caps[1].to_string(),
            photo: caps[2].to_string(),
        });
    }
    if let Some(caps) = RE_GALLERY.captures(path) {
        return Some(Link::Gallery {
            site,
            id: caps[1].to_string(),
            page: page_number(caps.get(2)),
        });
    }
    if let Some(caps) = RE_USER.captures(path) {
        return Some(Link::User {
            site,
            name: caps[1].to_string(),
            page: page_number(caps.get(2)),
        });
    }
    if let Some(caps) = RE_CREATOR.captures(path) {
        return Some(Link::Creator {
            site,
            name: caps[1].to_string(),
            page: page_number(caps.get(2)),
        });
    }
    if let Some(caps) = RE_CHANNEL.captures(path) {
        return Some(Link::Channel {
            site,
            slug: caps[1].to_string(),
            page: page_number(caps.get(2)),
        });
    }
    None
}

/// The player's byte generator: seven algorithms, one named by the first byte of a
/// ciphered link, each stepping a 32-bit state and yielding its low byte.
struct ByteGenerator {
    algorithm: u8,
    state: u32,
}

impl ByteGenerator {
    fn new(algorithm: u8, seed: u32) -> Option<Self> {
        (1..=7).contains(&algorithm).then_some(Self {
            algorithm,
            state: seed,
        })
    }

    fn next_byte(&mut self) -> u8 {
        let s = self.state;
        let out = match self.algorithm {
            // A linear congruential generator.
            1 => {
                let s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                self.state = s;
                s
            }
            // xorshift32.
            2 => {
                let mut s = s ^ (s << 13);
                s ^= s >> 17;
                s ^= s << 5;
                self.state = s;
                s
            }
            // A Weyl sequence finished with MurmurHash3's mixer.
            3 => {
                let s = s.wrapping_add(0x9e37_79b9);
                self.state = s;
                let mut e = s ^ (s >> 16);
                e = e.wrapping_mul(0x85eb_ca77);
                e ^= e >> 13;
                e = e.wrapping_mul(0xc2b2_ae3d);
                e ^ (e >> 16)
            }
            // A rotation, an addition and a multiply.
            4 => {
                let s = s.wrapping_add(0x6d2b_79f5);
                self.state = s;
                let mut e = s.rotate_left(7);
                e = e.wrapping_add(0x9e37_79b9);
                e ^= e >> 11;
                e.wrapping_mul(0x27d4_eb2d)
            }
            // xorshift with a final addition.
            5 => {
                let mut s = s ^ (s << 7);
                s ^= s >> 9;
                s ^= s << 8;
                s = s.wrapping_add(0xa5a5_a5a5);
                self.state = s;
                s
            }
            // A linear congruential generator with a variable right shift.
            6 => {
                let s = s.wrapping_mul(0x2c92_77b5).wrapping_add(0xac56_4b05);
                self.state = s;
                let mixed = s ^ (s >> 18);
                mixed >> ((s >> 27) & 31)
            }
            // A Weyl sequence with a multiply-xor-shift mixer.
            _ => {
                let s = s.wrapping_add(0x9e37_79b9);
                self.state = s;
                let mut e = s ^ (s << 5);
                e = e.wrapping_mul(0x7feb_352d);
                e ^= e >> 15;
                e.wrapping_mul(0x846c_a68b)
            }
        };
        (out & 0xff) as u8
    }
}

/// The text a ciphered hex string stands for: its first byte names the algorithm, the
/// next four seed it, and the rest is XORed with the bytes it yields.
pub fn decipher(hex: &str) -> Option<String> {
    if !RE_HEX.is_match(hex) {
        return None;
    }
    let bytes = hex::decode(hex).ok()?;
    let seed = u32::from_le_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]);
    let mut generator = ByteGenerator::new(bytes[0], seed)?;
    let plain: Vec<u8> = bytes[5..]
        .iter()
        .map(|byte| byte ^ generator.next_byte())
        .collect();
    Some(plain.iter().map(|b| *b as char).collect())
}

/// The link a player source names: ciphered whole, a link whose first path segment is
/// ciphered, or plain. A hex string shorter than a cipher is the player's placeholder.
pub fn decipher_url(text: &str) -> Option<Url> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if RE_HEX.is_match(text) {
        return Url::parse(&decipher(text)?).ok();
    }
    let url = Url::parse(text).ok()?;
    match RE_HEX_PATH.captures(url.path()) {
        Some(caps) => {
            let plain = decipher(&caps[1])?;
            let mut deciphered = url.clone();
            deciphered.set_path(&format!("/{plain}{}", &caps[2]));
            Some(deciphered)
        }
        None => Some(url),
    }
}

/// The `window.initials` object a page hands its scripts.
pub fn initials_of(html: &str) -> Option<Value> {
    let start = html.find("window.initials")?;
    let rest = &html[start + "window.initials".len()..];
    let assign = rest.find('=')?;
    page::leading_json(&rest[assign + 1..]).map(|(value, _)| value)
}

/// Why a video page shows no player, when it says.
pub fn closed_reason(html: &str) -> Option<String> {
    RE_CLOSED
        .captures(html)
        .and_then(|caps| clean_title(&util::clean_html(&caps[1])))
}

fn video_codec(name: &str) -> Option<VideoCodec> {
    match name {
        "h264" => Some(VideoCodec::H264),
        "av1" => Some(VideoCodec::Av1),
        "h265" | "hevc" => Some(VideoCodec::H265),
        "vp9" => Some(VideoCodec::Vp9),
        _ => None,
    }
}

/// An HLS master the player's sources name, filed under its codec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLink {
    pub codec: String,
    pub url: Url,
}

/// The player's HLS masters, one per codec, with their links deciphered.
pub fn source_links(sources: &Value) -> Vec<SourceLink> {
    sources["hls"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(codec, entry)| {
            let url = entry["url"].as_str().and_then(decipher_url)?;
            Some(SourceLink {
                codec: codec.clone(),
                url,
            })
        })
        .collect()
}

/// An MP4 file the player's sources name, filed under its codec and the height the
/// player labels it with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileLink {
    pub codec: String,
    /// The player's quality label, such as `480p`.
    pub label: String,
    pub url: Url,
}

/// The player's MP4 files, one per codec and height, with their links deciphered. The
/// `auto` entry the player lists among them names an HLS master and is left out.
pub fn file_links(sources: &Value) -> Vec<FileLink> {
    sources["standard"]
        .as_object()
        .into_iter()
        .flatten()
        .flat_map(|(codec, entries)| {
            entries
                .as_array()
                .into_iter()
                .flatten()
                .map(move |entry| (codec, entry))
        })
        .filter_map(|(codec, entry)| {
            let url = entry["url"].as_str().and_then(decipher_url)?;
            if url.path().ends_with(".m3u8") {
                return None;
            }
            let label = util::text(&entry["quality"])
                .or_else(|| util::text(&entry["label"]))
                .filter(|label| label != "auto")?;
            Some(FileLink {
                codec: codec.clone(),
                label,
                url,
            })
        })
        .collect()
}

/// The videos a listing page carries: every `videoThumbProps` list in its initials.
fn thumbs_of(initials: &Value) -> Vec<PlaylistEntry> {
    let mut entries: Vec<PlaylistEntry> = Vec::new();
    for list in util::find_keys(initials, "videoThumbProps") {
        for thumb in list.as_array().into_iter().flatten() {
            let Some(url) = util::url_of(&thumb["pageURL"], None) else {
                continue;
            };
            if entries.iter().any(|e| e.url == url) {
                continue;
            }
            entries.push(PlaylistEntry {
                url,
                title: thumb["title"].as_str().and_then(clean_title),
                duration: util::seconds(&thumb["duration"]),
            });
        }
    }
    entries
}

/// What a listing is, for its page links and its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Listing {
    User,
    Creator,
    Channel,
}

impl Listing {
    fn page_url(self, site: &str, name: &str, page: u32) -> String {
        match (self, page) {
            (Listing::User, page) => format!("{site}/users/{name}/videos/{page}"),
            (Listing::Creator, 1) => format!("{site}/creators/{name}"),
            (Listing::Creator, page) => format!("{site}/creators/{name}/{page}"),
            (Listing::Channel, 1) => format!("{site}/channels/{name}"),
            (Listing::Channel, page) => format!("{site}/channels/{name}/{page}"),
        }
    }

    fn title(self, initials: &Value, name: &str) -> String {
        let found = match self {
            Listing::User => util::text(&initials["username"])
                .or_else(|| util::text(&initials["profile"]["name"])),
            Listing::Creator => {
                util::text(&initials["infoExp4040Component"]["pornstarTop"]["name"])
            }
            Listing::Channel => util::text(
                &initials["layoutPage"]["channelLandingInfoProps"]["sponsorChannel"]["channelName"],
            )
            .or_else(|| util::text(&initials["layoutPage"]["pageTitle"])),
        };
        found
            .and_then(|t| clean_title(&t))
            .unwrap_or_else(|| name.to_string())
    }

    fn id(self) -> &'static str {
        match self {
            Listing::User => "user",
            Listing::Creator => "creator",
            Listing::Channel => "channel",
        }
    }
}

pub struct XhamsterResolver {
    http: Http,
}

impl XhamsterResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// A page's initials and its HTML, or the error its status means for `origin`.
    async fn initials(
        &self,
        page_url: &str,
        origin: &Url,
    ) -> Result<(Value, String), ResolveError> {
        let page_url =
            Url::parse(page_url).map_err(|e| ResolveError::malformed(origin, e.to_string()))?;
        let fetched = fetch(
            &self.http,
            &page_url,
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
        let initials = initials_of(&html)
            .ok_or_else(|| ResolveError::malformed(origin, "the page carries no initials"))?;
        Ok((initials, html))
    }

    /// The variants a video's sources play: each HLS master expanded to its renditions,
    /// filed under the master's codec, or the MP4 files the player names when no master
    /// plays.
    async fn variants_of(
        &self,
        sources: &Value,
    ) -> (Vec<Variant>, Vec<SubtitleTrack>, Option<Duration>) {
        let mut variants: Vec<Variant> = Vec::new();
        let mut subtitles = Vec::new();
        let mut duration = None;
        for link in source_links(sources) {
            let codec = link.codec.as_str();
            let prefix = format!("hls-{codec}-");
            match hls::expand(&self.http, &link.url, PLATFORM, BROWSER_UA, &[]).await {
                Ok(expanded) => {
                    for mut variant in expanded.variants {
                        if variant.video.is_none() {
                            variant.video = video_codec(codec);
                        }
                        if variant.audio.is_none() {
                            variant.audio = Some(AudioCodec::Aac);
                        }
                        let label = variant
                            .height
                            .map(|h| format!("{h}p"))
                            .or_else(|| variant.label.clone())
                            .unwrap_or_else(|| "auto".to_string());
                        variant.format_id = Some(format!("{prefix}{label}"));
                        variant.label = Some(label);
                        if !variants.iter().any(|v| v.url == variant.url) {
                            variants.push(variant);
                        }
                    }
                    subtitles.extend(expanded.subtitles);
                    duration = duration.or(expanded.duration);
                }
                Err(error) => {
                    tracing::debug!(url = %link.url, "xHamster playlist not expanded: {error}");
                }
            }
        }
        if variants.is_empty() {
            for link in file_links(sources) {
                let mut variant = Variant::new(link.url, VariantKind::File);
                variant.container = Some(Container::Mp4);
                variant.video = video_codec(&link.codec);
                variant.audio = Some(AudioCodec::Aac);
                variant.height = link
                    .label
                    .strip_suffix('p')
                    .and_then(|height| height.parse().ok());
                variant.format_id = Some(format!("mp4-{}-{}", link.codec, link.label));
                variant.label = Some(link.label);
                if !variants.iter().any(|v| v.url == variant.url) {
                    variants.push(variant);
                }
            }
        }
        (variants, subtitles, duration)
    }

    async fn resolve_video(&self, page_url: &str, url: &Url) -> Result<Resolution, ResolveError> {
        let (initials, html) = self.initials(page_url, url).await?;
        let video = &initials["videoModel"];
        let moment = &initials["layoutPage"]["momentProps"];
        let (model, short) = if video.is_object() {
            (video, false)
        } else if moment.is_object() {
            (moment, true)
        } else {
            return Err(match closed_reason(&html) {
                Some(reason) => ResolveError::unavailable(url, reason),
                None => ResolveError::NotFound(url.clone()),
            });
        };
        let settings = &initials["xplayerSettings"];
        let (variants, subtitles, hls_duration) = self.variants_of(&settings["sources"]).await;
        if variants.is_empty() {
            return Err(match closed_reason(&html) {
                Some(reason) => ResolveError::unavailable(url, reason),
                None => ResolveError::unavailable(url, "the player names no stream"),
            });
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = util::text(&model["idHashSlug"])
            .or_else(|| util::text(&model["id"]))
            .filter(|id| !id.is_empty());
        resolved.title = util::text(&model["title"]).and_then(|t| clean_title(&t));
        resolved.description = util::text(&model["description"]).and_then(|d| clean_title(&d));
        resolved.uploaded_at = util::epoch(&model["created"]);
        resolved.duration = util::seconds(&model["duration"])
            .or_else(|| util::seconds(&settings["duration"]))
            .or(hls_duration);
        resolved.thumbnail = if short {
            util::url_of(&model["posterUrl"], None)
                .or_else(|| util::url_of(&model["thumbUrl"], None))
        } else {
            util::url_of(&model["thumbURL"], None)
                .or_else(|| util::url_of(&model["previewThumbURL"], None))
        };
        resolved.webpage_url =
            util::url_of(&model["pageURL"], None).or_else(|| Url::parse(page_url).ok());
        if short {
            resolved.uploader = util::text(&model["landing"]["name"]).and_then(|n| clean_title(&n));
            resolved.uploader_url = util::url_of(&model["landing"]["link"], None);
        } else {
            resolved.uploader = util::text(&model["author"]["name"]).and_then(|n| clean_title(&n));
            resolved.uploader_url = util::url_of(&model["author"]["pageURL"], None);
        }
        resolved.age_limit = Some(18);
        resolved.subtitles = subtitles;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn resolve_gallery(
        &self,
        site: &str,
        id: &str,
        first_page: u32,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let mut entries: Vec<PlaylistEntry> = Vec::new();
        let mut title = None;
        let mut total = None;
        let mut page_url = format!("{site}/photos/gallery/{id}");
        if first_page > 1 {
            page_url = format!("{page_url}/{first_page}");
        }
        let mut page_number = first_page;
        loop {
            let (initials, _) = self.initials(&page_url, url).await?;
            let gallery = &initials["galleryPage"];
            if !gallery.is_object() {
                return Err(ResolveError::NotFound(url.clone()));
            }
            if title.is_none() {
                title = util::text(&gallery["galleryModel"]["title"]).and_then(|t| clean_title(&t));
            }
            total = total.or_else(|| util::uint(&gallery["photosCount"]).map(|n| n as usize));
            let before = entries.len();
            for (index, photo) in gallery["photoItems"]
                .as_array()
                .into_iter()
                .flatten()
                .enumerate()
            {
                let Some(link) = util::url_of(&photo["link"], None) else {
                    continue;
                };
                if entries.iter().any(|e| e.url == link) {
                    continue;
                }
                let number = entries.len() + 1;
                entries.push(PlaylistEntry {
                    url: link,
                    title: util::text(&photo["alt"])
                        .and_then(|t| clean_title(&t))
                        .or_else(|| title.clone())
                        .map(|t| format!("{t} ({number})"))
                        .or_else(|| Some(format!("Photo {}", index + 1))),
                    duration: None,
                });
            }
            let last_page =
                util::uint(&gallery["paginationProps"]["lastPageNumber"]).unwrap_or(1) as u32;
            let template = util::text(&gallery["paginationProps"]["pageLinkTemplate"]);
            page_number += 1;
            if entries.len() == before
                || entries.len() >= GALLERY_LIMIT
                || page_number > last_page
                || page_number - first_page >= LISTING_PAGES
            {
                break;
            }
            page_url = match template {
                Some(template) if template.contains("{#}") => {
                    template.replace("{#}", &page_number.to_string())
                }
                _ => format!("{site}/photos/gallery/{id}/{page_number}"),
            };
        }
        if entries.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(id.to_string()),
            title,
            total: total.or(Some(entries.len())),
            entries,
        }))
    }

    async fn resolve_photo(
        &self,
        site: &str,
        gallery: &str,
        photo: &str,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let page_url = format!("{site}/photos/gallery/{gallery}/{photo}");
        let (initials, _) = self.initials(&page_url, url).await?;
        let slider = &initials["layoutPage"]["photoSliderProps"];
        let items = slider["photoItems"].as_array().cloned().unwrap_or_default();
        let item = items
            .iter()
            .find(|item| util::text(&item["id"]).as_deref() == Some(photo))
            .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let image = util::url_of(&item["imageURL"], None)
            .ok_or_else(|| ResolveError::malformed(url, "the photo names no image"))?;
        let mut variant = Variant::file(image.clone());
        variant.container = Some(if image.path().contains("/webp/") {
            Container::Webp
        } else {
            super::path_extension(&image)
                .as_deref()
                .and_then(Container::from_extension)
                .unwrap_or(Container::Jpeg)
        });
        variant.format_id = Some("image".to_string());
        let mut resolved = Resolved::of(PLATFORM, MediaKind::Image);
        resolved.id = Some(format!("{gallery}-{photo}"));
        resolved.title = util::text(&item["title"])
            .and_then(|t| clean_title(&t))
            .map(|t| format!("{t} ({photo})"));
        resolved.thumbnail = util::url_of(&item["thumbURL"], None);
        resolved.webpage_url =
            util::url_of(&item["pageURL"], None).or_else(|| Url::parse(&page_url).ok());
        resolved.age_limit = Some(18);
        resolved.variants = vec![variant];
        Ok(Resolution::from(resolved))
    }

    async fn resolve_listing(
        &self,
        site: &str,
        listing: Listing,
        name: &str,
        first_page: u32,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let mut entries: Vec<PlaylistEntry> = Vec::new();
        let mut title = None;
        let mut total = None;
        let mut page_number = first_page;
        loop {
            let page_url = listing.page_url(site, name, page_number);
            let (initials, _) = self.initials(&page_url, url).await?;
            if title.is_none() {
                title = Some(listing.title(&initials, name));
            }
            total = total.or_else(|| {
                util::find_key(&initials, "videoCount")
                    .and_then(util::uint)
                    .map(|n| n as usize)
            });
            let before = entries.len();
            for entry in thumbs_of(&initials) {
                if !entries.iter().any(|e| e.url == entry.url) {
                    entries.push(entry);
                }
            }
            let last_page = util::find_key(&initials, "maxVideoPages")
                .or_else(|| util::find_key(&initials, "lastPageNumber"))
                .and_then(util::uint)
                .unwrap_or(1) as u32;
            page_number += 1;
            if entries.len() == before
                || page_number > last_page
                || page_number - first_page >= LISTING_PAGES
            {
                break;
            }
        }
        if entries.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(format!("{}-{name}", listing.id())),
            title,
            total: total.filter(|n| *n >= entries.len()),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for XhamsterResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "xHamster",
            hosts: &[
                "xhamster.com",
                "xhamster.desi",
                "xhamster.one",
                "xhamster.xxx",
                "xhms.pro",
                "xhvid.com",
                "xhamsterlive.com",
            ],
            features: &[
                "videos",
                "shorts",
                "embeds",
                "galleries",
                "photos",
                "users",
                "creators",
                "channels",
                "mirrors",
                "xhamsterlive rooms",
            ],
            formats: &["hls", "mp4", "webp", "jpg"],
            media: &[MediaKind::Video, MediaKind::Image],
            tags: &[Tag::Nsfw, Tag::Video, Tag::Images],
            session: SessionSupport::None,
            on_by_default: true,
            examples: &[
                "https://xhamster.com/videos/xhamster-awards-2025-the-winners-xh1FC1t",
                "https://xhamster.com/movies/1509445/femaleagent_shy_beauty_takes_the_bait.html",
                "https://xhamster.desi/videos/femaleagent-shy-beauty-takes-the-bait-1509445",
                "https://xhamster.com/xembed.php?video=1509445",
                "https://xhamster.com/shorts/giving-pov-cock-massage-tits-xhWbCN7",
                "https://xhamster.com/photos/gallery/katrina-moreno-lower-body-assist-15808144",
                "https://xhamster.com/photos/gallery/15808144/506218731",
                "https://xhamster.com/users/netvideogirls/videos",
                "https://xhamster.com/creators/squirt-orgasm-69",
                "https://xhamster.com/channels/fake-hub",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Video { site, id } => {
                self.resolve_video(&format!("{site}/videos/{id}"), url)
                    .await
            }
            Link::Short { site, path, .. } => {
                self.resolve_video(&format!("{site}{path}"), url).await
            }
            Link::Gallery { site, id, page } => self.resolve_gallery(&site, &id, page, url).await,
            Link::Photo {
                site,
                gallery,
                photo,
            } => self.resolve_photo(&site, &gallery, &photo, url).await,
            Link::User { site, name, page } => {
                self.resolve_listing(&site, Listing::User, &name, page, url)
                    .await
            }
            Link::Creator { site, name, page } => {
                self.resolve_listing(&site, Listing::Creator, &name, page, url)
                    .await
            }
            Link::Channel { site, slug, page } => {
                self.resolve_listing(&site, Listing::Channel, &slug, page, url)
                    .await
            }
            Link::Live { path } => {
                let room = Url::parse(&format!("{STRIPCHAT}{path}"))
                    .map_err(|e| ResolveError::malformed(url, e.to_string()))?;
                Err(ResolveError::Redirect(room))
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

    fn page_with(initials: Value) -> String {
        format!(
            "<html><head><title>x</title></head><body><script>window.initials = {initials};</script></body></html>"
        )
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let site = |s: &str| s.to_string();
        assert_eq!(
            link("https://xhamster.com/videos/xhamster-awards-2025-the-winners-xh1FC1t"),
            Some(Link::Video {
                site: site("https://xhamster.com"),
                id: "xh1FC1t".into()
            })
        );
        assert_eq!(
            link("https://m.xhamster.com/videos/femaleagent-shy-beauty-takes-the-bait-1509445?hd="),
            Some(Link::Video {
                site: site("https://xhamster.com"),
                id: "1509445".into()
            })
        );
        assert_eq!(
            link("http://xhamster.com/movies/1509445/femaleagent_shy_beauty_takes_the_bait.html"),
            Some(Link::Video {
                site: site("https://xhamster.com"),
                id: "1509445".into()
            })
        );
        assert_eq!(
            link("https://xhamster.com/xembed.php?video=3328539"),
            Some(Link::Video {
                site: site("https://xhamster.com"),
                id: "3328539".into()
            })
        );
        assert_eq!(
            link("https://xhamster.com/embed/1509445"),
            Some(Link::Video {
                site: site("https://xhamster.com"),
                id: "1509445".into()
            })
        );
        for mirror in [
            "https://xhamster.desi/videos/x-1509445",
            "https://xhamster2.com/videos/x-1509445",
            "https://xhamster20.desi/videos/x-1509445",
            "https://xhamster.one/videos/x-1509445",
            "https://xhamster.xxx/videos/x-1509445",
            "https://xhms.pro/videos/x-1509445",
            "https://xhvid.com/videos/x-1509445",
            "https://de.xhamster.com/videos/x-1509445",
        ] {
            assert!(
                matches!(link(mirror), Some(Link::Video { id, .. }) if id == "1509445"),
                "{mirror}"
            );
        }
        assert_eq!(
            link("https://xhamster.com/shorts/big-tits-xh9BF1h"),
            Some(Link::Short {
                site: site("https://xhamster.com"),
                id: "xh9BF1h".into(),
                path: "/shorts/big-tits-xh9BF1h".into()
            })
        );
        assert_eq!(
            link("https://xhamster.com/photos/gallery/big-tits-16588642"),
            Some(Link::Gallery {
                site: site("https://xhamster.com"),
                id: "16588642".into(),
                page: 1
            })
        );
        assert_eq!(
            link("https://xhamster.com/photos/gallery/big-tits-16588642/3"),
            Some(Link::Gallery {
                site: site("https://xhamster.com"),
                id: "16588642".into(),
                page: 3
            })
        );
        assert_eq!(
            link("https://xhamster.com/photos/gallery/16588642/520080114"),
            Some(Link::Photo {
                site: site("https://xhamster.com"),
                gallery: "16588642".into(),
                photo: "520080114".into()
            })
        );
        assert_eq!(
            link("https://xhamster.com/users/netvideogirls/videos/2"),
            Some(Link::User {
                site: site("https://xhamster.com"),
                name: "netvideogirls".into(),
                page: 2
            })
        );
        assert_eq!(
            link("https://xhamster.com/users/profiles/binajane"),
            Some(Link::User {
                site: site("https://xhamster.com"),
                name: "binajane".into(),
                page: 1
            })
        );
        assert_eq!(
            link("https://xhamster.com/creators/squirt-orgasm-69/exclusive"),
            Some(Link::Creator {
                site: site("https://xhamster.com"),
                name: "squirt-orgasm-69".into(),
                page: 1
            })
        );
        assert_eq!(
            link("https://xhamster.com/channels/fake-hub/4"),
            Some(Link::Channel {
                site: site("https://xhamster.com"),
                slug: "fake-hub".into(),
                page: 4
            })
        );
        assert_eq!(
            link("https://xhamsterlive.com/Asian_Asami"),
            Some(Link::Live {
                path: "/Asian_Asami".into()
            })
        );
        assert_eq!(link("https://xhamster.com/"), None);
        assert_eq!(link("https://xhamster.com/categories/german"), None);
        assert_eq!(link("https://xhamster.com/search/big+tits"), None);
        assert_eq!(link("https://example.com/videos/x-1509445"), None);
    }

    #[test]
    fn ciphered_links_are_deciphered_as_the_player_does() {
        assert_eq!(
            decipher_url("0269d7f6590c004badb4657a4cee3015369b686e4b861e1c73e6857397375e2dddc96804f545e8e79c9a48fb4e1ebd7ad6abe89fa8557588f2c029ea8c3101c51732f46743f10e62b048d37bad8e442388c3543d32acc3f553ccf5abf8d1702b7620c68f09b0454a95e4598aa8d5e81e3218eeb5f0a874d7289747af2e7f06cf124594cf5af84d7892").unwrap().as_str(),
            "https://video-h.xhcdn.com/key=wpsNGtiX1Ht4biFdQ11oBg,end=1790283600,limit=3/data=24.113.20.221-dvp/speed=0/001/509/445/144p.h264.mp4"
        );
        assert_eq!(
            decipher_url("0504b881281609e6fe6f40d3bcac802c73162fc68b51e095271bec4f0fe7962cb0e1b98eea412ebe3cffd6ebc70a65665b31c90cf397a3b19356d3ac2fe7bb78c5ef203c1ba882f9fcc38ec44d05f46828b75b8e7a75136da0c9fa5463c67eed11d73f00f44b834463aeb00fb46946375a951dbfa04ad7d6b46c520bc1349a8f661a299f144f9d14c28d853cf3aedd48a0").unwrap().as_str(),
            "https://video-nss-h.xhcdn.com/GRfIJZdeOpVbBxVUs_3IVg==,1790283600/media=hls4/multi=256x144:144p,426x240:240p/001/509/445/_TPL_.h264.mp4.m3u8"
        );
        assert_eq!(
            decipher_url("07403d6c7824dfb0ace1e8937584b00fff9454b85468589cd7a31b467f25b8e9690b67b0817d895da33ce59fbc83bfbaed8ffd2eaa626daa3fb78379f18042063dea3cbe41af0ef5af795ab4d5a74a24163543540c34596aa7cb30d72220b04bb813b09db23d3badcefd2bddbe9d8a953d6154d1ad4df258707904755d17c8dc25344031177e5b244701d28328a402669d9c50afea08c22bc2b8a2e97c9085f7f2fef6451e67259e1b825d8233e45f674fa21c").unwrap().as_str(),
            "https://video-h.xhcdn.com/key=qZmTvpd+vqXn8U9yHh4vfQ,end=1790283600/data=24.113.20.221-dvp/referer=/media=hls4/multi=256x144:144p,426x240:240p/001/509/445/_TPL_.h264.mp4.m3u8"
        );
        // The player's placeholder for a source it has not filled in.
        assert_eq!(decipher_url("03d9436655"), None);
        // A plain link, as shorts carry.
        assert_eq!(
            decipher_url(
                "https://video-nss.xhcdn.com/abc,1790283600/media=hls2/030/685/096/_TPL_.h264.mp4.m3u8"
            )
            .unwrap()
            .as_str(),
            "https://video-nss.xhcdn.com/abc,1790283600/media=hls2/030/685/096/_TPL_.h264.mp4.m3u8"
        );
        assert_eq!(
            decipher("0869d7f6590c004badb465"),
            None,
            "an unknown algorithm"
        );
    }

    fn video_initials(hls: &str) -> Value {
        json!({
            "videoModel": {
                "id": 28063317, "idHashSlug": "xh1FC1t", "duration": 372,
                "title": "xHamster Awards 2025 - The Winners", "description": "",
                "pageURL": "https://xhamster.com/videos/xhamster-awards-2025-the-winners-xh1FC1t",
                "created": 1763216161,
                "thumbURL": "https://ic-vt-nss.xhcdn.com/a/x/s(w:1280,h:720),webp/028/063/317/1280x720.17634215.jpg",
                "author": {"name": "xHamster", "pageURL": "https://xhamster.com/users/xhamster"}
            },
            "xplayerSettings": {
                "videoId": 28063317, "duration": 372,
                "sources": {
                    "hls": {"h264": {"url": hls, "fallback": "03d9436655"}},
                    "standard": {"h264": [
                        {"url": hls, "fallback": "03d9436655", "quality": "auto", "label": "auto"},
                        {"url": "https://video-h.xhcdn.com/key=k,end=1/data=d/speed=0/028/063/317/720p.h264.mp4", "fallback": "0224bbdc25", "quality": "720p", "label": "720p"}
                    ]}
                }
            }
        })
    }

    #[tokio::test]
    async fn videos_resolve_to_their_renditions() {
        let master = "https://video-nss-h.xhcdn.com/abc,1790283600/media=hls4/multi=1280x720:720p,1920x1080:1080p/028/063/317/_TPL_.h264.mp4.m3u8";
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://xhamster.com/videos/xh1FC1t",
            200,
            "text/html",
            page_with(video_initials(master)),
        ));
        fixture.exchanges.push(get(
            master,
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:PROGRAM-ID=1,AVERAGE-BANDWIDTH=1000000,BANDWIDTH=1400000,RESOLUTION=1280x720,CODECS=\"mp4a.40.2,avc1.4d401f\"\n720p.h264.mp4.m3u8\n#EXT-X-STREAM-INF:PROGRAM-ID=1,AVERAGE-BANDWIDTH=2000000,BANDWIDTH=2800000,RESOLUTION=1920x1080,CODECS=\"mp4a.40.2,avc1.640028\"\n1080p.h264.mp4.m3u8\n".into(),
        ));
        for name in ["720p", "1080p"] {
            fixture.exchanges.push(get(
                &format!("https://video-nss-h.xhcdn.com/abc,1790283600/media=hls4/multi=1280x720:720p,1920x1080:1080p/028/063/317/{name}.h264.mp4.m3u8"),
                200,
                "application/vnd.apple.mpegurl",
                "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nseg-1.ts\n#EXT-X-ENDLIST\n".into(),
            ));
        }
        let resolver = XhamsterResolver::new(Http::replay(fixture));
        let url =
            Url::parse("https://xhamster.com/videos/xhamster-awards-2025-the-winners-xh1FC1t")
                .unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("xh1FC1t"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("xHamster Awards 2025 - The Winners")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("xHamster"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://xhamster.com/users/xhamster"
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(372)));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.age_limit, Some(18));
        assert_eq!(resolved.media, MediaKind::Video);
        crate::resolve::assert_one_family(&resolved.variants);
        assert_eq!(
            resolved.variants.len(),
            2,
            "one rendition per height, and none of the files the player lists"
        );
        assert!(resolved.variants.iter().all(|v| v.kind == VariantKind::Hls));
        assert_eq!(resolved.variants[0].height, Some(720));
        assert_eq!(
            resolved.variants[0].format_id.as_deref(),
            Some("hls-h264-720p")
        );
        assert_eq!(resolved.variants[0].video, Some(VideoCodec::H264));
        assert_eq!(resolved.variants[1].height, Some(1080));
        assert_eq!(resolved.variants[1].label.as_deref(), Some("1080p"));
    }

    #[tokio::test]
    async fn shorts_resolve_from_their_moment() {
        let master = "https://video-nss.xhcdn.com/abc,1790283600/media=hls2/multi=1280x720:720p/030/685/096/_TPL_.h264.mp4.m3u8";
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://xhamster.com/shorts/big-tits-xh9BF1h",
            200,
            "text/html",
            page_with(json!({
                "layoutPage": {"momentProps": {
                    "id": 30685096, "title": "Big Big Tits", "created": 1788715805,
                    "pageURL": "https://xhamster.com/shorts/big-tits-xh9BF1h",
                    "posterUrl": "https://ic-vt-nss.xhcdn.com/a/x/frame.0.webp",
                    "landing": {"name": "BinaJane", "link": "https://xhamster.com/creators/binajane/shorts"}
                }},
                "xplayerSettings": {"duration": 7, "sources": {
                    "hls": {"h264": {"url": master, "fallback": ""}},
                    "standard": {"h264": [{"url": "https://video7.xhcdn.com/key=a,end=1,limit=3/data=d/speed=0/031/046/070/480p.h264.mp4", "fallback": "", "quality": "480p", "label": "480p"}]}
                }}
            })),
        ));
        fixture.exchanges.push(get(
            master,
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1400000,RESOLUTION=720x1280,CODECS=\"mp4a.40.2,avc1.4d401f\"\n720p.h264.mp4.m3u8\n".into(),
        ));
        fixture.exchanges.push(get(
            "https://video-nss.xhcdn.com/abc,1790283600/media=hls2/multi=1280x720:720p/030/685/096/720p.h264.mp4.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\nseg-1.ts\n#EXT-X-ENDLIST\n".into(),
        ));
        let resolver = XhamsterResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://xhamster.com/shorts/big-tits-xh9BF1h").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("30685096"));
        assert_eq!(resolved.title.as_deref(), Some("Big Big Tits"));
        assert_eq!(resolved.uploader.as_deref(), Some("BinaJane"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(7)));
        crate::resolve::assert_one_family(&resolved.variants);
        assert_eq!(
            resolved.variants.len(),
            1,
            "the rendition of the master, and not the file the player lists"
        );
        let rendition = &resolved.variants[0];
        assert_eq!(rendition.kind, VariantKind::Hls);
        assert_eq!(rendition.height, Some(1280));
        assert_eq!(rendition.format_id.as_deref(), Some("hls-h264-1280p"));
        assert_eq!(rendition.video, Some(VideoCodec::H264));
    }

    #[tokio::test]
    async fn shorts_without_a_master_resolve_to_the_players_file() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://xhamster.com/shorts/giving-pov-massage-xhWbCN7",
            200,
            "text/html",
            page_with(json!({
                "layoutPage": {"momentProps": {
                    "id": 31046070, "title": "Neighbor giving POV massage | Clip 2", "created": 1790259527,
                    "pageURL": "https://xhamster.com/shorts/giving-pov-massage-xhWbCN7",
                    "posterUrl": "https://ic-vt-nss.xhcdn.com/a/x/frame.0.webp",
                    "landing": {"name": "Squirt_orgasm_69", "link": "https://xhamster.com/creators/squirt-orgasm-69/shorts"}
                }},
                "xplayerSettings": {"duration": 30, "sources": {
                    "standard": {"h264": [{"url": "https://video7.xhcdn.com/key=a,end=1,limit=3/data=d/speed=0/031/046/070/480p.h264.mp4", "fallback": "", "quality": "480p", "label": "480p", "type": ""}]}
                }}
            })),
        ));
        let resolver = XhamsterResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://xhamster.com/shorts/giving-pov-massage-xhWbCN7").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("31046070"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(30)));
        assert_eq!(resolved.media, MediaKind::Video);
        assert_eq!(resolved.variants.len(), 1, "the one file the player names");
        let file = &resolved.variants[0];
        assert_eq!(file.kind, VariantKind::File);
        assert_eq!(
            file.url.as_str(),
            "https://video7.xhcdn.com/key=a,end=1,limit=3/data=d/speed=0/031/046/070/480p.h264.mp4"
        );
        assert_eq!(file.container, Some(Container::Mp4));
        assert_eq!(file.video, Some(VideoCodec::H264));
        assert_eq!(file.audio, Some(AudioCodec::Aac));
        assert_eq!(file.height, Some(480));
        assert_eq!(file.format_id.as_deref(), Some("mp4-h264-480p"));
        assert_eq!(file.label.as_deref(), Some("480p"));
        assert!(file.is_playable());
    }

    #[tokio::test]
    async fn galleries_list_their_photos_and_photos_are_images() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://xhamster.com/photos/gallery/16588642",
            200,
            "text/html",
            page_with(json!({"galleryPage": {
                "id": 16588642, "photosPerPage": 2, "photosCount": 3,
                "photoItems": [
                    {"id": 520080114, "imgSrc": "https://ic-ph-nss.xhcdn.com/a/x/webp/000/520/080/114_1000.jpg", "link": "https://xhamster.com/photos/gallery/16588642/520080114", "alt": "Big tits"},
                    {"id": 520080115, "imgSrc": "https://ic-ph-nss.xhcdn.com/a/y/webp/000/520/080/115_1000.jpg", "link": "https://xhamster.com/photos/gallery/16588642/520080115", "alt": "Big tits"}
                ],
                "paginationProps": {"currentPageNumber": 1, "lastPageNumber": 2, "pageLinkTemplate": "https://xhamster.com/photos/gallery/big-tits-16588642/{#}"},
                "galleryModel": {"title": "Big tits"}
            }})),
        ));
        fixture.exchanges.push(get(
            "https://xhamster.com/photos/gallery/big-tits-16588642/2",
            200,
            "text/html",
            page_with(json!({"galleryPage": {
                "id": 16588642, "photosPerPage": 2, "photosCount": 3,
                "photoItems": [
                    {"id": 520080116, "imgSrc": "https://ic-ph-nss.xhcdn.com/a/z/webp/000/520/080/116_1000.jpg", "link": "https://xhamster.com/photos/gallery/16588642/520080116", "alt": "Big tits"}
                ],
                "paginationProps": {"currentPageNumber": 2, "lastPageNumber": 2, "pageLinkTemplate": "https://xhamster.com/photos/gallery/big-tits-16588642/{#}"},
                "galleryModel": {"title": "Big tits"}
            }})),
        ));
        fixture.exchanges.push(get(
            "https://xhamster.com/photos/gallery/16588642/520080115",
            200,
            "text/html",
            page_with(json!({"layoutPage": {"photoSliderProps": {"photoItems": [
                {"id": 520080114, "galleryId": 16588642, "imageURL": "https://ic-ph-nss.xhcdn.com/a/x/webp/000/520/080/114_1000.jpg", "title": "Big tits", "pageURL": "https://xhamster.com/photos/gallery/16588642/520080114"},
                {"id": 520080115, "galleryId": 16588642, "imageURL": "https://ic-ph-nss.xhcdn.com/a/y/webp/000/520/080/115_1000.jpg", "thumbURL": "https://ic-ph-nss.xhcdn.com/a/y/webp/000/520/080/115_240.jpg", "title": "Big tits", "pageURL": "https://xhamster.com/photos/gallery/16588642/520080115"}
            ], "total": 3}}})),
        ));
        let resolver = XhamsterResolver::new(Http::replay(fixture));
        let Resolution::Playlist(gallery) = resolver
            .resolve(&Url::parse("https://xhamster.com/photos/gallery/big-tits-16588642").unwrap())
            .await
            .unwrap()
        else {
            panic!("a gallery is a playlist");
        };
        assert_eq!(gallery.title.as_deref(), Some("Big tits"));
        assert_eq!(gallery.total, Some(3));
        assert_eq!(gallery.entries.len(), 3);
        assert_eq!(gallery.entries[1].title.as_deref(), Some("Big tits (2)"));
        let photo = resolver
            .resolve(&gallery.entries[1].url)
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(photo.media, MediaKind::Image);
        assert_eq!(photo.id.as_deref(), Some("16588642-520080115"));
        assert_eq!(photo.title.as_deref(), Some("Big tits (520080115)"));
        assert_eq!(photo.variants.len(), 1);
        assert_eq!(photo.variants[0].container, Some(Container::Webp));
        assert_eq!(
            photo.variants[0].url.as_str(),
            "https://ic-ph-nss.xhcdn.com/a/y/webp/000/520/080/115_1000.jpg"
        );
        assert!(photo.thumbnail.is_some());
    }

    fn thumb(id: u64, title: &str) -> Value {
        json!({"id": id, "duration": 60 + id, "title": title, "pageURL": format!("https://xhamster.com/videos/{}-xh{id}", title.replace(' ', "-").to_lowercase())})
    }

    #[tokio::test]
    async fn channels_users_and_creators_list_their_videos() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://xhamster.com/channels/fake-hub",
            200,
            "text/html",
            page_with(json!({"layoutPage": {
                "pageTitle": "Fake Hub Porn Videos: fakehub.com",
                "videoListProps": {"videoThumbProps": [thumb(1, "Clip one"), thumb(2, "Clip two")]},
                "channelLandingInfoProps": {"sponsorChannel": {"videoCount": 3989, "channelName": "Fake Hub"}},
                "paginationProps": {"currentPageNumber": 1, "lastPageNumber": 84}
            }})),
        ));
        fixture.exchanges.push(get(
            "https://xhamster.com/channels/fake-hub/2",
            200,
            "text/html",
            page_with(json!({"layoutPage": {
                "videoListProps": {"videoThumbProps": [thumb(2, "Clip two"), thumb(3, "Clip three")]},
                "paginationProps": {"currentPageNumber": 2, "lastPageNumber": 84}
            }})),
        ));
        fixture.exchanges.push(get(
            "https://xhamster.com/channels/fake-hub/3",
            200,
            "text/html",
            page_with(json!({"layoutPage": {
                "videoListProps": {"videoThumbProps": [thumb(4, "Clip four")]},
                "paginationProps": {"currentPageNumber": 3, "lastPageNumber": 84}
            }})),
        ));
        fixture.exchanges.push(get(
            "https://xhamster.com/users/netvideogirls/videos/1",
            200,
            "text/html",
            page_with(
                json!({"username": "netvideogirls", "maxVideoPages": 1, "page": 1,
                "videoListComponent": {"videoThumbProps": [thumb(5, "Casting")]}}),
            ),
        ));
        fixture.exchanges.push(get(
            "https://xhamster.com/creators/squirt-orgasm-69",
            200,
            "text/html",
            page_with(json!({
                "momentsListComponent": {"videoThumbProps": [{"id": 9, "title": "Short one", "pageURL": "https://xhamster.com/shorts/short-one-xh9"}]},
                "newestVideoSectionComponent": {"videoListProps": {"videoThumbProps": [thumb(6, "Newest")]}},
                "trendingVideoSectionComponent": {"videoListProps": {"videoThumbProps": [thumb(6, "Newest"), thumb(7, "Trending")]}},
                "paginationComponent": {"currentPageNumber": 1, "lastPageNumber": 1},
                "infoExp4040Component": {"pornstarTop": {"videoCount": 182, "name": "Squirt_orgasm_69"}}
            })),
        ));
        let resolver = XhamsterResolver::new(Http::replay(fixture));
        let list = |link: &str| {
            let url = Url::parse(link).unwrap();
            let resolver = &resolver;
            async move {
                match resolver.resolve(&url).await.unwrap() {
                    Resolution::Playlist(playlist) => playlist,
                    other => panic!("expected a playlist, got {other:?}"),
                }
            }
        };
        let channel = list("https://xhamster.com/channels/fake-hub").await;
        assert_eq!(channel.title.as_deref(), Some("Fake Hub"));
        assert_eq!(channel.total, Some(3989));
        assert_eq!(channel.entries.len(), 4, "three pages, each video once");
        assert_eq!(channel.entries[0].title.as_deref(), Some("Clip one"));
        assert_eq!(channel.entries[0].duration, Some(Duration::from_secs(61)));
        let user = list("https://xhamster.com/users/netvideogirls/videos").await;
        assert_eq!(user.title.as_deref(), Some("netvideogirls"));
        assert_eq!(user.entries.len(), 1);
        assert_eq!(user.total, None);
        let creator = list("https://xhamster.com/creators/squirt-orgasm-69").await;
        assert_eq!(creator.title.as_deref(), Some("Squirt_orgasm_69"));
        assert_eq!(creator.entries.len(), 3, "shorts and videos, each once");
        assert_eq!(creator.total, Some(182));
    }

    #[tokio::test]
    async fn missing_and_closed_videos_say_so_and_live_rooms_go_to_stripchat() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://xhamster.com/videos/1",
            410,
            "text/html",
            "<html>gone</html>".into(),
        ));
        fixture.exchanges.push(get(
            "https://xhamster.com/videos/2",
            200,
            "text/html",
            format!(
                "<html><body><div id=\"videoClosed\" class=\"x\">This video was deleted by <b>its owner</b>.</div><script>window.initials = {};</script></body></html>",
                json!({"relatedVideoListComponent": {}})
            ),
        ));
        let resolver = XhamsterResolver::new(Http::replay(fixture));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://xhamster.com/videos/gone-1").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(&Url::parse("https://xhamster.com/videos/closed-2").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "This video was deleted by its owner."),
            "{error}"
        );
        let redirect = resolver
            .resolve(&Url::parse("https://xhamsterlive.com/Asian_Asami").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&redirect, ResolveError::Redirect(to) if to.as_str() == "https://stripchat.com/Asian_Asami"),
            "{redirect}"
        );
    }

    #[ignore = "reaches the live site: cargo test -- --ignored"]
    #[tokio::test]
    async fn live_examples_resolve() {
        let resolver = XhamsterResolver::new(Http::new(crate::http::HttpConfig::default()));
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
