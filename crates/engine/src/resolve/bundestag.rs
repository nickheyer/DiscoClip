//! Deutscher Bundestag parliament television (dbtg.tv, bundestag.de/mediathek): the
//! on-demand HLS instance of a video, the MP4 and MP3 files its share data lists, and the
//! title and description of the media library's overlay.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport,
    SubtitleFormat, SubtitleTrack, Variant, clean_title, fetch, fetch_ok, hls, navigation_headers,
    util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "bundestag";
const OVERLAY_URL: &str = "https://www.bundestag.de/mediathekoverlay";
const SHARE_URL: &str =
    "https://webtv.bundestag.de/player/macros/_x_s-144277506/shareData.json?contentId=";

static RE_DBTG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/[cf]vid/(\d+)").unwrap());
/// `/{id}_{codec}_{bitrate}kb_{channels}_{lang}_{n}.{ext}` of an audio file.
static RE_SHARE_AUDIO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/\d+_(\w+)_(\d+)kb_(\w+)_\w+_\d+\.(\w+)").unwrap());
/// `/{id}_{codec}_{width}_{height}_{bitrate}kb_{profile}_{lang}_{n}.{ext}` of a video file.
static RE_SHARE_VIDEO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/\d+_(\w+)_(\w+)_(\w+)_(\d+)kb_\w+_\w+_\d+\.(\w+)").unwrap());
static RE_H3: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<h3\b[^>]*>(.*?)</h3>").unwrap());
static RE_P: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?is)<p\b[^>]*>(.*?)</p>").unwrap());
static RE_SPAN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<span[^>]*>[^<]+</span>").unwrap());

/// The video id a `dbtg.tv/cvid/…`, `dbtg.tv/fvid/…` or `bundestag.de/mediathek?videoid=…`
/// link names.
pub fn video_id(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    match url.host_str()?.to_ascii_lowercase().as_str() {
        "dbtg.tv" => RE_DBTG.captures(url.path()).map(|c| c[1].to_string()),
        "www.bundestag.de" => {
            if !matches!(url.path(), "/mediathek" | "/mediathek/") {
                return None;
            }
            url.query_pairs()
                .find(|(k, _)| k == "videoid")
                .map(|(_, v)| v.into_owned())
                .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()))
        }
        _ => None,
    }
}

fn instance_url(id: &str) -> Url {
    Url::parse(&format!(
        "https://cldf-wzw-od.r53.cdn.tv1.eu/13014bundestagod/_definst_/13014bundestag/ondemand/3777parlamentsfernsehen/archiv/app144277506/145293313/{id}/{id}_playlist.smil/playlist.m3u8"
    ))
    .expect("valid")
}

fn video_codec(name: &str) -> VideoCodec {
    match name.to_ascii_lowercase().as_str() {
        "h264" | "avc1" => VideoCodec::H264,
        "h265" | "hevc" => VideoCodec::H265,
        "vp9" => VideoCodec::Vp9,
        "av1" => VideoCodec::Av1,
        other => VideoCodec::Other(other.to_string()),
    }
}

fn audio_codec(name: &str) -> AudioCodec {
    match name.to_ascii_lowercase().as_str() {
        "mp3" => AudioCodec::Mp3,
        "aac" | "mp4a" => AudioCodec::Aac,
        "opus" => AudioCodec::Opus,
        other => AudioCodec::Other(other.to_string()),
    }
}

