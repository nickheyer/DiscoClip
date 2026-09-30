//! Runs FFmpeg and FFprobe: the builds embedded at build time, unpacked under the cache
//! directory, or a build installed on the machine when the settings name one. Reads what
//! a file holds, and works out which encoders the build at hand can run.

use std::collections::{BTreeSet, HashMap};
use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::config::EncoderChoice;
use crate::media::{
    AttachedPicture, AudioCodec, AudioTrack, ColorInfo, Container, EmbeddedSubtitle, FieldOrder,
    HdrFormat, MediaInfo, MediaKind, Projection, StereoLayout, VideoCodec, VideoTrack,
};

include!(concat!(env!("OUT_DIR"), "/ffmpeg_embed.rs"));

const STDERR_TAIL: usize = 16 * 1024;
/// How many video packets are read to tell a variable frame rate from a constant one.
const TIMING_SAMPLE: usize = 600;
/// How much of a picture the interlace detector looks at.
const INTERLACE_SAMPLE: Duration = Duration::from_secs(6);
/// How long a candidate encoder gets to encode the test clip before it counts as broken.
const ENCODER_TEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, thiserror::Error)]
pub enum FfmpegError {
    #[error("ffmpeg could not be installed: {0}")]
    Install(String),
    #[error("ffmpeg failed: {0}")]
    Process(String),
    #[error("could not read media: {0}")]
    Probe(String),
    #[error("{encoder} cannot encode here: {reason}")]
    Encoder { encoder: String, reason: String },
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Where the tools live. Shared by every clone, so a relocation reaches them all.
#[derive(Debug, Clone)]
struct Tools {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
}

/// Which build the tools come from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ToolSource {
    /// The build embedded in the binary, unpacked under `cache_dir`.
    Embedded { cache_dir: PathBuf },
    /// A build installed on the machine.
    External { path: PathBuf },
}

/// What the build at hand can do: the encoders and filters it lists, and its banner.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Capabilities {
    pub version: String,
    pub encoders: BTreeSet<String>,
    pub filters: BTreeSet<String>,
}

impl Capabilities {
    pub fn has_encoder(&self, name: &str) -> bool {
        self.encoders.contains(name)
    }

    pub fn has_filter(&self, name: &str) -> bool {
        self.filters.contains(name)
    }
}

/// A family of hardware encoders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Hardware {
    Nvenc,
    Vaapi,
    Qsv,
    Videotoolbox,
    Amf,
    V4l2m2m,
}

impl Hardware {
    /// Every family, in the order `auto` tries them: the discrete GPU stacks first, then
    /// the integrated and embedded ones.
    pub const ALL: [Hardware; 6] = [
        Hardware::Nvenc,
        Hardware::Qsv,
        Hardware::Vaapi,
        Hardware::Amf,
        Hardware::Videotoolbox,
        Hardware::V4l2m2m,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Hardware::Nvenc => "nvenc",
            Hardware::Vaapi => "vaapi",
            Hardware::Qsv => "qsv",
            Hardware::Videotoolbox => "videotoolbox",
            Hardware::Amf => "amf",
            Hardware::V4l2m2m => "v4l2m2m",
        }
    }

    /// The ffmpeg name of the family's encoder for `codec`, whether or not the build has it.
    pub fn encoder_name(self, codec: &VideoCodec) -> Option<String> {
        let codec = match codec {
            VideoCodec::H264 => "h264",
            VideoCodec::H265 => "hevc",
            VideoCodec::Av1 => "av1",
            VideoCodec::Vp9 => "vp9",
            VideoCodec::Vp8 => "vp8",
            VideoCodec::Other(_) => return None,
        };
        let supported = match self {
            Hardware::Nvenc => matches!(codec, "h264" | "hevc" | "av1"),
            Hardware::Vaapi => matches!(codec, "h264" | "hevc" | "av1" | "vp9" | "vp8"),
            Hardware::Qsv => matches!(codec, "h264" | "hevc" | "av1" | "vp9"),
            Hardware::Videotoolbox => matches!(codec, "h264" | "hevc"),
            Hardware::Amf => matches!(codec, "h264" | "hevc" | "av1"),
            Hardware::V4l2m2m => matches!(codec, "h264" | "hevc" | "vp8"),
        };
        supported.then(|| format!("{codec}_{}", self.as_str()))
    }

    /// The global options that open the device the family encodes on.
    pub fn device_args(self, vaapi_device: &Path) -> Vec<OsString> {
        match self {
            Hardware::Vaapi => vec![
                "-init_hw_device".into(),
                {
                    let mut spec = OsString::from("vaapi=va:");
                    spec.push(vaapi_device.as_os_str());
                    spec
                },
                "-filter_hw_device".into(),
                "va".into(),
            ],
            Hardware::Qsv => vec![
                "-init_hw_device".into(),
                "qsv=hw".into(),
                "-filter_hw_device".into(),
                "hw".into(),
            ],
            _ => Vec::new(),
        }
    }

    /// The filters that hand frames to the device, appended to the software filter chain.
    pub fn upload_filters(self) -> &'static str {
        match self {
            Hardware::Vaapi => "format=nv12,hwupload",
            Hardware::Qsv => "format=nv12,hwupload=extra_hw_frames=64",
            _ => "",
        }
    }
}

/// The software encoder for `codec` that the build at hand has, best first.
pub fn software_encoder(codec: &VideoCodec, caps: &Capabilities) -> Option<&'static str> {
    let candidates: &[&'static str] = match codec {
        VideoCodec::H264 => &["libx264"],
        VideoCodec::H265 => &["libx265"],
        VideoCodec::Vp9 => &["libvpx-vp9"],
        VideoCodec::Vp8 => &["libvpx"],
        VideoCodec::Av1 => &["libsvtav1", "librav1e", "libaom-av1"],
        VideoCodec::Other(_) => &[],
    };
    candidates
        .iter()
        .copied()
        .find(|name| caps.has_encoder(name))
}

/// One encoder the transcoder runs: its ffmpeg name and the family it belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Encoder {
    pub name: String,
    /// `None` for a software encoder.
    pub hardware: Option<Hardware>,
}

/// How a candidate encoder fared when it was tried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EncoderTrial {
    pub encoder: String,
    pub ok: bool,
    pub detail: String,
}

/// The encoders chosen for the build and the machine: one per codec the destinations
/// may ask for, and what was tried to get there.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct EncoderSet {
    /// What the settings asked for.
    pub choice: EncoderChoice,
    /// The family in use, when a hardware one is.
    pub hardware: Option<Hardware>,
    /// The encoder for each codec, by the codec's serialized name.
    pub by_codec: HashMap<String, Encoder>,
    pub trials: Vec<EncoderTrial>,
    /// Why the settings' choice is not in use, when it is not.
    pub shortfall: Option<String>,
}

impl EncoderSet {
    pub fn for_codec(&self, codec: &VideoCodec) -> Option<&Encoder> {
        self.by_codec.get(codec_key(codec))
    }

    /// One line saying what encodes video here.
    pub fn summary(&self) -> String {
        let h264 = self
            .for_codec(&VideoCodec::H264)
            .map(|e| e.name.clone())
            .unwrap_or_else(|| "none".into());
        match self.hardware {
            Some(hw) => format!("{} hardware encoding through {h264}", hw.as_str()),
            None => format!("software encoding through {h264}"),
        }
    }
}

fn codec_key(codec: &VideoCodec) -> &str {
    match codec {
        VideoCodec::H264 => "h264",
        VideoCodec::H265 => "h265",
        VideoCodec::Vp8 => "vp8",
        VideoCodec::Vp9 => "vp9",
        VideoCodec::Av1 => "av1",
        VideoCodec::Other(name) => name.as_str(),
    }
}

const TARGET_CODECS: [VideoCodec; 5] = [
    VideoCodec::H264,
    VideoCodec::H265,
    VideoCodec::Vp9,
    VideoCodec::Av1,
    VideoCodec::Vp8,
];

#[derive(Debug)]
struct State {
    tools: Tools,
    source: ToolSource,
    capabilities: Arc<Capabilities>,
    encoders: Arc<EncoderSet>,
    vaapi_device: PathBuf,
}

/// The tools, wherever they come from, and what they can do. Every clone shares the
/// same state, so a change of build or encoders reaches them all.
#[derive(Debug, Clone)]
pub struct Ffmpeg {
    state: Arc<RwLock<State>>,
    /// Held while unpacking or switching builds, so two changes do not race.
    switching: Arc<tokio::sync::Mutex<()>>,
}

pub struct Output {
    pub stderr: String,
    pub last_time: Option<Duration>,
    /// The frames written, from the last progress report.
    pub frames: Option<u64>,
}

/// How a recording ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ending {
    /// ffmpeg ran to the end of its input or its own limits.
    Finished,
    /// The output stopped advancing and ffmpeg was told to stop.
    Stalled,
    /// The caller told ffmpeg to stop.
    Stopped,
}

