//! Publishes finished videos to the local file system for jobs submitted from the web app.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use discoclip_engine::job::{Job, Origin, SourceId};
use discoclip_engine::media::{LocalFile, safe_stem};
use discoclip_engine::publish::{Constraints, PublishError, Published, Publisher};
use jiff::Timestamp;

use crate::settings::LocalConfig;

pub const SOURCE_ID: &str = "local";

/// The `local` settings as the publisher reads them, replaced when they change in the app.
pub type SharedLocalConfig = Arc<RwLock<LocalConfig>>;

pub struct LocalPublisher {
    source: SourceId,
    config: SharedLocalConfig,
}

impl LocalPublisher {
    pub fn new(config: SharedLocalConfig) -> Self {
        Self {
            source: SourceId::new(SOURCE_ID),
            config,
        }
    }

    /// A publisher with settings of its own, for tests and for one-off use.
    pub fn with_config(dir: PathBuf, max_bytes: u64) -> Self {
        Self::new(Arc::new(RwLock::new(LocalConfig { dir, max_bytes })))
    }

    fn config(&self) -> LocalConfig {
        self.config.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// `<dir>/<short job id>-<title slug>.<ext>`
    fn destination(dir: &Path, job: &Job, file: &Path) -> PathBuf {
        let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("mp4");
        let title = job
            .artifacts
            .resolved
            .as_ref()
            .and_then(|r| r.title.as_deref());
        let id = job.id.to_string();
        let stem = format!("{}-{}", &id[..8], safe_stem(title, "video"));
        dir.join(format!("{stem}.{ext}"))
    }
}

#[async_trait]
impl Publisher for LocalPublisher {
    fn source(&self) -> &SourceId {
        &self.source
    }

    async fn constraints(&self, _origin: &Origin) -> Result<Constraints, PublishError> {
        Ok(Constraints::universal(self.config().max_bytes))
    }

    async fn publish(&self, job: &Job, file: &LocalFile) -> Result<Published, PublishError> {
        let config = self.config();
        if file.size > config.max_bytes {
            return Err(PublishError::TooLarge {
                size: file.size,
                max: config.max_bytes,
            });
        }
        let dest = Self::destination(&config.dir, job, &file.path);
        tokio::fs::create_dir_all(&config.dir).await?;
        tokio::fs::copy(&file.path, &dest).await?;
        let absolute = tokio::fs::canonicalize(&dest).await.unwrap_or(dest);
        Ok(Published {
            reference: absolute.to_string_lossy().into_owned(),
            url: None,
            at: Timestamp::now(),
        })
    }
}
