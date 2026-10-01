use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::event::{Progress, ProgressSender};
use crate::ffmpeg::{
    Capabilities, Encoder, EncoderSet, Ffmpeg, FfmpegError, Hardware, software_encoder,
};
use crate::media::{
    AudioCodec, Container, FieldOrder, HdrFormat, LocalFile, MediaInfo, MediaKind, Projection,
    StereoLayout, VideoCodec, VideoTrack,
};
use crate::resolve::ClipRange;

#[async_trait]
pub trait Transcoder: Send + Sync {
    /// Reads a source the way the transcoder needs it: its streams, and what its frames
    /// say that its headers do not.
    async fn probe(&self, path: &Path) -> Result<MediaInfo, TranscodeError>;
    async fn transcode(
        &self,
        input: &LocalFile,
        target: &Target,
        dest_dir: &Path,
        progress: ProgressSender,
    ) -> Result<Transcoded, TranscodeError>;
}

/// The output, and what was done on the way to it, for the job's log.
#[derive(Debug, Clone, PartialEq)]
pub struct Transcoded {
    pub file: LocalFile,
    pub notes: Vec<String>,
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

/// The subtitles rendered into the picture.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum BurnSource {
    /// A subtitle file beside the source: SubRip, WebVTT or ASS.
    File { path: PathBuf, label: String },
    /// A subtitle stream inside the source: the `index`th stream of the file, the
    /// `position`th among its subtitle streams.
    Embedded {
        index: usize,
        position: usize,
        bitmap: bool,
        label: String,
    },
}

impl BurnSource {
    pub fn label(&self) -> &str {
        match self {
            BurnSource::File { label, .. } | BurnSource::Embedded { label, .. } => label,
        }
    }
}

/// The picture sound alone plays over when it is published as video.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum StillSource {
    /// The cover art inside the source.
    Cover,
    /// A picture file: the platform's thumbnail.
    Picture { path: PathBuf },
    /// A waveform of the sound, drawn by the transcoder.
    Waveform,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoTarget {
    pub container: Container,
    pub video: VideoCodec,
    pub audio: Option<AudioCodec>,
    pub max_bytes: u64,
    pub max_height: u32,
    /// The most frames per second the output keeps.
    #[serde(default)]
    pub max_fps: Option<u32>,
    /// The portion of the input to keep.
    #[serde(default)]
    pub clip: Option<ClipRange>,
    /// Subtitles to render into the picture.
    #[serde(default)]
    pub burn: Option<BurnSource>,
    /// A picture to play the source's sound over: the source is sound alone.
    #[serde(default)]
    pub still: Option<StillSource>,
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
            max_fps: None,
            clip: None,
            burn: None,
            still: None,
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
    #[error("this ffmpeg build has no {encoder} to encode {codec}")]
    MissingEncoder { codec: String, encoder: String },
    #[error("this ffmpeg build cannot render the source: {0}")]
    UnsupportedSource(String),
    #[error("cannot fit {duration_secs}s of video under {max_bytes} bytes")]
    BudgetUnreachable { max_bytes: u64, duration_secs: u64 },
    #[error("cannot fit {duration_secs}s of audio under {max_bytes} bytes")]
    AudioBudgetUnreachable { max_bytes: u64, duration_secs: u64 },
    #[error("cannot fit the picture under {max_bytes} bytes")]
    ImageBudgetUnreachable { max_bytes: u64 },
    #[error(
        "File size is {size} bytes. The destination allows {max_bytes} bytes. This file cannot be reduced."
    )]
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

const SIZE_MARGIN: f64 = 0.95;
const MIN_BITS_PER_PIXEL: f64 = 0.03;
const MAX_BITS_PER_PIXEL: f64 = 0.16;
const MIN_VIDEO_BPS: f64 = 40_000.0;
const SHORT_SIDES: [u32; 6] = [1080, 720, 480, 360, 240, 144];
const AUDIO_BPS: [u64; 5] = [128_000, 96_000, 64_000, 48_000, 32_000];
const ATTEMPTS: usize = 4;
/// The widest frame rate any destination is fed without being told otherwise.
const DEFAULT_MAX_FPS: f64 = 60.0;
/// The horizontal field of view a flat rendering of a sphere shows.
const FLAT_H_FOV: f64 = 100.0;
/// The frame rate a still picture plays at under sound.
const STILL_FPS: u32 = 10;
/// The bits a still picture is allowed beside the sound.
const STILL_VIDEO_BPS: u64 = 80_000;

impl From<FfmpegError> for TranscodeError {
    fn from(error: FfmpegError) -> Self {
        match error {
            FfmpegError::Probe(m) => TranscodeError::Probe(m),
            FfmpegError::Io(e) => TranscodeError::Io(e),
            other => TranscodeError::Process(other.to_string()),
        }
    }
}

/// Encodes with ffmpeg: remuxes when the streams already fit the target, otherwise runs
/// an encode sized to the byte budget, stepping down resolution, frame rate, and audio
/// bitrate as needed. Tone-maps HDR, deinterlaces, flattens 360° pictures, steadies
/// variable frame rates, burns subtitles in, and plays sound alone over a still.
pub struct FfmpegTranscoder {
    ffmpeg: Ffmpeg,
}

impl FfmpegTranscoder {
    pub fn new(ffmpeg: Ffmpeg) -> Self {
        Self { ffmpeg }
    }
}

/// The encoders one video output runs on.
#[derive(Debug, Clone)]
struct Encoders {
    video: Encoder,
    audio: &'static str,
    faststart: bool,
    /// Whether the video encoder is run twice, the first pass sizing the second.
    two_pass: bool,
    vaapi_device: PathBuf,
}

/// The ffmpeg encoder for `codec`, when the build has it.
fn audio_encoder_for(
    codec: &AudioCodec,
    caps: &Capabilities,
) -> Result<&'static str, TranscodeError> {
    let name = match codec {
        AudioCodec::Aac => "aac",
        AudioCodec::Opus => "libopus",
        AudioCodec::Vorbis => "libvorbis",
        AudioCodec::Mp3 => "libmp3lame",
        AudioCodec::Flac => "flac",
        AudioCodec::Other(other) => {
            return Err(TranscodeError::MissingEncoder {
                codec: other.clone(),
                encoder: other.clone(),
            });
        }
    };
    if !caps.has_encoder(name) {
        return Err(TranscodeError::MissingEncoder {
            codec: format!("{codec:?}").to_lowercase(),
            encoder: name.to_string(),
        });
    }
    Ok(name)
}

fn software_video_encoder(
    target: &VideoTarget,
    caps: &Capabilities,
) -> Result<Encoder, TranscodeError> {
    software_encoder(&target.video, caps)
        .map(|name| Encoder {
            name: name.to_string(),
            hardware: None,
        })
        .ok_or_else(|| TranscodeError::MissingEncoder {
            codec: format!("{:?}", target.video).to_lowercase(),
            encoder: match target.video {
                VideoCodec::H264 => "libx264".into(),
                VideoCodec::H265 => "libx265".into(),
                VideoCodec::Vp9 => "libvpx-vp9".into(),
                VideoCodec::Vp8 => "libvpx".into(),
                VideoCodec::Av1 => "libsvtav1, librav1e or libaom-av1".into(),
                VideoCodec::Other(ref name) => name.clone(),
            },
        })
}

/// Whether an encoder is run in two passes: the software encoders that size a second
/// pass from a first.
fn takes_two_passes(encoder: &Encoder) -> bool {
    encoder.hardware.is_none()
        && matches!(
            encoder.name.as_str(),
            "libx264" | "libx265" | "libvpx" | "libvpx-vp9"
        )
}

