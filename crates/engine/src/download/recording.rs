//! One growing fragmented MP4 for a live capture. An ffmpeg remux reads the streams over
//! loopback TCP as their segments arrive and writes fragments as it goes, so the
//! recording plays while it is still being made and stands complete the moment the
//! capture ends: every fragment written is a fragment kept.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

use super::{DownloadError, mpegts};
use crate::ffmpeg::{Ffmpeg, FfmpegError, Output};
use crate::media::{AudioCodec, LocalFile, VideoCodec};

/// The file every live capture is written to, in the job directory.
pub const RECORDING: &str = "recording.mp4";

/// How long ffmpeg gets to write what it holds once its inputs close.
const FINISH: Duration = Duration::from_secs(30);
/// How long ffmpeg gets to connect to an input once it is due.
const CONNECT: Duration = Duration::from_secs(60);

/// How the recorder treats a stream: written as it is, or decoded and encoded again
/// because MP4 cannot carry it, or because what arrives changes shape along the way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Treatment {
    Copy,
    Encode,
}

/// Whether MP4 carries a video codec as it is.
pub fn video_treatment(codec: Option<&VideoCodec>) -> Treatment {
    match codec {
        Some(VideoCodec::H264 | VideoCodec::H265 | VideoCodec::Vp9 | VideoCodec::Av1) => {
            Treatment::Copy
        }
        Some(VideoCodec::Vp8) => Treatment::Encode,
        Some(VideoCodec::Other(name)) => video_treatment_of(name),
        None => Treatment::Copy,
    }
}

/// Whether MP4 carries an audio codec as it is.
pub fn audio_treatment(codec: Option<&AudioCodec>) -> Treatment {
    match codec {
        Some(AudioCodec::Aac | AudioCodec::Opus | AudioCodec::Mp3 | AudioCodec::Flac) => {
            Treatment::Copy
        }
        Some(AudioCodec::Vorbis) => Treatment::Encode,
        Some(AudioCodec::Other(name)) => audio_treatment_of(name),
        None => Treatment::Copy,
    }
}

const VIDEO_ESSENCES: &[&str] = &[
    "h264",
    "avc1",
    "avc3",
    "hevc",
    "h265",
    "hev1",
    "hvc1",
    "vp9",
    "vp09",
    "av1",
    "av01",
    "mpeg4",
    "mp4v",
    "mjpeg",
    "vp8",
    "vp08",
    "theora",
    "dvh1",
    "dvhe",
    "mpeg2video",
    "mpeg1video",
    "flv1",
    "h263",
    "vc1",
    "wmv3",
    "wvc1",
];

const AUDIO_ESSENCES: &[&str] = &[
    "aac",
    "mp4a",
    "opus",
    "mp3",
    "flac",
    "ac3",
    "ac-3",
    "eac3",
    "ec-3",
    "alac",
    "vorbis",
    "speex",
    "nellymoser",
    "wmav1",
    "wmav2",
    "wmapro",
    "wmalossless",
    "truehd",
    "dts",
    "amr_nb",
    "amr_wb",
    "g722",
    "g726",
    "g729",
];

/// [`video_treatment`] by the name ffprobe, an RTP map or a MIME `codecs` parameter
/// gives the codec.
pub fn video_treatment_of(name: &str) -> Treatment {
    let name = name.trim().to_ascii_lowercase();
    let essence = name.split('.').next().unwrap_or("");
    match essence {
        "h264" | "avc1" | "avc3" | "hevc" | "h265" | "hev1" | "hvc1" | "vp9" | "vp09" | "av1"
        | "av01" | "mpeg4" | "mp4v" | "mjpeg" | "mpeg2video" | "mpeg1video" | "dvh1" | "dvhe" => {
            Treatment::Copy
        }
        _ => Treatment::Encode,
    }
}

/// [`audio_treatment`] by the name ffprobe, an RTP map or a MIME `codecs` parameter
/// gives the codec.
pub fn audio_treatment_of(name: &str) -> Treatment {
    let name = name.trim().to_ascii_lowercase();
    let essence = name.split('.').next().unwrap_or("");
    match essence {
        "aac" | "mp4a" | "opus" | "mp3" | "flac" | "ac3" | "ac-3" | "eac3" | "ec-3" | "alac" => {
            Treatment::Copy
        }
        _ => Treatment::Encode,
    }
}

