//! Expands MPEG-DASH presentations into variants: one per video representation, with
//! the audio the presentation carries, the subtitle tracks it lists as whole files, its
//! length, whether it is live, and the DRM system it is locked with when it is.

use std::time::Duration;

use dash_mpd::{AdaptationSet, MPD, Representation};
use url::Url;

use super::{
    ResolveError, SubtitleFormat, SubtitleTrack, Variant, VariantKind, check_status, parse_codecs,
    parse_rate,
};
use crate::http::Http;
use crate::media::{AudioCodec, Container, VideoCodec};

const MAX_MANIFEST: usize = 16 * 1024 * 1024;

pub struct Expanded {
    pub variants: Vec<Variant>,
    pub subtitles: Vec<SubtitleTrack>,
    pub duration: Option<Duration>,
    pub live: bool,
    /// The DRM system every representation is locked with, when they all are.
    pub drm: Option<String>,
}

/// Reads the manifest at `url` as `platform` and expands it.
pub async fn expand(
    http: &Http,
    url: &Url,
    platform: &str,
    user_agent: &str,
    headers: &[(String, String)],
) -> Result<Expanded, ResolveError> {
    let response = http
        .get(url.clone())
        .platform(platform)
        .user_agent(user_agent)
        .headers(headers)
        .send()
        .await?;
    check_status(&response, url)?;
    let base = response.url.clone();
    let body = response.bytes(MAX_MANIFEST).await?;
    let text = String::from_utf8_lossy(&body);
    expand_manifest(url, &base, &text, headers)
}

/// The DRM system a `ContentProtection` scheme names, when it names one.
pub fn drm_system(scheme_id_uri: &str, value: Option<&str>) -> Option<String> {
    let scheme = scheme_id_uri.trim().to_ascii_lowercase();
    if scheme == "urn:mpeg:dash:mp4protection:2011" {
        // Only says the content is encrypted; the system comes from another element,
        // and when none does the content is at least CENC locked.
        return value
            .map(|v| format!("cenc ({})", v.trim()))
            .or(Some("cenc".into()));
    }
    let uuid = scheme.strip_prefix("urn:uuid:").unwrap_or(&scheme);
    Some(
        match uuid {
            "edef8ba9-79d6-4ace-a3c8-27dcd51d21ed" => "widevine",
            "9a04f079-9840-4286-ab92-e65be0885f95" => "playready",
            "94ce86fb-07ff-4f43-adb8-93d2fa968ca2" => "fairplay",
            "e2719d58-a985-b3c9-781a-b030af78d30e" | "1077efec-c0b2-4d02-ace3-3c1e52e2fb4b" => {
                "clearkey"
            }
            "5e629af5-38da-4063-8977-97ffbd9902d4" => "marlin",
            "adb41c24-2dbf-4a6d-958b-4457c0d27b95" => "nagra",
            "a68129d3-575b-4f1a-9cba-3223846cf7c3" => "videoguard",
            "9a27dd82-fde2-4725-8cbc-4234aa06ec09" => "verimatrix",
            "3d5e6d35-9b9a-41e8-b843-dd3c6e72c42c" => "wiseplay",
            other => other,
        }
        .to_string(),
    )
}

fn protection_of(set: &AdaptationSet, representation: &Representation) -> Option<String> {
    let protections = set
        .ContentProtection
        .iter()
        .chain(representation.ContentProtection.iter());
    let mut fallback = None;
    for protection in protections {
        let system = drm_system(&protection.schemeIdUri, protection.value.as_deref())?;
        if system.starts_with("cenc") {
            fallback = Some(system);
        } else {
            return Some(system);
        }
    }
    fallback
}

fn container_of(mime: Option<&str>) -> Option<Container> {
    let mime = mime?.to_ascii_lowercase();
    match mime.as_str() {
        "video/mp4" | "audio/mp4" | "application/mp4" => Some(Container::Mp4),
        "video/webm" | "audio/webm" => Some(Container::Webm),
        "video/mp2t" => Some(Container::Ts),
        _ => None,
    }
}

