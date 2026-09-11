use crate::config::Limits;
use crate::media::{LocalFile, MediaInfo, VideoCodec};
use crate::publish::Constraints;
use crate::resolve::{Variant, VariantKind};
use crate::transcode::{Target, TranscodeError};

#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    Passthrough,
    Transcode(Target),
}

/// Decides whether the downloaded file can be published as is.
pub fn plan(
    file: &LocalFile,
    info: &MediaInfo,
    constraints: &Constraints,
    limits: &Limits,
    target: Target,
) -> Result<Plan, TranscodeError> {
    let video = info.video.as_ref().ok_or(TranscodeError::NoVideo)?;
    let untouched = target.clip.is_none() && target.burn_subtitles.is_none();
    let fits = untouched
        && file.size <= constraints.max_bytes
        && constraints.accepts_container(&info.container)
        && constraints.accepts_video(&video.codec)
        && info
            .audio
            .as_ref()
            .is_none_or(|a| constraints.accepts_audio(&a.codec))
        && video.height <= limits.max_height
        && video.width <= limits.max_height * 16 / 9 + 2;
    if fits {
        return Ok(Plan::Passthrough);
    }
    Ok(Plan::Transcode(target))
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SelectError {
    #[error("{0} returned no media variants")]
    NoVariants(String),
    #[error("every variant is protected by {0} DRM")]
    Drm(String),
    #[error("{0} returned only audio")]
    AudioOnly(String),
}

fn kind_rank(kind: VariantKind) -> u8 {
    match kind {
        VariantKind::File => 7,
        VariantKind::Hls => 6,
        VariantKind::Dash => 5,
        VariantKind::Ism => 4,
        VariantKind::Browser => 3,
        VariantKind::Whep => 2,
        VariantKind::Rtsp => 1,
        VariantKind::Rtmp => 0,
    }
}

/// Codecs mainstream clients play as they are, so no transcode is needed, first.
fn codec_rank(codec: Option<&VideoCodec>) -> u8 {
    match codec {
        Some(VideoCodec::H264) => 4,
        Some(VideoCodec::Vp9) => 3,
        Some(VideoCodec::Av1) => 2,
        Some(VideoCodec::H265) => 1,
        Some(VideoCodec::Vp8) | Some(VideoCodec::Other(_)) | None => 0,
    }
}

/// The height counted for ranking: taller than the limit counts against, since it will be
/// scaled down anyway.
fn capped_height(height: Option<u32>, max_height: u32) -> u32 {
    let height = height.unwrap_or(0);
    if height > max_height {
        max_height.saturating_sub(height - max_height)
    } else {
        height
    }
}

/// Picks the variant to download: the largest picture no taller than the configured
/// maximum, then the codec least likely to need a transcode, then the highest bitrate,
/// preferring plain files over manifests. A video-only pick is paired with the best
/// audio-only variant; locked variants are never picked.
pub fn select_variant(
    variants: &[Variant],
    limits: &Limits,
    resolver: &str,
) -> Result<Variant, SelectError> {
    if variants.is_empty() {
        return Err(SelectError::NoVariants(resolver.to_string()));
    }
    let playable: Vec<&Variant> = variants.iter().filter(|v| v.is_playable()).collect();
    if playable.is_empty() {
        let system = variants
            .iter()
            .find_map(|v| v.drm.clone())
            .unwrap_or_else(|| "unknown".into());
        return Err(SelectError::Drm(system));
    }
    let allowed = |v: &Variant| v.size.is_none_or(|s| s <= limits.max_source_bytes);
    let score = |v: &Variant| {
        (
            capped_height(v.height, limits.max_height),
            codec_rank(v.video.as_ref()),
            v.bitrate.unwrap_or(0),
            kind_rank(v.kind),
        )
    };
    let with_video: Vec<&Variant> = playable.iter().copied().filter(|v| !v.audio_only).collect();
    if with_video.is_empty() {
        return Err(SelectError::AudioOnly(resolver.to_string()));
    }
    let chosen = with_video
        .iter()
        .copied()
        .filter(|v| allowed(v))
        .max_by_key(|v| score(v))
        .or_else(|| with_video.iter().copied().max_by_key(|v| score(v)))
        .expect("at least one video variant")
        .clone();
    if chosen.video_only && chosen.audio_url.is_none() {
        let audio = playable
            .iter()
            .copied()
            .filter(|v| v.audio_only && v.kind == chosen.kind)
            .max_by_key(|v| {
                (
                    v.language == chosen.language,
                    audio_rank(v),
                    v.bitrate.unwrap_or(0),
                )
            })
            .or_else(|| {
                playable
                    .iter()
                    .copied()
                    .filter(|v| v.audio_only)
                    .max_by_key(|v| (audio_rank(v), v.bitrate.unwrap_or(0)))
            });
        if let Some(audio) = audio {
            let mut paired = chosen;
            paired.audio_url = Some(audio.url.clone());
            paired.audio = audio.audio.clone();
            if paired.size.is_some() {
                paired.size = Some(paired.size.unwrap_or(0) + audio.size.unwrap_or(0));
            }
            return Ok(paired);
        }
    }
    Ok(chosen)
}

fn audio_rank(v: &Variant) -> u8 {
    match v.audio {
        Some(crate::media::AudioCodec::Aac) => 3,
        Some(crate::media::AudioCodec::Opus) => 2,
        Some(crate::media::AudioCodec::Mp3) => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::AudioCodec;
    use url::Url;

    fn variant(url: &str, kind: VariantKind, height: u32, bitrate: u64) -> Variant {
        let mut v = Variant::new(Url::parse(url).unwrap(), kind);
        v.height = Some(height);
        v.bitrate = Some(bitrate);
        v
    }

    fn limits() -> Limits {
        Limits {
            max_source_bytes: 1000,
            max_duration_secs: None,
            max_height: 1080,
        }
    }

    #[test]
    fn picks_the_largest_allowed_picture_then_codec_then_bitrate() {
        let mut best = variant("https://h/1080.mp4", VariantKind::File, 1080, 4000);
        best.video = Some(VideoCodec::H264);
        let mut hevc = variant("https://h/1080h.mp4", VariantKind::File, 1080, 9000);
        hevc.video = Some(VideoCodec::H265);
        let tall = variant("https://h/2160.mp4", VariantKind::File, 2160, 9000);
        let small = variant("https://h/720.mp4", VariantKind::File, 720, 9000);
        let chosen =
            select_variant(&[small.clone(), hevc, tall, best.clone()], &limits(), "x").unwrap();
        assert_eq!(chosen.url, best.url);
        let mut big = variant("https://h/big.mp4", VariantKind::File, 1080, 9000);
        big.size = Some(5000);
        let chosen = select_variant(&[big, small.clone()], &limits(), "x").unwrap();
        assert_eq!(chosen.url, small.url);
        let hls = variant("https://h/v.m3u8", VariantKind::Hls, 720, 9000);
        let chosen = select_variant(&[hls, small.clone()], &limits(), "x").unwrap();
        assert_eq!(chosen.kind, VariantKind::File);
    }

    #[test]
    fn video_only_variants_are_paired_with_audio() {
        let mut video = variant("https://h/v.webm", VariantKind::File, 1080, 3000);
        video.video_only = true;
        video.video = Some(VideoCodec::Vp9);
        video.size = Some(300);
        let mut opus = variant("https://h/a.webm", VariantKind::File, 0, 128);
        opus.audio_only = true;
        opus.audio = Some(AudioCodec::Opus);
        opus.size = Some(50);
        let mut aac = variant("https://h/a.m4a", VariantKind::File, 0, 128);
        aac.audio_only = true;
        aac.audio = Some(AudioCodec::Aac);
        aac.size = Some(60);
        let chosen =
            select_variant(&[opus.clone(), video.clone(), aac.clone()], &limits(), "x").unwrap();
        assert_eq!(chosen.url, video.url);
        assert_eq!(chosen.audio_url.unwrap(), aac.url);
        assert_eq!(chosen.audio, Some(AudioCodec::Aac));
        assert_eq!(chosen.size, Some(360));
        assert_eq!(
            select_variant(&[opus], &limits(), "x").unwrap_err(),
            SelectError::AudioOnly("x".into())
        );
    }

    #[test]
    fn locked_and_empty_lists_are_refused() {
        assert_eq!(
            select_variant(&[], &limits(), "yt").unwrap_err(),
            SelectError::NoVariants("yt".into())
        );
        let mut locked = variant("https://h/v.mpd", VariantKind::Dash, 1080, 1);
        locked.drm = Some("widevine".into());
        assert_eq!(
            select_variant(&[locked.clone()], &limits(), "yt").unwrap_err(),
            SelectError::Drm("widevine".into())
        );
        let open = variant("https://h/v.mp4", VariantKind::File, 360, 1);
        let chosen = select_variant(&[locked, open.clone()], &limits(), "yt").unwrap();
        assert_eq!(chosen.url, open.url);
    }
}
