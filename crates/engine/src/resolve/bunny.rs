//! Bunny Stream embeds: the player's HLS renditions, available MP4 fallbacks and
//! original upload, with the player referer on media and captions. Libraries locked to
//! their own site are asked with that site as referer, which the generic web resolver
//! names when it finds the player on a page, and MediaCage streams are activated and
//! kept alive with the player's pings while they download.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use scraper::Selector;
use url::Url;

use super::page::Page;
use super::{
    Keepalive, MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport,
    SubtitleFormat, SubtitleTrack, Tag, Variant, clean_title, essence, fetch, hls, is_hls_type,
    probe_file, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

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
/// `.setAttribute('src', '…')`: the stream a MediaCage player sets on its element.
static RE_SET_SRC: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\.setAttribute\(['"]src['"],\s*['"]([^'"]+)['"]\)"#).unwrap());
/// `loadUrl('…/activate')`: the request that unlocks a MediaCage stream.
static RE_ACTIVATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"loadUrl\(['"]([^'"]+/activate)['"]"#).unwrap());
/// `loadUrl('…/ping')`: the playback report the stream is kept alive with.
static RE_PING: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"loadUrl\(['"]([^'"]+/ping)['"]"#).unwrap());
/// `<iframe src="https://iframe.mediadelivery.net/embed/{library}/{id}">`.
static RE_IFRAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)<iframe[^>]+src=["']((?:https?:)?//(?:iframe|player)\.mediadelivery\.net/(?:embed|play)/\d+/[0-9a-f-]{36}[^"']*)["']"#)
        .unwrap()
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub library: String,
    pub id: String,
    /// The page the player was found on, which libraries locked to their site are
    /// asked with.
    pub referrer: Option<Url>,
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
                referrer: util::query_param(url, "referrer").and_then(|r| Url::parse(&r).ok()),
            })
        }
        _ => None,
    }
}

