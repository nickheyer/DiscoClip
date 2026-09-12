//! Fetching the chosen variant into the job directory, by delivery kind.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::event::{Progress, ProgressSender};
use crate::http::{Http, HttpError};
use crate::media::{Container, LocalFile};
use crate::resolve::{ClipRange, SubtitleFormat, Variant, VariantKind};

pub mod dash;
pub mod hls;
pub mod segments;
pub mod subtitles;

/// The bounds a job puts on its download.
#[derive(Debug, Clone, PartialEq)]
pub struct DownloadContext {
    /// Bytes the source may occupy on disk.
    pub max_bytes: u64,
    /// The tallest picture worth fetching; manifests offering several pick by this.
    pub max_height: u32,
    /// How long a live stream is captured before it is cut and treated as a recording.
    pub max_live: Duration,
    /// The portion wanted; downloaders that can seek fetch only it.
    pub clip: Option<ClipRange>,
    /// Whose cookies and proxy the requests use.
    pub platform: String,
}

impl DownloadContext {
    pub fn new(max_bytes: u64) -> Self {
        Self {
            max_bytes,
            max_height: crate::config::Limits::default().max_height,
            max_live: Duration::from_secs(3 * 60 * 60),
            clip: None,
            platform: crate::http::WEB_PLATFORM.to_string(),
        }
    }
}

/// A subtitle file fetched next to the media.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalSubtitle {
    pub language: String,
    pub name: Option<String>,
    pub path: PathBuf,
    pub format: SubtitleFormat,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Downloaded {
    pub file: LocalFile,
    pub subtitles: Vec<LocalSubtitle>,
}

impl Downloaded {
    pub fn file(file: LocalFile) -> Self {
        Self {
            file,
            subtitles: Vec::new(),
        }
    }
}

#[async_trait]
pub trait Downloader: Send + Sync {
    fn handles(&self, kind: VariantKind) -> bool;
    async fn download(
        &self,
        variant: &Variant,
        dest_dir: &Path,
        context: &DownloadContext,
        progress: ProgressSender,
    ) -> Result<Downloaded, DownloadError>;
}

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("source is {size} bytes, limit is {limit}")]
    TooLarge { size: u64, limit: u64 },
    #[error("server returned status {status} for {url}")]
    Status { status: u16, url: String },
    #[error("download produced no data")]
    Empty,
    #[error("{0}")]
    Manifest(String),
    #[error("{0}")]
    Process(String),
    #[error("{0} is protected by {1} DRM")]
    Drm(String, String),
    #[error("{0}")]
    Unsupported(String),
    #[error(transparent)]
    Dash(#[from] dash_mpd::DashMpdError),
    #[error(transparent)]
    Http(#[from] HttpError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub fn extension_for(variant: &Variant, content_type: Option<&str>) -> String {
    if let Some(container) = &variant.container {
        return container.extension().to_string();
    }
    if let Some(container) = content_type.and_then(Container::from_mime) {
        return container.extension().to_string();
    }
    let from_path = Path::new(variant.url.path())
        .extension()
        .and_then(|e| e.to_str())
        .and_then(Container::from_extension);
    match from_path {
        Some(container) => container.extension().to_string(),
        None => "bin".to_string(),
    }
}

/// Streams a single media file over HTTP into the job directory.
pub struct HttpDownloader {
    http: Http,
}

impl HttpDownloader {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Downloader for HttpDownloader {
    fn handles(&self, kind: VariantKind) -> bool {
        kind == VariantKind::File
    }

    async fn download(
        &self,
        variant: &Variant,
        dest_dir: &Path,
        context: &DownloadContext,
        progress: ProgressSender,
    ) -> Result<Downloaded, DownloadError> {
        let response = self
            .http
            .get(variant.url.clone())
            .platform(&context.platform)
            .headers(&variant.headers)
            .media()
            .send()
            .await?;
        let status = response.status;
        if !status.is_success() {
            return Err(DownloadError::Status {
                status: status.as_u16(),
                url: variant.url.to_string(),
            });
        }
        let total = response.content_length();
        if let Some(size) = total
            && size > context.max_bytes
        {
            return Err(DownloadError::TooLarge {
                size,
                limit: context.max_bytes,
            });
        }
        let content_type = response.content_type().map(str::to_owned);
        let ext = extension_for(variant, content_type.as_deref());
        let path: PathBuf = dest_dir.join(format!("source.{ext}"));
        tokio::fs::create_dir_all(dest_dir).await?;
        let mut file = tokio::fs::File::create(&path).await?;
        let mut stream = response.into_stream();
        let mut done = 0u64;
        progress.send_replace(Progress { done, total });
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            done += chunk.len() as u64;
            if done > context.max_bytes {
                drop(file);
                let _ = tokio::fs::remove_file(&path).await;
                return Err(DownloadError::TooLarge {
                    size: done,
                    limit: context.max_bytes,
                });
            }
            file.write_all(&chunk).await?;
            progress.send_replace(Progress { done, total });
        }
        file.flush().await?;
        drop(file);
        if done == 0 {
            let _ = tokio::fs::remove_file(&path).await;
            return Err(DownloadError::Empty);
        }
        Ok(Downloaded::file(LocalFile {
            path,
            size: done,
            info: None,
        }))
    }
}
