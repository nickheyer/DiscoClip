//! Captures pages whose player feeds a media source extension rather than playing a file
//! or a manifest the engine could fetch itself. The page is opened in a headless
//! Chromium, every buffer its player appends to a `SourceBuffer` is copied out as it goes
//! in, playback is driven to the end at the fastest rate the browser plays, and the
//! copied streams are put back together into one file: fragments the player appended
//! twice kept once, fragments appended out of order put in order, and a stream whose
//! codec configuration changed along the way joined by decoding it. A stream without an
//! end is recorded as it goes instead: each run of what the player appends is decoded and
//! encoded again on its way into the growing recording, so a rendition switch mid-stream
//! is absorbed, and the capture ends at its limit, when a person stops it, or when the
//! stream goes quiet.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::task::JoinHandle;

use super::recording::{Feed, Lane, Picture, Recorder, Spec, Treatment};
use super::segments::{Budget, mux_parts};
use super::{DownloadContext, DownloadError, Downloaded, Downloader, mp4};
use crate::browser::{self, Browser, BrowserError, Cdp, Event};
use crate::event::{Progress, ProgressSender};
use crate::ffmpeg::{Ffmpeg, summarize};
use crate::http::{Cookie, Http};
use crate::media::LocalFile;
use crate::resolve::{Variant, VariantKind};

/// What runs in every document of the page before its own scripts.
const HOOK: &str = include_str!("browser_hook.js");
/// The function the hook reaches the engine through.
const BINDING: &str = "__discoclip";
/// How long the page's player may take to open its first source buffer.
const LOAD: Duration = Duration::from_secs(60);
/// How long playback may go without new media or progress before the capture ends.
const STALL: Duration = Duration::from_secs(60);
/// How often a player that has not started, or has paused, is prodded.
const NUDGE_EVERY: Duration = Duration::from_secs(4);
/// How long a prod may take before it is given up on.
const NUDGE_TIMEOUT: Duration = Duration::from_secs(3);
/// How often the capture looks at where playback stands.
const TICK: Duration = Duration::from_millis(500);
/// How close to the end playback must get for the capture to count as complete.
const END_SLACK: f64 = 0.5;
/// The largest box or element a media source stream may carry.
const MAX_UNIT: usize = 512 * 1024 * 1024;
/// How long a page whose player reports nothing gets, after its first append, before its
/// stream counts as one without an end.
const DECIDE: Duration = Duration::from_secs(3);

pub struct BrowserDownloader {
    http: Http,
    ffmpeg: Ffmpeg,
    load_timeout: Duration,
    stall: Duration,
}

impl BrowserDownloader {
    pub fn new(http: Http, ffmpeg: Ffmpeg) -> Self {
        Self {
            http,
            ffmpeg,
            load_timeout: LOAD,
            stall: STALL,
        }
    }

    /// How long the page's player may take to open its first source buffer.
    pub fn load_timeout(mut self, load_timeout: Duration) -> Self {
        self.load_timeout = load_timeout;
        self
    }

    /// How long playback may go without new media or progress before the capture ends.
    pub fn quiet_after(mut self, stall: Duration) -> Self {
        self.stall = stall;
        self
    }
}

fn process(message: impl Into<String>) -> DownloadError {
    DownloadError::Process(message.into())
}

fn bad(message: impl Into<String>) -> DownloadError {
    DownloadError::Segment(message.into())
}

impl From<BrowserError> for DownloadError {
    fn from(error: BrowserError) -> Self {
        DownloadError::Process(error.to_string())
    }
}

/// The byte stream format a source buffer takes, from its MIME type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Mp4,
    WebM,
}

impl Format {
    fn of(mime: &str) -> Option<Self> {
        let essence = mime
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        match essence.as_str() {
            "video/mp4" | "audio/mp4" | "audio/aac" => Some(Format::Mp4),
            "video/webm" | "audio/webm" => Some(Format::WebM),
            _ => None,
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Format::Mp4 => "mp4",
            Format::WebM => "webm",
        }
    }
}

/// A complete piece of a stream, as a splitter hands it over.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Piece {
    /// An initialization section with the codec configuration it declares.
    Init { bytes: Vec<u8>, codec: Vec<u8> },
    /// A media segment, keyed by its track and the decode time it starts at.
    Fragment { bytes: Vec<u8>, key: (u32, u64) },
}

/// Cuts an ISO BMFF byte stream into initialization sections and media segments as the
/// bytes arrive, whatever pieces they arrive in.
#[derive(Default)]
struct Mp4Splitter {
    /// The `ftyp` waiting for its `moov`.
    ftyp: Option<Vec<u8>>,
    /// The `moof` collecting its `mdat` boxes, and how many it has.
    current: Option<((u32, u64), Vec<u8>, usize)>,
}

impl Mp4Splitter {
    /// Hands over the segment being collected. One without any `mdat` yet carries no
    /// samples and is dropped.
    fn flush(&mut self, out: &mut Vec<Piece>) {
        if let Some((key, bytes, mdats)) = self.current.take()
            && mdats > 0
        {
            out.push(Piece::Fragment { bytes, key });
        }
    }

    /// Hands over what is complete and drops what is not: the segment being collected
    /// once it has media data; not a partial box, nor an `ftyp` without its `moov`.
    fn finish(&mut self, pending: &mut Vec<u8>, out: &mut Vec<Piece>) -> Result<(), DownloadError> {
        self.push(pending, out)?;
        self.flush(out);
        self.ftyp = None;
        pending.clear();
        Ok(())
    }

    fn push(&mut self, pending: &mut Vec<u8>, out: &mut Vec<Piece>) -> Result<(), DownloadError> {
        loop {
            if pending.len() < 8 {
                return Ok(());
            }
            let size32 = u32::from_be_bytes([pending[0], pending[1], pending[2], pending[3]]);
            let kind: [u8; 4] = [pending[4], pending[5], pending[6], pending[7]];
            let (size, header) = match size32 {
                0 => {
                    return Err(bad(
                        "a box running to the end of the stream is not valid in a media source",
                    ));
                }
                1 => {
                    if pending.len() < 16 {
                        return Ok(());
                    }
                    let large = u64::from_be_bytes(pending[8..16].try_into().unwrap());
                    (usize::try_from(large).unwrap_or(usize::MAX), 16)
                }
                n => (n as usize, 8),
            };
            if size < header {
                return Err(bad(format!(
                    "{} box smaller than its header",
                    String::from_utf8_lossy(&kind)
                )));
            }
            if size > MAX_UNIT {
                return Err(bad(format!(
                    "{} box of {size} bytes is larger than a media source segment can be",
                    String::from_utf8_lossy(&kind)
                )));
            }
            if pending.len() < size {
                return Ok(());
            }
            let bytes: Vec<u8> = pending.drain(..size).collect();
            match &kind {
                b"ftyp" => {
                    self.flush(out);
                    self.ftyp = Some(bytes);
                }
                b"moov" => {
                    self.flush(out);
                    let mut init = self.ftyp.take().unwrap_or_default();
                    init.extend_from_slice(&bytes);
                    let codec = mp4::sample_descriptions(&init)?;
                    out.push(Piece::Init { bytes: init, codec });
                }
                b"moof" => {
                    self.flush(out);
                    let key = mp4::fragment_key(&bytes)?;
                    self.current = Some((key, bytes, 0));
                }
                b"mdat" => {
                    if let Some((_, fragment, mdats)) = &mut self.current {
                        fragment.extend_from_slice(&bytes);
                        *mdats += 1;
                    }
                }
                // styp, sidx, prft, emsg, free, skip, uuid: the start of a segment, or
                // nothing a demuxer needs.
                _ => self.flush(out),
            }
        }
    }
}

const EBML: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const INFO: u32 = 0x1549_A966;
const TRACKS: u32 = 0x1654_AE6B;
const CUES: u32 = 0x1C53_BB6B;
const CLUSTER: u32 = 0x1F43_B675;
const CHAPTERS: u32 = 0x1043_A770;
const TAGS: u32 = 0x1254_C367;
const ATTACHMENTS: u32 = 0x1941_A469;
const VOID: u32 = 0xEC;
const CRC32: u32 = 0xBF;
const TIMECODE: u32 = 0xE7;
/// The elements that sit directly in a segment, which end a cluster of unknown size.
const LEVEL_ONE: [u32; 8] = [
    SEEK_HEAD,
    INFO,
    TRACKS,
    CUES,
    CLUSTER,
    CHAPTERS,
    TAGS,
    ATTACHMENTS,
];

/// An element header at the start of `data`: its id, its size (`None` when unknown) and
/// the header's length. `None` when the header is not complete yet.
fn ebml_header(data: &[u8]) -> Result<Option<(u32, Option<u64>, usize)>, DownloadError> {
    let Some(&first) = data.first() else {
        return Ok(None);
    };
    let id_len = first.leading_zeros() as usize + 1;
    if id_len > 4 {
        return Err(bad("invalid EBML element id"));
    }
    if data.len() < id_len {
        return Ok(None);
    }
    let id = data[..id_len]
        .iter()
        .fold(0u32, |id, byte| (id << 8) | u32::from(*byte));
    let Some(&size_first) = data.get(id_len) else {
        return Ok(None);
    };
    let size_len = size_first.leading_zeros() as usize + 1;
    if size_len > 8 {
        return Err(bad("invalid EBML element size"));
    }
    if data.len() < id_len + size_len {
        return Ok(None);
    }
    let mut size = u64::from(size_first) & ((1u64 << (8 - size_len)) - 1);
    for byte in &data[id_len + 1..id_len + size_len] {
        size = (size << 8) | u64::from(*byte);
    }
    let unknown = size == (1u64 << (7 * size_len)) - 1;
    Ok(Some((id, (!unknown).then_some(size), id_len + size_len)))
}

/// How many bytes an element id takes: its marker bit sits in its first byte.
fn ebml_id_len(id: u32) -> usize {
    4 - id.leading_zeros() as usize / 8
}

/// Writes `size` over the size field of an element header, in the bytes the field has.
fn write_ebml_size(field: &mut [u8], size: u64) {
    let len = field.len();
    field.copy_from_slice(&size.to_be_bytes()[8 - len..]);
    field[0] |= 0x80 >> (len - 1);
}

/// The timecode of the cluster whose body is `body`. `None` when it carries none.
fn cluster_timecode(body: &[u8]) -> Result<Option<u64>, DownloadError> {
    let mut pos = 0;
    while let Some((id, size, header)) = ebml_header(&body[pos..])? {
        let size = size.ok_or_else(|| bad("element of unknown size inside a cluster"))?;
        let start = pos + header;
        let end = start.saturating_add(usize::try_from(size).unwrap_or(usize::MAX));
        if end > body.len() {
            break;
        }
        if id == TIMECODE {
            return Ok(Some(
                body[start..end]
                    .iter()
                    .fold(0u64, |value, byte| (value << 8) | u64::from(*byte)),
            ));
        }
        pos = end;
    }
    Ok(None)
}

