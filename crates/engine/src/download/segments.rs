//! Shared pieces for segment based downloads: a byte budget every file of one download
//! draws on, a sink that writes within it, ranged fetches, and the local mux step that
//! turns the fetched streams, each in one or more parts, into one Matroska file.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use url::Url;

use super::{DownloadError, mp4, mpegts};
use crate::event::{Progress, ProgressSender};
use crate::ffmpeg::Ffmpeg;
use crate::http::Http;
use crate::media::LocalFile;

/// The bytes one download may put on disk, drawn on by every file it writes.
pub struct Budget {
    limit: u64,
    used: AtomicU64,
}

impl Budget {
    pub fn new(limit: u64) -> Self {
        Self {
            limit,
            used: AtomicU64::new(0),
        }
    }

    /// Spends `bytes`, failing once the download would exceed its limit.
    pub fn take(&self, bytes: u64) -> Result<(), DownloadError> {
        let used = self.used.fetch_add(bytes, Ordering::Relaxed) + bytes;
        if used > self.limit {
            return Err(DownloadError::TooLarge {
                size: used,
                limit: self.limit,
            });
        }
        Ok(())
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }
}

pub struct Sink<'a> {
    file: tokio::fs::File,
    written: u64,
    budget: &'a Budget,
}

impl<'a> Sink<'a> {
    pub async fn create(path: &Path, budget: &'a Budget) -> Result<Self, DownloadError> {
        Ok(Self {
            file: tokio::fs::File::create(path).await?,
            written: 0,
            budget,
        })
    }

    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), DownloadError> {
        use tokio::io::AsyncWriteExt;
        self.budget.take(bytes.len() as u64)?;
        self.written += bytes.len() as u64;
        self.file.write_all(bytes).await?;
        Ok(())
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    pub async fn finish(mut self) -> Result<u64, DownloadError> {
        use tokio::io::AsyncWriteExt;
        self.file.flush().await?;
        Ok(self.written)
    }
}

/// GETs `url`, or the byte range `range` of it, as one buffer.
pub async fn fetch_bytes(
    http: &Http,
    url: &Url,
    platform: &str,
    headers: &[(String, String)],
    range: Option<(u64, u64)>,
) -> Result<Bytes, DownloadError> {
    let mut request = http
        .get(url.clone())
        .platform(platform)
        .headers(headers)
        .media();
    if let Some((start, end)) = range {
        request = request.header("range", &format!("bytes={start}-{end}"));
    }
    let response = request.send().await?;
    let status = response.status;
    if !status.is_success() {
        return Err(DownloadError::Status {
            status: status.as_u16(),
            url: url.to_string(),
        });
    }
    Ok(response.bytes(usize::MAX).await?)
}

/// GETs a manifest or playlist as text, failing when longer than `limit`.
pub async fn fetch_text(
    http: &Http,
    url: &Url,
    platform: &str,
    headers: &[(String, String)],
    limit: usize,
) -> Result<(Url, String), DownloadError> {
    let response = http
        .get(url.clone())
        .platform(platform)
        .headers(headers)
        .send()
        .await?;
    let status = response.status;
    if !status.is_success() {
        return Err(DownloadError::Status {
            status: status.as_u16(),
            url: url.to_string(),
        });
    }
    let final_url = response.url.clone();
    let bytes = response.bytes(limit).await.map_err(|e| match e {
        crate::http::HttpError::BodyTooLarge { .. } => {
            DownloadError::Manifest(format!("manifest at {url} exceeds {limit} bytes"))
        }
        other => DownloadError::Http(other),
    })?;
    Ok((final_url, String::from_utf8_lossy(&bytes).into_owned()))
}

/// An initialization section fetched once and kept for every segment that names it.
pub struct InitSection {
    /// The bytes written ahead of the segments: cleared of protection for fMP4.
    pub bytes: Vec<u8>,
    /// What the section says about its tracks, for decrypting sample-encrypted segments.
    pub parsed: Option<mp4::Init>,
}

