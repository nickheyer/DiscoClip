//! Eporner videos, categories, profiles and pornstar pages. A video page names the
//! player's video id and a hash; the hash, re-encoded the way the player's script does,
//! unlocks the site's video API, which lists the MP4 file of each quality (with an AV1
//! twin beside each when the page offers AV1 downloads) and an HLS playlist when the
//! video has one. Category, profile and pornstar pages are the site's own HTML, read
//! page by page through the next links they carry.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionSupport, Tag, Variant, clean_title, fetch, navigation_headers, page,
    status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "eporner";
const SITE: &str = "https://www.eporner.com/";
/// How many videos a listing is read up to.
const LISTING_LIMIT: usize = 180;
/// How many pages of a listing are turned.
const PAGE_LIMIT: u32 = 3;

static RE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9]{6,}$").unwrap());
static RE_SLUG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_.-]+$").unwrap());
/// `EP.video.player.vid = 'bnPrqfNPT34'`: the id the player asks the API for.
static RE_VID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"EP\.video\.player\.vid\s*=\s*['\x22]([A-Za-z0-9]+)['\x22]").unwrap()
});
/// The hash the player re-encodes before asking the API.
static RE_HASH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"hash\s*[:=]\s*['\x22]([\da-f]{32})['\x22]").unwrap());
/// `1080p@60fps HD`: the height and frame rate a format name carries.
static RE_HEIGHT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+)[pP]").unwrap());
static RE_FPS: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\d+)fps").unwrap());
/// The uploader named under a video.
static RE_UPLOADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<li class="vit-uploader"><a href="(/profile/[^"]+)"[^>]*>([^<]+)</a>"#).unwrap()
});
/// A video in a listing: `<div class="mb …" data-id="…">`.
static RE_ITEM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<div\b[^>]*\bclass="mb\b[^"]*"[^>]*>"#).unwrap());
static RE_ITEM_LINK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"href="/video-([A-Za-z0-9]+)/[^"]*""#).unwrap());
static RE_ITEM_TITLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<p class="mbtit"><a[^>]*>([^<]*)</a>"#).unwrap());
static RE_ITEM_DURATION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<span class="mbtim"[^>]*>([^<]+)</span>"#).unwrap());
static RE_NEXT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<link rel="next" href="([^"]+)""#).unwrap());
static RE_H1: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<h1[^>]*>(.*?)</h1>").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Video {
        id: String,
    },
    Category {
        slug: String,
        page: Option<u32>,
    },
    /// An uploader's profile, with their uploads.
    Profile {
        name: String,
    },
    Pornstar {
        slug: String,
        page: Option<u32>,
    },
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "eporner.com" && host != "www.eporner.com" {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let video = |id: &str| {
        RE_ID
            .is_match(id)
            .then(|| Link::Video { id: id.to_string() })
    };
    let page_number = |text: &str| text.parse::<u32>().ok().filter(|n| *n >= 1);
    match segments.as_slice() {
        [first, ..] if first.starts_with("video-") => video(&first[6..]),
        ["hd-porn" | "embed", id, ..] => video(id),
        ["cat", slug] if RE_SLUG.is_match(slug) => Some(Link::Category {
            slug: slug.to_string(),
            page: None,
        }),
        ["cat", slug, number] if RE_SLUG.is_match(slug) => Some(Link::Category {
            slug: slug.to_string(),
            page: Some(page_number(number)?),
        }),
        ["profile", name] if RE_SLUG.is_match(name) => Some(Link::Profile {
            name: name.to_string(),
        }),
        ["pornstar", slug] if RE_SLUG.is_match(slug) => Some(Link::Pornstar {
            slug: slug.to_string(),
            page: None,
        }),
        ["pornstar", slug, number] if RE_SLUG.is_match(slug) => Some(Link::Pornstar {
            slug: slug.to_string(),
            page: Some(page_number(number)?),
        }),
        _ => None,
    }
}

/// The hash the API takes: the page's 32 hex digits, in four 8-digit pieces, each
/// written in base 36, as the player's script does.
pub fn api_hash(page_hash: &str) -> Option<String> {
    if page_hash.len() != 32 || !page_hash.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = String::new();
    for piece in 0..4 {
        let value = u64::from_str_radix(&page_hash[piece * 8..piece * 8 + 8], 16).ok()?;
        out.push_str(&base36(value));
    }
    Some(out)
}

