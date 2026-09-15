//! Bunny Stream embeds: the player's HLS renditions, available MP4 fallbacks and
//! original upload, with the player referer on media and captions.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use scraper::Selector;
use url::Url;

use super::page::Page;
use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport,
    SubtitleFormat, SubtitleTrack, Variant, clean_title, essence, fetch, hls, is_hls_type,
    probe_file, status_error,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "bunny";
const PLAYER: &str = "https://iframe.mediadelivery.net/embed/";

static RE_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$").unwrap()
});
static RE_ORIGINAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\boriginalUrl\s*=\s*["']([^"']+)["']"#).unwrap());
static RE_PLAYLIST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\burlPlaylistUrl\s*=\s*["']([^"']+)["']"#).unwrap());
static RE_DRM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\bisEntDrm\s*=\s*true\b").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub library: String,
    pub id: String,
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(
            url.host_str()?,
            "iframe.mediadelivery.net" | "player.mediadelivery.net" | "video.bunnycdn.com"
        )
    {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        ["embed" | "play", library, id]
            if !library.is_empty()
                && library.bytes().all(|c| c.is_ascii_digit())
                && RE_ID.is_match(id) =>
        {
            Some(Link {
                library: library.to_string(),
                id: id.to_string(),
            })
        }
        _ => None,
    }
}

impl Link {
    fn player(&self, origin: &Url) -> Url {
        let mut url = Url::parse(&format!("{PLAYER}{}/{}", self.library, self.id)).expect("valid");
        // Signed embed links carry their authorization in the query.
        url.set_query(origin.query());
        url
    }
}

struct Player {
    title: Option<String>,
    description: Option<String>,
    duration: Option<Duration>,
    thumbnail: Option<Url>,
    master: Url,
    original: Option<Url>,
    subtitles: Vec<SubtitleTrack>,
}

fn player_of(html: &str, base: &Url, origin: &Url) -> Result<Player, ResolveError> {
    let page = Page::parse(html, base);
    if RE_DRM.is_match(html) {
        return Err(ResolveError::drm(origin, "Bunny MediaCage"));
    }
    let master = page
        .video_sources()
        .into_iter()
        .find(|u| u.path().ends_with(".m3u8"))
        .or_else(|| {
            RE_PLAYLIST
                .captures(html)
                .and_then(|c| base.join(&c[1]).ok())
        })
        .filter(|u| matches!(u.scheme(), "http" | "https"))
        .ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
    let original = RE_ORIGINAL
        .captures(html)
        .and_then(|c| base.join(&c[1]).ok())
        .filter(|u| matches!(u.scheme(), "http" | "https"));
    let mut subtitles = Vec::new();
    for element in page
        .document()
        .select(&Selector::parse("track[src]").expect("valid"))
    {
        let attrs = element.value();
        if !matches!(attrs.attr("kind"), Some("captions" | "subtitles")) {
            continue;
        }
        let Some(url) = attrs
            .attr("src")
            .and_then(|s| base.join(s).ok())
            .filter(|u| matches!(u.scheme(), "http" | "https"))
        else {
            continue;
        };
        let language = attrs.attr("srclang").unwrap_or("und");
        subtitles.push(SubtitleTrack {
            url,
            language: language.trim_end_matches("-auto").to_string(),
            name: attrs.attr("label").and_then(clean_title),
            format: SubtitleFormat::Vtt,
            auto: language.ends_with("-auto"),
            headers: vec![("referer".into(), base.to_string())],
        });
    }
    Ok(Player {
        title: page.title(),
        description: page.meta("og:description").and_then(|s| clean_title(&s)),
        duration: page
            .meta("video:duration")
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|s| s.is_finite() && *s > 0.0)
            .map(Duration::from_secs_f64),
        thumbnail: page.poster(),
        master,
        original,
        subtitles,
    })
}

pub struct BunnyResolver {
    http: Http,
}

