use std::path::PathBuf;

use async_trait::async_trait;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::job::Job;
use crate::media::LocalFile;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveConfig {
    pub dir: PathBuf,
}

#[async_trait]
pub trait Archiver: Send + Sync {
    async fn archive(&self, job: &Job, file: &LocalFile) -> Result<ArchiveEntry, ArchiveError>;
}

pub struct FsArchiver {
    config: ArchiveConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveEntry {
    pub path: PathBuf,
    pub size: u64,
    pub at: Timestamp,
}

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
