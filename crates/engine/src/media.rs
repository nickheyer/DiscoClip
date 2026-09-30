use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// What a piece of media is: the kind decides how it is picked, shrunk and shown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    /// A moving picture, with or without sound. Animated GIFs count.
    #[default]
    Video,
    /// Sound alone: a track, a podcast episode, a sound bite.
    Audio,
    /// A still picture.
    Image,
    /// Any other file: a document, an archive, a binary.
    File,
}

impl MediaKind {
    pub const ALL: [MediaKind; 4] = [
        MediaKind::Video,
        MediaKind::Audio,
        MediaKind::Image,
        MediaKind::File,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            MediaKind::Video => "video",
            MediaKind::Audio => "audio",
            MediaKind::Image => "image",
            MediaKind::File => "file",
        }
    }

    /// The kind a file's extension names, for hosts that serve any file.
    pub fn from_extension(ext: &str) -> MediaKind {
        Container::from_extension(ext).map_or(MediaKind::File, |c| c.kind())
    }

    /// The kind a served content type names.
    pub fn from_mime(mime: &str) -> MediaKind {
        if let Some(container) = Container::from_mime(mime) {
            return container.kind();
        }
        let essence = mime_essence(mime);
        match essence.split_once('/').map(|(t, _)| t) {
            Some("video") => MediaKind::Video,
            Some("audio") => MediaKind::Audio,
            Some("image") => MediaKind::Image,
            _ => MediaKind::File,
        }
    }
}

impl std::fmt::Display for MediaKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for MediaKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        MediaKind::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| format!("unknown media kind {s}"))
    }
}

fn mime_essence(mime: &str) -> String {
    mime.split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

/// The format a file is in: a video container, an audio container, an image format, or
/// anything else by its extension.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Container {
    Mp4,
    Webm,
    Mkv,
    Mov,
    Ts,
    Flv,
    Avi,
    Gif,
    Mp3,
    M4a,
    Ogg,
    Opus,
    Flac,
    Wav,
    Jpeg,
    Png,
    Webp,
    Avif,
    Other(String),
}

const AUDIO_EXTENSIONS: &[&str] = &[
    "mp3", "m4a", "aac", "ogg", "oga", "opus", "flac", "wav", "aiff", "aif", "wma", "alac", "m4b",
    "weba", "mka", "ac3", "amr", "mid", "midi", "ape", "wv",
];
const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "jpe", "png", "webp", "avif", "bmp", "tif", "tiff", "heic", "heif", "jxl",
    "svg", "ico", "psd",
];
const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "m4v", "webm", "mkv", "mov", "ts", "m2ts", "mts", "flv", "avi", "gif", "ogv", "3gp",
    "3g2", "wmv", "mpg", "mpeg", "vob", "f4v", "mxf", "rm", "rmvb", "asf", "divx", "apng",
];

impl Container {
    pub fn extension(&self) -> &str {
        match self {
            Container::Mp4 => "mp4",
            Container::Webm => "webm",
            Container::Mkv => "mkv",
            Container::Mov => "mov",
            Container::Ts => "ts",
            Container::Flv => "flv",
            Container::Avi => "avi",
            Container::Gif => "gif",
            Container::Mp3 => "mp3",
            Container::M4a => "m4a",
            Container::Ogg => "ogg",
            Container::Opus => "opus",
            Container::Flac => "flac",
            Container::Wav => "wav",
            Container::Jpeg => "jpg",
            Container::Png => "png",
            Container::Webp => "webp",
            Container::Avif => "avif",
            Container::Other(name) => name.as_str(),
        }
    }