impl BunnyResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for BunnyResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Bunny Stream",
            hosts: &[
                "iframe.mediadelivery.net",
                "player.mediadelivery.net",
                "video.bunnycdn.com",
            ],
            features: &[
                "player embeds",
                "signed links",
                "original uploads",
                "captions",
                "drm reported",
            ],
            formats: &["hls", "mp4"],
            session: SessionSupport::None,
            examples: &[
                "https://iframe.mediadelivery.net/embed/136145/32e34c4b-0d72-437c-9abb-05e67657da34",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let player_url = link.player(url);
        let fetched = fetch(&self.http, &player_url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        let player = player_of(&fetched.text(), &fetched.url, url)?;
        let headers = vec![("referer".to_string(), fetched.url.to_string())];
        let expanded = hls::expand(&self.http, &player.master, PLATFORM, BROWSER_UA, &headers)
            .await
            .map_err(|e| e.at(url))?;
        let duration = expanded.duration.or(player.duration);
        let mut variants = expanded.variants;
        let mut files = Vec::new();
        // The short edge names portrait renditions too. Only expose MP4 fallbacks
        // that exist: libraries can disable them or encode fewer than their HLS set.
        // https://bunny.net/docs/stream/mp4-downloads
        for variant in &variants {
            let Some(height) = variant.width.zip(variant.height).map(|(w, h)| w.min(h)) else {
                continue;
            };
            let mut file = player
                .master
                .join(&format!("play_{height}p.mp4"))
                .expect("valid");
            file.set_query(player.master.query());
            if !files.iter().any(|(u, _, _, _)| *u == file) {
                files.push((file, variant.width, variant.height, format!("{height}p")));
            }
        }
        if let Some(original) = player.original {
            files.push((original, None, None, "original".into()));
        }
        for (file, width, height, name) in files {
            let probed = match probe_file(&self.http, &file, PLATFORM, BROWSER_UA, &headers).await {
                Ok(probed) => probed,
                Err(error) => {
                    tracing::debug!(%file, %error, "bunny optional file probe failed");
                    continue;
                }
            };
            let mime = essence(probed.content_type.as_deref());
            if !probed.status.is_success() || mime.starts_with("text/") || is_hls_type(&mime) {
                continue;
            }
            let mut variant = Variant::file(probed.url);
            variant.container = Container::from_mime(&mime);
            if name != "original" {
                variant.container = Some(Container::Mp4);
                variant.video = Some(VideoCodec::H264);
                variant.audio = Some(AudioCodec::Aac);
            }
            variant.width = width;
            variant.height = height;
            variant.size = probed.size;
            variant.duration = duration;
            variant.headers = headers.clone();
            variant.format_id = Some(name.clone());
            variant.label = Some(name);
            variants.push(variant);
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(link.id);
        resolved.title = player.title;
        resolved.description = player.description;
        resolved.duration = duration;
        resolved.live = expanded.live;
        resolved.thumbnail = player.thumbnail;
        resolved.webpage_url = Some(player_url);
        resolved.subtitles = expanded.subtitles;
        resolved.subtitles.extend(player.subtitles);
        resolved.variants = variants;
        Ok(resolved.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::Fixture;
    use crate::resolve::VariantKind;

    const PORTRAIT: &str =
        "https://iframe.mediadelivery.net/embed/113933/e73edec1-e381-4c8b-ae73-717a140e0924";
    const LANDSCAPE: &str =
        "https://iframe.mediadelivery.net/embed/136145/32e34c4b-0d72-437c-9abb-05e67657da34";

    #[test]
    fn links_keep_the_video_and_signed_query() {
        let origin = Url::parse(&format!("{LANDSCAPE}?token=signed&expires=123")).unwrap();
        let link = parse_link(&origin).unwrap();
        assert_eq!(link.library, "136145");
        assert_eq!(link.player(&origin), origin);
        for alias in [
            LANDSCAPE.replace("iframe.", "player."),
            LANDSCAPE.replace(
                "iframe.mediadelivery.net/embed/",
                "video.bunnycdn.com/play/",
            ),
            LANDSCAPE.replace("/embed/", "/play/"),
        ] {
            assert_eq!(parse_link(&Url::parse(&alias).unwrap()), Some(link.clone()));
        }
        assert!(
            parse_link(&Url::parse("https://iframe.mediadelivery.net/embed/1/not-an-id").unwrap())
                .is_none()
        );
        assert!(
            parse_link(
                &Url::parse(&LANDSCAPE.replace("mediadelivery.net", "mediadelivery.net.evil.test"))
                    .unwrap()
            )
            .is_none()
        );
    }

    #[tokio::test]
    async fn recorded_players_offer_hls_existing_files_originals_and_captions() {
        let fixture = Fixture::parse(include_str!("bunny_fixture.json")).unwrap();
        let resolver = BunnyResolver::new(Http::replay(fixture));
        for (url, count, sizes) in [
            (PORTRAIT, 2, vec![310177, 573395, 1259392]),
            (LANDSCAPE, 3, vec![7139436, 12393701, 125159010]),
        ] {
            let resolved = resolver
                .resolve(&Url::parse(url).unwrap())
                .await
                .unwrap()
                .media()
                .unwrap();
            assert!(!resolved.live);
            assert!(resolved.title.is_some());
            assert!(resolved.duration.unwrap().as_secs_f64() > 5.0);
            assert!(resolved.thumbnail.is_some());
            assert_eq!(
                resolved
                    .variants
                    .iter()
                    .filter(|v| v.kind == VariantKind::Hls)
                    .count(),
                count
            );
            let files: Vec<_> = resolved
                .variants
                .iter()
                .filter(|v| v.kind == VariantKind::File)
                .collect();
            assert_eq!(
                files.iter().map(|v| v.size.unwrap()).collect::<Vec<_>>(),
                sizes
            );
            assert_eq!(files.last().unwrap().format_id.as_deref(), Some("original"));
            assert!(
                resolved
                    .variants
                    .iter()
                    .all(|v| v.headers.contains(&("referer".into(), url.to_string())))
            );
            if url == LANDSCAPE {
                assert_eq!(
                    resolved.title.as_deref(),
                    Some("Sanela ist Teil der #arbeitsmarktkraft")
                );
                assert_eq!(
                    resolved
                        .subtitles
                        .iter()
                        .map(|s| s.language.as_str())
                        .collect::<Vec<_>>(),
                    vec!["de", "en"]
                );
                assert!(resolved.subtitles.iter().all(|s| s.auto
                    && s.format == SubtitleFormat::Vtt
                    && s.headers == files[0].headers));
                assert!(!files.iter().any(|v| v.height == Some(1080)));
            } else {
                assert_eq!((files[0].width, files[0].height), (Some(240), Some(352)));
            }
        }
        let missing = Url::parse(
            "https://iframe.mediadelivery.net/embed/200867/2e8545ec-509d-4571-b855-4cf0235ccd75",
        )
        .unwrap();
        assert!(matches!(
            resolver.resolve(&missing).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[test]
    fn protected_players_report_drm() {
        let url = Url::parse(LANDSCAPE).unwrap();
        let result = player_of("<script>var isEntDrm = true;</script>", &url, &url);
        assert!(matches!(result, Err(ResolveError::Drm { .. })));
    }
}