/// What a probe of a video stream's packets and frames found beyond the headers.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sampled {
    /// The average rate the frames arrive at.
    pub fps: Option<f64>,
    pub vfr: bool,
}

impl Ffmpeg {
    /// Unpacks the embedded binaries into `cache_dir` (once per build) and verifies they run.
    /// Encoding is by software until [`configure`](Self::configure) picks otherwise.
    pub async fn provision(cache_dir: &Path) -> Result<Self, FfmpegError> {
        let tools = unpack(cache_dir).await?;
        let capabilities = read_capabilities(&tools).await?;
        tracing::info!(dir = %tools.ffmpeg.parent().unwrap_or(Path::new("")).display(), "{}", capabilities.version);
        let encoders = software_set(EncoderChoice::Software, &capabilities, None);
        let handle = Self {
            state: Arc::new(RwLock::new(State {
                tools,
                source: ToolSource::Embedded {
                    cache_dir: cache_dir.to_path_buf(),
                },
                capabilities: Arc::new(capabilities),
                encoders: Arc::new(encoders),
                vaapi_device: PathBuf::from("/dev/dri/renderD128"),
            })),
            switching: Arc::new(tokio::sync::Mutex::new(())),
        };
        Ok(handle)
    }

    /// Puts the settings into effect: the build the tools come from (`external` when it
    /// names one, else the embedded build under `cache_dir`), and the encoders, chosen
    /// by trying the candidates `choice` allows. Every clone follows. The set chosen is
    /// returned; its `shortfall` says when the choice could not be met and software
    /// encodes instead.
    pub async fn configure(
        &self,
        cache_dir: &Path,
        external: Option<&Path>,
        choice: EncoderChoice,
        vaapi_device: &Path,
    ) -> Result<Arc<EncoderSet>, FfmpegError> {
        let _switching = self.switching.lock().await;
        let wanted = match external {
            Some(path) => ToolSource::External {
                path: path.to_path_buf(),
            },
            None => ToolSource::Embedded {
                cache_dir: cache_dir.to_path_buf(),
            },
        };
        let current = self
            .state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .source
            .clone();
        let (tools, capabilities) = if wanted == current {
            let state = self.state.read().unwrap_or_else(|e| e.into_inner());
            (state.tools.clone(), state.capabilities.clone())
        } else {
            let tools = match &wanted {
                ToolSource::External { path } => external_tools(path).await?,
                ToolSource::Embedded { cache_dir } => unpack(cache_dir).await?,
            };
            let capabilities = read_capabilities(&tools).await?;
            tracing::info!(ffmpeg = %tools.ffmpeg.display(), "{}", capabilities.version);
            (tools, Arc::new(capabilities))
        };
        let encoders = Arc::new(select_encoders(&tools, &capabilities, choice, vaapi_device).await);
        match &encoders.shortfall {
            Some(reason) => tracing::error!("{reason}"),
            None => tracing::info!("{}", encoders.summary()),
        }
        let mut state = self.state.write().unwrap_or_else(|e| e.into_inner());
        state.tools = tools;
        state.source = wanted;
        state.capabilities = capabilities;
        state.encoders = encoders.clone();
        state.vaapi_device = vaapi_device.to_path_buf();
        Ok(encoders)
    }

    /// Checks that `path` is an ffmpeg that runs, with an ffprobe beside it or on `PATH`,
    /// without switching to it.
    pub async fn check_external(path: &Path) -> Result<String, FfmpegError> {
        let tools = external_tools(path).await?;
        let capabilities = read_capabilities(&tools).await?;
        Ok(capabilities.version)
    }

    fn tools(&self) -> Tools {
        self.state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .tools
            .clone()
    }

    /// Which build the tools come from.
    pub fn source(&self) -> ToolSource {
        self.state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .source
            .clone()
    }

    /// What the build at hand lists.
    pub fn capabilities(&self) -> Arc<Capabilities> {
        self.state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .capabilities
            .clone()
    }

    /// The encoders in use.
    pub fn encoders(&self) -> Arc<EncoderSet> {
        self.state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .encoders
            .clone()
    }

    /// Replaces the encoders in use, for tests of what happens when one fails.
    #[cfg(test)]
    pub(crate) fn set_encoders(&self, set: EncoderSet) {
        self.state
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .encoders = Arc::new(set);
    }

    /// The VAAPI render node encoders open.
    pub fn vaapi_device(&self) -> PathBuf {
        self.state
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .vaapi_device
            .clone()
    }

    /// The cache directory the embedded tools are unpacked under. An external build has
    /// none, so the directory the embedded one would use is returned.
    pub fn cache_dir(&self) -> PathBuf {
        match self.source() {
            ToolSource::Embedded { cache_dir } => cache_dir,
            ToolSource::External { .. } => self
                .dir()
                .parent()
                .and_then(Path::parent)
                .map(Path::to_path_buf)
                .unwrap_or_default(),
        }
    }

    /// The directory the tools are in.
    pub fn dir(&self) -> PathBuf {
        self.tools()
            .ffmpeg
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default()
    }

    /// Path of the ffmpeg executable, for libraries that run it themselves.
    pub fn ffmpeg_path(&self) -> PathBuf {
        self.tools().ffmpeg
    }

    pub fn ffprobe_path(&self) -> PathBuf {
        self.tools().ffprobe
    }

    pub async fn version(&self) -> Result<String, FfmpegError> {
        version_of(&self.ffmpeg_path()).await
    }

