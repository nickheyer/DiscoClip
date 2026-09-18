//! Vidyard players, through the player API the embed reads: every MP4 rendition by its
//! profile, the HLS master expanded to its renditions, the captions, and the video's
//! name, description, length and thumbnail, all behind the player referer the CDN
//! wants. Watch pages, share links, player links, embed scripts and inline embeds name
//! the player; a player of several chapters becomes a playlist, one chapter per link.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use scraper::Selector;
use serde_json::Value;
use url::Url;

use super::page::Page;
use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, SubtitleFormat, SubtitleTrack, Tag, Variant, VariantKind, clean_title, essence,
    fetch, hls, path_extension,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "vidyard";
const PLAYER: &str = "https://play.vidyard.com/";
const PLAYER_API: &str = "https://play.vidyard.com/player/";
/// The referer the CDN wants on every file and playlist.
const REFERER: &str = "https://play.vidyard.com/";

static RE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]{16,}$").unwrap());
static RE_SCRIPT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"play\.vidyard\.com/([A-Za-z0-9_-]{16,})\.js").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub id: String,
    /// One chapter of a player of several, counted from one.
    pub chapter: Option<usize>,
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !(host == "vidyard.com" || host.ends_with(".vidyard.com")) {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let id_of = |name: &str| {
        let id = name.split('.').next().unwrap_or(name);
        RE_ID.is_match(id).then(|| id.to_string())
    };
    let id = if host == "play.vidyard.com" {
        match segments.as_slice() {
            ["player", name] => id_of(name),
            ["embed", ..] | ["player", ..] => None,
            [name, ..] => id_of(name),
            [] => None,
        }
    } else {
        match segments.as_slice() {
            ["watch", name, ..] | ["share", name, ..] => id_of(name),
            _ => None,
        }
    }?;
    let chapter = url
        .query_pairs()
        .find(|(k, _)| k == "chapter")
        .and_then(|(_, v)| v.parse::<usize>().ok())
        .filter(|c| *c >= 1);
    Some(Link { id, chapter })
}

/// The player link for an id.
pub fn player_url(id: &str) -> Url {
    Url::parse(&format!("{PLAYER}{id}")).expect("valid")
}

fn selector(text: &str) -> Selector {
    Selector::parse(text).expect("selectors in this module are valid")
}

/// Players a page embeds inline or as a lightbox (`vidyard-player-embed` elements) or
/// through the player script, as links this resolver takes.
pub fn embeds_in(page: &Page) -> Vec<Url> {
    let mut found: Vec<Url> = Vec::new();
    let mut push = |id: &str| {
        let url = player_url(id);
        if !found.contains(&url) {
            found.push(url);
        }
    };
    for element in page
        .document()
        .select(&selector(".vidyard-player-embed[data-uuid]"))
    {
        if let Some(id) = element
            .value()
            .attr("data-uuid")
            .filter(|id| RE_ID.is_match(id))
        {
            push(id);
        }
    }
    for element in page.document().select(&selector("script[src]")) {
        if let Some(captures) = element
            .value()
            .attr("src")
            .and_then(|src| RE_SCRIPT.captures(src))
        {
            push(&captures[1]);
        }
    }
    found
}

fn duration_of(chapter: &Value) -> Option<Duration> {
    chapter["milliseconds"]
        .as_u64()
        .filter(|ms| *ms > 0)
        .map(Duration::from_millis)
        .or_else(|| {
            chapter["seconds"]
                .as_f64()
                .filter(|s| *s > 0.0)
                .map(Duration::from_secs_f64)
        })
}

/// The height a profile such as `720p` names.
fn profile_height(profile: &str) -> Option<u32> {
    profile
        .strip_suffix('p')
        .and_then(|h| h.parse().ok())
        .filter(|h| *h > 0)
}

/// The chapter's captions.
pub fn subtitles_of(chapter: &Value, headers: &[(String, String)]) -> Vec<SubtitleTrack> {
    chapter["captions"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|caption| {
            let url = caption["vttUrl"]
                .as_str()
                .and_then(|u| Url::parse(u).ok())?;
            Some(SubtitleTrack {
                url,
                language: caption["language"]
                    .as_str()
                    .filter(|l| !l.is_empty())
                    .unwrap_or("und")
                    .to_string(),
                name: caption["name"].as_str().and_then(clean_title),
                format: SubtitleFormat::Vtt,
                auto: false,
                headers: headers.to_vec(),
            })
        })
        .collect()
}