/// Which part of a track a segment belongs to: segments continue one another while they
/// share a run, the discontinuity sequence or period they fall in, and an initialization
/// section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartKey {
    pub run: u64,
    pub init: Option<String>,
}

/// When a segment plays: where it starts in the presentation, and the time its own
/// timestamps count from, both in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Timing {
    pub start: f64,
    pub origin: f64,
}

/// Where a track's fetched segments go.
#[async_trait]
pub trait Consumer: Send {
    async fn take(
        &mut self,
        key: &PartKey,
        timing: Timing,
        init: Option<&InitSection>,
        bytes: &[u8],
    ) -> Result<(), DownloadError>;
    /// Segments were missed: what follows does not continue what came before.
    fn gap(&mut self);
}

/// The extension a part gets, by what its first bytes are.
pub fn part_extension(bytes: &[u8]) -> &'static str {
    if mpegts::is_transport_stream(bytes) {
        "ts"
    } else if mp4::is_mp4(bytes) {
        "mp4"
    } else if bytes.starts_with(&[0x1a, 0x45, 0xdf, 0xa3]) {
        "webm"
    } else if bytes.starts_with(b"ID3") || (bytes.len() > 1 && bytes[0] == 0xff && bytes[1] & 0xf6 == 0xf0) {
        "aac"
    } else {
        "bin"
    }
}

/// A media track written as parts: one file per run of segments the stream says are
/// continuous and share an initialization section.
pub struct TrackWriter<'a> {
    dir: PathBuf,
    name: String,
    budget: &'a Budget,
    parts: Vec<PathBuf>,
    current: Option<(Sink<'a>, PartKey)>,
    broken: bool,
    /// The media time, in seconds, the first segment starts at.
    start_time: Option<f64>,
}

impl<'a> TrackWriter<'a> {
    pub fn new(dir: &Path, name: &str, budget: &'a Budget) -> Self {
        Self {
            dir: dir.to_path_buf(),
            name: name.to_string(),
            budget,
            parts: Vec::new(),
            current: None,
            broken: false,
            start_time: None,
        }
    }

    /// The parts written so far.
    pub fn parts(&self) -> &[PathBuf] {
        &self.parts
    }

    /// Closes the track: its parts in order, and the media time it starts at.
    pub async fn finish(mut self) -> Result<(Vec<PathBuf>, Option<f64>), DownloadError> {
        if let Some((sink, _)) = self.current.take() {
            sink.finish().await?;
        }
        Ok((self.parts, self.start_time))
    }
}

#[async_trait]
impl Consumer for TrackWriter<'_> {
    async fn take(
        &mut self,
        key: &PartKey,
        _timing: Timing,
        init: Option<&InitSection>,
        bytes: &[u8],
    ) -> Result<(), DownloadError> {
        let continues = !self.broken
            && self
                .current
                .as_ref()
                .is_some_and(|(_, current)| current == key);
        if !continues {
            if let Some((sink, _)) = self.current.take() {
                sink.finish().await?;
            }
            let extension = part_extension(init.map_or(bytes, |i| &i.bytes));
            let path = self
                .dir
                .join(format!("{}.{}.{extension}", self.name, self.parts.len()));
            let mut sink = Sink::create(&path, self.budget).await?;
            if let Some(init) = init {
                sink.write(&init.bytes).await?;
            }
            self.parts.push(path);
            self.current = Some((sink, key.clone()));
            self.broken = false;
        }
        if self.start_time.is_none() {
            self.start_time = if mpegts::is_transport_stream(bytes) {
                mpegts::start_time(bytes).map(|ticks| ticks as f64 / 90_000.0)
            } else {
                init.and_then(|i| i.parsed.as_ref())
                    .and_then(|parsed| mp4::start_time(bytes, parsed))
            };
        }
        let (sink, _) = self.current.as_mut().unwrap();
        sink.write(bytes).await?;
        Ok(())
    }

    fn gap(&mut self) {
        self.broken = true;
    }
}