fn base36(mut value: u64) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if value == 0 {
        return "0".to_string();
    }
    let mut digits = Vec::new();
    while value > 0 {
        digits.push(DIGITS[(value % 36) as usize]);
        value /= 36;
    }
    digits.reverse();
    String::from_utf8(digits).expect("ascii digits")
}

/// The variants a video's API answer lists: an MP4 per quality, the AV1 twin of each
/// when the page offers AV1, and the HLS playlists.
pub fn variants_of(video: &Value, av1: bool) -> Vec<Variant> {
    let mut variants = Vec::new();
    let sources = &video["sources"];
    for (name, source) in sources["mp4"].as_object().into_iter().flatten() {
        let Some(url) = util::url_of(&source["src"], None) else {
            continue;
        };
        let height = RE_HEIGHT
            .captures(name)
            .and_then(|c| c[1].parse::<u32>().ok());
        let fps = RE_FPS.captures(name).and_then(|c| c[1].parse::<f64>().ok());
        let label = util::text(&source["labelShort"])
            .and_then(|l| clean_title(&l))
            .unwrap_or_else(|| name.clone());
        let mut v = Variant::file(url.clone());
        v.container = Some(Container::Mp4);
        v.video = Some(VideoCodec::H264);
        v.audio = Some(AudioCodec::Aac);
        v.height = height;
        v.fps = fps;
        v.label = Some(label.clone());
        v.format_id = Some(name.clone());
        variants.push(v);
        if av1 && url.path().ends_with(".mp4") {
            let mut twin = url.clone();
            twin.set_path(&format!("{}-av1.mp4", url.path().trim_end_matches(".mp4")));
            let mut v = Variant::file(twin);
            v.container = Some(Container::Mp4);
            v.video = Some(VideoCodec::Av1);
            v.audio = Some(AudioCodec::Aac);
            v.height = height;
            v.fps = fps;
            v.label = Some(format!("{label} AV1"));
            v.format_id = Some(format!("av1-{name}"));
            variants.push(v);
        }
    }
    for (name, source) in sources["hls"].as_object().into_iter().flatten() {
        let Some(url) = util::url_of(&source["src"], None) else {
            continue;
        };
        let mut v = Variant::hls(url);
        v.height = RE_HEIGHT
            .captures(name)
            .and_then(|c| c[1].parse::<u32>().ok());
        v.label = util::text(&source["labelShort"])
            .and_then(|l| clean_title(&l))
            .or_else(|| Some(name.clone()));
        v.format_id = Some(format!("hls-{name}"));
        variants.push(v);
    }
    variants
}

/// The videos a listing page lists, each once.
pub fn listing_entries(html: &str) -> Vec<PlaylistEntry> {
    let starts: Vec<usize> = RE_ITEM.find_iter(html).map(|m| m.start()).collect();
    let mut entries: Vec<PlaylistEntry> = Vec::new();
    for (index, start) in starts.iter().enumerate() {
        let end = starts.get(index + 1).copied().unwrap_or(html.len());
        let block = &html[*start..end];
        let Some(caps) = RE_ITEM_LINK.captures(block) else {
            continue;
        };
        let Ok(url) = Url::parse(&format!("{SITE}video-{}/", &caps[1])) else {
            continue;
        };
        if entries.iter().any(|e| e.url == url) {
            continue;
        }
        entries.push(PlaylistEntry {
            url,
            title: RE_ITEM_TITLE
                .captures(block)
                .and_then(|c| clean_title(&util::html_unescape(&c[1]))),
            duration: RE_ITEM_DURATION
                .captures(block)
                .and_then(|c| util::parse_duration(&c[1])),
        });
    }
    entries
}

/// The link to the next page of a listing.
pub fn next_page(html: &str, base: &Url) -> Option<Url> {
    RE_NEXT
        .captures(html)
        .and_then(|c| base.join(&util::html_unescape(&c[1])).ok())
}

/// The page's heading, as the name of the listing.
fn heading(html: &str) -> Option<String> {
    RE_H1
        .captures(html)
        .and_then(|c| clean_title(&util::clean_html(&c[1]).replace('\n', " ")))
}

pub struct EpornerResolver {
    http: Http,
}