pub struct VidyardResolver {
    http: Http,
}

impl VidyardResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// The player's payload: its chapters and their sources.
    async fn player(&self, id: &str, origin: &Url) -> Result<Value, ResolveError> {
        let api = Url::parse(&format!("{PLAYER_API}{id}.json")).expect("valid");
        let fetched = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        let body = fetched.json(origin).unwrap_or(Value::Null);
        match fetched.status.as_u16() {
            200..=299 => {}
            404 | 410 => return Err(ResolveError::NotFound(origin.clone())),
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the player API answered HTTP {status}"),
                ));
            }
        }
        if body["type"].as_str() == Some("error") {
            return Err(match body["payload"]["code"].as_u64() {
                Some(404) => ResolveError::NotFound(origin.clone()),
                _ => ResolveError::unavailable(
                    origin,
                    body["payload"]["message"]
                        .as_str()
                        .unwrap_or("the player API answered an error")
                        .to_string(),
                ),
            });
        }
        let payload = body["payload"].clone();
        if !payload.is_object() {
            return Err(ResolveError::malformed(
                origin,
                "the player API answered no payload",
            ));
        }
        Ok(payload)
    }

    /// One chapter as media: its HLS renditions, MP4 files and captions.
    async fn chapter_media(&self, chapter: &Value, origin: &Url) -> Result<Resolved, ResolveError> {
        let headers = vec![("referer".to_string(), REFERER.to_string())];
        let mut duration = duration_of(chapter);
        let mut variants = Vec::new();
        let mut subtitles = Vec::new();
        let mut live = false;
        let sources = chapter["sources"].as_object();
        let hls_list: Vec<&Value> = sources
            .and_then(|s| s.get("hls"))
            .and_then(|list| list.as_array())
            .map(|list| list.iter().collect())
            .unwrap_or_default();
        let master = hls_list
            .iter()
            .find(|s| s["profile"].as_str() == Some("auto"))
            .and_then(|s| s["url"].as_str())
            .and_then(|u| Url::parse(u).ok());
        match master {
            Some(master) => {
                let expanded = hls::expand(&self.http, &master, PLATFORM, BROWSER_UA, &headers)
                    .await
                    .map_err(|e| e.at(origin))?;
                if duration.is_none() {
                    duration = expanded.duration;
                }
                live = expanded.live;
                subtitles.extend(expanded.subtitles);
                variants.extend(expanded.variants);
            }
            None => {
                // Without a master, each rendition's own playlist plays.
                for source in &hls_list {
                    let Some(url) = source["url"].as_str().and_then(|u| Url::parse(u).ok()) else {
                        continue;
                    };
                    let profile = source["profile"].as_str().unwrap_or_default();
                    let mut v = Variant::new(url, VariantKind::Hls);
                    v.height = profile_height(profile);
                    v.label = Some(profile.to_string());
                    v.format_id = Some(format!("hls-{profile}"));
                    v.headers = headers.clone();
                    variants.push(v);
                }
            }
        }
        for (kind, list) in sources.into_iter().flatten() {
            if kind == "hls" {
                continue;
            }
            for source in list.as_array().into_iter().flatten() {
                let Some(url) = source["url"].as_str().and_then(|u| Url::parse(u).ok()) else {
                    continue;
                };
                let profile = source["profile"].as_str().unwrap_or_default();
                let mime = essence(source["mimeType"].as_str());
                let mut v = Variant::new(url, VariantKind::File);
                v.container = Container::from_mime(&mime).or_else(|| {
                    path_extension(&v.url)
                        .as_deref()
                        .and_then(Container::from_extension)
                });
                if v.container == Some(Container::Mp4) {
                    v.video = Some(VideoCodec::H264);
                    v.audio = Some(AudioCodec::Aac);
                }
                v.height = profile_height(profile);
                v.label = Some(profile.to_string());
                v.format_id = Some(format!("{kind}-{profile}"));
                v.headers = headers.clone();
                variants.push(v);
            }
        }
        if variants.is_empty() {
            return Err(ResolveError::unavailable(
                origin,
                "the video has no playable sources",
            ));
        }
        for v in &mut variants {
            if v.duration.is_none() {
                v.duration = duration;
            }
        }
        subtitles.extend(subtitles_of(chapter, &headers));
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = chapter["facadeUuid"]
            .as_str()
            .or(chapter["videoUuid"].as_str())
            .map(String::from);
        resolved.title = chapter["name"].as_str().and_then(clean_title);
        resolved.description = chapter["description"].as_str().and_then(clean_title);
        resolved.duration = duration;
        resolved.live = live;
        resolved.thumbnail = chapter["thumbnailUrls"]["normal"]
            .as_str()
            .or(chapter["thumbnailUrls"]["small"].as_str())
            .and_then(|u| Url::parse(u).ok());
        resolved.webpage_url = Some(origin.clone());
        resolved.subtitles = subtitles;
        resolved.variants = variants;
        Ok(resolved)
    }
}