/// The treatments a `CODECS` attribute or a MIME `codecs` parameter calls for: the
/// video entry's and the audio entry's, each copied when the list has none, or names a
/// codec the list does not place.
pub fn treatments_of(codecs: Option<&str>) -> (Treatment, Treatment) {
    let mut video = Treatment::Copy;
    let mut audio = Treatment::Copy;
    for entry in codecs.unwrap_or("").split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            continue;
        }
        let essence = entry.split('.').next().unwrap_or("").to_ascii_lowercase();
        if VIDEO_ESSENCES.contains(&essence.as_str()) {
            video = video_treatment_of(entry);
        } else if AUDIO_ESSENCES.contains(&essence.as_str()) {
            audio = audio_treatment_of(entry);
        }
    }
    (video, audio)
}

/// Whether sound arrives as ADTS AAC, which MP4 takes only once each frame is stripped
/// of its ADTS header: in a transport stream whose program says so, or as a bare ADTS
/// stream, with or without an ID3 tag ahead of it.
pub fn adts_audio(bytes: &[u8]) -> bool {
    if mpegts::is_transport_stream(bytes) {
        return mpegts::carries_adts_aac(bytes);
    }
    let body = if bytes.starts_with(b"ID3") && bytes.len() >= 10 {
        let size = bytes[6..10]
            .iter()
            .fold(0usize, |size, byte| (size << 7) | usize::from(byte & 0x7f));
        bytes.get(10 + size..).unwrap_or(&[])
    } else {
        bytes
    };
    body.len() > 1 && body[0] == 0xff && body[1] & 0xf6 == 0xf0
}

/// The picture an encoded recording is made at: every frame scaled into it, so a stream
/// that changes shape along the way stays one stream.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

/// The ffmpeg output arguments of a recording: the codecs by their treatment, then the
/// fragmented MP4 that plays while it grows. An encoded picture is scaled into
/// `picture` when one is given, at its frame rate, and the sound resampled to stay in
/// step with it across gaps. Copied `adts` sound has its ADTS headers stripped on the
/// way in, which a fragmented MP4 needs asked for.
pub fn output_args(
    video: Option<Treatment>,
    audio: Option<Treatment>,
    picture: Option<&Picture>,
    adts: bool,
    language: Option<&str>,
    dest: &Path,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    match video {
        Some(Treatment::Copy) => args.extend(["-c:v".into(), "copy".into()]),
        Some(Treatment::Encode) => {
            if let Some(picture) = picture {
                let (w, h) = (picture.width.max(2) & !1, picture.height.max(2) & !1);
                args.extend([
                    "-vf".into(),
                    format!(
                        "scale={w}:{h}:force_original_aspect_ratio=decrease:flags=bicubic,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,format=yuv420p"
                    )
                    .into(),
                    "-r".into(),
                    format!("{:.3}", picture.fps.clamp(1.0, 240.0)).into(),
                    "-fps_mode".into(),
                    "cfr".into(),
                ]);
            }
            args.extend(
                [
                    "-c:v", "libx264", "-preset", "veryfast", "-crf", "23", "-pix_fmt", "yuv420p",
                    "-g", "60",
                ]
                .map(OsString::from),
            );
        }
        None => args.push("-vn".into()),
    }
    match audio {
        Some(Treatment::Copy) => {
            args.extend(["-c:a".into(), "copy".into()]);
            if adts {
                args.extend(["-bsf:a".into(), "aac_adtstoasc".into()]);
            }
        }
        Some(Treatment::Encode) => {
            if picture.is_some() {
                args.extend(["-af".into(), "aresample=async=1".into()]);
            }
            args.extend(["-c:a", "aac", "-b:a", "160k"].map(OsString::from));
        }
        None => args.push("-an".into()),
    }
    if audio.is_some() {
        args.extend(super::segments::language_args(language));
    }
    args.extend(
        [
            "-sn",
            "-dn",
            "-strict",
            "experimental",
            "-f",
            "mp4",
            "-movflags",
            "frag_keyframe+empty_moov+default_base_moof",
        ]
        .map(OsString::from),
    );
    args.push(dest.as_os_str().to_owned());
    args
}