/// How far the complete children of a cluster reach, its body starting at `pos` in
/// `data` under a declared `size`, and whether the cluster is closed there: one of known
/// size by its declared end, one of unknown size by the next element of the segment.
fn cluster_extent(
    data: &[u8],
    pos: usize,
    size: Option<u64>,
) -> Result<(usize, bool), DownloadError> {
    let limit = match size {
        Some(size) => {
            let end = pos.saturating_add(usize::try_from(size).unwrap_or(usize::MAX));
            if end <= data.len() {
                return Ok((end, true));
            }
            data.len()
        }
        None => data.len(),
    };
    let mut at = pos;
    loop {
        let Some((id, child, header)) = ebml_header(&data[at..limit])? else {
            return Ok((at, false));
        };
        if size.is_none() && (id == EBML || id == SEGMENT || LEVEL_ONE.contains(&id)) {
            return Ok((at, true));
        }
        let child = child.ok_or_else(|| bad("element of unknown size inside a cluster"))?;
        let end = at
            .saturating_add(header)
            .saturating_add(usize::try_from(child).unwrap_or(usize::MAX));
        if end > limit {
            return Ok((at, false));
        }
        at = end;
    }
}

/// Cuts a WebM byte stream into initialization sections and clusters as the bytes arrive.
#[derive(Default)]
struct WebmSplitter {
    /// The EBML header, the segment header and what sits before the tracks.
    head: Vec<u8>,
}

impl WebmSplitter {
    /// Splits what `pending` holds. `ending` hands over what is complete and drops what
    /// is not: the complete blocks of a cluster still open, under the size they make; not
    /// a partial element, nor a head without its tracks.
    fn push(
        &mut self,
        pending: &mut Vec<u8>,
        out: &mut Vec<Piece>,
        ending: bool,
    ) -> Result<(), DownloadError> {
        while let Some((id, size, header)) = ebml_header(pending)? {
            let bounded = |size: u64| -> Result<usize, DownloadError> {
                let end = header.saturating_add(usize::try_from(size).unwrap_or(usize::MAX));
                if end > MAX_UNIT {
                    return Err(bad(format!(
                        "element {id:#x} of {size} bytes is larger than a media source segment can be"
                    )));
                }
                Ok(end)
            };
            let whole = |pending: &Vec<u8>| -> Result<Option<usize>, DownloadError> {
                let size = size.ok_or_else(|| {
                    bad(format!(
                        "element {id:#x} of unknown size in the media source stream"
                    ))
                })?;
                let end = bounded(size)?;
                Ok((pending.len() >= end).then_some(end))
            };
            match id {
                EBML => {
                    let Some(end) = whole(pending)? else {
                        break;
                    };
                    self.head = pending.drain(..end).collect();
                }
                SEGMENT => {
                    self.head.extend(pending.drain(..header));
                }
                SEEK_HEAD | INFO | VOID | CRC32 => {
                    let Some(end) = whole(pending)? else {
                        break;
                    };
                    let bytes: Vec<u8> = pending.drain(..end).collect();
                    if !self.head.is_empty() {
                        self.head.extend_from_slice(&bytes);
                    }
                }
                TRACKS => {
                    let Some(end) = whole(pending)? else {
                        break;
                    };
                    let tracks: Vec<u8> = pending.drain(..end).collect();
                    let mut init = std::mem::take(&mut self.head);
                    init.extend_from_slice(&tracks);
                    out.push(Piece::Init {
                        bytes: init,
                        codec: tracks,
                    });
                }
                CUES | CHAPTERS | TAGS | ATTACHMENTS => {
                    let Some(end) = whole(pending)? else {
                        break;
                    };
                    pending.drain(..end);
                }
                CLUSTER => {
                    if let Some(size) = size {
                        bounded(size)?;
                    }
                    let (end, closed) = cluster_extent(pending, header, size)?;
                    if closed {
                        let bytes: Vec<u8> = pending.drain(..end).collect();
                        let timecode = cluster_timecode(&bytes[header..])?
                            .ok_or_else(|| bad("cluster without a timecode"))?;
                        out.push(Piece::Fragment {
                            bytes,
                            key: (0, timecode),
                        });
                        continue;
                    }
                    if !ending {
                        break;
                    }
                    // Still open: its complete children, under the size they make. Without
                    // a timecode there is no block the player could have timed.
                    let mut bytes: Vec<u8> = pending.drain(..end).collect();
                    if let Some(timecode) = cluster_timecode(&bytes[header..])? {
                        if size.is_some() {
                            let body = (end - header) as u64;
                            write_ebml_size(&mut bytes[ebml_id_len(CLUSTER)..header], body);
                        }
                        out.push(Piece::Fragment {
                            bytes,
                            key: (0, timecode),
                        });
                    }
                    break;
                }
                other => {
                    return Err(bad(format!(
                        "unexpected element {other:#x} in the media source stream"
                    )));
                }
            }
        }
        if ending {
            self.head.clear();
            pending.clear();
        }
        Ok(())
    }
}

enum Splitter {
    Mp4(Mp4Splitter),
    WebM(WebmSplitter),
}

impl Splitter {
    fn for_format(format: Format) -> Self {
        match format {
            Format::Mp4 => Splitter::Mp4(Mp4Splitter::default()),
            Format::WebM => Splitter::WebM(WebmSplitter::default()),
        }
    }

    fn push(&mut self, pending: &mut Vec<u8>, out: &mut Vec<Piece>) -> Result<(), DownloadError> {
        match self {
            Splitter::Mp4(splitter) => splitter.push(pending, out),
            Splitter::WebM(splitter) => splitter.push(pending, out, false),
        }
    }

    /// Hands over what is complete and drops what is not: at the end of the stream, and
    /// when the player aborts an append, which keeps what the browser had parsed and
    /// drops the rest.
    fn finish(&mut self, pending: &mut Vec<u8>, out: &mut Vec<Piece>) -> Result<(), DownloadError> {
        match self {
            Splitter::Mp4(splitter) => splitter.finish(pending, out),
            Splitter::WebM(splitter) => splitter.push(pending, out, true),
        }
    }
}

/// One stretch of a source buffer's stream under one codec configuration, on disk as an
/// initialization section followed by its media segments in the order they arrived.
struct Run {
    path: PathBuf,
    file: tokio::fs::File,
    codec: Vec<u8>,
    init_len: u64,
    /// Each fragment's decode time, offset and length.
    fragments: Vec<(u64, u64, u64)>,
    /// Whether the fragments so far arrived in decode order.
    ordered: bool,
}

/// One `SourceBuffer` of the page, and what was appended to it.
struct Buffer {
    mime: String,
    format: Format,
    splitter: Splitter,
    pending: Vec<u8>,
    runs: Vec<Run>,
    seen: HashSet<(u32, u64)>,
    appended: u64,
    duplicates: usize,
    /// Media segments that arrived before any initialization section.
    stray: usize,
}

impl Buffer {
    fn is_video(&self) -> bool {
        self.mime
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("video/")
    }
}

/// A source buffer's stream put back together: the runs to join, in order.
struct Stream {
    mime: String,
    video: bool,
    bytes: u64,
    runs: Vec<PathBuf>,
    duplicates: usize,
    stray: usize,
}

/// How a page capture is kept: put together from what the player appended once
/// playback ends, or recorded as it goes when the stream has no end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Keeping {
    Undecided,
    Whole,
    Live,
}

/// The ffmpeg that turns one run of a buffer into a transport stream for the recorder:
/// what it reads on its standard input it decodes and encodes again, so the run's own
/// codec configuration goes no further than it, and what it writes on its standard
/// output the pump carries into the recorder's lane. Timestamps are kept, so the runs
/// of one buffer continue one another on the media's own timeline.
struct RunEncoder {
    child: tokio::process::Child,
    stdin: Option<tokio::process::ChildStdin>,
    /// Carries the encoded stream into the lane. Hands the feed back when the stream
    /// ends.
    pump: JoinHandle<(Feed, Result<(), DownloadError>)>,
    stderr: JoinHandle<Vec<u8>>,
}

impl RunEncoder {
    async fn spawn(
        ffmpeg: &Ffmpeg,
        format: Format,
        video: bool,
        muxed: bool,
        feed: Feed,
    ) -> Result<Self, DownloadError> {
        let mut args: Vec<&str> = vec![
            "-hide_banner",
            "-nostdin",
            "-nostats",
            "-loglevel",
            "warning",
            "-fflags",
            "+genpts",
            "-f",
            match format {
                Format::Mp4 => "mp4",
                Format::WebM => "matroska",
            },
            "-i",
            "pipe:0",
        ];
        args.extend(match (video, muxed) {
            (true, true) => ["-map", "0:v:0?", "-map", "0:a:0?"],
            (true, false) => ["-map", "0:v:0", "-an", "-sn"],
            (false, _) => ["-vn", "-map", "0:a:0", "-sn"],
        });
        args.extend([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-crf",
            "18",
            "-pix_fmt",
            "yuv420p",
            "-bf",
            "0",
            "-g",
            "60",
            "-c:a",
            "aac",
            "-b:a",
            "192k",
            "-ar",
            "48000",
            "-ac",
            "2",
            "-copyts",
            "-muxdelay",
            "0",
            "-muxpreload",
            "0",
            "-mpegts_copyts",
            "1",
            "-f",
            "mpegts",
            "pipe:1",
        ]);
        let mut child = Command::new(ffmpeg.ffmpeg_path())
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| process("the run encoder has no stdin"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| process("the run encoder has no stdout"))?;
        let mut stderr_pipe = child
            .stderr
            .take()
            .ok_or_else(|| process("the run encoder has no stderr"))?;
        let stderr = tokio::spawn(async move {
            let mut buf = Vec::new();
            let _ = stderr_pipe.read_to_end(&mut buf).await;
            buf
        });
        let pump = tokio::spawn(async move {
            let mut feed = feed;
            let mut buf = vec![0u8; 64 * 1024];
            loop {
                match stdout.read(&mut buf).await {
                    Ok(0) => break (feed, Ok(())),
                    Ok(n) => {
                        if let Err(error) = feed.write(&buf[..n]).await {
                            break (feed, Err(error));
                        }
                    }
                    Err(error) => break (feed, Err(error.into())),
                }
            }
        });
        Ok(Self {
            child,
            stdin: Some(stdin),
            pump,
            stderr,
        })
    }

    async fn write(&mut self, bytes: &[u8]) -> Result<(), DownloadError> {
        let Some(stdin) = self.stdin.as_mut() else {
            return Err(process("the run encoder's input is closed"));
        };
        stdin
            .write_all(bytes)
            .await
            .map_err(|error| process(format!("the run encoder stopped taking its input: {error}")))
    }

    /// Ends the run: the encoder writes out what it holds and the pump carries it into
    /// the lane. The feed, for the next run.
    async fn end(mut self) -> Result<Feed, DownloadError> {
        drop(self.stdin.take());
        let (feed, pumped) = self
            .pump
            .await
            .map_err(|error| process(format!("the run encoder's pump failed: {error}")))?;
        let status = self.child.wait().await?;
        let stderr = self.stderr.await.unwrap_or_default();
        if !status.success() {
            return Err(process(format!(
                "run encoder: exit status {}: {}",
                status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".into()),
                summarize(&String::from_utf8_lossy(&stderr))
            )));
        }
        pumped?;
        Ok(feed)
    }
}

