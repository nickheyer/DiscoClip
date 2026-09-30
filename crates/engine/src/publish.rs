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
    /// Delivers `file`, or a link to it when the job's delivery is a link. A job whose
    /// capture was announced carries the message in `job.artifacts.announced`: the
    /// result goes into that message.
    async fn publish(&self, job: &Job, file: &LocalFile) -> Result<Published, PublishError>;
    /// Tells the destination a live capture has begun, with `recording` growing as it
    /// goes, when the destination has somewhere to say so: a message holding a link to
    /// the page that plays the recording. Publishers with no such place post nothing.
    async fn announce(
        &self,
        job: &Job,
        recording: &LocalFile,
    ) -> Result<Option<Published>, PublishError> {
        let _ = (job, recording);
        Ok(None)
    }
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

/// What a destination's media is made into: the container and codecs video is encoded
/// to, how big and fast the picture may be, and the forms audio, images and other files
/// arrive in. One per destination, and a server can override it per Discord guild.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DestinationTarget {
    pub container: Container,
    pub video_codec: VideoCodec,
    pub audio_codec: AudioCodec,
    /// A picture height the destination caps at, below the engine's own limit.
    pub max_height: Option<u32>,
    /// The most frames per second the picture keeps. Faster sources are slowed to it.
    pub max_fps: Option<u32>,
    /// Whether sound alone is published as a video of the sound playing over its cover
    /// art, the platform's thumbnail, or a waveform, so destinations that play video
    /// inline play it.
    pub audio_over_still: bool,
    /// The containers audio-only media is published in as it is. Empty when the
    /// destination takes no audio as such.
    pub audio_containers: Vec<Container>,
    /// The formats still images are published in as they are. Empty when the destination
    /// takes no images.
    pub image_containers: Vec<Container>,
    /// Whether files that are neither video, audio nor images are taken.
    pub files: bool,
}

impl Default for DestinationTarget {
    /// H.264 video and AAC audio in MP4, AAC in M4A, MP3 and Ogg for audio alone, JPEG,
    /// PNG, WebP and GIF for images, and any other file: what every mainstream client
    /// plays or shows.
    fn default() -> Self {
        Self {
            container: Container::Mp4,
            video_codec: VideoCodec::H264,
            audio_codec: AudioCodec::Aac,
            max_height: None,
            max_fps: None,
            audio_over_still: false,
            audio_containers: vec![Container::M4a, Container::Mp3, Container::Ogg],
            image_containers: vec![
                Container::Jpeg,
                Container::Png,
                Container::Webp,
                Container::Gif,
            ],
            files: true,
        }
    }
}

/// Whether `container` holds video coded as `video` in a form mainstream players open.
pub fn container_takes_video(container: &Container, video: &VideoCodec) -> bool {
    match container {
        Container::Mp4 => matches!(video, VideoCodec::H264 | VideoCodec::H265 | VideoCodec::Av1),
        Container::Mov => matches!(video, VideoCodec::H264 | VideoCodec::H265),
        Container::Mkv => !matches!(video, VideoCodec::Other(_)),
        Container::Webm => matches!(video, VideoCodec::Vp8 | VideoCodec::Vp9 | VideoCodec::Av1),
        _ => false,
    }
}

/// Whether `container` holds sound coded as `audio` beside a picture.
pub fn container_takes_audio(container: &Container, audio: &AudioCodec) -> bool {
    match container {
        Container::Mp4 | Container::Mov => matches!(audio, AudioCodec::Aac | AudioCodec::Mp3),
        Container::Mkv => !matches!(audio, AudioCodec::Other(_)),
        Container::Webm => matches!(audio, AudioCodec::Opus | AudioCodec::Vorbis),
        _ => false,
    }
}

impl DestinationTarget {
    /// Why the target could not be made, when its parts do not go together.
    pub fn check(&self) -> Result<(), String> {
        if !matches!(
            self.container,
            Container::Mp4 | Container::Mov | Container::Mkv | Container::Webm
        ) {
            return Err(format!(
                "container must be mp4, mov, mkv or webm, not {}",
                self.container.extension()
            ));
        }
        if !container_takes_video(&self.container, &self.video_codec) {
            return Err(format!(
                "{} does not hold {:?} video",
                self.container.extension(),
                self.video_codec
            )
            .to_lowercase());
        }
        if !container_takes_audio(&self.container, &self.audio_codec) {
            return Err(format!(
                "{} does not hold {:?} audio",
                self.container.extension(),
                self.audio_codec
            )
            .to_lowercase());
        }
        if self.max_height == Some(0) {
            return Err("max_height must be at least 1".into());
        }
        if self.max_fps == Some(0) {
            return Err("max_fps must be at least 1".into());
        }
        for container in &self.audio_containers {
            if container.kind() != MediaKind::Audio {
                return Err(format!(
                    "{} is not an audio container",
                    container.extension()
                ));
            }
        }
        for container in &self.image_containers {
            if !matches!(
                container,
                Container::Jpeg | Container::Png | Container::Webp | Container::Gif
            ) {
                return Err(format!(
                    "{} is not an image format the encoder writes",
                    container.extension()
                ));
            }
        }
        Ok(())
    }