/// Which of the recorder's inputs a feed writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// The video stream, or the one stream that carries both.
    Video,
    /// A separate audio stream.
    Audio,
}

/// What the recorder is started with: the streams it takes and how, the format they
/// arrive in when it should not be probed for, and the picture an encoded recording is
/// made at.
#[derive(Debug, Clone)]
pub struct Spec {
    pub video: Treatment,
    pub audio: Option<Treatment>,
    /// The `-f` of every input. `None` lets ffmpeg probe the bytes.
    pub format: Option<&'static str>,
    pub picture: Option<Picture>,
    /// The sound arrives as ADTS AAC, to be stripped of its headers when copied.
    pub adts: bool,
    /// The language of the sound, when the platform said what it is.
    pub language: Option<String>,
}

impl Spec {
    /// Streams copied as they are, probed for their format.
    pub fn copied(video: Treatment, audio: Option<Treatment>) -> Self {
        Self {
            video,
            audio,
            format: None,
            picture: None,
            adts: false,
            language: None,
        }
    }
}

/// One stream on its way into the recorder: the socket ffmpeg reads it from, connected
/// once ffmpeg opens the input.
struct Input {
    listener: Option<TcpListener>,
    stream: Option<TcpStream>,
    closed: bool,
}

impl Input {
    async fn listen() -> Result<(Self, OsString), DownloadError> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let port = listener.local_addr()?.port();
        Ok((
            Self {
                listener: Some(listener),
                stream: None,
                closed: false,
            },
            format!("tcp://127.0.0.1:{port}").into(),
        ))
    }
}

/// How the ffmpeg behind a recorder ended, once it has.
type Outcome = Arc<std::sync::Mutex<Option<Result<Output, FfmpegError>>>>;

struct Inner {
    path: PathBuf,
    video: tokio::sync::Mutex<Input>,
    audio: Option<tokio::sync::Mutex<Input>>,
    outcome: Outcome,
}

impl Inner {
    fn lane(&self, lane: Lane) -> Option<&tokio::sync::Mutex<Input>> {
        match lane {
            Lane::Video => Some(&self.video),
            Lane::Audio => self.audio.as_ref(),
        }
    }

    /// Whether ffmpeg has stopped on its own.
    fn ended(&self) -> bool {
        self.outcome
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// What ffmpeg said when it stopped on its own.
    fn failure(&self) -> Option<String> {
        match self
            .outcome
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            Some(Err(error)) => Some(error.to_string()),
            Some(Ok(_)) => Some("ffmpeg ended before its input did".into()),
            None => None,
        }
    }

    async fn write(&self, lane: Lane, bytes: &[u8]) -> Result<(), DownloadError> {
        let Some(input) = self.lane(lane) else {
            return Err(DownloadError::Process(
                "the recorder was started without a separate audio input".into(),
            ));
        };
        let mut input = input.lock().await;
        if input.closed {
            return Err(DownloadError::Process(
                "the recorder is closed and takes no more input".into(),
            ));
        }
        if input.stream.is_none() {
            let Some(listener) = input.listener.as_ref() else {
                return Err(DownloadError::Process(
                    "the recorder's input is no longer listening".into(),
                ));
            };
            let started = tokio::time::Instant::now();
            let stream = loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (stream, _) = accepted?;
                        break stream;
                    }
                    _ = tokio::time::sleep(Duration::from_millis(250)) => {
                        if let Some(failure) = self.failure() {
                            return Err(DownloadError::Process(format!(
                                "the recorder stopped before its input was connected: {failure}"
                            )));
                        }
                        if started.elapsed() > CONNECT {
                            return Err(DownloadError::Process(format!(
                                "the recorder did not open its input within {} s",
                                CONNECT.as_secs()
                            )));
                        }
                    }
                }
            };
            input.stream = Some(stream);
        }
        let stream = input.stream.as_mut().expect("connected");
        match stream.write_all(bytes).await {
            Ok(()) => Ok(()),
            Err(error) => match self.failure() {
                Some(failure) => Err(DownloadError::Process(format!(
                    "the recorder stopped taking its input: {failure}"
                ))),
                None => Err(error.into()),
            },
        }
    }

    /// Closes `lane`, so ffmpeg sees the end of that stream.
    async fn close(&self, lane: Lane) {
        let Some(input) = self.lane(lane) else {
            return;
        };
        let mut input = input.lock().await;
        input.closed = true;
        if let Some(mut stream) = input.stream.take() {
            let _ = stream.shutdown().await;
        }
        input.listener = None;
    }
}

