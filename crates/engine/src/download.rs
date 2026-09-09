use std::path::Path;

use async_trait::async_trait;

use crate::event::ProgressSender;
use crate::media::LocalFile;
use crate::resolve::{Variant, VariantKind};

#[async_trait]
pub trait Downloader: Send + Sync {
    fn handles(&self, kind: VariantKind) -> bool;
    async fn download(
        &self,
        variant: &Variant,
        dest: &Path,
        progress: ProgressSender,
    ) -> Result<LocalFile, DownloadError>;
}

pub struct HttpDownloader {
    client: reqwest::Client,
    max_bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("source is {size} bytes, limit is {limit}")]
    TooLarge { size: u64, limit: u64 },
    #[error("server returned status {0}")]
    Status(u16),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