/// One lane of the live recording: the buffer it carries, and the encoder of the run in
/// hand.
struct LiveLane {
    buffer: usize,
    /// Whether the buffer carries the picture.
    video: bool,
    /// Whether the buffer carries picture and sound both.
    muxed: bool,
    /// The recorder's input, held by the pump of the run being encoded while one runs.
    feed: Option<Feed>,
    encoder: Option<RunEncoder>,
}

impl LiveLane {
    /// Starts a run: the one before it, if any, is written out first, so the lane
    /// carries the runs one after another.
    async fn begin_run(
        &mut self,
        ffmpeg: &Ffmpeg,
        format: Format,
        init: &[u8],
    ) -> Result<(), DownloadError> {
        self.end_run().await?;
        let feed = self
            .feed
            .take()
            .ok_or_else(|| process("the lane's feed is held by a run that did not end"))?;
        let mut encoder = RunEncoder::spawn(ffmpeg, format, self.video, self.muxed, feed).await?;
        encoder.write(init).await?;
        self.encoder = Some(encoder);
        Ok(())
    }

    async fn fragment(&mut self, bytes: &[u8]) -> Result<(), DownloadError> {
        match self.encoder.as_mut() {
            Some(encoder) => encoder.write(bytes).await,
            None => Ok(()),
        }
    }

    async fn end_run(&mut self) -> Result<(), DownloadError> {
        if let Some(encoder) = self.encoder.take() {
            self.feed = Some(encoder.end().await?);
        }
        Ok(())
    }

    /// Ends the run in hand and the lane: the recorder sees the end of the stream.
    async fn close(&mut self) -> Result<(), DownloadError> {
        self.end_run().await?;
        if let Some(feed) = self.feed.as_mut() {
            feed.close().await;
        }
        Ok(())
    }
}

/// A recording made as the page plays: the recorder and the lanes that feed it.
struct LiveRecording {
    ffmpeg: Ffmpeg,
    recorder: Recorder,
    lanes: Vec<LiveLane>,
}

impl LiveRecording {
    fn lane_of(&mut self, buffer: usize) -> Option<&mut LiveLane> {
        self.lanes.iter_mut().find(|lane| lane.buffer == buffer)
    }
}

/// What a finished capture hands over: the streams to put together, or the recording
/// made as the page played, with the runs to clear away and the notes for the log.
enum Finished {
    Whole(Vec<Stream>),
    Live {
        file: Box<LocalFile>,
        streams: Vec<Stream>,
        notes: Vec<String>,
    },
}

/// Every source buffer the page opened, keyed by the session and execution context it
/// opened it in and its number there.
struct Capture {
    dir: PathBuf,
    budget: Budget,
    buffers: Vec<Buffer>,
    index: HashMap<(String, u64, u64), usize>,
    keeping: Keeping,
    /// The ffmpeg a live recording runs on, once the capture is kept live.
    ffmpeg: Option<Ffmpeg>,
    live: Option<LiveRecording>,
    /// Whether the byte budget could take no more of a live capture.
    full: bool,
}

type BufferKey = (String, u64, u64);

impl Capture {
    fn new(dir: &Path, max_bytes: u64) -> Self {
        Self {
            dir: dir.to_path_buf(),
            budget: Budget::new(max_bytes),
            buffers: Vec::new(),
            index: HashMap::new(),
            keeping: Keeping::Undecided,
            ffmpeg: None,
            live: None,
            full: false,
        }
    }

    fn keeping(&self) -> Keeping {
        self.keeping
    }

    /// Whether the capture is a live one: kept as a recording that grows as it goes.
    fn is_live(&self) -> bool {
        self.keeping == Keeping::Live
    }

    /// Whether the byte budget could take no more.
    fn full(&self) -> bool {
        self.full
    }

    /// Bytes of the recording so far, once one is being made.
    fn recorded(&self) -> Option<u64> {
        self.live.as_ref().map(|live| live.recorder.bytes())
    }

    /// Keeps the capture whole: the streams are put together once playback ends.
    fn keep_whole(&mut self) {
        if self.keeping == Keeping::Undecided {
            self.keeping = Keeping::Whole;
        }
    }

    /// Keeps the capture as a recording that grows as the page plays, starting from
    /// what was appended so far.
    async fn keep_live(&mut self, ffmpeg: Ffmpeg) -> Result<(), DownloadError> {
        if self.keeping != Keeping::Undecided {
            return Ok(());
        }
        self.keeping = Keeping::Live;
        self.ffmpeg = Some(ffmpeg);
        self.start_live().await
    }

    fn open(&mut self, key: BufferKey, mime: &str) -> Result<(), DownloadError> {
        let format = Format::of(mime).ok_or_else(|| {
            DownloadError::Unsupported(format!(
                "the player appends {mime}, which is not a media source format the engine reads"
            ))
        })?;
        self.buffers.push(Buffer {
            mime: mime.to_string(),
            format,
            splitter: Splitter::for_format(format),
            pending: Vec::new(),
            runs: Vec::new(),
            seen: HashSet::new(),
            appended: 0,
            duplicates: 0,
            stray: 0,
        });
        self.index.insert(key, self.buffers.len() - 1);
        Ok(())
    }

    fn appended(&self) -> u64 {
        self.buffers.iter().map(|b| b.appended).sum()
    }

    async fn data(&mut self, key: &BufferKey, bytes: &[u8]) -> Result<(), DownloadError> {
        let Some(&index) = self.index.get(key) else {
            return Ok(());
        };
        let buffer = &mut self.buffers[index];
        buffer.appended += bytes.len() as u64;
        buffer.pending.extend_from_slice(bytes);
        let mut pieces = Vec::new();
        buffer.splitter.push(&mut buffer.pending, &mut pieces)?;
        self.take(index, pieces).await
    }

    /// Takes in the player aborting an append: the segments and blocks the browser had
    /// parsed stay, the partial rest goes.
    async fn abort(&mut self, key: &BufferKey) -> Result<(), DownloadError> {
        let Some(&index) = self.index.get(key) else {
            return Ok(());
        };
        let buffer = &mut self.buffers[index];
        let mut pieces = Vec::new();
        buffer.splitter.finish(&mut buffer.pending, &mut pieces)?;
        self.take(index, pieces).await
    }

    /// Takes in the player changing the buffer's type, which aborts the append in flight
    /// the same way.
    async fn retype(&mut self, key: &BufferKey, mime: &str) -> Result<(), DownloadError> {
        let Some(&index) = self.index.get(key) else {
            return Ok(());
        };
        let format = Format::of(mime).ok_or_else(|| {
            DownloadError::Unsupported(format!(
                "the player switches to {mime}, which is not a media source format the engine reads"
            ))
        })?;
        self.abort(key).await?;
        let buffer = &mut self.buffers[index];
        buffer.mime = mime.to_string();
        buffer.format = format;
        buffer.splitter = Splitter::for_format(format);
        Ok(())
    }

    /// Spends `bytes` of the budget. A live capture the budget cannot hold ends instead
    /// of failing: whether the bytes were taken.
    fn spend(&mut self, bytes: u64) -> Result<bool, DownloadError> {
        if self.keeping == Keeping::Live && !self.budget.fits(bytes) {
            self.full = true;
            return Ok(false);
        }
        self.budget.take(bytes)?;
        Ok(true)
    }

    async fn take(&mut self, index: usize, pieces: Vec<Piece>) -> Result<(), DownloadError> {
        for piece in pieces {
            if self.full {
                break;
            }
            match piece {
                Piece::Init { bytes, codec } => {
                    if self.buffers[index]
                        .runs
                        .last()
                        .is_some_and(|run| run.codec == codec)
                    {
                        continue;
                    }
                    if !self.spend(bytes.len() as u64)? {
                        break;
                    }
                    let buffer = &mut self.buffers[index];
                    let path = self.dir.join(format!(
                        "browser-{index}-{}.{}",
                        buffer.runs.len(),
                        buffer.format.extension()
                    ));
                    let mut file = tokio::fs::File::create(&path).await?;
                    file.write_all(&bytes).await?;
                    buffer.runs.push(Run {
                        path,
                        file,
                        codec,
                        init_len: bytes.len() as u64,
                        fragments: Vec::new(),
                        ordered: true,
                    });
                    let format = buffer.format;
                    if let Some(live) = self.live.as_mut()
                        && let Some(lane) = live.lanes.iter_mut().find(|l| l.buffer == index)
                    {
                        lane.begin_run(&live.ffmpeg, format, &bytes).await?;
                    }
                }
                Piece::Fragment { bytes, key } => {
                    let buffer = &mut self.buffers[index];
                    if buffer.runs.last().is_none() {
                        buffer.stray += 1;
                        continue;
                    }
                    if !buffer.seen.insert(key) {
                        buffer.duplicates += 1;
                        continue;
                    }
                    if !self.spend(bytes.len() as u64)? {
                        break;
                    }
                    let buffer = &mut self.buffers[index];
                    let run = buffer.runs.last_mut().expect("a run to take the fragment");
                    let offset =
                        run.init_len + run.fragments.iter().map(|(_, _, len)| len).sum::<u64>();
                    run.file.write_all(&bytes).await?;
                    if run
                        .fragments
                        .last()
                        .is_some_and(|(time, _, _)| *time > key.1)
                    {
                        run.ordered = false;
                    }
                    run.fragments.push((key.1, offset, bytes.len() as u64));
                    if let Some(live) = self.live.as_mut()
                        && let Some(lane) = live.lane_of(index)
                    {
                        lane.fragment(&bytes).await?;
                    }
                }
            }
        }
        if self.keeping == Keeping::Live && self.live.is_none() {
            self.start_live().await?;
        }
        Ok(())
    }

