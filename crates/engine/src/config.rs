use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::archive::ArchiveConfig;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct EngineConfig {
    pub cache_dir: PathBuf,
    pub workers: usize,
    pub limits: Limits,
    pub archive: Option<ArchiveConfig>,
    pub playlists: PlaylistConfig,
    pub live: LiveConfig,
    pub retention: RetentionConfig,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            cache_dir: PathBuf::from("cache"),
            workers: 2,
            limits: Limits::default(),
            archive: None,
            playlists: PlaylistConfig::default(),
            live: LiveConfig::default(),
            retention: RetentionConfig::default(),
        }
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

/// How long finished jobs and their cached files are kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
    /// Finished jobs older than this are removed; `0` keeps them forever.
    pub jobs_days: u32,
    /// Failed and cancelled jobs older than this are removed; `0` keeps them forever.
    pub failed_jobs_days: u32,
    /// The cache directory is trimmed back under this many bytes; `0` never trims.
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
