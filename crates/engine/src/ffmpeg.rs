//! Runs the FFmpeg and FFprobe binaries embedded at build time.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use crate::media::{AudioCodec, AudioTrack, Container, MediaInfo, VideoCodec, VideoTrack};

include!(concat!(env!("OUT_DIR"), "/ffmpeg_embed.rs"));

const STDERR_TAIL: usize = 16 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum FfmpegError {
    #[error("ffmpeg could not be installed: {0}")]
    Install(String),
    #[error("ffmpeg failed: {0}")]
    Process(String),
    #[error("could not read media: {0}")]
    Probe(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Where the tools live; shared by every clone, so a relocation reaches them all.
#[derive(Debug, Clone)]
struct Tools {
    ffmpeg: PathBuf,
    ffprobe: PathBuf,
}

/// The embedded tools, unpacked under a cache directory and moved when it changes.
#[derive(Debug, Clone)]
pub struct Ffmpeg {
    tools: Arc<RwLock<Tools>>,
    /// Held while unpacking, so two relocations at once do not race over the files.
    unpacking: Arc<tokio::sync::Mutex<()>>,
}

pub struct Output {
    pub stderr: String,
    pub last_time: Option<Duration>,
}

impl Ffmpeg {
    /// Unpacks the embedded binaries into `cache_dir` (once per build) and verifies they run.
    pub async fn provision(cache_dir: &Path) -> Result<Self, FfmpegError> {
        let tools = unpack(cache_dir).await?;
        let handle = Self {
            tools: Arc::new(RwLock::new(tools)),
            unpacking: Arc::new(tokio::sync::Mutex::new(())),
        };
        let banner = handle.version().await?;
        tracing::info!(dir = %handle.dir().display(), "{banner}");
        Ok(handle)
    }

    /// Unpacks the binaries under another cache directory and uses them from then on;
    /// every clone of this handle follows.
    pub async fn relocate(&self, cache_dir: &Path) -> Result<(), FfmpegError> {
        let _unpacking = self.unpacking.lock().await;
        let tools = unpack(cache_dir).await?;
        let banner = version_of(&tools.ffmpeg).await?;
        let dir = tools
            .ffmpeg
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default();
        *self.tools.write().unwrap_or_else(|e| e.into_inner()) = tools;
        tracing::info!(dir = %dir.display(), "{banner}");
        Ok(())
    }

    fn tools(&self) -> Tools {
        self.tools.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The cache directory the tools are unpacked under.
    pub fn cache_dir(&self) -> PathBuf {
        self.dir()
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_default()
    }

    /// The directory the tools are unpacked in.
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
    /// readable progress are added; `on_time` receives the output timestamp as encoding advances.
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
        while let Some(line) = lines.next_line().await? {
            if let Some(value) = line
                .strip_prefix("out_time_us=")
                .or_else(|| line.strip_prefix("out_time_ms="))
                && let Ok(us) = value.trim().parse::<u64>()
            {
                let t = Duration::from_micros(us);
                last_time = Some(t);
                on_time(t);
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
        Ok(Output { stderr, last_time })
    }

    /// Reads container, duration, and the first video and audio streams with ffprobe.
    pub async fn probe(&self, path: &Path) -> Result<MediaInfo, FfmpegError> {
        let out = Command::new(self.ffprobe_path())
            .args([
                "-v",
                "error",
                "-print_format",
                "json",
                "-show_format",
                "-show_streams",
            ])
            .arg(path)
            .stdin(std::process::Stdio::null())
            .output()
            .await?;
        if !out.status.success() {
            return Err(FfmpegError::Probe(summarize(&String::from_utf8_lossy(
                &out.stderr,
            ))));
        }
        let report: ProbeReport = serde_json::from_slice(&out.stdout)
            .map_err(|e| FfmpegError::Probe(format!("ffprobe output: {e}")))?;
        let mut info = media_info(&report, path);
        if info.duration.is_none() {
            info.duration = self.measure_duration(path).await?;
        }
        Ok(info)
    }

    /// Decodes the video stream to nowhere and reads the final timestamp, for containers
    /// that do not declare a duration.
    async fn measure_duration(&self, path: &Path) -> Result<Option<Duration>, FfmpegError> {
        let args: Vec<OsString> = vec![
            "-loglevel".into(),
            "error".into(),
            "-i".into(),
            path.as_os_str().to_owned(),
            "-map".into(),
            "0:v:0".into(),
            "-c".into(),
            "copy".into(),
            "-f".into(),
            "null".into(),
            "-".into(),
        ];
        let output = self.run(args, |_| {}).await?;
        Ok(output.last_time.filter(|t| !t.is_zero()))
    }
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
    Ok(tools)
}

async fn version_of(ffmpeg: &Path) -> Result<String, FfmpegError> {
    let out = Command::new(ffmpeg)
        .arg("-version")
        .stdin(std::process::Stdio::null())
        .output()
        .await?;
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

fn summarize(stderr: &str) -> String {
    let lines: Vec<&str> = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
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
    codec_type: String,
    codec_name: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
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
}

#[derive(Deserialize)]
struct SideData {
    rotation: Option<f64>,
}

impl ProbeStream {
    fn codec(&self) -> &str {
        self.codec_name.as_deref().unwrap_or("")
    }

    fn is_moving_picture(&self) -> bool {
        self.codec_type == "video"
            && self.disposition.attached_pic == 0
            && !matches!(
                self.codec(),
                "png" | "mjpeg" | "bmp" | "tiff" | "webp" | "jpeg2000"
            )
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
}

fn media_info(report: &ProbeReport, path: &Path) -> MediaInfo {
    let video = report
        .streams
        .iter()
        .find(|s| s.is_moving_picture())
        .and_then(video_track);
    let audio = report
        .streams
        .iter()
        .find(|s| s.codec_type == "audio")
        .map(audio_track);
    let duration = report
        .format
        .duration
        .as_deref()
        .and_then(|d| d.parse::<f64>().ok())
        .filter(|d| *d > 0.0)
        .map(Duration::from_secs_f64);
    MediaInfo {
        container: container_from_formats(&report.format.format_name, path),
        duration,
        video,
        audio,
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
    Some(VideoTrack {
        codec: video_codec(stream.codec()),
        width,
        height,
        fps,
        bitrate: stream.bitrate(),
    })
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
    } else {
        Container::Other(list.first().copied().unwrap_or("unknown").to_string())
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
        other => AudioCodec::Other(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{ProbeReport, media_info};
    use crate::media::{AudioCodec, Container, VideoCodec};

    const REPORT: &str = r#"{
      "streams": [
        {"index": 0, "codec_name": "mjpeg", "codec_type": "video", "width": 600, "height": 600,
         "avg_frame_rate": "0/0", "r_frame_rate": "90000/1", "disposition": {"attached_pic": 1}},
        {"index": 1, "codec_name": "h264", "codec_type": "video", "width": 1920, "height": 1080,
         "avg_frame_rate": "30000/1001", "r_frame_rate": "30000/1001", "bit_rate": "4000000",
         "side_data_list": [{"side_data_type": "Display Matrix", "displaymatrix": "...", "rotation": -90}]},
        {"index": 2, "codec_name": "aac", "codec_type": "audio", "sample_rate": "44100", "channels": 2,
         "bit_rate": "128000", "avg_frame_rate": "0/0", "r_frame_rate": "0/0"}
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
        assert_eq!((video.width, video.height), (1080, 1920));
        assert!((video.fps.unwrap() - 29.97).abs() < 0.01);
        assert_eq!(video.bitrate, Some(4_000_000));
        let audio = info.audio.unwrap();
        assert_eq!(audio.codec, AudioCodec::Aac);
        assert_eq!((audio.channels, audio.sample_rate), (2, 44_100));
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
}
