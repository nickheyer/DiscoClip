//! SpankBang videos, playlists and profiles: the site sits behind Cloudflare and answers
//! only a browser's TLS fingerprint, and challenges Chrome's while letting Safari's
//! through, so every page is read as Safari. A video page names
//! its stream key, which the stream API turns into MP4 files by height, an HLS master
//! and, when the site offers one, a DASH manifest. Playlist and profile pages list their
//! videos a page at a time, with how many there are in all. spankbang.party serves the
//! same site.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionSupport, Tag, Variant, VariantKind, clean_title, dash, fetch_as_browser_of,
    hls, status_error, util,
};
use crate::http::{BROWSER_UA, Browser, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "spankbang";
/// How many pages of a listing are read.
const LISTING_PAGES: u32 = 3;

static RE_HOST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[a-z0-9-]+\.)*spankbang\.(com|party)$").unwrap());
/// `/{id}/video/{slug}`, `/{id}/play/{slug}`, `/{id}/embed/`.
static RE_VIDEO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/([0-9a-z]+)/(?:video|play|embed)(?:/|$)").unwrap());
/// `/{playlist}-{entry}/playlist/{slug}`: a video as a playlist lists it.
static RE_ENTRY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/([0-9a-z]+)-([0-9a-z]+)/playlist/[^/?#]+/?$").unwrap());
/// `/{id}/playlist/{slug}`, `/{id}/playlist/{slug}/{page}/`.
static RE_PLAYLIST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/([0-9a-z]+)/playlist/([^/?#]+)(?:/(\d+))?/?$").unwrap());
/// `/profile/{name}`, `/profile/{name}/videos`, paged as `/profile/{name}/videos?page=N`.
static RE_PROFILE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/profile/([A-Za-z0-9_.-]+)(?:/videos)?$").unwrap());
/// `data-streamkey="…"`: what the stream API is asked for.
static RE_STREAM_KEY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-streamkey\s*=\s*["']([^"']+)["']"#).unwrap());
/// `<h1 … data-testid=video-title>…</h1>`.
static RE_VIDEO_TITLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<h1[^>]*data-testid=["']?video-title["']?[^>]*>(.*?)</h1>"#).unwrap()
});
/// `<h1 … data-testid=playlist-title>… Playlist</h1>`.
static RE_PLAYLIST_TITLE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<h1[^>]*data-testid=["']?playlist-title["']?[^>]*>(.*?)</h1>"#).unwrap()
});
/// `<h1 class="profile_name">…</h1>`.
static RE_PROFILE_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<h1[^>]*class=["']profile_name["'][^>]*>(.*?)</h1>"#).unwrap()
});
/// The uploader's profile link in the owner block under the player.
static RE_OWNER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?s)data-testid=["']owner-details["'].*?<a[^>]+href=["']/profile/([A-Za-z0-9_.-]+)["']"#,
    )
    .unwrap()
});
/// `<div … id="video_removed">` or `class="video_removed"`.
static RE_REMOVED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<[^>]+\b(?:id|class)=["']video_removed"#).unwrap());
/// A video or playlist entry link in a listing.
static RE_ITEM_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"<a[^>]+href=["'](/[0-9a-z]+(?:-[0-9a-z]+)?/(?:video|playlist)/[^"'?#]+)["']([^>]*)>"#,
    )
    .unwrap()
});
/// `<img … alt="…">`: the thumbnail's caption, which is the title.
static RE_ALT: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"\balt=["']([^"']*)["']"#).unwrap());
/// `data-testid="video-item-length"> 12m <`.
static RE_LENGTH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"data-testid=["']video-item-length["'][^>]*>\s*([^<]+?)\s*<"#).unwrap()
});
/// `in total <b>28</b>`: how many a listing holds.
static RE_TOTAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"in total\s*<b>\s*([\d,]+)\s*</b>").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A video page, or a playlist's entry page that leads to one.
    Video { site: String, path: String },
    /// A playlist, from `page` on.
    Playlist {
        site: String,
        id: String,
        slug: String,
        page: u32,
    },
    /// A user's videos, from `page` on.
    Profile {
        site: String,
        name: String,
        page: u32,
    },
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
    let tld = RE_HOST.captures(&host)?[1].to_string();
    let site = format!("https://spankbang.{tld}");
    let path = url.path();
    if let Some(caps) = RE_VIDEO.captures(path) {
        return Some(Link::Video {
            site,
            path: format!("/{}/video/", &caps[1]),
        });
    }
    if RE_ENTRY.is_match(path) {
        return Some(Link::Video {
            site,
            path: path.trim_end_matches('/').to_string(),
        });
    }
    if let Some(caps) = RE_PLAYLIST.captures(path) {
        return Some(Link::Playlist {
            site,
            id: caps[1].to_string(),
            slug: caps[2].to_string(),
            page: page_number(caps.get(3)),
        });
    }
    if let Some(caps) = RE_PROFILE.captures(path) {
        return Some(Link::Profile {
            site,
            name: caps[1].to_string(),
            page: util::query_param(url, "page")
                .and_then(|p| p.parse().ok())
                .filter(|p| *p >= 1)
                .unwrap_or(1),
        });
    }
    None
}

