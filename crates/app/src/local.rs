//! Publishes finished videos to the local file system for jobs submitted from the web app.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use discoclip_engine::job::{Job, Origin, SourceId};
use discoclip_engine::media::{LocalFile, safe_stem};
use discoclip_engine::publish::{Constraints, PublishError, Published, Publisher};
use jiff::Timestamp;

pub const SOURCE_ID: &str = "local";

pub struct LocalPublisher {
    source: SourceId,
    dir: PathBuf,
    max_bytes: u64,
}

impl LocalPublisher {
    pub fn new(dir: PathBuf, max_bytes: u64) -> Self {
        Self {
            source: SourceId::new(SOURCE_ID),
            dir,
            max_bytes,
        }
    }

    /// `<dir>/<short job id>-<title slug>.<ext>`
    fn destination(&self, job: &Job, file: &Path) -> PathBuf {
        let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("mp4");
        let title = job
            .artifacts
            .resolved
            .as_ref()
            .and_then(|r| r.title.as_deref());
        let id = job.id.to_string();
        let stem = format!("{}-{}", &id[..8], safe_stem(title, "video"));
        self.dir.join(format!("{stem}.{ext}"))
    }
}

#[async_trait]
impl Publisher for LocalPublisher {
    fn source(&self) -> &SourceId {
        &self.source
    }

    async fn constraints(&self, _origin: &Origin) -> Result<Constraints, PublishError> {
        Ok(Constraints::universal(self.max_bytes))
    }

    async fn publish(&self, job: &Job, file: &LocalFile) -> Result<Published, PublishError> {
        if file.size > self.max_bytes {
            return Err(PublishError::TooLarge {
                size: file.size,
                max: self.max_bytes,
            });
        }
        let dest = self.destination(job, &file.path);
        tokio::fs::create_dir_all(&self.dir).await?;
        tokio::fs::copy(&file.path, &dest).await?;
        let absolute = tokio::fs::canonicalize(&dest).await.unwrap_or(dest);
        Ok(Published {
            reference: absolute.to_string_lossy().into_owned(),
            url: None,
            at: Timestamp::now(),
        })
    }
}
