use std::path::{Path, PathBuf};

use async_trait::async_trait;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::job::Job;
use crate::media::safe_stem;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveConfig {
    pub dir: PathBuf,
    #[serde(default)]
    pub keep: Keep,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Keep {
    #[default]
    Output,
    Source,
    Both,
}

#[async_trait]
pub trait Archiver: Send + Sync {
    async fn archive(&self, job: &Job) -> Result<ArchiveEntry, ArchiveError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveEntry {
    pub files: Vec<PathBuf>,
    pub bytes: u64,
    pub at: Timestamp,
}

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("job has no {0} file to archive")]
    Missing(&'static str),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("could not serialize job record: {0}")]
    Encode(#[from] serde_json::Error),
}

/// Stores media under `dir/YYYY/MM/<job>-<title>.<ext>` next to a JSON record of the job.
pub struct FsArchiver {
    config: ArchiveConfig,
}

impl FsArchiver {
    pub fn new(config: ArchiveConfig) -> Self {
        Self { config }
    }

    fn target_dir(&self, job: &Job) -> PathBuf {
        let zoned = job.created_at.to_zoned(jiff::tz::TimeZone::UTC);
        self.config
            .dir
            .join(format!("{:04}", zoned.year()))
            .join(format!("{:02}", zoned.month()))
    }
}

async fn copy_into(
    src: &Path,
    dir: &Path,
    stem: &str,
    suffix: &str,
) -> Result<(PathBuf, u64), ArchiveError> {
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("bin")
        .to_string();
    let dest = dir.join(format!("{stem}{suffix}.{ext}"));
    let bytes = tokio::fs::copy(src, &dest).await?;
    Ok((dest, bytes))
}

#[async_trait]
impl Archiver for FsArchiver {
    async fn archive(&self, job: &Job) -> Result<ArchiveEntry, ArchiveError> {
        let dir = self.target_dir(job);
        tokio::fs::create_dir_all(&dir).await?;
        let id = job.id.to_string();
        let title = job
            .artifacts
            .resolved
            .as_ref()
            .and_then(|r| r.title.as_deref());
        let stem = format!("{}-{}", &id[..8], safe_stem(title, "video"));

        let mut files = Vec::new();
        let mut bytes = 0;
        let keep = self.config.keep;
        if matches!(keep, Keep::Output | Keep::Both) {
            let output = job
                .artifacts
                .output
                .as_ref()
                .ok_or(ArchiveError::Missing("output"))?;
            let (path, n) = copy_into(&output.path, &dir, &stem, "").await?;
            files.push(path);
            bytes += n;
        }
        if matches!(keep, Keep::Source | Keep::Both) {
            let source = job
                .artifacts
                .source
                .as_ref()
                .ok_or(ArchiveError::Missing("source"))?;
            let suffix = if keep == Keep::Both { "-source" } else { "" };
            let (path, n) = copy_into(&source.path, &dir, &stem, suffix).await?;
            files.push(path);
            bytes += n;
        }

        let record = dir.join(format!("{stem}.json"));
        let json = serde_json::to_vec_pretty(job)?;
        tokio::fs::write(&record, json).await?;
        files.push(record);

        Ok(ArchiveEntry {
            files,
            bytes,
            at: Timestamp::now(),
        })
    }
}