/// The height a stream key names: `480p`, `4k`.
fn height_of(key: &str) -> Option<u32> {
    match key {
        "4k" => Some(2160),
        _ => key.strip_suffix('p').and_then(|h| h.parse().ok()),
    }
}

/// The first link of a stream entry: the API lists each as an array.
fn first_link(entry: &Value) -> Option<Url> {
    match entry {
        Value::Array(items) => items.first().and_then(|item| util::url_of(item, None)),
        other => util::url_of(other, None),
    }
}

/// `<div x-data="videoList" class="…">`: a block of videos on a listing page.
static RE_VIDEO_LIST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<div\s[^>]*x-data=["']videoList["'][^>]*>"#).unwrap());

/// The byte ranges of the promoted blocks a listing page opens with: a video list laid
/// out with `js-forced-layout`, up to the next video list. Their videos are the site's
/// picks, not the listing's own.
fn promoted_ranges(html: &str) -> Vec<(usize, usize)> {
    let lists: Vec<(usize, bool)> = RE_VIDEO_LIST
        .find_iter(html)
        .map(|m| (m.start(), m.as_str().contains("js-forced-layout")))
        .collect();
    lists
        .iter()
        .enumerate()
        .filter(|(_, (_, promoted))| *promoted)
        .map(|(index, (start, _))| {
            let end = lists.get(index + 1).map_or(html.len(), |(next, _)| *next);
            (*start, end)
        })
        .collect()
}

/// The videos a listing page carries as its own, each once: a link, its caption and its
/// length. The promoted block every listing page opens with is left out.
pub fn listing_items(html: &str, site: &str) -> Vec<PlaylistEntry> {
    let mut entries: Vec<PlaylistEntry> = Vec::new();
    let promoted = promoted_ranges(html);
    let starts: Vec<usize> = html
        .match_indices("data-testid=\"video-item\"")
        .map(|(i, _)| i)
        .collect();
    for (index, start) in starts.iter().enumerate() {
        if promoted
            .iter()
            .any(|(from, to)| (from..to).contains(&start))
        {
            continue;
        }
        let end = starts.get(index + 1).copied().unwrap_or(html.len());
        let block = &html[*start..end];
        let mut link = None;
        let mut title = None;
        for caps in RE_ITEM_LINK.captures_iter(block) {
            let Ok(url) = Url::parse(&format!("{site}{}", &caps[1])) else {
                continue;
            };
            let attributes = &caps[2];
            if title.is_none() {
                title = util::attribute(&format!("<a{attributes}>"), "title")
                    .and_then(|t| clean_title(&util::html_unescape(&t)));
            }
            link.get_or_insert(url);
        }
        let Some(url) = link else {
            continue;
        };
        if entries.iter().any(|e| e.url == url) {
            continue;
        }
        entries.push(PlaylistEntry {
            url,
            title: title.or_else(|| {
                RE_ALT
                    .captures(block)
                    .and_then(|c| clean_title(&util::html_unescape(&c[1])))
            }),
            duration: RE_LENGTH
                .captures(block)
                .and_then(|c| util::parse_duration(&c[1])),
        });
    }
    entries
}

/// How many videos a listing reports.
pub fn listing_total(html: &str) -> Option<usize> {
    RE_TOTAL
        .captures(html)
        .and_then(|c| c[1].replace(',', "").parse().ok())
}

pub struct SpankbangResolver {
    http: Http,
}

impl SpankbangResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// A page read as Safari, whose fingerprint the site's bot check lets through where
    /// it challenges Chrome's: its HTML and where it ended up.
    async fn page(&self, page_url: &Url, origin: &Url) -> Result<(String, Url), ResolveError> {
        let fetched = fetch_as_browser_of(
            &self.http,
            page_url,
            PLATFORM,
            Browser::Safari,
            &[("accept-language".to_string(), "en-US,en;q=0.9".to_string())],
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(fetched.status, origin) {
            return Err(error);
        }
        Ok((fetched.text(), fetched.url))
    }

    /// The streams the API hands out for a stream key: files by height, the HLS master
    /// and the DASH manifest, expanded.
    async fn streams(
        &self,
        site: &str,
        key: &str,
        page_url: &Url,
        origin: &Url,
    ) -> Result<(Vec<Variant>, Vec<super::SubtitleTrack>, Option<Duration>), ResolveError> {
        let api = Url::parse(&format!("{site}/api/videos/stream")).expect("valid");
        let response = self
            .http
            .post(api.clone())
            .platform(PLATFORM)
            .impersonate_as(Browser::Safari)
            .header("accept", "application/json")
            .header("referer", page_url.as_str())
            .header("x-requested-with", "XMLHttpRequest")
            .form(&[("id", key), ("data", "0")])
            .send()
            .await?;
        if let Some(error) = status_error(response.status, origin) {
            return Err(error);
        }
        let streams: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, format!("stream JSON: {e}")))?;
        let mut variants: Vec<Variant> = Vec::new();
        let mut subtitles = Vec::new();
        let mut duration = util::seconds(&streams["length"]);
        let push = |variant: Variant, variants: &mut Vec<Variant>| {
            if !variants.iter().any(|v| v.url == variant.url) {
                variants.push(variant);
            }
        };
        for (key, entry) in streams.as_object().into_iter().flatten() {
            let Some(height) = height_of(key) else {
                continue;
            };
            let Some(url) = first_link(entry) else {
                continue;
            };
            let mut variant = Variant::file(url);
            variant.container = Some(Container::Mp4);
            variant.video = Some(VideoCodec::H264);
            variant.audio = Some(AudioCodec::Aac);
            variant.height = Some(height);
            variant.format_id = Some(format!("mp4-{key}"));
            variant.label = Some(key.clone());
            push(variant, &mut variants);
        }
        let mut master_expanded = false;
        if let Some(master) = first_link(&streams["m3u8"]) {
            match hls::expand(&self.http, &master, PLATFORM, BROWSER_UA, &[]).await {
                Ok(expanded) => {
                    master_expanded = !expanded.variants.is_empty();
                    for mut variant in expanded.variants {
                        let label = variant
                            .height
                            .map(|h| format!("{h}p"))
                            .unwrap_or_else(|| "hls".to_string());
                        variant.format_id = Some(format!("hls-{label}"));
                        variant.label = Some(label);
                        push(variant, &mut variants);
                    }
                    subtitles.extend(expanded.subtitles);
                    duration = duration.or(expanded.duration);
                }
                Err(error) => {
                    tracing::debug!(%master, "SpankBang HLS master not expanded: {error}");
                }
            }
        }
        if !master_expanded {
            for (key, entry) in streams.as_object().into_iter().flatten() {
                let Some(quality) = key.strip_prefix("m3u8_") else {
                    continue;
                };
                let Some(url) = first_link(entry) else {
                    continue;
                };
                let mut variant = Variant::new(url, VariantKind::Hls);
                variant.height = height_of(quality);
                variant.format_id = Some(format!("hls-{quality}"));
                variant.label = Some(quality.to_string());
                push(variant, &mut variants);
            }
        }
        if let Some(manifest) = first_link(&streams["mpd"]) {
            match dash::expand(&self.http, &manifest, PLATFORM, BROWSER_UA, &[]).await {
                Ok(expanded) => {
                    for mut variant in expanded.variants {
                        if variant.format_id.is_none() {
                            variant.format_id = Some(match variant.height {
                                Some(h) => format!("dash-{h}p"),
                                None => "dash".to_string(),
                            });
                        }
                        push(variant, &mut variants);
                    }
                    subtitles.extend(expanded.subtitles);
                    duration = duration.or(expanded.duration);
                }
                Err(error) => {
                    tracing::debug!(%manifest, "SpankBang DASH manifest not expanded: {error}");
                }
            }
        }
        Ok((variants, subtitles, duration))
    }

    async fn resolve_video(
        &self,
        site: &str,
        path: &str,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let page_url = Url::parse(&format!("{site}{path}"))
            .map_err(|e| ResolveError::malformed(url, e.to_string()))?;
        let (html, final_url) = self.page(&page_url, url).await?;
        if RE_REMOVED.is_match(&html) {
            return Err(ResolveError::unavailable(url, "the video was removed"));
        }
        let key = RE_STREAM_KEY
            .captures(&html)
            .map(|c| c[1].to_string())
            .ok_or_else(|| ResolveError::unavailable(url, "the page names no stream key"))?;
        let (variants, subtitles, duration) = self.streams(site, &key, &final_url, url).await?;
        if variants.is_empty() {
            return Err(ResolveError::unavailable(
                url,
                "the stream API named no stream",
            ));
        }
        let page = Page::parse(&html, &final_url);
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = RE_VIDEO
            .captures(final_url.path())
            .map(|c| c[1].to_string())
            .or_else(|| RE_VIDEO.captures(path).map(|c| c[1].to_string()));
        resolved.title = RE_VIDEO_TITLE
            .captures(&html)
            .and_then(|c| clean_title(&util::clean_html(&c[1])))
            .or_else(|| {
                page.meta("og:title").and_then(|t| {
                    clean_title(
                        t.trim_end_matches(" - SpankBang")
                            .rsplit_once(": ")
                            .map_or(&t, |(name, _)| name),
                    )
                })
            });
        resolved.duration = page
            .meta("og:video:duration")
            .and_then(|d| d.trim().parse::<u64>().ok())
            .filter(|d| *d > 0)
            .map(Duration::from_secs)
            .or(duration);
        resolved.thumbnail = page.meta("og:image").and_then(|t| Url::parse(&t).ok());
        if let Some(caps) = RE_OWNER.captures(&html) {
            let name = caps[1].to_string();
            resolved.uploader_url = Url::parse(&format!("{site}/profile/{name}")).ok();
            resolved.uploader = Some(name);
        }
        resolved.webpage_url = Some(final_url);
        resolved.age_limit = Some(18);
        resolved.subtitles = subtitles;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    /// Reads a listing from `first_page` on, each page's link made by `page_url`, until
    /// the pages run out, the total is reached or `LISTING_PAGES` have been read.
    async fn resolve_listing(
        &self,
        site: &str,
        page_url: &(dyn Fn(u32) -> String + Sync),
        id: &str,
        title_of: fn(&str) -> Option<String>,
        first_page: u32,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let mut entries: Vec<PlaylistEntry> = Vec::new();
        let mut title = None;
        let mut total = None;
        let mut page_number = first_page;
        loop {
            let page_url = Url::parse(&page_url(page_number))
                .map_err(|e| ResolveError::malformed(url, e.to_string()))?;
            let (html, _) = match self.page(&page_url, url).await {
                Ok(page) => page,
                // A page past the last answers 404.
                Err(ResolveError::NotFound(_)) if page_number > first_page => break,
                Err(error) => return Err(error),
            };
            if title.is_none() {
                title = title_of(&html);
            }
            total = total.or_else(|| listing_total(&html));
            let before = entries.len();
            for entry in listing_items(&html, site) {
                if !entries.iter().any(|e| e.url == entry.url) {
                    entries.push(entry);
                }
            }
            page_number += 1;
            if entries.len() == before
                || total.is_some_and(|n| entries.len() >= n)
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
            id: Some(id.to_string()),
            title,
            total: total.filter(|n| *n >= entries.len()),
            entries,
        }))
    }
}

