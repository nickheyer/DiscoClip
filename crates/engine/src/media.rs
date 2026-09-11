use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

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
    Other(String),
}

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
            Container::Other(name) => name.as_str(),
        }
    }

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
            _ => return None,
        })
    }

    pub fn from_mime(mime: &str) -> Option<Container> {
        let essence = mime
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        Some(match essence.as_str() {
            "video/mp4" | "video/x-m4v" | "application/mp4" => Container::Mp4,
            "video/webm" => Container::Webm,
            "video/x-matroska" => Container::Mkv,
            "video/quicktime" => Container::Mov,
            "video/mp2t" => Container::Ts,
            "video/x-flv" => Container::Flv,
            "video/x-msvideo" => Container::Avi,
            "image/gif" => Container::Gif,
            _ => return None,
        })
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaInfo {
    pub container: Container,
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
    use super::safe_stem;

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
