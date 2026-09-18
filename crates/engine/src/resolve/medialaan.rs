//! Medialaan (DPG Media) videos hosted on mychannels.video, through the embed API the
//! player calls: the player's embed links, the video pages of the group's newspaper and
//! radio sites, and the players embedded in other pages. VTM hands its mychannels ids to
//! [`resolve_mychannels`].
//!
//! DPG Media's Akamai front admits the crawlers it knows and rejects a browser user agent
//! whose connection is not a browser's, so the player page and the embed API are asked
//! for as the Discord embed crawler; the media CDNs answer any user agent.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Tag,
    Variant, clean_title, fetch, hls, navigation_headers, page, path_extension, status_error, util,
};
use crate::http::{BROWSER_UA, EMBED_BOT_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "medialaan";

/// The player's host, then the sites whose video pages play its productions.
const HOSTS: &[&str] = &[
    "mychannels.video",
    "7sur7.be",
    "demorgen.be",
    "hln.be",
    "joe.be",
    "qmusic.be",
    "ad.nl",
    "bd.nl",
    "ed.nl",
    "bndestem.nl",
    "destentor.nl",
    "gelderlander.nl",
    "pzc.nl",
    "tubantia.nl",
    "volkskrant.nl",
];

/// `/embed/{id}` on `mychannels.video` and `embed.mychannels.video`.
static RE_EMBED: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/embed/(\d+)").unwrap());
/// `/production/{id}`, `/sdk/production/{id}`, `/script/production/{id}` on
/// `embed.mychannels.video`.
static RE_PRODUCTION: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(?:s(?:dk|cript)/)?production/(\d+)").unwrap());
/// A site's `/video/…` or `/videos/…` page whose last segment ends in `-{id}` or `~p{id}`.
static RE_ARTICLE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/videos?/(?:[^/?#]+/)*[^/?&#]+(?:-|~p)(\d+)").unwrap());
/// The player page's brand configuration, `window.mychannels.brand_config = {…}`.
static RE_BRAND_CONFIG: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"window\.mychannels\.brand_config\s*=").unwrap());

/// The mychannels production id a link names.
pub fn parse_link(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let path = url.path();
    let captured = match host.as_str() {
        "mychannels.video" => RE_EMBED.captures(path),
        "embed.mychannels.video" => RE_EMBED
            .captures(path)
            .or_else(|| RE_PRODUCTION.captures(path)),
        _ => {
            let site = host.strip_prefix("www.").unwrap_or(&host);
            if !HOSTS.iter().skip(1).any(|known| *known == site) {
                return None;
            }
            RE_ARTICLE.captures(path)
        }
    };
    captured.map(|c| c[1].to_string())
}

/// The production ids of the players a page embeds:
/// `<div data-mychannels-type="video" data-mychannels-id="…">`.
pub fn embedded_ids(html: &str) -> Vec<String> {
    util::tags_where(html, "div", &|attrs| {
        attrs
            .iter()
            .any(|(name, value)| name == "data-mychannels-type" && value == "video")
    })
    .into_iter()
    .filter_map(|tag| util::attribute(&tag, "data-mychannels-id"))
    .filter(|id| !id.is_empty())
    .collect()
}

/// The brand the player page's `window.mychannels.brand_config` names; the embed API
/// answers only to a brand.
pub fn brand_of(html: &str) -> Option<String> {
    let found = RE_BRAND_CONFIG.find(html)?;
    let (config, _) = page::leading_json(&html[found.end()..])?;
    config["brand"]
        .as_str()
        .map(str::trim)
        .filter(|brand| !brand.is_empty())
        .map(str::to_string)
}

/// Reads production `id` as `platform`, the way yt-dlp's `MedialaanBaseIE` does: the
/// brand from the player's embed page, then the embed API as that brand, every stream a
/// variant (HLS playlists expanded, files with the size their quality label names).
pub async fn resolve_mychannels(
    http: &Http,
    platform: &str,
    id: &str,
    origin: &Url,
) -> Result<Resolved, ResolveError> {
    let embed = Url::parse(&format!("https://mychannels.video/embed/{id}"))
        .map_err(|e| ResolveError::malformed(origin, format!("mychannels id {id}: {e}")))?;
    let fetched = fetch(
        http,
        &embed,
        platform,
        EMBED_BOT_UA,
        &navigation_headers(),
        MAX_PAGE,
    )
    .await?;
    if let Some(error) = status_error(fetched.status, origin) {
        return Err(error);
    }
    let brand = brand_of(&fetched.text())
        .ok_or_else(|| ResolveError::malformed(origin, "the player page has no brand config"))?;
    let api = Url::parse(&format!("https://api.mychannels.world/v1/embed/video/{id}"))
        .map_err(|e| ResolveError::malformed(origin, format!("mychannels id {id}: {e}")))?;
    let headers = [
        ("x-mychannels-brand".to_string(), brand),
        ("accept".to_string(), "application/json".to_string()),
    ];
    let fetched = fetch(http, &api, platform, EMBED_BOT_UA, &headers, MAX_PAGE).await?;
    if let Some(error) = status_error(fetched.status, origin) {
        return Err(error);
    }
    let video = fetched.json(origin)?;

    let mut variants = Vec::new();
    let mut subtitles = Vec::new();
    let mut playlist_duration = None;
    let mut playlist_live = false;
    let mut playlist_error = None;
    for stream in video["streams"].as_array().into_iter().flatten() {
        let Some(source) = stream["url"].as_str().and_then(|u| util::join_url(None, u)) else {
            continue;
        };
        let extension = path_extension(&source);
        if extension.as_deref() == Some("m3u8") {
            // The player lists the playlist beside its files: one that cannot be read
            // costs only itself.
            match hls::expand(http, &source, platform, BROWSER_UA, &[]).await {
                Ok(expanded) => {
                    playlist_duration = playlist_duration.or(expanded.duration);
                    playlist_live |= expanded.live;
                    variants.extend(expanded.variants);
                    subtitles.extend(expanded.subtitles);
                }
                Err(error) => playlist_error = Some(error),
            }
        } else {
            let quality = stream["quality"].as_str().map(str::to_string);
            let mut variant = Variant::file(source);
            variant.container = extension.as_deref().and_then(Container::from_extension);
            if variant.container == Some(Container::Mp4) {
                variant.video = Some(VideoCodec::H264);
                variant.audio = Some(AudioCodec::Aac);
            }
            if let Some(quality) = &quality {
                let (width, height, fps) = util::parse_resolution(quality);
                variant.width = width;
                variant.height = height;
                variant.fps = fps;
            }
            variant.format_id = quality.clone();
            variant.label = quality;
            variants.push(variant);
        }
    }
    if variants.is_empty() {
        return Err(match playlist_error {
            Some(error) => error,
            None => ResolveError::unavailable(origin, "the video has no streams"),
        });
    }

    let mut resolved = Resolved::new(platform);
    resolved.id = Some(id.to_string());
    resolved.title = video["title"]
        .as_str()
        .map(util::clean_html)
        .and_then(|t| clean_title(&t));
    resolved.description = video["description"]
        .as_str()
        .map(util::clean_html)
        .filter(|d| !d.is_empty());
    let live = video["live"].as_bool() == Some(true) || playlist_live;
    // A live playlist's length is only its window, not the stream's duration.
    resolved.duration = util::int(&video["durationMs"])
        .filter(|ms| *ms >= 0)
        .map(|ms| Duration::from_millis(ms as u64))
        .or(playlist_duration.filter(|_| !live));
    resolved.live = live;
    resolved.uploaded_at = util::epoch(&video["publicationTimestampMs"]);
    resolved.thumbnail = util::url_of(&video["image"]["baseUrl"], None);
    resolved.uploader = video["organisation"]["title"]
        .as_str()
        .map(util::clean_html)
        .and_then(|t| clean_title(&t))
        .or_else(|| {
            video["channel"]["title"]
                .as_str()
                .map(util::clean_html)
                .and_then(|t| clean_title(&t))
        });
    resolved.webpage_url = Some(origin.clone());
    resolved.subtitles = subtitles;
    resolved.variants = variants;
    Ok(resolved)
}

pub struct MedialaanResolver {
    http: Http,
}

impl MedialaanResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for MedialaanResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Medialaan",
            hosts: HOSTS,
            features: &["videos", "live", "embeds"],
            formats: &["mp4", "hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::News, Tag::Video, Tag::Live],
            session: SessionSupport::None,
            examples: &[
                "https://www.bndestem.nl/video/de-terugkeer-van-ally-de-aap-en-wie-vertrekt-er-nog-bij-nac~p193993",
                "https://www.gelderlander.nl/video/kanalen/degelderlander~c320/series/snel-nieuws~s984/noodbevel-in-doetinchem-politie-stuurt-mensen-centrum-uit~p194093",
                "https://www.7sur7.be/videos/production/lla-tendance-tiktok-qui-enflamme-lespagne-707650",
                "https://mychannels.video/embed/313117",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let id = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let resolved = resolve_mychannels(&self.http, PLATFORM, &id, url).await?;
        Ok(Resolution::from(resolved))
    }

    fn embeds_in(&self, page: &Page) -> Vec<Url> {
        embedded_ids(page.html())
            .into_iter()
            .filter_map(|id| Url::parse(&format!("https://mychannels.video/embed/{id}")).ok())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};
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

    const PLAYER_PAGE: &str = r##"<html><head><script>
        window.mychannels = window.mychannels || {};
        window.mychannels.brand_config = {"brand":"bndestem","theme":{"color":"#c00"}};
        </script></head><body></body></html>"##;
    const MASTER: &str = "https://videos.mychannels.video/193993/master.m3u8";
    const MEDIA: &str = "https://videos.mychannels.video/193993/1080p/index.m3u8";

    fn playlists(fixture: &mut Fixture, ended: bool) {
        fixture.exchanges.push(get(
            MASTER,
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=4000000,RESOLUTION=1920x1080\n1080p/index.m3u8\n"
                .into(),
        ));
        let mut media =
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\n0.ts\n#EXTINF:4.0,\n1.ts\n"
                .to_string();
        if ended {
            media.push_str("#EXT-X-ENDLIST\n");
        }
        fixture
            .exchanges
            .push(get(MEDIA, 200, "application/vnd.apple.mpegurl", media));
    }

    #[test]
    fn links_are_read() {
        let id = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            id(
                "https://www.bndestem.nl/video/de-terugkeer-van-ally-de-aap-en-wie-vertrekt-er-nog-bij-nac~p193993"
            ),
            Some("193993".into())
        );
        assert_eq!(
            id(
                "https://www.gelderlander.nl/video/kanalen/degelderlander~c320/series/snel-nieuws~s984/noodbevel-in-doetinchem-politie-stuurt-mensen-centrum-uit~p194093"
            ),
            Some("194093".into())
        );
        assert_eq!(
            id(
                "https://www.7sur7.be/videos/production/lla-tendance-tiktok-qui-enflamme-lespagne-707650"
            ),
            Some("707650".into())
        );
        assert_eq!(
            id("https://mychannels.video/embed/313117"),
            Some("313117".into())
        );
        assert_eq!(
            id("https://embed.mychannels.video/sdk/production/193993"),
            Some("193993".into())
        );
        assert_eq!(
            id("https://embed.mychannels.video/script/production/193993"),
            Some("193993".into())
        );
        assert_eq!(
            id("https://embed.mychannels.video/production/193993"),
            Some("193993".into())
        );
        assert_eq!(
            id("https://embed.mychannels.video/embed/193993"),
            Some("193993".into())
        );
        assert_eq!(
            id("https://hln.be/video/nieuws/brand-in-antwerpen-1576607?utm=x"),
            Some("1576607".into())
        );
        assert_eq!(
            id("https://www.demorgen.be/snelnieuws/tom-waes-promoot-alcoholtesten~b7457c0d/"),
            None
        );
        assert_eq!(id("https://www.hln.be/video/"), None);
        assert_eq!(id("https://mychannels.video/embed/"), None);
        assert_eq!(id("https://mychannels.video/production/193993"), None);
        assert_eq!(id("https://example.com/video/clip-123"), None);
    }

    #[test]
    fn embedded_players_are_found() {
        let html = r#"<div class="video" data-mychannels-type="video" data-mychannels-id="1576607"></div>
            <div data-mychannels-type='video' data-mychannels-id='1576608' data-autoplay="1">
            <div data-mychannels-type="playlist" data-mychannels-id="9"></div>
            <div data-mychannels-type="video"></div>"#;
        assert_eq!(embedded_ids(html), vec!["1576607", "1576608"]);
        let resolver = MedialaanResolver::new(Http::replay(Fixture::new(PLATFORM, None)));
        let page = Page::parse(
            html,
            &Url::parse("https://www.demorgen.be/snelnieuws/x~b1/").unwrap(),
        );
        let found: Vec<String> = resolver
            .embeds_in(&page)
            .iter()
            .map(|u| u.to_string())
            .collect();
        assert_eq!(
            found,
            vec![
                "https://mychannels.video/embed/1576607",
                "https://mychannels.video/embed/1576608"
            ]
        );
        assert!(resolver.matches(&Url::parse(&found[0]).unwrap()));
    }

    #[tokio::test]
    async fn videos_resolve_with_every_stream() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://mychannels.video/embed/193993",
            200,
            "text/html",
            PLAYER_PAGE.into(),
        ));
        fixture.exchanges.push(get("https://api.mychannels.world/v1/embed/video/193993", 200, "application/json", json!({
            "id": 193993,
            "title": "De terugkeer van Ally de Aap en wie vertrekt er nog bij <b>NAC</b>?",
            "description": "In een nieuwe Gegenpressing video bespreken Yadran Blanco en Dennis Kas het nieuws omrent NAC.",
            "durationMs": 238000,
            "live": false,
            "publicationTimestampMs": 1611663540000i64,
            "genre": {"title": "Sports"},
            "tags": [{"title": "NAC"}, {"title": "Voetbal"}],
            "image": {"baseUrl": "https://images.mychannels.video/imgix/193993.jpg"},
            "channel": {"id": 418, "title": "BN DeStem"},
            "organisation": {"id": 26, "title": "BN De Stem"},
            "show": {"id": 972, "title": "Korte Reportage"},
            "streams": [
                {"url": MASTER, "quality": "hls"},
                {"url": "https://videos.mychannels.video/193993/720p.mp4", "quality": "720p"},
                {"url": "https://videos.mychannels.video/193993/1920x1080.mp4", "quality": "1920x1080"},
                {"url": "", "quality": "480p"}
            ]
        }).to_string()));
        playlists(&mut fixture, true);
        let resolver = MedialaanResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.bndestem.nl/video/de-terugkeer-van-ally-de-aap-en-wie-vertrekt-er-nog-bij-nac~p193993").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("193993"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("De terugkeer van Ally de Aap en wie vertrekt er nog bij NAC?")
        );
        assert_eq!(
            resolved.description.as_deref(),
            Some(
                "In een nieuwe Gegenpressing video bespreken Yadran Blanco en Dennis Kas het nieuws omrent NAC."
            )
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(238)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1611663540)
        );
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://images.mychannels.video/imgix/193993.jpg"
        );
        assert_eq!(resolved.uploader.as_deref(), Some("BN De Stem"));
        assert_eq!(resolved.webpage_url.as_ref(), Some(&url));
        assert!(!resolved.live);
        assert_eq!(resolved.variants.len(), 3);
        assert_eq!(resolved.variants[0].kind, VariantKind::Hls);
        assert_eq!(resolved.variants[0].url.as_str(), MEDIA);
        assert_eq!(resolved.variants[0].height, Some(1080));
        let file = &resolved.variants[1];
        assert_eq!(file.kind, VariantKind::File);
        assert_eq!(file.height, Some(720));
        assert_eq!(file.container, Some(Container::Mp4));
        assert_eq!(file.video, Some(VideoCodec::H264));
        assert_eq!(file.format_id.as_deref(), Some("720p"));
        assert_eq!(resolved.variants[2].width, Some(1920));
        assert_eq!(resolved.variants[2].height, Some(1080));
    }

    #[tokio::test]
    async fn live_streams_are_marked() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://mychannels.video/embed/313117",
            200,
            "text/html",
            PLAYER_PAGE.into(),
        ));
        fixture.exchanges.push(get(
            "https://api.mychannels.world/v1/embed/video/313117",
            200,
            "application/json",
            json!({
                "title": "Nieuws Update live",
                "durationMs": -1,
                "live": true,
                "publicationTimestampMs": 1735169425000i64,
                "channel": {"id": 238, "title": "AD"},
                "organisation": {"id": 1, "title": "AD"},
                "streams": [{"url": MASTER}]
            })
            .to_string(),
        ));
        playlists(&mut fixture, false);
        let resolver = MedialaanResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://mychannels.video/embed/313117").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert!(resolved.live);
        assert!(resolved.variants[0].live);
        assert_eq!(resolved.duration, None);
        assert_eq!(resolved.uploader.as_deref(), Some("AD"));
    }

    #[tokio::test]
    async fn missing_videos_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://mychannels.video/embed/1",
            200,
            "text/html",
            PLAYER_PAGE.into(),
        ));
        fixture.exchanges.push(get(
            "https://api.mychannels.world/v1/embed/video/1",
            404,
            "application/json",
            json!({"message": "Not Found"}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://mychannels.video/embed/2",
            200,
            "text/html",
            "<html><body>Access Denied</body></html>".into(),
        ));
        fixture.exchanges.push(get(
            "https://mychannels.video/embed/3",
            200,
            "text/html",
            PLAYER_PAGE.into(),
        ));
        fixture.exchanges.push(get(
            "https://api.mychannels.world/v1/embed/video/3",
            200,
            "application/json",
            json!({"title": "gone", "streams": []}).to_string(),
        ));
        let resolver = MedialaanResolver::new(Http::replay(fixture));
        let link = |id: &str| Url::parse(&format!("https://mychannels.video/embed/{id}")).unwrap();
        assert!(matches!(
            resolver.resolve(&link("1")).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver.resolve(&link("2")).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Malformed { detail, .. } if detail.contains("brand config")),
            "{error}"
        );
        let error = resolver.resolve(&link("3")).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("no streams")),
            "{error}"
        );
    }
}