fn playlist_title(html: &str) -> Option<String> {
    RE_PLAYLIST_TITLE
        .captures(html)
        .and_then(|c| clean_title(&util::clean_html(&c[1])))
        .map(|t| t.trim_end_matches(" Playlist").to_string())
}

fn profile_title(html: &str) -> Option<String> {
    RE_PROFILE_NAME
        .captures(html)
        .and_then(|c| clean_title(&util::clean_html(&c[1])))
}

#[async_trait]
impl Resolver for SpankbangResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "SpankBang",
            hosts: &["spankbang.com", "spankbang.party"],
            features: &["videos", "embeds", "playlists", "profiles"],
            formats: &["mp4", "hls", "dash"],
            media: &[MediaKind::Video],
            tags: &[Tag::Nsfw, Tag::Video],
            session: SessionSupport::None,
            examples: &[
                "https://spankbang.com/a58qa/video/pornstarplatinum+leya+falcon+gets+pounded+hard+in+a+wild+interracial+play",
                "https://spankbang.com/a58qa/embed/",
                "https://spankbang.com/ug0k/playlist/big+ass+titties",
                "https://spankbang.party/ug0k/playlist/big+ass+titties",
                "https://spankbang.com/profile/mindself",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Video { site, path } => self.resolve_video(&site, &path, url).await,
            Link::Playlist {
                site,
                id,
                slug,
                page,
            } => {
                // `/{id}/playlist/{slug}/`, then `/{id}/playlist/{slug}/{page}/`.
                let page_url = |page: u32| match page {
                    1 => format!("{site}/{id}/playlist/{slug}/"),
                    _ => format!("{site}/{id}/playlist/{slug}/{page}/"),
                };
                self.resolve_listing(&site, &page_url, &id, playlist_title, page, url)
                    .await
            }
            Link::Profile { site, name, page } => {
                // `/profile/{name}/videos`, then `/profile/{name}/videos?page={page}`; a
                // trailing slash on a profile path answers 404.
                let page_url = |page: u32| match page {
                    1 => format!("{site}/profile/{name}/videos"),
                    _ => format!("{site}/profile/{name}/videos?page={page}"),
                };
                self.resolve_listing(&site, &page_url, &name, profile_title, page, url)
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
    use serde_json::json;

    fn exchange(
        method: &str,
        url: &str,
        status: u16,
        content_type: &str,
        body: String,
    ) -> Exchange {
        exchange_at(method, url, url, status, content_type, body)
    }

    fn exchange_at(
        method: &str,
        url: &str,
        final_url: &str,
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
                url: final_url.into(),
                headers: vec![("content-type".into(), content_type.into())],
                body: RecordedBody::Text(body),
                truncated: false,
            },
        }
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let site = |s: &str| s.to_string();
        assert_eq!(
            link("https://spankbang.com/5514q/video/woodman+x"),
            Some(Link::Video {
                site: site("https://spankbang.com"),
                path: "/5514q/video/".into()
            })
        );
        assert_eq!(
            link("https://m.spankbang.com/3vvn/play/fantasy+solo/480p/"),
            Some(Link::Video {
                site: site("https://spankbang.com"),
                path: "/3vvn/video/".into()
            })
        );
        assert_eq!(
            link("https://spankbang.party/2y3td/embed/"),
            Some(Link::Video {
                site: site("https://spankbang.party"),
                path: "/2y3td/video/".into()
            })
        );
        assert_eq!(
            link("https://spankbang.com/ug0k-7ardd/playlist/big+ass+titties"),
            Some(Link::Video {
                site: site("https://spankbang.com"),
                path: "/ug0k-7ardd/playlist/big+ass+titties".into()
            })
        );
        assert_eq!(
            link("https://spankbang.com/ug0k/playlist/big+ass+titties/"),
            Some(Link::Playlist {
                site: site("https://spankbang.com"),
                id: "ug0k".into(),
                slug: "big+ass+titties".into(),
                page: 1
            })
        );
        assert_eq!(
            link("https://spankbang.com/ug0k/playlist/big+ass+titties/3/"),
            Some(Link::Playlist {
                site: site("https://spankbang.com"),
                id: "ug0k".into(),
                slug: "big+ass+titties".into(),
                page: 3
            })
        );
        assert_eq!(
            link("https://spankbang.com/profile/mindself"),
            Some(Link::Profile {
                site: site("https://spankbang.com"),
                name: "mindself".into(),
                page: 1
            })
        );
        assert_eq!(
            link("https://spankbang.com/profile/mindself/videos?page=2"),
            Some(Link::Profile {
                site: site("https://spankbang.com"),
                name: "mindself".into(),
                page: 2
            })
        );
        assert_eq!(link("https://spankbang.com/"), None);
        assert_eq!(link("https://spankbang.com/new_videos/"), None);
        assert_eq!(
            link("https://spankbang.com/mh/channel/pornstarplatinum/"),
            None
        );
        assert_eq!(
            link("https://spankbang.com/profile/mindself/playlists"),
            None
        );
        assert_eq!(link("https://example.com/5514q/video/x"), None);
    }

    #[test]
    fn promoted_blocks_are_left_out_of_listings() {
        let html = concat!(
            r#"<div x-data="videoList" class="js-media-list js-forced-layout grid-cols-4 grid">"#,
            r#"<div data-testid="video-item"><a href="/aaaa1/video/promoted+one" title="Promoted one"></a></div>"#,
            r#"<div data-testid="video-item"><a href="/aaaa2/video/promoted+two" title="Promoted two"></a></div>"#,
            r#"</div><div x-data="videoList" class="js-media-list grid h-fit">"#,
            r#"<div data-testid="video-item"><a href="/bbbb1/video/own+one" title="Own one"></a>"#,
            r#"<div data-testid="video-item-length"> 12m </div></div>"#,
            r#"<div data-testid="video-item"><a href="/bbbb2/video/own+two" title="Own two"></a></div>"#,
            r#"</div>"#
        );
        let items = listing_items(html, "https://spankbang.com");
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].url.as_str(),
            "https://spankbang.com/bbbb1/video/own+one"
        );
        assert_eq!(items[0].title.as_deref(), Some("Own one"));
        assert_eq!(items[0].duration, Some(Duration::from_secs(720)));
        assert_eq!(items[1].title.as_deref(), Some("Own two"));
        let bare = r#"<div data-testid="video-item"><a href="/cccc1/video/x" title="X"></a></div>"#;
        assert_eq!(listing_items(bare, "https://spankbang.com").len(), 1);
    }

    const VIDEO_PAGE: &str = r#"<html><head><title>woodman x: Fansly &amp; Anal Sex Porn - SpankBang</title>
        <meta property="og:title" content="woodman x: Fansly &amp; Anal Sex Porn - SpankBang" />
        <meta property="og:image" content="https://tbi.sb-cd.com/t/8632826/fa/a3/w:500/t6-enh/woodman-x.jpg" />
        <meta property="og:video:duration" content="13926" /></head><body>
        <div id="video" data-streamkey="ODYzMjgyNg.tawTmUEqu7c_2TaE9HfZU35tvfE"></div>
        <h1 class="text-primary" data-testid=video-title>woodman x</h1>
        <div data-testid="owner-details"><div data-testid="profile"><a href="/profile/peludo93" aria-label="None"><img alt="peludo93" /></a>
        <a href="/profile/peludo93" class="text-link-primary"><p class="text-link-secondary">peludo93</p></a></div></div>
        </body></html>"#;

    #[tokio::test]
    async fn videos_resolve_to_files_by_height_and_hls_renditions() {
        let master = "https://hls-uranus.sb-cd.com/hls/8/6/8632826-,240p,480p,.mp4.urlset/master.m3u8?secure=s";
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange_at(
            "GET",
            "https://spankbang.com/5514q/video/",
            "https://spankbang.com/5514q/video/woodman+x",
            200,
            "text/html",
            VIDEO_PAGE.into(),
        ));
        fixture.exchanges.push(exchange(
            "POST",
            "https://spankbang.com/api/videos/stream",
            200,
            "application/json",
            json!({
                "1080p": [], "240p": ["https://vdownload-7.sb-cd.com/8/6/8632826-240p.mp4?secure=s"],
                "480p": ["https://vdownload-7.sb-cd.com/8/6/8632826-480p.mp4?secure=s"], "4k": [],
                "cover_image": "https://tbi.sb-cd.com/t/8632826/fa/a3/w:800/t6-enh/woodman-x.jpg", "length": 13926,
                "m3u8": [master], "m3u8_240p": ["https://hls-uranus.sb-cd.com/hls/8/6/8632826-,240p,.mp4.urlset/master.m3u8?secure=s"],
                "m3u8_480p": ["https://hls-uranus.sb-cd.com/hls/8/6/8632826-,480p,.mp4.urlset/master.m3u8?secure=s"],
                "mpd": [], "stream_raw_id": 8632826, "stream_u_id": 274333
            })
            .to_string(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            master,
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=229373,RESOLUTION=426x240,FRAME-RATE=25.000,CODECS=\"avc1.42c01e,mp4a.40.2\"\nhttps://hls-uranus.sb-cd.com/hls/8/6/8632826-240p.mp4/index-v1-a1.m3u8?secure=t\n#EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=963694,RESOLUTION=852x480,FRAME-RATE=25.000,CODECS=\"avc1.4d401f,mp4a.40.2\"\nhttps://hls-uranus.sb-cd.com/hls/8/6/8632826-480p.mp4/index-v1-a1.m3u8?secure=t\n".into(),
        ));
        for name in ["240p", "480p"] {
            fixture.exchanges.push(exchange(
                "GET",
                &format!("https://hls-uranus.sb-cd.com/hls/8/6/8632826-{name}.mp4/index-v1-a1.m3u8?secure=t"),
                200,
                "application/vnd.apple.mpegurl",
                "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\nseg-1.ts\n#EXT-X-ENDLIST\n".into(),
            ));
        }
        let resolver = SpankbangResolver::new(Http::replay(fixture));
        let url = Url::parse("https://spankbang.com/5514q/video/woodman+x").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("5514q"));
        assert_eq!(resolved.title.as_deref(), Some("woodman x"));
        assert_eq!(resolved.uploader.as_deref(), Some("peludo93"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://spankbang.com/profile/peludo93"
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(13926)));
        assert_eq!(resolved.age_limit, Some(18));
        assert!(resolved.thumbnail.is_some());
        assert_eq!(
            resolved.webpage_url.as_ref().unwrap().as_str(),
            "https://spankbang.com/5514q/video/woodman+x"
        );
        assert_eq!(resolved.variants.len(), 4, "two files and two renditions");
        let files: Vec<&Variant> = resolved
            .variants
            .iter()
            .filter(|v| v.kind == VariantKind::File)
            .collect();
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].height, Some(240));
        assert_eq!(files[0].format_id.as_deref(), Some("mp4-240p"));
        assert_eq!(files[0].container, Some(Container::Mp4));
        assert_eq!(files[1].height, Some(480));
        let renditions: Vec<&Variant> = resolved
            .variants
            .iter()
            .filter(|v| v.kind == VariantKind::Hls)
            .collect();
        assert_eq!(renditions.len(), 2);
        assert_eq!(renditions[1].height, Some(480));
        assert_eq!(renditions[1].format_id.as_deref(), Some("hls-480p"));
        assert_eq!(renditions[1].video, Some(VideoCodec::H264));
    }

    #[tokio::test]
    async fn removed_and_missing_videos_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "GET",
            "https://spankbang.com/1vt0/video/",
            200,
            "text/html",
            "<html><body><div id=\"video_removed\">This video has been removed</div></body></html>"
                .into(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://spankbang.com/56b3d/video/",
            404,
            "text/html",
            "<html>nothing</html>".into(),
        ));
        let resolver = SpankbangResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://spankbang.com/1vt0/video/solvane+gangbang").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "the video was removed"),
            "{error}"
        );
        assert!(matches!(
            resolver
                .resolve(
                    &Url::parse("https://spankbang.com/56b3d/video/the+slut+maker+hmv").unwrap()
                )
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    fn item(href: &str, title: &str, length: &str) -> String {
        format!(
            r#"<div data-testid="video-item" data-id="1" class="js-video-item"><a href="{href}" class="relative"><picture><img src="https://tbi.sb-cd.com/t/1/w:300/x.jpg" loading="lazy" alt="{title}" /></picture>
            <div class="text-body-sm" data-testid="video-item-length" > {length} </div></a>
            <div data-testid="video-info-with-badge"><p class="line-clamp-2"><a href="{href}" title="{title}" ><span>{title}</span></a></p></div></div>"#
        )
    }

    #[tokio::test]
    async fn playlists_and_profiles_list_their_videos() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "GET",
            "https://spankbang.com/ug0k/playlist/big+ass+titties/",
            200,
            "text/html",
            format!(
                r#"<html><body><h1 class="text-primary" data-testid=playlist-title>Big Ass Titties Playlist</h1>{}{}
                <div class="pagination-page-info">displaying <b>1 - 2</b> records in total <b>3</b></div></body></html>"#,
                item("/ug0k-7ardd/playlist/big+ass+titties", "How I love my women", "12m"),
                item("/ug0k-7as31/playlist/big+ass+titties", "Second", "1h 5m"),
            ),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://spankbang.com/ug0k/playlist/big+ass+titties/2/",
            200,
            "text/html",
            format!(
                r#"<html><body>{}{}</body></html>"#,
                item("/ug0k-7as31/playlist/big+ass+titties", "Second", "1h 5m"),
                item("/ug0k-7f63i/playlist/big+ass+titties", "Third", "45s"),
            ),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://spankbang.com/profile/mindself/videos",
            200,
            "text/html",
            format!(
                r#"<html><body><h1 class="profile_name">mindself</h1>{}
                <div class="pagination-page-info">displaying <b>1 - 1</b> mindself videos in total <b>1</b></div></body></html>"#,
                item("/a58qa/video/pornstarplatinum+leya+falcon", "PORNSTARPLATINUM Leya Falcon", "12m"),
            ),
        ));
        let resolver = SpankbangResolver::new(Http::replay(fixture));
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
        let playlist = list("https://spankbang.com/ug0k/playlist/big+ass+titties").await;
        assert_eq!(playlist.title.as_deref(), Some("Big Ass Titties"));
        assert_eq!(playlist.total, Some(3));
        assert_eq!(playlist.entries.len(), 3, "two pages, each video once");
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://spankbang.com/ug0k-7ardd/playlist/big+ass+titties"
        );
        assert_eq!(
            playlist.entries[0].title.as_deref(),
            Some("How I love my women")
        );
        assert_eq!(playlist.entries[0].duration, Some(Duration::from_secs(720)));
        assert_eq!(
            playlist.entries[1].duration,
            Some(Duration::from_secs(3900))
        );
        assert_eq!(playlist.entries[2].duration, Some(Duration::from_secs(45)));
        assert!(resolver.matches(&playlist.entries[0].url));
        let profile = list("https://spankbang.com/profile/mindself").await;
        assert_eq!(profile.title.as_deref(), Some("mindself"));
        assert_eq!(profile.total, Some(1));
        assert_eq!(profile.entries.len(), 1);
        assert_eq!(
            profile.entries[0].url.as_str(),
            "https://spankbang.com/a58qa/video/pornstarplatinum+leya+falcon"
        );
    }

    /// Every example link resolves live: videos to files and renditions, listings to
    /// entries.
    #[tokio::test]
    #[ignore = "requires live SpankBang access"]
    async fn live_examples_resolve() {
        let resolver = SpankbangResolver::new(Http::new(crate::http::HttpConfig::default()));
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
