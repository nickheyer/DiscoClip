//! Первый канал (1tv.ru): show episodes and fragments, sport pages, news stories and
//! news issues, through the video material lists the site's player reads, and the live
//! channel through the stream API's DASH manifests.
//!
//! Show and sport pages name their player's list link (`data-playlist-url`); news
//! stories inline the video's number in the page's Next.js payload and the player reads
//! `video_materials.json` by it; a news issue lists its fragments, each a story of its
//! own. Every material comes with an HLS master and MP4 files by quality. The live
//! channel's HLS links are bound to a player session, so its DASH manifests are listed.

use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use jiff::civil::Date;
use jiff::tz::TimeZone;
use serde_json::Value;
use url::Url;

use super::page::{Page, json_after, next_flight_data};
use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, Tag, Variant, VariantKind, clean_title, fetch, fetch_ok, hls,
    navigation_headers,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "1tv";
const SITE: &str = "https://www.1tv.ru/";
const MATERIALS: &str = "https://www.1tv.ru/video_materials.json";
const LIVE_STREAMS: &str = "https://stream.1tv.ru/api/playlist/1tvch-v1_as_array.json";
/// The material type of a news story; every other type is a video material.
const NEWS: u64 = 11;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Live,
    /// A player embed naming the material and its type: `/embed/<uid>:<type>`.
    Embed {
        uid: u64,
        kind: u64,
    },
    /// Any other page of the site, by its path.
    Page(String),
}

fn site_host(host: &str) -> bool {
    matches!(
        host,
        "1tv.ru" | "www.1tv.ru" | "sport1tv.ru" | "www.sport1tv.ru" | "static.1tv.ru"
    )
}