/// The ffmpeg remux and the sockets that feed it.
pub struct Recorder {
    inner: Arc<Inner>,
    task: Option<JoinHandle<()>>,
}

impl Recorder {
    /// Starts the remux writing `dir/recording.mp4`, reading the video from one socket
    /// and, when the spec names a separate audio stream, the audio from another. Bytes
    /// go in through the [`Feed`] of each lane.
    pub async fn start(ffmpeg: &Ffmpeg, dir: &Path, spec: &Spec) -> Result<Self, DownloadError> {
        let path = dir.join(RECORDING);
        let (video, video_url) = Input::listen().await?;
        let audio = match spec.audio {
            Some(_) => Some(Input::listen().await?),
            None => None,
        };
        let mut args: Vec<OsString> = vec!["-loglevel".into(), "warning".into()];
        // A stream that arrives as it plays is read for a second before the streams are
        // taken as known, so the recording begins with the first segments rather than
        // after ffmpeg's usual five.
        let input_args = |url: &OsString| -> Vec<OsString> {
            let mut args: Vec<OsString> = vec![
                "-fflags".into(),
                "+genpts+discardcorrupt".into(),
                "-analyzeduration".into(),
                "1000000".into(),
                "-probesize".into(),
                "2000000".into(),
            ];
            if let Some(format) = spec.format {
                args.extend(["-f".into(), format.into()]);
            }
            args.extend(["-i".into(), url.clone()]);
            args
        };
        args.extend(input_args(&video_url));
        match &audio {
            Some((_, audio_url)) => {
                args.extend(input_args(audio_url));
                args.extend(["-map", "0:v:0", "-map", "1:a:0"].map(OsString::from));
            }
            None => args.extend(["-map", "0:v:0?", "-map", "0:a:0?"].map(OsString::from)),
        }
        args.extend(output_args(
            Some(spec.video),
            Some(spec.audio.unwrap_or(spec.video)),
            spec.picture.as_ref(),
            spec.adts,
            spec.language.as_deref(),
            &path,
        ));
        let outcome: Outcome = Arc::new(std::sync::Mutex::new(None));
        let ffmpeg = ffmpeg.clone();
        let task = {
            let outcome = outcome.clone();
            tokio::spawn(async move {
                let result = ffmpeg.run(args, |_| {}).await;
                *outcome.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
            })
        };
        Ok(Self {
            inner: Arc::new(Inner {
                path,
                video: tokio::sync::Mutex::new(video),
                audio: audio.map(|(input, _)| tokio::sync::Mutex::new(input)),
                outcome,
            }),
            task: Some(task),
        })
    }

    /// Where the recording is written.
    pub fn path(&self) -> &Path {
        &self.inner.path
    }

    /// Bytes written to the recording so far.
    pub fn bytes(&self) -> u64 {
        std::fs::metadata(&self.inner.path)
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// The writer of one input, held apart from the other so two writers can feed them
    /// at the same time.
    pub fn feed(&self, lane: Lane) -> Feed {
        Feed {
            inner: self.inner.clone(),
            lane,
        }
    }

    /// Closes the inputs and waits for ffmpeg to write out what it holds. The recording
    /// as it stands.
    pub async fn finish(mut self) -> Result<LocalFile, DownloadError> {
        self.inner.close(Lane::Video).await;
        self.inner.close(Lane::Audio).await;
        if let Some(mut task) = self.task.take() {
            match tokio::time::timeout(FINISH, &mut task).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    return Err(DownloadError::Process(format!(
                        "the recorder task failed: {error}"
                    )));
                }
                Err(_) => {
                    task.abort();
                    tracing::warn!(
                        path = %self.inner.path.display(),
                        "the recorder did not finish within {} s and was stopped",
                        FINISH.as_secs()
                    );
                }
            }
        }
        let size = tokio::fs::metadata(&self.inner.path)
            .await
            .map(|m| m.len())
            .unwrap_or(0);
        if size == 0 {
            return Err(DownloadError::Empty);
        }
        if let Some(Err(error)) = self
            .inner
            .outcome
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            return Err(DownloadError::Process(error.to_string()));
        }
        LocalFile::from_path(self.inner.path.clone())
            .await
            .map_err(DownloadError::from)
    }
}