    /// Starts the recording once the buffer carrying the picture has media in a run to
    /// read the picture's shape from: the recorder, a lane for that buffer and for the
    /// separate sound when there is one, and everything those buffers hold so far fed
    /// through. Until then, what arrives waits in the runs on disk.
    async fn start_live(&mut self) -> Result<(), DownloadError> {
        let Some(ffmpeg) = self.ffmpeg.clone() else {
            return Ok(());
        };
        let video = self.buffers.iter().position(|b| b.is_video());
        let audio = self.buffers.iter().position(|b| !b.is_video());
        let Some(primary) = video.or(audio) else {
            return Ok(());
        };
        let Some(run) = self.buffers[primary]
            .runs
            .iter_mut()
            .find(|r| !r.fragments.is_empty())
        else {
            return Ok(());
        };
        run.file.flush().await?;
        let path = run.path.clone();
        let info = ffmpeg
            .probe(&path)
            .await
            .map_err(|e| process(format!("{}: {e}", path.display())))?;
        let picture = match video {
            Some(_) => {
                let track = info
                    .video
                    .as_ref()
                    .ok_or_else(|| process("the video buffer's first run holds no picture"))?;
                Some(Picture {
                    width: track.width,
                    height: track.height,
                    fps: track.fps.filter(|fps| *fps > 0.0).unwrap_or(30.0),
                })
            }
            None => None,
        };
        let muxed = video.is_some() && info.audio.is_some();
        let separate_audio = audio.filter(|_| video.is_some() && !muxed);
        let spec = Spec {
            video: Treatment::Encode,
            audio: separate_audio.map(|_| Treatment::Encode),
            format: Some("mpegts"),
            picture,
            adts: false,
            language: None,
        };
        let recorder = Recorder::start(&ffmpeg, &self.dir, &spec).await?;
        let mut lanes = vec![LiveLane {
            buffer: primary,
            video: video.is_some(),
            muxed,
            feed: Some(recorder.feed(Lane::Video)),
            encoder: None,
        }];
        if let Some(audio) = separate_audio {
            lanes.push(LiveLane {
                buffer: audio,
                video: false,
                muxed: false,
                feed: Some(recorder.feed(Lane::Audio)),
                encoder: None,
            });
        }
        let mut live = LiveRecording {
            ffmpeg,
            recorder,
            lanes,
        };
        // What the chosen buffers hold so far goes through first: every earlier run
        // whole, and the run in hand left open for what follows.
        for lane_index in 0..live.lanes.len() {
            let buffer_index = live.lanes[lane_index].buffer;
            let format = self.buffers[buffer_index].format;
            let count = self.buffers[buffer_index].runs.len();
            for n in 0..count {
                let run = &mut self.buffers[buffer_index].runs[n];
                run.file.flush().await?;
                let init_len = usize::try_from(run.init_len).unwrap_or(usize::MAX);
                let bytes = tokio::fs::read(&run.path).await?;
                let lane = &mut live.lanes[lane_index];
                lane.begin_run(&live.ffmpeg, format, &bytes[..init_len.min(bytes.len())])
                    .await?;
                if bytes.len() > init_len {
                    lane.fragment(&bytes[init_len..]).await?;
                }
                if n + 1 < count {
                    lane.end_run().await?;
                }
            }
        }
        self.live = Some(live);
        Ok(())
    }

    /// Closes every run, putting the fragments of a run that arrived out of order into
    /// decode order, and describes each buffer's stream. A live capture's recording is
    /// written out and handed over with them.
    async fn finish(mut self) -> Result<Finished, DownloadError> {
        for index in 0..self.buffers.len() {
            let mut pieces = Vec::new();
            {
                let buffer = &mut self.buffers[index];
                buffer.splitter.finish(&mut buffer.pending, &mut pieces)?;
            }
            self.take(index, pieces).await?;
        }
        let live = self.live.take();
        let recorded: Vec<usize> = live
            .as_ref()
            .map(|live| live.lanes.iter().map(|lane| lane.buffer).collect())
            .unwrap_or_default();
        let mut streams = Vec::new();
        let mut notes = Vec::new();
        for (index, buffer) in self.buffers.into_iter().enumerate() {
            let video = buffer.is_video();
            let mut runs = Vec::new();
            for mut run in buffer.runs {
                run.file.flush().await?;
                if run.fragments.is_empty() {
                    let _ = tokio::fs::remove_file(&run.path).await;
                    continue;
                }
                if !run.ordered && live.is_none() {
                    reorder(&mut run).await?;
                }
                runs.push(run.path);
            }
            if runs.is_empty() {
                continue;
            }
            if live.is_some() && !recorded.contains(&index) {
                notes.push(format!(
                    "browser: a further {} stream of {} bytes was left out",
                    buffer.mime, buffer.appended
                ));
            }
            streams.push(Stream {
                video,
                mime: buffer.mime,
                bytes: buffer.appended,
                runs,
                duplicates: buffer.duplicates,
                stray: buffer.stray,
            });
        }
        match live {
            Some(mut live) => {
                for lane in live.lanes.iter_mut() {
                    lane.close().await?;
                }
                let file = live.recorder.finish().await?;
                Ok(Finished::Live {
                    file: Box::new(file),
                    streams,
                    notes,
                })
            }
            None => Ok(Finished::Whole(streams)),
        }
    }
}

/// Rewrites `run` with its fragments in decode order.
async fn reorder(run: &mut Run) -> Result<(), DownloadError> {
    let sorted_path = run.path.with_extension("sorted");
    let mut source = tokio::fs::File::open(&run.path).await?;
    let mut sorted = tokio::fs::File::create(&sorted_path).await?;
    let mut init = vec![0u8; usize::try_from(run.init_len).unwrap_or(usize::MAX)];
    source.read_exact(&mut init).await?;
    sorted.write_all(&init).await?;
    let mut order = run.fragments.clone();
    order.sort_by_key(|(time, offset, _)| (*time, *offset));
    let mut fragments = Vec::with_capacity(order.len());
    let mut at = run.init_len;
    for (time, offset, len) in order {
        source.seek(std::io::SeekFrom::Start(offset)).await?;
        let mut bytes = vec![0u8; usize::try_from(len).unwrap_or(usize::MAX)];
        source.read_exact(&mut bytes).await?;
        sorted.write_all(&bytes).await?;
        fragments.push((time, at, len));
        at += len;
    }
    sorted.flush().await?;
    drop(sorted);
    drop(source);
    tokio::fs::rename(&sorted_path, &run.path).await?;
    run.fragments = fragments;
    run.ordered = true;
    run.file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(&run.path)
        .await?;
    Ok(())
}

/// Where playback stands, from the page's media elements fed by a media source.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Playback {
    current: f64,
    duration: Option<f64>,
    buffered_end: f64,
    ended: bool,
    paused: bool,
    source_ended: bool,
}

/// The playback the page reports, over its elements fed by a media source. `None` when
/// none is.
fn playback_of(elements: &Value) -> Option<Playback> {
    let mut playback: Option<Playback> = None;
    for element in elements.as_array().into_iter().flatten() {
        if element["mse"] != true {
            continue;
        }
        let entry = playback.get_or_insert(Playback {
            paused: true,
            ..Playback::default()
        });
        let current = element["cur"].as_f64().unwrap_or(0.0);
        entry.current = entry.current.max(current);
        if let Some(duration) = element["dur"]
            .as_f64()
            .filter(|d| d.is_finite() && *d > 0.0)
        {
            entry.duration = Some(entry.duration.map_or(duration, |d: f64| d.max(duration)));
        }
        for range in element["buf"].as_array().into_iter().flatten() {
            if let Some(end) = range.get(1).and_then(Value::as_f64) {
                entry.buffered_end = entry.buffered_end.max(end);
            }
        }
        entry.ended |= element["ended"] == true;
        entry.paused &= element["paused"] == true;
        entry.source_ended |= element["ms"] == "ended";
    }
    playback
}

impl Playback {
    fn complete(&self) -> bool {
        if self.ended {
            return true;
        }
        match self.duration {
            Some(duration) => {
                self.current + END_SLACK >= duration
                    || (self.source_ended && self.buffered_end + END_SLACK >= duration)
            }
            None => false,
        }
    }

    fn describe(&self) -> String {
        match self.duration {
            Some(duration) => format!("{:.1} s of {:.1} s", self.current, duration),
            None => format!("{:.1} s", self.current),
        }
    }
}

/// How the capture came to an end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ending {
    /// Playback reached the end.
    Finished,
    /// A stream without an end stopped sending.
    Quiet,
    /// A stream without an end reached the capture limit.
    Cut,
    /// A stream without an end reached the byte limit.
    Bytes,
    /// A person stopped the capture.
    Stopped,
}

/// What every page and frame of the capture is set up with.
struct Setup {
    user_agent: Value,
    headers: Vec<(String, String)>,
    cookies: Vec<Value>,
    credentials: Option<(String, String)>,
}

impl Setup {
    fn new(
        browser: &Browser,
        variant: &Variant,
        jar: &[Cookie],
        credentials: Option<(String, String)>,
    ) -> Self {
        let version = browser.version();
        let (full, major) = version.numbers();
        let platform = if cfg!(target_os = "windows") {
            "Windows"
        } else if cfg!(target_os = "macos") {
            "macOS"
        } else {
            "Linux"
        };
        let architecture = if cfg!(target_arch = "aarch64") {
            "arm"
        } else {
            "x86"
        };
        let user_agent = json!({
            "userAgent": version.plain_user_agent(),
            "acceptLanguage": "en-US,en;q=0.9",
            "userAgentMetadata": {
                "brands": [
                    {"brand": "Chromium", "version": major},
                    {"brand": "Google Chrome", "version": major},
                    {"brand": "Not:A-Brand", "version": "24"},
                ],
                "fullVersionList": [
                    {"brand": "Chromium", "version": full},
                    {"brand": "Google Chrome", "version": full},
                    {"brand": "Not:A-Brand", "version": "24.0.0.0"},
                ],
                "fullVersion": full,
                "platform": platform,
                "platformVersion": "",
                "architecture": architecture,
                "model": "",
                "mobile": false,
                "bitness": "64",
            }
        });
        let headers: Vec<(String, String)> = variant
            .headers
            .iter()
            .filter(|(name, _)| !name.eq_ignore_ascii_case("user-agent"))
            .cloned()
            .collect();
        let now = jiff::Timestamp::now();
        let cookies = jar
            .iter()
            .filter(|cookie| !cookie.is_expired(now))
            .map(|cookie| {
                let mut value = json!({
                    "name": cookie.name,
                    "value": cookie.value,
                    "domain": if cookie.host_only { cookie.domain.clone() } else { format!(".{}", cookie.domain) },
                    "path": cookie.path,
                    "secure": cookie.secure,
                    "httpOnly": cookie.http_only,
                });
                if let Some(expires) = cookie.expires {
                    value["expires"] = json!(expires.as_second());
                }
                value
            })
            .collect();
        Self {
            user_agent,
            headers,
            cookies,
            credentials,
        }
    }
}

/// Sets a page or frame session up: the domains the capture listens to, the identity
/// pages see, the binding and the hook, and the frames and workers it may open.
async fn prepare(cdp: &Cdp, session: &str, setup: &Setup, first: bool) -> Result<(), BrowserError> {
    cdp.call(Some(session), "Page.enable", json!({})).await?;
    cdp.call(Some(session), "Runtime.enable", json!({})).await?;
    cdp.call(Some(session), "Network.enable", json!({})).await?;
    cdp.call(
        Some(session),
        "Emulation.setUserAgentOverride",
        setup.user_agent.clone(),
    )
    .await?;
    if !setup.headers.is_empty() {
        let headers: serde_json::Map<String, Value> = setup
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), Value::String(value.clone())))
            .collect();
        cdp.call(
            Some(session),
            "Network.setExtraHTTPHeaders",
            json!({"headers": headers}),
        )
        .await?;
    }
    if first && !setup.cookies.is_empty() {
        cdp.call(
            Some(session),
            "Network.setCookies",
            json!({"cookies": setup.cookies}),
        )
        .await?;
    }
    cdp.call(
        Some(session),
        "Runtime.addBinding",
        json!({"name": BINDING}),
    )
    .await?;
    cdp.call(
        Some(session),
        "Page.addScriptToEvaluateOnNewDocument",
        json!({"source": HOOK}),
    )
    .await?;
    if setup.credentials.is_some() {
        cdp.call(
            Some(session),
            "Fetch.enable",
            json!({"handleAuthRequests": true, "patterns": [{"urlPattern": "*"}]}),
        )
        .await?;
    }
    cdp.call(
        Some(session),
        "Target.setAutoAttach",
        json!({"autoAttach": true, "waitForDebuggerOnStart": true, "flatten": true}),
    )
    .await?;
    Ok(())
}

