//! Fetching the chosen variant into the job directory, by delivery kind.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::fs::File;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use url::Url;

use crate::config::DownloadConfig;
use crate::event::{Progress, ProgressSender};
use crate::ffmpeg::Ffmpeg;
use crate::http::{Http, HttpError, Response, RetryPolicy, StatusCode};
use crate::media::{AudioCodec, Container, LocalFile};
use crate::resolve::{Cipher, ClipRange, SubtitleFormat, SubtitleTrack, Variant, VariantKind};

pub mod dash;
pub mod hls;
pub mod ism;
pub mod mp4;
pub mod mpegts;
pub mod segments;
pub mod stream;
pub mod subtitles;
pub mod whep;

/// Which subtitles the job wants beside the media: the tracks the resolver listed, after
/// the job's language preference, and that preference for renditions a downloader finds
/// on its own.
#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleChoice {
    pub tracks: Vec<SubtitleTrack>,
    pub language: Option<String>,
}

/// The bounds a job puts on its download.
#[derive(Debug, Clone, PartialEq)]
pub struct DownloadContext {
    /// Bytes the source may occupy on disk.
    pub max_bytes: u64,
    /// The tallest picture worth fetching. Manifests offering several pick by this.
    pub max_height: u32,
    /// How long a live stream is captured before it is cut and treated as a recording.
    pub max_live: Duration,
    /// The portion wanted. Downloaders that can seek fetch only it.
    pub clip: Option<ClipRange>,
    /// Whose cookies and proxy the requests use.
    pub platform: String,
    /// How many connections fetch one file, how much each asks for, and how often a
    /// broken transfer is picked up again.
    pub download: DownloadConfig,
    /// The subtitles wanted beside the media. `None` when the job skips them. A
    /// downloader fetches the tracks it knows how to follow with the media, such as HLS
    /// renditions, and reports them. The pipeline fetches the rest.
    pub subtitles: Option<SubtitleChoice>,
}