/// Progress reported for the primary track: media seconds captured against the
/// presentation's length, or the live capture limit.
pub struct Meter<'a> {
    progress: &'a ProgressSender,
    done: f64,
    total: Option<f64>,
}

impl<'a> Meter<'a> {
    pub fn new(progress: &'a ProgressSender) -> Self {
        Self {
            progress,
            done: 0.0,
            total: None,
        }
    }

    pub fn set_total(&mut self, total: f64) {
        self.total = Some(total);
        self.send();
    }

    pub fn add(&mut self, seconds: f64) {
        self.done += seconds;
        self.send();
    }

    fn send(&self) {
        self.progress.send_replace(Progress {
            done: self.done.round() as u64,
            total: self.total.map(|t| t.round() as u64),
        });
    }
}

/// The ffmpeg input arguments for a stream in `parts`: the file itself when there is
/// one, else a concat list written beside it at `list`, which joins the parts with
/// their timestamps run on from one to the next.
async fn input_args(parts: &[PathBuf], list: &Path) -> Result<Vec<OsString>, DownloadError> {
    match parts {
        [] => Err(DownloadError::Empty),
        [only] => Ok(vec!["-i".into(), only.as_os_str().to_owned()]),
        many => {
            let mut text = String::from("ffconcat version 1.0\n");
            for part in many {
                let path = part.to_str().ok_or_else(|| {
                    DownloadError::Process(format!("{} is not valid UTF-8", part.display()))
                })?;
                text.push_str(&format!("file '{}'\n", path.replace('\'', "'\\''")));
            }
            tokio::fs::write(list, text).await?;
            Ok(vec![
                "-f".into(),
                "concat".into(),
                "-safe".into(),
                "0".into(),
                "-i".into(),
                list.as_os_str().to_owned(),
            ])
        }
    }
}

/// Remuxes locally downloaded streams into `dest` without touching the network: the
/// video's parts joined, and the audio's when there is a separate audio stream.
pub async fn mux_parts(
    ffmpeg: &Ffmpeg,
    video: &[PathBuf],
    audio: Option<&[PathBuf]>,
    dest: &Path,
) -> Result<LocalFile, DownloadError> {
    let dir = dest.parent().unwrap_or(Path::new("."));
    let mut args: Vec<OsString> = vec!["-loglevel".into(), "warning".into()];
    args.extend(input_args(video, &dir.join("video.parts.txt")).await?);
    match audio {
        Some(audio) => {
            args.extend(input_args(audio, &dir.join("audio.parts.txt")).await?);
            args.extend([
                "-map".into(),
                "0:v:0".into(),
                "-map".into(),
                "1:a:0".into(),
            ]);
        }
        None => {
            args.extend([
                "-map".into(),
                "0:v:0?".into(),
                "-map".into(),
                "0:a:0?".into(),
            ]);
        }
    }
    args.extend([
        "-c".into(),
        "copy".into(),
        "-sn".into(),
        "-dn".into(),
        "-f".into(),
        "matroska".into(),
        dest.as_os_str().to_owned(),
    ]);
    ffmpeg
        .run(args, |_| {})
        .await
        .map_err(|e| DownloadError::Process(e.to_string()))?;
    for list in ["video.parts.txt", "audio.parts.txt"] {
        let _ = tokio::fs::remove_file(dir.join(list)).await;
    }
    let file = LocalFile::from_path(dest.to_path_buf()).await?;
    if file.size == 0 {
        return Err(DownloadError::Empty);
    }
    Ok(file)
}

/// Remuxes one video file and, when given, one audio file into `dest`.
pub async fn mux(
    ffmpeg: &Ffmpeg,
    video: &Path,
    audio: Option<&Path>,
    dest: &Path,
) -> Result<LocalFile, DownloadError> {
    let video = [video.to_path_buf()];
    let audio = audio.map(|a| vec![a.to_path_buf()]);
    mux_parts(ffmpeg, &video, audio.as_deref(), dest).await
}
