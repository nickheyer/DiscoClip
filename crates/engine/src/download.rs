//! Fetching the chosen variant into the job directory, by delivery kind.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use url::Url;

use crate::event::{Progress, ProgressSender};
use crate::ffmpeg::Ffmpeg;
use crate::http::{Http, HttpError};
use crate::media::{AudioCodec, Container, LocalFile};
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

/// Streams media files over HTTP into the job directory: one file, or a video-only file
/// and its separate audio, muxed together with the embedded ffmpeg.
pub struct HttpDownloader {
    http: Http,
    ffmpeg: Ffmpeg,
}

/// Bytes fetched so far across the files of one download, for the progress shown.
struct Tally<'a> {
    progress: &'a ProgressSender,
    done: u64,
    total: Option<u64>,
}

impl Tally<'_> {
    fn advance(&mut self, bytes: u64) {
        self.done += bytes;
        self.progress.send_replace(Progress {
            done: self.done,
            total: self.total,
        });
    }
}

impl HttpDownloader {
    pub fn new(http: Http, ffmpeg: Ffmpeg) -> Self {
        Self { http, ffmpeg }
    }

    /// Streams `url` into `path`, failing once it is longer than `max_bytes`; the bytes
    /// written and the content type served.
    async fn fetch(
        &self,
        url: &Url,
        headers: &[(String, String)],
        platform: &str,
        path: &Path,
        max_bytes: u64,
        tally: &mut Tally<'_>,
    ) -> Result<(u64, Option<String>), DownloadError> {
        let response = self
            .http
            .get(url.clone())
            .platform(platform)
            .headers(headers)
            .media()
            .send()
            .await?;
        let status = response.status;
        if !status.is_success() {
            return Err(DownloadError::Status {
                status: status.as_u16(),
                url: url.to_string(),
            });
        }
        if let Some(size) = response.content_length()
            && size > max_bytes
        {
            return Err(DownloadError::TooLarge {
                size,
                limit: max_bytes,
            });
        }
        if tally.total.is_none() {
            tally.total = response.content_length().map(|size| tally.done + size);
        }
        let content_type = response.content_type().map(str::to_owned);
        let mut file = tokio::fs::File::create(path).await?;
        let mut stream = response.into_stream();
        let mut done = 0u64;
        tally.advance(0);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            done += chunk.len() as u64;
            if done > max_bytes {
                drop(file);
                let _ = tokio::fs::remove_file(path).await;
                return Err(DownloadError::TooLarge {
                    size: done,
                    limit: max_bytes,
                });
            }
            file.write_all(&chunk).await?;
            tally.advance(chunk.len() as u64);
        }
        file.flush().await?;
        drop(file);
        if done == 0 {
            let _ = tokio::fs::remove_file(path).await;
            return Err(DownloadError::Empty);
        }
        Ok((done, content_type))
    }
}

