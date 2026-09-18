use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::event::ProgressSender;
use crate::media::{AudioCodec, Container, LocalFile, MediaInfo, MediaKind, VideoCodec};
use crate::resolve::ClipRange;

#[async_trait]
pub trait Transcoder: Send + Sync {
    async fn probe(&self, path: &Path) -> Result<MediaInfo, TranscodeError>;
    async fn transcode(
        &self,
        input: &LocalFile,
        target: &Target,
        dest_dir: &Path,
        progress: ProgressSender,
    ) -> Result<LocalFile, TranscodeError>;
}

/// What the output has to be, by the kind of media it is.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    Video(VideoTarget),
    Audio(AudioTarget),
    Image(ImageTarget),
    /// Any other file: published as it is when it fits, never converted.
    File {
        max_bytes: u64,
    },
}

impl Target {
    pub fn kind(&self) -> MediaKind {
        match self {
            Target::Video(_) => MediaKind::Video,
            Target::Audio(_) => MediaKind::Audio,
            Target::Image(_) => MediaKind::Image,
            Target::File { .. } => MediaKind::File,
        }
    }

    pub fn max_bytes(&self) -> u64 {
        match self {
            Target::Video(t) => t.max_bytes,
            Target::Audio(t) => t.max_bytes,
            Target::Image(t) => t.max_bytes,
            Target::File { max_bytes } => *max_bytes,
        }
    }

    /// The portion of the input kept, for media with a timeline.
    pub fn clip(&self) -> Option<ClipRange> {
        match self {
            Target::Video(t) => t.clip,
            Target::Audio(t) => t.clip,
            Target::Image(_) | Target::File { .. } => None,
        }
    }

    /// The extension the output file gets.
    pub fn extension(&self) -> Option<&str> {
        match self {
            Target::Video(t) => Some(t.container.extension()),
            Target::Audio(t) => Some(t.container.extension()),
            Target::Image(t) => Some(t.container.extension()),
            Target::File { .. } => None,
        }
    }
}

/// Sound alone: encoded into `container` with `codec` under `max_bytes`, the bitrate
/// stepped down until it fits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioTarget {
    pub container: Container,
    pub codec: AudioCodec,
    pub max_bytes: u64,
    /// The portion of the input to keep.
    #[serde(default)]
    pub clip: Option<ClipRange>,
}

/// A still image: re-encoded into `container` under `max_bytes`, the picture scaled down
/// and the quality lowered until it fits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImageTarget {
    pub container: Container,
    pub max_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoTarget {
    pub container: Container,
    pub video: VideoCodec,
    pub audio: Option<AudioCodec>,
    pub max_bytes: u64,
    pub max_height: u32,
    /// The portion of the input to keep.
    #[serde(default)]
    pub clip: Option<ClipRange>,
    /// A subtitle file to render into the picture.
    #[serde(default)]
    pub burn_subtitles: Option<PathBuf>,
}

impl VideoTarget {
    pub fn new(
        container: Container,
        video: VideoCodec,
        audio: Option<AudioCodec>,
        max_bytes: u64,
        max_height: u32,
    ) -> Self {
        Self {
            container,
            video,
            audio,
            max_bytes,
            max_height,
            clip: None,
            burn_subtitles: None,
        }
    }

    /// How much of `duration` the output covers once the clip is applied.
    pub fn output_duration(&self, duration: Duration) -> Duration {
        clipped_duration(self.clip, duration)
    }
}

