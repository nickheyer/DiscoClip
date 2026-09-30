use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::archive::ArchiveConfig;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineConfig {
    pub cache_dir: PathBuf,
    /// An ffmpeg installed on the machine to run instead of the embedded build, with its
    /// ffprobe beside it or on `PATH`. Unset, the embedded build is unpacked and run.
    pub ffmpeg: Option<PathBuf>,
    pub workers: usize,
    pub limits: Limits,
    pub archive: Option<ArchiveConfig>,
    pub playlists: PlaylistConfig,
    pub live: LiveConfig,
    pub download: DownloadConfig,
    pub browser: BrowserConfig,
    pub retention: RetentionConfig,
    pub transcode: TranscodeConfig,
    pub shutdown: ShutdownConfig,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            cache_dir: PathBuf::from("cache"),
            ffmpeg: None,
            workers: 2,
            limits: Limits::default(),
            archive: None,
            playlists: PlaylistConfig::default(),
            live: LiveConfig::default(),
            download: DownloadConfig::default(),
            browser: BrowserConfig::default(),
            retention: RetentionConfig::default(),
            transcode: TranscodeConfig::default(),
            shutdown: ShutdownConfig::default(),
        }
    }
}

/// Which video encoders the transcoder may run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncoderChoice {
    /// The first hardware family whose encoders run on this machine, else software.
    #[default]
    Auto,
    /// libx264 and its kin, whatever the machine has.
    Software,
    /// NVIDIA NVENC.
    Nvenc,
    /// VA-API: Intel and AMD GPUs on Linux.
    Vaapi,
    /// Intel Quick Sync Video.
    Qsv,
    /// Apple VideoToolbox.
    Videotoolbox,
    /// AMD AMF, on Windows.
    Amf,
    /// Video4Linux memory-to-memory encoders: Raspberry Pi and other boards.
    V4l2m2m,
}

impl EncoderChoice {
    pub const ALL: [EncoderChoice; 8] = [
        EncoderChoice::Auto,
        EncoderChoice::Software,
        EncoderChoice::Nvenc,
        EncoderChoice::Vaapi,
        EncoderChoice::Qsv,
        EncoderChoice::Videotoolbox,
        EncoderChoice::Amf,
        EncoderChoice::V4l2m2m,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            EncoderChoice::Auto => "auto",
            EncoderChoice::Software => "software",
            EncoderChoice::Nvenc => "nvenc",
            EncoderChoice::Vaapi => "vaapi",
            EncoderChoice::Qsv => "qsv",
            EncoderChoice::Videotoolbox => "videotoolbox",
            EncoderChoice::Amf => "amf",
            EncoderChoice::V4l2m2m => "v4l2m2m",
        }
    }
}

/// How video is encoded: which encoders may run, and the device they run on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TranscodeConfig {
    pub encoder: EncoderChoice,
    /// The render node VA-API encoders open.
    pub vaapi_device: PathBuf,
}

impl Default for TranscodeConfig {
    fn default() -> Self {
        Self {
            encoder: EncoderChoice::Auto,
            vaapi_device: PathBuf::from("/dev/dri/renderD128"),
        }
    }
}

/// How the engine stops: jobs under way get this long to finish before they are
/// interrupted and queued again for the next start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ShutdownConfig {
    pub grace_secs: u64,
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self { grace_secs: 30 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_source_bytes: u64,
    pub max_duration_secs: Option<u64>,
    pub max_height: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_source_bytes: 2 * 1024 * 1024 * 1024,
            max_duration_secs: Some(3 * 60 * 60),
            max_height: 1080,
        }
    }
}

/// How links to playlists and channels are handled: each entry becomes a job of its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlaylistConfig {
    pub enabled: bool,
    /// The most entries one playlist link expands into.
    pub max_entries: usize,
}

impl Default for PlaylistConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_entries: 50,
        }
    }
}

/// How live streams are captured: from the moment the link is seen, for at most this long.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LiveConfig {
    pub max_capture_secs: u64,
}

impl Default for LiveConfig {
    fn default() -> Self {
        Self {
            max_capture_secs: 3 * 60 * 60,
        }
    }
}

/// How files served over plain HTTP are fetched: in ranged chunks over several
/// connections when the host serves byte ranges, and picked up from where a broken
/// transfer stopped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DownloadConfig {
    /// Connections fetching one file at the same time, when the host serves byte ranges.
    pub connections: usize,
    /// Bytes each ranged request asks for.
    pub chunk_bytes: u64,
    /// How many times a transfer that breaks is picked up from where it stopped before
    /// the download fails. `0` fails at the first break.
    pub resume_attempts: u32,
}

impl Default for DownloadConfig {
    fn default() -> Self {
        Self {
            connections: 4,
            chunk_bytes: 8 * 1024 * 1024,
            resume_attempts: 5,
        }
    }
}

/// The headless browser that captures pages whose player feeds a media source extension
/// rather than playing a file or a manifest the engine could fetch itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserConfig {
    /// The Chromium or Chrome executable. Unset, the first found among the usual names
    /// on `PATH` and the usual install locations is used.
    pub executable: Option<PathBuf>,
    /// Arguments added to the browser's command line, such as `--no-sandbox` where the
    /// sandbox cannot be set up.
    pub args: Vec<String>,
}

/// How long finished jobs and their cached files are kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
    /// Finished jobs older than this are removed. `0` keeps them forever.
    pub jobs_days: u32,
    /// Failed and cancelled jobs older than this are removed. `0` keeps them forever.
    pub failed_jobs_days: u32,
    /// The cache directory is trimmed back under this many bytes. `0` never trims.
    pub cache_max_bytes: u64,
    /// How often retention runs.
    pub sweep_interval_secs: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            jobs_days: 90,
            failed_jobs_days: 30,
            cache_max_bytes: 20 * 1024 * 1024 * 1024,
            sweep_interval_secs: 3600,
        }
    }
}
