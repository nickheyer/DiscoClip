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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VideoTrack {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub fps: Option<f64>,
    pub bitrate: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioTrack {
    pub codec: AudioCodec,
    pub channels: u16,
    pub sample_rate: u32,
    pub bitrate: Option<u64>,
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
    use super::{Container, MediaKind, safe_stem};

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