/// How much of `duration` remains once `clip` is applied.
pub fn clipped_duration(clip: Option<ClipRange>, duration: Duration) -> Duration {
    match clip {
        Some(clip) => {
            let start = clip.start.min(duration);
            let end = clip.end.map_or(duration, |e| e.min(duration));
            end.saturating_sub(start)
        }
        None => duration,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TranscodeError {
    #[error("could not read media: {0}")]
    Probe(String),
    #[error("input has no video stream")]
    NoVideo,
    #[error("input has no audio stream")]
    NoAudio,
    #[error("input is not a still image")]
    NoPicture,
    #[error("input duration is unknown")]
    NoDuration,
    #[error("unsupported target: {container:?} with {video:?}")]
    UnsupportedTarget {
        container: Container,
        video: VideoCodec,
    },
    #[error("unsupported audio target: {container:?} with {codec:?}")]
    UnsupportedAudioTarget {
        container: Container,
        codec: AudioCodec,
    },
    #[error("unsupported image target: {container:?}")]
    UnsupportedImageTarget { container: Container },
    #[error("cannot fit {duration_secs}s of video under {max_bytes} bytes")]
    BudgetUnreachable { max_bytes: u64, duration_secs: u64 },
    #[error("cannot fit {duration_secs}s of audio under {max_bytes} bytes")]
    AudioBudgetUnreachable { max_bytes: u64, duration_secs: u64 },
    #[error("cannot fit the picture under {max_bytes} bytes")]
    ImageBudgetUnreachable { max_bytes: u64 },
    #[error("file is {size} bytes and cannot be made smaller; destination allows {max_bytes}")]
    CannotShrink { size: u64, max_bytes: u64 },
    #[error("destination does not take {0} files")]
    NotAccepted(MediaKind),
    #[error("a file that is not media is published as it is, never converted")]
    NotMedia,
    #[error("{0}")]
    Process(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

use std::ffi::OsString;
use std::time::Duration;

use crate::event::Progress;
use crate::ffmpeg::{Ffmpeg, FfmpegError};
use crate::media::VideoTrack;

const SIZE_MARGIN: f64 = 0.95;
const MIN_BITS_PER_PIXEL: f64 = 0.03;
const MAX_BITS_PER_PIXEL: f64 = 0.16;
const MIN_VIDEO_BPS: f64 = 40_000.0;
const SHORT_SIDES: [u32; 6] = [1080, 720, 480, 360, 240, 144];
const AUDIO_BPS: [u64; 5] = [128_000, 96_000, 64_000, 48_000, 32_000];
const ATTEMPTS: usize = 4;

impl From<FfmpegError> for TranscodeError {
    fn from(error: FfmpegError) -> Self {
        match error {
            FfmpegError::Probe(m) => TranscodeError::Probe(m),
            FfmpegError::Io(e) => TranscodeError::Io(e),
            other => TranscodeError::Process(other.to_string()),
        }
    }
}

/// Encodes with the embedded ffmpeg: remuxes when the streams already fit the target,
/// otherwise runs a two pass encode sized to the byte budget, stepping down resolution,
/// frame rate, and audio bitrate as needed.
pub struct FfmpegTranscoder {
    ffmpeg: Ffmpeg,
}

impl FfmpegTranscoder {
    pub fn new(ffmpeg: Ffmpeg) -> Self {
        Self { ffmpeg }
    }
}

struct Encoders {
    video: &'static str,
    audio: &'static str,
    faststart: bool,
}

fn encoders_for(target: &VideoTarget) -> Result<Encoders, TranscodeError> {
    let unsupported = || TranscodeError::UnsupportedTarget {
        container: target.container.clone(),
        video: target.video.clone(),
    };
    let video = match (&target.container, &target.video) {
        (Container::Mp4 | Container::Mov | Container::Mkv, VideoCodec::H264) => "libx264",
        (Container::Webm | Container::Mkv, VideoCodec::Vp9) => "libvpx-vp9",
        _ => return Err(unsupported()),
    };
    let audio = match (&target.container, target.audio.as_ref()) {
        (_, None) => "",
        (Container::Mp4 | Container::Mov | Container::Mkv, Some(AudioCodec::Aac)) => "aac",
        (Container::Webm | Container::Mkv, Some(AudioCodec::Opus)) => "libopus",
        _ => return Err(unsupported()),
    };
    Ok(Encoders {
        video,
        audio,
        faststart: matches!(target.container, Container::Mp4 | Container::Mov),
    })
}

#[derive(Debug, Clone, Copy)]
struct Step {
    width: u32,
    height: u32,
    fps: f64,
    video_bps: u64,
    audio_bps: u64,
}

struct Ladder {
    shapes: Vec<(u32, u32, f64)>,
    audio: Vec<u64>,
}

impl Ladder {
    fn new(video: &VideoTrack, max_height: u32, has_audio: bool) -> Self {
        let landscape = video.width >= video.height;
        let short = video.width.min(video.height).max(2);
        let long = video.width.max(video.height).max(2);
        let cap = short.min(max_height);
        let mut sides = vec![cap];
        sides.extend(SHORT_SIDES.iter().copied().filter(|s| *s < cap));
        let src_fps = video.fps.unwrap_or(30.0).clamp(1.0, 60.0);
        let rates: Vec<f64> = if src_fps > 30.5 {
            vec![src_fps, 30.0]
        } else {
            vec![src_fps]
        };
        let mut shapes = Vec::new();
        for s in sides {
            let s = s & !1;
            let l = ((long as f64 * s as f64 / short as f64).round() as u32) & !1;
            let (w, h) = if landscape { (l, s) } else { (s, l) };
            for &fps in &rates {
                shapes.push((w.max(2), h.max(2), fps));
            }
        }
        let audio = if has_audio {
            AUDIO_BPS.to_vec()
        } else {
            vec![0]
        };
        Self { shapes, audio }
    }

    fn pick(&self, total_bps: f64) -> Option<Step> {
        for &(w, h, fps) in &self.shapes {
            let pixels = w as f64 * h as f64 * fps;
            let min = (MIN_BITS_PER_PIXEL * pixels).max(MIN_VIDEO_BPS);
            let max = MAX_BITS_PER_PIXEL * pixels;
            for &audio in &self.audio {
                let video = total_bps - audio as f64;
                if video >= min {
                    return Some(Step {
                        width: w,
                        height: h,
                        fps,
                        video_bps: video.min(max) as u64,
                        audio_bps: audio,
                    });
                }
            }
        }
        None
    }
}

/// Escapes a path for use as an ffmpeg filter option value: the graph parser and the
/// option parser each strip one level of backslashes, so `:` becomes `\\:`, `'` becomes
/// `\\\'`, and `\` becomes `\\\\`.
pub fn filter_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    let mut out = String::with_capacity(text.len() + 8);
    for ch in text.chars() {
        match ch {
            '\\' => out.push_str("\\\\\\\\"),
            '\'' => out.push_str("\\\\\\'"),
            ':' => out.push_str("\\\\:"),
            '[' | ']' | ',' | ';' => {
                out.push('\\');
                out.push(ch);
            }
            other => out.push(other),
        }
    }
    out
}

fn video_filter(video: &VideoTrack, step: &Step, subtitles: Option<&Path>) -> String {
    let mut parts = Vec::new();
    if let Some(path) = subtitles {
        parts.push(format!("subtitles=filename={}", filter_path(path)));
    }
    if step.width < video.width || step.height < video.height {
        parts.push(format!(
            "scale={}:{}:force_original_aspect_ratio=decrease:force_divisible_by=2:flags=lanczos",
            step.width, step.height
        ));
    }
    if let Some(src) = video.fps
        && step.fps + 0.5 < src
    {
        parts.push(format!("fps={}", step.fps));
    }
    parts.push("format=yuv420p".to_string());
    parts.join(",")
}

/// `-ss`/`-t` input options for a clip, placed before `-i` so the seek is fast.
fn clip_args(clip: Option<ClipRange>) -> Vec<OsString> {
    let Some(clip) = clip else {
        return Vec::new();
    };
    let mut args: Vec<OsString> = vec![
        "-ss".into(),
        format!("{:.3}", clip.start.as_secs_f64()).into(),
    ];
    if let Some(end) = clip.end {
        let length = end.saturating_sub(clip.start);
        args.extend(["-t".into(), format!("{:.3}", length.as_secs_f64()).into()]);
    }
    args
}

fn remux_mode(info: &MediaInfo, size: u64, target: &VideoTarget) -> Option<bool> {
    let video = info.video.as_ref()?;
    if target.clip.is_some() || target.burn_subtitles.is_some() {
        return None;
    }
    if video.codec != target.video || size > target.max_bytes {
        return None;
    }
    if video.width.min(video.height) > target.max_height {
        return None;
    }
    let audio_matches = match (&info.audio, &target.audio) {
        (None, _) => true,
        (Some(_), None) => true,
        (Some(a), Some(t)) => a.codec == *t,
    };
    Some(audio_matches)
}

struct RemuxPlan {
    wants_audio: bool,
    copy_audio: bool,
    audio_encoder: &'static str,
    faststart: bool,
}

impl FfmpegTranscoder {
    async fn remux(
        &self,
        input: &LocalFile,
        info: &MediaInfo,
        plan: &RemuxPlan,
        output: &Path,
        progress: &ProgressSender,
    ) -> Result<LocalFile, TranscodeError> {
        let duration = info.duration.unwrap_or_default();
        let mut args: Vec<OsString> = vec![
            "-loglevel".into(),
            "warning".into(),
            "-i".into(),
            input.path.as_os_str().to_owned(),
            "-map".into(),
            "0:v:0".into(),
            "-c:v".into(),
            "copy".into(),
        ];
        match (info.audio.is_some(), plan.wants_audio) {
            (true, true) if plan.copy_audio => {
                args.extend(["-map".into(), "0:a:0".into(), "-c:a".into(), "copy".into()]);
            }
            (true, true) => {
                args.extend([
                    "-map".into(),
                    "0:a:0".into(),
                    "-c:a".into(),
                    plan.audio_encoder.into(),
                    "-b:a".into(),
                    "128k".into(),
                    "-ac".into(),
                    "2".into(),
                    "-ar".into(),
                    "48000".into(),
                ]);
            }
            _ => args.push("-an".into()),
        }
        args.extend(["-sn".into(), "-dn".into()]);
        if plan.faststart {
            args.extend(["-movflags".into(), "+faststart".into()]);
        }
        args.push(output.as_os_str().to_owned());
        let total = duration.as_micros() as u64;
        let reporter = progress.clone();
        self.ffmpeg
            .run(args, move |t| {
                reporter.send_replace(Progress {
                    done: (t.as_micros() as u64).min(total),
                    total: Some(total),
                });
            })
            .await?;
        let mut file = LocalFile::from_path(output.to_path_buf()).await?;
        file.info = Some(self.ffmpeg.probe(output).await?);
        Ok(file)
    }

    #[allow(clippy::too_many_arguments)]
    async fn encode(
        &self,
        input: &LocalFile,
        video: &VideoTrack,
        has_audio: bool,
        encoders: &Encoders,
        step: &Step,
        output: &Path,
        log_prefix: &Path,
        progress: &ProgressSender,
        duration: Duration,
        target: &VideoTarget,
    ) -> Result<LocalFile, TranscodeError> {
        let total = duration.as_micros() as u64;
        let filter = video_filter(video, step, target.burn_subtitles.as_deref());
        let bitrate = step.video_bps.to_string();
        let maxrate = (step.video_bps as f64 * 1.3) as u64;
        let bufsize = step.video_bps * 2;
        let clip = clip_args(target.clip);
        let common = |pass: u8| -> Vec<OsString> {
            let mut a: Vec<OsString> = vec!["-loglevel".into(), "warning".into()];
            a.extend(clip.iter().cloned());
            a.extend([
                "-i".into(),
                input.path.as_os_str().to_owned(),
                "-map".into(),
                "0:v:0".into(),
                "-vf".into(),
                filter.clone().into(),
                "-c:v".into(),
                encoders.video.into(),
                "-b:v".into(),
                bitrate.clone().into(),
                "-pass".into(),
                pass.to_string().into(),
                "-passlogfile".into(),
                log_prefix.as_os_str().to_owned(),
            ]);
            match encoders.video {
                "libx264" => a.extend([
                    "-preset".into(),
                    "medium".into(),
                    "-profile:v".into(),
                    "high".into(),
                ]),
                _ => a.extend([
                    "-row-mt".into(),
                    "1".into(),
                    "-deadline".into(),
                    "good".into(),
                    "-cpu-used".into(),
                    if pass == 1 { "4".into() } else { "2".into() },
                ]),
            }
            a
        };

        let mut pass1 = common(1);
        pass1.extend([
            "-an".into(),
            "-sn".into(),
            "-dn".into(),
            "-f".into(),
            "null".into(),
            "-".into(),
        ]);
        let reporter = progress.clone();
        self.ffmpeg
            .run(pass1, move |t| {
                reporter.send_replace(Progress {
                    done: ((t.as_micros() as u64) / 2).min(total / 2),
                    total: Some(total),
                });
            })
            .await?;

        let mut pass2 = common(2);
        pass2.extend([
            "-maxrate".into(),
            maxrate.to_string().into(),
            "-bufsize".into(),
            bufsize.to_string().into(),
        ]);
        if has_audio && step.audio_bps > 0 {
            pass2.extend([
                "-map".into(),
                "0:a:0".into(),
                "-c:a".into(),
                encoders.audio.into(),
                "-b:a".into(),
                step.audio_bps.to_string().into(),
                "-ac".into(),
                "2".into(),
                "-ar".into(),
                "48000".into(),
            ]);
        } else {
            pass2.push("-an".into());
        }
        pass2.extend(["-sn".into(), "-dn".into()]);
        if encoders.faststart {
            pass2.extend(["-movflags".into(), "+faststart".into()]);
        }
        pass2.push(output.as_os_str().to_owned());
        let reporter = progress.clone();
        self.ffmpeg
            .run(pass2, move |t| {
                reporter.send_replace(Progress {
                    done: (total / 2 + (t.as_micros() as u64) / 2).min(total),
                    total: Some(total),
                });
            })
            .await?;
        let mut file = LocalFile::from_path(output.to_path_buf()).await?;
        file.info = Some(self.ffmpeg.probe(output).await?);
        Ok(file)
    }
}

/// The bitrates audio alone is tried at, highest first.
const AUDIO_ONLY_BPS: [u64; 8] = [
    192_000, 160_000, 128_000, 96_000, 64_000, 48_000, 32_000, 24_000,
];
/// The long edges an image is scaled down to, in turn, when it is over budget.
const IMAGE_EDGES: [u32; 10] = [4096, 3072, 2048, 1600, 1280, 1024, 800, 640, 480, 320];
/// JPEG `-q:v` levels, best first; WebP quality falls with them.
const JPEG_QUALITIES: [u32; 5] = [2, 4, 7, 12, 20];

fn audio_encoder(target: &AudioTarget) -> Result<&'static str, TranscodeError> {
    Ok(match (&target.container, &target.codec) {
        (Container::M4a | Container::Mp4, AudioCodec::Aac) => "aac",
        (Container::Mp3, AudioCodec::Mp3) => "libmp3lame",
        (Container::Ogg | Container::Opus | Container::Webm, AudioCodec::Opus) => "libopus",
        (Container::Ogg, AudioCodec::Vorbis) => "libvorbis",
        (Container::Flac, AudioCodec::Flac) => "flac",
        _ => {
            return Err(TranscodeError::UnsupportedAudioTarget {
                container: target.container.clone(),
                codec: target.codec.clone(),
            });
        }
    })
}

/// The ffmpeg muxer named for an audio container, where the extension alone is not enough.
fn audio_muxer(container: &Container) -> Option<&'static str> {
    match container {
        Container::M4a => Some("ipod"),
        Container::Opus | Container::Ogg => Some("ogg"),
        _ => None,
    }
}

struct ImageEncoder {
    codec: &'static str,
    /// Whether a quality option is taken, and which.
    quality: Option<&'static str>,
    pix_fmt: Option<&'static str>,
}

fn image_encoder(target: &ImageTarget) -> Result<ImageEncoder, TranscodeError> {
    Ok(match &target.container {
        Container::Jpeg => ImageEncoder {
            codec: "mjpeg",
            quality: Some("-q:v"),
            pix_fmt: Some("yuvj420p"),
        },
        Container::Png => ImageEncoder {
            codec: "png",
            quality: None,
            pix_fmt: None,
        },
        Container::Webp => ImageEncoder {
            codec: "libwebp",
            quality: Some("-quality"),
            pix_fmt: None,
        },
        other => {
            return Err(TranscodeError::UnsupportedImageTarget {
                container: other.clone(),
            });
        }
    })
}

/// The picture sizes an image is tried at: its own, then each smaller long edge.
fn image_steps(width: u32, height: u32) -> Vec<(u32, u32)> {
    let long = width.max(height).max(1);
    let mut steps = vec![(width.max(1), height.max(1))];
    for edge in IMAGE_EDGES {
        if edge >= long {
            continue;
        }
        let scale = edge as f64 / long as f64;
        let w = ((width as f64 * scale).round() as u32).max(1);
        let h = ((height as f64 * scale).round() as u32).max(1);
        steps.push((w, h));
    }
    steps
}

impl FfmpegTranscoder {
    async fn transcode_audio(
        &self,
        input: &LocalFile,
        info: &MediaInfo,
        target: &AudioTarget,
        dest_dir: &Path,
        progress: &ProgressSender,
    ) -> Result<LocalFile, TranscodeError> {
        if info.audio.is_none() {
            return Err(TranscodeError::NoAudio);
        }
        let duration = clipped_duration(
            target.clip,
            info.duration.ok_or(TranscodeError::NoDuration)?,
        );
        if duration.is_zero() {
            return Err(TranscodeError::NoDuration);
        }
        let encoder = audio_encoder(target)?;
        tokio::fs::create_dir_all(dest_dir).await?;
        let output = dest_dir.join(format!("output.{}", target.container.extension()));
        let secs = duration.as_secs_f64().max(0.1);
        let unreachable = || TranscodeError::AudioBudgetUnreachable {
            max_bytes: target.max_bytes,
            duration_secs: duration.as_secs(),
        };
        let mut budget_bps = target.max_bytes as f64 * 8.0 * SIZE_MARGIN / secs;
        let total = duration.as_micros() as u64;
        for attempt in 1..=ATTEMPTS {
            let bps = AUDIO_ONLY_BPS
                .iter()
                .copied()
                .find(|b| (*b as f64) <= budget_bps)
                .ok_or_else(unreachable)?;
            tracing::info!(attempt, bps, encoder, "encoding audio");
            let mut args: Vec<OsString> = vec!["-loglevel".into(), "warning".into()];
            args.extend(clip_args(target.clip));
            args.extend([
                "-i".into(),
                input.path.as_os_str().to_owned(),
                "-map".into(),
                "0:a:0".into(),
                "-vn".into(),
                "-sn".into(),
                "-dn".into(),
                "-c:a".into(),
                encoder.into(),
                "-ac".into(),
                "2".into(),
                "-ar".into(),
                "48000".into(),
            ]);
            if encoder != "flac" {
                args.extend(["-b:a".into(), bps.to_string().into()]);
            }
            if let Some(muxer) = audio_muxer(&target.container) {
                args.extend(["-f".into(), muxer.into()]);
            }
            if target.container == Container::M4a {
                args.extend(["-movflags".into(), "+faststart".into()]);
            }
            args.push(output.as_os_str().to_owned());
            let reporter = progress.clone();
            self.ffmpeg
                .run(args, move |t| {
                    reporter.send_replace(Progress {
                        done: (t.as_micros() as u64).min(total),
                        total: Some(total),
                    });
                })
                .await?;
            let mut file = LocalFile::from_path(output.clone()).await?;
            if file.size <= target.max_bytes {
                file.info = Some(self.ffmpeg.probe(&output).await?);
                return Ok(file);
            }
            let ratio = target.max_bytes as f64 * SIZE_MARGIN / file.size as f64;
            budget_bps = (bps as f64 * ratio.min(0.97)).min(bps as f64 - 1.0);
            tracing::info!(
                size = file.size,
                ratio,
                "audio over budget, lowering bitrate"
            );
        }
        Err(unreachable())
    }

    async fn transcode_image(
        &self,
        input: &LocalFile,
        info: &MediaInfo,
        target: &ImageTarget,
        dest_dir: &Path,
        progress: &ProgressSender,
    ) -> Result<LocalFile, TranscodeError> {
        let picture = info.video.as_ref().ok_or(TranscodeError::NoPicture)?;
        let encoder = image_encoder(target)?;
        tokio::fs::create_dir_all(dest_dir).await?;
        let output = dest_dir.join(format!("output.{}", target.container.extension()));
        let steps = image_steps(picture.width, picture.height);
        let qualities: &[u32] = match encoder.quality {
            Some(_) => &JPEG_QUALITIES,
            None => &[0],
        };
        let attempts = steps.len() * qualities.len();
        let mut done = 0u64;
        // Quality falls before the picture shrinks, so the largest picture that fits wins.
        for (w, h) in steps {
            for &q in qualities {
                done += 1;
                progress.send_replace(Progress {
                    done,
                    total: Some(attempts as u64),
                });
                let mut args: Vec<OsString> = vec![
                    "-loglevel".into(),
                    "warning".into(),
                    "-i".into(),
                    input.path.as_os_str().to_owned(),
                    "-map".into(),
                    "0:v:0".into(),
                    "-frames:v".into(),
                    "1".into(),
                    "-an".into(),
                    "-sn".into(),
                    "-dn".into(),
                    "-vf".into(),
                    format!("scale={w}:{h}:flags=lanczos").into(),
                    "-c:v".into(),
                    encoder.codec.into(),
                ];
                if let Some(pix_fmt) = encoder.pix_fmt {
                    args.extend(["-pix_fmt".into(), pix_fmt.into()]);
                }
                match encoder.quality {
                    Some("-quality") => {
                        // libwebp takes 0..100, best last; map the JPEG scale onto it.
                        let quality = 100u32.saturating_sub(q * 4);
                        args.extend(["-quality".into(), quality.to_string().into()]);
                    }
                    Some(option) => args.extend([option.into(), q.to_string().into()]),
                    None => {}
                }
                args.push(output.as_os_str().to_owned());
                self.ffmpeg.run(args, |_| {}).await?;
                let mut file = LocalFile::from_path(output.clone()).await?;
                if file.size <= target.max_bytes {
                    file.info = Some(self.ffmpeg.probe(&output).await?);
                    return Ok(file);
                }
                tracing::info!(size = file.size, w, h, q, "image over budget, shrinking");
            }
        }
        let _ = tokio::fs::remove_file(&output).await;
        Err(TranscodeError::ImageBudgetUnreachable {
            max_bytes: target.max_bytes,
        })
    }

    async fn transcode_video(
        &self,
        input: &LocalFile,
        info: &MediaInfo,
        target: &VideoTarget,
        dest_dir: &Path,
        progress: &ProgressSender,
    ) -> Result<LocalFile, TranscodeError> {
        let video = info.video.clone().ok_or(TranscodeError::NoVideo)?;
        let duration = target.output_duration(info.duration.ok_or(TranscodeError::NoDuration)?);
        if duration.is_zero() {
            return Err(TranscodeError::NoDuration);
        }
        let encoders = encoders_for(target)?;
        tokio::fs::create_dir_all(dest_dir).await?;
        let output = dest_dir.join(format!("output.{}", target.container.extension()));

        if let Some(copy_audio) = remux_mode(info, input.size, target) {
            let plan = RemuxPlan {
                wants_audio: target.audio.is_some(),
                copy_audio,
                audio_encoder: encoders.audio,
                faststart: encoders.faststart,
            };
            match self.remux(input, info, &plan, &output, progress).await {
                Ok(file) if file.size <= target.max_bytes => return Ok(file),
                Ok(file) => {
                    tracing::info!(size = file.size, "remux exceeds budget, encoding instead");
                }
                Err(error) => {
                    tracing::warn!("remux failed, encoding instead: {error}");
                }
            }
            let _ = tokio::fs::remove_file(&output).await;
        }

        let secs = duration.as_secs_f64().max(0.1);
        let unreachable = || TranscodeError::BudgetUnreachable {
            max_bytes: target.max_bytes,
            duration_secs: duration.as_secs(),
        };
        let ladder = Ladder::new(
            &video,
            target.max_height,
            info.audio.is_some() && target.audio.is_some(),
        );
        let mut total_bps = target.max_bytes as f64 * 8.0 * SIZE_MARGIN / secs;
        let log_prefix = dest_dir.join("passlog");
        for attempt in 1..=ATTEMPTS {
            let step = ladder.pick(total_bps).ok_or_else(unreachable)?;
            tracing::info!(
                attempt,
                width = step.width,
                height = step.height,
                fps = step.fps,
                video_bps = step.video_bps,
                audio_bps = step.audio_bps,
                "encoding"
            );
            let file = self
                .encode(
                    input,
                    &video,
                    info.audio.is_some(),
                    &encoders,
                    &step,
                    &output,
                    &log_prefix,
                    progress,
                    duration,
                    target,
                )
                .await?;
            if file.size <= target.max_bytes {
                return Ok(file);
            }
            let ratio = target.max_bytes as f64 * SIZE_MARGIN / file.size as f64;
            total_bps *= ratio.min(0.97);
            tracing::info!(
                size = file.size,
                ratio,
                "output over budget, shrinking bitrate"
            );
        }
        Err(unreachable())
    }
}

#[async_trait]
impl Transcoder for FfmpegTranscoder {
    async fn probe(&self, path: &Path) -> Result<MediaInfo, TranscodeError> {
        Ok(self.ffmpeg.probe(path).await?)
    }

    async fn transcode(
        &self,
        input: &LocalFile,
        target: &Target,
        dest_dir: &Path,
        progress: ProgressSender,
    ) -> Result<LocalFile, TranscodeError> {
        let info = match &input.info {
            Some(info) => info.clone(),
            None => self.ffmpeg.probe(&input.path).await?,
        };
        match target {
            Target::Video(target) => {
                self.transcode_video(input, &info, target, dest_dir, &progress)
                    .await
            }
            Target::Audio(target) => {
                self.transcode_audio(input, &info, target, dest_dir, &progress)
                    .await
            }
            Target::Image(target) => {
                self.transcode_image(input, &info, target, dest_dir, &progress)
                    .await
            }
            Target::File { .. } => Err(TranscodeError::NotMedia),
        }
    }
}

#[cfg(test)]
mod target_tests {
    use super::*;

    #[test]
    fn clips_bound_the_output_duration_and_seek_args() {
        let mut target = VideoTarget::new(Container::Mp4, VideoCodec::H264, None, 1000, 1080);
        let full = Duration::from_secs(100);
        assert_eq!(target.output_duration(full), full);
        target.clip = Some(ClipRange {
            start: Duration::from_secs(10),
            end: Some(Duration::from_secs(25)),
        });
        assert_eq!(target.output_duration(full), Duration::from_secs(15));
        assert_eq!(
            clip_args(target.clip),
            vec![
                OsString::from("-ss"),
                "10.000".into(),
                "-t".into(),
                "15.000".into()
            ]
        );
        target.clip = Some(ClipRange {
            start: Duration::from_secs(90),
            end: None,
        });
        assert_eq!(target.output_duration(full), Duration::from_secs(10));
        assert_eq!(
            clip_args(target.clip),
            vec![OsString::from("-ss"), "90.000".into()]
        );
        assert!(clip_args(None).is_empty());
    }

    #[test]
    fn image_steps_start_at_the_picture_and_shrink_by_long_edge() {
        let steps = image_steps(3000, 1500);
        assert_eq!(steps[0], (3000, 1500));
        assert_eq!(steps[1], (2048, 1024));
        assert_eq!(*steps.last().unwrap(), (320, 160));
        assert!(steps.windows(2).all(|w| w[0].0 > w[1].0));
        assert_eq!(image_steps(100, 50), vec![(100, 50)]);
    }

    #[test]
    fn audio_and_image_encoders_follow_the_container() {
        let m4a = AudioTarget {
            container: Container::M4a,
            codec: AudioCodec::Aac,
            max_bytes: 1,
            clip: None,
        };
        assert_eq!(audio_encoder(&m4a).unwrap(), "aac");
        let mp3 = AudioTarget {
            container: Container::Mp3,
            codec: AudioCodec::Mp3,
            ..m4a.clone()
        };
        assert_eq!(audio_encoder(&mp3).unwrap(), "libmp3lame");
        let wrong = AudioTarget {
            container: Container::Mp3,
            codec: AudioCodec::Aac,
            ..m4a
        };
        assert!(matches!(
            audio_encoder(&wrong),
            Err(TranscodeError::UnsupportedAudioTarget { .. })
        ));
        let jpeg = ImageTarget {
            container: Container::Jpeg,
            max_bytes: 1,
        };
        assert_eq!(image_encoder(&jpeg).unwrap().codec, "mjpeg");
        let avif = ImageTarget {
            container: Container::Avif,
            max_bytes: 1,
        };
        assert!(matches!(
            image_encoder(&avif),
            Err(TranscodeError::UnsupportedImageTarget { .. })
        ));
    }

    #[test]
    fn filter_paths_are_escaped() {
        assert_eq!(filter_path(Path::new("/tmp/a b.vtt")), "/tmp/a b.vtt");
        assert_eq!(
            filter_path(Path::new("C:\\x[1].srt")),
            "C\\\\:\\\\\\\\x\\[1\\].srt"
        );
        let video = VideoTrack {
            codec: VideoCodec::H264,
            width: 1920,
            height: 1080,
            fps: Some(30.0),
            bitrate: None,
        };
        let step = Step {
            width: 1280,
            height: 720,
            fps: 30.0,
            video_bps: 1,
            audio_bps: 0,
        };
        let filter = video_filter(&video, &step, Some(Path::new("/s/it's.vtt")));
        assert!(
            filter.starts_with("subtitles=filename=/s/it\\\\\\'s.vtt,scale=1280:720"),
            "{filter}"
        );
    }
}