impl DownloadContext {
    pub fn new(max_bytes: u64) -> Self {
        Self {
            max_bytes,
            max_height: crate::config::Limits::default().max_height,
            max_live: Duration::from_secs(3 * 60 * 60),
            clip: None,
            platform: crate::http::WEB_PLATFORM.to_string(),
            download: DownloadConfig::default(),
            subtitles: None,
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
    /// Where it was fetched from.
    #[serde(default)]
    pub url: Option<Url>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Downloaded {
    pub file: LocalFile,
    pub subtitles: Vec<LocalSubtitle>,
    /// What happened along the way that the job's log should say: a live capture cut
    /// at its limit, segments skipped, a stream joined from parts.
    pub notes: Vec<String>,
}

impl Downloaded {
    pub fn file(file: LocalFile) -> Self {
        Self {
            file,
            subtitles: Vec::new(),
            notes: Vec::new(),
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
    /// A server answered a ranged request with bytes that do not fit the file being put
    /// together.
    #[error("{0}")]
    Range(String),
    /// A media segment could not be read or decrypted.
    #[error("{0}")]
    Segment(String),
    /// A transfer broke once more than it could be picked up again.
    #[error("download of {url} broke again after {resumes} resume(s): {last}")]
    Interrupted {
        url: String,
        resumes: u32,
        last: String,
    },
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
    let name = percent_encoding::percent_decode_str(variant.url.path())
        .decode_utf8_lossy()
        .into_owned();
    let from_path = Path::new(&name)
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(Container::from_name);
    match from_path {
        Some(container) => container.extension().to_string(),
        None => "bin".to_string(),
    }
}

/// Fetches media files over HTTP into the job directory: one file, or a video-only file
/// and its separate audio, muxed together with the embedded ffmpeg. A host that serves
/// byte ranges is asked for the file in chunks over concurrent connections. A
/// transfer that breaks is picked up from the byte it stopped at. A file that changes
/// while it is fetched is fetched again from its first byte.
pub struct HttpDownloader {
    http: Http,
    ffmpeg: Ffmpeg,
}

/// Bytes fetched so far across the files of one download, for the progress shown. Shared
/// by the connections fetching one file at the same time.
struct Tally<'a> {
    progress: &'a ProgressSender,
    done: AtomicU64,
    /// Zero until a length is known.
    total: AtomicU64,
}

impl<'a> Tally<'a> {
    fn new(progress: &'a ProgressSender, total: Option<u64>) -> Self {
        let tally = Self {
            progress,
            done: AtomicU64::new(0),
            total: AtomicU64::new(total.unwrap_or(0)),
        };
        tally.send();
        tally
    }

    fn send(&self) {
        let total = self.total.load(Ordering::Relaxed);
        self.progress.send_replace(Progress {
            done: self.done.load(Ordering::Relaxed),
            total: (total > 0).then_some(total),
        });
    }

    fn advance(&self, bytes: u64) {
        self.done.fetch_add(bytes, Ordering::Relaxed);
        self.send();
    }

    fn retreat(&self, bytes: u64) {
        self.done.fetch_sub(bytes, Ordering::Relaxed);
        self.send();
    }

    /// Takes `size` as the length of the file being fetched when no total is known yet.
    /// whether it was taken.
    fn learn(&self, size: u64) -> bool {
        let total = self.done.load(Ordering::Relaxed) + size;
        let learned = self
            .total
            .compare_exchange(0, total, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok();
        if learned {
            self.send();
        }
        learned
    }

    fn forget(&self) {
        self.total.store(0, Ordering::Relaxed);
        self.send();
    }
}

/// A stream cipher run over a file's bytes in the order they arrive.
enum Decryptor {
    Aes128Ctr(ctr::Ctr128BE<aes::Aes128>),
}

impl Decryptor {
    /// The cipher positioned at byte `offset` of the file, so a chunk fetched on its own
    /// decrypts as it would in sequence.
    fn at(cipher: &Cipher, offset: u64) -> Result<Self, DownloadError> {
        use ctr::cipher::{KeyIvInit, StreamCipherSeek};
        match cipher {
            Cipher::Aes128Ctr { key, nonce } => {
                let mut iv = [0u8; 16];
                iv[..8].copy_from_slice(nonce);
                let mut ctr = ctr::Ctr128BE::<aes::Aes128>::new(key.into(), &iv.into());
                ctr.try_seek(offset).map_err(|e| {
                    DownloadError::Process(format!("cannot decrypt from byte {offset}: {e}"))
                })?;
                Ok(Decryptor::Aes128Ctr(ctr))
            }
        }
    }

    fn apply(&mut self, data: &mut [u8]) {
        use ctr::cipher::StreamCipher;
        match self {
            Decryptor::Aes128Ctr(ctr) => ctr.apply_keystream(data),
        }
    }
}

/// What a server said to a ranged request.
enum Answer {
    /// 206: the bytes from `start` to `end`, of `total` when the server knows it.
    Partial {
        start: u64,
        end: u64,
        total: Option<u64>,
    },
    /// 200: the whole file from its first byte, whatever range was asked for.
    Whole,
    /// 416: the file has no byte where the range began.
    Unsatisfiable,
}

fn classify(response: &Response, url: &Url) -> Result<Answer, DownloadError> {
    match response.status {
        StatusCode::PARTIAL_CONTENT => {
            let value = response.header("content-range").ok_or_else(|| {
                DownloadError::Range(format!("{url} answered 206 without a Content-Range"))
            })?;
            let (start, end, total) = parse_content_range(value).ok_or_else(|| {
                DownloadError::Range(format!("{url} answered an unreadable Content-Range: {value}"))
            })?;
            Ok(Answer::Partial { start, end, total })
        }
        StatusCode::RANGE_NOT_SATISFIABLE => Ok(Answer::Unsatisfiable),
        status if status.is_success() => Ok(Answer::Whole),
        status => Err(DownloadError::Status {
            status: status.as_u16(),
            url: url.to_string(),
        }),
    }
}

/// `bytes 0-1023/4096` or `bytes 0-1023/*`: the range served and the file's length.
fn parse_content_range(value: &str) -> Option<(u64, u64, Option<u64>)> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let (range, total) = rest.split_once('/')?;
    let (start, end) = range.trim().split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end: u64 = end.trim().parse().ok()?;
    if end < start {
        return None;
    }
    let total = match total.trim() {
        "*" => None,
        total => Some(total.parse().ok()?),
    };
    if let Some(total) = total
        && end >= total
    {
        return None;
    }
    Some((start, end, total))
}

/// What `If-Range` carries so a file that changed between requests comes back whole
/// rather than as ranges of two different files: a strong ETag, else the modification
/// date.
fn validator_of(response: &Response) -> Option<String> {
    if let Some(etag) = response.header("etag")
        && !etag.trim_start().starts_with("W/")
    {
        return Some(etag.to_string());
    }
    response.header("last-modified").map(str::to_owned)
}

/// Why a transfer stopped at `at` bytes: the error that cut it, or a body that ended early.
fn short(broke: Option<HttpError>, at: u64, url: &Url) -> String {
    match broke {
        Some(error) => format!("{error}, after {at} bytes"),
        None => format!("{url} closed the body after {at} bytes"),
    }
}

/// Why one pass over a file stopped short of finishing it.
enum Hitch {
    /// A ranged request for a piece of the file was answered with the whole file: the
    /// file changed, or the host stopped serving ranges. The pass starts over.
    Whole,
    Failed(DownloadError),
}

impl From<DownloadError> for Hitch {
    fn from(error: DownloadError) -> Self {
        Hitch::Failed(error)
    }
}

impl From<HttpError> for Hitch {
    fn from(error: HttpError) -> Self {
        Hitch::Failed(error.into())
    }
}

impl From<std::io::Error> for Hitch {
    fn from(error: std::io::Error) -> Self {
        Hitch::Failed(error.into())
    }
}

/// One file being fetched: what every request for it shares.
struct Transfer<'a> {
    http: &'a Http,
    url: &'a Url,
    headers: &'a [(String, String)],
    platform: &'a str,
    cipher: Option<&'a Cipher>,
    path: &'a Path,
    max_bytes: u64,
    config: &'a DownloadConfig,
    /// The pause before each resume.
    pauses: RetryPolicy,
    tally: &'a Tally<'a>,
    /// Bytes this file has added to the tally, taken back when it starts over.
    counted: AtomicU64,
    /// Whether the tally's total came from this file.
    sized: AtomicBool,
    /// Resumes used so far, across every connection fetching the file.
    resumes: AtomicU32,
}

impl Transfer<'_> {
    /// Fetches the whole file, starting over when a pass finds the file changed under
    /// it. The bytes written and the content type served.
    async fn fetch(&self) -> Result<(u64, Option<String>), DownloadError> {
        loop {
            match self.pass().await {
                Ok(done) => return Ok(done),
                Err(Hitch::Failed(error)) => return Err(error),
                Err(Hitch::Whole) => {
                    self.resume(format!(
                        "{} answered a ranged request with the whole file",
                        self.url
                    ))
                    .await?;
                }
            }
        }
    }

    fn count(&self, bytes: u64) {
        self.counted.fetch_add(bytes, Ordering::Relaxed);
        self.tally.advance(bytes);
    }

    fn check_size(&self, size: u64) -> Result<(), DownloadError> {
        if size > self.max_bytes {
            return Err(DownloadError::TooLarge {
                size,
                limit: self.max_bytes,
            });
        }
        Ok(())
    }

    fn learn(&self, size: u64) {
        if self.tally.learn(size) {
            self.sized.store(true, Ordering::Relaxed);
        }
    }

    /// Takes back what was counted for the file, before it is fetched from the start.
    fn start_over(&self) {
        let counted = self.counted.swap(0, Ordering::Relaxed);
        self.tally.retreat(counted);
        if self.sized.swap(false, Ordering::Relaxed) {
            self.tally.forget();
        }
    }

    /// Spends one resume, pausing before it as the HTTP retry policy would. Fails with
    /// `why` once they are used up.
    async fn resume(&self, why: String) -> Result<(), DownloadError> {
        let used = self.resumes.fetch_add(1, Ordering::Relaxed) + 1;
        if used > self.config.resume_attempts {
            return Err(DownloadError::Interrupted {
                url: self.url.to_string(),
                resumes: self.config.resume_attempts,
                last: why,
            });
        }
        tracing::debug!(url = %self.url, resume = used, "{why}; resuming");
        tokio::time::sleep(self.pauses.delay(used, None)).await;
        Ok(())
    }

    /// Asks for bytes `start..=end`, or from `start` to the end of the file.
    async fn request(
        &self,
        start: u64,
        end: Option<u64>,
        validator: Option<&str>,
    ) -> Result<Response, DownloadError> {
        let range = match end {
            Some(end) => format!("bytes={start}-{end}"),
            None => format!("bytes={start}-"),
        };
        let mut request = self
            .http
            .get(self.url.clone())
            .platform(self.platform)
            .headers(self.headers)
            .media()
            .header("range", &range);
        if let Some(validator) = validator {
            request = request.header("if-range", validator);
        }
        Ok(request.send().await?)
    }

    /// One pass over the file from its first byte: the first chunk tells whether the host
    /// serves ranges and how long the file is, and the rest follows in chunks over the
    /// configured connections, or as one stream when it does not.
    async fn pass(&self) -> Result<(u64, Option<String>), Hitch> {
        self.start_over();
        let first_end = self.config.chunk_bytes.max(1) - 1;
        let response = self.request(0, Some(first_end), None).await?;
        let content_type = response.content_type().map(str::to_owned);
        let validator = validator_of(&response);
        let validator = validator.as_deref();
        let mut file = File::create(self.path).await?;
        let size = match classify(&response, self.url)? {
            Answer::Partial {
                start: 0,
                end,
                total,
            } => {
                if let Some(total) = total {
                    self.check_size(total)?;
                    self.learn(total);
                }
                let (written, broke) = self
                    .write_body(response, &mut file, 0, Some(end + 1))
                    .await?;
                if written <= end {
                    self.resume(short(broke, written, self.url)).await?;
                    self.fetch_range(written, end, validator).await?;
                }
                match total {
                    Some(total) if total > end + 1 => {
                        file.set_len(total).await?;
                        file.flush().await?;
                        drop(file);
                        self.fetch_chunks(end + 1, total, validator).await?;
                        total
                    }
                    Some(total) => total,
                    None => {
                        self.fetch_rest(&mut file, end + 1, None, None, validator)
                            .await?
                    }
                }
            }
            Answer::Partial { start, .. } => {
                return Err(DownloadError::Range(format!(
                    "asked {} for bytes from 0, got bytes from {start}",
                    self.url
                ))
                .into());
            }
            Answer::Whole => {
                let length = response.content_length();
                if let Some(length) = length {
                    self.check_size(length)?;
                    self.learn(length);
                }
                self.fetch_rest(&mut file, 0, Some(response), length, validator)
                    .await?
            }
            Answer::Unsatisfiable => 0,
        };
        if size == 0 {
            return Err(DownloadError::Empty.into());
        }
        let on_disk = tokio::fs::metadata(self.path).await?.len();
        if on_disk != size {
            return Err(DownloadError::Range(format!(
                "download of {} left {on_disk} bytes on disk, {size} expected",
                self.url
            ))
            .into());
        }
        Ok((size, content_type))
    }

    /// Fetches bytes `from..total` as ranged chunks over the configured connections,
    /// each into its place in the file.
    async fn fetch_chunks(
        &self,
        from: u64,
        total: u64,
        validator: Option<&str>,
    ) -> Result<(), Hitch> {
        let size = self.config.chunk_bytes.max(1);
        let mut ranges = Vec::new();
        let mut start = from;
        while start < total {
            let end = start.saturating_add(size).min(total) - 1;
            ranges.push((start, end));
            start = end + 1;
        }
        let mut chunks = futures::stream::iter(
            ranges
                .into_iter()
                .map(|(start, end)| self.fetch_range(start, end, validator)),
        )
        .buffer_unordered(self.config.connections.max(1));
        while let Some(chunk) = chunks.next().await {
            chunk?;
        }
        Ok(())
    }

    /// Fetches bytes `start..=end` into their place in the file, picking up from wherever
    /// a broken transfer stopped.
    async fn fetch_range(&self, start: u64, end: u64, validator: Option<&str>) -> Result<(), Hitch> {
        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .open(self.path)
            .await?;
        let mut at = start;
        loop {
            let response = self.request(at, Some(end), validator).await?;
            match classify(&response, self.url)? {
                Answer::Partial {
                    start: got,
                    end: got_end,
                    ..
                } if got == at && got_end <= end => {
                    file.seek(SeekFrom::Start(at)).await?;
                    let promised = got_end - at + 1;
                    let (written, broke) = self
                        .write_body(response, &mut file, at, Some(promised))
                        .await?;
                    at += written;
                    if at > end {
                        file.flush().await?;
                        return Ok(());
                    }
                    if written == promised && broke.is_none() {
                        // The host serves shorter ranges than asked for. The rest follows.
                        continue;
                    }
                    self.resume(short(broke, at, self.url)).await?;
                }
                Answer::Partial {
                    start: got,
                    end: got_end,
                    ..
                } => {
                    return Err(DownloadError::Range(format!(
                        "asked {} for bytes {at}-{end}, got {got}-{got_end}",
                        self.url
                    ))
                    .into());
                }
                Answer::Whole => return Err(Hitch::Whole),
                Answer::Unsatisfiable => {
                    return Err(DownloadError::Range(format!(
                        "{} has no byte {at}, though it said it had {}",
                        self.url,
                        end + 1
                    ))
                    .into());
                }
            }
        }
    }

    /// Streams from byte `from` to the end of the file, however long it turns out to be,
    /// picking up from wherever a broken transfer stopped. `response` is an answer already
    /// in hand for `from`, and `total` the length when known. A host that answers the
    /// pick-up with the whole file gets the file started over from its first byte. The
    /// file's length.
    async fn fetch_rest(
        &self,
        file: &mut File,
        mut from: u64,
        mut response: Option<Response>,
        mut total: Option<u64>,
        validator: Option<&str>,
    ) -> Result<u64, Hitch> {
        loop {
            let answer = match response.take() {
                Some(answer) => answer,
                None => {
                    let next = self.request(from, None, validator).await?;
                    match classify(&next, self.url)? {
                        Answer::Partial {
                            start, total: said, ..
                        } if start == from => {
                            if let Some(said) = said {
                                self.check_size(said)?;
                                self.learn(said);
                                total = Some(said);
                            }
                            next
                        }
                        Answer::Partial { start, .. } => {
                            return Err(DownloadError::Range(format!(
                                "asked {} for bytes from {from}, got bytes from {start}",
                                self.url
                            ))
                            .into());
                        }
                        Answer::Whole => {
                            self.start_over();
                            file.set_len(0).await?;
                            from = 0;
                            total = next.content_length();
                            if let Some(length) = total {
                                self.check_size(length)?;
                                self.learn(length);
                            }
                            next
                        }
                        Answer::Unsatisfiable => match total {
                            Some(total) if total != from => {
                                return Err(DownloadError::Range(format!(
                                    "{} ended at {from} bytes of the {total} it promised",
                                    self.url
                                ))
                                .into());
                            }
                            _ => {
                                file.flush().await?;
                                return Ok(from);
                            }
                        },
                    }
                }
            };
            file.seek(SeekFrom::Start(from)).await?;
            let promised = total.map(|total| total.saturating_sub(from));
            let (written, broke) = self.write_body(answer, file, from, promised).await?;
            from += written;
            let complete = match total {
                Some(total) => from >= total,
                None => broke.is_none(),
            };
            if complete {
                file.flush().await?;
                return Ok(from);
            }
            self.resume(short(broke, from, self.url)).await?;
        }
    }

    /// Writes the body of `response` into `file` at `offset`, decrypting as it goes: the
    /// bytes written, and the error that cut the body short when one did. A body longer
    /// than the `promised` bytes is refused.
    async fn write_body(
        &self,
        response: Response,
        file: &mut File,
        offset: u64,
        promised: Option<u64>,
    ) -> Result<(u64, Option<HttpError>), DownloadError> {
        let mut decryptor = match self.cipher {
            Some(cipher) => Some(Decryptor::at(cipher, offset)?),
            None => None,
        };
        let mut stream = response.into_stream();
        let mut written = 0u64;
        let mut broke = None;
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    broke = Some(error);
                    break;
                }
            };
            let len = chunk.len() as u64;
            if len == 0 {
                continue;
            }
            if let Some(promised) = promised
                && written + len > promised
            {
                return Err(DownloadError::Range(format!(
                    "{} sent more than the {promised} bytes it promised",
                    self.url
                )));
            }
            let size = offset + written + len;
            if size > self.max_bytes {
                return Err(DownloadError::TooLarge {
                    size,
                    limit: self.max_bytes,
                });
            }
            match decryptor.as_mut() {
                Some(decryptor) => {
                    let mut plain = chunk.to_vec();
                    decryptor.apply(&mut plain);
                    file.write_all(&plain).await?;
                }
                None => file.write_all(&chunk).await?,
            }
            written += len;
            self.count(len);
        }
        Ok((written, broke))
    }
}