    /// Runs ffmpeg with the given arguments. Global options for unattended use and machine
    /// readable progress are added. `on_time` receives the output timestamp as encoding advances.
    pub async fn run(
        &self,
        args: impl IntoIterator<Item = OsString>,
        mut on_time: impl FnMut(Duration) + Send,
    ) -> Result<Output, FfmpegError> {
        let mut command = Command::new(self.ffmpeg_path());
        command
            .arg("-hide_banner")
            .arg("-nostdin")
            .arg("-y")
            .arg("-nostats")
            .arg("-progress")
            .arg("pipe:1")
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| FfmpegError::Process("no stdout".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| FfmpegError::Process("no stderr".into()))?;

        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf).await;
            buf
        });

        let mut lines = BufReader::new(stdout).lines();
        let mut last_time = None;
        let mut frames = None;
        while let Some(line) = lines.next_line().await? {
            if let Some(value) = line
                .strip_prefix("out_time_us=")
                .or_else(|| line.strip_prefix("out_time_ms="))
                && let Ok(us) = value.trim().parse::<u64>()
            {
                let t = Duration::from_micros(us);
                last_time = Some(t);
                on_time(t);
            } else if let Some(value) = line.strip_prefix("frame=")
                && let Ok(n) = value.trim().parse::<u64>()
            {
                frames = Some(n);
            }
        }
        let status = child.wait().await?;
        let stderr_bytes = stderr_task.await.unwrap_or_default();
        let stderr = tail(&stderr_bytes);
        if !status.success() {
            return Err(FfmpegError::Process(format!(
                "exit status {}: {}",
                status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into()),
                summarize(&stderr)
            )));
        }
        Ok(Output {
            stderr,
            last_time,
            frames,
        })
    }

    /// Runs ffmpeg as [`run`](Self::run) does, on a source that arrives as it plays:
    /// ffmpeg is told to stop, and so writes its output out whole, when `stop` resolves or
    /// when its output has not advanced for `stall`, as when the stream behind it has
    /// gone quiet. How it ended comes back with the output.
    pub async fn record(
        &self,
        args: impl IntoIterator<Item = OsString>,
        mut on_time: impl FnMut(Duration) + Send,
        stall: Duration,
        stop: Option<tokio::sync::oneshot::Receiver<()>>,
    ) -> Result<(Output, Ending), FfmpegError> {
        use tokio::io::AsyncWriteExt;

        let mut command = Command::new(self.ffmpeg_path());
        command
            .arg("-hide_banner")
            .arg("-y")
            .arg("-nostats")
            .arg("-progress")
            .arg("pipe:1")
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut child = command.spawn()?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| FfmpegError::Process("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| FfmpegError::Process("no stdout".into()))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| FfmpegError::Process("no stderr".into()))?;
        let stderr_task = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf).await;
            buf
        });
        let stop = async move {
            match stop {
                Some(receiver) => {
                    let _ = receiver.await;
                }
                None => std::future::pending::<()>().await,
            }
        };
        tokio::pin!(stop);
        let mut lines = BufReader::new(stdout).lines();
        let mut last_time = None;
        let mut frames = None;
        let mut ending = Ending::Finished;
        let mut asked = false;
        loop {
            let line = tokio::select! {
                line = lines.next_line() => line?,
                _ = &mut stop, if !asked => {
                    ending = Ending::Stopped;
                    asked = true;
                    let _ = stdin.write_all(b"q\n").await;
                    let _ = stdin.flush().await;
                    continue;
                }
                _ = tokio::time::sleep(stall), if !asked => {
                    ending = Ending::Stalled;
                    asked = true;
                    let _ = stdin.write_all(b"q\n").await;
                    let _ = stdin.flush().await;
                    continue;
                }
            };
            let Some(line) = line else {
                break;
            };
            if let Some(value) = line
                .strip_prefix("out_time_us=")
                .or_else(|| line.strip_prefix("out_time_ms="))
                && let Ok(us) = value.trim().parse::<u64>()
            {
                let t = Duration::from_micros(us);
                last_time = Some(t);
                on_time(t);
            } else if let Some(value) = line.strip_prefix("frame=")
                && let Ok(n) = value.trim().parse::<u64>()
            {
                frames = Some(n);
            }
        }
        drop(stdin);
        // A stopped ffmpeg has a few seconds to write its trailer before it is killed.
        let status = match tokio::time::timeout(Duration::from_secs(15), child.wait()).await {
            Ok(status) => status?,
            Err(_) => {
                child.kill().await?;
                child.wait().await?
            }
        };
        let stderr_bytes = stderr_task.await.unwrap_or_default();
        let stderr = tail(&stderr_bytes);
        // ffmpeg stopped by request exits with 255. That is the stop taking effect.
        let stopped_on_request = asked && status.code() == Some(255);
        if !(status.success() || stopped_on_request) {
            return Err(FfmpegError::Process(format!(
                "exit status {}: {}",
                status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into()),
                summarize(&stderr)
            )));
        }
        Ok((
            Output {
                stderr,
                last_time,
                frames,
            },
            ending,
        ))
    }

    /// Runs ffprobe with `args` on `path` and returns its standard output.
    async fn ffprobe(&self, args: &[&str], path: &Path) -> Result<Vec<u8>, FfmpegError> {
        let out = Command::new(self.ffprobe_path())
            .args(["-v", "error"])
            .args(args)
            .arg(path)
            .stdin(std::process::Stdio::null())
            .output()
            .await?;
        if !out.status.success() {
            return Err(FfmpegError::Probe(summarize(&String::from_utf8_lossy(
                &out.stderr,
            ))));
        }
        Ok(out.stdout)
    }

    /// Reads the container, the duration and the streams with ffprobe: the moving
    /// picture, the sound, the cover art and the subtitles, with everything the headers
    /// say about colour, fields, aspect and projection.
    pub async fn probe(&self, path: &Path) -> Result<MediaInfo, FfmpegError> {
        let stdout = self
            .ffprobe(
                &["-print_format", "json", "-show_format", "-show_streams"],
                path,
            )
            .await?;
        let report: ProbeReport = serde_json::from_slice(&stdout)
            .map_err(|e| FfmpegError::Probe(format!("ffprobe output: {e}")))?;
        let mut info = media_info(&report, path);
        if info.duration.is_none() && info.kind != MediaKind::Image {
            info.duration = self.measure_duration(path).await?;
        }
        Ok(info)
    }

    /// Reads a source the way the transcoder needs it: [`probe`](Self::probe), then the
    /// video stream's packets for its true frame rate and whether it varies, and, when
    /// the headers do not say whether the picture is interlaced, its frames.
    pub async fn inspect(&self, path: &Path) -> Result<MediaInfo, FfmpegError> {
        let mut info = self.probe(path).await?;
        if info.kind != MediaKind::Video {
            return Ok(info);
        }
        let Some(video) = info.video.as_mut() else {
            return Ok(info);
        };
        let sampled = self.sample_timing(path, video.index).await?;
        if let Some(fps) = sampled.fps {
            let declared = video.fps.unwrap_or(0.0);
            // A declared rate near the measured one is the exact rational and is kept.
            if sampled.vfr
                || declared <= 0.0
                || declared > 240.0
                || (declared - fps).abs() > fps * 0.05
            {
                video.fps = Some(fps);
            }
        }
        video.vfr = sampled.vfr;
        if video.field_order == FieldOrder::Unknown
            && interlacing_is_plausible(&video.codec, video.height)
            && let Some(order) = self.detect_interlacing(path, video.index).await?
        {
            video.field_order = order;
        }
        Ok(info)
    }

    /// Reads the timestamps of the first video packets: the rate they arrive at, and
    /// whether the intervals between them vary.
    pub async fn sample_timing(&self, path: &Path, stream: usize) -> Result<Sampled, FfmpegError> {
        let selector = format!("{stream}");
        let interval = format!("%+#{TIMING_SAMPLE}");
        let stdout = self
            .ffprobe(
                &[
                    "-select_streams",
                    &selector,
                    "-show_entries",
                    "packet=pts_time,dts_time",
                    "-read_intervals",
                    &interval,
                    "-of",
                    "csv=p=0",
                ],
                path,
            )
            .await?;
        Ok(sample_from_packets(&String::from_utf8_lossy(&stdout)))
    }

    /// Looks at the first seconds of a picture for combing. The field order found when
    /// most frames are interlaced, `None` when they are not.
    pub async fn detect_interlacing(
        &self,
        path: &Path,
        stream: usize,
    ) -> Result<Option<FieldOrder>, FfmpegError> {
        let args: Vec<OsString> = vec![
            "-loglevel".into(),
            "info".into(),
            "-t".into(),
            format!("{}", INTERLACE_SAMPLE.as_secs()).into(),
            "-i".into(),
            path.as_os_str().to_owned(),
            "-map".into(),
            format!("0:{stream}").into(),
            "-an".into(),
            "-sn".into(),
            "-dn".into(),
            "-vf".into(),
            "idet".into(),
            "-f".into(),
            "null".into(),
            "-".into(),
        ];
        let output = self.run(args, |_| {}).await?;
        Ok(interlacing_from_idet(&output.stderr))
    }

    /// Decodes the first stream with a timeline to nowhere and reads the final
    /// timestamp, for containers that do not declare a duration.
    async fn measure_duration(&self, path: &Path) -> Result<Option<Duration>, FfmpegError> {
        let args: Vec<OsString> = vec![
            "-loglevel".into(),
            "error".into(),
            "-i".into(),
            path.as_os_str().to_owned(),
            "-map".into(),
            "0:v:0?".into(),
            "-map".into(),
            "0:a:0?".into(),
            "-c".into(),
            "copy".into(),
            "-f".into(),
            "null".into(),
            "-".into(),
        ];
        let output = self.run(args, |_| {}).await?;
        Ok(output.last_time.filter(|t| !t.is_zero()))
    }

    /// Renders a line of subtitles onto a blank frame and reports the font it was drawn
    /// with. An error names what stops subtitles from being burnt in on this machine.
    pub async fn check_fonts(&self, work_dir: &Path) -> Result<String, FfmpegError> {
        tokio::fs::create_dir_all(work_dir).await?;
        let script = work_dir.join(format!(".fonts-{}.srt", std::process::id()));
        tokio::fs::write(
            &script,
            "1\n00:00:00,000 --> 00:00:01,000\nSubtitles render here\n",
        )
        .await?;
        let args: Vec<OsString> = vec![
            "-loglevel".into(),
            "verbose".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            "color=c=black:s=64x64:r=5:d=0.4".into(),
            "-vf".into(),
            format!(
                "subtitles=filename={}",
                crate::transcode::filter_path(&script)
            )
            .into(),
            "-f".into(),
            "null".into(),
            "-".into(),
        ];
        let result = self.run(args, |_| {}).await;
        let _ = tokio::fs::remove_file(&script).await;
        let output = result?;
        fonts_from_render(&output.stderr)
    }
}

/// Whether a codec ever carries interlaced pictures, at a size broadcast used.
fn interlacing_is_plausible(codec: &VideoCodec, height: u32) -> bool {
    if height < 480 {
        return false;
    }
    match codec {
        VideoCodec::H264 | VideoCodec::H265 => true,
        VideoCodec::Vp8 | VideoCodec::Vp9 | VideoCodec::Av1 => false,
        VideoCodec::Other(name) => matches!(
            name.as_str(),
            "mpeg2video"
                | "mpeg1video"
                | "mpeg4"
                | "dvvideo"
                | "vc1"
                | "wmv3"
                | "prores"
                | "mjpeg"
                | "dnxhd"
                | "h263"
                | "msmpeg4v3"
                | "rawvideo"
                | "huffyuv"
                | "ffv1"
                | "v210"
        ),
    }
}

