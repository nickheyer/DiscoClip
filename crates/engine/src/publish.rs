use async_trait::async_trait;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::job::{Job, Origin, SourceId};
use crate::media::{AudioCodec, Container, LocalFile, VideoCodec};

#[async_trait]
pub trait Publisher: Send + Sync {
    fn source(&self) -> &SourceId;
    async fn constraints(&self, origin: &Origin) -> Result<Constraints, PublishError>;
    async fn publish(&self, job: &Job, file: &LocalFile) -> Result<Published, PublishError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constraints {
    pub max_bytes: u64,
    pub containers: Vec<Container>,
    pub video_codecs: Vec<VideoCodec>,
    pub audio_codecs: Vec<AudioCodec>,
    /// A picture height the destination caps at, below the engine's own limit.
    #[serde(default)]
    pub max_height: Option<u32>,
}

impl Constraints {
    /// H.264 video and AAC audio in MP4: playable by every mainstream client.
    pub fn universal(max_bytes: u64) -> Self {
        Self {
            max_bytes,
            containers: vec![Container::Mp4],
            video_codecs: vec![VideoCodec::H264],
            audio_codecs: vec![AudioCodec::Aac],
            max_height: None,
        }
    }

    pub fn preferred_container(&self) -> Container {
        self.containers.first().cloned().unwrap_or(Container::Mp4)
    }

    pub fn preferred_video(&self) -> VideoCodec {
        self.video_codecs
            .first()
            .cloned()
            .unwrap_or(VideoCodec::H264)
    }

    pub fn preferred_audio(&self) -> AudioCodec {
        self.audio_codecs
            .first()
            .cloned()
            .unwrap_or(AudioCodec::Aac)
    }

    pub fn accepts_container(&self, container: &Container) -> bool {
        self.containers.is_empty() && *container == Container::Mp4
            || self.containers.contains(container)
    }

    pub fn accepts_video(&self, codec: &VideoCodec) -> bool {
        self.video_codecs.is_empty() && *codec == VideoCodec::H264
            || self.video_codecs.contains(codec)
    }

    pub fn accepts_audio(&self, codec: &AudioCodec) -> bool {
        self.audio_codecs.is_empty() && *codec == AudioCodec::Aac
            || self.audio_codecs.contains(codec)
    }
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