fn duration_of(mpd: &MPD) -> Option<Duration> {
    if let Some(duration) = mpd.mediaPresentationDuration {
        return Some(duration);
    }
    let sum: Duration = mpd.periods.iter().filter_map(|p| p.duration).sum();
    (sum > Duration::ZERO).then_some(sum)
}

/// [`expand`] for a manifest already read: `text` came from `base`, and `url` names the
/// manifest in errors and in the variants.
pub fn expand_manifest(
    url: &Url,
    base: &Url,
    text: &str,
    headers: &[(String, String)],
) -> Result<Expanded, ResolveError> {
    let mpd = dash_mpd::parse(text)
        .map_err(|e| ResolveError::malformed(url, format!("invalid MPD: {e}")))?;
    let live = mpd
        .mpdtype
        .as_deref()
        .is_some_and(|t| t.eq_ignore_ascii_case("dynamic"));
    let duration = duration_of(&mpd);
    let mut variants = Vec::new();
    let mut subtitles = Vec::new();
    let mut locked = 0usize;
    let mut systems: Vec<String> = Vec::new();
    for (period_index, period) in mpd.periods.iter().enumerate() {
        let audio_sets: Vec<&AdaptationSet> = period
            .adaptations
            .iter()
            .filter(dash_mpd::is_audio_adaptation)
            .collect();
        let best_audio = audio_sets
            .iter()
            .flat_map(|set| set.representations.iter().map(move |r| (set, r)))
            .max_by_key(|(_, r)| r.bandwidth.unwrap_or(0));
        let audio_codec = best_audio.and_then(|(set, rep)| {
            let codecs = rep.codecs.as_deref().or(set.codecs.as_deref());
            parse_codecs(codecs).1
        });
        let audio_bitrate = best_audio.and_then(|(_, rep)| rep.bandwidth);
        let audio_language =
            best_audio.and_then(|(set, rep)| rep.lang.clone().or_else(|| set.lang.clone()));
        for set in period
            .adaptations
            .iter()
            .filter(dash_mpd::is_video_adaptation)
        {
            for representation in &set.representations {
                let codecs = representation.codecs.as_deref().or(set.codecs.as_deref());
                let (video, audio_in_video) = parse_codecs(codecs);
                let mut v = Variant::new(url.clone(), VariantKind::Dash);
                v.format_id = representation.id.clone().map(|id| {
                    if period_index == 0 {
                        id
                    } else {
                        format!("p{period_index}-{id}")
                    }
                });
                v.width = representation.width.or(set.width).map(|w| w as u32);
                v.height = representation.height.or(set.height).map(|h| h as u32);
                v.fps = representation
                    .frameRate
                    .as_deref()
                    .or(set.frameRate.as_deref())
                    .and_then(parse_rate);
                v.bitrate = representation.bandwidth.map(|b| {
                    b + if audio_in_video.is_some() {
                        0
                    } else {
                        audio_bitrate.unwrap_or(0)
                    }
                });
                v.codecs = codecs.map(str::to_string);
                v.video = video.or_else(|| {
                    representation
                        .mimeType
                        .as_deref()
                        .or(set.mimeType.as_deref())
                        .filter(|m| m.contains("webm"))
                        .map(|_| VideoCodec::Vp9)
                });
                v.audio = audio_in_video
                    .or(audio_codec.clone())
                    .or_else(|| (!audio_sets.is_empty()).then_some(AudioCodec::Aac));
                v.container = container_of(
                    representation
                        .mimeType
                        .as_deref()
                        .or(set.mimeType.as_deref()),
                );
                v.language = audio_language.clone();
                v.duration = duration;
                v.live = live;
                v.headers = headers.to_vec();
                v.label = v.height.map(|h| match v.fps {
                    Some(fps) if fps > 30.5 => format!("{h}p{}", fps.round()),
                    _ => format!("{h}p"),
                });
                v.drm = protection_of(set, representation);
                if let Some(system) = &v.drm {
                    locked += 1;
                    if !systems.contains(system) {
                        systems.push(system.clone());
                    }
                }
                variants.push(v);
            }
        }
        // Audio-only presentations: one variant per audio representation.
        if !period
            .adaptations
            .iter()
            .any(|set| dash_mpd::is_video_adaptation(&set))
        {
            for set in &audio_sets {
                for representation in &set.representations {
                    let codecs = representation.codecs.as_deref().or(set.codecs.as_deref());
                    let mut v = Variant::new(url.clone(), VariantKind::Dash);
                    v.format_id = representation.id.clone();
                    v.bitrate = representation.bandwidth;
                    v.codecs = codecs.map(str::to_string);
                    v.audio = parse_codecs(codecs).1;
                    v.audio_only = true;
                    v.language = representation.lang.clone().or_else(|| set.lang.clone());
                    v.duration = duration;
                    v.live = live;
                    v.headers = headers.to_vec();
                    v.drm = protection_of(set, representation);
                    if let Some(system) = &v.drm {
                        locked += 1;
                        if !systems.contains(system) {
                            systems.push(system.clone());
                        }
                    }
                    variants.push(v);
                }
            }
        }
        for set in period
            .adaptations
            .iter()
            .filter(dash_mpd::is_subtitle_adaptation)
        {
            for representation in &set.representations {
                let mime = representation
                    .mimeType
                    .as_deref()
                    .or(set.mimeType.as_deref())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let codecs = representation
                    .codecs
                    .as_deref()
                    .or(set.codecs.as_deref())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let format = if mime.contains("vtt") || codecs.contains("wvtt") {
                    SubtitleFormat::Vtt
                } else if mime.contains("ttml") || codecs.contains("stpp") || mime.contains("xml") {
                    SubtitleFormat::Ttml
                } else {
                    continue;
                };
                // Only whole-file tracks are fetched; segmented text streams need a
                // segment fetcher and are the DASH downloader's business.
                let Some(file) = representation
                    .BaseURL
                    .first()
                    .map(|b| b.base.clone())
                    .filter(|_| representation.SegmentTemplate.is_none())
                else {
                    continue;
                };
                let Ok(track_url) = base.join(&file) else {
                    continue;
                };
                subtitles.push(SubtitleTrack {
                    url: track_url,
                    language: representation
                        .lang
                        .clone()
                        .or_else(|| set.lang.clone())
                        .unwrap_or_else(|| "und".into()),
                    name: set.Label.first().map(|l| l.content.clone()),
                    format,
                    auto: false,
                    headers: headers.to_vec(),
                });
            }
        }
    }
    if variants.is_empty() {
        return Err(ResolveError::NotFound(url.clone()));
    }
    let drm = (locked == variants.len()).then(|| systems.join(", "));
    Ok(Expanded {
        variants,
        subtitles,
        duration,
        live,
        drm,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

    const MPD: &str = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT1M30.5S" profiles="urn:mpeg:dash:profile:isoff-on-demand:2011">
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video" frameRate="30000/1001">
      <Representation id="v720" bandwidth="1500000" width="1280" height="720" codecs="avc1.64001f"/>
      <Representation id="v1080" bandwidth="4000000" width="1920" height="1080" codecs="avc1.640028" frameRate="60"/>
    </AdaptationSet>
    <AdaptationSet mimeType="audio/mp4" contentType="audio" lang="en">
      <Representation id="a128" bandwidth="128000" codecs="mp4a.40.2"/>
      <Representation id="a64" bandwidth="64000" codecs="mp4a.40.2"/>
    </AdaptationSet>
    <AdaptationSet mimeType="text/vtt" contentType="text" lang="de">
      <Label>Deutsch</Label>
      <Representation id="s1" bandwidth="1000"><BaseURL>subs/de.vtt</BaseURL></Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    #[test]
    fn representations_become_variants_with_audio_and_subtitles() {
        let url = Url::parse("https://cdn.test/v/manifest.mpd").unwrap();
        let expanded = expand_manifest(
            &url,
            &url,
            MPD,
            &[("referer".into(), "https://s.test/".into())],
        )
        .unwrap();
        assert_eq!(expanded.variants.len(), 2);
        assert_eq!(expanded.duration, Some(Duration::from_secs_f64(90.5)));
        assert!(!expanded.live);
        assert!(expanded.drm.is_none());
        let best = &expanded.variants[1];
        assert_eq!(best.format_id.as_deref(), Some("v1080"));
        assert_eq!(best.height, Some(1080));
        assert_eq!(best.fps, Some(60.0));
        assert_eq!(best.bitrate, Some(4_128_000));
        assert_eq!(best.video, Some(VideoCodec::H264));
        assert_eq!(best.audio, Some(AudioCodec::Aac));
        assert_eq!(best.container, Some(Container::Mp4));
        assert_eq!(best.language.as_deref(), Some("en"));
        assert_eq!(best.label.as_deref(), Some("1080p60"));
        assert_eq!(best.headers[0].0, "referer");
        assert!((expanded.variants[0].fps.unwrap() - 29.97).abs() < 0.01);
        assert_eq!(expanded.subtitles.len(), 1);
        assert_eq!(
            expanded.subtitles[0].url.as_str(),
            "https://cdn.test/v/subs/de.vtt"
        );
        assert_eq!(expanded.subtitles[0].name.as_deref(), Some("Deutsch"));
        assert_eq!(expanded.subtitles[0].format, SubtitleFormat::Vtt);
    }

    #[test]
    fn locked_and_live_presentations_are_marked() {
        let locked = MPD
            .replace(
                r#"<AdaptationSet mimeType="video/mp4" contentType="video" frameRate="30000/1001">"#,
                r#"<AdaptationSet mimeType="video/mp4" contentType="video" frameRate="30000/1001">
      <ContentProtection schemeIdUri="urn:mpeg:dash:mp4protection:2011" value="cenc"/>
      <ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"/>"#,
            )
            .replace(r#"type="static""#, r#"type="dynamic""#);
        let url = Url::parse("https://cdn.test/v/manifest.mpd").unwrap();
        let expanded = expand_manifest(&url, &url, &locked, &[]).unwrap();
        assert_eq!(expanded.drm.as_deref(), Some("widevine"));
        assert!(expanded.live);
        assert!(
            expanded
                .variants
                .iter()
                .all(|v| v.drm.as_deref() == Some("widevine") && v.live)
        );
        assert_eq!(
            drm_system("urn:uuid:9a04f079-9840-4286-ab92-e65be0885f95", None).as_deref(),
            Some("playready")
        );
        assert_eq!(
            drm_system("urn:mpeg:dash:mp4protection:2011", Some("cbcs")).as_deref(),
            Some("cenc (cbcs)")
        );
        let broken = expand_manifest(&url, &url, "<MPD", &[]);
        assert!(matches!(broken, Err(ResolveError::Malformed { .. })));
    }

    #[tokio::test]
    async fn manifests_are_fetched_as_the_platform() {
        let mut fixture = Fixture::new("dash", None);
        fixture.exchanges.push(Exchange {
            request: RecordedRequest {
                method: "GET".into(),
                url: "https://cdn.test/v/manifest.mpd".into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status: 200,
                url: "https://cdn.test/v/manifest.mpd".into(),
                headers: vec![("content-type".into(), "application/dash+xml".into())],
                body: RecordedBody::Text(MPD.into()),
                truncated: false,
            },
        });
        let http = Http::replay(fixture);
        let expanded = expand(
            &http,
            &Url::parse("https://cdn.test/v/manifest.mpd").unwrap(),
            "web",
            "ua",
            &[],
        )
        .await
        .unwrap();
        assert_eq!(expanded.variants.len(), 2);
        assert_eq!(expanded.variants[0].kind, VariantKind::Dash);
    }
}
