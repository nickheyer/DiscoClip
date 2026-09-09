use std::path::PathBuf;

use serde::Deserialize;

use crate::archive::ArchiveConfig;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EngineConfig {
    pub data_dir: PathBuf,
    pub cache_dir: PathBuf,
    pub workers: usize,
    pub limits: Limits,
    pub archive: Option<ArchiveConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub max_source_bytes: u64,
    pub max_duration_secs: Option<u64>,
}
