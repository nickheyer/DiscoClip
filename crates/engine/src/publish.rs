use async_trait::async_trait;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::job::{Job, SourceId};
use crate::media::{AudioCodec, Container, LocalFile, MediaKind, VideoCodec};

#[async_trait]
pub trait Publisher: Send + Sync {
    fn source(&self) -> &SourceId;
    /// What the destination of `job` takes. The job is resolved by then, so the platform
    /// and the media are known.
    async fn constraints(&self, job: &Job) -> Result<Constraints, PublishError>;
    /// Delivers `file`, or a link to it when the job's delivery is a link.
    async fn publish(&self, job: &Job, file: &LocalFile) -> Result<Published, PublishError>;
}

/// The least a video may be reduced to before a link to the full one is posted instead
/// of the reduced upload. Each bound is measured against the source too: a source below
/// the bound is not reduced by being kept as it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualityFloor {
    /// Picture height in pixels.
    pub min_height: u32,
    /// Bits per second over the whole file.
    pub min_bitrate: u64,
}

/// Where the media goes when it cannot be uploaded well: a page that plays it, linked
/// from the destination instead. The output made for the page is bounded by `max_bytes`
/// rather than the destination's limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fallback {
    pub max_bytes: u64,
    pub floor: QualityFloor,
}

/// What a destination takes: the byte limit, and for each kind of media the formats it
/// plays as they are. Video is described by container and codecs. Audio alone and images
/// by the containers they may arrive in. Other files by whether they are taken at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Constraints {
    pub max_bytes: u64,
    pub containers: Vec<Container>,
    pub video_codecs: Vec<VideoCodec>,
    pub audio_codecs: Vec<AudioCodec>,
    /// A picture height the destination caps at, below the engine's own limit.
    #[serde(default)]
    pub max_height: Option<u32>,
    /// The containers audio-only media is published in as it is. Empty when the
    /// destination takes no audio.
    #[serde(default)]
    pub audio_containers: Vec<Container>,
    /// The formats still images are published in as they are. Empty when the destination
    /// takes no images.
    #[serde(default)]
    pub image_containers: Vec<Container>,
    /// Whether files that are neither video, audio nor images are taken.
    #[serde(default)]
    pub files: bool,
    /// A link to post instead of an upload that would be too large or too reduced.
    #[serde(default)]
    pub fallback: Option<Fallback>,
}

impl Constraints {
    /// H.264 video and AAC audio in MP4, AAC in M4A, MP3 and Ogg for audio alone, JPEG,
    /// PNG, WebP and GIF for images, and any other file: what every mainstream client
    /// plays or shows.
    pub fn universal(max_bytes: u64) -> Self {
        Self {
            max_bytes,
            containers: vec![Container::Mp4],
            video_codecs: vec![VideoCodec::H264],
            audio_codecs: vec![AudioCodec::Aac],
            max_height: None,
            audio_containers: vec![Container::M4a, Container::Mp3, Container::Ogg],
            image_containers: vec![
                Container::Jpeg,
                Container::Png,
                Container::Webp,
                Container::Gif,
            ],
            files: true,
            fallback: None,
        }
    }

    /// The same destination as it stands for a link: the fallback's byte bound in place
    /// of the upload limit, and no further fallback.
    pub fn for_link(&self) -> Option<Constraints> {
        let fallback = self.fallback.as_ref()?;
        Some(Constraints {
            max_bytes: fallback.max_bytes,
            fallback: None,
            ..self.clone()
        })
    }

    /// Whether media of `kind` is taken at all.
    pub fn accepts(&self, kind: MediaKind) -> bool {
        match kind {
            MediaKind::Video => true,
            MediaKind::Audio => !self.audio_containers.is_empty(),
            MediaKind::Image => !self.image_containers.is_empty(),
            MediaKind::File => self.files,
        }
    }

    /// The container audio alone is encoded into when it has to be.
    pub fn preferred_audio_container(&self) -> Container {
        self.audio_containers
            .first()
            .cloned()
            .unwrap_or(Container::M4a)
    }

    /// The format an image is encoded into when it has to be.
    pub fn preferred_image_container(&self) -> Container {
        self.image_containers
            .first()
            .cloned()
            .unwrap_or(Container::Jpeg)
    }

    /// Whether an audio-only file in `container` holding `codec` is published as it is.
    pub fn accepts_audio_file(&self, container: &Container, codec: Option<&AudioCodec>) -> bool {
        self.audio_containers.contains(container)
            && codec.is_none_or(|codec| audio_codec_fits(container, codec))
    }

    pub fn accepts_image(&self, container: &Container) -> bool {
        self.image_containers.contains(container)
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

/// The codec an audio container is expected to hold for every client to play it.
pub fn audio_codec_fits(container: &Container, codec: &AudioCodec) -> bool {
    match container {
        Container::M4a => matches!(codec, AudioCodec::Aac),
        Container::Mp3 => matches!(codec, AudioCodec::Mp3),
        Container::Ogg => matches!(codec, AudioCodec::Vorbis | AudioCodec::Opus),
        Container::Opus => matches!(codec, AudioCodec::Opus),
        Container::Flac => matches!(codec, AudioCodec::Flac),
        Container::Wav => matches!(codec, AudioCodec::Other(name) if name.starts_with("pcm_")),
        _ => false,
    }
}

/// The codec audio is encoded with for `container`.
pub fn audio_codec_for(container: &Container) -> AudioCodec {
    match container {
        Container::Mp3 => AudioCodec::Mp3,
        Container::Ogg | Container::Opus => AudioCodec::Opus,
        Container::Flac => AudioCodec::Flac,
        _ => AudioCodec::Aac,
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
