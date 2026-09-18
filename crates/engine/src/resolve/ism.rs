//! Expands Microsoft Smooth Streaming manifests (`.ism/Manifest`) into variants: one per
//! video quality level, with the manifest's audio, its length, whether it is live, and
//! the PlayReady protection it declares when it does.

use std::time::Duration;

use url::Url;

use super::util::xml;
use super::{ResolveError, Variant, VariantKind, check_status};
use crate::http::Http;
use crate::media::{AudioCodec, Container, VideoCodec};

const MAX_MANIFEST: usize = 8 * 1024 * 1024;

pub struct Expanded {
    pub variants: Vec<Variant>,
    pub duration: Option<Duration>,
    pub live: bool,
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
    let body = response.bytes(MAX_MANIFEST).await?;
    let text = String::from_utf8_lossy(&body);
    expand_manifest(url, &text, headers)
}

fn video_codec(four_cc: &str) -> Option<VideoCodec> {
    match four_cc.to_ascii_uppercase().as_str() {
        "H264" | "AVC1" | "X264" | "DAVC" => Some(VideoCodec::H264),
        "HEVC" | "HVC1" | "HEV1" | "H265" => Some(VideoCodec::H265),
        "VP09" | "VP90" => Some(VideoCodec::Vp9),
        "AV01" => Some(VideoCodec::Av1),
        "" => None,
        other => Some(VideoCodec::Other(other.to_ascii_lowercase())),
    }
}

fn audio_codec(four_cc: &str, audio_tag: Option<&str>) -> Option<AudioCodec> {
    match four_cc.to_ascii_uppercase().as_str() {
        "AACL" | "AACH" | "AAC" => Some(AudioCodec::Aac),
        "EC-3" | "EC3" => Some(AudioCodec::Other("ec-3".into())),
        "AC-3" | "AC3" | "DD" => Some(AudioCodec::Other("ac-3".into())),
        "MP3" => Some(AudioCodec::Mp3),
        "" => match audio_tag {
            Some("255") => Some(AudioCodec::Aac),
            Some("85") => Some(AudioCodec::Mp3),
            _ => None,
        },
        other => Some(AudioCodec::Other(other.to_ascii_lowercase())),
    }
}

