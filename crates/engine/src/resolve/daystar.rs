//! Daystar clips: the player page frames a Lightcast player, whose frame names the
//! configuration script signed for the visit; the script lists the clip with its HLS
//! playlist, captions and poster.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport,
    SubtitleFormat, SubtitleTrack, clean_title, fetch_ok, hls, navigation_headers, util,
};
use crate::http::{BROWSER_UA, Http};

pub const PLATFORM: &str = "daystar";

static RE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/(\w+)").unwrap());
static RE_IFRAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<iframe[^>]+src="([^"]+)""#).unwrap());
/// `var configUrl = 'config2.php?…&ct=…'` in the player frame: the configuration script,
/// signed for the visit, that the frame loads next.
static RE_CONFIG_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"var\s+configUrl\s*=\s*['"]([^'"]+)['"]"#).unwrap());
/// `playlist: [` in the configuration's `jwplayer().setup` call.
static RE_PLAYLIST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\bplaylist:\s*\[").unwrap());
/// `subtitles_en.srt`: the language a caption file is named with.
static RE_CAPTION_LANGUAGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"_([a-z]{2,3})\.(?:srt|vtt)(?:\?|$)").unwrap());

/// The clip id a `player.daystar.tv` link names.
pub fn video_id(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    if !url.host_str()?.eq_ignore_ascii_case("player.daystar.tv") {
        return None;
    }
    RE_ID.captures(url.path()).map(|c| c[1].to_string())
}

/// The playlist the configuration script hands `jwplayer().setup`: one item per clip,
/// with its `sources`, `tracks`, `image` and `duration`.
pub fn playlist_of(config: &str) -> Option<Value> {
    let found = RE_PLAYLIST.find(config)?;
    let after = &config[found.end() - 1..];
    let end = util::balanced_js_end(after)?;
    util::parse_js(&after[..end])
}

/// The caption format a track file's extension names.
fn caption_format(url: &Url) -> Option<SubtitleFormat> {
    match super::path_extension(url)?.as_str() {
        "srt" => Some(SubtitleFormat::Srt),
        "vtt" => Some(SubtitleFormat::Vtt),
        _ => None,
    }
}

pub struct DaystarResolver {
    http: Http,
}

impl DaystarResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for DaystarResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Daystar",
            hosts: &["player.daystar.tv"],
            features: &["clips"],
            formats: &["hls"],
            session: SessionSupport::None,
            examples: &["https://player.daystar.tv/0MTO2ITM"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        video_id(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let id = video_id(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let html = fetch_ok(
            &self.http,
            url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?
        .text();
        let (page_title, page_description) = {
            let page = Page::parse(&html, url);
            (
                page.meta("og:title")
                    .or_else(|| page.meta("twitter:title"))
                    .and_then(|t| clean_title(&t)),
                page.meta("og:description")
                    .or_else(|| page.meta("twitter:description"))
                    .and_then(|d| clean_title(&d)),
            )
        };
        let frame = util::search(&RE_IFRAME, &html)
            .and_then(|src| util::join_url(Some(url), &util::html_unescape(&src)))
            .ok_or_else(|| ResolveError::malformed(url, "the player page frames no player"))?;
        let page_referer = vec![("referer".to_string(), url.to_string())];
        let frame_html = fetch_ok(
            &self.http,
            &frame,
            PLATFORM,
            BROWSER_UA,
            &page_referer,
            MAX_PAGE,
        )
        .await?
        .text();
        let config_url = util::search(&RE_CONFIG_URL, &frame_html)
            .and_then(|link| util::join_url(Some(&frame), &link))
            .ok_or_else(|| {
                ResolveError::malformed(url, "the player frame names no configuration script")
            })?;
        let frame_referer = vec![("referer".to_string(), frame.to_string())];
        let config = fetch_ok(
            &self.http,
            &config_url,
            PLATFORM,
            BROWSER_UA,
            &frame_referer,
            MAX_PAGE,
        )
        .await?
        .text();
        let playlist = playlist_of(&config).ok_or_else(|| {
            ResolveError::malformed(url, "the player configuration lists no playlist")
        })?;
        let items = playlist.as_array().cloned().unwrap_or_default();
        let item = items
            .iter()
            .find(|item| item["mediaid"].as_str() == Some(id.as_str()))
            .or_else(|| items.first())
            .ok_or_else(|| ResolveError::malformed(url, "the player playlist is empty"))?;

        let mut resolved = Resolved::new(PLATFORM);
        let mut last_error = None;
        for source in item["sources"].as_array().into_iter().flatten() {
            let Some(file) = source["file"].as_str() else {
                continue;
            };
            if source["type"].as_str() != Some("m3u8") {
                continue;
            }
            let Some(manifest) = util::join_url(Some(&config_url), file) else {
                continue;
            };
            match hls::expand(&self.http, &manifest, PLATFORM, BROWSER_UA, &frame_referer).await {
                Ok(expanded) => {
                    resolved.variants.extend(expanded.variants);
                    resolved.subtitles.extend(expanded.subtitles);
                    resolved.duration = resolved.duration.or(expanded.duration);
                    resolved.live |= expanded.live;
                }
                Err(error) => last_error = Some(error),
            }
        }
        if resolved.variants.is_empty() {
            return Err(last_error.unwrap_or_else(|| {
                ResolveError::unavailable(url, "the player offers no HLS stream")
            }));
        }
        for track in item["tracks"].as_array().into_iter().flatten() {
            if track["kind"].as_str() != Some("captions") {
                continue;
            }
            let Some(file) = track["file"]
                .as_str()
                .and_then(|file| util::join_url(Some(&config_url), file))
            else {
                continue;
            };
            let Some(format) = caption_format(&file) else {
                continue;
            };
            let label = track["label"].as_str().and_then(clean_title);
            let language = util::search(&RE_CAPTION_LANGUAGE, file.path())
                .or_else(|| label.clone())
                .unwrap_or_else(|| "und".to_string());
            resolved.subtitles.push(SubtitleTrack {
                url: file,
                language,
                name: label,
                format,
                auto: false,
                headers: Vec::new(),
            });
        }
        resolved.id = Some(id);
        resolved.title = page_title.or_else(|| item["title"].as_str().and_then(clean_title));
        resolved.description =
            page_description.or_else(|| item["description"].as_str().and_then(clean_title));
        resolved.duration = util::seconds(&item["duration"]).or(resolved.duration);
        resolved.thumbnail = item["image"]
            .as_str()
            .and_then(|image| util::join_url(Some(&config_url), image));
        resolved.webpage_url = Some(url.clone());
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};

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

    const PLAYER: &str =
        "https://www.lightcast.com/embed/player.php?autoStart=0&type=&site=playerLanding&id=439621";
    const CONFIG: &str = "https://www.lightcast.com/embed/config2.php?autoStart=0&type=&site=playerLanding&id=439621&ct=NWQ3NjI0MzcyYzUzM2YxMTEzMWIxODYxNzFmYWE1Y2F8NDM5NjIxfDE3ODk1MTQ2MjI&refererURL=https%253A%252F%252Fplayer.daystar.tv%252F0MTO2ITM";
    const MASTER: &str = "https://www.lightcast.com/embed/playlist_m3u8_vod2.php?code=eyJ2aWRlb19pZCI6IjQzOTYyMSJ9&play_id=1789514677777468";

    fn page() -> String {
        format!(
            r#"<html><head><meta property='og:title' content='The Dark World of COVID Pt. 1 | Aaron Siri'>
            <meta property='og:description' content='Attorney Aaron Siri exposes the truth.'></head>
            <body><iframe style="width:86vw" src="{PLAYER}" frameborder="0" allowfullscreen></iframe></body></html>"#
        )
    }

    fn frame() -> String {
        r#"<html><head><title>Media Player</title></head><body><script>
            var configUrl = 'config2.php?autoStart=0&type=&site=playerLanding&id=439621&ct=NWQ3NjI0MzcyYzUzM2YxMTEzMWIxODYxNzFmYWE1Y2F8NDM5NjIxfDE3ODk1MTQ2MjI&refererURL=https%253A%252F%252Fplayer.daystar.tv%252F0MTO2ITM';
            if(navigator.platform === 'MacIntel'){ configUrl += '&newIpad=1'; }
            </script></body></html>"#.to_string()
    }

    fn config() -> String {
        r#"lightcastPlayer = jwplayer("lightcast-player");lightcastPlayer.setup({skin: {name: 'lightcast',},androidhls: 'true',visualplaylist: 'false',aspectratio: '16:9',autostart: false,playlist: [{title: "The Dark World of COVID Pt. 1 | Aaron Siri",description: "Is the fear of the pandemic being used? (J2167)",duration: 1710,mediaid: "0MTO2ITM",videoid: "439621",tracks: [{file: "\/\/st1-fs.cdn01.net\/videos\/0000439\/0439621\/subtitles\/subtitles_en.srt?v1",label: "English",kind: "captions",},{file: "\/\/st1-fs.cdn01.net\/videos\/0000439\/0439621\/scrub_preview\/F327BC0GK-web.vtt?v1",kind: "thumbnails",},],image: "https:\/\/st1-fs.cdn01.net\/videos\/0000439\/0439621\/thumbs\/0439621__101oahd.jpg",sources: [{file: "playlist_m3u8_vod2.php?code=eyJ2aWRlb19pZCI6IjQzOTYyMSJ9&play_id=1789514677777468",type: "m3u8",},],},],sharing: {heading: 'Share Video',link: 'https://player.daystar.tv/MEDIAID',sites: [{icon: "data:image/svg+xml;charset=US-ASCII,%3Csvg%3E",},],},});"#.to_string()
    }

    #[test]
    fn links_are_read() {
        let id = |s: &str| video_id(&Url::parse(s).unwrap());
        assert_eq!(
            id("https://player.daystar.tv/0MTO2ITM"),
            Some("0MTO2ITM".into())
        );
        assert_eq!(
            id("http://player.daystar.tv/abc_1?x=1"),
            Some("abc_1".into())
        );
        assert_eq!(id("https://player.daystar.tv/"), None);
        assert_eq!(id("https://www.daystar.tv/0MTO2ITM"), None);
    }

    #[test]
    fn the_configuration_playlist_is_read() {
        let playlist = playlist_of(&config()).unwrap();
        let items = playlist.as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["mediaid"], "0MTO2ITM");
        assert_eq!(items[0]["duration"], 1710);
        assert_eq!(items[0]["sources"][0]["type"], "m3u8");
        assert_eq!(items[0]["tracks"][0]["kind"], "captions");
        assert!(playlist_of("visualplaylist: 'false', nothing else").is_none());
    }

    #[tokio::test]
    async fn clips_resolve_through_the_signed_lightcast_configuration() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://player.daystar.tv/0MTO2ITM",
            200,
            "text/html",
            page(),
        ));
        fixture
            .exchanges
            .push(get(PLAYER, 200, "text/html", frame()));
        fixture
            .exchanges
            .push(get(CONFIG, 200, "text/html", config()));
        fixture.exchanges.push(get(
            MASTER,
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=3588000,NAME=\"Full HD\"\nhttps://hls-vod-media-fs.cdn01.net/x/video_3584k/F327BC0GK.m3u8?play_id=1789514677777468\n"
                .into(),
        ));
        fixture.exchanges.push(get(
            "https://hls-vod-media-fs.cdn01.net/x/video_3584k/F327BC0GK.m3u8?play_id=1789514677777468",
            200,
            "application/x-mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:7\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:5.439,\nF327BC0GK-1.ts\n#EXTINF:6.139,\nF327BC0GK-2.ts\n#EXT-X-ENDLIST\n".into(),
        ));
        let resolver = DaystarResolver::new(Http::replay(fixture));
        let url = Url::parse("https://player.daystar.tv/0MTO2ITM").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("0MTO2ITM"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("The Dark World of COVID Pt. 1 | Aaron Siri")
        );
        assert_eq!(
            resolved.description.as_deref(),
            Some("Attorney Aaron Siri exposes the truth.")
        );
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://st1-fs.cdn01.net/videos/0000439/0439621/thumbs/0439621__101oahd.jpg"
        );
        assert_eq!(resolved.duration, Some(std::time::Duration::from_secs(1710)));
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].bitrate, Some(3_588_000));
        assert!(
            resolved.variants[0]
                .headers
                .iter()
                .any(|(k, v)| k == "referer" && v == PLAYER)
        );
        assert_eq!(resolved.subtitles.len(), 1);
        assert_eq!(resolved.subtitles[0].language, "en");
        assert_eq!(resolved.subtitles[0].name.as_deref(), Some("English"));
        assert_eq!(resolved.subtitles[0].format, SubtitleFormat::Srt);
        assert_eq!(
            resolved.subtitles[0].url.as_str(),
            "https://st1-fs.cdn01.net/videos/0000439/0439621/subtitles/subtitles_en.srt?v1"
        );
    }

    #[tokio::test]
    async fn missing_clips_and_players_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://player.daystar.tv/gone",
            404,
            "text/html",
            "".into(),
        ));
        fixture.exchanges.push(get(
            "https://player.daystar.tv/plain",
            200,
            "text/html",
            "<html><body>No player here</body></html>".into(),
        ));
        fixture.exchanges.push(get(
            "https://player.daystar.tv/unsigned",
            200,
            "text/html",
            page(),
        ));
        fixture.exchanges.push(get(
            PLAYER,
            200,
            "text/html",
            "<html><body><script>var player = 1;</script></body></html>".into(),
        ));
        let resolver = DaystarResolver::new(Http::replay(fixture));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://player.daystar.tv/gone").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(&Url::parse("https://player.daystar.tv/plain").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Malformed { detail, .. } if detail.contains("frames no player")),
            "{error}"
        );
        let error = resolver
            .resolve(&Url::parse("https://player.daystar.tv/unsigned").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Malformed { detail, .. } if detail.contains("no configuration script")),
            "{error}"
        );
    }
}