/// The extension a separate audio file gets, by its codec.
fn audio_extension(codec: Option<&AudioCodec>) -> &'static str {
    match codec {
        Some(AudioCodec::Aac) => "m4a",
        Some(AudioCodec::Opus) | Some(AudioCodec::Vorbis) => "webm",
        Some(AudioCodec::Mp3) => "mp3",
        Some(AudioCodec::Other(_)) | None => "bin",
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
        tokio::fs::create_dir_all(dest_dir).await?;
        let mut tally = Tally {
            progress: &progress,
            done: 0,
            total: variant.size,
        };
        let Some(audio_url) = &variant.audio_url else {
            let part = dest_dir.join("source.part");
            let (size, content_type) = self
                .fetch(
                    &variant.url,
                    &variant.headers,
                    &context.platform,
                    &part,
                    context.max_bytes,
                    &mut tally,
                )
                .await?;
            let ext = extension_for(variant, content_type.as_deref());
            let path = dest_dir.join(format!("source.{ext}"));
            tokio::fs::rename(&part, &path).await?;
            return Ok(Downloaded::file(LocalFile {
                path,
                size,
                info: None,
            }));
        };
        let video_path = dest_dir.join(format!(
            "video.{}",
            variant
                .container
                .as_ref()
                .map(|c| c.extension())
                .unwrap_or("bin")
        ));
        let (written, _) = self
            .fetch(
                &variant.url,
                &variant.headers,
                &context.platform,
                &video_path,
                context.max_bytes,
                &mut tally,
            )
            .await?;
        let audio_path =
            dest_dir.join(format!("audio.{}", audio_extension(variant.audio.as_ref())));
        let remaining = context.max_bytes.saturating_sub(written).max(1);
        self.fetch(
            audio_url,
            &variant.headers,
            &context.platform,
            &audio_path,
            remaining,
            &mut tally,
        )
        .await?;
        let dest = dest_dir.join("source.mkv");
        let file = segments::mux(&self.ffmpeg, &video_path, Some(&audio_path), &dest).await?;
        let _ = tokio::fs::remove_file(&video_path).await;
        let _ = tokio::fs::remove_file(&audio_path).await;
        if file.size > context.max_bytes {
            return Err(DownloadError::TooLarge {
                size: file.size,
                limit: context.max_bytes,
            });
        }
        Ok(Downloaded::file(file))
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;

    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };
    use crate::media::VideoCodec;

    fn serve(url: &str, content_type: &str, bytes: &[u8]) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: "GET".into(),
                url: url.into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status: 200,
                url: url.into(),
                headers: vec![
                    ("content-type".into(), content_type.into()),
                    ("content-length".into(), bytes.len().to_string()),
                ],
                body: RecordedBody::from_bytes(bytes),
                truncated: false,
            },
        }
    }

    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[tokio::test]
    async fn a_video_only_file_is_muxed_with_its_separate_audio() {
        let dir = std::env::temp_dir().join(format!("discoclip-mux-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let video_src = dir.join("in-video.mp4");
        let audio_src = dir.join("in-audio.m4a");
        ffmpeg
            .run(
                args(&[
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "testsrc=size=64x64:rate=10:duration=1",
                    "-c:v",
                    "mpeg4",
                    "-an",
                ])
                .into_iter()
                .chain([video_src.as_os_str().to_owned()]),
                |_| {},
            )
            .await
            .unwrap();
        ffmpeg
            .run(
                args(&[
                    "-loglevel",
                    "error",
                    "-f",
                    "lavfi",
                    "-i",
                    "sine=frequency=440:duration=1",
                    "-c:a",
                    "aac",
                ])
                .into_iter()
                .chain([audio_src.as_os_str().to_owned()]),
                |_| {},
            )
            .await
            .unwrap();
        let video_bytes = tokio::fs::read(&video_src).await.unwrap();
        let audio_bytes = tokio::fs::read(&audio_src).await.unwrap();

        let mut fixture = Fixture::new("mux", None);
        fixture
            .exchanges
            .push(serve("https://cdn.test/video", "video/mp4", &video_bytes));
        fixture
            .exchanges
            .push(serve("https://cdn.test/audio", "audio/mp4", &audio_bytes));
        let downloader = HttpDownloader::new(Http::replay(fixture), ffmpeg.clone());
        let mut variant = Variant::file(Url::parse("https://cdn.test/video").unwrap());
        variant.audio_url = Some(Url::parse("https://cdn.test/audio").unwrap());
        variant.container = Some(Container::Mp4);
        variant.video = Some(VideoCodec::Other("mpeg4".into()));
        variant.audio = Some(AudioCodec::Aac);
        variant.video_only = true;
        variant.size = Some((video_bytes.len() + audio_bytes.len()) as u64);
        let (progress, watched) = tokio::sync::watch::channel(Progress::default());
        let job_dir = dir.join("job");
        let downloaded = downloader
            .download(
                &variant,
                &job_dir,
                &DownloadContext::new(50_000_000),
                progress,
            )
            .await
            .unwrap();
        assert!(downloaded.file.path.ends_with("source.mkv"));
        assert!(downloaded.file.size > 0);
        let info = ffmpeg.probe(&downloaded.file.path).await.unwrap();
        assert!(info.video.is_some(), "{info:?}");
        assert!(info.audio.is_some(), "{info:?}");
        assert!(!job_dir.join("video.mp4").exists());
        assert!(!job_dir.join("audio.m4a").exists());
        let last = *watched.borrow();
        assert_eq!(last.done, (video_bytes.len() + audio_bytes.len()) as u64);
        assert_eq!(last.total, Some(last.done));

        // A lone file keeps its extension from the content type.
        let mut fixture = Fixture::new("single", None);
        fixture
            .exchanges
            .push(serve("https://cdn.test/only", "video/mp4", &video_bytes));
        let downloader = HttpDownloader::new(Http::replay(fixture), ffmpeg.clone());
        let variant = Variant::file(Url::parse("https://cdn.test/only").unwrap());
        let (progress, _) = tokio::sync::watch::channel(Progress::default());
        let downloaded = downloader
            .download(
                &variant,
                &dir.join("single"),
                &DownloadContext::new(50_000_000),
                progress,
            )
            .await
            .unwrap();
        assert!(downloaded.file.path.ends_with("source.mp4"));
        assert_eq!(downloaded.file.size, video_bytes.len() as u64);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