#[async_trait]
impl Resolver for VidyardResolver {
    fn embeds_in(&self, page: &Page) -> Vec<Url> {
        embeds_in(page)
    }

    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Vidyard",
            hosts: &["vidyard.com"],
            features: &[
                "players",
                "watch pages",
                "share links",
                "player embeds",
                "chapters as playlists",
                "captions",
            ],
            formats: &["mp4", "hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::Players],
            session: SessionSupport::None,
            examples: &[
                "https://play.vidyard.com/oTDMPlUv--51Th455G5u7Q",
                "https://share.vidyard.com/watch/PaQzDAT1h8JqB8ivEu2j6Y",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let payload = self.player(&link.id, url).await?;
        let chapters = payload["chapters"]
            .as_array()
            .filter(|chapters| !chapters.is_empty())
            .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let index = match (link.chapter, chapters.len()) {
            (Some(number), count) => {
                if number > count {
                    return Err(ResolveError::NotFound(url.clone()));
                }
                number - 1
            }
            (None, 1) => 0,
            (None, _) => {
                let player_id = payload["playerUuid"]
                    .as_str()
                    .unwrap_or(&link.id)
                    .to_string();
                let entries = chapters
                    .iter()
                    .enumerate()
                    .map(|(index, chapter)| {
                        let mut entry_url = player_url(&player_id);
                        entry_url
                            .query_pairs_mut()
                            .append_pair("chapter", &(index + 1).to_string());
                        PlaylistEntry {
                            url: entry_url,
                            title: chapter["name"].as_str().and_then(clean_title),
                            duration: duration_of(chapter),
                        }
                    })
                    .collect::<Vec<_>>();
                return Ok(Resolution::Playlist(Playlist {
                    resolver: PLATFORM.into(),
                    id: Some(player_id),
                    title: payload["name"].as_str().and_then(clean_title),
                    total: Some(entries.len()),
                    entries,
                }));
            }
        };
        Ok(Resolution::from(
            self.chapter_media(&chapters[index], url).await?,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::Fixture;

    fn fixture() -> Fixture {
        Fixture::parse(include_str!("vidyard_fixture.json")).unwrap()
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let id = |id: &str| {
            Some(Link {
                id: id.into(),
                chapter: None,
            })
        };
        assert_eq!(
            link("https://play.vidyard.com/oTDMPlUv--51Th455G5u7Q"),
            id("oTDMPlUv--51Th455G5u7Q")
        );
        assert_eq!(
            link("https://play.vidyard.com/oTDMPlUv--51Th455G5u7Q.html?autoplay=1"),
            id("oTDMPlUv--51Th455G5u7Q")
        );
        assert_eq!(
            link("https://play.vidyard.com/player/oTDMPlUv--51Th455G5u7Q.json"),
            id("oTDMPlUv--51Th455G5u7Q")
        );
        assert_eq!(
            link("https://play.vidyard.com/d61w8EQoZv1LDuPxDkQP2Q/type/standalone"),
            id("d61w8EQoZv1LDuPxDkQP2Q")
        );
        assert_eq!(
            link("https://vyexample03.hubs.vidyard.com/watch/oTDMPlUv--51Th455G5u7Q"),
            id("oTDMPlUv--51Th455G5u7Q")
        );
        assert_eq!(
            link("https://share.vidyard.com/watch/PaQzDAT1h8JqB8ivEu2j6Y?"),
            id("PaQzDAT1h8JqB8ivEu2j6Y")
        );
        assert_eq!(
            link("https://embed.vidyard.com/share/oTDMPlUv--51Th455G5u7Q"),
            id("oTDMPlUv--51Th455G5u7Q")
        );
        assert_eq!(
            link("https://play.vidyard.com/TwoChapterPlayer1234ab?chapter=2"),
            Some(Link {
                id: "TwoChapterPlayer1234ab".into(),
                chapter: Some(2)
            })
        );
        assert_eq!(link("https://play.vidyard.com/embed/v4.js"), None);
        assert_eq!(link("https://www.vidyard.com/pricing/"), None);
    }

    #[test]
    fn embedded_players_are_found_in_pages() {
        let html = r#"<html><body>
            <script type="text/javascript" async src="https://play.vidyard.com/embed/v4.js"></script>
            <img class="vidyard-player-embed" src="https://play.vidyard.com/oTDMPlUv--51Th455G5u7Q.jpg" data-uuid="oTDMPlUv--51Th455G5u7Q" data-v="4" data-type="inline" />
            <script src="//play.vidyard.com/PaQzDAT1h8JqB8ivEu2j6Y.js?v=3.1.1"></script>
            </body></html>"#;
        let page = Page::parse(html, &Url::parse("https://site.test/page").unwrap());
        let found: Vec<String> = embeds_in(&page).iter().map(|u| u.to_string()).collect();
        assert_eq!(
            found,
            vec![
                "https://play.vidyard.com/oTDMPlUv--51Th455G5u7Q",
                "https://play.vidyard.com/PaQzDAT1h8JqB8ivEu2j6Y",
            ]
        );
    }

    #[tokio::test]
    async fn players_resolve_with_their_renditions_behind_the_referer() {
        let resolver = VidyardResolver::new(Http::replay(fixture()));
        let resolved = resolver
            .resolve(&Url::parse("https://play.vidyard.com/oTDMPlUv--51Th455G5u7Q").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("oTDMPlUv--51Th455G5u7Q"));
        assert_eq!(resolved.title.as_deref(), Some("Homepage Video"));
        assert_eq!(
            resolved.description.as_deref(),
            Some("Look I changed the description.")
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(99)));
        assert!(
            resolved
                .thumbnail
                .as_ref()
                .unwrap()
                .as_str()
                .contains("/thumbnails/50347/")
        );
        assert_eq!(
            resolved.variants.len(),
            6,
            "{:?}",
            resolved
                .variants
                .iter()
                .map(|v| v.url.as_str())
                .collect::<Vec<_>>()
        );
        assert!(resolved.variants.iter().all(|v| {
            v.headers
                .contains(&("referer".to_string(), REFERER.to_string()))
        }));
        let hls: Vec<&Variant> = resolved
            .variants
            .iter()
            .filter(|v| v.kind == VariantKind::Hls)
            .collect();
        assert_eq!(hls.len(), 3);
        assert_eq!(hls[0].height, Some(720));
        assert_eq!(hls[0].video, Some(VideoCodec::H264));
        let mp4: Vec<&Variant> = resolved
            .variants
            .iter()
            .filter(|v| v.kind == VariantKind::File)
            .collect();
        assert_eq!(
            mp4.iter().map(|v| v.height).collect::<Vec<_>>(),
            vec![Some(720), Some(480), Some(360)]
        );
        assert_eq!(mp4[0].container, Some(Container::Mp4));
        assert_eq!(mp4[0].format_id.as_deref(), Some("mp4-720p"));
        let error = resolver
            .resolve(&Url::parse("https://play.vidyard.com/zzzzzzzzzzzzzzzzzzzzzz").unwrap())
            .await
            .unwrap_err();
        assert!(matches!(error, ResolveError::NotFound(_)), "{error}");
    }

    #[tokio::test]
    async fn players_of_several_chapters_are_playlists() {
        let resolver = VidyardResolver::new(Http::replay(fixture()));
        let playlist = match resolver
            .resolve(&Url::parse("https://play.vidyard.com/TwoChapterPlayer1234ab").unwrap())
            .await
            .unwrap()
        {
            Resolution::Playlist(p) => p,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(playlist.title.as_deref(), Some("Two chapters"));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[1].url.as_str(),
            "https://play.vidyard.com/TwoChapterPlayer1234ab?chapter=2"
        );
        assert_eq!(playlist.entries[1].title.as_deref(), Some("Inline Embed"));
        assert_eq!(
            playlist.entries[1].duration,
            Some(Duration::from_millis(41186))
        );
        let first = resolver
            .resolve(&playlist.entries[0].url)
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(first.title.as_deref(), Some("Homepage Video"));
        let error = resolver
            .resolve(
                &Url::parse("https://play.vidyard.com/TwoChapterPlayer1234ab?chapter=3").unwrap(),
            )
            .await
            .unwrap_err();
        assert!(matches!(error, ResolveError::NotFound(_)), "{error}");
    }
}