    /// What a file in this format is.
    pub fn kind(&self) -> MediaKind {
        match self {
            Container::Mp4
            | Container::Webm
            | Container::Mkv
            | Container::Mov
            | Container::Ts
            | Container::Flv
            | Container::Avi
            | Container::Gif => MediaKind::Video,
            Container::Mp3
            | Container::M4a
            | Container::Ogg
            | Container::Opus
            | Container::Flac
            | Container::Wav => MediaKind::Audio,
            Container::Jpeg | Container::Png | Container::Webp | Container::Avif => {
                MediaKind::Image
            }
            Container::Other(name) => {
                let ext = name.to_ascii_lowercase();
                if VIDEO_EXTENSIONS.contains(&ext.as_str()) {
                    MediaKind::Video
                } else if AUDIO_EXTENSIONS.contains(&ext.as_str()) {
                    MediaKind::Audio
                } else if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
                    MediaKind::Image
                } else {
                    MediaKind::File
                }
            }
        }
    }

    /// The format the extension names, when it is one this crate knows by name.
    pub fn from_extension(ext: &str) -> Option<Container> {
        Some(match ext.to_ascii_lowercase().as_str() {
            "mp4" | "m4v" => Container::Mp4,
            "webm" => Container::Webm,
            "mkv" => Container::Mkv,
            "mov" => Container::Mov,
            "ts" | "m2ts" | "mts" => Container::Ts,
            "flv" => Container::Flv,
            "avi" => Container::Avi,
            "gif" => Container::Gif,
            "mp3" => Container::Mp3,
            "m4a" | "aac" | "m4b" => Container::M4a,
            "ogg" | "oga" => Container::Ogg,
            "opus" => Container::Opus,
            "flac" => Container::Flac,
            "wav" => Container::Wav,
            "jpg" | "jpeg" | "jpe" => Container::Jpeg,
            "png" => Container::Png,
            "webp" => Container::Webp,
            "avif" => Container::Avif,
            _ => return None,
        })
    }

    /// The format a file name's extension names: one known by name, or `Other` carrying
    /// the extension itself. `None` when the name has no extension.
    pub fn from_name(name: &str) -> Option<Container> {
        let (_, ext) = name.rsplit_once('.')?;
        if ext.is_empty() || ext.len() > 8 || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        Some(
            Container::from_extension(ext)
                .unwrap_or_else(|| Container::Other(ext.to_ascii_lowercase())),
        )
    }

    pub fn from_mime(mime: &str) -> Option<Container> {
        let essence = mime_essence(mime);
        Some(match essence.as_str() {
            "video/mp4" | "video/x-m4v" | "application/mp4" => Container::Mp4,
            "video/webm" => Container::Webm,
            "video/x-matroska" => Container::Mkv,
            "video/quicktime" => Container::Mov,
            "video/mp2t" => Container::Ts,
            "video/x-flv" => Container::Flv,
            "video/x-msvideo" => Container::Avi,
            "image/gif" => Container::Gif,
            "audio/mpeg" | "audio/mp3" | "audio/mpeg3" => Container::Mp3,
            "audio/mp4" | "audio/x-m4a" | "audio/aac" | "audio/aacp" => Container::M4a,
            "audio/ogg" | "audio/vorbis" | "application/ogg" => Container::Ogg,
            "audio/opus" => Container::Opus,
            "audio/flac" | "audio/x-flac" => Container::Flac,
            "audio/wav" | "audio/x-wav" | "audio/wave" | "audio/vnd.wave" => Container::Wav,
            "image/jpeg" | "image/pjpeg" => Container::Jpeg,
            "image/png" | "image/apng" => Container::Png,
            "image/webp" => Container::Webp,
            "image/avif" => Container::Avif,
            _ => return None,
        })
    }

    /// The media type the format is served as.
    pub fn mime(&self) -> &str {
        match self {
            Container::Mp4 => "video/mp4",
            Container::Webm => "video/webm",
            Container::Mkv => "video/x-matroska",
            Container::Mov => "video/quicktime",
            Container::Ts => "video/mp2t",
            Container::Flv => "video/x-flv",
            Container::Avi => "video/x-msvideo",
            Container::Gif => "image/gif",
            Container::Mp3 => "audio/mpeg",
            Container::M4a => "audio/mp4",
            Container::Ogg => "audio/ogg",
            Container::Opus => "audio/opus",
            Container::Flac => "audio/flac",
            Container::Wav => "audio/wav",
            Container::Jpeg => "image/jpeg",
            Container::Png => "image/png",
            Container::Webp => "image/webp",
            Container::Avif => "image/avif",
            Container::Other(ext) => match ext.to_ascii_lowercase().as_str() {
                "pdf" => "application/pdf",
                "zip" => "application/zip",
                "7z" => "application/x-7z-compressed",
                "rar" => "application/vnd.rar",
                "gz" | "tgz" => "application/gzip",
                "tar" => "application/x-tar",
                "txt" | "log" | "md" => "text/plain; charset=utf-8",
                "json" => "application/json; charset=utf-8",
                "csv" => "text/csv; charset=utf-8",
                "bmp" => "image/bmp",
                "tif" | "tiff" => "image/tiff",
                "heic" => "image/heic",
                "heif" => "image/heif",
                "jxl" => "image/jxl",
                "svg" => "image/svg+xml",
                "aiff" | "aif" => "audio/aiff",
                "wma" => "audio/x-ms-wma",
                "mka" => "audio/x-matroska",
                "ogv" => "video/ogg",
                "3gp" => "video/3gpp",
                "wmv" => "video/x-ms-wmv",
                "mpg" | "mpeg" => "video/mpeg",
                "doc" => "application/msword",
                "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
                "xls" => "application/vnd.ms-excel",
                "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                "ppt" => "application/vnd.ms-powerpoint",
                "pptx" => {
                    "application/vnd.openxmlformats-officedocument.presentationml.presentation"
                }
                "epub" => "application/epub+zip",
                _ => "application/octet-stream",
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoCodec {
    H264,
    H265,
    Vp8,
    Vp9,
    Av1,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioCodec {
    Aac,
    Opus,
    Vorbis,
    Mp3,
    Flac,
    Other(String),
}

/// How the picture's fields are laid out in time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldOrder {
    /// The stream does not say.
    #[default]
    Unknown,
    Progressive,
    /// Interlaced, the top field first.
    TopFirst,
    /// Interlaced, the bottom field first.
    BottomFirst,
}

impl FieldOrder {
    pub fn is_interlaced(self) -> bool {
        matches!(self, FieldOrder::TopFirst | FieldOrder::BottomFirst)
    }
}

/// A high dynamic range signal, by the transfer it is coded with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "format")]
pub enum HdrFormat {
    /// SMPTE ST 2084 perceptual quantizer: HDR10 and HDR10+.
    Pq,
    /// ARIB STD-B67 hybrid log-gamma.
    Hlg,
    /// Dolby Vision, by profile. Profiles 8 and 7 carry a PQ or HLG base layer any
    /// player renders. Profile 5 carries nothing else and needs its own rendering.
    DolbyVision { profile: u8 },
}

/// How a 360° picture is laid out in the frame.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "layout")]
pub enum Projection {
    Equirectangular,
    /// The six faces of a cube in a 3×2 grid, `padding` pixels between them.
    Cubemap {
        padding: u32,
    },
    /// YouTube's equi-angular cubemap.
    EquiAngularCubemap,
    /// A crop of an equirectangular picture: the frame covers the sphere from `left` to
    /// `right` and `top` to `bottom`, each a fraction of the full width or height.
    EquirectangularTile {
        left: f32,
        top: f32,
        right: f32,
        bottom: f32,
    },
}

/// How a stereoscopic picture packs its two eyes into one frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StereoLayout {
    SideBySide,
    TopBottom,
}

/// The colour signalling of a video stream, as the container or codec declares it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ColorInfo {
    pub primaries: Option<String>,
    pub transfer: Option<String>,
    pub matrix: Option<String>,
    pub range: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoTrack {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: Option<f64>,
    pub bitrate: Option<u64>,
    /// The stream's index in the file, for ffmpeg's `-map 0:N`.
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub pix_fmt: Option<String>,
    #[serde(default)]
    pub color: ColorInfo,
    /// The high dynamic range format the picture is coded in, when it is not SDR.
    #[serde(default)]
    pub hdr: Option<HdrFormat>,
    #[serde(default)]
    pub field_order: FieldOrder,
    /// The pixel aspect ratio as `(num, den)` when pixels are not square.
    #[serde(default)]
    pub sample_aspect: Option<(u32, u32)>,
    /// Whether the frames arrive at varying intervals.
    #[serde(default)]
    pub vfr: bool,
    /// Whether the pixel format carries transparency.
    #[serde(default)]
    pub alpha: bool,
    /// How a 360° picture is laid out, when it is one.
    #[serde(default)]
    pub projection: Option<Projection>,
    /// How two eyes share the frame, when they do.
    #[serde(default)]
    pub stereo: Option<StereoLayout>,
    /// The initial view of a 360° picture in degrees, as the file names it.
    #[serde(default)]
    pub view: Option<(f32, f32, f32)>,
}

impl VideoTrack {
    /// The size the picture shows at: the coded size with the pixel aspect ratio
    /// applied, so anamorphic sources come out with square pixels.
    pub fn display_size(&self) -> (u32, u32) {
        match self.sample_aspect {
            Some((num, den)) if num > 0 && den > 0 && num != den => {
                let width = (self.width as f64 * num as f64 / den as f64).round() as u32;
                (width.max(2), self.height.max(2))
            }
            _ => (self.width, self.height),
        }
    }

    /// Whether the picture needs work before any destination shows it as intended:
    /// tone-mapping, deinterlacing, or a flat view of a sphere.
    pub fn needs_processing(&self) -> bool {
        self.hdr.is_some() || self.field_order.is_interlaced() || self.projection.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioTrack {
    pub codec: AudioCodec,
    pub channels: u16,
    pub sample_rate: u32,
    pub bitrate: Option<u64>,
    /// The stream's index in the file, for ffmpeg's `-map 0:N`.
    #[serde(default)]
    pub index: usize,
    #[serde(default)]
    pub language: Option<String>,
}

/// A picture attached to the file: cover art, a poster frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttachedPicture {
    pub index: usize,
    pub width: u32,
    pub height: u32,
}

/// A subtitle stream inside the file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmbeddedSubtitle {
    pub index: usize,
    pub codec: String,
    #[serde(default)]
    pub language: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// Pictures rather than text: DVD, Blu-ray and DVB subtitles.
    pub bitmap: bool,
    /// Whether the stream is marked as the one to show by default.
    #[serde(default)]
    pub default: bool,
    /// Whether the stream is marked forced: shown even with subtitles off.
    #[serde(default)]
    pub forced: bool,
}

/// What a file holds, as ffprobe reads it. A still image is reported with its picture in
/// `video` (its width and height, no frame rate) and `kind` set to `Image`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaInfo {
    pub container: Container,
    /// What the probe found the file to be: a moving picture, sound alone, or a still.
    #[serde(default)]
    pub kind: MediaKind,
    pub duration: Option<Duration>,
    pub video: Option<VideoTrack>,
    pub audio: Option<AudioTrack>,
    /// Cover art or a poster frame carried beside the streams.
    #[serde(default)]
    pub cover: Option<AttachedPicture>,
    /// The subtitle streams inside the file, in order.
    #[serde(default)]
    pub subtitles: Vec<EmbeddedSubtitle>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalFile {
    pub path: PathBuf,
    pub size: u64,
    pub info: Option<MediaInfo>,
}

impl LocalFile {
    pub async fn from_path(path: PathBuf) -> std::io::Result<LocalFile> {
        let size = tokio::fs::metadata(&path).await?.len();
        Ok(LocalFile {
            path,
            size,
            info: None,
        })
    }
}

/// Builds a file name stem safe for any file system and for upload APIs: an ASCII slug of
/// the title, at most 64 characters, or `fallback` when the title yields nothing.
pub fn safe_stem(title: Option<&str>, fallback: &str) -> String {
    let mut stem: String = slug::slugify(title.unwrap_or_default())
        .chars()
        .take(64)
        .collect();
    while stem.ends_with('-') {
        stem.pop();
    }
    if stem.is_empty() {
        fallback.to_string()
    } else {
        stem
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ColorInfo, Container, FieldOrder, HdrFormat, MediaKind, Projection, VideoCodec, VideoTrack,
        safe_stem,
    };

    #[test]
    fn formats_know_their_kind() {
        assert_eq!(Container::from_extension("MP3"), Some(Container::Mp3));
        assert_eq!(Container::Mp3.kind(), MediaKind::Audio);
        assert_eq!(Container::from_extension("jpeg"), Some(Container::Jpeg));
        assert_eq!(Container::Jpeg.kind(), MediaKind::Image);
        assert_eq!(Container::Gif.kind(), MediaKind::Video);
        assert_eq!(Container::Other("wma".into()).kind(), MediaKind::Audio);
        assert_eq!(Container::Other("heic".into()).kind(), MediaKind::Image);
        assert_eq!(Container::Other("wmv".into()).kind(), MediaKind::Video);
        assert_eq!(Container::Other("pdf".into()).kind(), MediaKind::File);
        assert_eq!(
            Container::from_name("a.b.PDF"),
            Some(Container::Other("pdf".into()))
        );
        assert_eq!(Container::from_name("noext"), None);
        assert_eq!(Container::from_name("odd.ext with space"), None);
        assert_eq!(
            Container::from_mime("audio/mpeg; charset=x"),
            Some(Container::Mp3)
        );
        assert_eq!(MediaKind::from_mime("image/x-unknown"), MediaKind::Image);
        assert_eq!(MediaKind::from_mime("application/pdf"), MediaKind::File);
        assert_eq!(MediaKind::from_extension("png"), MediaKind::Image);
        assert_eq!(Container::Other("pdf".into()).mime(), "application/pdf");
        assert_eq!("image".parse::<MediaKind>(), Ok(MediaKind::Image));
    }

    #[test]
    fn anamorphic_pictures_show_at_their_display_size() {
        let mut track = VideoTrack {
            codec: VideoCodec::H264,
            width: 720,
            height: 576,
            fps: Some(25.0),
            bitrate: None,
            index: 0,
            pix_fmt: None,
            color: ColorInfo::default(),
            hdr: None,
            field_order: FieldOrder::Unknown,
            sample_aspect: Some((64, 45)),
            vfr: false,
            alpha: false,
            projection: None,
            stereo: None,
            view: None,
        };
        assert_eq!(track.display_size(), (1024, 576));
        track.sample_aspect = Some((1, 1));
        assert_eq!(track.display_size(), (720, 576));
        assert!(!track.needs_processing());
        track.field_order = FieldOrder::TopFirst;
        assert!(track.needs_processing());
        track.field_order = FieldOrder::Progressive;
        track.hdr = Some(HdrFormat::Pq);
        assert!(track.needs_processing());
        track.hdr = None;
        track.projection = Some(Projection::Equirectangular);
        assert!(track.needs_processing());
        let json = serde_json::to_value(&track).unwrap();
        assert_eq!(json["projection"]["layout"], "equirectangular");
        assert_eq!(json["field_order"], "progressive");
    }

    #[test]
    fn slugs_titles() {
        assert_eq!(
            safe_stem(Some("Hello, World! Ünïcode 2024"), "video"),
            "hello-world-unicode-2024"
        );
    }

    #[test]
    fn falls_back_when_nothing_survives() {
        assert_eq!(safe_stem(Some("!!! ???"), "video"), "video");
        assert_eq!(safe_stem(None, "video"), "video");
    }

    #[test]
    fn caps_length_without_trailing_dash() {
        let long = "word ".repeat(40);
        let stem = safe_stem(Some(&long), "video");
        assert!(stem.len() <= 64);
        assert!(!stem.ends_with('-'));
        assert!(stem.starts_with("word-word"));
    }
}
