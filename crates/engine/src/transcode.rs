use std::path::Path;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::event::ProgressSender;
use crate::media::{AudioCodec, Container, LocalFile, MediaInfo, VideoCodec};

#[async_trait]
pub trait Transcoder: Send + Sync {
    async fn probe(&self, path: &Path) -> Result<MediaInfo, TranscodeError>;
    async fn transcode(
        &self,
        input: &LocalFile,
        target: &Target,
        dest: &Path,
        progress: ProgressSender,
    ) -> Result<LocalFile, TranscodeError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub container: Container,
    pub video: VideoCodec,
    pub audio: Option<AudioCodec>,
    pub max_bytes: u64,
    pub max_height: Option<u32>,
}

#[derive(Debug, thiserror::Error)]
pub enum TranscodeError {
    #[error("could not read media: {0}")]
    Probe(String),
    #[error("unsupported input: {container:?} with {codec:?}")]
    Unsupported { container: Container, codec: String },
    #[error("cannot fit output under {max_bytes} bytes")]
    BudgetUnreachable { max_bytes: u64 },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