/// The font a test render drew with. libass says when it found none.
fn fonts_from_render(stderr: &str) -> Result<String, FfmpegError> {
    if let Some(line) = stderr
        .lines()
        .find(|l| l.contains("Failed to load") && l.to_lowercase().contains("font"))
    {
        return Err(FfmpegError::Process(format!(
            "no fonts for subtitles: {}",
            line.trim()
        )));
    }
    let picked = stderr.lines().find_map(|line| {
        let (_, rest) = line.split_once("fontselect: (")?;
        let (_, font) = rest.split_once(" -> ")?;
        let path = font.split(',').next()?.trim();
        (!path.is_empty()).then(|| path.to_string())
    });
    match picked {
        Some(font) => Ok(font),
        None => Err(FfmpegError::Process(
            "no font was found for subtitles: libass drew nothing".into(),
        )),
    }
}

/// The frame rate and its steadiness from the packet times ffprobe listed, one per line.
fn sample_from_packets(text: &str) -> Sampled {
    let mut times: Vec<f64> = text
        .lines()
        .filter_map(|line| {
            // `pts_time,dts_time`: the presentation time, else the decode time.
            let mut fields = line.split(',').map(str::trim);
            let pts = fields.next().and_then(|f| f.parse::<f64>().ok());
            let dts = fields.next().and_then(|f| f.parse::<f64>().ok());
            pts.or(dts)
        })
        .collect();
    times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    times.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
    if times.len() < 3 {
        return Sampled {
            fps: None,
            vfr: false,
        };
    }
    let span = times[times.len() - 1] - times[0];
    if span <= 0.0 {
        return Sampled {
            fps: None,
            vfr: false,
        };
    }
    let fps = (times.len() - 1) as f64 / span;
    let mut deltas: Vec<f64> = times.windows(2).map(|w| w[1] - w[0]).collect();
    deltas.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    // The middle of the intervals is the rate. Any interval well off it means the rate
    // varies: a fifth of the frame time allows for the container's timestamp rounding.
    let median = deltas[deltas.len() / 2];
    let tolerance = (median * 0.2).max(0.0015);
    let off = deltas
        .iter()
        .filter(|d| (**d - median).abs() > tolerance)
        .count();
    let vfr = deltas.len() >= 10 && off * 50 > deltas.len();
    Sampled {
        fps: Some(fps),
        vfr,
    }
}

/// The field order the interlace detector saw most, when it saw interlacing at all.
fn interlacing_from_idet(stderr: &str) -> Option<FieldOrder> {
    let line = stderr
        .lines()
        .rev()
        .find(|l| l.contains("Multi frame detection:"))?;
    let count = |key: &str| -> u64 {
        line.split_once(key)
            .and_then(|(_, rest)| rest.split_whitespace().next())
            .and_then(|n| n.parse().ok())
            .unwrap_or(0)
    };
    let tff = count("TFF:");
    let bff = count("BFF:");
    let progressive = count("Progressive:");
    let undetermined = count("Undetermined:");
    let interlaced = tff + bff;
    let total = interlaced + progressive + undetermined;
    // Interlacing has to be the clear reading: well over the frames that read as
    // progressive, and a good share of all the frames looked at.
    if interlaced == 0 || interlaced <= progressive * 2 || interlaced * 10 < total * 3 {
        return None;
    }
    Some(if tff >= bff {
        FieldOrder::TopFirst
    } else {
        FieldOrder::BottomFirst
    })
}

/// Where the tools of this build live under `cache_dir`, unpacked if they are not yet.
async fn unpack(cache_dir: &Path) -> Result<Tools, FfmpegError> {
    let dir = cache_dir.join("ffmpeg").join(&TOOLS_DIGEST[..16]);
    let tools = Tools {
        ffmpeg: dir.join(FFMPEG_EXE),
        ffprobe: dir.join(FFPROBE_EXE),
    };
    let targets = [
        (tools.ffmpeg.clone(), FFMPEG_ZST),
        (tools.ffprobe.clone(), FFPROBE_ZST),
    ];
    tokio::task::spawn_blocking(move || {
        targets
            .iter()
            .try_for_each(|(exe, payload)| install(exe, payload))
    })
    .await
    .map_err(|e| FfmpegError::Install(e.to_string()))??;
    version_of(&tools.ffmpeg).await?;
    Ok(tools)
}

/// An installed ffmpeg at `path`, with the ffprobe beside it or the one on `PATH`.
async fn external_tools(path: &Path) -> Result<Tools, FfmpegError> {
    let ffmpeg = tokio::fs::canonicalize(path).await.map_err(|e| {
        FfmpegError::Install(format!(
            "{} is not an ffmpeg executable: {e}",
            path.display()
        ))
    })?;
    let probe_name = format!("ffprobe{}", std::env::consts::EXE_SUFFIX);
    let beside = ffmpeg.with_file_name(&probe_name);
    let ffprobe = if beside.is_file() {
        beside
    } else {
        let on_path = std::env::var_os("PATH")
            .map(|paths| {
                std::env::split_paths(&paths)
                    .map(|dir| dir.join(&probe_name))
                    .find(|candidate| candidate.is_file())
            })
            .unwrap_or(None);
        on_path.ok_or_else(|| {
            FfmpegError::Install(format!("no ffprobe beside {} or on PATH", ffmpeg.display()))
        })?
    };
    version_of(&ffmpeg).await?;
    version_of(&ffprobe).await?;
    Ok(Tools { ffmpeg, ffprobe })
}

async fn version_of(ffmpeg: &Path) -> Result<String, FfmpegError> {
    let out = Command::new(ffmpeg)
        .arg("-version")
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| FfmpegError::Install(format!("{} does not run: {e}", ffmpeg.display())))?;
    if !out.status.success() {
        return Err(FfmpegError::Install(format!(
            "{} exited with {}",
            ffmpeg.display(),
            out.status
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text.lines().next().unwrap_or("ffmpeg").to_string())
}

/// The banner, encoders and filters a build lists.
async fn read_capabilities(tools: &Tools) -> Result<Capabilities, FfmpegError> {
    let version = version_of(&tools.ffmpeg).await?;
    let list = |flag: &'static str| {
        let ffmpeg = tools.ffmpeg.clone();
        async move {
            let out = Command::new(&ffmpeg)
                .args(["-hide_banner", flag])
                .stdin(std::process::Stdio::null())
                .output()
                .await?;
            Ok::<String, FfmpegError>(String::from_utf8_lossy(&out.stdout).into_owned())
        }
    };
    let encoders = parse_listing(&list("-encoders").await?);
    let filters = parse_listing(&list("-filters").await?);
    Ok(Capabilities {
        version,
        encoders,
        filters,
    })
}

/// The names in an ffmpeg `-encoders` or `-filters` listing: the second word of every
/// line after the `---` rule, where the first word is the flag column.
fn parse_listing(text: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let mut listing = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if !listing {
            if trimmed.starts_with("---") || trimmed.starts_with("...") {
                listing = true;
                // A `-filters` listing has no rule: its first entry starts with flags.
                if trimmed.starts_with("---") {
                    continue;
                }
            } else {
                continue;
            }
        }
        let mut words = trimmed.split_whitespace();
        let (Some(flags), Some(name)) = (words.next(), words.next()) else {
            continue;
        };
        if flags.chars().all(|c| c == '.' || c.is_ascii_uppercase()) && !flags.is_empty() {
            names.insert(name.to_string());
        }
    }
    names
}

fn software_set(
    choice: EncoderChoice,
    caps: &Capabilities,
    shortfall: Option<String>,
) -> EncoderSet {
    let mut set = EncoderSet {
        choice,
        hardware: None,
        by_codec: HashMap::new(),
        trials: Vec::new(),
        shortfall,
    };
    for codec in &TARGET_CODECS {
        if let Some(name) = software_encoder(codec, caps) {
            set.by_codec.insert(
                codec_key(codec).to_string(),
                Encoder {
                    name: name.to_string(),
                    hardware: None,
                },
            );
        }
    }
    set
}

