//! The archive: a copy of every finished job's media kept for good, beside a JSON record
//! of the job, under `dir/YYYY/MM/`. The cache the jobs work in is swept by retention;
//! the archive is not.

use std::path::{Path, PathBuf};
use std::sync::RwLock;

use async_trait::async_trait;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use crate::job::Job;
use crate::media::{LocalFile, safe_stem};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArchiveConfig {
    /// Whether finished jobs are archived at all.
    pub enabled: bool,
    /// Where the archive lives.
    pub dir: PathBuf,
    /// Which of a job's files are kept.
    pub keep: Keep,
}

impl Default for ArchiveConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: PathBuf::from("data/archive"),
            keep: Keep::Output,
        }
    }
}

impl ArchiveConfig {
    /// The archive switched off, as a stored `null` once meant.
    pub fn off() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }
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
    /// Whether finished jobs are archived at all right now.
    fn enabled(&self) -> bool {
        true
    }
    /// Takes new settings while running.
    fn reconfigure(&self, _config: ArchiveConfig) {}
    /// The archived files of `entry` still on disk, sized as they stand
    async fn locate(&self, entry: &ArchiveEntry) -> ArchivedFiles {
        async fn present(path: Option<&PathBuf>) -> Option<LocalFile> {
            let path = path?;
            let size = tokio::fs::metadata(path)
                .await
                .ok()
                .filter(|m| m.is_file())?
                .len();
            Some(LocalFile {
                path: path.clone(),
                size,
                info: None,
            })
        }
        ArchivedFiles {
            output: present(entry.output.as_ref()).await,
            thumbnail: present(entry.thumbnail.as_ref()).await.map(|f| f.path),
            source: present(entry.source.as_ref()).await,
        }
    }
}

/// What of an archive entry is still there to read back
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ArchivedFiles {
    pub output: Option<LocalFile>,
    pub thumbnail: Option<PathBuf>,
    pub source: Option<LocalFile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveEntry {
    /// Every file written: the media kept, its still and the job record.
    pub files: Vec<PathBuf>,
    /// The archived copy of the output, when the archive keeps outputs.
    #[serde(default)]
    pub output: Option<PathBuf>,
    /// The archived copy of the output's still, beside the output.
    #[serde(default)]
    pub thumbnail: Option<PathBuf>,
    /// The archived copy of the source, when the archive keeps sources.
    #[serde(default)]
    pub source: Option<PathBuf>,
    /// The JSON record of the job.
    #[serde(default)]
    pub record: Option<PathBuf>,
    pub bytes: u64,
    pub at: Timestamp,
}

#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("job has no {0} file to archive")]
    Missing(&'static str),
    #[error("archiving is turned off")]
    Disabled,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("could not serialize job record: {0}")]
    Encode(#[from] serde_json::Error),
}

/// Stores media under `dir/YYYY/MM/<job>-<title>.<ext>` next to a JSON record of the job.
/// Its settings can change while it runs.
pub struct FsArchiver {
    config: RwLock<ArchiveConfig>,
}

impl FsArchiver {
    pub fn new(config: ArchiveConfig) -> Self {
        Self {
            config: RwLock::new(config),
        }
    }

    pub fn config(&self) -> ArchiveConfig {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn target_dir(config: &ArchiveConfig, job: &Job) -> PathBuf {
        let zoned = job.created_at.to_zoned(jiff::tz::TimeZone::UTC);
        config
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
    fn enabled(&self) -> bool {
        self.config().enabled
    }

    fn reconfigure(&self, config: ArchiveConfig) {
        *self.config.write().unwrap_or_else(|e| e.into_inner()) = config;
    }

    async fn archive(&self, job: &Job) -> Result<ArchiveEntry, ArchiveError> {
        let config = self.config();
        if !config.enabled {
            return Err(ArchiveError::Disabled);
        }
        let dir = Self::target_dir(&config, job);
        tokio::fs::create_dir_all(&dir).await?;
        let id = job.id.to_string();
        let title = job
            .artifacts
            .resolved
            .as_ref()
            .and_then(|r| r.title.as_deref());
        let stem = format!("{}-{}", &id[..8], safe_stem(title, job.media().as_str()));

        let mut files = Vec::new();
        let mut bytes = 0;
        let keep = config.keep;
        let mut archived_output = None;
        let mut archived_thumbnail = None;
        let mut archived_source = None;
        if matches!(keep, Keep::Output | Keep::Both) {
            let output = job
                .artifacts
                .output
                .as_ref()
                .ok_or(ArchiveError::Missing("output"))?;
            let (path, n) = copy_into(&output.path, &dir, &stem, "").await?;
            files.push(path.clone());
            archived_output = Some(path);
            bytes += n;
            if let Some(thumbnail) = &job.artifacts.thumbnail {
                let (path, n) = copy_into(&thumbnail.path, &dir, &stem, "-thumbnail").await?;
                files.push(path.clone());
                archived_thumbnail = Some(path);
                bytes += n;
            }
        }
        if matches!(keep, Keep::Source | Keep::Both) {
            let source = job
                .artifacts
                .source
                .as_ref()
                .ok_or(ArchiveError::Missing("source"))?;
            let suffix = if keep == Keep::Both { "-source" } else { "" };
            let (path, n) = copy_into(&source.path, &dir, &stem, suffix).await?;
            files.push(path.clone());
            archived_source = Some(path);
            bytes += n;
        }

        let record = dir.join(format!("{stem}.json"));
        let json = serde_json::to_vec_pretty(job)?;
        tokio::fs::write(&record, json).await?;
        files.push(record.clone());

        Ok(ArchiveEntry {
            files,
            output: archived_output,
            thumbnail: archived_thumbnail,
            source: archived_source,

            record: Some(record),
            bytes,
            at: Timestamp::now(),
        })
    }
}
