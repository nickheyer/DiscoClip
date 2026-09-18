use crate::config::Limits;
use crate::media::{LocalFile, MediaInfo, MediaKind, VideoCodec};
use crate::publish::Constraints;
use crate::resolve::{Variant, VariantKind};
use crate::transcode::{Target, TranscodeError};

#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    Passthrough,
    Transcode(Target),
}

/// Decides whether the downloaded file can be published as is, for the kind of media
/// `target` names: a video passes when its container, codecs, size and picture fit the
/// destination; audio alone when its container and codec are ones the destination plays
/// and it fits; an image when its format is one the destination shows and it fits; any
/// other file when the destination takes files and it fits, since nothing else can be
/// done with it. `info` is what ffprobe found, and is required for anything but a file.
pub fn plan(
    file: &LocalFile,
    info: Option<&MediaInfo>,
    constraints: &Constraints,
    limits: &Limits,
    target: Target,
) -> Result<Plan, TranscodeError> {
    match &target {
        Target::Video(video_target) => {
            let info = info.ok_or(TranscodeError::NoVideo)?;
            let video = info.video.as_ref().ok_or(TranscodeError::NoVideo)?;
            if info.kind != MediaKind::Video {
                return Err(TranscodeError::NoVideo);
            }
            let untouched = video_target.clip.is_none() && video_target.burn_subtitles.is_none();
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
        Target::Audio(audio_target) => {
            let info = info.ok_or(TranscodeError::NoAudio)?;
            let audio = info.audio.as_ref().ok_or(TranscodeError::NoAudio)?;
            if !constraints.accepts(MediaKind::Audio) {
                return Err(TranscodeError::NotAccepted(MediaKind::Audio));
            }
            let fits = audio_target.clip.is_none()
                && info.kind == MediaKind::Audio
                && file.size <= constraints.max_bytes
                && constraints.accepts_audio_file(&info.container, Some(&audio.codec));
            if fits {
                return Ok(Plan::Passthrough);
            }
            Ok(Plan::Transcode(target))
        }
        Target::Image(_) => {
            let info = info.ok_or(TranscodeError::NoPicture)?;
            if info.kind != MediaKind::Image || info.video.is_none() {
                return Err(TranscodeError::NoPicture);
            }
            if !constraints.accepts(MediaKind::Image) {
                return Err(TranscodeError::NotAccepted(MediaKind::Image));
            }
            let fits =
                file.size <= constraints.max_bytes && constraints.accepts_image(&info.container);
            if fits {
                return Ok(Plan::Passthrough);
            }
            Ok(Plan::Transcode(target))
        }
        Target::File { max_bytes } => {
            if !constraints.accepts(MediaKind::File) {
                return Err(TranscodeError::NotAccepted(MediaKind::File));
            }
            let max_bytes = (*max_bytes).min(constraints.max_bytes);
            if file.size > max_bytes {
                return Err(TranscodeError::CannotShrink {
                    size: file.size,
                    max_bytes,
                });
            }
            Ok(Plan::Passthrough)
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SelectError {
    #[error("{0} returned no media variants")]
    NoVariants(String),
    #[error("every variant is protected by {0} DRM")]
    Drm(String),
    #[error("{0} returned only audio")]
    AudioOnly(String),
    #[error("{0} returned no variant with sound")]
    NoAudio(String),
    #[error("{0} returned no {1} variant")]
    NoFile(String, MediaKind),
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

/// Picks the variant to download for the kind of media the link is. For a video: the
/// largest picture no taller than the configured maximum, then the codec least likely to
/// need a transcode, then the highest bitrate, preferring plain files over manifests; a
/// video-only pick is paired with the best audio-only variant. For audio: the best
/// audio-only variant by codec and bitrate, or failing one the variant with sound whose
/// picture is smallest, since only the sound is kept. For an image or a file: the
/// largest under the byte limit. Locked variants are never picked, and a variant over the
/// byte limit only when nothing under it exists.
pub fn select_variant(
    variants: &[Variant],
    limits: &Limits,
    resolver: &str,
    media: MediaKind,
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
    match media {
        MediaKind::Video => select_video(&playable, limits, resolver, allowed),
        MediaKind::Audio => {
            let with_sound: Vec<&Variant> =
                playable.iter().copied().filter(|v| !v.video_only).collect();
            if with_sound.is_empty() {
                return Err(SelectError::NoAudio(resolver.to_string()));
            }
            let score = |v: &Variant| {
                (
                    allowed(v),
                    v.audio_only,
                    audio_rank(v),
                    v.bitrate.unwrap_or(0),
                    u32::MAX - v.height.unwrap_or(0),
                    kind_rank(v.kind),
                )
            };
            Ok(with_sound
                .iter()
                .copied()
                .max_by_key(|v| score(v))
                .expect("at least one variant with sound")
                .clone())
        }
        MediaKind::Image | MediaKind::File => {
            let files: Vec<&Variant> = playable
                .iter()
                .copied()
                .filter(|v| !v.audio_only && !v.video_only)
                .collect();
            if files.is_empty() {
                return Err(SelectError::NoFile(resolver.to_string(), media));
            }
            let score = |v: &Variant| {
                (
                    allowed(v),
                    v.height.unwrap_or(0),
                    v.size.unwrap_or(0),
                    kind_rank(v.kind),
                )
            };
            Ok(files
                .iter()
                .copied()
                .max_by_key(|v| score(v))
                .expect("at least one file variant")
                .clone())
        }
    }
}

fn select_video(
    playable: &[&Variant],
    limits: &Limits,
    resolver: &str,
    allowed: impl Fn(&Variant) -> bool,
) -> Result<Variant, SelectError> {
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

/// Codecs every client plays as they are first, so no transcode is needed.
fn audio_rank(v: &Variant) -> u8 {
    match v.audio {
        Some(crate::media::AudioCodec::Aac) => 5,
        Some(crate::media::AudioCodec::Mp3) => 4,
        Some(crate::media::AudioCodec::Opus) => 3,
        Some(crate::media::AudioCodec::Vorbis) => 2,
        Some(crate::media::AudioCodec::Flac) => 1,
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
        let chosen = select_variant(
            &[small.clone(), hevc, tall, best.clone()],
            &limits(),
            "x",
            MediaKind::Video,
        )
        .unwrap();
        assert_eq!(chosen.url, best.url);
        let mut big = variant("https://h/big.mp4", VariantKind::File, 1080, 9000);
        big.size = Some(5000);
        let chosen =
            select_variant(&[big, small.clone()], &limits(), "x", MediaKind::Video).unwrap();
        assert_eq!(chosen.url, small.url);
        let hls = variant("https://h/v.m3u8", VariantKind::Hls, 720, 9000);
        let chosen =
            select_variant(&[hls, small.clone()], &limits(), "x", MediaKind::Video).unwrap();
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
        let chosen = select_variant(
            &[opus.clone(), video.clone(), aac.clone()],
            &limits(),
            "x",
            MediaKind::Video,
        )
        .unwrap();
        assert_eq!(chosen.url, video.url);
        assert_eq!(chosen.audio_url.unwrap(), aac.url);
        assert_eq!(chosen.audio, Some(AudioCodec::Aac));
        assert_eq!(chosen.size, Some(360));
        assert_eq!(
            select_variant(&[opus.clone()], &limits(), "x", MediaKind::Video).unwrap_err(),
            SelectError::AudioOnly("x".into())
        );
        // For audio, the audio-only variant wins over the video with sound, and the best
        // codec over the rest.
        let mut full = variant("https://h/full.mp4", VariantKind::File, 720, 5000);
        full.audio = Some(AudioCodec::Aac);
        let chosen = select_variant(
            &[full.clone(), opus.clone(), aac.clone()],
            &limits(),
            "x",
            MediaKind::Audio,
        )
        .unwrap();
        assert_eq!(chosen.url, aac.url);
        let chosen = select_variant(
            &[full.clone(), video.clone()],
            &limits(),
            "x",
            MediaKind::Audio,
        )
        .unwrap();
        assert_eq!(chosen.url, full.url);
        assert_eq!(
            select_variant(&[video], &limits(), "x", MediaKind::Audio).unwrap_err(),
            SelectError::NoAudio("x".into())
        );
    }

    #[test]
    fn images_and_files_take_the_largest_under_the_limit() {
        let mut thumb = variant("https://h/t.jpg", VariantKind::File, 240, 0);
        thumb.size = Some(20);
        let mut large = variant("https://h/l.jpg", VariantKind::File, 2000, 0);
        large.size = Some(900);
        let mut huge = variant("https://h/h.jpg", VariantKind::File, 6000, 0);
        huge.size = Some(5000);
        let chosen = select_variant(
            &[thumb.clone(), huge.clone(), large.clone()],
            &limits(),
            "x",
            MediaKind::Image,
        )
        .unwrap();
        assert_eq!(chosen.url, large.url);
        let chosen = select_variant(&[huge.clone()], &limits(), "x", MediaKind::Image).unwrap();
        assert_eq!(chosen.url, huge.url);
        let mut sound = variant("https://h/a.mp3", VariantKind::File, 0, 128);
        sound.audio_only = true;
        assert_eq!(
            select_variant(&[sound], &limits(), "x", MediaKind::File).unwrap_err(),
            SelectError::NoFile("x".into(), MediaKind::File)
        );
    }

    #[test]
    fn locked_and_empty_lists_are_refused() {
        assert_eq!(
            select_variant(&[], &limits(), "yt", MediaKind::Video).unwrap_err(),
            SelectError::NoVariants("yt".into())
        );
        let mut locked = variant("https://h/v.mpd", VariantKind::Dash, 1080, 1);
        locked.drm = Some("widevine".into());
        assert_eq!(
            select_variant(&[locked.clone()], &limits(), "yt", MediaKind::Video).unwrap_err(),
            SelectError::Drm("widevine".into())
        );
        let open = variant("https://h/v.mp4", VariantKind::File, 360, 1);
        let chosen =
            select_variant(&[locked, open.clone()], &limits(), "yt", MediaKind::Video).unwrap();
        assert_eq!(chosen.url, open.url);
    }
}