/// `<uid>:<type>` as the embed links carry it.
fn parse_material(text: &str) -> Option<(u64, u64)> {
    let (uid, kind) = text.split_once(':')?;
    Some((uid.parse().ok()?, kind.parse().ok()?))
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !site_host(&host) {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    if host == "static.1tv.ru" {
        return match segments.as_slice() {
            ["eump", "embeds", _] => url
                .query_pairs()
                .find(|(k, _)| k == "v")
                .and_then(|(_, v)| parse_material(&v))
                .map(|(uid, kind)| Link::Embed { uid, kind }),
            _ => None,
        };
    }
    match segments.as_slice() {
        ["live"] => Some(Link::Live),
        ["embed", material] => {
            parse_material(material).map(|(uid, kind)| Link::Embed { uid, kind })
        }
        [_, _, ..] => Some(Link::Page(url.path().to_string())),
        _ => None,
    }
}

pub fn embed_url(uid: u64, kind: u64) -> Url {
    Url::parse(&format!("{SITE}embed/{uid}:{kind}")).expect("valid")
}

/// The material ids a player list link names: `video_id`, `videos_ids[]` or `news_ids[]`.
pub fn listed_ids(list: &Url) -> Vec<u64> {
    list.query_pairs()
        .filter(|(k, _)| matches!(k.as_ref(), "video_id" | "videos_ids[]" | "news_ids[]"))
        .filter_map(|(_, v)| v.parse().ok())
        .collect()
}

/// The materials a page's Next.js payload inlines for its player, as `(uid, type)`.
pub fn inlined_materials(flight: &str) -> Vec<(u64, u64)> {
    const KEY: &str = "\"video\":{\"uid\":";
    let mut found: Vec<(u64, u64)> = Vec::new();
    let mut rest = flight;
    while let Some(at) = rest.find(KEY) {
        let after = &rest[at + KEY.len()..];
        let uid: String = after.chars().take_while(char::is_ascii_digit).collect();
        let kind = after[uid.len()..]
            .strip_prefix(",\"type\":")
            .map(|k| {
                k.chars()
                    .take_while(char::is_ascii_digit)
                    .collect::<String>()
            })
            .and_then(|k| k.parse().ok());
        if let (Ok(uid), Some(kind)) = (uid.parse::<u64>(), kind)
            && !found.contains(&(uid, kind))
        {
            found.push((uid, kind));
        }
        rest = after;
    }
    found
}

/// A news issue's fragments as the page's payload lists them, the full issue first when
/// the issue's own material is present.
pub fn issue_entries(flight: &str, date: &str) -> Vec<PlaylistEntry> {
    let mut entries = Vec::new();
    if let Some(main) = json_after_main_id(flight)
        .filter(|_| flight.contains("\"mainNewsVideoMaterialPresent\":true"))
        .and_then(|id| Url::parse(&format!("{SITE}news/{date}/{id}")).ok())
    {
        entries.push(PlaylistEntry {
            url: main,
            title: None,
            duration: None,
        });
    }
    for fragment in json_after(flight, "\"fragments\":")
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
    {
        let Some(url) = fragment["link"]
            .as_str()
            .and_then(|l| Url::parse(l).ok())
            .or_else(|| {
                fragment["id"]
                    .as_u64()
                    .and_then(|id| Url::parse(&format!("{SITE}news/{date}/{id}")).ok())
            })
        else {
            continue;
        };
        entries.push(PlaylistEntry {
            url,
            title: fragment["title"].as_str().and_then(clean_title),
            duration: None,
        });
    }
    entries
}

/// The issue's own news id, a bare number after `"mainNewsId":` in the payload.
fn json_after_main_id(flight: &str) -> Option<u64> {
    let start = flight.find("\"mainNewsId\":")? + "\"mainNewsId\":".len();
    let digits: String = flight[start..]
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

/// The kbit/s a file name carries as `_3800.mp4`.
fn file_kbps(url: &Url) -> Option<u64> {
    let name = url.path_segments()?.next_back()?;
    let stem = name.strip_suffix(".mp4")?;
    let digits = stem.rsplit('_').next()?;
    (digits.len() >= 3).then(|| digits.parse().ok()).flatten()
}

fn absolute(src: &str) -> Option<Url> {
    let src = src.trim();
    if src.starts_with("//") {
        Url::parse(&format!("https:{src}")).ok()
    } else {
        Url::parse(src).ok()
    }
}

/// The MP4 files a material names by quality, `hd`, `sd` and `ld`, with the source
/// list's MP4 when it is not among them, sized from the HLS renditions of the same
/// bit rate; a file the HLS master does not serve at that rate is a redirect to a
/// lower one and is left out.
pub fn file_variants(item: &Value, hls: &[Variant]) -> Vec<Variant> {
    let mut files: Vec<(String, Url)> = Vec::new();
    for entry in item["mbr"].as_array().into_iter().flatten() {
        if let (Some(name), Some(url)) = (
            entry["name"].as_str(),
            entry["src"].as_str().and_then(absolute),
        ) && !files.iter().any(|(_, u)| *u == url)
        {
            files.push((name.to_string(), url));
        }
    }
    for source in item["sources"].as_array().into_iter().flatten() {
        if source["type"].as_str() == Some("video/mp4")
            && let Some(url) = source["src"].as_str().and_then(absolute)
            && !files.iter().any(|(_, u)| *u == url)
        {
            files.push(("mp4".to_string(), url));
        }
    }
    let duration = item["duration"]
        .as_f64()
        .filter(|d| *d > 0.0)
        .map(Duration::from_secs_f64);
    let mut variants = Vec::new();
    for (name, url) in files {
        let kbps = file_kbps(&url);
        let rendition = kbps.and_then(|k| {
            hls.iter().find(|v| {
                v.bitrate
                    .is_some_and(|b| b.abs_diff(k * 1000) <= k * 1000 / 5)
            })
        });
        if !hls.is_empty() && kbps.is_some() && rendition.is_none() {
            continue;
        }
        let mut v = Variant::new(url, VariantKind::File);
        v.container = Some(Container::Mp4);
        v.video = Some(VideoCodec::H264);
        v.audio = Some(AudioCodec::Aac);
        v.bitrate = kbps.map(|k| k * 1000);
        v.duration = duration;
        if let Some(r) = rendition {
            v.width = r.width;
            v.height = r.height;
            v.fps = r.fps;
        }
        v.format_id = Some(format!("http-{name}"));
        v.label = Some(name);
        variants.push(v);
    }
    variants
}

fn air_date(item: &Value) -> Option<Timestamp> {
    item["dvr_begin_at"]
        .as_i64()
        .filter(|t| *t > 0)
        .and_then(|t| Timestamp::from_second(t).ok())
        .or_else(|| {
            let text = item["date_air"].as_str()?;
            Date::strptime("%Y-%m-%d", text.trim())
                .ok()?
                .to_zoned(TimeZone::UTC)
                .ok()
                .map(|z| z.timestamp())
        })
}

fn entry_of(item: &Value) -> Option<PlaylistEntry> {
    let uid = item["uid"].as_u64()?;
    let kind = item["type"].as_u64().unwrap_or(12);
    let url = item["sharing"]["link"]
        .as_str()
        .and_then(|l| Url::parse(l).ok())
        .unwrap_or_else(|| embed_url(uid, kind));
    Some(PlaylistEntry {
        url,
        title: item["title"].as_str().and_then(clean_title),
        duration: item["duration"]
            .as_f64()
            .filter(|d| *d > 0.0)
            .map(Duration::from_secs_f64),
    })
}

/// What a page says about itself, read before its document is let go.
#[derive(Debug, Clone, Default)]
pub struct PageInfo {
    pub url: Option<Url>,
    pub title: Option<String>,
    pub description: Option<String>,
}

impl PageInfo {
    pub fn of(page: &Page) -> Self {
        Self {
            url: Some(page.url().clone()),
            title: page
                .meta("og:title")
                .or_else(|| page.title())
                .and_then(|t| clean_title(&t)),
            description: page.meta("og:description").and_then(|d| clean_title(&d)),
        }
    }
}

pub struct FirstTvResolver {
    http: Http,
}

impl FirstTvResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    async fn json(&self, url: &Url, link: &Url) -> Result<Value, ResolveError> {
        let fetched = fetch(&self.http, url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        match fetched.status.as_u16() {
            200..=299 => fetched.json(link),
            404 | 410 => Err(ResolveError::NotFound(link.clone())),
            429 => Err(ResolveError::RateLimited(link.clone())),
            status => Err(ResolveError::unavailable(
                link,
                format!("{} answered HTTP {status}", url.path()),
            )),
        }
    }

    /// One material as a resolved video: its HLS renditions and MP4 files.
    async fn material(
        &self,
        item: &Value,
        link: &Url,
        page: Option<&PageInfo>,
    ) -> Result<Resolution, ResolveError> {
        if let Some(expires) = item["date_expiration"]
            .as_str()
            .and_then(|t| t.parse::<Timestamp>().ok())
            && expires < Timestamp::now()
        {
            return Err(ResolveError::unavailable(
                link,
                format!("the video expired at {expires}"),
            ));
        }
        if item["available_to_subscribers"].as_bool() == Some(true) {
            return Err(ResolveError::unavailable(
                link,
                "the video is only available to the site's subscribers",
            ));
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = item["id"]
            .as_u64()
            .or_else(|| item["uid"].as_u64())
            .map(|id| id.to_string());
        resolved.title = item["title"].as_str().and_then(clean_title);
        resolved.description = page.and_then(|p| p.description.clone());
        resolved.uploader = item["project"].as_str().and_then(clean_title);
        resolved.uploaded_at = air_date(item);
        resolved.duration = item["duration"]
            .as_f64()
            .filter(|d| *d > 0.0)
            .map(Duration::from_secs_f64);
        resolved.thumbnail = item["poster"].as_str().and_then(|u| Url::parse(u).ok());
        resolved.webpage_url = item["sharing"]["link"]
            .as_str()
            .and_then(|l| Url::parse(l).ok())
            .or_else(|| page.and_then(|p| p.url.clone()))
            .or_else(|| Some(link.clone()));
        resolved.age_limit = item["age_restriction"]
            .as_u64()
            .filter(|a| *a > 0)
            .map(|a| a.min(21) as u8);
        let master = item["sources"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|s| {
                s["type"]
                    .as_str()
                    .is_some_and(|t| t.eq_ignore_ascii_case("application/x-mpegurl"))
            })
            .and_then(|s| s["src"].as_str().and_then(absolute));
        let mut hls_variants = Vec::new();
        if let Some(master) = master {
            match hls::expand(&self.http, &master, PLATFORM, BROWSER_UA, &[]).await {
                Ok(expanded) => {
                    resolved.subtitles = expanded.subtitles;
                    resolved.duration = resolved.duration.or(expanded.duration);
                    hls_variants = expanded.variants;
                }
                Err(ResolveError::Http(error)) => return Err(ResolveError::Http(error)),
                Err(error) => {
                    tracing::debug!(url = %master, %error, "the HLS master could not be read");
                }
            }
        }
        let files = file_variants(item, &hls_variants);
        resolved.variants = hls_variants;
        // A DASH manifest among the sources expands into its representations too.
        let mpd_sources: Vec<Url> = item["sources"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| {
                s["type"]
                    .as_str()
                    .is_some_and(|t| t.eq_ignore_ascii_case("application/dash+xml"))
                    || s["src"].as_str().is_some_and(|src| src.ends_with(".mpd"))
            })
            .filter_map(|s| s["src"].as_str().and_then(absolute))
            .collect();
        for manifest in mpd_sources {
            match super::dash::expand(&self.http, &manifest, PLATFORM, BROWSER_UA, &[]).await {
                Ok(expanded) => {
                    resolved.duration = resolved.duration.or(expanded.duration);
                    for mut representation in expanded.variants {
                        representation.format_id = Some(match &representation.label {
                            Some(label) => format!("dash-{label}"),
                            None => "dash".to_string(),
                        });
                        resolved.variants.push(representation);
                    }
                    resolved.subtitles.extend(expanded.subtitles);
                }
                Err(ResolveError::Http(error)) => return Err(ResolveError::Http(error)),
                Err(error) => {
                    tracing::debug!(url = %manifest, %error, "the DASH manifest could not be read");
                }
            }
        }
        resolved.variants.extend(files);
        if resolved.variants.is_empty() {
            return Err(ResolveError::NotFound(link.clone()));
        }
        Ok(Resolution::from(resolved))
    }

    async fn by_material(
        &self,
        uid: u64,
        kind: u64,
        link: &Url,
        page: Option<&PageInfo>,
    ) -> Result<Resolution, ResolveError> {
        let key = if kind == NEWS { "news_id" } else { "video_id" };
        let api = Url::parse(&format!("{MATERIALS}?{key}={uid}")).expect("valid");
        let list = self.json(&api, link).await?;
        let item = list
            .as_array()
            .into_iter()
            .flatten()
            .find(|item| item["uid"].as_u64() == Some(uid))
            .ok_or_else(|| ResolveError::NotFound(link.clone()))?;
        self.material(item, link, page).await
    }

    async fn by_list(
        &self,
        list_url: &Url,
        link: &Url,
        page: &PageInfo,
    ) -> Result<Resolution, ResolveError> {
        let wanted = listed_ids(list_url);
        let list = self.json(list_url, link).await?;
        let items: Vec<&Value> = list
            .as_array()
            .into_iter()
            .flatten()
            .filter(|item| {
                item["uid"]
                    .as_u64()
                    .is_some_and(|uid| wanted.is_empty() || wanted.contains(&uid))
            })
            .collect();
        match items.as_slice() {
            [] => Err(ResolveError::NotFound(link.clone())),
            [item] => self.material(item, link, Some(page)).await,
            many => Ok(Resolution::Playlist(Playlist {
                resolver: PLATFORM.to_string(),
                id: Some(link.path().trim_matches('/').to_string()),
                title: page.title.clone(),
                entries: many.iter().filter_map(|item| entry_of(item)).collect(),
                total: Some(many.len()),
            })),
        }
    }

    async fn page(&self, link: &Url) -> Result<Resolution, ResolveError> {
        let fetched = fetch_ok(
            &self.http,
            link,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        let html = fetched.text();
        let page = PageInfo::of(&Page::parse(&html, &fetched.url));
        if let Some(list_url) = super::page::between(&html, "data-playlist-url=\"", "\"")
            .map(|raw| raw.replace("&amp;", "&"))
            .and_then(|raw| fetched.url.join(&raw).ok())
        {
            return self.by_list(&list_url, link, &page).await;
        }
        let Some(flight) = next_flight_data(&html) else {
            return Err(ResolveError::NotFound(link.clone()));
        };
        let materials = inlined_materials(&flight);
        match materials.as_slice() {
            [(uid, kind)] => return self.by_material(*uid, *kind, link, Some(&page)).await,
            [_, _, ..] => {
                return Ok(Resolution::Playlist(Playlist {
                    resolver: PLATFORM.to_string(),
                    id: Some(link.path().trim_matches('/').to_string()),
                    title: page.title.clone(),
                    entries: materials
                        .iter()
                        .map(|(uid, kind)| PlaylistEntry {
                            url: embed_url(*uid, *kind),
                            title: None,
                            duration: None,
                        })
                        .collect(),
                    total: Some(materials.len()),
                }));
            }
            [] => {}
        }
        let date = fetched
            .url
            .path_segments()
            .into_iter()
            .flatten()
            .filter(|s| !s.is_empty())
            .nth(2)
            .unwrap_or_default()
            .to_string();
        let entries = issue_entries(&flight, &date);
        if entries.is_empty() {
            return Err(ResolveError::NotFound(link.clone()));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: json_after_main_id(&flight)
                .map(|id| id.to_string())
                .or_else(|| Some(link.path().trim_matches('/').to_string())),
            title: page.title.clone(),
            total: Some(entries.len()),
            entries,
        }))
    }

    async fn live(&self, link: &Url) -> Result<Resolution, ResolveError> {
        let page_url = Url::parse(&format!("{SITE}live")).expect("valid");
        let title = match fetch(
            &self.http,
            &page_url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await
        {
            Ok(fetched) if fetched.status.is_success() => {
                Page::parse(&fetched.text(), &fetched.url)
                    .title()
                    .and_then(|t| clean_title(&t))
            }
            _ => None,
        };
        let api = Url::parse(LIVE_STREAMS).expect("valid");
        let streams = self.json(&api, link).await?;
        let mut variants = Vec::new();
        for (index, manifest) in streams["mpd"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|m| m.as_str().and_then(|m| Url::parse(m).ok()))
            .enumerate()
        {
            if variants.iter().any(|v: &Variant| v.url == manifest) {
                continue;
            }
            let mut v = Variant::new(manifest, VariantKind::Dash);
            v.live = true;
            v.format_id = Some(format!("dash-{}", index + 1));
            v.label = Some(format!("DASH, CDN {}", index + 1));
            variants.push(v);
        }
        // Each CDN's manifest lists the same representations; the first that answers
        // names their sizes, and the rest stay as whole manifests to fall back to.
        if let Some(first) = variants.first().cloned() {
            let mut subtitles = Vec::new();
            let expanded = super::manifests::expand_all(
                &self.http,
                PLATFORM,
                vec![first],
                &mut subtitles,
                None,
            )
            .await;
            if expanded.len() > 1 || expanded.first().is_some_and(|v| v.height.is_some()) {
                let mut sized: Vec<Variant> = expanded
                    .into_iter()
                    .map(|mut v| {
                        v.live = true;
                        v
                    })
                    .collect();
                sized.extend(variants.into_iter().skip(1));
                variants = sized;
            }
        }
        if variants.is_empty() {
            return Err(ResolveError::unavailable(
                link,
                "the stream API lists no DASH manifest",
            ));
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some("live".to_string());
        resolved.title = title.or_else(|| Some("Первый канал, прямой эфир".to_string()));
        resolved.uploader = Some("Первый канал".to_string());
        resolved.webpage_url = Some(page_url);
        resolved.live = true;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }
}

#[async_trait]
impl Resolver for FirstTvResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Первый канал",
            hosts: &["1tv.ru", "sport1tv.ru"],
            features: &["shows", "sport", "news", "issues", "embeds", "live"],
            formats: &["hls", "mp4", "dash"],
            media: &[MediaKind::Video],
            tags: &[Tag::News, Tag::Video, Tag::Live],
            session: SessionSupport::None,
            examples: &[
                "https://www.1tv.ru/shows/dobroe-utro/pro-zdorove/vesennyaya-allergiya-dobroe-utro-fragment-vypuska-ot-07042016",
                "https://www.1tv.ru/shows/naedine-so-vsemi/vypuski/gost-lyudmila-senchina-naedine-so-vsemi-vypusk-ot-12-02-2015",
                "https://www.1tv.ru/live",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Live => self.live(url).await,
            Link::Embed { uid, kind } => self.by_material(uid, kind, url, None).await,
            Link::Page(_) => self.page(url).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Fixture;

    const FRAGMENT: &str = "https://www.1tv.ru/shows/dobroe-utro/pro-zdorove/vesennyaya-allergiya-dobroe-utro-fragment-vypuska-ot-07042016";
    const SPORT: &str =
        "https://www.sport1tv.ru/sport/chempionat-rossii-po-figurnomu-kataniyu-2025";
    const NEWS_STORY: &str = "https://www.1tv.ru/news/2026-09-14/553139";
    const NEWS_ISSUE: &str = "https://www.1tv.ru/news/issue/2026-09-13/21:00";

    fn resolver() -> FirstTvResolver {
        let fixture = Fixture::parse(include_str!("firsttv_fixture.json")).unwrap();
        FirstTvResolver::new(Http::replay(fixture))
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&url(s));
        assert_eq!(link("https://www.1tv.ru/live"), Some(Link::Live));
        assert_eq!(link("http://www.1tv.ru/live?x=1"), Some(Link::Live));
        assert_eq!(
            link("https://www.1tv.ru/embed/553139:11"),
            Some(Link::Embed {
                uid: 553139,
                kind: 11
            })
        );
        assert_eq!(
            link("https://static.1tv.ru/eump/embeds/public_vod.html?v=553139:11&start=none"),
            Some(Link::Embed {
                uid: 553139,
                kind: 11
            })
        );
        assert_eq!(
            link(FRAGMENT),
            Some(Link::Page(
                "/shows/dobroe-utro/pro-zdorove/vesennyaya-allergiya-dobroe-utro-fragment-vypuska-ot-07042016".into()
            ))
        );
        assert_eq!(
            link(SPORT),
            Some(Link::Page(
                "/sport/chempionat-rossii-po-figurnomu-kataniyu-2025".into()
            ))
        );
        assert_eq!(
            link("https://www.1tv.ru/-/oomxxj"),
            Some(Link::Page("/-/oomxxj".into()))
        );
        assert_eq!(link("https://www.1tv.ru/"), None);
        assert_eq!(link("https://www.1tv.ru/shows"), None);
        assert_eq!(link("https://www.1tv.ru/embed/abc"), None);
        assert_eq!(
            link("https://static.1tv.ru/eump/embeds/public_vod.html"),
            None
        );
        assert_eq!(
            link("https://1tv.ru.evil.test/news/2026-09-14/553139"),
            None
        );
        assert_eq!(embed_url(1, 12).as_str(), "https://www.1tv.ru/embed/1:12");
    }

    #[test]
    fn list_links_name_their_materials() {
        let ids = |s: &str| listed_ids(&url(s));
        assert_eq!(
            ids("https://www.1tv.ru/playlist?admin=false&single=false&sort=none&video_id=65827"),
            vec![65827]
        );
        assert_eq!(
            ids("https://www.1tv.ru/playlist?videos_ids[]=1&videos_ids[]=2&news_ids[]=3"),
            vec![1, 2, 3]
        );
        assert_eq!(
            ids("https://www.1tv.ru/playlist?collection_id=6497"),
            Vec::<u64>::new()
        );
    }

    #[test]
    fn inlined_materials_and_issue_fragments_are_read() {
        let flight = r#"2f:[["$","$L32",null,{"options":{"video":{"uid":553139,"type":11},"options":{"title":false}}}]]
30:{"video":{"uid":553139,"type":11}} 31:{"video":{"uid":7,"type":12}}"#;
        assert_eq!(inlined_materials(flight), vec![(553139, 11), (7, 12)]);
        assert_eq!(
            inlined_materials(r#"{"video":{"uid":"x","type":11}}"#),
            vec![]
        );

        let issue = r#"{"mainNewsId":553085,"mainNewsVideoMaterialPresent":true,"releaseNewsSize":19,"fragments":[{"id":553095,"link":"https://www.1tv.ru/news/2026-09-13/553095","title":"Главное событие","time":"21:01"},{"id":553096,"title":"Без ссылки"}]}"#;
        let entries = issue_entries(issue, "2026-09-13");
        assert_eq!(
            entries.iter().map(|e| e.url.as_str()).collect::<Vec<_>>(),
            vec![
                "https://www.1tv.ru/news/2026-09-13/553085",
                "https://www.1tv.ru/news/2026-09-13/553095",
                "https://www.1tv.ru/news/2026-09-13/553096",
            ]
        );
        assert_eq!(entries[1].title.as_deref(), Some("Главное событие"));
        let without_main = issue.replace(
            "\"mainNewsVideoMaterialPresent\":true",
            "\"mainNewsVideoMaterialPresent\":false",
        );
        assert_eq!(issue_entries(&without_main, "2026-09-13").len(), 2);
    }

    #[test]
    fn files_are_sized_from_the_hls_renditions_and_kept_without_one() {
        let item = serde_json::json!({
            "duration": 179,
            "mbr": [
                {"name": "hd", "src": "//balancer-vod.1tv.ru/video/x_3800.mp4"},
                {"name": "sd", "src": "//balancer-vod.1tv.ru/video/x_950.mp4"},
                {"name": "ld", "src": "//balancer-vod.1tv.ru/video/x_350.mp4"}
            ],
            "sources": [
                {"src": "https://balancer-vod.1tv.ru/video/x_,350,950,3800,.mp4.urlset/master.m3u8", "type": "application/x-mpegURL"},
                {"src": "https://balancer-vod.1tv.ru/video/x_950.mp4", "type": "video/mp4"}
            ]
        });
        let mut sd = Variant::hls(url("https://balancer-vod.1tv.ru/video/x_950/index.m3u8"));
        sd.bitrate = Some(1_000_000);
        sd.width = Some(640);
        sd.height = Some(360);
        let mut ld = Variant::hls(url("https://balancer-vod.1tv.ru/video/x_350/index.m3u8"));
        ld.bitrate = Some(380_000);
        ld.width = Some(480);
        ld.height = Some(270);
        let files = file_variants(&item, &[sd, ld]);
        assert_eq!(
            files
                .iter()
                .map(|v| (v.label.as_deref().unwrap(), v.url.as_str(), v.height))
                .collect::<Vec<_>>(),
            vec![
                (
                    "sd",
                    "https://balancer-vod.1tv.ru/video/x_950.mp4",
                    Some(360)
                ),
                (
                    "ld",
                    "https://balancer-vod.1tv.ru/video/x_350.mp4",
                    Some(270)
                ),
            ]
        );
        assert_eq!(files[0].bitrate, Some(950_000));
        assert_eq!(files[0].duration, Some(Duration::from_secs(179)));

        let all = file_variants(&item, &[]);
        assert_eq!(all.len(), 3, "without a master every file is listed");
        assert_eq!(all[0].label.as_deref(), Some("hd"));
    }

    #[tokio::test]
    async fn show_fragments_resolve_through_the_player_list() {
        let resolver = resolver();
        assert!(resolver.matches(&url(FRAGMENT)));
        let resolved = resolver
            .resolve(&url(FRAGMENT))
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("364746"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Весенняя аллергия. Доброе утро. Фрагмент выпуска от 07.04.2016")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("Доброе утро"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(179)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.to_string()),
            Some("2016-04-07T00:00:00Z".to_string())
        );
        assert_eq!(
            resolved.thumbnail.as_ref().map(Url::as_str),
            Some("https://static.1tv.ru/uploads/photo/image/6/big/198806_big_ba3653ef60.jpg")
        );
        assert_eq!(
            resolved.webpage_url.as_ref().map(Url::as_str),
            Some(FRAGMENT)
        );
        let hls: Vec<&Variant> = resolved
            .variants
            .iter()
            .filter(|v| v.kind == VariantKind::Hls)
            .collect();
        assert!(!hls.is_empty(), "{:?}", resolved.variants);
        assert!(
            hls.iter()
                .all(|v| v.height.is_some() && v.bitrate.is_some())
        );
        let files: Vec<&Variant> = resolved
            .variants
            .iter()
            .filter(|v| v.kind == VariantKind::File)
            .collect();
        assert!(!files.is_empty());
        assert!(files.iter().all(|v| v.container == Some(Container::Mp4)));
        assert!(files.iter().any(|v| v.height.is_some()));
        assert!(
            files
                .iter()
                .all(|v| v.url.as_str().starts_with("https://balancer-vod.1tv.ru/"))
        );
    }

    #[tokio::test]
    async fn sport_links_redirect_to_the_site_and_resolve_by_collection() {
        let resolver = resolver();
        let resolved = resolver
            .resolve(&url(SPORT))
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("791002"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Танцы на льду. Лучшее. Чемпионат России по фигурному катанию 2025")
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(1097)));
        assert!(resolved.variants.iter().any(|v| v.kind == VariantKind::Hls));
    }

    #[tokio::test]
    async fn news_stories_resolve_by_their_inlined_material() {
        let resolver = resolver();
        let resolved = resolver
            .resolve(&url(NEWS_STORY))
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("883177"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Выпуск новостей в 09:00 от 14.09.2026")
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(1314)));
        assert!(resolved.description.is_some());
        assert!(resolved.variants.iter().any(|v| v.kind == VariantKind::Hls));
        assert!(
            resolved
                .variants
                .iter()
                .any(|v| v.kind == VariantKind::File)
        );

        let embed = resolver
            .resolve(&url("https://www.1tv.ru/embed/553139:11"))
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(embed.id.as_deref(), Some("883177"));
    }

    #[tokio::test]
    async fn news_issues_list_the_issue_and_its_fragments() {
        let resolver = resolver();
        let Resolution::Playlist(playlist) = resolver.resolve(&url(NEWS_ISSUE)).await.unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(playlist.id.as_deref(), Some("553085"));
        assert_eq!(
            playlist.title.as_deref(),
            Some(
                "Выпуск программы «Воскресное время» в 21:00 от 13.09.2026. Новости. Первый канал"
            )
        );
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://www.1tv.ru/news/2026-09-13/553085"
        );
        assert!(playlist.entries.len() > 2);
        assert!(playlist.entries[1..].iter().all(|e| {
            e.title.is_some()
                && e.url
                    .as_str()
                    .starts_with("https://www.1tv.ru/news/2026-09-13/")
        }));
        assert_eq!(playlist.total, Some(playlist.entries.len()));
    }

    #[tokio::test]
    async fn the_live_channel_lists_its_dash_manifests() {
        let resolver = resolver();
        let resolved = resolver
            .resolve(&url("https://www.1tv.ru/live"))
            .await
            .unwrap()
            .media()
            .unwrap();
        assert!(resolved.live);
        assert_eq!(resolved.id.as_deref(), Some("live"));
        assert_eq!(resolved.title.as_deref(), Some("Первый канал онлайн"));
        assert!(!resolved.variants.is_empty());
        assert!(
            resolved
                .variants
                .iter()
                .all(|v| v.kind == VariantKind::Dash && v.live && v.url.path().ends_with(".mpd"))
        );
    }
}