/// Sets a worker session up: the binding and the hook, for a player that runs its media
/// source in a worker.
async fn prepare_worker(cdp: &Cdp, session: &str) -> Result<(), BrowserError> {
    cdp.call(Some(session), "Runtime.enable", json!({})).await?;
    cdp.call(
        Some(session),
        "Runtime.addBinding",
        json!({"name": BINDING}),
    )
    .await?;
    cdp.call(
        Some(session),
        "Runtime.evaluate",
        json!({"expression": HOOK, "returnByValue": true}),
    )
    .await?;
    Ok(())
}

/// Prods the player in every page-like session: its controls clicked, its media asked
/// to play.
async fn nudge(cdp: &Cdp, sessions: &[String]) {
    for session in sessions {
        let _ = tokio::time::timeout(
            NUDGE_TIMEOUT,
            cdp.call(
                Some(session),
                "Runtime.evaluate",
                json!({
                    "expression": "typeof __discoclipNudge === 'function' ? __discoclipNudge() : -1",
                    "returnByValue": true,
                }),
            ),
        )
        .await;
    }
}

impl BrowserDownloader {
    /// Opens the page and copies out what its player appends until playback ends, or a
    /// stream without an end reaches its limit, goes quiet, or is stopped.
    async fn capture(
        &self,
        browser: &mut Browser,
        variant: &Variant,
        dest_dir: &Path,
        context: &DownloadContext,
        progress: &ProgressSender,
        credentials: Option<(String, String)>,
    ) -> Result<(Capture, Ending, Playback, Vec<String>), DownloadError> {
        let url = &variant.url;
        let mut events = browser
            .take_events()
            .ok_or_else(|| process("the browser's events were already taken"))?;
        let cdp = browser.cdp().clone();
        let page = browser.open_page().await?;
        let jar = self.http.jar(&context.platform).into_cookies();
        let setup = Setup::new(browser, variant, &jar, credentials);
        prepare(&cdp, &page.session, &setup, true).await?;
        let mut capture = Capture::new(dest_dir, context.max_bytes);
        if variant.live {
            capture.keep_live(self.ffmpeg.clone()).await?;
        }
        let mut playback: Option<Playback> = None;
        let mut sessions = vec![page.session.clone()];
        let mut notes = Vec::new();
        let started = Instant::now();
        let mut opened_at: Option<Instant> = None;
        let mut last_append: Option<Instant> = None;
        let mut last_advance = started;
        let mut furthest = 0.0f64;
        let mut last_nudge = started;
        let mut announced = false;
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        progress.send_replace(Progress::of(0, None));

        let navigated = cdp
            .call(
                Some(&page.session),
                "Page.navigate",
                json!({"url": url.as_str()}),
            )
            .await?;
        let main_frame = navigated["frameId"].as_str().map(str::to_owned);
        if let Some(text) = navigated["errorText"].as_str().filter(|t| !t.is_empty()) {
            // The browser gives up on a document served with an error status and no
            // body, after reporting the response: the events before its answer carry
            // the status, which is the error to report.
            while let Ok(event) = events.try_recv() {
                self.handle(
                    &cdp,
                    &page.session,
                    &setup,
                    main_frame.as_deref(),
                    event,
                    &mut capture,
                    &mut playback,
                    &mut sessions,
                    &mut opened_at,
                    &mut last_append,
                    &mut last_advance,
                    &mut furthest,
                )
                .await?;
            }
            return Err(process(format!("{url} could not be loaded: {text}")));
        }

        let ending = loop {
            tokio::select! {
                event = events.recv() => {
                    let Some(event) = event else {
                        return Err(process(format!(
                            "the browser went away during the capture of {url}: {}",
                            browser.stderr_tail().join(" | ")
                        )));
                    };
                    self.handle(
                        &cdp, &page.session, &setup, main_frame.as_deref(), event,
                        &mut capture, &mut playback, &mut sessions,
                        &mut opened_at, &mut last_append, &mut last_advance, &mut furthest,
                    ).await?;
                    if !announced && capture.recorded().is_some() {
                        announced = true;
                        context.capture.started(&dest_dir.join(super::recording::RECORDING));
                    }
                }
                _ = context.stop.cancelled(), if capture.is_live() => {
                    break Ending::Stopped;
                }
                _ = ticker.tick() => {
                    let now = Instant::now();
                    let elapsed = started.elapsed();
                    match opened_at {
                        None => {
                            if elapsed >= self.load_timeout {
                                return Err(process(format!(
                                    "{url}: the page's player opened no media source within {}s",
                                    self.load_timeout.as_secs()
                                )));
                            }
                            if now.duration_since(last_nudge) >= NUDGE_EVERY {
                                nudge(&cdp, &sessions).await;
                                last_nudge = Instant::now();
                            }
                        }
                        Some(_) => {
                            let state = playback.unwrap_or_default();
                            let live = capture.keeping() != Keeping::Whole;
                            progress.send_replace(if live {
                                Progress {
                                    done: elapsed.as_secs(),
                                    total: Some(context.max_live.as_secs()),
                                    bytes: capture.recorded(),
                                }
                            } else if let Some(duration) = state.duration {
                                Progress::of(state.current.round() as u64, Some(duration.round() as u64))
                            } else {
                                Progress::of(capture.appended(), None)
                            });
                            if capture.full() {
                                break Ending::Bytes;
                            }
                            if state.complete() {
                                break Ending::Finished;
                            }
                            let quiet = last_append.is_some_and(|at| now.duration_since(at) >= self.stall)
                                && now.duration_since(last_advance) >= self.stall;
                            if quiet {
                                if live {
                                    break Ending::Quiet;
                                }
                                if state.duration.is_some_and(|d| state.buffered_end + 1.0 >= d) {
                                    break Ending::Finished;
                                }
                                return Err(process(format!(
                                    "{url}: playback stalled at {} after {}s without new media",
                                    state.describe(),
                                    self.stall.as_secs()
                                )));
                            }
                            if elapsed >= context.max_live {
                                if live {
                                    break Ending::Cut;
                                }
                                return Err(process(format!(
                                    "{url}: the capture did not finish within {}s: reached {}",
                                    context.max_live.as_secs(),
                                    state.describe()
                                )));
                            }
                            if state.paused && !state.ended && now.duration_since(last_nudge) >= NUDGE_EVERY {
                                nudge(&cdp, &sessions).await;
                                last_nudge = Instant::now();
                            }
                        }
                    }
                }
            }
        };
        let state = playback.unwrap_or_default();
        let secs = started.elapsed().as_secs_f64();
        match ending {
            Ending::Finished => {
                if let Some(duration) = state.duration {
                    let total = duration.round() as u64;
                    progress.send_replace(Progress::of(total, Some(total)));
                }
            }
            Ending::Quiet => notes.push(format!(
                "browser: the stream went quiet after {}s without new media. Capture ended at {secs:.0} s.",
                self.stall.as_secs()
            )),
            Ending::Cut => notes.push(format!(
                "Capture ended at the {:.0} s limit.",
                context.max_live.as_secs_f64()
            )),
            Ending::Bytes => notes.push(format!(
                "Capture ended at the {} byte limit.",
                context.max_bytes
            )),
            Ending::Stopped => notes.push(format!("Capture stopped at {secs:.0} s.")),
        }
        Ok((capture, ending, state, notes))
    }