/// Picks the encoders `choice` allows: a hardware family whose H.264 encoder runs here,
/// with its other encoders where they run too, and software for the rest.
async fn select_encoders(
    tools: &Tools,
    caps: &Capabilities,
    choice: EncoderChoice,
    vaapi_device: &Path,
) -> EncoderSet {
    let families: Vec<Hardware> = match choice {
        EncoderChoice::Software => Vec::new(),
        EncoderChoice::Auto => Hardware::ALL.to_vec(),
        EncoderChoice::Nvenc => vec![Hardware::Nvenc],
        EncoderChoice::Vaapi => vec![Hardware::Vaapi],
        EncoderChoice::Qsv => vec![Hardware::Qsv],
        EncoderChoice::Videotoolbox => vec![Hardware::Videotoolbox],
        EncoderChoice::Amf => vec![Hardware::Amf],
        EncoderChoice::V4l2m2m => vec![Hardware::V4l2m2m],
    };
    let mut set = software_set(choice, caps, None);
    let mut reasons = Vec::new();
    for family in families {
        let Some(h264) = family.encoder_name(&VideoCodec::H264) else {
            continue;
        };
        if !caps.has_encoder(&h264) {
            reasons.push(format!("this ffmpeg build has no {h264}"));
            continue;
        }
        match test_encoder(tools, &h264, family, vaapi_device).await {
            Ok(detail) => set.trials.push(EncoderTrial {
                encoder: h264.clone(),
                ok: true,
                detail,
            }),
            Err(error) => {
                set.trials.push(EncoderTrial {
                    encoder: h264.clone(),
                    ok: false,
                    detail: error.to_string(),
                });
                reasons.push(error.to_string());
                continue;
            }
        }
        set.hardware = Some(family);
        set.by_codec.insert(
            "h264".into(),
            Encoder {
                name: h264,
                hardware: Some(family),
            },
        );
        for codec in TARGET_CODECS.iter().filter(|c| **c != VideoCodec::H264) {
            let Some(name) = family.encoder_name(codec) else {
                continue;
            };
            if !caps.has_encoder(&name) {
                continue;
            }
            match test_encoder(tools, &name, family, vaapi_device).await {
                Ok(detail) => {
                    set.trials.push(EncoderTrial {
                        encoder: name.clone(),
                        ok: true,
                        detail,
                    });
                    set.by_codec.insert(
                        codec_key(codec).to_string(),
                        Encoder {
                            name,
                            hardware: Some(family),
                        },
                    );
                }
                Err(error) => set.trials.push(EncoderTrial {
                    encoder: name,
                    ok: false,
                    detail: error.to_string(),
                }),
            }
        }
        return set;
    }
    if choice != EncoderChoice::Software && choice != EncoderChoice::Auto {
        set.shortfall = Some(format!(
            "{} encoding is not available, so video is encoded in software: {}",
            choice.as_str(),
            reasons.join("; ")
        ));
    }
    set
}