/// The audio and video files the share data lists (its `audio…` and `download…` links),
/// and the subtitle file among them.
pub fn share_formats(share: &Value) -> (Vec<Variant>, Vec<SubtitleTrack>) {
    let mut variants = Vec::new();
    let mut subtitles = Vec::new();
    for (name, value) in share.as_object().into_iter().flatten() {
        let Some(url) = util::url_of(value, None) else {
            continue;
        };
        if name.starts_with("audio") {
            let mut variant = Variant::file(url.clone());
            variant.format_id = Some(name.clone());
            variant.audio_only = true;
            if let Some(caps) = RE_SHARE_AUDIO.captures(url.path()) {
                variant.audio = Some(audio_codec(&caps[1]));
                variant.bitrate = caps[2].parse::<u64>().ok().map(|kb| kb * 1000);
                variant.container = Container::from_extension(&caps[4])
                    .or_else(|| Some(Container::Other(caps[4].to_ascii_lowercase())));
                variant.label = Some(format!("{} {}", &caps[3], &caps[1]));
            }
            variants.push(variant);
        } else if name.starts_with("download") {
            if url.path().to_ascii_lowercase().ends_with(".srt") {
                subtitles.push(SubtitleTrack {
                    url,
                    language: "de".into(),
                    name: Some("Deutsch".into()),
                    format: SubtitleFormat::Srt,
                    auto: false,
                    headers: Vec::new(),
                });
                continue;
            }
            let mut variant = Variant::file(url.clone());
            variant.format_id = Some(name.clone());
            if let Some(caps) = RE_SHARE_VIDEO.captures(url.path()) {
                variant.video = Some(video_codec(&caps[1]));
                variant.width = caps[2].parse().ok();
                variant.height = caps[3].parse().ok();
                variant.bitrate = caps[4].parse::<u64>().ok().map(|kb| kb * 1000);
                variant.container = Container::from_extension(&caps[5])
                    .or_else(|| Some(Container::Other(caps[5].to_ascii_lowercase())));
                variant.label = variant.height.map(|h| format!("{h}p"));
            }
            variants.push(variant);
        }
    }
    (variants, subtitles)
}

/// The title and description of the media library overlay: its first heading without
/// the session badge, and its first paragraph.
pub fn overlay_metadata(html: &str) -> (Option<String>, Option<String>) {
    let title = util::search(&RE_H3, html)
        .map(|h| RE_SPAN.replace_all(&h, "").to_string())
        .map(|h| util::clean_html(&h))
        .and_then(|h| clean_title(&h));
    let description = util::search(&RE_P, html)
        .map(|p| util::clean_html(&p))
        .and_then(|p| clean_title(&p));
    (title, description)
}

pub struct BundestagResolver {
    http: Http,
}