    /// This target with every value `over` names replaced.
    pub fn with(&self, over: &TargetOverride) -> DestinationTarget {
        DestinationTarget {
            container: over
                .container
                .clone()
                .unwrap_or_else(|| self.container.clone()),
            video_codec: over
                .video_codec
                .clone()
                .unwrap_or_else(|| self.video_codec.clone()),
            audio_codec: over
                .audio_codec
                .clone()
                .unwrap_or_else(|| self.audio_codec.clone()),
            max_height: over.max_height.or(self.max_height),
            max_fps: over.max_fps.or(self.max_fps),
            audio_over_still: over.audio_over_still.unwrap_or(self.audio_over_still),
            audio_containers: over
                .audio_containers
                .clone()
                .unwrap_or_else(|| self.audio_containers.clone()),
            image_containers: over
                .image_containers
                .clone()
                .unwrap_or_else(|| self.image_containers.clone()),
            files: over.files.unwrap_or(self.files),
        }
    }

    /// What a destination with this target takes under `max_bytes`.
    pub fn constraints(&self, max_bytes: u64) -> Constraints {
        Constraints {
            max_bytes,
            containers: vec![self.container.clone()],
            video_codecs: vec![self.video_codec.clone()],
            audio_codecs: vec![self.audio_codec.clone()],
            max_height: self.max_height,
            max_fps: self.max_fps,
            audio_over_still: self.audio_over_still,
            audio_containers: self.audio_containers.clone(),
            image_containers: self.image_containers.clone(),
            files: self.files,
            fallback: None,
        }
    }
}

/// The parts of a [`DestinationTarget`] one place overrides. Each unset value keeps the
/// destination's own.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TargetOverride {
    pub container: Option<Container>,
    pub video_codec: Option<VideoCodec>,
    pub audio_codec: Option<AudioCodec>,
    pub max_height: Option<u32>,
    pub max_fps: Option<u32>,
    pub audio_over_still: Option<bool>,
    pub audio_containers: Option<Vec<Container>>,
    pub image_containers: Option<Vec<Container>>,
    pub files: Option<bool>,
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
    /// The most frames per second the destination takes.
    #[serde(default)]
    pub max_fps: Option<u32>,
    /// Whether sound alone is made into a video of it playing over a still picture.
    #[serde(default)]
    pub audio_over_still: bool,
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
    /// The default [`DestinationTarget`] under `max_bytes`: what every mainstream client
    /// plays or shows.
    pub fn universal(max_bytes: u64) -> Self {
        DestinationTarget::default().constraints(max_bytes)
    }

    /// Whether sound alone is published as sound, or has to become a picture: when the
    /// destination asks for it, or takes no audio files at all.
    pub fn renders_audio_as_video(&self) -> bool {
        self.audio_over_still || self.audio_containers.is_empty()
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
            // Sound is always taken: as a file, or played over a still when the
            // destination takes no sound files.
            MediaKind::Audio => true,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_check_their_parts_and_take_overrides() {
        let target = DestinationTarget::default();
        assert!(target.check().is_ok());
        let webm = DestinationTarget {
            container: Container::Webm,
            video_codec: VideoCodec::Vp9,
            audio_codec: AudioCodec::Opus,
            ..DestinationTarget::default()
        };
        assert!(webm.check().is_ok());
        let wrong = DestinationTarget {
            container: Container::Mp4,
            video_codec: VideoCodec::Vp9,
            ..DestinationTarget::default()
        };
        assert!(wrong.check().unwrap_err().contains("mp4 does not hold vp9"));
        let wrong_audio = DestinationTarget {
            audio_codec: AudioCodec::Opus,
            ..DestinationTarget::default()
        };
        assert!(wrong_audio.check().is_err());
        let not_video = DestinationTarget {
            container: Container::Mp3,
            ..DestinationTarget::default()
        };
        assert!(not_video.check().is_err());
        let bad_image = DestinationTarget {
            image_containers: vec![Container::Avif],
            ..DestinationTarget::default()
        };
        assert!(bad_image.check().is_err());
        let over = TargetOverride {
            max_height: Some(720),
            audio_over_still: Some(true),
            files: Some(false),
            ..TargetOverride::default()
        };
        let overridden = target.with(&over);
        assert_eq!(overridden.max_height, Some(720));
        assert!(overridden.audio_over_still);
        assert!(!overridden.files);
        assert_eq!(overridden.container, Container::Mp4);
        let constraints = overridden.constraints(1000);
        assert_eq!(constraints.max_bytes, 1000);
        assert_eq!(constraints.max_height, Some(720));
        assert!(constraints.renders_audio_as_video());
        assert!(constraints.accepts(MediaKind::Audio));
        assert!(!constraints.accepts(MediaKind::File));
        let no_audio = DestinationTarget {
            audio_containers: Vec::new(),
            ..DestinationTarget::default()
        }
        .constraints(1);
        assert!(no_audio.accepts(MediaKind::Audio));
        assert!(no_audio.renders_audio_as_video());
        assert_eq!(Constraints::universal(5).max_bytes, 5);
        assert!(!Constraints::universal(5).renders_audio_as_video());
        let json = serde_json::to_value(&webm).unwrap();
        assert_eq!(json["container"], "webm");
        assert_eq!(json["video_codec"], "vp9");
        assert_eq!(json["audio_containers"][0], "m4a");
    }
}