fn encoders_for(
    target: &VideoTarget,
    set: &EncoderSet,
    caps: &Capabilities,
    vaapi_device: PathBuf,
) -> Result<Encoders, TranscodeError> {
    if !crate::publish::container_takes_video(&target.container, &target.video) {
        return Err(TranscodeError::UnsupportedTarget {
            container: target.container.clone(),
            video: target.video.clone(),
        });
    }
    let video = match set.for_codec(&target.video) {
        Some(encoder) => encoder.clone(),
        None => software_video_encoder(target, caps)?,
    };
    let audio = match target.audio.as_ref() {
        None => "",
        Some(codec) => {
            if !crate::publish::container_takes_audio(&target.container, codec) {
                return Err(TranscodeError::UnsupportedTarget {
                    container: target.container.clone(),
                    video: target.video.clone(),
                });
            }
            audio_encoder_for(codec, caps)?
        }
    };
    Ok(Encoders {
        two_pass: takes_two_passes(&video),
        video,
        audio,
        faststart: matches!(target.container, Container::Mp4 | Container::Mov),
        vaapi_device,
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
    /// The sizes and rates a picture of `width`×`height` at `fps` may be made in, largest
    /// first, capped by `max_height` on the short side and `max_fps`.
    fn new(
        width: u32,
        height: u32,
        fps: Option<f64>,
        max_height: u32,
        max_fps: Option<u32>,
        has_audio: bool,
    ) -> Self {
        let landscape = width >= height;
        let short = width.min(height).max(2);
        let long = width.max(height).max(2);
        let cap = short.min(max_height);
        let mut sides = vec![cap];
        sides.extend(SHORT_SIDES.iter().copied().filter(|s| *s < cap));
        let ceiling = max_fps.map_or(DEFAULT_MAX_FPS, |f| f as f64);
        let src_fps = fps.unwrap_or(30.0).clamp(1.0, ceiling);
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

fn even(n: u32) -> u32 {
    (n & !1).max(2)
}

/// The size a flat view of a sphere of `width` pixels around is rendered at: wide enough
/// to keep the detail the view covers, in 16:9.
fn flat_view_size(width: u32) -> (u32, u32) {
    let w = (width as f64 * FLAT_H_FOV / 360.0 * 1.5).round() as u32;
    let w = even(w.clamp(640, width.max(640)));
    (w, even((w as f64 * 9.0 / 16.0).round() as u32))
}

/// The vertical field of view of a flat picture `width`×`height` that shows `h_fov`
/// degrees across.
fn vertical_fov(h_fov: f64, width: u32, height: u32) -> f64 {
    let half = (h_fov.to_radians() / 2.0).tan() * height as f64 / width as f64;
    2.0 * half.atan().to_degrees()
}

/// The picture as the transcoder works on it: which stream, the size the filters leave
/// it at, and the filters that get it there from the decoded frames.
#[derive(Debug, Clone)]
struct Shape {
    index: usize,
    width: u32,
    height: u32,
    fps: Option<f64>,
    vfr: bool,
    /// The filters between the decoder and the scaler, as a graph fragment in which
    /// `{in}` and `{out}` stand for the labels around it.
    prefix: Vec<String>,
    /// The filters after the scaler: subtitles drawn at the output size.
    suffix: Vec<String>,
    /// The output colour tags, when the picture's own no longer hold.
    color_args: Vec<OsString>,
    notes: Vec<String>,
}

/// What the picture needs on its way to the destination, from what the probe found.
fn shape_of(
    video: &VideoTrack,
    burn: Option<&BurnSource>,
    input: &Path,
    caps: &Capabilities,
) -> Result<Shape, TranscodeError> {
    let mut prefix: Vec<String> = Vec::new();
    let mut suffix: Vec<String> = Vec::new();
    let mut notes = Vec::new();
    let mut color_args = Vec::new();
    let (mut width, mut height) = video.display_size();
    if video.sample_aspect.is_some() {
        notes.push(format!(
            "Squared the pixels: {}×{} shows as {width}×{height}.",
            video.width, video.height
        ));
    }
    if video.alpha {
        prefix.push(
            "split=2[bgsrc{n}][fg{n}];[bgsrc{n}]drawbox=c=black:t=fill,format=yuv420p[bg{n}];[bg{n}][fg{n}]overlay=format=auto:eof_action=pass"
                .to_string(),
        );
        notes.push("Laid the transparent picture over black.".into());
    }
    match video.field_order {
        FieldOrder::TopFirst => {
            prefix.push("bwdif=mode=send_frame:parity=tff:deint=all".into());
            notes.push("Deinterlaced the picture, top field first.".into());
        }
        FieldOrder::BottomFirst => {
            prefix.push("bwdif=mode=send_frame:parity=bff:deint=all".into());
            notes.push("Deinterlaced the picture, bottom field first.".into());
        }
        FieldOrder::Progressive | FieldOrder::Unknown => {}
    }
    if let Some(hdr) = video.hdr {
        let name = match hdr {
            HdrFormat::Pq => "HDR10 (PQ)".to_string(),
            HdrFormat::Hlg => "HLG".to_string(),
            HdrFormat::DolbyVision { profile } => format!("Dolby Vision profile {profile}"),
        };
        let needs_placebo = matches!(hdr, HdrFormat::DolbyVision { profile: 5 });
        if needs_placebo {
            if !caps.has_filter("libplacebo") {
                return Err(TranscodeError::UnsupportedSource(format!(
                    "{name} carries no HDR10 or HLG base layer, and rendering it needs an ffmpeg build with the libplacebo filter"
                )));
            }
            prefix.push(
                "libplacebo=tonemapping=auto:colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv:format=yuv420p"
                    .into(),
            );
        } else if caps.has_filter("zscale") && caps.has_filter("tonemap") {
            prefix.push(
                "zscale=t=linear:npl=100,format=gbrpf32le,zscale=p=bt709,tonemap=tonemap=hable:desat=0,zscale=t=bt709:m=bt709:r=tv,format=yuv420p"
                    .into(),
            );
        } else if caps.has_filter("libplacebo") {
            prefix.push(
                "libplacebo=tonemapping=auto:colorspace=bt709:color_primaries=bt709:color_trc=bt709:range=tv:format=yuv420p"
                    .into(),
            );
        } else {
            return Err(TranscodeError::UnsupportedSource(format!(
                "tone-mapping {name} needs the zscale and tonemap filters, or libplacebo, and this ffmpeg build has none of them"
            )));
        }
        color_args.extend(
            [
                "-color_primaries",
                "bt709",
                "-color_trc",
                "bt709",
                "-colorspace",
                "bt709",
                "-color_range",
                "tv",
            ]
            .map(OsString::from),
        );
        notes.push(format!("Tone-mapped {name} to SDR."));
    }
    match (video.projection, video.stereo) {
        (Some(projection), stereo) => {
            if !caps.has_filter("v360") {
                return Err(TranscodeError::UnsupportedSource(
                    "flattening a 360° picture needs the v360 filter, which this ffmpeg build lacks"
                        .into(),
                ));
            }
            let input_name = match projection {
                Projection::Equirectangular => "e".to_string(),
                Projection::EquirectangularTile {
                    left,
                    top,
                    right,
                    bottom,
                } => {
                    let full_w = (width as f64 / (right - left).max(0.01) as f64).round() as u32;
                    let full_h = (height as f64 / (bottom - top).max(0.01) as f64).round() as u32;
                    let x = (full_w as f64 * left as f64).round() as u32;
                    let y = (full_h as f64 * top as f64).round() as u32;
                    prefix.push(format!(
                        "pad={}:{}:{x}:{y}:color=black",
                        even(full_w),
                        even(full_h)
                    ));
                    width = even(full_w);
                    "e".to_string()
                }
                Projection::Cubemap { padding } => {
                    let face = (width as f64 / 3.0).max(1.0);
                    format!("c3x2:in_pad={:.4}", (padding as f64 / face).min(0.1))
                }
                Projection::EquiAngularCubemap => "eac".to_string(),
            };
            let (fw, fh) = flat_view_size(width);
            let v_fov = vertical_fov(FLAT_H_FOV, fw, fh);
            let (yaw, pitch, roll) = video.view.unwrap_or((0.0, 0.0, 0.0));
            let mut filter = format!(
                "v360=input={input_name}:output=flat:h_fov={FLAT_H_FOV}:v_fov={v_fov:.2}:w={fw}:h={fh}:yaw={yaw:.2}:pitch={pitch:.2}:roll={roll:.2}:interp=lanczos"
            );
            match stereo {
                Some(StereoLayout::SideBySide) => filter.push_str(":in_stereo=sbs:out_stereo=2d"),
                Some(StereoLayout::TopBottom) => filter.push_str(":in_stereo=tb:out_stereo=2d"),
                None => {}
            }
            prefix.push(filter);
            width = fw;
            height = fh;
            let layout = match projection {
                Projection::Equirectangular | Projection::EquirectangularTile { .. } => {
                    "equirectangular"
                }
                Projection::Cubemap { .. } => "cubemap",
                Projection::EquiAngularCubemap => "equi-angular cubemap",
            };
            notes.push(format!(
                "Rendered a {FLAT_H_FOV:.0}° view of the {layout} 360° picture{}.",
                if stereo.is_some() {
                    " from one eye"
                } else {
                    ""
                }
            ));
        }
        (None, Some(stereo)) => {
            let (filter, name) = match stereo {
                StereoLayout::SideBySide => ("stereo3d=sbsl:ml", "side-by-side"),
                StereoLayout::TopBottom => ("stereo3d=abl:ml", "top-and-bottom"),
            };
            prefix.push(filter.into());
            match stereo {
                StereoLayout::SideBySide => width = even(width / 2),
                StereoLayout::TopBottom => height = even(height / 2),
            }
            notes.push(format!("Kept the left eye of the {name} stereo picture."));
        }
        (None, None) => {}
    }
    match burn {
        Some(BurnSource::Embedded {
            index,
            bitmap: true,
            label,
            ..
        }) => {
            prefix.push(format!("[0:{index}]overlay=eof_action=pass"));
            notes.push(format!("Burned in the {label} subtitles."));
        }
        Some(BurnSource::Embedded {
            position,
            bitmap: false,
            label,
            ..
        }) => {
            suffix.push(format!(
                "subtitles=filename={}:si={position}",
                filter_path(input)
            ));
            notes.push(format!("Burned in the {label} subtitles."));
        }
        Some(BurnSource::File { path, label }) => {
            suffix.push(format!("subtitles=filename={}", filter_path(path)));
            notes.push(format!("Burned in the {label} subtitles."));
        }
        None => {}
    }
    Ok(Shape {
        index: video.index,
        width,
        height,
        fps: video.fps,
        vfr: video.vfr,
        prefix,
        suffix,
        color_args,
        notes,
    })
}

/// The `-filter_complex` graph from the decoded picture to the encoder's input label
/// `[v]`, for one size and rate.
fn video_graph(shape: &Shape, step: &Step, hardware: Option<Hardware>) -> String {
    let mut graph = String::new();
    let mut current = format!("[0:{}]", shape.index);
    let mut n = 0;
    let mut push = |graph: &mut String, current: &mut String, filter: &str| {
        let next = format!("[s{n}]");
        let filter = filter.replace("{n}", &n.to_string());
        if let Some(rest) = filter.strip_prefix('[') {
            // A filter that takes a second input names it first: `[0:3]overlay=...`.
            let (label, rest) = rest.split_once(']').unwrap_or(("", rest));
            graph.push_str(&format!("{current}[{label}]{rest}{next};"));
        } else {
            graph.push_str(&format!("{current}{filter}{next};"));
        }
        *current = next;
        n += 1;
    };
    for filter in &shape.prefix {
        push(&mut graph, &mut current, filter);
    }
    push(
        &mut graph,
        &mut current,
        &format!(
            "scale={}:{}:flags=lanczos,setsar=1",
            step.width, step.height
        ),
    );
    if shape.vfr || shape.fps.is_some_and(|src| step.fps + 0.5 < src) {
        push(
            &mut graph,
            &mut current,
            &format!("fps=fps={:.4}", step.fps),
        );
    }
    for filter in &shape.suffix {
        push(&mut graph, &mut current, filter);
    }
    let last = match hardware.map(Hardware::upload_filters) {
        Some(upload) if !upload.is_empty() => upload.to_string(),
        _ => "format=yuv420p".to_string(),
    };
    graph.push_str(&format!("{current}{last}[v]"));
    graph
}

/// `-ss`/`-t` options for a clip.
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

/// The encoder's own options: rate control, speed and profile, and the pass it runs.
fn encoder_args(
    encoder: &Encoder,
    pass: Option<u8>,
    log_prefix: &Path,
    video_bps: u64,
    still: bool,
) -> Vec<OsString> {
    let maxrate = (video_bps as f64 * 1.3) as u64;
    let bufsize = video_bps * 2;
    let mut a: Vec<OsString> = vec!["-c:v".into(), encoder.name.clone().into()];
    let rate = |a: &mut Vec<OsString>| {
        a.extend([
            "-b:v".into(),
            video_bps.to_string().into(),
            "-maxrate".into(),
            maxrate.to_string().into(),
            "-bufsize".into(),
            bufsize.to_string().into(),
        ]);
    };
    match encoder.hardware {
        Some(Hardware::Nvenc) => {
            a.extend(["-preset".into(), "p5".into(), "-rc".into(), "vbr".into()]);
            rate(&mut a);
        }
        Some(Hardware::Vaapi) => {
            a.extend(["-rc_mode".into(), "VBR".into()]);
            rate(&mut a);
        }
        Some(Hardware::Amf) => {
            a.extend(["-rc".into(), "vbr_peak".into()]);
            rate(&mut a);
        }
        Some(Hardware::Qsv) => {
            a.extend(["-preset".into(), "medium".into()]);
            rate(&mut a);
        }
        Some(Hardware::Videotoolbox) | Some(Hardware::V4l2m2m) => rate(&mut a),
        None => match encoder.name.as_str() {
            "libx264" => {
                a.extend([
                    "-preset".into(),
                    "medium".into(),
                    "-profile:v".into(),
                    "high".into(),
                ]);
                if still {
                    a.extend([
                        "-tune".into(),
                        "stillimage".into(),
                        "-crf".into(),
                        "26".into(),
                        "-maxrate".into(),
                        maxrate.to_string().into(),
                        "-bufsize".into(),
                        bufsize.to_string().into(),
                    ]);
                } else {
                    a.extend(["-b:v".into(), video_bps.to_string().into()]);
                    if pass == Some(2) || pass.is_none() {
                        a.extend([
                            "-maxrate".into(),
                            maxrate.to_string().into(),
                            "-bufsize".into(),
                            bufsize.to_string().into(),
                        ]);
                    }
                }
            }
            "libx265" => {
                a.extend([
                    "-preset".into(),
                    "medium".into(),
                    "-x265-params".into(),
                    "log-level=error".into(),
                    "-b:v".into(),
                    video_bps.to_string().into(),
                ]);
                if pass == Some(2) || pass.is_none() {
                    a.extend([
                        "-maxrate".into(),
                        maxrate.to_string().into(),
                        "-bufsize".into(),
                        bufsize.to_string().into(),
                    ]);
                }
            }
            "libvpx-vp9" | "libvpx" => {
                a.extend([
                    "-b:v".into(),
                    video_bps.to_string().into(),
                    "-row-mt".into(),
                    "1".into(),
                    "-deadline".into(),
                    "good".into(),
                    "-cpu-used".into(),
                    if pass == Some(1) {
                        "4".into()
                    } else {
                        "2".into()
                    },
                ]);
                if pass == Some(2) || pass.is_none() {
                    a.extend([
                        "-maxrate".into(),
                        maxrate.to_string().into(),
                        "-bufsize".into(),
                        bufsize.to_string().into(),
                    ]);
                }
            }
            "libsvtav1" => {
                a.extend(["-preset".into(), "8".into()]);
                rate(&mut a);
            }
            "librav1e" => {
                a.extend(["-speed".into(), "8".into()]);
                rate(&mut a);
            }
            "libaom-av1" => {
                a.extend(["-cpu-used".into(), "6".into(), "-row-mt".into(), "1".into()]);
                rate(&mut a);
            }
            _ => rate(&mut a),
        },
    }
    if let Some(pass) = pass {
        a.extend([
            "-pass".into(),
            pass.to_string().into(),
            "-passlogfile".into(),
            log_prefix.as_os_str().to_owned(),
        ]);
    }
    a
}

fn audio_args(encoder: &str, bps: u64) -> Vec<OsString> {
    let mut a: Vec<OsString> = vec![
        "-c:a".into(),
        encoder.into(),
        "-ac".into(),
        "2".into(),
        "-ar".into(),
        "48000".into(),
    ];
    if encoder != "flac" && bps > 0 {
        a.extend(["-b:a".into(), bps.to_string().into()]);
    }
    a
}

fn remux_mode(info: &MediaInfo, size: u64, target: &VideoTarget) -> Option<bool> {
    let video = info.video.as_ref()?;
    if target.clip.is_some() || target.burn.is_some() || target.still.is_some() {
        return None;
    }
    if video.codec != target.video || size > target.max_bytes {
        return None;
    }
    if video.needs_processing() {
        return None;
    }
    let (width, height) = video.display_size();
    if width.min(height) > target.max_height {
        return None;
    }
    if let (Some(max_fps), Some(fps)) = (target.max_fps, video.fps)
        && fps > max_fps as f64 + 0.5
    {
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

/// Progress over `passes` runs: the fraction done before this pass, and this pass's share.
fn report(progress: &ProgressSender, total: u64, passes: u64, pass: u64, t: Duration) {
    let per_pass = total / passes.max(1);
    progress.send_replace(Progress::of(
        (per_pass * pass + (t.as_micros() as u64) / passes.max(1)).min(total),
        Some(total),
    ));
}

impl FfmpegTranscoder {
    async fn probe_output(&self, output: &Path) -> Result<LocalFile, TranscodeError> {
        let mut file = LocalFile::from_path(output.to_path_buf()).await?;
        file.info = Some(self.ffmpeg.probe(output).await?);
        Ok(file)
    }

    async fn remux(
        &self,
        input: &LocalFile,
        info: &MediaInfo,
        plan: &RemuxPlan,
        output: &Path,
        progress: &ProgressSender,
    ) -> Result<LocalFile, TranscodeError> {
        let duration = info.duration.unwrap_or_default();
        let video_index = info.video.as_ref().map_or(0, |v| v.index);
        let mut args: Vec<OsString> = vec![
            "-loglevel".into(),
            "warning".into(),
            "-i".into(),
            input.path.as_os_str().to_owned(),
            "-map".into(),
            format!("0:{video_index}").into(),
            "-c:v".into(),
            "copy".into(),
        ];
        match (&info.audio, plan.wants_audio) {
            (Some(audio), true) if plan.copy_audio => {
                args.extend([
                    "-map".into(),
                    format!("0:{}", audio.index).into(),
                    "-c:a".into(),
                    "copy".into(),
                ]);
            }
            (Some(audio), true) => {
                args.extend(["-map".into(), format!("0:{}", audio.index).into()]);
                args.extend(audio_args(plan.audio_encoder, 128_000));
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
            .run(args, move |t| report(&reporter, total, 1, 0, t))
            .await?;
        self.probe_output(output).await
    }

    /// One encode of the picture at `step`: two passes for the software encoders that
    /// size a second pass from a first, one for the rest.
    #[allow(clippy::too_many_arguments)]
    async fn encode(
        &self,
        input: &LocalFile,
        shape: &Shape,
        audio_index: Option<usize>,
        encoders: &Encoders,
        step: &Step,
        output: &Path,
        log_prefix: &Path,
        progress: &ProgressSender,
        duration: Duration,
        target: &VideoTarget,
    ) -> Result<LocalFile, TranscodeError> {
        let total = duration.as_micros() as u64;
        let graph = video_graph(shape, step, encoders.video.hardware);
        let clip = clip_args(target.clip);
        // Subtitles are timed against the source, so a clip with subtitles burnt in is
        // cut on the way out, after they are drawn, rather than by seeking the input.
        let seek_output = target.burn.is_some();
        let hardware_args = encoders
            .video
            .hardware
            .map(|hw| hw.device_args(&encoders.vaapi_device))
            .unwrap_or_default();
        let common = |pass: Option<u8>| -> Vec<OsString> {
            let mut a: Vec<OsString> = vec!["-loglevel".into(), "warning".into()];
            a.extend(hardware_args.iter().cloned());
            if !seek_output {
                a.extend(clip.iter().cloned());
            }
            a.extend([
                "-i".into(),
                input.path.as_os_str().to_owned(),
                "-filter_complex".into(),
                graph.clone().into(),
                "-map".into(),
                "[v]".into(),
                // The same frames in every pass, whatever the muxer would do on its own.
                "-fps_mode".into(),
                "cfr".into(),
            ]);
            if seek_output {
                a.extend(clip.iter().cloned());
            }
            a.extend(encoder_args(
                &encoders.video,
                pass,
                log_prefix,
                step.video_bps,
                false,
            ));
            a.extend(shape.color_args.iter().cloned());
            a
        };
        let passes = if encoders.two_pass { 2 } else { 1 };
        if encoders.two_pass {
            let mut pass1 = common(Some(1));
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
                .run(pass1, move |t| report(&reporter, total, passes, 0, t))
                .await?;
        }
        let mut last = common(encoders.two_pass.then_some(2));
        match audio_index {
            Some(index) if step.audio_bps > 0 => {
                last.extend(["-map".into(), format!("0:{index}").into()]);
                last.extend(audio_args(encoders.audio, step.audio_bps));
            }
            _ => last.push("-an".into()),
        }
        last.extend(["-sn".into(), "-dn".into()]);
        if encoders.faststart {
            last.extend(["-movflags".into(), "+faststart".into()]);
        }
        last.push(output.as_os_str().to_owned());
        let reporter = progress.clone();
        self.ffmpeg
            .run(last, move |t| {
                report(&reporter, total, passes, passes - 1, t)
            })
            .await?;
        self.probe_output(output).await
    }
}

/// The bitrates audio alone is tried at, highest first.
const AUDIO_ONLY_BPS: [u64; 8] = [
    192_000, 160_000, 128_000, 96_000, 64_000, 48_000, 32_000, 24_000,
];
/// The long edges an image is scaled down to, in turn, when it is over budget.
const IMAGE_EDGES: [u32; 10] = [4096, 3072, 2048, 1600, 1280, 1024, 800, 640, 480, 320];
/// JPEG `-q:v` levels, best first. WebP quality falls with them.
const JPEG_QUALITIES: [u32; 5] = [2, 4, 7, 12, 20];

fn audio_encoder(
    target: &AudioTarget,
    caps: &Capabilities,
) -> Result<&'static str, TranscodeError> {
    let fits = matches!(
        (&target.container, &target.codec),
        (Container::M4a | Container::Mp4, AudioCodec::Aac)
            | (Container::Mp3, AudioCodec::Mp3)
            | (
                Container::Ogg | Container::Opus | Container::Webm,
                AudioCodec::Opus
            )
            | (Container::Ogg, AudioCodec::Vorbis)
            | (Container::Flac, AudioCodec::Flac)
    );
    if !fits {
        return Err(TranscodeError::UnsupportedAudioTarget {
            container: target.container.clone(),
            codec: target.codec.clone(),
        });
    }
    audio_encoder_for(&target.codec, caps)
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
    ) -> Result<Transcoded, TranscodeError> {
        let audio = info.audio.as_ref().ok_or(TranscodeError::NoAudio)?;
        let duration = clipped_duration(
            target.clip,
            info.duration.ok_or(TranscodeError::NoDuration)?,
        );
        if duration.is_zero() {
            return Err(TranscodeError::NoDuration);
        }
        let caps = self.ffmpeg.capabilities();
        let encoder = audio_encoder(target, &caps)?;
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
                format!("0:{}", audio.index).into(),
                "-vn".into(),
                "-sn".into(),
                "-dn".into(),
            ]);
            args.extend(audio_args(encoder, bps));
            if let Some(muxer) = audio_muxer(&target.container) {
                args.extend(["-f".into(), muxer.into()]);
            }
            if target.container == Container::M4a {
                args.extend(["-movflags".into(), "+faststart".into()]);
            }
            args.push(output.as_os_str().to_owned());
            let reporter = progress.clone();
            self.ffmpeg
                .run(args, move |t| report(&reporter, total, 1, 0, t))
                .await?;
            let file = LocalFile::from_path(output.clone()).await?;
            if file.size <= target.max_bytes {
                let file = self.probe_output(&output).await?;
                return Ok(Transcoded {
                    file,
                    notes: vec![format!(
                        "Encoded the sound with {encoder} at {} kb/s.",
                        bps / 1000
                    )],
                });
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
    ) -> Result<Transcoded, TranscodeError> {
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
                progress.send_replace(Progress::of(done, Some(attempts as u64)));
                let mut args: Vec<OsString> = vec![
                    "-loglevel".into(),
                    "warning".into(),
                    "-i".into(),
                    input.path.as_os_str().to_owned(),
                    "-map".into(),
                    format!("0:{}", picture.index).into(),
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
                        // libwebp takes 0..100, best last. Map the JPEG scale onto it.
                        let quality = 100u32.saturating_sub(q * 4);
                        args.extend(["-quality".into(), quality.to_string().into()]);
                    }
                    Some(option) => args.extend([option.into(), q.to_string().into()]),
                    None => {}
                }
                args.push(output.as_os_str().to_owned());
                self.ffmpeg.run(args, |_| {}).await?;
                let file = LocalFile::from_path(output.clone()).await?;
                if file.size <= target.max_bytes {
                    let file = self.probe_output(&output).await?;
                    return Ok(Transcoded {
                        file,
                        notes: vec![format!(
                            "Encoded the picture at {w}×{h} with {}.",
                            encoder.codec
                        )],
                    });
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
    ) -> Result<Transcoded, TranscodeError> {
        if let Some(still) = &target.still {
            return self
                .transcode_still(input, info, target, still, dest_dir, progress)
                .await;
        }
        let video = info.video.clone().ok_or(TranscodeError::NoVideo)?;
        let duration = target.output_duration(info.duration.ok_or(TranscodeError::NoDuration)?);
        if duration.is_zero() {
            return Err(TranscodeError::NoDuration);
        }
        let caps = self.ffmpeg.capabilities();
        let set = self.ffmpeg.encoders();
        let mut encoders = encoders_for(target, &set, &caps, self.ffmpeg.vaapi_device())?;
        tokio::fs::create_dir_all(dest_dir).await?;
        let output = dest_dir.join(format!("output.{}", target.container.extension()));
        let mut notes = Vec::new();

        if let Some(copy_audio) = remux_mode(info, input.size, target) {
            let plan = RemuxPlan {
                wants_audio: target.audio.is_some(),
                copy_audio,
                audio_encoder: encoders.audio,
                faststart: encoders.faststart,
            };
            match self.remux(input, info, &plan, &output, progress).await {
                Ok(file) if file.size <= target.max_bytes => {
                    notes.push(if copy_audio {
                        "Copied the streams into the container without re-encoding.".to_string()
                    } else {
                        format!(
                            "Copied the picture and re-encoded the sound with {}.",
                            encoders.audio
                        )
                    });
                    return Ok(Transcoded { file, notes });
                }
                Ok(file) => {
                    tracing::info!(size = file.size, "remux exceeds budget, encoding instead");
                }
                Err(error) => {
                    tracing::warn!("remux failed, encoding instead: {error}");
                }
            }
            let _ = tokio::fs::remove_file(&output).await;
        }

        let shape = shape_of(&video, target.burn.as_ref(), &input.path, &caps)?;
        notes.extend(shape.notes.iter().cloned());
        if video.vfr {
            notes.push("Steadied the variable frame rate.".into());
        }
        let secs = duration.as_secs_f64().max(0.1);
        let unreachable = || TranscodeError::BudgetUnreachable {
            max_bytes: target.max_bytes,
            duration_secs: duration.as_secs(),
        };
        let has_audio = info.audio.is_some() && target.audio.is_some();
        let audio_index = info.audio.as_ref().map(|a| a.index);
        let ladder = Ladder::new(
            shape.width,
            shape.height,
            shape.fps,
            target.max_height,
            target.max_fps,
            has_audio,
        );
        let mut total_bps = target.max_bytes as f64 * 8.0 * SIZE_MARGIN / secs;
        let log_prefix = dest_dir.join("passlog");
        let mut attempt = 0;
        let mut fell_back = false;
        while attempt < ATTEMPTS {
            attempt += 1;
            let step = ladder.pick(total_bps).ok_or_else(unreachable)?;
            tracing::info!(
                attempt,
                width = step.width,
                height = step.height,
                fps = step.fps,
                video_bps = step.video_bps,
                audio_bps = step.audio_bps,
                encoder = encoders.video.name,
                "encoding"
            );
            let result = self
                .encode(
                    input,
                    &shape,
                    audio_index,
                    &encoders,
                    &step,
                    &output,
                    &log_prefix,
                    progress,
                    duration,
                    target,
                )
                .await;
            let file = match result {
                Ok(file) => file,
                Err(TranscodeError::Process(message))
                    if encoders.video.hardware.is_some() && !fell_back =>
                {
                    let software = software_video_encoder(target, &caps)?;
                    tracing::warn!(
                        encoder = encoders.video.name,
                        "hardware encode failed, encoding in software: {message}"
                    );
                    notes.push(format!(
                        "{} failed ({message}). Encoded in software with {} instead.",
                        encoders.video.name, software.name
                    ));
                    encoders.two_pass = takes_two_passes(&software);
                    encoders.video = software;
                    fell_back = true;
                    attempt -= 1;
                    let _ = tokio::fs::remove_file(&output).await;
                    continue;
                }
                Err(error) => return Err(error),
            };
            if file.size <= target.max_bytes {
                notes.push(format!(
                    "Encoded {}×{} at {:.4} fps with {}: {} kb/s video{}.",
                    step.width,
                    step.height,
                    step.fps,
                    encoders.video.name,
                    step.video_bps / 1000,
                    if step.audio_bps > 0 {
                        format!(", {} kb/s {} audio", step.audio_bps / 1000, encoders.audio)
                    } else {
                        String::new()
                    }
                ));
                return Ok(Transcoded { file, notes });
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

    /// The picture file sound is played over, made under `dest_dir` when it has to be.
    async fn still_picture(
        &self,
        input: &LocalFile,
        info: &MediaInfo,
        still: &StillSource,
        dest_dir: &Path,
    ) -> Result<(PathBuf, String), TranscodeError> {
        match still {
            StillSource::Picture { path } => {
                Ok((path.clone(), "the platform's thumbnail".to_string()))
            }
            StillSource::Cover => {
                let cover = info.cover.as_ref().ok_or(TranscodeError::NoPicture)?;
                let path = dest_dir.join("still.png");
                let args: Vec<OsString> = vec![
                    "-loglevel".into(),
                    "warning".into(),
                    "-i".into(),
                    input.path.as_os_str().to_owned(),
                    "-map".into(),
                    format!("0:{}", cover.index).into(),
                    "-frames:v".into(),
                    "1".into(),
                    "-an".into(),
                    "-sn".into(),
                    "-dn".into(),
                    "-c:v".into(),
                    "png".into(),
                    path.as_os_str().to_owned(),
                ];
                self.ffmpeg.run(args, |_| {}).await?;
                Ok((path, "the cover art".to_string()))
            }
            StillSource::Waveform => {
                let audio = info.audio.as_ref().ok_or(TranscodeError::NoAudio)?;
                let path = dest_dir.join("still.png");
                let graph = format!(
                    "color=c=0x14151a:s=1280x720:r=1[bg];[0:{}]showwavespic=s=1280x360:colors=0xf2542d|0x2ec4b6:draw=full[w];[bg][w]overlay=0:180:format=auto[v]",
                    audio.index
                );
                let args: Vec<OsString> = vec![
                    "-loglevel".into(),
                    "warning".into(),
                    "-i".into(),
                    input.path.as_os_str().to_owned(),
                    "-filter_complex".into(),
                    graph.into(),
                    "-map".into(),
                    "[v]".into(),
                    "-frames:v".into(),
                    "1".into(),
                    "-c:v".into(),
                    "png".into(),
                    path.as_os_str().to_owned(),
                ];
                self.ffmpeg.run(args, |_| {}).await?;
                Ok((path, "a waveform of the sound".to_string()))
            }
        }
    }

    /// Sound alone as a video: the picture held for the length of the sound.
    async fn transcode_still(
        &self,
        input: &LocalFile,
        info: &MediaInfo,
        target: &VideoTarget,
        still: &StillSource,
        dest_dir: &Path,
        progress: &ProgressSender,
    ) -> Result<Transcoded, TranscodeError> {
        let audio = info.audio.as_ref().ok_or(TranscodeError::NoAudio)?;
        let duration = target.output_duration(info.duration.ok_or(TranscodeError::NoDuration)?);
        if duration.is_zero() {
            return Err(TranscodeError::NoDuration);
        }
        let caps = self.ffmpeg.capabilities();
        let set = self.ffmpeg.encoders();
        let audio_codec = target.audio.clone().ok_or(TranscodeError::NoAudio)?;
        let mut encoders = encoders_for(
            &VideoTarget {
                audio: Some(audio_codec),
                ..target.clone()
            },
            &set,
            &caps,
            self.ffmpeg.vaapi_device(),
        )?;
        // A held picture is what the software encoders' still-image tuning is for.
        if let Ok(software) = software_video_encoder(target, &caps) {
            encoders.video = software;
        }
        encoders.two_pass = false;
        tokio::fs::create_dir_all(dest_dir).await?;
        let (picture, picture_name) = self.still_picture(input, info, still, dest_dir).await?;
        let output = dest_dir.join(format!("output.{}", target.container.extension()));
        let height = even(720.min(target.max_height));
        let width = even((height as f64 * 16.0 / 9.0).round() as u32);
        let graph = format!(
            "[0:v]scale={width}:{height}:force_original_aspect_ratio=decrease:flags=lanczos,pad={width}:{height}:(ow-iw)/2:(oh-ih)/2:color=black,setsar=1,{}[v]",
            match encoders.video.hardware.map(Hardware::upload_filters) {
                Some(upload) if !upload.is_empty() => upload,
                _ => "format=yuv420p",
            }
        );
        let secs = duration.as_secs_f64().max(0.1);
        let unreachable = || TranscodeError::BudgetUnreachable {
            max_bytes: target.max_bytes,
            duration_secs: duration.as_secs(),
        };
        let total = duration.as_micros() as u64;
        let mut budget_bps = target.max_bytes as f64 * 8.0 * SIZE_MARGIN / secs;
        for attempt in 1..=ATTEMPTS {
            let audio_bps = AUDIO_ONLY_BPS
                .iter()
                .copied()
                .find(|b| (*b as f64) + STILL_VIDEO_BPS as f64 <= budget_bps)
                .ok_or_else(unreachable)?;
            tracing::info!(
                attempt,
                audio_bps,
                encoder = encoders.video.name,
                "encoding sound over a still"
            );
            let mut args: Vec<OsString> = vec!["-loglevel".into(), "warning".into()];
            if let Some(hw) = encoders.video.hardware {
                args.extend(hw.device_args(&encoders.vaapi_device));
            }
            args.extend([
                "-loop".into(),
                "1".into(),
                "-framerate".into(),
                STILL_FPS.to_string().into(),
                "-i".into(),
                picture.as_os_str().to_owned(),
            ]);
            args.extend(clip_args(target.clip));
            args.extend([
                "-i".into(),
                input.path.as_os_str().to_owned(),
                "-filter_complex".into(),
                graph.clone().into(),
                "-map".into(),
                "[v]".into(),
                "-map".into(),
                format!("1:{}", audio.index).into(),
            ]);
            args.extend(encoder_args(
                &encoders.video,
                None,
                &dest_dir.join("passlog"),
                STILL_VIDEO_BPS,
                true,
            ));
            args.extend(["-r".into(), STILL_FPS.to_string().into()]);
            args.extend(audio_args(encoders.audio, audio_bps));
            args.extend(["-sn".into(), "-dn".into(), "-shortest".into()]);
            if encoders.faststart {
                args.extend(["-movflags".into(), "+faststart".into()]);
            }
            args.push(output.as_os_str().to_owned());
            let reporter = progress.clone();
            self.ffmpeg
                .run(args, move |t| report(&reporter, total, 1, 0, t))
                .await?;
            let file = LocalFile::from_path(output.clone()).await?;
            if file.size <= target.max_bytes {
                let file = self.probe_output(&output).await?;
                return Ok(Transcoded {
                    file,
                    notes: vec![format!(
                        "Played the sound over {picture_name} at {width}×{height}, encoded with {} and {} kb/s {} audio.",
                        encoders.video.name,
                        audio_bps / 1000,
                        encoders.audio
                    )],
                });
            }
            let ratio = target.max_bytes as f64 * SIZE_MARGIN / file.size as f64;
            budget_bps = ((audio_bps + STILL_VIDEO_BPS) as f64 * ratio.min(0.97))
                .min((audio_bps + STILL_VIDEO_BPS) as f64 - 1.0);
            tracing::info!(
                size = file.size,
                ratio,
                "still over budget, lowering the sound's bitrate"
            );
        }
        Err(unreachable())
    }
}

#[async_trait]
impl Transcoder for FfmpegTranscoder {
    async fn probe(&self, path: &Path) -> Result<MediaInfo, TranscodeError> {
        Ok(self.ffmpeg.inspect(path).await?)
    }

    async fn transcode(
        &self,
        input: &LocalFile,
        target: &Target,
        dest_dir: &Path,
        progress: ProgressSender,
    ) -> Result<Transcoded, TranscodeError> {
        let info = match &input.info {
            Some(info) => info.clone(),
            None => self.ffmpeg.inspect(&input.path).await?,
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
    use crate::media::ColorInfo;

    fn track(width: u32, height: u32) -> VideoTrack {
        VideoTrack {
            codec: VideoCodec::H264,
            width,
            height,
            fps: Some(30.0),
            bitrate: None,
            index: 0,
            pix_fmt: Some("yuv420p".into()),
            color: ColorInfo::default(),
            hdr: None,
            field_order: FieldOrder::Progressive,
            sample_aspect: None,
            vfr: false,
            alpha: false,
            projection: None,
            stereo: None,
            view: None,
        }
    }

    fn caps(filters: &[&str], encoders: &[&str]) -> Capabilities {
        Capabilities {
            version: String::new(),
            encoders: encoders.iter().map(|s| s.to_string()).collect(),
            filters: filters.iter().map(|s| s.to_string()).collect(),
        }
    }

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
        let caps = caps(&[], &["aac", "libmp3lame", "libopus", "flac"]);
        let m4a = AudioTarget {
            container: Container::M4a,
            codec: AudioCodec::Aac,
            max_bytes: 1,
            clip: None,
        };
        assert_eq!(audio_encoder(&m4a, &caps).unwrap(), "aac");
        let mp3 = AudioTarget {
            container: Container::Mp3,
            codec: AudioCodec::Mp3,
            ..m4a.clone()
        };
        assert_eq!(audio_encoder(&mp3, &caps).unwrap(), "libmp3lame");
        let wrong = AudioTarget {
            container: Container::Mp3,
            codec: AudioCodec::Aac,
            ..m4a.clone()
        };
        assert!(matches!(
            audio_encoder(&wrong, &caps),
            Err(TranscodeError::UnsupportedAudioTarget { .. })
        ));
        let vorbis = AudioTarget {
            container: Container::Ogg,
            codec: AudioCodec::Vorbis,
            ..m4a
        };
        assert!(matches!(
            audio_encoder(&vorbis, &caps),
            Err(TranscodeError::MissingEncoder { ref encoder, .. }) if encoder == "libvorbis"
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
    }

    #[test]
    fn the_graph_scales_steadies_and_burns_in_order() {
        let caps = caps(&["zscale", "tonemap", "v360", "bwdif"], &["libx264"]);
        let video = track(1920, 1080);
        let burn = BurnSource::File {
            path: PathBuf::from("/s/it's.vtt"),
            label: "en".into(),
        };
        let shape = shape_of(&video, Some(&burn), Path::new("/in.mp4"), &caps).unwrap();
        let step = Step {
            width: 1280,
            height: 720,
            fps: 30.0,
            video_bps: 1,
            audio_bps: 0,
        };
        let graph = video_graph(&shape, &step, None);
        assert_eq!(
            graph,
            "[0:0]scale=1280:720:flags=lanczos,setsar=1[s0];[s0]subtitles=filename=/s/it\\\\\\'s.vtt[s1];[s1]format=yuv420p[v]"
        );
        assert_eq!(shape.notes, vec!["Burned in the en subtitles."]);

        // A slower step adds the rate filter. A variable rate always gets it.
        let slower = Step { fps: 24.0, ..step };
        assert!(video_graph(&shape, &slower, None).contains("fps=fps=24.0000"));
        let mut vfr = track(1920, 1080);
        vfr.vfr = true;
        let shape = shape_of(&vfr, None, Path::new("/in.mp4"), &caps).unwrap();
        assert!(video_graph(&shape, &step, None).contains("[s0]fps=fps=30.0000[s1]"));

        // Hardware encoders take uploaded frames at the end of the chain.
        assert!(
            video_graph(&shape, &step, Some(Hardware::Vaapi)).ends_with("format=nv12,hwupload[v]")
        );
    }

    #[test]
    fn hdr_interlacing_alpha_and_spheres_are_handled_before_scaling() {
        let caps = caps(&["zscale", "tonemap", "v360", "bwdif"], &["libx264"]);
        let mut video = track(3840, 2160);
        video.hdr = Some(HdrFormat::Hlg);
        video.field_order = FieldOrder::BottomFirst;
        video.alpha = true;
        video.sample_aspect = Some((4, 3));
        let shape = shape_of(&video, None, Path::new("/in.mkv"), &caps).unwrap();
        assert_eq!((shape.width, shape.height), (5120, 2160));
        let step = Step {
            width: 1920,
            height: 810,
            fps: 30.0,
            video_bps: 1,
            audio_bps: 0,
        };
        let graph = video_graph(&shape, &step, None);
        let alpha = graph.find("drawbox").unwrap();
        let deinterlace = graph.find("bwdif=mode=send_frame:parity=bff").unwrap();
        let tonemap = graph.find("tonemap=tonemap=hable").unwrap();
        let scale = graph.find("scale=1920:810").unwrap();
        assert!(
            alpha < deinterlace && deinterlace < tonemap && tonemap < scale,
            "{graph}"
        );
        assert_eq!(shape.color_args[1], OsString::from("bt709"));
        assert_eq!(shape.notes.len(), 4, "{:?}", shape.notes);

        let mut sphere = track(3840, 1920);
        sphere.projection = Some(Projection::Equirectangular);
        sphere.stereo = Some(StereoLayout::TopBottom);
        sphere.view = Some((15.0, 0.0, 0.0));
        let shape = shape_of(&sphere, None, Path::new("/in.mp4"), &caps).unwrap();
        assert_eq!((shape.width, shape.height), (1600, 900));
        let graph = video_graph(&shape, &step, None);
        assert!(graph.contains("v360=input=e:output=flat:h_fov=100:v_fov=67.67:w=1600:h=900:yaw=15.00:pitch=0.00:roll=0.00:interp=lanczos:in_stereo=tb:out_stereo=2d"), "{graph}");

        let mut eac = track(2560, 1440);
        eac.projection = Some(Projection::EquiAngularCubemap);
        let shape = shape_of(&eac, None, Path::new("/in.mp4"), &caps).unwrap();
        assert!(video_graph(&shape, &step, None).contains("v360=input=eac:"));
        let mut cube = track(3000, 2000);
        cube.projection = Some(Projection::Cubemap { padding: 10 });
        let shape = shape_of(&cube, None, Path::new("/in.mp4"), &caps).unwrap();
        assert!(video_graph(&shape, &step, None).contains("v360=input=c3x2:in_pad=0.0100:"));
        let mut tile = track(1000, 500);
        tile.projection = Some(Projection::EquirectangularTile {
            left: 0.25,
            top: 0.25,
            right: 0.75,
            bottom: 0.75,
        });
        let shape = shape_of(&tile, None, Path::new("/in.mp4"), &caps).unwrap();
        let graph = video_graph(&shape, &step, None);
        assert!(
            graph.contains("pad=2000:1000:500:250:color=black[s0];[s0]v360=input=e:"),
            "{graph}"
        );

        let mut sbs = track(3840, 1080);
        sbs.stereo = Some(StereoLayout::SideBySide);
        let shape = shape_of(&sbs, None, Path::new("/in.mp4"), &caps).unwrap();
        assert_eq!((shape.width, shape.height), (1920, 1080));
        assert!(video_graph(&shape, &step, None).contains("stereo3d=sbsl:ml"));

        // Bitmap subtitles are laid over the source-sized picture. Text ones are drawn
        // after scaling from the file itself.
        let bitmap = BurnSource::Embedded {
            index: 3,
            position: 1,
            bitmap: true,
            label: "fra".into(),
        };
        let shape = shape_of(
            &track(1920, 1080),
            Some(&bitmap),
            Path::new("/in.mkv"),
            &caps,
        )
        .unwrap();
        let graph = video_graph(&shape, &step, None);
        assert!(
            graph.starts_with("[0:0][0:3]overlay=eof_action=pass[s0];[s0]scale="),
            "{graph}"
        );
        let text = BurnSource::Embedded {
            index: 2,
            position: 0,
            bitmap: false,
            label: "eng".into(),
        };
        let shape = shape_of(&track(1920, 1080), Some(&text), Path::new("/in.mkv"), &caps).unwrap();
        assert!(video_graph(&shape, &step, None).contains("subtitles=filename=/in.mkv:si=0"));
    }

    #[test]
    fn sources_the_build_cannot_render_are_refused_by_name() {
        let bare = caps(&[], &["libx264"]);
        let mut hdr = track(1920, 1080);
        hdr.hdr = Some(HdrFormat::Pq);
        let error = shape_of(&hdr, None, Path::new("/in.mp4"), &bare).unwrap_err();
        assert!(error.to_string().contains("zscale"), "{error}");
        let mut dolby = track(1920, 1080);
        dolby.hdr = Some(HdrFormat::DolbyVision { profile: 5 });
        let with_zscale = caps(&["zscale", "tonemap"], &["libx264"]);
        let error = shape_of(&dolby, None, Path::new("/in.mp4"), &with_zscale).unwrap_err();
        assert!(error.to_string().contains("libplacebo"), "{error}");
        let with_placebo = caps(&["libplacebo"], &["libx264"]);
        let shape = shape_of(&dolby, None, Path::new("/in.mp4"), &with_placebo).unwrap();
        assert!(shape.prefix[0].starts_with("libplacebo="));
        dolby.hdr = Some(HdrFormat::DolbyVision { profile: 8 });
        let shape = shape_of(&dolby, None, Path::new("/in.mp4"), &with_zscale).unwrap();
        assert!(shape.prefix[0].starts_with("zscale="));
        let mut sphere = track(1920, 960);
        sphere.projection = Some(Projection::Equirectangular);
        assert!(shape_of(&sphere, None, Path::new("/in.mp4"), &bare).is_err());
    }

    #[test]
    fn encoders_follow_the_set_and_the_target() {
        let caps = caps(
            &[],
            &[
                "libx264",
                "libx265",
                "libvpx-vp9",
                "aac",
                "libopus",
                "h264_vaapi",
            ],
        );
        let mut set = EncoderSet::default();
        set.by_codec.insert(
            "h264".into(),
            Encoder {
                name: "h264_vaapi".into(),
                hardware: Some(Hardware::Vaapi),
            },
        );
        let target = VideoTarget::new(
            Container::Mp4,
            VideoCodec::H264,
            Some(AudioCodec::Aac),
            1,
            1080,
        );
        let encoders =
            encoders_for(&target, &set, &caps, PathBuf::from("/dev/dri/renderD128")).unwrap();
        assert_eq!(encoders.video.name, "h264_vaapi");
        assert!(!encoders.two_pass);
        assert_eq!(encoders.audio, "aac");
        assert!(encoders.faststart);
        let webm = VideoTarget::new(
            Container::Webm,
            VideoCodec::Vp9,
            Some(AudioCodec::Opus),
            1,
            1080,
        );
        let encoders = encoders_for(&webm, &set, &caps, PathBuf::new()).unwrap();
        assert_eq!(encoders.video.name, "libvpx-vp9");
        assert!(encoders.two_pass);
        assert!(!encoders.faststart);
        let wrong = VideoTarget::new(Container::Webm, VideoCodec::H264, None, 1, 1080);
        assert!(matches!(
            encoders_for(&wrong, &set, &caps, PathBuf::new()),
            Err(TranscodeError::UnsupportedTarget { .. })
        ));
        let wrong_audio = VideoTarget::new(
            Container::Mp4,
            VideoCodec::H264,
            Some(AudioCodec::Opus),
            1,
            1080,
        );
        assert!(matches!(
            encoders_for(&wrong_audio, &set, &caps, PathBuf::new()),
            Err(TranscodeError::UnsupportedTarget { .. })
        ));
        let av1 = VideoTarget::new(Container::Mp4, VideoCodec::Av1, None, 1, 1080);
        assert!(matches!(
            encoders_for(&av1, &set, &caps, PathBuf::new()),
            Err(TranscodeError::MissingEncoder { .. })
        ));
        let args = encoder_args(
            &encoders_for(&target, &set, &caps, PathBuf::new())
                .unwrap()
                .video,
            None,
            Path::new("/log"),
            1_000_000,
            false,
        );
        assert!(args.contains(&OsString::from("-rc_mode")));
        assert!(args.contains(&OsString::from("1300000")));
        let x264 = Encoder {
            name: "libx264".into(),
            hardware: None,
        };
        let pass1 = encoder_args(&x264, Some(1), Path::new("/log"), 1_000_000, false);
        assert!(
            pass1.contains(&OsString::from("-pass"))
                && !pass1.contains(&OsString::from("-maxrate"))
        );
        let still = encoder_args(&x264, None, Path::new("/log"), 80_000, true);
        assert!(
            still.contains(&OsString::from("stillimage"))
                && still.contains(&OsString::from("-crf"))
        );
    }

    #[test]
    fn the_ladder_keeps_to_the_caps() {
        let ladder = Ladder::new(1920, 1080, Some(59.94), 720, None, true);
        assert_eq!(ladder.shapes[0], (1280, 720, 59.94));
        assert_eq!(ladder.shapes[1], (1280, 720, 30.0));
        let capped = Ladder::new(1920, 1080, Some(59.94), 1080, Some(24), false);
        assert_eq!(capped.shapes[0], (1920, 1080, 24.0));
        assert_eq!(capped.audio, vec![0]);
        let portrait = Ladder::new(1080, 1920, Some(30.0), 480, None, true);
        assert_eq!(portrait.shapes[0], (480, 852, 30.0));
        let step = portrait.pick(2_000_000.0).unwrap();
        assert_eq!((step.width, step.height), (480, 852));
        assert_eq!(step.audio_bps, 128_000);
        assert!(
            Ladder::new(64, 64, Some(30.0), 1080, None, false)
                .pick(1.0)
                .is_none()
        );
        assert_eq!(flat_view_size(3840), (1600, 900));
        assert_eq!(flat_view_size(1024), (640, 360));
        assert!((vertical_fov(100.0, 1600, 900) - 67.67).abs() < 0.01);
    }

    #[test]
    fn remuxing_is_ruled_out_by_processing_needs() {
        let target = VideoTarget::new(
            Container::Mp4,
            VideoCodec::H264,
            Some(AudioCodec::Aac),
            10_000,
            1080,
        );
        let mut info = MediaInfo {
            container: Container::Mp4,
            kind: MediaKind::Video,
            duration: Some(Duration::from_secs(1)),
            video: Some(track(1920, 1080)),
            audio: None,
            cover: None,
            subtitles: Vec::new(),
        };
        assert_eq!(remux_mode(&info, 5000, &target), Some(true));
        assert_eq!(remux_mode(&info, 50_000, &target), None);
        info.video.as_mut().unwrap().hdr = Some(HdrFormat::Pq);
        assert_eq!(remux_mode(&info, 5000, &target), None);
        info.video.as_mut().unwrap().hdr = None;
        info.video.as_mut().unwrap().fps = Some(60.0);
        let slow = VideoTarget {
            max_fps: Some(30),
            ..target.clone()
        };
        assert_eq!(remux_mode(&info, 5000, &slow), None);
        assert_eq!(remux_mode(&info, 5000, &target), Some(true));
        let still = VideoTarget {
            still: Some(StillSource::Waveform),
            ..target
        };
        assert_eq!(remux_mode(&info, 5000, &still), None);
    }
}

#[cfg(test)]
mod ffmpeg_tests {
    use std::ffi::OsString;
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::*;
    use crate::config::EncoderChoice;
    use crate::media::{FieldOrder, HdrFormat, MediaKind, Projection};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("discoclip-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn tools(dir: &Path) -> Ffmpeg {
        Ffmpeg::provision(&dir.join("tools")).await.unwrap()
    }

    /// Writes a sample with ffmpeg: `args` after the global options, then the output.
    async fn sample(ffmpeg: &Ffmpeg, dir: &Path, name: &str, args: &[&str]) -> PathBuf {
        let path = dir.join(name);
        let mut full: Vec<OsString> = vec!["-loglevel".into(), "error".into()];
        full.extend(args.iter().map(OsString::from));
        full.push(path.as_os_str().to_owned());
        ffmpeg.run(full, |_| {}).await.unwrap();
        path
    }

    async fn source(ffmpeg: &Ffmpeg, path: PathBuf) -> LocalFile {
        let mut file = LocalFile::from_path(path).await.unwrap();
        file.info = Some(ffmpeg.inspect(&file.path).await.unwrap());
        file
    }

    fn video_target(max_bytes: u64) -> VideoTarget {
        VideoTarget::new(
            Container::Mp4,
            VideoCodec::H264,
            Some(AudioCodec::Aac),
            max_bytes,
            1080,
        )
    }

    async fn run(
        transcoder: &FfmpegTranscoder,
        input: &LocalFile,
        target: VideoTarget,
        dir: &Path,
    ) -> Result<Transcoded, TranscodeError> {
        let (progress, _watched) = tokio::sync::watch::channel(Progress::default());
        transcoder
            .transcode(input, &Target::Video(target), &dir.join("out"), progress)
            .await
    }

    /// One frame of `path` at `at` as 8-bit grey, for comparing pictures.
    async fn grey_frame(ffmpeg: &Ffmpeg, path: &Path, at: f64, dir: &Path) -> Vec<u8> {
        let raw = dir.join(format!("frame-{at}.raw"));
        let args: Vec<OsString> = vec![
            "-loglevel".into(),
            "error".into(),
            "-ss".into(),
            format!("{at}").into(),
            "-i".into(),
            path.as_os_str().to_owned(),
            "-frames:v".into(),
            "1".into(),
            "-an".into(),
            "-f".into(),
            "rawvideo".into(),
            "-pix_fmt".into(),
            "gray".into(),
            raw.as_os_str().to_owned(),
        ];
        ffmpeg.run(args, |_| {}).await.unwrap();
        tokio::fs::read(&raw).await.unwrap()
    }

    #[tokio::test]
    async fn hdr_sources_are_tone_mapped_to_sdr() {
        let dir = temp_dir("hdr");
        let ffmpeg = tools(&dir).await;
        let path = sample(
            &ffmpeg,
            &dir,
            "hdr.mp4",
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=320x180:r=25:d=2",
                "-f",
                "lavfi",
                "-i",
                "sine=f=440:d=2",
                "-vf",
                "setparams=color_primaries=bt2020:color_trc=smpte2084:colorspace=bt2020nc",
                "-pix_fmt",
                "yuv420p10le",
                "-c:v",
                "libx265",
                "-x265-params",
                "log-level=none",
                "-preset",
                "ultrafast",
                "-c:a",
                "aac",
                "-shortest",
            ],
        )
        .await;
        let input = source(&ffmpeg, path).await;
        let video = input.info.as_ref().unwrap().video.as_ref().unwrap();
        assert_eq!(video.hdr, Some(HdrFormat::Pq));
        let transcoder = FfmpegTranscoder::new(ffmpeg.clone());
        let out = run(&transcoder, &input, video_target(5_000_000), &dir)
            .await
            .unwrap();
        assert!(
            out.notes
                .iter()
                .any(|n| n == "Tone-mapped HDR10 (PQ) to SDR."),
            "{:?}",
            out.notes
        );
        let info = out.file.info.unwrap();
        let video = info.video.unwrap();
        assert_eq!(video.hdr, None);
        assert_eq!(video.pix_fmt.as_deref(), Some("yuv420p"));
        assert_eq!(video.color.transfer.as_deref(), Some("bt709"));
        assert_eq!(video.color.primaries.as_deref(), Some("bt709"));
        assert!(info.audio.is_some());
        // The tone-mapped picture is not black: the test pattern survives.
        let frame = grey_frame(&ffmpeg, &out.file.path, 1.0, &dir).await;
        let bright = frame.iter().filter(|b| **b > 100).count();
        assert!(
            bright > frame.len() / 10,
            "{bright} bright pixels of {}",
            frame.len()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn interlaced_sources_are_deinterlaced_whether_flagged_or_not() {
        let dir = temp_dir("interlace");
        let ffmpeg = tools(&dir).await;
        let flagged = sample(
            &ffmpeg,
            &dir,
            "flagged.mp4",
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=320x240:r=50:d=2",
                "-vf",
                "tinterlace=mode=interleave_top",
                "-flags",
                "+ilme+ildct",
                "-field_order",
                "tt",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-x264-params",
                "interlaced=1:tff=1",
            ],
        )
        .await;
        let input = source(&ffmpeg, flagged).await;
        assert_eq!(
            input
                .info
                .as_ref()
                .unwrap()
                .video
                .as_ref()
                .unwrap()
                .field_order,
            FieldOrder::TopFirst
        );
        let transcoder = FfmpegTranscoder::new(ffmpeg.clone());
        let out = run(&transcoder, &input, video_target(5_000_000), &dir)
            .await
            .unwrap();
        assert!(
            out.notes.iter().any(|n| n.starts_with("Deinterlaced")),
            "{:?}",
            out.notes
        );
        assert_eq!(
            out.file.info.unwrap().video.unwrap().field_order,
            FieldOrder::Progressive
        );

        // Combed frames in a file that does not say are found by looking at them, as in
        // an old MPEG-4 capture. The pattern is smoothed first, as footage is:
        // single-pixel lines read as combing.
        let unflagged = sample(
            &ffmpeg,
            &dir,
            "unflagged.avi",
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=320x240:r=50:d=2",
                "-vf",
                "scale=640:480:flags=bicubic,tinterlace=mode=interleave_top,setfield=prog",
                "-c:v",
                "mpeg4",
                "-q:v",
                "3",
            ],
        )
        .await;
        let input = source(&ffmpeg, unflagged).await;
        assert_eq!(
            input
                .info
                .as_ref()
                .unwrap()
                .video
                .as_ref()
                .unwrap()
                .field_order,
            FieldOrder::TopFirst
        );
        // A picture its file calls progressive is taken at its word, however busy the
        // pattern, and is not deinterlaced.
        let progressive = sample(
            &ffmpeg,
            &dir,
            "progressive.mp4",
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=1280x720:r=25:d=2",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
            ],
        )
        .await;
        let input = source(&ffmpeg, progressive).await;
        assert_eq!(
            input
                .info
                .as_ref()
                .unwrap()
                .video
                .as_ref()
                .unwrap()
                .field_order,
            FieldOrder::Progressive
        );
        let out = run(&transcoder, &input, video_target(5_000_000), &dir)
            .await
            .unwrap();
        assert!(
            !out.notes.iter().any(|n| n.starts_with("Deinterlaced")),
            "{:?}",
            out.notes
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn variable_frame_rates_are_steadied() {
        let dir = temp_dir("vfr");
        let ffmpeg = tools(&dir).await;
        let path = sample(
            &ffmpeg,
            &dir,
            "vfr.webm",
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=320x180:r=30:d=3",
                "-vf",
                "select='if(lt(n,30),1,not(mod(n,3)))'",
                "-fps_mode",
                "vfr",
                "-c:v",
                "libvpx",
                "-deadline",
                "realtime",
            ],
        )
        .await;
        let input = source(&ffmpeg, path).await;
        let video = input.info.as_ref().unwrap().video.as_ref().unwrap();
        assert!(video.vfr);
        let fps = video.fps.unwrap();
        assert!(fps > 10.0 && fps < 30.0, "{fps}");
        let transcoder = FfmpegTranscoder::new(ffmpeg.clone());
        let mut target = video_target(5_000_000);
        target.audio = None;
        let out = run(&transcoder, &input, target, &dir).await.unwrap();
        assert!(
            out.notes
                .iter()
                .any(|n| n == "Steadied the variable frame rate."),
            "{:?}",
            out.notes
        );
        let steady = ffmpeg.sample_timing(&out.file.path, 0).await.unwrap();
        assert!(!steady.vfr, "{steady:?}");
        assert!(
            (steady.fps.unwrap() - fps).abs() < 1.0,
            "{steady:?} vs {fps}"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn subtitles_are_burnt_in_from_files_and_streams() {
        let dir = temp_dir("burn");
        let ffmpeg = tools(&dir).await;
        let srt = dir.join("subs.srt");
        std::fs::write(
            &srt,
            "1\n00:00:00,000 --> 00:00:01,500\nHELLO THERE HELLO THERE\n\n2\n00:00:01,500 --> 00:00:03,000\nSECOND LINE SECOND LINE\n",
        )
        .unwrap();
        let path = sample(
            &ffmpeg,
            &dir,
            "withsubs.mkv",
            &[
                "-f",
                "lavfi",
                "-i",
                "color=c=0x202020:s=320x180:r=25:d=3",
                "-f",
                "lavfi",
                "-i",
                "sine=f=440:d=3",
                "-i",
                srt.to_str().unwrap(),
                "-map",
                "0:v",
                "-map",
                "1:a",
                "-map",
                "2:s",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-c:a",
                "aac",
                "-c:s",
                "srt",
                "-metadata:s:s:0",
                "language=eng",
            ],
        )
        .await;
        let input = source(&ffmpeg, path.clone()).await;
        let info = input.info.as_ref().unwrap();
        assert_eq!(info.subtitles.len(), 1);
        assert_eq!(info.subtitles[0].language.as_deref(), Some("eng"));
        let transcoder = FfmpegTranscoder::new(ffmpeg.clone());

        let mut plain = video_target(5_000_000);
        plain.audio = None;
        let unburnt = run(&transcoder, &input, plain.clone(), &dir.join("plain"))
            .await
            .unwrap();
        let mut embedded = plain.clone();
        embedded.burn = Some(BurnSource::Embedded {
            index: info.subtitles[0].index,
            position: 0,
            bitmap: false,
            label: "eng".into(),
        });
        let burnt = run(&transcoder, &input, embedded, &dir.join("embedded"))
            .await
            .unwrap();
        assert!(
            burnt
                .notes
                .iter()
                .any(|n| n == "Burned in the eng subtitles."),
            "{:?}",
            burnt.notes
        );
        let before = grey_frame(&ffmpeg, &unburnt.file.path, 0.5, &dir.join("plain")).await;
        let after = grey_frame(&ffmpeg, &burnt.file.path, 0.5, &dir.join("embedded")).await;
        let changed = before
            .iter()
            .zip(&after)
            .filter(|(a, b)| (**a as i32 - **b as i32).abs() > 40)
            .count();
        assert!(changed > 100, "{changed} pixels changed by the subtitles");

        let mut from_file = plain.clone();
        from_file.burn = Some(BurnSource::File {
            path: srt.clone(),
            label: "English".into(),
        });
        let burnt = run(&transcoder, &input, from_file, &dir.join("file"))
            .await
            .unwrap();
        assert!(
            burnt
                .notes
                .iter()
                .any(|n| n == "Burned in the English subtitles.")
        );
        let after = grey_frame(&ffmpeg, &burnt.file.path, 0.5, &dir.join("file")).await;
        let changed = before
            .iter()
            .zip(&after)
            .filter(|(a, b)| (**a as i32 - **b as i32).abs() > 40)
            .count();
        assert!(changed > 100, "{changed} pixels changed by the subtitles");

        // A clip with subtitles is cut after they are drawn, so the second cue shows at
        // the start of a clip that starts inside it.
        let mut clipped = plain.clone();
        clipped.clip = Some(ClipRange {
            start: Duration::from_millis(1800),
            end: Some(Duration::from_millis(2800)),
        });
        clipped.burn = Some(BurnSource::File {
            path: srt,
            label: "English".into(),
        });
        let cut = run(&transcoder, &input, clipped, &dir.join("clip"))
            .await
            .unwrap();
        let length = cut
            .file
            .info
            .as_ref()
            .unwrap()
            .duration
            .unwrap()
            .as_secs_f64();
        assert!((length - 1.0).abs() < 0.15, "{length}");
        let start = grey_frame(&ffmpeg, &cut.file.path, 0.1, &dir.join("clip")).await;
        // The frame at 2.2s of the unburnt output, with the second cue drawn, matches.
        let second = grey_frame(&ffmpeg, &burnt.file.path, 2.2, &dir.join("file")).await;
        let differs = start
            .iter()
            .zip(&second)
            .filter(|(a, b)| (**a as i32 - **b as i32).abs() > 40)
            .count();
        assert!(
            differs < 100,
            "{differs} pixels differ between the clip's start and the second cue"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn sound_alone_plays_over_a_still() {
        let dir = temp_dir("still");
        let ffmpeg = tools(&dir).await;
        let cover = sample(
            &ffmpeg,
            &dir,
            "cover.png",
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=300x300:d=0.1",
                "-frames:v",
                "1",
            ],
        )
        .await;
        let song = sample(
            &ffmpeg,
            &dir,
            "song.mp3",
            &[
                "-f",
                "lavfi",
                "-i",
                "sine=f=440:d=3",
                "-i",
                cover.to_str().unwrap(),
                "-map",
                "0:a",
                "-map",
                "1:v",
                "-c:a",
                "libmp3lame",
                "-c:v",
                "mjpeg",
                "-disposition:v",
                "attached_pic",
                "-id3v2_version",
                "3",
            ],
        )
        .await;
        let input = source(&ffmpeg, song).await;
        let info = input.info.as_ref().unwrap();
        assert_eq!(info.kind, MediaKind::Audio);
        assert!(info.cover.is_some());
        let transcoder = FfmpegTranscoder::new(ffmpeg.clone());
        for (name, still, expected) in [
            ("cover", StillSource::Cover, "the cover art"),
            (
                "picture",
                StillSource::Picture {
                    path: cover.clone(),
                },
                "the platform's thumbnail",
            ),
            ("wave", StillSource::Waveform, "a waveform of the sound"),
        ] {
            let mut target = video_target(2_000_000);
            target.still = Some(still);
            let out = run(&transcoder, &input, target, &dir.join(name))
                .await
                .unwrap();
            assert!(
                out.notes.iter().any(|n| n.contains(expected)),
                "{name}: {:?}",
                out.notes
            );
            let info = out.file.info.unwrap();
            assert_eq!(info.kind, MediaKind::Video, "{name}");
            let video = info.video.unwrap();
            assert_eq!((video.width, video.height), (1280, 720), "{name}");
            assert!(info.audio.is_some(), "{name}");
            let length = info.duration.unwrap().as_secs_f64();
            assert!((length - 3.0).abs() < 0.3, "{name}: {length}");
        }
        // The budget squeezes the sound rather than the picture.
        let mut tight = video_target(60_000);
        tight.still = Some(StillSource::Cover);
        let out = run(&transcoder, &input, tight, &dir.join("tight"))
            .await
            .unwrap();
        assert!(out.file.size <= 60_000, "{}", out.file.size);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn transparent_anamorphic_and_spherical_pictures_come_out_flat() {
        let dir = temp_dir("shapes");
        let ffmpeg = tools(&dir).await;
        let transcoder = FfmpegTranscoder::new(ffmpeg.clone());
        let alpha = sample(
            &ffmpeg,
            &dir,
            "alpha.webm",
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=160x120:r=10:d=1",
                "-vf",
                "format=yuva420p",
                "-c:v",
                "libvpx-vp9",
                "-pix_fmt",
                "yuva420p",
            ],
        )
        .await;
        let input = source(&ffmpeg, alpha).await;
        assert!(input.info.as_ref().unwrap().video.as_ref().unwrap().alpha);
        let mut target = video_target(2_000_000);
        target.audio = None;
        let out = run(&transcoder, &input, target.clone(), &dir.join("alpha"))
            .await
            .unwrap();
        assert!(
            out.notes
                .iter()
                .any(|n| n.starts_with("Laid the transparent")),
            "{:?}",
            out.notes
        );
        let video = out.file.info.unwrap().video.unwrap();
        assert_eq!((video.width, video.height), (160, 120));
        assert_eq!(video.pix_fmt.as_deref(), Some("yuv420p"));

        // VP9 in WebM, so the picture is encoded rather than copied into the MP4.
        let anamorphic = sample(
            &ffmpeg,
            &dir,
            "anamorphic.webm",
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=320x240:r=25:d=1",
                "-vf",
                "setsar=16/11",
                "-c:v",
                "libvpx-vp9",
                "-deadline",
                "realtime",
            ],
        )
        .await;
        let input = source(&ffmpeg, anamorphic).await;
        // Matroska keeps the display size, so the ratio comes back rounded: 93:64 for 16:11.
        let sar = input
            .info
            .as_ref()
            .unwrap()
            .video
            .as_ref()
            .unwrap()
            .sample_aspect
            .unwrap();
        assert!(
            (sar.0 as f64 / sar.1 as f64 - 16.0 / 11.0).abs() < 0.01,
            "{sar:?}"
        );
        let out = run(&transcoder, &input, target.clone(), &dir.join("anamorphic"))
            .await
            .unwrap();
        assert!(
            out.notes
                .iter()
                .any(|n| n.starts_with("Squared the pixels")),
            "{:?}",
            out.notes
        );
        let video = out.file.info.unwrap().video.unwrap();
        assert_eq!((video.width, video.height), (464, 240));
        assert_eq!(video.sample_aspect, None);

        let sphere = sample(
            &ffmpeg,
            &dir,
            "sphere.mp4",
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=640x320:r=10:d=1",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
            ],
        )
        .await;
        let mut input = source(&ffmpeg, sphere).await;
        // The platform said the picture is a sphere. The file does not.
        input
            .info
            .as_mut()
            .unwrap()
            .video
            .as_mut()
            .unwrap()
            .projection = Some(Projection::Equirectangular);
        let out = run(&transcoder, &input, target, &dir.join("sphere"))
            .await
            .unwrap();
        assert!(
            out.notes
                .iter()
                .any(|n| n.starts_with("Rendered a 100° view")),
            "{:?}",
            out.notes
        );
        let video = out.file.info.unwrap().video.unwrap();
        assert_eq!((video.width, video.height), (640, 360));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn remuxing_and_passthrough_yield_to_processing() {
        let dir = temp_dir("remux");
        let ffmpeg = tools(&dir).await;
        let transcoder = FfmpegTranscoder::new(ffmpeg.clone());
        let plain = sample(
            &ffmpeg,
            &dir,
            "plain.mkv",
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=320x180:r=25:d=1",
                "-f",
                "lavfi",
                "-i",
                "sine=f=440:d=1",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-c:a",
                "libopus",
                "-shortest",
            ],
        )
        .await;
        let input = source(&ffmpeg, plain).await;
        let out = run(
            &transcoder,
            &input,
            video_target(5_000_000),
            &dir.join("remux"),
        )
        .await
        .unwrap();
        assert!(
            out.notes
                .iter()
                .any(|n| n.starts_with("Copied the picture and re-encoded the sound")),
            "{:?}",
            out.notes
        );
        let mut slow = video_target(5_000_000);
        slow.max_fps = Some(15);
        let out = run(&transcoder, &input, slow, &dir.join("slow"))
            .await
            .unwrap();
        assert!(
            out.notes.iter().any(|n| n.contains("at 15.0000 fps")),
            "{:?}",
            out.notes
        );
        let fps = out.file.info.unwrap().video.unwrap().fps.unwrap();
        assert!((fps - 15.0).abs() < 0.1, "{fps}");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn a_failing_hardware_encoder_falls_back_to_software() {
        let dir = temp_dir("fallback");
        let ffmpeg = tools(&dir).await;
        // A VA-API encoder on a render node that does not exist fails on every machine,
        // whatever GPU it has.
        ffmpeg
            .configure(
                &dir.join("tools"),
                None,
                EncoderChoice::Software,
                &dir.join("no-render-node"),
            )
            .await
            .unwrap();
        let mut set = (*ffmpeg.encoders()).clone();
        set.hardware = Some(Hardware::Vaapi);
        set.by_codec.insert(
            "h264".into(),
            Encoder {
                name: "h264_vaapi".into(),
                hardware: Some(Hardware::Vaapi),
            },
        );
        ffmpeg.set_encoders(set);
        let transcoder = FfmpegTranscoder::new(ffmpeg.clone());
        let plain = sample(
            &ffmpeg,
            &dir,
            "plain.mp4",
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=320x180:r=25:d=1",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
            ],
        )
        .await;
        let input = source(&ffmpeg, plain).await;
        let mut target = video_target(500_000);
        target.audio = None;
        target.max_height = 144;
        let out = run(&transcoder, &input, target, &dir).await.unwrap();
        assert!(
            out.notes
                .iter()
                .any(|n| n.starts_with("h264_vaapi failed") && n.contains("libx264")),
            "{:?}",
            out.notes
        );
        assert!(
            out.notes.iter().any(|n| n.contains("with libx264:")),
            "{:?}",
            out.notes
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn encoder_choices_are_tried_and_reported() {
        let dir = temp_dir("choice");
        let ffmpeg = tools(&dir).await;
        let cache = dir.join("tools");
        let set = ffmpeg
            .configure(
                &cache,
                None,
                EncoderChoice::Software,
                Path::new("/dev/dri/renderD128"),
            )
            .await
            .unwrap();
        assert_eq!(set.hardware, None);
        assert_eq!(set.for_codec(&VideoCodec::H264).unwrap().name, "libx264");
        assert_eq!(set.for_codec(&VideoCodec::Vp9).unwrap().name, "libvpx-vp9");
        assert!(set.shortfall.is_none());
        assert!(set.summary().contains("libx264"));
        let set = ffmpeg
            .configure(
                &cache,
                None,
                EncoderChoice::Vaapi,
                &dir.join("no-render-node"),
            )
            .await
            .unwrap();
        assert_eq!(set.hardware, None);
        let shortfall = set.shortfall.clone().unwrap();
        assert!(
            shortfall.contains("vaapi") && shortfall.contains("h264_vaapi"),
            "{shortfall}"
        );
        assert_eq!(set.for_codec(&VideoCodec::H264).unwrap().name, "libx264");
        // Auto takes what runs here, and never reports a shortfall.
        let set = ffmpeg
            .configure(
                &cache,
                None,
                EncoderChoice::Auto,
                Path::new("/dev/dri/renderD128"),
            )
            .await
            .unwrap();
        assert!(set.shortfall.is_none());
        assert!(set.trials.iter().all(|t| t.ok || !t.detail.is_empty()));
        assert!(matches!(
            ffmpeg.source(),
            crate::ffmpeg::ToolSource::Embedded { .. }
        ));
        assert!(Ffmpeg::check_external(&dir.join("missing")).await.is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn an_installed_ffmpeg_with_vaapi_encodes_on_its_device() {
        let external = Path::new("/usr/bin/ffmpeg");
        let device = Path::new("/dev/dri/renderD128");
        if !external.is_file() || !device.exists() {
            return;
        }
        let dir = temp_dir("vaapi");
        let ffmpeg = tools(&dir).await;
        let banner = Ffmpeg::check_external(external).await.unwrap();
        assert!(banner.starts_with("ffmpeg version"), "{banner}");
        let set = ffmpeg
            .configure(
                &dir.join("tools"),
                Some(external),
                EncoderChoice::Vaapi,
                device,
            )
            .await
            .unwrap();
        assert!(matches!(
            ffmpeg.source(),
            crate::ffmpeg::ToolSource::External { .. }
        ));
        let Some(Hardware::Vaapi) = set.hardware else {
            let shortfall = set.shortfall.clone().unwrap();
            assert!(shortfall.contains("vaapi"), "{shortfall}");
            std::fs::remove_dir_all(dir).unwrap();
            return;
        };
        let transcoder = FfmpegTranscoder::new(ffmpeg.clone());
        let plain = sample(
            &ffmpeg,
            &dir,
            "plain.mp4",
            &[
                "-f",
                "lavfi",
                "-i",
                "testsrc2=s=640x360:r=25:d=2",
                "-f",
                "lavfi",
                "-i",
                "sine=f=440:d=2",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-c:a",
                "aac",
                "-shortest",
            ],
        )
        .await;
        let input = source(&ffmpeg, plain).await;
        let mut target = video_target(300_000);
        target.max_height = 240;
        let out = run(&transcoder, &input, target, &dir).await.unwrap();
        assert!(
            out.notes.iter().any(|n| n.contains("with h264_vaapi:")),
            "{:?}",
            out.notes
        );
        assert!(out.file.size <= 300_000);
        assert_eq!(out.file.info.unwrap().video.unwrap().height, 240);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn fonts_are_found_for_subtitles() {
        let dir = temp_dir("fonts");
        let ffmpeg = tools(&dir).await;
        let font = ffmpeg.check_fonts(&dir).await.unwrap();
        assert!(font.contains('/'), "{font}");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