impl Drop for Recorder {
    /// A recorder dropped before it finished takes its ffmpeg down with it.
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// One input of a running recorder.
pub struct Feed {
    inner: Arc<Inner>,
    lane: Lane,
}

impl Feed {
    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), DownloadError> {
        self.inner.write(self.lane, bytes).await
    }

    /// Ends this input: ffmpeg sees the end of the stream.
    pub async fn close(&mut self) {
        self.inner.close(self.lane).await;
    }

    /// Whether ffmpeg has stopped on its own, as when it refused the streams.
    pub fn ended(&self) -> bool {
        self.inner.ended()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tokio::process::Command;

    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "discoclip-recording-{tag}-{}",
            uuid::Uuid::now_v7()
        ))
    }

    /// `seconds` of test picture and tone as one transport stream.
    async fn transport_stream(ffmpeg: &Ffmpeg, dir: &Path, seconds: u32) -> Vec<u8> {
        let path = dir.join("source.ts");
        let status = Command::new(ffmpeg.ffmpeg_path())
            .args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error"])
            .args(["-f", "lavfi", "-i"])
            .arg(format!("testsrc=size=64x64:rate=10:duration={seconds}"))
            .args(["-f", "lavfi", "-i"])
            .arg(format!("sine=frequency=440:duration={seconds}"))
            .args([
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
                "-g",
                "10",
                "-c:a",
                "aac",
                "-f",
                "mpegts",
            ])
            .arg(&path)
            .status()
            .await
            .unwrap();
        assert!(status.success());
        tokio::fs::read(&path).await.unwrap()
    }

    #[tokio::test]
    async fn a_transport_stream_fed_in_pieces_becomes_a_playable_growing_mp4() {
        let dir = temp_dir("ts");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let bytes = transport_stream(&ffmpeg, &dir, 4).await;
        let spec = Spec {
            adts: adts_audio(&bytes),
            ..Spec::copied(Treatment::Copy, None)
        };
        assert!(spec.adts, "a transport stream from ffmpeg carries ADTS AAC");
        let recorder = Recorder::start(&ffmpeg, &dir, &spec).await.unwrap();
        let mut feed = recorder.feed(Lane::Video);
        let half = bytes.len() / 2;
        feed.write(&bytes[..half]).await.unwrap();
        // What has been fed is on disk as a fragmented MP4 with its header first, before
        // the capture ends.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if recorder.bytes() > 0 {
                break;
            }
            assert!(tokio::time::Instant::now() < deadline, "nothing written");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let head = tokio::fs::read(recorder.path()).await.unwrap();
        assert_eq!(&head[4..8], b"ftyp", "{:?}", &head[..16]);
        feed.write(&bytes[half..]).await.unwrap();
        let file = recorder.finish().await.unwrap();
        assert_eq!(file.path.file_name().unwrap(), RECORDING);
        let info = ffmpeg.probe(&file.path).await.unwrap();
        assert!(info.video.is_some(), "{info:?}");
        assert!(info.audio.is_some(), "{info:?}");
        assert!(
            (info.duration.unwrap().as_secs_f64() - 4.0).abs() < 0.8,
            "{info:?}"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn separate_video_and_audio_inputs_are_muxed_together() {
        let dir = temp_dir("split");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let make = |name: &str, args: Vec<String>| {
            let path = dir.join(name);
            let ffmpeg = ffmpeg.clone();
            async move {
                let status = Command::new(ffmpeg.ffmpeg_path())
                    .args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error"])
                    .args(&args)
                    .arg(&path)
                    .status()
                    .await
                    .unwrap();
                assert!(status.success());
                tokio::fs::read(&path).await.unwrap()
            }
        };
        let video = make(
            "video.ts",
            [
                "-f",
                "lavfi",
                "-i",
                "testsrc=size=64x64:rate=10:duration=3",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
                "-g",
                "10",
                "-f",
                "mpegts",
            ]
            .map(String::from)
            .to_vec(),
        )
        .await;
        let audio = make(
            "audio.ts",
            [
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=3",
                "-c:a",
                "aac",
                "-f",
                "mpegts",
            ]
            .map(String::from)
            .to_vec(),
        )
        .await;
        let spec = Spec {
            adts: adts_audio(&audio),
            ..Spec::copied(Treatment::Copy, Some(Treatment::Copy))
        };
        let recorder = Recorder::start(&ffmpeg, &dir, &spec).await.unwrap();
        // The audio arrives first, as it may: its writer waits for ffmpeg to open the
        // second input, which happens once the video has been probed.
        let mut v = recorder.feed(Lane::Video);
        let mut a = recorder.feed(Lane::Audio);
        let audio_write = a.write(&audio);
        let video_write = async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            v.write(&video).await
        };
        let (audio_result, video_result) = tokio::join!(audio_write, video_write);
        audio_result.unwrap();
        video_result.unwrap();
        let file = recorder.finish().await.unwrap();
        let info = ffmpeg.probe(&file.path).await.unwrap();
        assert!(info.video.is_some(), "{info:?}");
        assert!(info.audio.is_some(), "{info:?}");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn a_refused_stream_reports_what_ffmpeg_said() {
        let dir = temp_dir("refused");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        // Sorenson video in Matroska: MP4 has no tag for it, so a copy is refused at
        // the header.
        let path = dir.join("sorenson.mkv");
        let status = Command::new(ffmpeg.ffmpeg_path())
            .args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error"])
            .args(["-f", "lavfi", "-i", "testsrc=size=64x64:rate=10:duration=2"])
            .args(["-c:v", "flv", "-an", "-f", "matroska"])
            .arg(&path)
            .status()
            .await
            .unwrap();
        assert!(status.success());
        let bytes = tokio::fs::read(&path).await.unwrap();
        let recorder = Recorder::start(&ffmpeg, &dir, &Spec::copied(Treatment::Copy, None))
            .await
            .unwrap();
        let mut feed = recorder.feed(Lane::Video);
        let mut failed = None;
        for chunk in bytes.chunks(4096) {
            if let Err(error) = feed.write(chunk).await {
                failed = Some(error.to_string());
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let message = match failed {
            Some(message) => message,
            None => recorder.finish().await.unwrap_err().to_string(),
        };
        assert!(
            message.contains("Could not find tag for codec"),
            "{message}"
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn treatments_follow_what_mp4_carries() {
        assert_eq!(video_treatment(Some(&VideoCodec::H264)), Treatment::Copy);
        assert_eq!(video_treatment(Some(&VideoCodec::Vp8)), Treatment::Encode);
        assert_eq!(video_treatment_of("avc1.64001f"), Treatment::Copy);
        assert_eq!(video_treatment_of("vp8"), Treatment::Encode);
        assert_eq!(audio_treatment(Some(&AudioCodec::Opus)), Treatment::Copy);
        assert_eq!(
            audio_treatment(Some(&AudioCodec::Vorbis)),
            Treatment::Encode
        );
        assert_eq!(audio_treatment_of("pcm_mulaw"), Treatment::Encode);
        assert_eq!(audio_treatment_of("mp4a.40.2"), Treatment::Copy);
        assert!(adts_audio(&[0xff, 0xf1, 0x50, 0x80, 0x01, 0x7f, 0xfc]));
        assert!(adts_audio(
            &[
                b"ID3\x04\x00\x00\x00\x00\x00\x02\x00\x00".as_slice(),
                &[0xff, 0xf9, 0x50]
            ]
            .concat()
        ));
        assert!(!adts_audio(&[
            0x00, 0x00, 0x00, 0x18, b'f', b't', b'y', b'p'
        ]));
        assert_eq!(
            treatments_of(Some("avc1.64001f,mp4a.40.2")),
            (Treatment::Copy, Treatment::Copy)
        );
        assert_eq!(
            treatments_of(Some("vp8, vorbis")),
            (Treatment::Encode, Treatment::Encode)
        );
        assert_eq!(
            treatments_of(Some("hvc1.1.6.L93.B0,ec-3")),
            (Treatment::Copy, Treatment::Copy)
        );
        assert_eq!(treatments_of(None), (Treatment::Copy, Treatment::Copy));
    }
}