impl HttpDownloader {
    pub fn new(http: Http, ffmpeg: Ffmpeg) -> Self {
        Self { http, ffmpeg }
    }

    /// Fetches `url` into `path`, decrypting with `cipher` when the host stores the file
    /// encrypted, failing once it is longer than `max_bytes`. The bytes written and the
    /// content type served. Nothing is left at `path` when it fails.
    #[allow(clippy::too_many_arguments)]
    async fn fetch(
        &self,
        url: &Url,
        headers: &[(String, String)],
        platform: &str,
        cipher: Option<&Cipher>,
        path: &Path,
        max_bytes: u64,
        config: &DownloadConfig,
        tally: &Tally<'_>,
    ) -> Result<(u64, Option<String>), DownloadError> {
        let transfer = Transfer {
            http: &self.http,
            url,
            headers,
            platform,
            cipher,
            path,
            max_bytes,
            config,
            pauses: self.http.config().retry,
            tally,
            counted: AtomicU64::new(0),
            sized: AtomicBool::new(false),
            resumes: AtomicU32::new(0),
        };
        let result = transfer.fetch().await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(path).await;
        }
        result
    }
}

/// The extension a separate audio file gets, by its codec.
fn audio_extension(codec: Option<&AudioCodec>) -> &'static str {
    match codec {
        Some(AudioCodec::Aac) => "m4a",
        Some(AudioCodec::Opus) | Some(AudioCodec::Vorbis) => "webm",
        Some(AudioCodec::Mp3) => "mp3",
        Some(AudioCodec::Flac) => "flac",
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
        let tally = Tally::new(&progress, variant.size);
        let Some(audio_url) = &variant.audio_url else {
            let part = dest_dir.join("source.part");
            let (size, content_type) = self
                .fetch(
                    &variant.url,
                    &variant.headers,
                    &context.platform,
                    variant.cipher.as_ref(),
                    &part,
                    context.max_bytes,
                    &context.download,
                    &tally,
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
                variant.cipher.as_ref(),
                &video_path,
                context.max_bytes,
                &context.download,
                &tally,
            )
            .await?;
        let audio_path =
            dest_dir.join(format!("audio.{}", audio_extension(variant.audio.as_ref())));
        let remaining = context.max_bytes.saturating_sub(written).max(1);
        self.fetch(
            audio_url,
            &variant.headers,
            &context.platform,
            None,
            &audio_path,
            remaining,
            &context.download,
            &tally,
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
    use std::sync::Arc;

    use super::*;
    use crate::http::transport::site::{Reply, Site};
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };
    use crate::media::VideoCodec;

    const FILE: &str = "https://cdn.test/file";

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

    /// Bytes that differ from place to place, so a chunk written to the wrong offset shows.
    fn pattern(len: usize) -> Vec<u8> {
        (0..len as u64)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect()
    }

    fn context(connections: usize, chunk_bytes: u64, resume_attempts: u32) -> DownloadContext {
        let mut context = DownloadContext::new(50_000_000);
        context.download = DownloadConfig {
            connections,
            chunk_bytes,
            resume_attempts,
        };
        context
    }

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("discoclip-{tag}-{}", uuid::Uuid::now_v7()))
    }

    fn file_reply(body: &[u8]) -> Reply {
        Reply::new("application/octet-stream", body.to_vec()).etag("\"v1\"")
    }

    async fn fetch_from(
        site: &Arc<Site>,
        ffmpeg: &Ffmpeg,
        context: &DownloadContext,
        dir: &Path,
        cipher: Option<Cipher>,
    ) -> (Result<Downloaded, DownloadError>, Progress) {
        let http = Http::with_transport(site.clone(), Http::test_config());
        let downloader = HttpDownloader::new(http, ffmpeg.clone());
        let mut variant = Variant::file(Url::parse(FILE).unwrap());
        variant.container = Some(Container::Mp4);
        variant.cipher = cipher;
        let (progress, watched) = tokio::sync::watch::channel(Progress::default());
        let result = downloader.download(&variant, dir, context, progress).await;
        let last = *watched.borrow();
        (result, last)
    }

    #[tokio::test]
    async fn a_host_serving_ranges_is_fetched_in_parallel_chunks() {
        let dir = temp_dir("chunks");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let body = pattern(100_000);
        let site = Site::new();
        site.put(FILE, file_reply(&body));
        let (result, progress) =
            fetch_from(&site, &ffmpeg, &context(3, 16_000, 5), &dir.join("job"), None).await;
        let downloaded = result.unwrap();
        assert_eq!(downloaded.file.size, 100_000);
        assert_eq!(tokio::fs::read(&downloaded.file.path).await.unwrap(), body);
        assert_eq!(progress.done, 100_000);
        assert_eq!(progress.total, Some(100_000));
        let seen = site.seen();
        assert_eq!(seen.len(), 7, "{seen:?}");
        assert_eq!(seen[0].range.as_deref(), Some("bytes=0-15999"));
        assert_eq!(seen[0].if_range, None);
        let mut rest: Vec<String> = seen[1..]
            .iter()
            .map(|s| {
                assert_eq!(s.if_range.as_deref(), Some("\"v1\""));
                s.range.clone().unwrap()
            })
            .collect();
        rest.sort();
        assert_eq!(
            rest,
            vec![
                "bytes=16000-31999",
                "bytes=32000-47999",
                "bytes=48000-63999",
                "bytes=64000-79999",
                "bytes=80000-95999",
                "bytes=96000-99999",
            ]
        );

        // A file no longer than one chunk takes one request.
        let site = Site::new();
        site.put(FILE, file_reply(&body[..10_000]));
        let (result, _) =
            fetch_from(&site, &ffmpeg, &context(3, 16_000, 5), &dir.join("small"), None).await;
        assert_eq!(result.unwrap().file.size, 10_000);
        assert_eq!(site.hits(FILE), 1);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn a_broken_transfer_is_resumed_from_where_it_stopped() {
        let dir = temp_dir("resume");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let body = pattern(100_000);
        let site = Site::new();
        site.put(FILE, file_reply(&body));
        site.break_at(FILE, 0, 5_000);
        site.break_at(FILE, 32_000, 7_500);
        site.break_at(FILE, 39_500, 1_000);
        let (result, progress) =
            fetch_from(&site, &ffmpeg, &context(2, 16_000, 5), &dir.join("job"), None).await;
        let downloaded = result.unwrap();
        assert_eq!(tokio::fs::read(&downloaded.file.path).await.unwrap(), body);
        assert_eq!(progress.done, 100_000);
        let ranges = site.ranges_seen(FILE);
        assert_eq!(ranges.len(), 10, "{ranges:?}");
        assert_eq!(ranges[0], "bytes=0-15999");
        assert_eq!(ranges[1], "bytes=5000-15999");
        assert!(ranges.contains(&"bytes=39500-47999".to_string()), "{ranges:?}");
        assert!(ranges.contains(&"bytes=40500-47999".to_string()), "{ranges:?}");

        // One more break than the resumes allowed fails the download and leaves nothing.
        let site = Site::new();
        site.put(FILE, file_reply(&body));
        site.break_at(FILE, 0, 5_000);
        site.break_at(FILE, 5_000, 1_000);
        site.break_at(FILE, 6_000, 1_000);
        let (result, _) =
            fetch_from(&site, &ffmpeg, &context(2, 16_000, 2), &dir.join("spent"), None).await;
        match result.unwrap_err() {
            DownloadError::Interrupted { resumes: 2, last, .. } => {
                assert!(last.contains("connection reset"), "{last}");
                assert!(last.contains("after 7000 bytes"), "{last}");
            }
            other => panic!("{other:?}"),
        }
        assert!(!dir.join("spent").join("source.part").exists());
        assert!(!dir.join("spent").join("source.mp4").exists());
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn a_host_without_ranges_is_streamed_whole_and_started_over_when_it_breaks() {
        let dir = temp_dir("whole");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let body = pattern(50_000);
        let site = Site::without_ranges();
        site.put(FILE, file_reply(&body));
        site.break_at(FILE, 0, 20_000);
        let (result, progress) =
            fetch_from(&site, &ffmpeg, &context(4, 16_000, 5), &dir.join("job"), None).await;
        let downloaded = result.unwrap();
        assert_eq!(tokio::fs::read(&downloaded.file.path).await.unwrap(), body);
        assert_eq!(progress.done, 50_000);
        assert_eq!(progress.total, Some(50_000));
        // The whole file came back to the first request. The pick-up was asked for as a
        // range, answered with the whole file again, and taken from the start.
        assert_eq!(
            site.ranges_seen(FILE),
            vec!["bytes=0-15999", "bytes=20000-"],
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn a_file_that_changes_under_the_download_is_fetched_again_whole() {
        let dir = temp_dir("changed");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let old = pattern(60_000);
        let new: Vec<u8> = pattern(45_000).into_iter().map(|b| b ^ 0x5a).collect();
        let site = Site::new();
        site.put_series(
            FILE,
            vec![
                file_reply(&old),
                file_reply(&old),
                Reply::new("application/octet-stream", new.clone()).etag("\"v2\""),
            ],
        );
        let (result, progress) =
            fetch_from(&site, &ffmpeg, &context(1, 20_000, 5), &dir.join("job"), None).await;
        let downloaded = result.unwrap();
        assert_eq!(downloaded.file.size, 45_000);
        assert_eq!(tokio::fs::read(&downloaded.file.path).await.unwrap(), new);
        assert_eq!(progress.done, 45_000);
        assert_eq!(progress.total, Some(45_000));
        let seen = site.seen();
        // Two chunks of the old file, a chunk request answered whole because the etag no
        // longer matched, then the new file from its first byte in chunks of its own.
        assert_eq!(seen[2].if_range.as_deref(), Some("\"v1\""));
        assert_eq!(seen[3].range.as_deref(), Some("bytes=0-19999"));
        assert_eq!(seen[3].if_range, None);
        assert_eq!(seen[4].if_range.as_deref(), Some("\"v2\""));
        assert_eq!(seen.len(), 6, "{seen:?}");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn a_file_longer_than_the_limit_is_refused_from_its_first_answer() {
        let dir = temp_dir("large");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let site = Site::new();
        site.put(FILE, file_reply(&pattern(100_000)));
        let mut context = context(4, 16_000, 5);
        context.max_bytes = 90_000;
        let (result, _) = fetch_from(&site, &ffmpeg, &context, &dir.join("job"), None).await;
        assert!(matches!(
            result.unwrap_err(),
            DownloadError::TooLarge {
                size: 100_000,
                limit: 90_000
            }
        ));
        assert_eq!(site.hits(FILE), 1);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn content_ranges_are_read() {
        assert_eq!(
            parse_content_range("bytes 0-1023/4096"),
            Some((0, 1023, Some(4096)))
        );
        assert_eq!(parse_content_range("bytes 10-19/*"), Some((10, 19, None)));
        assert_eq!(parse_content_range("bytes 5-4/10"), None);
        assert_eq!(parse_content_range("bytes 0-10/10"), None);
        assert_eq!(parse_content_range("items 0-1/2"), None);
        assert_eq!(parse_content_range("bytes */10"), None);
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

    #[tokio::test]
    async fn an_encrypted_file_is_decrypted_as_it_arrives() {
        use ctr::cipher::{KeyIvInit, StreamCipher};
        let dir = std::env::temp_dir().join(format!("discoclip-ctr-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let key = [7u8; 16];
        let nonce = [1, 2, 3, 4, 5, 6, 7, 8];
        let plain: Vec<u8> = (0..100_000u32).map(|i| (i % 251) as u8).collect();
        let mut encrypted = plain.clone();
        let mut iv = [0u8; 16];
        iv[..8].copy_from_slice(&nonce);
        ctr::Ctr128BE::<aes::Aes128>::new(&key.into(), &iv.into()).apply_keystream(&mut encrypted);
        let cipher = Cipher::Aes128Ctr { key, nonce };

        // As one stream, from a host without ranges.
        let mut fixture = Fixture::new("ctr", None);
        fixture.exchanges.push(serve(
            "https://cdn.test/locked",
            "application/octet-stream",
            &encrypted,
        ));
        let downloader = HttpDownloader::new(Http::replay(fixture), ffmpeg.clone());
        let mut variant = Variant::file(Url::parse("https://cdn.test/locked").unwrap());
        variant.container = Some(Container::Mp4);
        variant.cipher = Some(cipher.clone());
        let (progress, _) = tokio::sync::watch::channel(Progress::default());
        let downloaded = downloader
            .download(
                &variant,
                &dir.join("job"),
                &DownloadContext::new(50_000_000),
                progress,
            )
            .await
            .unwrap();
        assert_eq!(tokio::fs::read(&downloaded.file.path).await.unwrap(), plain);

        // In parallel chunks, each decrypted from its own offset, one of them resumed
        // part way through.
        let site = Site::new();
        site.put(FILE, file_reply(&encrypted));
        site.break_at(FILE, 60_000, 12_345);
        let (result, _) = fetch_from(
            &site,
            &ffmpeg,
            &context(3, 30_000, 5),
            &dir.join("chunks"),
            Some(cipher),
        )
        .await;
        let downloaded = result.unwrap();
        assert_eq!(tokio::fs::read(&downloaded.file.path).await.unwrap(), plain);
        assert!(site.ranges_seen(FILE).contains(&"bytes=72345-89999".to_string()));
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