    /// Takes one event of the browser in.
    #[allow(clippy::too_many_arguments)]
    async fn handle(
        &self,
        cdp: &Cdp,
        page_session: &str,
        setup: &Setup,
        main_frame: Option<&str>,
        event: Event,
        capture: &mut Capture,
        playback: &mut Option<Playback>,
        sessions: &mut Vec<String>,
        opened_at: &mut Option<Instant>,
        last_append: &mut Option<Instant>,
        last_advance: &mut Instant,
        furthest: &mut f64,
    ) -> Result<(), DownloadError> {
        match event.method.as_str() {
            "Runtime.bindingCalled" if event.params["name"] == BINDING => {
                let session = event.session.clone().unwrap_or_default();
                let context = event.params["executionContextId"].as_u64().unwrap_or(0);
                let Ok(message) =
                    serde_json::from_str::<Value>(event.params["payload"].as_str().unwrap_or(""))
                else {
                    return Ok(());
                };
                let key = (session, context, message["sb"].as_u64().unwrap_or(0));
                match message["t"].as_str() {
                    Some("open") => {
                        capture.open(key, message["mime"].as_str().unwrap_or(""))?;
                        opened_at.get_or_insert_with(Instant::now);
                    }
                    Some("data") => {
                        let bytes =
                            BASE64
                                .decode(message["b"].as_str().unwrap_or(""))
                                .map_err(|e| {
                                    process(format!("the hook sent bytes that do not decode: {e}"))
                                })?;
                        capture.data(&key, &bytes).await?;
                        let first = last_append.is_none();
                        *last_append = Some(Instant::now());
                        // A player that reports nothing about its playback gets a while
                        // after its first append; then its stream counts as one without
                        // an end.
                        if capture.keeping() == Keeping::Undecided
                            && !first
                            && opened_at.is_some_and(|at| at.elapsed() >= DECIDE)
                            && playback.is_none()
                        {
                            capture.keep_live(self.ffmpeg.clone()).await?;
                        }
                    }
                    Some("abort") => capture.abort(&key).await?,
                    Some("type") => {
                        capture
                            .retype(&key, message["mime"].as_str().unwrap_or(""))
                            .await?;
                    }
                    Some("tick") => {
                        if let Some(state) = playback_of(&message["els"]) {
                            if state.current > *furthest + 0.01 {
                                *furthest = state.current;
                                *last_advance = Instant::now();
                            }
                            *playback = Some(state);
                            // Once media has been appended, the player knows whether its
                            // stream has an end: one without is recorded as it goes.
                            if capture.keeping() == Keeping::Undecided && last_append.is_some() {
                                if state.duration.is_none() {
                                    capture.keep_live(self.ffmpeg.clone()).await?;
                                } else {
                                    capture.keep_whole();
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            "Target.attachedToTarget" => {
                let Some(session) = event.params["sessionId"].as_str() else {
                    return Ok(());
                };
                let kind = event.params["targetInfo"]["type"].as_str().unwrap_or("");
                match kind {
                    "page" | "iframe" => match prepare(cdp, session, setup, false).await {
                        Ok(()) => sessions.push(session.to_string()),
                        Err(error) => {
                            tracing::debug!(session, kind, "frame could not be set up: {error}");
                        }
                    },
                    "worker" | "shared_worker" | "service_worker" => {
                        if let Err(error) = prepare_worker(cdp, session).await {
                            tracing::debug!(session, kind, "worker could not be set up: {error}");
                        }
                    }
                    _ => {}
                }
                if event.params["waitingForDebugger"] == true {
                    let _ = cdp
                        .call(Some(session), "Runtime.runIfWaitingForDebugger", json!({}))
                        .await;
                }
            }
            "Target.detachedFromTarget" => {
                if let Some(session) = event.params["sessionId"].as_str() {
                    sessions.retain(|s| s != session);
                }
            }
            "Inspector.targetCrashed" if event.session.as_deref() == Some(page_session) => {
                return Err(process("the page crashed in the browser"));
            }
            "Page.javascriptDialogOpening" => {
                if let Some(session) = &event.session {
                    let _ = cdp
                        .call(
                            Some(session),
                            "Page.handleJavaScriptDialog",
                            json!({"accept": true}),
                        )
                        .await;
                }
            }
            "Network.responseReceived"
                if event.session.as_deref() == Some(page_session)
                    && event.params["type"] == "Document"
                    && event.params["frameId"].as_str() == main_frame =>
            {
                let status = event.params["response"]["status"].as_u64().unwrap_or(0);
                if status >= 400 {
                    return Err(DownloadError::Status {
                        status: status as u16,
                        url: event.params["response"]["url"]
                            .as_str()
                            .unwrap_or("")
                            .to_string(),
                    });
                }
            }
            "Fetch.requestPaused" => {
                if let (Some(session), Some(request)) =
                    (&event.session, event.params["requestId"].as_str())
                {
                    let _ = cdp
                        .call(
                            Some(session),
                            "Fetch.continueRequest",
                            json!({"requestId": request}),
                        )
                        .await;
                }
            }
            "Fetch.authRequired" => {
                if let (Some(session), Some(request), Some((username, password))) = (
                    &event.session,
                    event.params["requestId"].as_str(),
                    &setup.credentials,
                ) {
                    let _ = cdp
                        .call(
                            Some(session),
                            "Fetch.continueWithAuth",
                            json!({
                                "requestId": request,
                                "authChallengeResponse": {
                                    "response": "ProvideCredentials",
                                    "username": username,
                                    "password": password,
                                }
                            }),
                        )
                        .await;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Joins the runs of one stream into one file. A stream in one run is that run. A
    /// stream whose codec configuration changed is decoded and encoded again, at the
    /// largest picture among its runs.
    async fn join(&self, stream: &Stream, dest: &Path) -> Result<PathBuf, DownloadError> {
        if let [only] = stream.runs.as_slice() {
            return Ok(only.clone());
        }
        let mut picture: Option<(u32, u32)> = None;
        for run in &stream.runs {
            let info = self
                .ffmpeg
                .probe(run)
                .await
                .map_err(|e| process(format!("{}: {e}", run.display())))?;
            if let Some(video) = info.video
                && picture.is_none_or(|(w, h)| video.width * video.height > w * h)
            {
                picture = Some((video.width, video.height));
            }
        }
        let list = dest.with_extension("runs.txt");
        let mut text = String::from("ffconcat version 1.0\n");
        for run in &stream.runs {
            let path = run
                .to_str()
                .ok_or_else(|| process(format!("{} is not valid UTF-8", run.display())))?;
            text.push_str(&format!("file '{}'\n", path.replace('\'', "'\\''")));
        }
        tokio::fs::write(&list, text).await?;
        let mut args: Vec<OsString> = vec![
            "-loglevel".into(),
            "warning".into(),
            "-f".into(),
            "concat".into(),
            "-safe".into(),
            "0".into(),
            "-i".into(),
            list.as_os_str().to_owned(),
            "-map".into(),
            "0:v:0?".into(),
            "-map".into(),
            "0:a:0?".into(),
            "-c:v".into(),
            "libx264".into(),
            "-preset".into(),
            "veryfast".into(),
            "-crf".into(),
            "16".into(),
            "-pix_fmt".into(),
            "yuv420p".into(),
        ];
        if let Some((width, height)) = picture {
            args.extend(["-vf".into(), format!("scale={width}:{height}").into()]);
        }
        args.extend([
            "-c:a".into(),
            "aac".into(),
            "-b:a".into(),
            "192k".into(),
            "-sn".into(),
            "-dn".into(),
            "-f".into(),
            "matroska".into(),
            dest.as_os_str().to_owned(),
        ]);
        self.ffmpeg
            .run(args, |_| {})
            .await
            .map_err(|e| process(e.to_string()))?;
        let _ = tokio::fs::remove_file(&list).await;
        Ok(dest.to_path_buf())
    }

    /// Puts the captured streams together into one file: the video stream with the most
    /// bytes and the audio stream with the most, or whichever of the two there is.
    async fn assemble(
        &self,
        streams: &[Stream],
        dest_dir: &Path,
        notes: &mut Vec<String>,
    ) -> Result<LocalFile, DownloadError> {
        let video = streams
            .iter()
            .enumerate()
            .filter(|(_, s)| s.video)
            .max_by_key(|(_, s)| s.bytes);
        let audio = streams
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.video)
            .max_by_key(|(_, s)| s.bytes);
        let chosen: Vec<usize> = video.iter().chain(audio.iter()).map(|(i, _)| *i).collect();
        for (index, stream) in streams.iter().enumerate() {
            if !chosen.contains(&index) {
                notes.push(format!(
                    "browser: a further {} stream of {} bytes was left out",
                    stream.mime, stream.bytes
                ));
            }
        }
        let mut joined: HashMap<usize, PathBuf> = HashMap::new();
        for (index, stream) in video.iter().chain(audio.iter()) {
            if stream.runs.len() > 1 {
                notes.push(format!(
                    "browser: the {} stream changed codec configuration {} time(s). Its parts were joined by decoding them.",
                    stream.mime,
                    stream.runs.len() - 1
                ));
            }
            if stream.duplicates > 0 {
                notes.push(format!(
                    "browser: {} segment(s) of the {} stream the player appended again were kept once",
                    stream.duplicates, stream.mime
                ));
            }
            if stream.stray > 0 {
                notes.push(format!(
                    "browser: {} segment(s) of the {} stream arrived before any initialization section and were left out",
                    stream.stray, stream.mime
                ));
            }
            let path = self
                .join(stream, &dest_dir.join(format!("browser-{index}.mkv")))
                .await?;
            joined.insert(*index, path);
        }
        let dest = dest_dir.join("source.mkv");
        let file = match (video, audio) {
            (Some((v, _)), Some((a, _))) => {
                mux_parts(
                    &self.ffmpeg,
                    &[joined[&v].clone()],
                    Some(&[joined[&a].clone()]),
                    &dest,
                    None,
                )
                .await?
            }
            (Some((only, _)), None) | (None, Some((only, _))) => {
                mux_parts(&self.ffmpeg, &[joined[&only].clone()], None, &dest, None).await?
            }
            (None, None) => return Err(DownloadError::Empty),
        };
        Ok(file)
    }
}

#[async_trait]
impl Downloader for BrowserDownloader {
    fn handles(&self, kind: VariantKind) -> bool {
        kind == VariantKind::Browser
    }

    async fn download(
        &self,
        variant: &Variant,
        dest_dir: &Path,
        context: &DownloadContext,
        progress: ProgressSender,
    ) -> Result<Downloaded, DownloadError> {
        let url = &variant.url;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(DownloadError::Unsupported(format!(
                "{url} is not a page a browser opens"
            )));
        }
        tokio::fs::create_dir_all(dest_dir).await?;
        let unavailable = |reason: String| DownloadError::BrowserUnavailable {
            url: url.to_string(),
            reason,
        };
        let executable =
            browser::locate(&context.browser).map_err(|e| unavailable(e.to_string()))?;
        let proxy = self
            .http
            .config()
            .proxies
            .for_request(Some(&context.platform), url.host_str().unwrap_or(""));
        let credentials = proxy.as_ref().and_then(|p| browser::proxy_flag(p).1);
        let profile = dest_dir.join("browser-profile");
        let mut browser =
            Browser::launch(&executable, &context.browser.args, &profile, proxy.as_ref())
                .await
                .map_err(|e| unavailable(e.to_string()))?;
        let product = browser.version().product.clone();
        let captured = self
            .capture(
                &mut browser,
                variant,
                dest_dir,
                context,
                &progress,
                credentials,
            )
            .await;
        browser.close().await;
        let (capture, _, state, mut notes) = captured?;
        let (file, streams) = match capture.finish().await? {
            Finished::Whole(streams) => {
                if streams.is_empty() {
                    return Err(process(format!(
                        "{url}: the page's player appended no media before playback ended at {}",
                        state.describe()
                    )));
                }
                let file = self.assemble(&streams, dest_dir, &mut notes).await?;
                for index in 0..streams.len() {
                    let _ =
                        tokio::fs::remove_file(dest_dir.join(format!("browser-{index}.mkv"))).await;
                }
                (file, streams)
            }
            Finished::Live {
                file,
                streams,
                notes: left_out,
            } => {
                notes.extend(left_out);
                (*file, streams)
            }
        };
        notes.insert(
            0,
            format!(
                "browser: {product} captured {} stream(s): {}",
                streams.len(),
                streams
                    .iter()
                    .map(|s| format!("{} ({} bytes)", s.mime, s.bytes))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
        for stream in &streams {
            for run in &stream.runs {
                let _ = tokio::fs::remove_file(run).await;
            }
        }
        Ok(Downloaded {
            file,
            subtitles: Vec::new(),
            notes,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use url::Url;

    use super::*;
    use crate::config::BrowserConfig;
    use crate::download::mp4::build::{full_box, plain_box};
    use crate::media::MediaKind;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("discoclip-browser-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An initialization section declaring one track with `codec` as its sample entry.
    fn init(codec: &[u8; 4]) -> Vec<u8> {
        let stsd = full_box(
            b"stsd",
            0,
            0,
            &[&1u32.to_be_bytes()[..], &plain_box(codec, &[0u8; 16])].concat(),
        );
        let stbl = plain_box(b"stbl", &stsd);
        let minf = plain_box(b"minf", &stbl);
        let mdia = plain_box(b"mdia", &minf);
        let trak = plain_box(b"trak", &mdia);
        let moov = plain_box(b"moov", &trak);
        [plain_box(b"ftyp", b"iso5"), moov].concat()
    }

    /// A media segment of track 1 starting at decode time `time`, carrying `payload`.
    fn fragment(time: u64, payload: &[u8]) -> Vec<u8> {
        let tfhd = full_box(b"tfhd", 0, 0x20000, &1u32.to_be_bytes());
        let tfdt = full_box(b"tfdt", 1, 0, &time.to_be_bytes());
        let traf = plain_box(b"traf", &[tfhd, tfdt].concat());
        let moof = plain_box(b"moof", &traf);
        [moof, plain_box(b"mdat", payload)].concat()
    }

    fn ebml(id: u32, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let id_len = 4 - id.leading_zeros() as usize / 8;
        out.extend_from_slice(&id.to_be_bytes()[4 - id_len..]);
        // Sizes written in eight bytes, marker included.
        out.push(0x01);
        out.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
        out.extend_from_slice(body);
        out
    }

    fn ebml_unknown(id: u32, body: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let id_len = 4 - id.leading_zeros() as usize / 8;
        out.extend_from_slice(&id.to_be_bytes()[4 - id_len..]);
        out.push(0xFF);
        out.extend_from_slice(body);
        out
    }

    fn cluster(timecode: u8, payload: &[u8]) -> Vec<u8> {
        ebml(
            CLUSTER,
            &[ebml(TIMECODE, &[timecode]), ebml(0xA3, payload)].concat(),
        )
    }

    fn webm_init(tracks: &[u8]) -> Vec<u8> {
        [
            ebml(EBML, &ebml(0x4282, b"webm")),
            ebml_unknown(SEGMENT, &[]),
            ebml(INFO, &ebml(0x2AD7B1, &[0x0F, 0x42, 0x40])),
            ebml(TRACKS, tracks),
        ]
        .concat()
    }

    async fn feed(capture: &mut Capture, key: &BufferKey, bytes: &[u8], piece: usize) {
        for chunk in bytes.chunks(piece) {
            capture.data(key, chunk).await.unwrap();
        }
    }

    fn key(n: u64) -> BufferKey {
        ("s".into(), 1, n)
    }

    #[tokio::test]
    async fn mp4_streams_are_split_deduplicated_and_ordered_whatever_the_appends() {
        let dir = temp_dir("mp4");
        let mut capture = Capture::new(&dir, 10_000_000);
        capture
            .open(key(1), "video/mp4; codecs=\"avc1.64001e\"")
            .unwrap();
        let mut stream = init(b"avc1");
        stream.extend(fragment(0, b"aaaa"));
        stream.extend(fragment(1000, b"bbbb"));
        stream.extend(plain_box(b"sidx", &[0u8; 8]));
        stream.extend(fragment(1000, b"bbbb"));
        stream.extend(fragment(3000, b"dddd"));
        stream.extend(fragment(2000, b"cccc"));
        // The same configuration announced again is not a new run.
        stream.extend(init(b"avc1"));
        stream.extend(fragment(4000, b"eeee"));
        feed(&mut capture, &key(1), &stream, 7).await;
        assert_eq!(capture.appended(), stream.len() as u64);
        let Finished::Whole(streams) = capture.finish().await.unwrap() else {
            panic!("a capture kept whole");
        };
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].runs.len(), 1);
        assert_eq!(streams[0].duplicates, 1);
        assert!(streams[0].video);
        let bytes = tokio::fs::read(&streams[0].runs[0]).await.unwrap();
        let expected = [
            init(b"avc1"),
            fragment(0, b"aaaa"),
            fragment(1000, b"bbbb"),
            fragment(2000, b"cccc"),
            fragment(3000, b"dddd"),
            fragment(4000, b"eeee"),
        ]
        .concat();
        assert_eq!(bytes, expected);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn a_new_codec_configuration_starts_a_run_and_aborts_drop_partial_segments() {
        let dir = temp_dir("runs");
        let mut capture = Capture::new(&dir, 10_000_000);
        capture.open(key(1), "video/mp4").unwrap();
        let first = [init(b"avc1"), fragment(0, b"aaaa")].concat();
        feed(&mut capture, &key(1), &first, 5).await;
        // Half a segment, then the player aborts: the half is thrown away.
        let half = fragment(1000, b"bbbb");
        capture
            .data(&key(1), &half[..half.len() / 2])
            .await
            .unwrap();
        capture.abort(&key(1)).await.unwrap();
        let second = [
            init(b"hvc1"),
            fragment(1000, b"cccc"),
            fragment(2000, b"dddd"),
        ]
        .concat();
        feed(&mut capture, &key(1), &second, 3).await;
        // A media segment before any initialization section is counted and left out.
        capture.open(key(2), "audio/mp4").unwrap();
        feed(&mut capture, &key(2), &fragment(0, b"zzzz"), 100).await;
        let Finished::Whole(streams) = capture.finish().await.unwrap() else {
            panic!("a capture kept whole");
        };
        assert_eq!(streams.len(), 1, "a buffer with no run is not a stream");
        assert_eq!(streams[0].runs.len(), 2);
        assert_eq!(
            tokio::fs::read(&streams[0].runs[0]).await.unwrap(),
            [init(b"avc1"), fragment(0, b"aaaa")].concat()
        );
        assert_eq!(
            tokio::fs::read(&streams[0].runs[1]).await.unwrap(),
            [
                init(b"hvc1"),
                fragment(1000, b"cccc"),
                fragment(2000, b"dddd")
            ]
            .concat()
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn webm_streams_are_split_at_clusters_of_known_and_unknown_size() {
        let dir = temp_dir("webm");
        let mut capture = Capture::new(&dir, 10_000_000);
        capture.open(key(1), "video/webm; codecs=\"vp9\"").unwrap();
        let tracks = ebml(0xAE, &ebml(0x86, b"V_VP9"));
        let mut stream = webm_init(&tracks);
        stream.extend(cluster(0, b"aaaa"));
        stream.extend(ebml(CUES, &[0u8; 4]));
        // A cluster of unknown size ends where the next one starts.
        stream.extend(ebml_unknown(
            CLUSTER,
            &[ebml(TIMECODE, &[1]), ebml(0xA3, b"bbbb")].concat(),
        ));
        stream.extend(cluster(1, b"bbbb"));
        stream.extend(ebml_unknown(
            CLUSTER,
            &[ebml(TIMECODE, &[2]), ebml(0xA3, b"cccc")].concat(),
        ));
        feed(&mut capture, &key(1), &stream, 4).await;
        let Finished::Whole(streams) = capture.finish().await.unwrap() else {
            panic!("a capture kept whole");
        };
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].runs.len(), 1);
        assert_eq!(streams[0].duplicates, 1);
        let bytes = tokio::fs::read(&streams[0].runs[0]).await.unwrap();
        let expected = [
            webm_init(&tracks),
            cluster(0, b"aaaa"),
            ebml_unknown(
                CLUSTER,
                &[ebml(TIMECODE, &[1]), ebml(0xA3, b"bbbb")].concat(),
            ),
            ebml_unknown(
                CLUSTER,
                &[ebml(TIMECODE, &[2]), ebml(0xA3, b"cccc")].concat(),
            ),
        ]
        .concat();
        assert_eq!(bytes, expected);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn aborts_keep_the_complete_blocks_of_an_open_cluster() {
        let dir = temp_dir("webm-abort");
        let mut capture = Capture::new(&dir, 10_000_000);
        capture.open(key(1), "video/webm").unwrap();
        let tracks = ebml(0xAE, &ebml(0x86, b"V_VP9"));
        let mut stream = webm_init(&tracks);
        // A cluster of unknown size with two blocks, a third cut short by the abort.
        let open = ebml_unknown(
            CLUSTER,
            &[
                ebml(TIMECODE, &[0]),
                ebml(0xA3, b"aaaa"),
                ebml(0xA3, b"bbbb"),
            ]
            .concat(),
        );
        stream.extend(&open);
        let cut = ebml(0xA3, b"cccc");
        stream.extend(&cut[..cut.len() - 2]);
        feed(&mut capture, &key(1), &stream, 5).await;
        capture.abort(&key(1)).await.unwrap();
        // A cluster without its timecode yet has no block the player could have timed.
        let mut empty = ebml_unknown(CLUSTER, &[]);
        empty.push(0xE7);
        feed(&mut capture, &key(1), &empty, 100).await;
        capture.abort(&key(1)).await.unwrap();
        // A head without its tracks is no initialization section.
        feed(&mut capture, &key(1), &webm_init(&tracks)[..20], 100).await;
        capture.abort(&key(1)).await.unwrap();
        // A cluster of known size cut short at the end of the stream keeps its complete
        // blocks under the size they make.
        let known = ebml(
            CLUSTER,
            &[
                ebml(TIMECODE, &[1]),
                ebml(0xA3, b"dddd"),
                ebml(0xA3, b"eeee"),
            ]
            .concat(),
        );
        feed(&mut capture, &key(1), &known[..known.len() - 2], 7).await;
        let Finished::Whole(streams) = capture.finish().await.unwrap() else {
            panic!("a capture kept whole");
        };
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].runs.len(), 1);
        assert_eq!(streams[0].duplicates, 0);
        assert_eq!(streams[0].stray, 0);
        let bytes = tokio::fs::read(&streams[0].runs[0]).await.unwrap();
        let expected = [
            webm_init(&tracks),
            open,
            ebml(
                CLUSTER,
                &[ebml(TIMECODE, &[1]), ebml(0xA3, b"dddd")].concat(),
            ),
        ]
        .concat();
        assert_eq!(bytes, expected);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn unreadable_streams_and_formats_are_refused() {
        let mut capture = Capture::new(Path::new("."), 1000);
        let refused = capture.open(key(1), "video/mp2t").unwrap_err();
        assert!(
            matches!(refused, DownloadError::Unsupported(_)),
            "{refused}"
        );
        assert_eq!(
            Format::of("audio/mp4; codecs=\"mp4a.40.2\""),
            Some(Format::Mp4)
        );
        assert_eq!(Format::of("video/webm;codecs=vp9"), Some(Format::WebM));
        let mut splitter = Mp4Splitter::default();
        let mut pending = vec![0, 0, 0, 0, b'm', b'd', b'a', b't'];
        let mut out = Vec::new();
        let refused = splitter.push(&mut pending, &mut out).unwrap_err();
        assert!(matches!(refused, DownloadError::Segment(_)), "{refused}");
        let mut splitter = WebmSplitter::default();
        let mut pending = ebml(0x4286, &[1]);
        let refused = splitter.push(&mut pending, &mut out, false).unwrap_err();
        assert!(
            refused.to_string().contains("unexpected element"),
            "{refused}"
        );
    }

    #[test]
    fn playback_is_summed_up_over_the_media_source_elements() {
        assert_eq!(playback_of(&json!([{"mse": false, "cur": 3.0}])), None);
        let state = playback_of(&json!([
            {"mse": false, "cur": 99.0, "dur": 99.0, "ended": true, "paused": false, "buf": []},
            {"mse": true, "cur": 3.5, "dur": 10.0, "ended": false, "paused": false, "ms": "open", "buf": [[0.0, 6.0]]},
            {"mse": true, "cur": 1.0, "dur": null, "ended": false, "paused": true, "ms": "ended", "buf": [[0.0, 2.0], [4.0, 8.0]]},
        ]))
        .unwrap();
        assert_eq!(
            state,
            Playback {
                current: 3.5,
                duration: Some(10.0),
                buffered_end: 8.0,
                ended: false,
                paused: false,
                source_ended: true,
            }
        );
        assert!(!state.complete());
        let near = Playback {
            current: 9.6,
            ..state
        };
        assert!(near.complete());
        let buffered_to_the_end = Playback {
            buffered_end: 9.8,
            ..state
        };
        assert!(buffered_to_the_end.complete());
        let live = Playback {
            duration: None,
            ended: false,
            ..state
        };
        assert!(!live.complete());
        assert_eq!(live.describe(), "3.5 s");
    }

    /// A host serving `files` by path, as a page's player would fetch them.
    async fn serve(files: HashMap<&'static str, (&'static str, Vec<u8>)>) -> Url {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let files = Arc::new(files);
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    break;
                };
                let files = files.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut byte = [0u8; 1];
                    while !request.ends_with(b"\r\n\r\n")
                        && socket.read(&mut byte).await.unwrap_or(0) == 1
                    {
                        request.push(byte[0]);
                    }
                    let text = String::from_utf8_lossy(&request);
                    let path = text
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/")
                        .to_string();
                    let response = match files.get(path.as_str()) {
                        Some((content_type, body)) => {
                            let mut response = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            )
                            .into_bytes();
                            response.extend_from_slice(body);
                            response
                        }
                        None => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                    };
                    let _ = socket.write_all(&response).await;
                    let _ = socket.shutdown().await;
                });
            }
        });
        Url::parse(&format!("http://127.0.0.1:{port}/")).unwrap()
    }

    async fn encode(ffmpeg: &Ffmpeg, dest: &Path, args: &[&str]) {
        let mut all: Vec<OsString> = vec![
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            "testsrc=size=320x240:rate=30:duration=4".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            "sine=frequency=440:sample_rate=48000:duration=4".into(),
        ];
        all.extend(args.iter().map(OsString::from));
        all.push(dest.as_os_str().to_owned());
        ffmpeg.run(all, |_| {}).await.unwrap();
    }

    /// A page whose player fetches `sources` whole and appends each to a source buffer
    /// of its own in slices, then ends the stream: media reachable through nothing but
    /// the media source.
    fn player_page(sources: &[(&str, &str)]) -> Vec<u8> {
        let feeds: Vec<String> = sources
            .iter()
            .map(|(path, mime)| format!("feed({path:?}, {mime:?})"))
            .collect();
        format!(
            r#"<!doctype html><html><head><title>MSE player</title></head><body>
<video id="v" playsinline></video>
<script>
const video = document.getElementById('v');
const source = new MediaSource();
video.src = URL.createObjectURL(source);
const feed = async (path, mime) => {{
  const buffer = source.addSourceBuffer(mime);
  const data = new Uint8Array(await (await fetch(path)).arrayBuffer());
  for (let offset = 0; offset < data.length; offset += 40000) {{
    buffer.appendBuffer(data.subarray(offset, Math.min(offset + 40000, data.length)));
    await new Promise((resolve) => buffer.addEventListener('updateend', resolve, {{ once: true }}));
  }}
}};
source.addEventListener('sourceopen', async () => {{
  await Promise.all([{feeds}]);
  source.endOfStream();
}});
</script></body></html>"#,
            feeds = feeds.join(", ")
        )
        .into_bytes()
    }

    #[tokio::test]
    async fn media_source_players_are_captured_through_the_browser() {
        let dir = temp_dir("capture");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let muxed = dir.join("muxed.mp4");
        encode(
            &ffmpeg,
            &muxed,
            &[
                "-c:v",
                "libvpx-vp9",
                "-b:v",
                "200k",
                "-deadline",
                "realtime",
                "-cpu-used",
                "8",
                "-c:a",
                "libopus",
                "-b:a",
                "48k",
                "-movflags",
                "frag_keyframe+empty_moov+default_base_moof",
                "-frag_duration",
                "1000000",
                "-f",
                "mp4",
            ],
        )
        .await;
        let webm_video = dir.join("video.webm");
        encode(
            &ffmpeg,
            &webm_video,
            &[
                "-map",
                "0:v",
                "-c:v",
                "libvpx-vp9",
                "-b:v",
                "200k",
                "-deadline",
                "realtime",
                "-cpu-used",
                "8",
                "-cluster_time_limit",
                "1000",
                "-f",
                "webm",
            ],
        )
        .await;
        let webm_audio = dir.join("audio.webm");
        encode(
            &ffmpeg,
            &webm_audio,
            &[
                "-map", "1:a", "-c:a", "libopus", "-b:a", "48k", "-f", "webm",
            ],
        )
        .await;
        let mut files = HashMap::new();
        files.insert(
            "/muxed.html",
            (
                "text/html",
                player_page(&[("/muxed.mp4", "video/mp4; codecs=\"vp09.00.10.08, opus\"")]),
            ),
        );
        files.insert(
            "/muxed.mp4",
            ("video/mp4", tokio::fs::read(&muxed).await.unwrap()),
        );
        files.insert(
            "/split.html",
            (
                "text/html",
                player_page(&[
                    ("/video.webm", "video/webm; codecs=\"vp9\""),
                    ("/audio.webm", "audio/webm; codecs=\"opus\""),
                ]),
            ),
        );
        files.insert(
            "/video.webm",
            ("video/webm", tokio::fs::read(&webm_video).await.unwrap()),
        );
        files.insert(
            "/audio.webm",
            ("audio/webm", tokio::fs::read(&webm_audio).await.unwrap()),
        );
        files.insert(
            "/plain.html",
            (
                "text/html",
                b"<html><body><p>No player here.</p></body></html>".to_vec(),
            ),
        );
        let base = serve(files).await;
        let http = Http::new(Http::test_config());
        let downloader = BrowserDownloader::new(http, ffmpeg.clone())
            .load_timeout(Duration::from_secs(20))
            .quiet_after(Duration::from_secs(20));
        let context = DownloadContext::new(50_000_000);

        for (page, kind) in [
            ("muxed.html", "one muxed mp4 buffer"),
            ("split.html", "webm video and audio buffers"),
        ] {
            let variant = Variant::new(base.join(page).unwrap(), VariantKind::Browser);
            let job = dir.join(page.trim_end_matches(".html"));
            let (progress, watched) = tokio::sync::watch::channel(Progress::default());
            let downloaded = downloader
                .download(&variant, &job, &context, progress)
                .await
                .unwrap_or_else(|e| panic!("{kind}: {e}"));
            let info = ffmpeg.probe(&downloaded.file.path).await.unwrap();
            assert_eq!(info.kind, MediaKind::Video, "{kind}: {info:?}");
            assert!(info.video.is_some(), "{kind}: {info:?}");
            assert!(info.audio.is_some(), "{kind}: {info:?}");
            let duration = info.duration.unwrap().as_secs_f64();
            assert!((3.5..=4.6).contains(&duration), "{kind}: {info:?}");
            assert_eq!(watched.borrow().total, Some(4), "{kind}");
            assert!(
                downloaded.notes[0].contains("captured") && downloaded.notes[0].contains("Chrom"),
                "{kind}: {:?}",
                downloaded.notes
            );
            assert!(
                !job.join("browser-profile").exists(),
                "{kind}: the profile stays"
            );
            assert!(
                !job.join("browser-0-0.mp4").exists() && !job.join("browser-0-0.webm").exists()
            );
        }

        let plain = Variant::new(base.join("plain.html").unwrap(), VariantKind::Browser);
        let quick = BrowserDownloader::new(Http::new(Http::test_config()), ffmpeg.clone())
            .load_timeout(Duration::from_secs(3));
        let (progress, _) = tokio::sync::watch::channel(Progress::default());
        let refused = quick
            .download(&plain, &dir.join("plain"), &context, progress)
            .await
            .unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("opened no media source within 3s"),
            "{refused}"
        );

        let missing = Variant::new(base.join("missing.html").unwrap(), VariantKind::Browser);
        let (progress, _) = tokio::sync::watch::channel(Progress::default());
        let refused = quick
            .download(&missing, &dir.join("missing"), &context, progress)
            .await
            .unwrap_err();
        assert!(
            matches!(refused, DownloadError::Status { status: 404, .. }),
            "{refused}"
        );

        let mut without = DownloadContext::new(50_000_000);
        without.browser = BrowserConfig {
            executable: Some(dir.join("no-such-browser")),
            args: vec![],
        };
        let (progress, _) = tokio::sync::watch::channel(Progress::default());
        let refused = quick
            .download(&plain, &dir.join("without"), &without, progress)
            .await
            .unwrap_err();
        assert!(
            matches!(&refused, DownloadError::BrowserUnavailable { reason, .. } if reason.contains("no-such-browser")),
            "{refused}"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn runs_of_different_configurations_are_joined_by_decoding_at_the_largest_picture() {
        let dir = temp_dir("join");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let small = dir.join("small.mp4");
        encode(
            &ffmpeg,
            &small,
            &[
                "-map",
                "0:v",
                "-c:v",
                "libvpx-vp9",
                "-b:v",
                "200k",
                "-deadline",
                "realtime",
                "-cpu-used",
                "8",
                "-movflags",
                "frag_keyframe+empty_moov+default_base_moof",
                "-frag_duration",
                "1000000",
                "-f",
                "mp4",
            ],
        )
        .await;
        let large = dir.join("large.mp4");
        encode(
            &ffmpeg,
            &large,
            &[
                "-map",
                "0:v",
                "-vf",
                "scale=640:480",
                "-c:v",
                "libvpx-vp9",
                "-b:v",
                "400k",
                "-deadline",
                "realtime",
                "-cpu-used",
                "8",
                "-movflags",
                "frag_keyframe+delay_moov+default_base_moof+frag_discont",
                "-frag_duration",
                "1000000",
                "-output_ts_offset",
                "4",
                "-f",
                "mp4",
            ],
        )
        .await;
        let job = dir.join("job");
        tokio::fs::create_dir_all(&job).await.unwrap();
        let mut capture = Capture::new(&job, 50_000_000);
        capture
            .open(key(1), "video/mp4; codecs=\"vp09.00.10.08\"")
            .unwrap();
        feed(
            &mut capture,
            &key(1),
            &tokio::fs::read(&small).await.unwrap(),
            30_001,
        )
        .await;
        feed(
            &mut capture,
            &key(1),
            &tokio::fs::read(&large).await.unwrap(),
            30_001,
        )
        .await;
        let Finished::Whole(streams) = capture.finish().await.unwrap() else {
            panic!("a capture kept whole");
        };
        assert_eq!(streams.len(), 1);
        assert_eq!(
            streams[0].runs.len(),
            2,
            "two codec configurations, two runs"
        );
        let downloader = BrowserDownloader::new(Http::new(Http::test_config()), ffmpeg.clone());
        let mut notes = Vec::new();
        let file = downloader
            .assemble(&streams, &job, &mut notes)
            .await
            .unwrap();
        let info = ffmpeg.probe(&file.path).await.unwrap();
        let video = info.video.clone().unwrap();
        assert_eq!((video.width, video.height), (640, 480), "{info:?}");
        let duration = info.duration.unwrap().as_secs_f64();
        assert!((7.5..=8.6).contains(&duration), "{info:?}");
        assert!(
            notes
                .iter()
                .any(|n| n.contains("changed codec configuration 1 time")),
            "{notes:?}"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