/// [`expand`] for a manifest already read.
pub fn expand_manifest(
    url: &Url,
    text: &str,
    headers: &[(String, String)],
) -> Result<Expanded, ResolveError> {
    let document = xml(text)
        .ok_or_else(|| ResolveError::malformed(url, "invalid Smooth Streaming manifest"))?;
    let root = document.root_element();
    if root.tag_name().name() != "SmoothStreamingMedia" {
        return Err(ResolveError::malformed(
            url,
            format!(
                "not a Smooth Streaming manifest: <{}>",
                root.tag_name().name()
            ),
        ));
    }
    let timescale: f64 = root
        .attribute("TimeScale")
        .and_then(|t| t.parse().ok())
        .unwrap_or(10_000_000.0);
    let duration = root
        .attribute("Duration")
        .and_then(|d| d.parse::<f64>().ok())
        .filter(|d| *d > 0.0)
        .map(|ticks| Duration::from_secs_f64(ticks / timescale));
    let live = root
        .attribute("IsLive")
        .is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let drm = root
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "Protection")
        .map(|protection| {
            protection
                .children()
                .find(|n| n.is_element() && n.tag_name().name() == "ProtectionHeader")
                .and_then(|h| h.attribute("SystemID"))
                .map(|id| match id.to_ascii_lowercase().as_str() {
                    "9a04f079-9840-4286-ab92-e65be0885f95" => "playready".to_string(),
                    "edef8ba9-79d6-4ace-a3c8-27dcd51d21ed" => "widevine".to_string(),
                    other => other.to_string(),
                })
                .unwrap_or_else(|| "playready".to_string())
        });
    let streams: Vec<roxmltree::Node> = root
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "StreamIndex")
        .collect();
    let audio = streams
        .iter()
        .filter(|s| {
            s.attribute("Type")
                .is_some_and(|t| t.eq_ignore_ascii_case("audio"))
        })
        .flat_map(|s| {
            let language = s.attribute("Language").map(str::to_string);
            s.children()
                .filter(|n| n.is_element() && n.tag_name().name() == "QualityLevel")
                .map(move |q| (q, language.clone()))
        })
        .max_by_key(|(q, _)| {
            q.attribute("Bitrate")
                .and_then(|b| b.parse::<u64>().ok())
                .unwrap_or(0)
        });
    let audio_codec_found = audio.as_ref().and_then(|(q, _)| {
        audio_codec(q.attribute("FourCC").unwrap_or(""), q.attribute("AudioTag"))
    });
    let audio_bitrate = audio
        .as_ref()
        .and_then(|(q, _)| q.attribute("Bitrate").and_then(|b| b.parse::<u64>().ok()));
    let audio_language = audio.as_ref().and_then(|(_, language)| language.clone());
    let mut variants = Vec::new();
    for stream in streams.iter().filter(|s| {
        s.attribute("Type")
            .is_some_and(|t| t.eq_ignore_ascii_case("video"))
    }) {
        for (index, level) in stream
            .children()
            .filter(|n| n.is_element() && n.tag_name().name() == "QualityLevel")
            .enumerate()
        {
            let bitrate = level
                .attribute("Bitrate")
                .and_then(|b| b.parse::<u64>().ok());
            let mut v = Variant::new(url.clone(), VariantKind::Ism);
            v.format_id = Some(match level.attribute("Index").or(Some("")) {
                Some(id) if !id.is_empty() => format!("video-{id}"),
                _ => format!("video-{index}"),
            });
            v.width = level
                .attribute("MaxWidth")
                .or_else(|| level.attribute("Width"))
                .and_then(|w| w.parse().ok());
            v.height = level
                .attribute("MaxHeight")
                .or_else(|| level.attribute("Height"))
                .and_then(|h| h.parse().ok());
            v.bitrate = bitrate.map(|b| b + audio_bitrate.unwrap_or(0));
            v.video = video_codec(level.attribute("FourCC").unwrap_or(""));
            v.audio = audio_codec_found.clone();
            v.container = Some(Container::Mp4);
            v.language = audio_language.clone();
            v.duration = duration;
            v.live = live;
            v.drm = drm.clone();
            v.headers = headers.to_vec();
            v.label = v.height.map(|h| format!("{h}p"));
            variants.push(v);
        }
    }
    if variants.is_empty()
        && let Some((level, language)) = audio
    {
        let mut v = Variant::new(url.clone(), VariantKind::Ism);
        v.format_id = Some("audio".into());
        v.audio_only = true;
        v.audio = audio_codec_found.clone();
        v.bitrate = level.attribute("Bitrate").and_then(|b| b.parse().ok());
        v.language = language;
        v.duration = duration;
        v.live = live;
        v.drm = drm.clone();
        v.headers = headers.to_vec();
        variants.push(v);
    }
    if variants.is_empty() {
        return Err(ResolveError::NotFound(url.clone()));
    }
    Ok(Expanded {
        variants,
        duration,
        live,
        drm,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<SmoothStreamingMedia MajorVersion="2" MinorVersion="2" Duration="1200000000" TimeScale="10000000">
  <StreamIndex Type="video" Chunks="12" QualityLevels="2" Url="QualityLevels({bitrate})/Fragments(video={start time})">
    <QualityLevel Index="0" Bitrate="2962000" FourCC="H264" MaxWidth="1280" MaxHeight="720" CodecPrivateData="00"/>
    <QualityLevel Index="1" Bitrate="1000000" FourCC="H264" MaxWidth="640" MaxHeight="360" CodecPrivateData="00"/>
    <c t="0" d="20000000"/>
  </StreamIndex>
  <StreamIndex Type="audio" Language="eng" Chunks="12" QualityLevels="1" Url="QualityLevels({bitrate})/Fragments(audio={start time})">
    <QualityLevel Index="0" Bitrate="128000" FourCC="AACL" SamplingRate="48000" Channels="2" AudioTag="255"/>
    <c t="0" d="20000000"/>
  </StreamIndex>
</SmoothStreamingMedia>"#;

    #[test]
    fn quality_levels_become_variants() {
        let url = Url::parse("https://cdn.test/v.ism/Manifest").unwrap();
        let expanded = expand_manifest(&url, MANIFEST, &[]).unwrap();
        assert_eq!(expanded.duration, Some(Duration::from_secs(120)));
        assert!(!expanded.live);
        assert!(expanded.drm.is_none());
        assert_eq!(expanded.variants.len(), 2);
        let best = &expanded.variants[0];
        assert_eq!(best.kind, VariantKind::Ism);
        assert_eq!(best.format_id.as_deref(), Some("video-0"));
        assert_eq!((best.width, best.height), (Some(1280), Some(720)));
        assert_eq!(best.bitrate, Some(3_090_000));
        assert_eq!(best.video, Some(VideoCodec::H264));
        assert_eq!(best.audio, Some(AudioCodec::Aac));
        assert_eq!(best.language.as_deref(), Some("eng"));
        assert_eq!(best.label.as_deref(), Some("720p"));
    }

    #[test]
    fn protected_and_live_manifests_say_so() {
        let locked = MANIFEST.replace(
            r#"TimeScale="10000000">"#,
            r#"TimeScale="10000000" IsLive="TRUE"><Protection><ProtectionHeader SystemID="9A04F079-9840-4286-AB92-E65BE0885F95">AAAA</ProtectionHeader></Protection>"#,
        );
        let url = Url::parse("https://cdn.test/v.ism/Manifest").unwrap();
        let expanded = expand_manifest(&url, &locked, &[]).unwrap();
        assert_eq!(expanded.drm.as_deref(), Some("playready"));
        assert!(expanded.live);
        assert!(expanded.variants.iter().all(|v| v.drm.is_some() && v.live));
        assert!(matches!(
            expand_manifest(&url, "<MPD/>", &[]),
            Err(ResolveError::Malformed { .. })
        ));
        assert!(matches!(
            expand_manifest(&url, "<SmoothStreamingMedia/>", &[]),
            Err(ResolveError::NotFound(_))
        ));
    }
}