/// Encodes half a second of a test pattern with `encoder` on `family`'s device. The
/// encoder's own account of the run when it worked, the reason when it did not.
async fn test_encoder(
    tools: &Tools,
    encoder: &str,
    family: Hardware,
    vaapi_device: &Path,
) -> Result<String, FfmpegError> {
    let mut command = Command::new(&tools.ffmpeg);
    command
        .args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error"])
        .args(family.device_args(vaapi_device))
        .args(["-f", "lavfi", "-i", "testsrc2=s=256x144:r=10:d=0.5"]);
    let upload = family.upload_filters();
    let filter = if upload.is_empty() {
        "format=yuv420p".to_string()
    } else {
        upload.to_string()
    };
    command
        .args([
            "-vf", &filter, "-c:v", encoder, "-b:v", "400k", "-f", "null", "-",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(ENCODER_TEST_TIMEOUT, command.output())
        .await
        .map_err(|_| FfmpegError::Encoder {
            encoder: encoder.to_string(),
            reason: format!(
                "the test encode did not finish within {}s",
                ENCODER_TEST_TIMEOUT.as_secs()
            ),
        })?
        .map_err(|e| FfmpegError::Encoder {
            encoder: encoder.to_string(),
            reason: e.to_string(),
        })?;
    if output.status.success() {
        return Ok(format!("{encoder} encoded the test clip"));
    }
    Err(FfmpegError::Encoder {
        encoder: encoder.to_string(),
        reason: summarize(&String::from_utf8_lossy(&output.stderr)),
    })
}

fn install(exe: &Path, payload: &[u8]) -> Result<(), FfmpegError> {
    if exe.is_file() {
        return Ok(());
    }
    let dir = exe
        .parent()
        .ok_or_else(|| FfmpegError::Install("no parent dir".into()))?;
    let name = exe
        .file_name()
        .ok_or_else(|| FfmpegError::Install("no file name".into()))?
        .to_string_lossy();
    fs::create_dir_all(dir)?;
    static UNPACKS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = UNPACKS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!("{name}.{}.{unique}.part", std::process::id()));
    {
        let mut out = File::create(&tmp)?;
        zstd::stream::copy_decode(payload, &mut out)
            .map_err(|e| FfmpegError::Install(format!("decompress {name}: {e}")))?;
        out.sync_all()?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))?;
    }
    match fs::rename(&tmp, exe) {
        Ok(()) => Ok(()),
        Err(_) if exe.is_file() => {
            let _ = fs::remove_file(&tmp);
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

fn tail(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(STDERR_TAIL);
    String::from_utf8_lossy(&bytes[start..]).into_owned()
}

pub(crate) fn summarize(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("Fontconfig"))
        .collect();
    let take = lines.len().saturating_sub(3);
    let text = lines[take..].join(" | ");
    if text.is_empty() {
        "no diagnostic output".to_string()
    } else {
        text
    }
}

#[derive(Deserialize)]
struct ProbeReport {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    format: ProbeFormat,
}

#[derive(Deserialize)]
struct ProbeFormat {
    format_name: String,
    duration: Option<String>,
}

#[derive(Deserialize)]
struct ProbeStream {
    #[serde(default)]
    index: usize,
    codec_type: String,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    pix_fmt: Option<String>,
    field_order: Option<String>,
    sample_aspect_ratio: Option<String>,
    color_range: Option<String>,
    color_space: Option<String>,
    color_transfer: Option<String>,
    color_primaries: Option<String>,
    avg_frame_rate: Option<String>,
    r_frame_rate: Option<String>,
    bit_rate: Option<String>,
    sample_rate: Option<String>,
    channels: Option<u16>,
    #[serde(default)]
    disposition: Disposition,
    #[serde(default)]
    side_data_list: Vec<SideData>,
    #[serde(default)]
    tags: HashMap<String, String>,
}

#[derive(Default, Deserialize)]
struct Disposition {
    #[serde(default)]
    attached_pic: u8,
    #[serde(default)]
    default: u8,
    #[serde(default)]
    forced: u8,
    #[serde(default)]
    still_image: u8,
}

#[derive(Default, Deserialize)]
struct SideData {
    #[serde(default)]
    side_data_type: String,
    rotation: Option<f64>,
    projection: Option<String>,
    padding: Option<u32>,
    bound_left: Option<u32>,
    bound_top: Option<u32>,
    bound_right: Option<u32>,
    bound_bottom: Option<u32>,
    yaw: Option<f64>,
    pitch: Option<f64>,
    roll: Option<f64>,
    /// The stereo packing, for `Stereo 3D` side data.
    #[serde(rename = "type")]
    stereo_type: Option<String>,
    dv_profile: Option<u8>,
}

impl ProbeStream {
    fn codec(&self) -> &str {
        self.codec_name.as_deref().unwrap_or("")
    }

    fn is_moving_picture(&self) -> bool {
        self.codec_type == "video" && self.disposition.attached_pic == 0 && !self.is_still()
    }

    /// A still image codec: one picture, not a stream of them.
    fn is_still(&self) -> bool {
        self.codec_type == "video"
            && (self.disposition.still_image == 1
                || matches!(
                    self.codec(),
                    "png"
                        | "mjpeg"
                        | "bmp"
                        | "tiff"
                        | "webp"
                        | "jpeg2000"
                        | "jpegxl"
                        | "jpegls"
                        | "avif"
                        | "heif"
                        | "hevc_still"
                        | "psd"
                        | "sgi"
                        | "ppm"
                        | "pgm"
                        | "pbm"
                        | "pam"
                        | "targa"
                        | "pcx"
                        | "dds"
                        | "exr"
                        | "qoi"
                        | "svg"
                ))
    }

    /// Whether the display matrix turns the picture by a quarter turn, swapping its sides.
    fn is_rotated(&self) -> bool {
        self.side_data_list
            .iter()
            .find_map(|d| d.rotation)
            .or_else(|| self.tags.get("rotate").and_then(|r| r.parse().ok()))
            .is_some_and(|degrees: f64| ((degrees.abs() / 90.0).round() as u32) % 2 == 1)
    }

    fn bitrate(&self) -> Option<u64> {
        self.bit_rate.as_deref().and_then(|b| b.parse().ok())
    }

    fn tag(&self, name: &str) -> Option<String> {
        self.tags
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
            .filter(|value| !value.is_empty() && value != "und")
    }

    fn side(&self, kind: &str) -> Option<&SideData> {
        self.side_data_list
            .iter()
            .find(|d| d.side_data_type.eq_ignore_ascii_case(kind))
    }
}

/// A still image counts as a picture of its own size, so a still is reported as `Image`
/// with the picture in `video` and no frame rate. A file with a moving picture is `Video`,
/// one with sound alone `Audio`, and anything ffprobe reads but finds neither in is `File`.
fn media_info(report: &ProbeReport, path: &Path) -> MediaInfo {
    let moving = report
        .streams
        .iter()
        .find(|s| s.is_moving_picture())
        .and_then(video_track);
    let still = report
        .streams
        .iter()
        .find(|s| s.disposition.attached_pic == 0 && s.is_still())
        .and_then(video_track);
    let audio_streams: Vec<&ProbeStream> = report
        .streams
        .iter()
        .filter(|s| s.codec_type == "audio")
        .collect();
    // The stream marked default plays by default everywhere. Else the first.
    let audio = audio_streams
        .iter()
        .find(|s| s.disposition.default == 1)
        .or_else(|| audio_streams.first())
        .map(|s| audio_track(s));
    let cover = report
        .streams
        .iter()
        .find(|s| s.codec_type == "video" && s.disposition.attached_pic == 1)
        .and_then(|s| {
            Some(AttachedPicture {
                index: s.index,
                width: s.width?,
                height: s.height?,
            })
        });
    let subtitles = report
        .streams
        .iter()
        .filter(|s| s.codec_type == "subtitle")
        .map(|s| EmbeddedSubtitle {
            index: s.index,
            codec: s.codec().to_string(),
            language: s.tag("language"),
            name: s.tag("title"),
            bitmap: matches!(
                s.codec(),
                "hdmv_pgs_subtitle" | "dvd_subtitle" | "dvb_subtitle" | "xsub" | "dvb_teletext"
            ),
            default: s.disposition.default == 1,
            forced: s.disposition.forced == 1,
        })
        .collect();
    let duration = report
        .format
        .duration
        .as_deref()
        .and_then(|d| d.parse::<f64>().ok())
        .filter(|d| *d > 0.0)
        .map(Duration::from_secs_f64);
    let container = container_from_formats(&report.format.format_name, path);
    let (kind, video) = match (moving, still, &audio) {
        (Some(video), _, _) => (MediaKind::Video, Some(video)),
        (None, _, Some(_)) => (MediaKind::Audio, None),
        (None, Some(mut picture), None) => {
            picture.fps = None;
            (MediaKind::Image, Some(picture))
        }
        (None, None, None) => (MediaKind::File, None),
    };
    MediaInfo {
        container,
        kind,
        duration: if kind == MediaKind::Image {
            None
        } else {
            duration
        },
        video,
        audio,
        cover,
        subtitles,
    }
}

fn video_track(stream: &ProbeStream) -> Option<VideoTrack> {
    let (mut width, mut height) = (stream.width?, stream.height?);
    if stream.is_rotated() {
        std::mem::swap(&mut width, &mut height);
    }
    let fps = stream
        .avg_frame_rate
        .as_deref()
        .and_then(parse_rate)
        .or_else(|| stream.r_frame_rate.as_deref().and_then(parse_rate))
        .filter(|f| *f > 0.0);
    let color = ColorInfo {
        primaries: stream.color_primaries.clone(),
        transfer: stream.color_transfer.clone(),
        matrix: stream.color_space.clone(),
        range: stream.color_range.clone(),
    };
    let hdr = match stream
        .side("DOVI configuration record")
        .and_then(|d| d.dv_profile)
    {
        Some(profile) => Some(HdrFormat::DolbyVision { profile }),
        None => match color.transfer.as_deref() {
            Some("smpte2084") => Some(HdrFormat::Pq),
            Some("arib-std-b67") => Some(HdrFormat::Hlg),
            _ => None,
        },
    };
    let field_order = match stream.field_order.as_deref() {
        Some("progressive") => FieldOrder::Progressive,
        Some("tt") | Some("tb") => FieldOrder::TopFirst,
        Some("bb") | Some("bt") => FieldOrder::BottomFirst,
        _ => FieldOrder::Unknown,
    };
    let sample_aspect = stream
        .sample_aspect_ratio
        .as_deref()
        .and_then(|sar| {
            let (num, den) = sar.split_once(':')?;
            Some((num.parse::<u32>().ok()?, den.parse::<u32>().ok()?))
        })
        .filter(|(num, den)| *num > 0 && *den > 0 && num != den);
    let alpha = stream.pix_fmt.as_deref().is_some_and(pix_fmt_has_alpha)
        || stream.tag("alpha_mode").as_deref() == Some("1");
    let spherical = stream.side("Spherical Mapping");
    let projection = spherical.and_then(|side| match side.projection.as_deref()? {
        "equirectangular" => Some(Projection::Equirectangular),
        "cubemap" => Some(Projection::Cubemap {
            padding: side.padding.unwrap_or(0),
        }),
        "tiled equirectangular" => {
            let (w, h) = (stream.width? as f32, stream.height? as f32);
            let left = side.bound_left.unwrap_or(0) as f32;
            let top = side.bound_top.unwrap_or(0) as f32;
            let right = side.bound_right.unwrap_or(0) as f32;
            let bottom = side.bound_bottom.unwrap_or(0) as f32;
            let full_w = w + left + right;
            let full_h = h + top + bottom;
            Some(Projection::EquirectangularTile {
                left: left / full_w,
                top: top / full_h,
                right: (left + w) / full_w,
                bottom: (top + h) / full_h,
            })
        }
        _ => None,
    });
    let view =
        spherical.and_then(|side| Some((side.yaw? as f32, side.pitch? as f32, side.roll? as f32)));
    let stereo = stream
        .side("Stereo 3D")
        .and_then(|side| match side.stereo_type.as_deref()? {
            "side by side" => Some(StereoLayout::SideBySide),
            "top and bottom" => Some(StereoLayout::TopBottom),
            _ => None,
        });
    Some(VideoTrack {
        codec: video_codec(stream.codec()),
        width,
        height,
        fps,
        bitrate: stream.bitrate(),
        index: stream.index,
        pix_fmt: stream.pix_fmt.clone(),
        color,
        hdr,
        field_order,
        sample_aspect,
        vfr: false,
        alpha,
        projection,
        stereo,
        view,
    })
}

/// Whether a pixel format carries an alpha plane or channel.
fn pix_fmt_has_alpha(name: &str) -> bool {
    name.starts_with("yuva")
        || name.starts_with("ya")
        || name.starts_with("gbrap")
        || name.starts_with("rgba")
        || name.starts_with("bgra")
        || name.starts_with("argb")
        || name.starts_with("abgr")
        || name.starts_with("ayuv")
        || name.starts_with("vuya")
        || name == "pal8"
}

fn audio_track(stream: &ProbeStream) -> AudioTrack {
    AudioTrack {
        codec: audio_codec(stream.codec()),
        channels: stream.channels.unwrap_or(2),
        sample_rate: stream
            .sample_rate
            .as_deref()
            .and_then(|r| r.parse().ok())
            .unwrap_or(48_000),
        bitrate: stream.bitrate(),
        index: stream.index,
        language: stream.tag("language"),
    }
}

/// Parses ffprobe's `num/den` rationals, and plain decimals.
fn parse_rate(rate: &str) -> Option<f64> {
    match rate.split_once('/') {
        Some((num, den)) => {
            let num: f64 = num.parse().ok()?;
            let den: f64 = den.parse().ok()?;
            (den != 0.0).then(|| num / den)
        }
        None => rate.parse().ok(),
    }
}

fn container_from_formats(formats: &str, path: &Path) -> Container {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    let list: Vec<&str> = formats.split(',').map(str::trim).collect();
    if list.contains(&"mp4") {
        if ext == "mov" {
            Container::Mov
        } else if ext == "m4a" || ext == "m4b" || ext == "aac" {
            Container::M4a
        } else {
            Container::Mp4
        }
    } else if list.contains(&"webm") || list.contains(&"matroska") {
        if ext == "webm" {
            Container::Webm
        } else {
            Container::Mkv
        }
    } else if list.contains(&"mpegts") {
        Container::Ts
    } else if list.contains(&"flv") {
        Container::Flv
    } else if list.contains(&"avi") {
        Container::Avi
    } else if list.contains(&"gif") {
        Container::Gif
    } else if list.contains(&"mp3") {
        Container::Mp3
    } else if list.contains(&"ogg") {
        if ext == "opus" {
            Container::Opus
        } else {
            Container::Ogg
        }
    } else if list.contains(&"flac") {
        Container::Flac
    } else if list.contains(&"wav") {
        Container::Wav
    } else if list.contains(&"image2") || list.contains(&"jpeg_pipe") {
        Container::Jpeg
    } else if list.contains(&"png_pipe") || list.contains(&"apng") {
        Container::Png
    } else if list.contains(&"webp_pipe") {
        Container::Webp
    } else if list.contains(&"avif") {
        Container::Avif
    } else {
        Container::from_extension(&ext).unwrap_or_else(|| {
            Container::Other(list.first().copied().unwrap_or("unknown").to_string())
        })
    }
}

fn video_codec(name: &str) -> VideoCodec {
    match name {
        "h264" => VideoCodec::H264,
        "hevc" => VideoCodec::H265,
        "vp8" => VideoCodec::Vp8,
        "vp9" => VideoCodec::Vp9,
        "av1" => VideoCodec::Av1,
        other => VideoCodec::Other(other.to_string()),
    }
}

fn audio_codec(name: &str) -> AudioCodec {
    match name {
        "aac" => AudioCodec::Aac,
        "opus" => AudioCodec::Opus,
        "vorbis" => AudioCodec::Vorbis,
        "mp3" | "mp3float" => AudioCodec::Mp3,
        "flac" => AudioCodec::Flac,
        other => AudioCodec::Other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{
        Capabilities, Hardware, ProbeReport, fonts_from_render, interlacing_from_idet, media_info,
        parse_listing, sample_from_packets, software_encoder,
    };
    use crate::media::{
        AudioCodec, Container, FieldOrder, HdrFormat, MediaKind, Projection, StereoLayout,
        VideoCodec,
    };

    const REPORT: &str = r#"{
      "streams": [
        {"index": 0, "codec_name": "mjpeg", "codec_type": "video", "width": 600, "height": 600,
         "avg_frame_rate": "0/0", "r_frame_rate": "90000/1", "disposition": {"attached_pic": 1}},
        {"index": 1, "codec_name": "h264", "codec_type": "video", "width": 1920, "height": 1080,
         "avg_frame_rate": "30000/1001", "r_frame_rate": "30000/1001", "bit_rate": "4000000",
         "pix_fmt": "yuv420p", "field_order": "progressive", "sample_aspect_ratio": "1:1",
         "color_range": "tv", "color_space": "bt709", "color_transfer": "bt709", "color_primaries": "bt709",
         "side_data_list": [{"side_data_type": "Display Matrix", "displaymatrix": "...", "rotation": -90}]},
        {"index": 2, "codec_name": "aac", "codec_type": "audio", "sample_rate": "44100", "channels": 2,
         "bit_rate": "128000", "avg_frame_rate": "0/0", "r_frame_rate": "0/0", "tags": {"language": "eng"}},
        {"index": 3, "codec_name": "subrip", "codec_type": "subtitle", "tags": {"language": "eng", "title": "English"},
         "disposition": {"default": 1}},
        {"index": 4, "codec_name": "hdmv_pgs_subtitle", "codec_type": "subtitle", "tags": {"language": "fra"}}
      ],
      "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2", "duration": "12.500000"}
    }"#;

    #[test]
    fn reads_streams_skipping_cover_art_and_applying_rotation() {
        let report: ProbeReport = serde_json::from_str(REPORT).unwrap();
        let info = media_info(&report, Path::new("clip.mp4"));
        assert_eq!(info.container, Container::Mp4);
        assert_eq!(info.duration.unwrap().as_millis(), 12_500);
        let video = info.video.unwrap();
        assert_eq!(video.codec, VideoCodec::H264);
        assert_eq!(video.index, 1);
        assert_eq!((video.width, video.height), (1080, 1920));
        assert!((video.fps.unwrap() - 29.97).abs() < 0.01);
        assert_eq!(video.bitrate, Some(4_000_000));
        assert_eq!(video.field_order, FieldOrder::Progressive);
        assert_eq!(video.hdr, None);
        assert_eq!(video.sample_aspect, None);
        assert!(!video.alpha);
        assert_eq!(video.color.transfer.as_deref(), Some("bt709"));
        let audio = info.audio.unwrap();
        assert_eq!(audio.codec, AudioCodec::Aac);
        assert_eq!(audio.index, 2);
        assert_eq!(audio.language.as_deref(), Some("eng"));
        assert_eq!((audio.channels, audio.sample_rate), (2, 44_100));
        assert_eq!(info.kind, MediaKind::Video);
        let cover = info.cover.unwrap();
        assert_eq!((cover.index, cover.width, cover.height), (0, 600, 600));
        assert_eq!(info.subtitles.len(), 2);
        assert_eq!(info.subtitles[0].index, 3);
        assert_eq!(info.subtitles[0].name.as_deref(), Some("English"));
        assert!(info.subtitles[0].default);
        assert!(!info.subtitles[0].bitmap);
        assert!(info.subtitles[1].bitmap);
        assert_eq!(info.subtitles[1].language.as_deref(), Some("fra"));
    }

    #[test]
    fn hdr_interlacing_aspect_alpha_and_projection_are_read() {
        let report = r#"{"streams": [
            {"index": 0, "codec_name": "hevc", "codec_type": "video", "width": 3840, "height": 1920,
             "avg_frame_rate": "30/1", "r_frame_rate": "30/1", "pix_fmt": "yuv420p10le",
             "field_order": "tt", "sample_aspect_ratio": "64:45", "color_transfer": "smpte2084",
             "color_primaries": "bt2020", "color_space": "bt2020nc",
             "side_data_list": [
               {"side_data_type": "Spherical Mapping", "projection": "equirectangular", "yaw": 10, "pitch": 0, "roll": 0},
               {"side_data_type": "Stereo 3D", "type": "top and bottom", "inverted": 0}
             ]}],
          "format": {"format_name": "matroska,webm", "duration": "5"}}"#;
        let report: ProbeReport = serde_json::from_str(report).unwrap();
        let video = media_info(&report, Path::new("x.mkv")).video.unwrap();
        assert_eq!(video.hdr, Some(HdrFormat::Pq));
        assert_eq!(video.field_order, FieldOrder::TopFirst);
        assert_eq!(video.sample_aspect, Some((64, 45)));
        assert_eq!(video.display_size(), (5461, 1920));
        assert_eq!(video.projection, Some(Projection::Equirectangular));
        assert_eq!(video.stereo, Some(StereoLayout::TopBottom));
        assert_eq!(video.view, Some((10.0, 0.0, 0.0)));
        assert!(video.needs_processing());

        let dolby = r#"{"streams": [
            {"index": 0, "codec_name": "hevc", "codec_type": "video", "width": 1920, "height": 1080,
             "pix_fmt": "yuv420p10le", "field_order": "bb", "color_transfer": "arib-std-b67",
             "side_data_list": [{"side_data_type": "DOVI configuration record", "dv_profile": 5}]},
            {"index": 1, "codec_name": "vp9", "codec_type": "video", "width": 16, "height": 16, "pix_fmt": "yuva420p"}],
          "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2"}}"#;
        let report: ProbeReport = serde_json::from_str(dolby).unwrap();
        let video = media_info(&report, Path::new("x.mp4")).video.unwrap();
        assert_eq!(video.hdr, Some(HdrFormat::DolbyVision { profile: 5 }));
        assert_eq!(video.field_order, FieldOrder::BottomFirst);
        assert!(!video.alpha);

        let tile = r#"{"streams": [
            {"index": 0, "codec_name": "h264", "codec_type": "video", "width": 1000, "height": 500,
             "pix_fmt": "yuva420p", "avg_frame_rate": "30/1",
             "side_data_list": [{"side_data_type": "Spherical Mapping", "projection": "tiled equirectangular",
               "bound_left": 500, "bound_top": 250, "bound_right": 500, "bound_bottom": 250, "yaw": 0, "pitch": 0, "roll": 0},
               {"side_data_type": "Spherical Mapping", "projection": "cubemap", "padding": 4}]}],
          "format": {"format_name": "mov,mp4,m4a,3gp,3g2,mj2"}}"#;
        let report: ProbeReport = serde_json::from_str(tile).unwrap();
        let video = media_info(&report, Path::new("x.mp4")).video.unwrap();
        assert!(video.alpha);
        assert_eq!(
            video.projection,
            Some(Projection::EquirectangularTile {
                left: 0.25,
                top: 0.25,
                right: 0.75,
                bottom: 0.75
            })
        );
        let webm_alpha = r#"{"streams": [
            {"index": 0, "codec_name": "vp9", "codec_type": "video", "width": 16, "height": 16,
             "pix_fmt": "yuv420p", "tags": {"alpha_mode": "1"}}],
          "format": {"format_name": "matroska,webm"}}"#;
        let report: ProbeReport = serde_json::from_str(webm_alpha).unwrap();
        assert!(
            media_info(&report, Path::new("x.webm"))
                .video
                .unwrap()
                .alpha
        );
    }

    #[test]
    fn stills_audio_and_bare_files_are_told_apart() {
        let still = r#"{"streams": [
            {"index": 0, "codec_name": "png", "codec_type": "video", "width": 640, "height": 480,
             "avg_frame_rate": "25/1", "r_frame_rate": "25/1"}],
          "format": {"format_name": "png_pipe", "duration": "0.040000"}}"#;
        let report: ProbeReport = serde_json::from_str(still).unwrap();
        let info = media_info(&report, Path::new("pic.png"));
        assert_eq!(info.kind, MediaKind::Image);
        assert_eq!(info.container, Container::Png);
        assert_eq!(info.duration, None);
        let picture = info.video.unwrap();
        assert_eq!(
            (picture.width, picture.height, picture.fps),
            (640, 480, None)
        );
        assert!(info.audio.is_none());

        let song = r#"{"streams": [
            {"index": 0, "codec_name": "mp3", "codec_type": "audio", "sample_rate": "44100", "channels": 2,
             "bit_rate": "192000"},
            {"index": 1, "codec_name": "mjpeg", "codec_type": "video", "width": 500, "height": 500,
             "disposition": {"attached_pic": 1}}],
          "format": {"format_name": "mp3", "duration": "180.5"}}"#;
        let report: ProbeReport = serde_json::from_str(song).unwrap();
        let info = media_info(&report, Path::new("song.mp3"));
        assert_eq!(info.kind, MediaKind::Audio);
        assert_eq!(info.container, Container::Mp3);
        assert!(info.video.is_none());
        assert_eq!(info.cover.unwrap().index, 1);
        assert_eq!(info.duration.unwrap().as_millis(), 180_500);

        let m4a = REPORT.replace(
            r#""codec_name": "h264", "codec_type": "video""#,
            r#""codec_name": "h264", "codec_type": "data""#,
        );
        let report: ProbeReport = serde_json::from_str(&m4a).unwrap();
        let info = media_info(&report, Path::new("talk.m4a"));
        assert_eq!(info.kind, MediaKind::Audio);
        assert_eq!(info.container, Container::M4a);

        let bare = r#"{"streams": [], "format": {"format_name": "tty"}}"#;
        let report: ProbeReport = serde_json::from_str(bare).unwrap();
        let info = media_info(&report, Path::new("notes.txt"));
        assert_eq!(info.kind, MediaKind::File);
        assert_eq!(info.container, Container::Other("tty".into()));

        // The default audio stream wins over an earlier one.
        let dual = r#"{"streams": [
            {"index": 0, "codec_name": "ac3", "codec_type": "audio", "channels": 6, "tags": {"language": "jpn"}},
            {"index": 1, "codec_name": "aac", "codec_type": "audio", "channels": 2, "tags": {"language": "eng"},
             "disposition": {"default": 1}}],
          "format": {"format_name": "matroska,webm", "duration": "1"}}"#;
        let report: ProbeReport = serde_json::from_str(dual).unwrap();
        let audio = media_info(&report, Path::new("x.mka")).audio.unwrap();
        assert_eq!(audio.index, 1);
        assert_eq!(audio.codec, AudioCodec::Aac);
    }

    #[test]
    fn container_follows_extension_within_a_format_family() {
        let report: ProbeReport = serde_json::from_str(REPORT).unwrap();
        assert_eq!(
            media_info(&report, Path::new("clip.mov")).container,
            Container::Mov
        );
        let webm = REPORT.replace("mov,mp4,m4a,3gp,3g2,mj2", "matroska,webm");
        let report: ProbeReport = serde_json::from_str(&webm).unwrap();
        assert_eq!(
            media_info(&report, Path::new("clip.webm")).container,
            Container::Webm
        );
        assert_eq!(
            media_info(&report, Path::new("clip.mkv")).container,
            Container::Mkv
        );
    }

    #[test]
    fn packet_times_tell_the_rate_and_whether_it_varies() {
        let steady: String = (0..60)
            .map(|n| format!("{:.6},{:.6}\n", n as f64 / 25.0, n as f64 / 25.0))
            .collect();
        let sampled = sample_from_packets(&steady);
        assert!((sampled.fps.unwrap() - 25.0).abs() < 0.01);
        assert!(!sampled.vfr);
        // Millisecond rounding, as Matroska stores times, does not count as varying.
        let rounded: String = (0..60)
            .map(|n| format!("{:.3}\n", n as f64 / 30.0))
            .collect();
        let sampled = sample_from_packets(&rounded);
        assert!(!sampled.vfr, "{sampled:?}");
        // Frames dropped from a third of the way in: the intervals vary.
        let dropped: String = (0..90)
            .filter(|n| *n < 30 || n % 3 == 0)
            .map(|n| format!("{:.3}\n", n as f64 / 30.0))
            .collect();
        let sampled = sample_from_packets(&dropped);
        assert!(sampled.vfr, "{sampled:?}");
        // B-frames list out of order: sorting puts them right.
        let reordered =
            "0.000\n0.120\n0.040\n0.080\n0.160\n0.280\n0.200\n0.240\n0.320\n0.360\n0.400\n0.440\n";
        let sampled = sample_from_packets(reordered);
        assert!((sampled.fps.unwrap() - 25.0).abs() < 0.01);
        assert!(!sampled.vfr);
        assert_eq!(sample_from_packets("0.0\n").fps, None);
        assert_eq!(sample_from_packets("N/A\nN/A\n").fps, None);
    }

    #[test]
    fn idet_counts_decide_the_field_order() {
        let tff = "[Parsed_idet_0 @ 0x1] Repeated Fields: Neither: 50 Top: 0 Bottom: 0\n\
                   [Parsed_idet_0 @ 0x1] Single frame detection: TFF: 50 BFF: 0 Progressive: 0 Undetermined: 0\n\
                   [Parsed_idet_0 @ 0x1] Multi frame detection: TFF: 38 BFF: 0 Progressive: 2 Undetermined: 10\n";
        assert_eq!(interlacing_from_idet(tff), Some(FieldOrder::TopFirst));
        let bff = tff.replace("TFF: 38 BFF: 0", "TFF: 1 BFF: 30");
        assert_eq!(interlacing_from_idet(&bff), Some(FieldOrder::BottomFirst));
        let progressive = tff.replace(
            "TFF: 38 BFF: 0 Progressive: 2",
            "TFF: 3 BFF: 0 Progressive: 40",
        );
        assert_eq!(interlacing_from_idet(&progressive), None);
        let unsure = tff.replace(
            "TFF: 38 BFF: 0 Progressive: 2 Undetermined: 10",
            "TFF: 12 BFF: 0 Progressive: 5 Undetermined: 40",
        );
        assert_eq!(interlacing_from_idet(&unsure), None);
        let close = tff.replace(
            "TFF: 38 BFF: 0 Progressive: 2",
            "TFF: 20 BFF: 0 Progressive: 15",
        );
        assert_eq!(interlacing_from_idet(&close), None);
        assert_eq!(interlacing_from_idet("nothing here"), None);
    }

    #[test]
    fn listings_yield_names_and_software_encoders_follow_them() {
        let encoders = "Encoders:\n V..... = Video\n ------\n V....D libx264              libx264 H.264\n A....D aac                  AAC\n V..... h264_nvenc           NVIDIA\n";
        let names = parse_listing(encoders);
        assert!(names.contains("libx264"));
        assert!(names.contains("aac"));
        assert!(names.contains("h264_nvenc"));
        assert!(!names.contains("Video"));
        let filters = "Filters:\n  T.. = Timeline support\n ... zscale            V->V       Apply resizing\n .SC v360              V->V       Convert 360\n";
        let names = parse_listing(filters);
        assert!(names.contains("zscale"));
        assert!(names.contains("v360"));
        let caps = Capabilities {
            version: String::new(),
            encoders: ["libx264", "libaom-av1", "libsvtav1"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            filters: Default::default(),
        };
        assert_eq!(software_encoder(&VideoCodec::H264, &caps), Some("libx264"));
        assert_eq!(software_encoder(&VideoCodec::Av1, &caps), Some("libsvtav1"));
        assert_eq!(software_encoder(&VideoCodec::H265, &caps), None);
        assert_eq!(
            Hardware::Nvenc.encoder_name(&VideoCodec::H265).as_deref(),
            Some("hevc_nvenc")
        );
        assert_eq!(Hardware::Videotoolbox.encoder_name(&VideoCodec::Av1), None);
        assert_eq!(Hardware::Vaapi.upload_filters(), "format=nv12,hwupload");
        assert_eq!(
            Hardware::Nvenc.device_args(Path::new("/dev/dri/renderD128")),
            Vec::<std::ffi::OsString>::new()
        );
    }

    #[test]
    fn a_test_render_names_its_font_or_the_lack_of_one() {
        let ok = "[Parsed_subtitles_0 @ 0x1] Using font provider fontconfig\n[Parsed_subtitles_0 @ 0x1] fontselect: (Arial, 400, 0) -> /usr/share/fonts/liberation/LiberationSans-Regular.ttf, 0, LiberationSans\n";
        assert_eq!(
            fonts_from_render(ok).unwrap(),
            "/usr/share/fonts/liberation/LiberationSans-Regular.ttf"
        );
        let none = "[Parsed_subtitles_0 @ 0x1] Failed to load fonctconfig fonts!\n[Parsed_subtitles_0 @ 0x1] Using font provider fontconfig\n";
        assert!(
            fonts_from_render(none)
                .unwrap_err()
                .to_string()
                .contains("no fonts")
        );
        assert!(fonts_from_render("").is_err());
    }
}