impl EpornerResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    async fn resolve_video(&self, id: &str, url: &Url) -> Result<Resolution, ResolveError> {
        let page_url = Url::parse(&format!("{SITE}video-{id}/")).expect("valid");
        let fetched = fetch(
            &self.http,
            &page_url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        let html = fetched.text();
        let vid = util::search(&RE_VID, &html).unwrap_or_else(|| id.to_string());
        let page_hash = util::search(&RE_HASH, &html)
            .ok_or_else(|| ResolveError::malformed(url, "the video page carries no hash"))?;
        let hash = api_hash(&page_hash)
            .ok_or_else(|| ResolveError::malformed(url, "the video page's hash is not hex"))?;
        let api = util::with_query(
            &Url::parse(&format!("{SITE}xhr/video/{vid}")).expect("valid"),
            &[
                ("hash", hash.as_str()),
                ("device", "generic"),
                ("domain", "www.eporner.com"),
                ("fallback", "false"),
            ],
        );
        let answer = fetch(
            &self.http,
            &api,
            PLATFORM,
            BROWSER_UA,
            &[
                ("accept".to_string(), "application/json".to_string()),
                ("referer".to_string(), page_url.to_string()),
            ],
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(answer.status, url) {
            return Err(error);
        }
        let video = answer.json(url)?;
        if video["available"].as_bool() == Some(false) {
            return Err(ResolveError::unavailable(
                url,
                util::text(&video["message"])
                    .and_then(|m| clean_title(&m))
                    .unwrap_or_else(|| "the video is not available".to_string()),
            ));
        }
        let variants = variants_of(&video, html.contains("download-av1"));
        if variants.is_empty() {
            return Err(ResolveError::unavailable(
                url,
                "the video API names no file",
            ));
        }
        let page = Page::parse(&html, &fetched.url);
        let objects = page.ld_json();
        let video_object = page::ld_objects_of_type(&objects, "VideoObject")
            .into_iter()
            .next();
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(vid);
        resolved.title = page
            .meta("og:title")
            .map(|t| t.trim_end_matches(" - EPORNER").to_string())
            .and_then(|t| clean_title(&t))
            .or_else(|| {
                video_object
                    .and_then(|o| util::text(&o["name"]))
                    .and_then(|t| clean_title(&t))
            });
        resolved.description = video_object
            .and_then(|o| util::text(&o["description"]))
            .and_then(|d| clean_title(&d));
        if let Some(caps) = RE_UPLOADER.captures(&html) {
            resolved.uploader = clean_title(&util::html_unescape(&caps[2]));
            resolved.uploader_url = Url::parse(SITE).ok().and_then(|s| s.join(&caps[1]).ok());
        }
        resolved.uploaded_at = video_object.and_then(|o| util::time(&o["uploadDate"]));
        resolved.duration = util::seconds(&video["duration"]).or_else(|| {
            video_object
                .and_then(|o| util::text(&o["duration"]).and_then(|d| util::parse_duration(&d)))
        });
        resolved.thumbnail = page.meta("og:image").and_then(|u| Url::parse(&u).ok());
        resolved.webpage_url = page.canonical().or_else(|| Some(fetched.url.clone()));
        resolved.age_limit = Some(18);
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    /// A listing read from `first` onward through its next links, up to the page and
    /// video limits, or the one page `only` names.
    async fn resolve_listing(
        &self,
        id: String,
        first: Url,
        only: Option<u32>,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let mut entries: Vec<PlaylistEntry> = Vec::new();
        let mut title = None;
        let mut next = Some(first);
        let mut turned = 0;
        while let Some(page_url) = next.take() {
            let fetched = fetch(
                &self.http,
                &page_url,
                PLATFORM,
                BROWSER_UA,
                &navigation_headers(),
                MAX_PAGE,
            )
            .await?;
            if let Some(error) = status_error(fetched.status, url) {
                if turned > 0 && matches!(error, ResolveError::NotFound(_)) {
                    break;
                }
                return Err(error);
            }
            turned += 1;
            let html = fetched.text();
            title = title.or_else(|| heading(&html));
            let found = listing_entries(&html);
            if found.is_empty() {
                break;
            }
            for entry in found {
                if !entries.iter().any(|e| e.url == entry.url) {
                    entries.push(entry);
                }
            }
            if only.is_some() || turned >= PAGE_LIMIT || entries.len() >= LISTING_LIMIT {
                break;
            }
            next = next_page(&html, &fetched.url);
        }
        entries.truncate(LISTING_LIMIT);
        if entries.is_empty() {
            return Err(ResolveError::unavailable(
                url,
                format!("{id} lists no videos"),
            ));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            title: Some(title.unwrap_or_else(|| id.clone())),
            id: Some(id),
            total: Some(entries.len()),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for EpornerResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Eporner",
            hosts: &["eporner.com"],
            features: &["videos", "embeds", "categories", "profiles", "pornstars"],
            formats: &["mp4", "hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::Nsfw, Tag::Video],
            session: SessionSupport::None,
            examples: &[
                "https://www.eporner.com/video-bnPrqfNPT34/step-sister-asian-pussy-cures-my-depression/",
                "https://www.eporner.com/hd-porn/UF6oacvBq6P/Fantastic-Girl-Banged-By-Masseur/",
                "https://www.eporner.com/embed/rd6LL5J4ufv/",
                "https://www.eporner.com/cat/teens/",
                "https://www.eporner.com/profile/xdf1xd/",
                "https://www.eporner.com/pornstar/manuel-ferrara/",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Video { id } => self.resolve_video(&id, url).await,
            Link::Category { slug, page } => {
                let first = match page {
                    Some(number) => format!("{SITE}cat/{slug}/{number}/"),
                    None => format!("{SITE}cat/{slug}/"),
                };
                self.resolve_listing(
                    format!("cat/{slug}"),
                    Url::parse(&first).expect("valid"),
                    page,
                    url,
                )
                .await
            }
            Link::Profile { name } => {
                self.resolve_listing(
                    format!("profile/{name}"),
                    Url::parse(&format!("{SITE}profile/{name}/")).expect("valid"),
                    None,
                    url,
                )
                .await
            }
            Link::Pornstar { slug, page } => {
                let first = match page {
                    Some(number) => format!("{SITE}pornstar/{slug}/{number}/"),
                    None => format!("{SITE}pornstar/{slug}/"),
                };
                self.resolve_listing(
                    format!("pornstar/{slug}"),
                    Url::parse(&first).expect("valid"),
                    page,
                    url,
                )
                .await
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

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let video = |id: &str| Some(Link::Video { id: id.into() });
        assert_eq!(
            link("https://www.eporner.com/video-bnPrqfNPT34/step-sister-asian-pussy/"),
            video("bnPrqfNPT34")
        );
        assert_eq!(
            link("http://www.eporner.com/hd-porn/3YRUtzMcWn0/Star-Wars-XXX-Parody/"),
            video("3YRUtzMcWn0")
        );
        assert_eq!(
            link("https://eporner.com/hd-porn/3YRUtzMcWn0"),
            video("3YRUtzMcWn0")
        );
        assert_eq!(
            link("https://www.eporner.com/embed/3YRUtzMcWn0"),
            video("3YRUtzMcWn0")
        );
        assert_eq!(
            link("https://www.eporner.com/cat/teens/"),
            Some(Link::Category {
                slug: "teens".into(),
                page: None
            })
        );
        assert_eq!(
            link("https://www.eporner.com/cat/teens/2/"),
            Some(Link::Category {
                slug: "teens".into(),
                page: Some(2)
            })
        );
        assert_eq!(
            link("https://www.eporner.com/profile/xdf1xd/"),
            Some(Link::Profile {
                name: "xdf1xd".into()
            })
        );
        assert_eq!(
            link("https://www.eporner.com/pornstar/manuel-ferrara/3/"),
            Some(Link::Pornstar {
                slug: "manuel-ferrara".into(),
                page: Some(3)
            })
        );
        assert_eq!(link("https://www.eporner.com/"), None);
        assert_eq!(link("https://www.eporner.com/search/anal/"), None);
        assert_eq!(link("https://www.eporner.com/cat/teens/x/"), None);
        assert_eq!(link("https://example.com/video-bnPrqfNPT34/x/"), None);
    }

    #[test]
    fn the_api_hash_is_the_page_hash_in_base_36_pieces() {
        assert_eq!(
            api_hash("52122d2fc67705b528e86aea67d25713").as_deref(),
            Some("mrs8j31j2eshhbcm86ist1mvn")
        );
        assert_eq!(base36(0), "0");
        assert_eq!(base36(35), "z");
        assert_eq!(base36(36), "10");
        assert_eq!(api_hash("not a hash"), None);
    }

    const VIDEO_PAGE: &str = concat!(
        r#"<html><head><meta property="og:title" content="Step Sister Asian Pussy Cures My Depression - EPORNER" />"#,
        r#"<meta property="og:image" content="https://static-ca-cdn.eporner.com/thumbs/8_240.jpg" />"#,
        r#"<link rel="canonical" href="https://www.eporner.com/video-bnPrqfNPT34/step-sister-asian-pussy-cures-my-depression/" />"#,
        r#"<script type="application/ld+json">{"@context": "http://schema.org/", "@type": "VideoObject", "name": "Step Sister Asian Pussy Cures My Depression", "duration": "PT0H32M33S", "description": "Vivianne Vo HouseHoldFantasy", "uploadDate": "2026-08-03T10:27:56+02:00"}</script>"#,
        r#"</head><body><script>EP.video.player.vid = 'bnPrqfNPT34'; EP.video.player.hash = '52122d2fc67705b528e86aea67d25713';</script>"#,
        r#"<div id="video-info"><h1>Step Sister Asian Pussy Cures My Depression</h1><ul><li class="vit-uploader"><a href="/profile/xdf1xd/" title="Uploader">xdf1xd</a></li></ul></div>"#,
        r##"<a class="download-av1" href="#">AV1</a></body></html>"##
    );

    fn api_answer() -> Value {
        json!({
            "vid": "bnPrqfNPT34", "duration": 1953, "available": true, "code": 0, "message": "",
            "sources": {
                "mp4": {
                    "1080p@60fps HD": {"labelShort": "1080p", "src": "https://vid-s3.eporner.com/v6/abc/17842243-1080p.mp4", "type": "video/mp4", "default": "False"},
                    "480p": {"labelShort": "480p", "src": "https://vid-s3.eporner.com/v6/abc/17842243-480p.mp4", "type": "video/mp4", "default": "True"}
                },
                "hls": {
                    "auto": {"labelShort": "auto", "src": "https://vid-s3.eporner.com/v6/abc/17842243.m3u8", "type": "application/x-mpegURL"}
                }
            }
        })
    }

    #[tokio::test]
    async fn videos_resolve_through_the_hashed_api() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "https://www.eporner.com/video-bnPrqfNPT34/",
            200,
            "text/html",
            VIDEO_PAGE,
        ));
        fixture.exchanges.push(exchange(
            "https://www.eporner.com/xhr/video/bnPrqfNPT34?hash=mrs8j31j2eshhbcm86ist1mvn&device=generic&domain=www.eporner.com&fallback=false",
            200,
            "application/json",
            &api_answer().to_string(),
        ));
        let resolver = EpornerResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.eporner.com/embed/bnPrqfNPT34/").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("bnPrqfNPT34"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Step Sister Asian Pussy Cures My Depression")
        );
        assert_eq!(
            resolved.description.as_deref(),
            Some("Vivianne Vo HouseHoldFantasy")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("xdf1xd"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://www.eporner.com/profile/xdf1xd/"
        );
        assert_eq!(
            resolved.duration,
            Some(std::time::Duration::from_secs(1953))
        );
        assert_eq!(
            resolved.uploaded_at.unwrap().to_string(),
            "2026-08-03T08:27:56Z"
        );
        assert_eq!(resolved.age_limit, Some(18));
        assert_eq!(
            resolved.webpage_url.as_ref().unwrap().as_str(),
            "https://www.eporner.com/video-bnPrqfNPT34/step-sister-asian-pussy-cures-my-depression/"
        );
        assert_eq!(
            resolved.variants.len(),
            5,
            "two MP4s, their AV1 twins, and HLS"
        );
        let best = &resolved.variants[0];
        assert_eq!(best.height, Some(1080));
        assert_eq!(best.fps, Some(60.0));
        assert_eq!(best.video, Some(VideoCodec::H264));
        assert_eq!(best.format_id.as_deref(), Some("1080p@60fps HD"));
        assert_eq!(best.label.as_deref(), Some("1080p"));
        let av1 = &resolved.variants[1];
        assert_eq!(av1.video, Some(VideoCodec::Av1));
        assert_eq!(
            av1.url.as_str(),
            "https://vid-s3.eporner.com/v6/abc/17842243-1080p-av1.mp4"
        );
        assert_eq!(av1.format_id.as_deref(), Some("av1-1080p@60fps HD"));
        let hls = resolved.variants.last().unwrap();
        assert_eq!(hls.kind, VariantKind::Hls);
        assert_eq!(hls.format_id.as_deref(), Some("hls-auto"));
        assert_eq!(variants_of(&api_answer(), false).len(), 3);
    }

    #[tokio::test]
    async fn missing_and_withdrawn_videos_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "https://www.eporner.com/video-3YRUtzMcWn0/",
            404,
            "text/html",
            "<html>gone</html>",
        ));
        fixture.exchanges.push(exchange(
            "https://www.eporner.com/video-AbCdEf1234/",
            200,
            "text/html",
            VIDEO_PAGE.replace("bnPrqfNPT34", "AbCdEf1234").as_str(),
        ));
        fixture.exchanges.push(exchange(
            "https://www.eporner.com/xhr/video/AbCdEf1234?hash=mrs8j31j2eshhbcm86ist1mvn&device=generic&domain=www.eporner.com&fallback=false",
            200,
            "application/json",
            &json!({"vid": "AbCdEf1234", "available": false, "code": 1, "message": "Video is not available", "sources": {"mp4": {}}}).to_string(),
        ));
        let resolver = EpornerResolver::new(Http::replay(fixture));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.eporner.com/hd-porn/3YRUtzMcWn0/").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(&Url::parse("https://www.eporner.com/video-AbCdEf1234/x/").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "Video is not available"),
            "{error}"
        );
    }

    fn item(id: &str, title: &str, duration: &str) -> String {
        format!(
            concat!(
                r#"<div class="mb hdy" data-id="1" id="vf1"><div class="mbimg"><div class="mbcontent"><a href="/video-{id}/slug/"><img src="x.jpg" alt="{title}" /></a></div></div>"#,
                r#"<div class="mbunder"><p class="mbtit"><a href="/video-{id}/slug/">{title}</a></p><p class="mbstats"><span class="mbtim" title="Duration">{duration}</span></p></div></div>"#
            ),
            id = id,
            title = title,
            duration = duration
        )
    }

    #[tokio::test]
    async fn categories_profiles_and_pornstars_list_their_videos() {
        let page1 = format!(
            r#"<html><head><link rel="next" href="https://www.eporner.com/cat/teens/2/"></head><body><h1>Teen Porn Videos<i class="eighteenplus"></i></h1>{}{}</body></html>"#,
            item("E82OWaHFalb", "Eng Sub &amp; Rena", "115:39"),
            item("1BGJvxMJIQw", "Second", "5:27")
        );
        let page2 = format!(
            r#"<html><body><h1>Teen Porn Videos - Page 2</h1>{}</body></html>"#,
            item("ZQ68dkweBLS", "Third", "40:23")
        );
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "https://www.eporner.com/cat/teens/",
            200,
            "text/html",
            &page1,
        ));
        fixture.exchanges.push(exchange(
            "https://www.eporner.com/cat/teens/2/",
            200,
            "text/html",
            &page2,
        ));
        fixture.exchanges.push(exchange(
            "https://www.eporner.com/profile/nobody/",
            404,
            "text/html",
            "<html>gone</html>",
        ));
        let resolver = EpornerResolver::new(Http::replay(fixture));
        let Resolution::Playlist(playlist) = resolver
            .resolve(&Url::parse("https://www.eporner.com/cat/teens/").unwrap())
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(playlist.id.as_deref(), Some("cat/teens"));
        assert_eq!(playlist.title.as_deref(), Some("Teen Porn Videos"));
        assert_eq!(playlist.entries.len(), 3);
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://www.eporner.com/video-E82OWaHFalb/"
        );
        assert_eq!(playlist.entries[0].title.as_deref(), Some("Eng Sub & Rena"));
        assert_eq!(
            playlist.entries[0].duration,
            Some(std::time::Duration::from_secs(6939))
        );
        assert_eq!(playlist.entries[2].title.as_deref(), Some("Third"));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.eporner.com/profile/nobody/").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    /// Every example link resolves live: videos with a playable MP4, listings with
    /// entries.
    #[ignore = "reaches the live site: cargo test -- --ignored"]
    #[tokio::test]

    async fn live_examples_resolve() {
        use std::time::Duration;

        let resolver = EpornerResolver::new(Http::new(crate::http::HttpConfig::default()));
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
                        resolved
                            .variants
                            .iter()
                            .any(|v| v.is_playable() && v.kind == VariantKind::File),
                        "{link}: no playable MP4"
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
                        "{link}: {:?} with {} entries",
                        playlist.title,
                        playlist.entries.len()
                    );
                }
            }
        }
    }
}