impl BundestagResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for BundestagResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Bundestag",
            hosts: &["dbtg.tv", "bundestag.de"],
            features: &["videos", "audio"],
            formats: &["hls", "mp4", "mp3"],
            session: SessionSupport::None,
            examples: &[
                "https://dbtg.tv/cvid/7605304",
                "https://www.bundestag.de/mediathek?videoid=7602120&url=L21lZGlhdGhla292ZXJsYXk=&mod=mediathek",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        video_id(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let id = video_id(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.clone());
        match hls::expand(&self.http, &instance_url(&id), PLATFORM, BROWSER_UA, &[]).await {
            Ok(expanded) => {
                for mut variant in expanded.variants {
                    variant.format_id = Some(format!(
                        "instance-{}",
                        variant.label.clone().unwrap_or_default()
                    ));
                    resolved.variants.push(variant);
                }
                resolved.subtitles.extend(expanded.subtitles);
                resolved.duration = expanded.duration;
                resolved.live = expanded.live;
            }
            Err(ResolveError::NotFound(_)) => {
                return Err(ResolveError::unavailable(url, "Could not find video id"));
            }
            Err(error) => {
                tracing::warn!(platform = PLATFORM, %id, "error extracting hls formats: {error}");
            }
        }

        let share_url = Url::parse(&format!("{SHARE_URL}{id}")).expect("valid");
        let share = fetch_ok(&self.http, &share_url, PLATFORM, BROWSER_UA, &[], MAX_PAGE)
            .await?
            .json(url)?;
        if util::int(&share["status"]["code"]) == Some(1) {
            let (variants, subtitles) = share_formats(&share);
            resolved.variants.extend(variants);
            resolved.subtitles.extend(subtitles);
        } else {
            let message = share["status"]["message"]
                .as_str()
                .unwrap_or("Unknown Share API Error");
            tracing::warn!(platform = PLATFORM, %id, "Share API response: {message}");
        }
        if resolved.variants.is_empty() {
            return Err(ResolveError::unavailable(
                url,
                "Could not find suitable formats",
            ));
        }

        let overlay = util::with_query(
            &Url::parse(OVERLAY_URL).expect("valid"),
            &[("videoid", id.as_str()), ("view", "main")],
        );
        match fetch(
            &self.http,
            &overlay,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await
        {
            Ok(fetched) if fetched.status.is_success() => {
                let (title, description) = overlay_metadata(&fetched.text());
                resolved.title = title;
                resolved.description = description;
            }
            Ok(fetched) => {
                tracing::warn!(platform = PLATFORM, %id, "the metadata overlay answered HTTP {}", fetched.status);
            }
            Err(error) => {
                tracing::warn!(platform = PLATFORM, %id, "the metadata overlay failed: {error}");
            }
        }
        resolved.webpage_url = Some(url.clone());
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};
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

    const INSTANCE: &str = "https://cldf-wzw-od.r53.cdn.tv1.eu/13014bundestagod/_definst_/13014bundestag/ondemand/3777parlamentsfernsehen/archiv/app144277506/145293313/7605304/7605304_playlist.smil/playlist.m3u8";
    const CHUNKLIST: &str = "https://cldf-wzw-od.r53.cdn.tv1.eu/13014bundestagod/_definst_/13014bundestag/ondemand/3777parlamentsfernsehen/archiv/app144277506/145293313/7605304/7605304_playlist.smil/chunklist_b3130260.m3u8";

    fn share_json() -> String {
        json!({
            "audioUrlMono": "",
            "audioUrlStereo": "https://cldf-od.r53.cdn.tv1.eu/1000153copo/ondemand/app144277506/145293313/7605304/7605304_mp3_128kb_stereo_de_128.mp3?fdl=1",
            "downloadUrlHdPlus": "",
            "downloadUrlHigh": "https://cldf-od.r53.cdn.tv1.eu/1000153copo/ondemand/app144277506/145293313/7605304/7605304_h264_1280_720_3000kb_baseline_de_3000.mp4?fdl=1",
            "downloadUrl": "https://cldf-od.r53.cdn.tv1.eu/1000153copo/ondemand/app144277506/145293313/7605304/7605304_h264_1920_1080_5000kb_baseline_de_5000.mp4?fdl=1",
            "downloadUrlSRT": "https://cldf-od.r53.cdn.tv1.eu/1000153copo/ondemand/app144277506/145293313/7605304/7605304.srt",
            "rubricName": "Plenarsitzung",
            "top": true,
            "status": {"code": 1, "message": "ok"}
        })
        .to_string()
    }

    #[test]
    fn links_are_read() {
        let id = |s: &str| video_id(&Url::parse(s).unwrap());
        assert_eq!(id("https://dbtg.tv/cvid/7605304"), Some("7605304".into()));
        assert_eq!(id("http://dbtg.tv/fvid/3594346"), Some("3594346".into()));
        assert_eq!(
            id(
                "https://www.bundestag.de/mediathek?videoid=7602120&url=L21lZGlhdGhla292ZXJsYXk=&mod=mediathek"
            ),
            Some("7602120".into())
        );
        assert_eq!(
            id(
                "https://www.bundestag.de/mediathek?videoid=7604941#url=L21lZGlhdGhla292ZXJsYXk/dmlkZW9pZD03NjA0OTQx&mod=mediathek"
            ),
            Some("7604941".into())
        );
        assert_eq!(id("https://www.bundestag.de/mediathek?mod=mediathek"), None);
        assert_eq!(
            id("https://www.bundestag.de/parlament?videoid=7604941"),
            None
        );
        assert_eq!(id("https://dbtg.tv/xvid/7605304"), None);
    }

    #[test]
    fn share_files_are_read() {
        let share: Value = serde_json::from_str(&share_json()).unwrap();
        let (variants, subtitles) = share_formats(&share);
        assert_eq!(variants.len(), 3);
        let audio = variants.iter().find(|v| v.audio_only).unwrap();
        assert_eq!(audio.format_id.as_deref(), Some("audioUrlStereo"));
        assert_eq!(audio.audio, Some(AudioCodec::Mp3));
        assert_eq!(audio.bitrate, Some(128_000));
        let best = variants.iter().find(|v| v.height == Some(1080)).unwrap();
        assert_eq!(best.format_id.as_deref(), Some("downloadUrl"));
        assert_eq!(best.width, Some(1920));
        assert_eq!(best.bitrate, Some(5_000_000));
        assert_eq!(best.video, Some(VideoCodec::H264));
        assert_eq!(best.container, Some(Container::Mp4));
        assert_eq!(subtitles.len(), 1);
        assert_eq!(subtitles[0].format, SubtitleFormat::Srt);
    }

    #[test]
    fn overlay_titles_lose_their_badge() {
        let html = r#"<div><h3 class="bt-video__title"><span class="badge">Plenum</span>145. Sitzung vom 15.12.2023, TOP 24 Barrierefreiheit</h3><p>Barrierefreiheit &amp; Teilhabe</p></div>"#;
        let (title, description) = overlay_metadata(html);
        assert_eq!(
            title.as_deref(),
            Some("145. Sitzung vom 15.12.2023, TOP 24 Barrierefreiheit")
        );
        assert_eq!(description.as_deref(), Some("Barrierefreiheit & Teilhabe"));
    }

    #[tokio::test]
    async fn videos_resolve_with_instance_and_share_formats() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            INSTANCE,
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=3130260,RESOLUTION=1280x720\nchunklist_b3130260.m3u8\n".into(),
        ));
        fixture.exchanges.push(get(
            CHUNKLIST,
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n0.ts\n#EXT-X-ENDLIST\n".into(),
        ));
        fixture.exchanges.push(get(
            "https://webtv.bundestag.de/player/macros/_x_s-144277506/shareData.json?contentId=7605304",
            200,
            "application/json",
            share_json(),
        ));
        fixture.exchanges.push(get(
            "https://www.bundestag.de/mediathekoverlay?videoid=7605304&view=main",
            200,
            "text/html",
            "<div><h3><span>Plenum</span>145. Sitzung vom 15.12.2023, TOP 24 Barrierefreiheit</h3><p>Barrierefreiheit</p></div>".into(),
        ));
        let resolver = BundestagResolver::new(Http::replay(fixture));
        let url = Url::parse("https://dbtg.tv/cvid/7605304").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("7605304"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("145. Sitzung vom 15.12.2023, TOP 24 Barrierefreiheit")
        );
        assert_eq!(resolved.description.as_deref(), Some("Barrierefreiheit"));
        assert_eq!(resolved.variants.len(), 4);
        assert_eq!(
            resolved.variants[0].format_id.as_deref(),
            Some("instance-720p")
        );
        assert_eq!(resolved.duration, Some(std::time::Duration::from_secs(10)));
        assert_eq!(resolved.subtitles.len(), 1);
    }

    #[tokio::test]
    async fn missing_videos_and_failed_share_data_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture
            .exchanges
            .push(get(INSTANCE, 404, "text/plain", "not found".into()));
        let resolver = BundestagResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://dbtg.tv/cvid/7605304").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "Could not find video id"),
            "{error}"
        );

        let mut fixture = Fixture::new(PLATFORM, None);
        fixture
            .exchanges
            .push(get(INSTANCE, 500, "text/plain", "broken".into()));
        fixture.exchanges.push(get(
            "https://webtv.bundestag.de/player/macros/_x_s-144277506/shareData.json?contentId=7605304",
            200,
            "application/json",
            json!({"status": {"code": 0, "message": "no such content"}}).to_string(),
        ));
        let resolver = BundestagResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://dbtg.tv/cvid/7605304").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "Could not find suitable formats"),
            "{error}"
        );
    }
}