impl Link {
    fn player(&self, origin: &Url) -> Url {
        let mut url = Url::parse(&format!("{PLAYER}{}/{}", self.library, self.id)).expect("valid");
        // Signed embed links carry their authorization in the query.
        let signed: Vec<(String, String)> = origin
            .query_pairs()
            .filter(|(k, _)| k == "token" || k == "expires")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        if !signed.is_empty() {
            url.query_pairs_mut()
                .extend_pairs(signed.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        }
        url
    }
}

/// A MediaCage stream: the player sets it on its element after an activation request,
/// and keeps it alive with pings signed by the secret and context in its link.
pub struct Cage {
    pub stream: Url,
    pub activate: Url,
    pub ping: Url,
    pub secret: String,
    pub context_id: String,
}

pub fn cage_of(html: &str) -> Option<Cage> {
    let stream = util::search(&RE_SET_SRC, html).and_then(|s| Url::parse(&s).ok())?;
    let activate = util::search(&RE_ACTIVATE, html).and_then(|s| Url::parse(&s).ok())?;
    let ping = util::search(&RE_PING, html).and_then(|s| Url::parse(&s).ok())?;
    let secret = util::query_param(&stream, "secret")?;
    let context_id = util::query_param(&stream, "contextId")?;
    Some(Cage {
        stream,
        activate,
        ping,
        secret,
        context_id,
    })
}

struct Player {
    title: Option<String>,
    description: Option<String>,
    duration: Option<Duration>,
    uploaded_at: Option<jiff::Timestamp>,
    thumbnail: Option<Url>,
    /// The HLS master of a plain player.
    master: Option<Url>,
    /// The stream of a MediaCage player.
    cage: Option<Cage>,
    original: Option<Url>,
    subtitles: Vec<SubtitleTrack>,
}

fn player_of(html: &str, base: &Url, origin: &Url) -> Result<Player, ResolveError> {
    let page = Page::parse(html, base);
    // A library that refuses the request answers a page titled with the status.
    match page.title().as_deref().map(str::trim) {
        Some("403") => {
            return Err(ResolveError::unavailable(
                origin,
                "the library serves the video only to its own site; the page it is embedded on must name it",
            ));
        }
        Some("404") => return Err(ResolveError::NotFound(origin.clone())),
        _ => {}
    }
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
        .filter(|u| matches!(u.scheme(), "http" | "https"));
    let cage = cage_of(html);
    if master.is_none() && cage.is_none() {
        return Err(ResolveError::NotFound(origin.clone()));
    }
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
    let objects = page.ld_json();
    let ld = super::page::ld_objects_of_type(&objects, "VideoObject")
        .into_iter()
        .next()
        .cloned()
        .unwrap_or_default();
    Ok(Player {
        title: page
            .title()
            .or_else(|| ld["name"].as_str().and_then(clean_title)),
        description: page
            .meta("og:description")
            .and_then(|s| clean_title(&s))
            .or_else(|| ld["description"].as_str().and_then(clean_title)),
        duration: page
            .meta("video:duration")
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|s| s.is_finite() && *s > 0.0)
            .map(Duration::from_secs_f64),
        uploaded_at: ld["uploadDate"].as_str().and_then(util::parse_timestamp),
        thumbnail: page
            .poster()
            .or_else(|| util::url_of(&ld["thumbnailUrl"], None)),
        master,
        cage,
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
            media: &[MediaKind::Video],
            tags: &[Tag::Players],
            session: SessionSupport::None,
            examples: &[
                "https://iframe.mediadelivery.net/embed/136145/32e34c4b-0d72-437c-9abb-05e67657da34",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    /// Players other pages frame, each with that page as its referrer.
    fn embeds_in(&self, page: &Page) -> Vec<Url> {
        let mut found: Vec<Url> = Vec::new();
        for caps in RE_IFRAME.captures_iter(page.html()) {
            let Some(mut url) = util::join_url(Some(page.url()), &util::html_unescape(&caps[1]))
            else {
                continue;
            };
            if parse_link(&url).is_none() {
                continue;
            }
            url.query_pairs_mut()
                .append_pair("referrer", page.url().as_str());
            if !found.contains(&url) {
                found.push(url);
            }
        }
        found
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let player_url = link.player(url);
        let page_referer = link
            .referrer
            .as_ref()
            .map(|r| r.to_string())
            .unwrap_or_else(|| "https://iframe.mediadelivery.net/".to_string());
        let page_headers = vec![("referer".to_string(), page_referer)];
        let fetched = fetch(
            &self.http,
            &player_url,
            PLATFORM,
            BROWSER_UA,
            &page_headers,
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        let player = player_of(&fetched.text(), &fetched.url, url)?;
        let headers = vec![("referer".to_string(), fetched.url.to_string())];
        let mut variants = Vec::new();
        let mut subtitles = Vec::new();
        let mut duration = player.duration;
        let mut live = false;
        if let Some(master) = &player.master {
            let expanded = hls::expand(&self.http, master, PLATFORM, BROWSER_UA, &headers)
                .await
                .map_err(|e| e.at(url))?;
            duration = expanded.duration.or(duration);
            live = expanded.live;
            for mut variant in expanded.variants {
                variant.format_id = Some(match &variant.label {
                    Some(label) => format!("hls-{label}"),
                    None => "hls".to_string(),
                });
                variants.push(variant);
            }
            subtitles.extend(expanded.subtitles);
        }
        if let Some(cage) = &player.cage {
            // Activation unlocks the stream for the pings that follow.
            let activation = fetch(
                &self.http,
                &cage.activate,
                PLATFORM,
                BROWSER_UA,
                &headers,
                MAX_PAGE,
            )
            .await?;
            if let Some(error) = status_error(activation.status, url) {
                return Err(error);
            }
            let expanded = hls::expand(&self.http, &cage.stream, PLATFORM, BROWSER_UA, &headers)
                .await
                .map_err(|e| e.at(url))?;
            duration = duration.or(expanded.duration);
            live |= expanded.live;
            for mut variant in expanded.variants {
                variant.keepalive = Some(Keepalive::BunnyPing {
                    url: cage.ping.clone(),
                    secret: cage.secret.clone(),
                    context_id: cage.context_id.clone(),
                });
                variant.format_id = Some(match &variant.label {
                    Some(label) => format!("cage-{label}"),
                    None => "cage".to_string(),
                });
                variants.push(variant);
            }
            subtitles.extend(expanded.subtitles);
        }
        let mut files = Vec::new();
        // The short edge names portrait renditions too. Only expose MP4 fallbacks
        // that exist: libraries can disable them or encode fewer than their HLS set.
        // https://bunny.net/docs/stream/mp4-downloads
        if let Some(master) = &player.master {
            for variant in &variants {
                let Some(height) = variant.width.zip(variant.height).map(|(w, h)| w.min(h)) else {
                    continue;
                };
                let mut file = master.join(&format!("play_{height}p.mp4")).expect("valid");
                file.set_query(master.query());
                if !files.iter().any(|(u, _, _, _)| *u == file) {
                    files.push((file, variant.width, variant.height, format!("{height}p")));
                }
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
        resolved.live = live;
        resolved.uploaded_at = player.uploaded_at;
        resolved.thumbnail = player.thumbnail;
        resolved.webpage_url = Some(player_url);
        resolved.subtitles = subtitles;
        resolved.subtitles.extend(player.subtitles);
        if variants.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
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
        let third = resolver
            .resolve(&Url::parse("https://iframe.mediadelivery.net/embed/200867/2e8545ec-509d-4571-b855-4cf0235ccd75").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(third.title.as_deref(), Some("netflix part 1"));
        assert!(third.uploaded_at.is_some());
        assert_eq!(third.variants.len(), 3);
        assert!(
            third
                .variants
                .iter()
                .all(|v| v.kind == super::super::VariantKind::Hls && v.height.is_some())
        );
        assert_eq!(third.variants[0].format_id.as_deref(), Some("hls-480p"));
    }

    #[test]
    fn protected_players_report_drm() {
        let url = Url::parse(LANDSCAPE).unwrap();
        let result = player_of("<script>var isEntDrm = true;</script>", &url, &url);
        assert!(matches!(result, Err(ResolveError::Drm { .. })));
    }

    #[tokio::test]
    async fn mediacage_players_are_activated_and_their_streams_kept_alive() {
        use crate::http::{Exchange, RecordedBody, RecordedRequest, RecordedResponse};
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
        let page = r#"<html><head><title>Caged</title><script type="application/ld+json">{"@type":"VideoObject","name":"Caged","uploadDate":"2024-05-31T10:44:29Z","thumbnailUrl":"https://vz-1.b-cdn.net/x/thumbnail.jpg"}</script></head><body><video id="main-video"></video><script>
            player.setAttribute('src', 'https://video-1.mediadelivery.net/play/1/abc/playlist.drm?contextId=ctx1&secret=s3cret');
            loadUrl('https://video-1.mediadelivery.net/.drm/1/abc/activate');
            loadUrl('https://video-1.mediadelivery.net/.drm/1/abc/ping');
        </script></body></html>"#;
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://iframe.mediadelivery.net/embed/1/2e8545ec-509d-4571-b855-4cf0235ccd75",
            200,
            "text/html",
            page,
        ));
        fixture.exchanges.push(get(
            "https://video-1.mediadelivery.net/.drm/1/abc/activate",
            200,
            "application/json",
            "{}",
        ));
        fixture.exchanges.push(get("https://video-1.mediadelivery.net/play/1/abc/playlist.drm?contextId=ctx1&secret=s3cret", 200, "application/vnd.apple.mpegurl", "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720\n720.m3u8\n"));
        fixture.exchanges.push(get(
            "https://video-1.mediadelivery.net/play/1/abc/720.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n0.ts\n#EXT-X-ENDLIST\n",
        ));
        fixture.exchanges.push(get(
            "https://iframe.mediadelivery.net/embed/1/e73edec1-e381-4c8b-ae73-717a140e0924",
            200,
            "text/html",
            "<html><head><title>403</title></head></html>",
        ));
        let resolver = BunnyResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(
                &Url::parse(
                    "https://iframe.mediadelivery.net/embed/1/2e8545ec-509d-4571-b855-4cf0235ccd75",
                )
                .unwrap(),
            )
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.title.as_deref(), Some("Caged"));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1717152269)
        );
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].height, Some(720));
        assert_eq!(resolved.variants[0].format_id.as_deref(), Some("cage-720p"));
        assert_eq!(
            resolved.variants[0].keepalive,
            Some(Keepalive::BunnyPing {
                url: Url::parse("https://video-1.mediadelivery.net/.drm/1/abc/ping").unwrap(),
                secret: "s3cret".into(),
                context_id: "ctx1".into()
            })
        );
        let error = resolver
            .resolve(
                &Url::parse(
                    "https://iframe.mediadelivery.net/embed/1/e73edec1-e381-4c8b-ae73-717a140e0924",
                )
                .unwrap(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("own site")),
            "{error}"
        );
        let page = Page::parse(
            r#"<iframe src="https://iframe.mediadelivery.net/embed/136145/32e34c4b-0d72-437c-9abb-05e67657da34?autoplay=true"></iframe>"#,
            &Url::parse("https://example.com/post").unwrap(),
        );
        let embeds = resolver.embeds_in(&page);
        assert_eq!(embeds.len(), 1);
        let link = parse_link(&embeds[0]).unwrap();
        assert_eq!(
            link.referrer.as_ref().unwrap().as_str(),
            "https://example.com/post"
        );
    }
}
