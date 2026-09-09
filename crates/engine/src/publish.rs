use async_trait::async_trait;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::job::{Origin, SourceId};
use crate::media::{AudioCodec, Container, LocalFile, VideoCodec};
use crate::resolve::Resolved;

#[async_trait]
pub trait Publisher: Send + Sync {
    fn source(&self) -> &SourceId;
    async fn constraints(&self, origin: &Origin) -> Result<Constraints, PublishError>;
    async fn publish(
        &self,
        origin: &Origin,
        file: &LocalFile,
        resolved: &Resolved,
    ) -> Result<Published, PublishError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constraints {
    pub max_bytes: u64,
    pub containers: Vec<Container>,
    pub video_codecs: Vec<VideoCodec>,
    pub audio_codecs: Vec<AudioCodec>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Published {
    pub reference: String,
    pub url: Option<Url>,
    pub at: Timestamp,
}

#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("file is {size} bytes, destination allows {max}")]
    TooLarge { size: u64, max: u64 },
    #[error("destination rejected upload: {0}")]
    Rejected(String),
    #[error("cannot address origin {0}")]
    InvalidOrigin(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
