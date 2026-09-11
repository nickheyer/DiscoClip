use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::event::ProgressSender;
use crate::media::{AudioCodec, Container, LocalFile, MediaInfo, VideoCodec};
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Target {
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

impl Target {
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
        match self.clip {
            Some(clip) => {
                let start = clip.start.min(duration);
                let end = clip.end.map_or(duration, |e| e.min(duration));
                end.saturating_sub(start)
            }
            None => duration,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TranscodeError {
    #[error("could not read media: {0}")]
    Probe(String),
    #[error("input has no video stream")]
    NoVideo,
    #[error("input duration is unknown")]
    NoDuration,
    #[error("unsupported target: {container:?} with {video:?}")]
    UnsupportedTarget {
        container: Container,
        video: VideoCodec,
    },
    #[error("cannot fit {duration_secs}s of video under {max_bytes} bytes")]
    BudgetUnreachable { max_bytes: u64, duration_secs: u64 },
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

fn encoders_for(target: &Target) -> Result<Encoders, TranscodeError> {
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

fn remux_mode(info: &MediaInfo, size: u64, target: &Target) -> Option<bool> {
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
        target: &Target,
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
        let video = info.video.clone().ok_or(TranscodeError::NoVideo)?;
        let duration = target.output_duration(info.duration.ok_or(TranscodeError::NoDuration)?);
        if duration.is_zero() {
            return Err(TranscodeError::NoDuration);
        }
        let encoders = encoders_for(target)?;
        tokio::fs::create_dir_all(dest_dir).await?;
        let output = dest_dir.join(format!("output.{}", target.container.extension()));

        if let Some(copy_audio) = remux_mode(&info, input.size, target) {
            let plan = RemuxPlan {
                wants_audio: target.audio.is_some(),
                copy_audio,
                audio_encoder: encoders.audio,
                faststart: encoders.faststart,
            };
            match self.remux(input, &info, &plan, &output, &progress).await {
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
                    &progress,
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

#[cfg(test)]
mod target_tests {
    use super::*;

    #[test]
    fn clips_bound_the_output_duration_and_seek_args() {
        let mut target = Target::new(Container::Mp4, VideoCodec::H264, None, 1000, 1080);
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
