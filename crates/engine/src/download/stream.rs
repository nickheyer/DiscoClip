//! Record RTMP, RTSP and RTP streams through ffmpeg. A live stream becomes the growing
//! fragmented MP4 every capture is kept as; a stream with a known length becomes a
//! Matroska file taken whole. Apply capture and byte limits.
//!
//! Fetch SDP through the engine client so cookies, headers and proxies apply, then pass
//! the local file to ffmpeg.

use std::ffi::OsString;
use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;

use super::recording::{self, RECORDING, Treatment};
use super::segments::fetch_text;
use super::{DownloadContext, DownloadError, Downloaded, Downloader};
use crate::event::{Progress, ProgressSender};
use crate::ffmpeg::{Ending, Ffmpeg};
use crate::http::Http;
use crate::media::LocalFile;
use crate::resolve::{Variant, VariantKind};

const MAX_SDP: usize = 1024 * 1024;
/// How long a stream may go without a byte before it counts as gone.
const STALL: Duration = Duration::from_secs(20);

/// The codec ffmpeg refused to put in the container, from what it said on the way out:
/// whether it is an audio codec, and its name.
fn refused_codec(message: &str) -> Option<(bool, String)> {
    let start = message.find("Could not find tag for codec ")?;
    let rest = &message[start + "Could not find tag for codec ".len()..];
    let name: String = rest
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    if name.is_empty() {
        return None;
    }
    let audio = name.starts_with("pcm_")
        || name.starts_with("adpcm_")
        || name.starts_with("wmav")
        || name.starts_with("amr_")
        || name.starts_with("g72")
        || matches!(
            name.as_str(),
            "speex" | "nellymoser" | "vorbis" | "wmalossless" | "wmapro" | "truehd" | "dts"
        );
    Some((audio, name))
}

pub struct StreamDownloader {
    http: Http,
    ffmpeg: Ffmpeg,
    quiet_after: Duration,
}

impl StreamDownloader {
    pub fn new(http: Http, ffmpeg: Ffmpeg) -> Self {
        Self {
            http,
            ffmpeg,
            quiet_after: STALL,
        }
    }

    /// How long a stream may go without advancing before it counts as ended.
    pub fn quiet_after(mut self, quiet_after: Duration) -> Self {
        self.quiet_after = quiet_after;
        self
    }
}

/// The ffmpeg input options and source for a variant of `kind`.
async fn input_of(
    http: &Http,
    variant: &Variant,
    dest_dir: &Path,
    platform: &str,
) -> Result<Vec<OsString>, DownloadError> {
    let stall = STALL.as_micros().to_string();
    let mut args: Vec<OsString> = Vec::new();
    match variant.kind {
        VariantKind::Rtmp => {
            if !matches!(
                variant.url.scheme(),
                "rtmp" | "rtmpe" | "rtmps" | "rtmpt" | "rtmpte" | "rtmpts"
            ) {
                return Err(DownloadError::Unsupported(format!(
                    "{} is not an RTMP address",
                    variant.url
                )));
            }
            args.extend(["-rw_timeout".into(), stall.clone().into()]);
            // A referrer is the page the player sat on, which some servers check.
            if let Some((_, referer)) = variant
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("referer"))
            {
                args.extend(["-rtmp_pageurl".into(), referer.into()]);
            }
            args.extend(["-i".into(), variant.url.as_str().into()]);
        }
        VariantKind::Rtsp => {
            if !matches!(variant.url.scheme(), "rtsp" | "rtsps") {
                return Err(DownloadError::Unsupported(format!(
                    "{} is not an RTSP address",
                    variant.url
                )));
            }
            args.extend([
                "-rtsp_transport".into(),
                "tcp".into(),
                "-timeout".into(),
                stall.into(),
                "-i".into(),
                variant.url.as_str().into(),
            ]);
        }
        VariantKind::Rtp => {
            let source: OsString = match variant.url.scheme() {
                "http" | "https" => {
                    let (_, text) =
                        fetch_text(http, &variant.url, platform, &variant.headers, MAX_SDP).await?;
                    if !text.lines().any(|line| line.trim_start().starts_with("m=")) {
                        return Err(DownloadError::Manifest(format!(
                            "{} is not a session description: no media line",
                            variant.url
                        )));
                    }
                    let path = dest_dir.join("session.sdp");
                    tokio::fs::write(&path, text).await?;
                    path.into_os_string()
                }
                "rtp" | "udp" | "srt" => variant.url.as_str().into(),
                other => {
                    return Err(DownloadError::Unsupported(format!(
                        "{other}:// is not an RTP source"
                    )));
                }
            };
            // The session demuxer has no socket timeout of its own. A quiet session is
            // ended by the recorder's watch on its progress.
            args.extend([
                "-protocol_whitelist".into(),
                "file,udp,rtp,tcp,srt".into(),
                "-i".into(),
                source,
            ]);
        }
        other => {
            return Err(DownloadError::Unsupported(format!(
                "the stream recorder does not take {} variants",
                other.as_str()
            )));
        }
    }
    Ok(args)
}

#[async_trait]
impl Downloader for StreamDownloader {
    fn handles(&self, kind: VariantKind) -> bool {
        matches!(
            kind,
            VariantKind::Rtmp | VariantKind::Rtsp | VariantKind::Rtp
        )
    }

    async fn download(
        &self,
        variant: &Variant,
        dest_dir: &Path,
        context: &DownloadContext,
        progress: ProgressSender,
    ) -> Result<Downloaded, DownloadError> {
        tokio::fs::create_dir_all(dest_dir).await?;
        let input = input_of(&self.http, variant, dest_dir, &context.platform).await?;
        // A recording with a known length is taken whole. Anything else is a live capture,
        // ended at the limit, or when a person stops it.
        let live = variant.live || variant.duration.is_none();
        let total = if live {
            Some(context.max_live)
        } else {
            variant.duration
        };
        let dest = if live {
            dest_dir.join(RECORDING)
        } else {
            dest_dir.join("source.mkv")
        };
        let build = |video: Treatment, audio: Treatment| {
            let mut args: Vec<OsString> = vec!["-loglevel".into(), "warning".into()];
            args.extend(input.iter().cloned());
            if live {
                args.extend([
                    "-t".into(),
                    format!("{:.3}", context.max_live.as_secs_f64()).into(),
                ]);
            }
            args.extend([
                "-fs".into(),
                context.max_bytes.to_string().into(),
                "-map".into(),
                "0:v:0?".into(),
                "-map".into(),
                "0:a:0?".into(),
            ]);
            if live {
                args.extend(recording::output_args(
                    Some(video),
                    Some(audio),
                    None,
                    false,
                    None,
                    &dest,
                ));
            } else {
                args.extend([
                    "-c".into(),
                    "copy".into(),
                    "-sn".into(),
                    "-dn".into(),
                    "-f".into(),
                    "matroska".into(),
                    dest.as_os_str().to_owned(),
                ]);
            }
            args
        };
        progress.send_replace(Progress::of(0, total.map(|t| t.as_secs())));
        let report = |time: Duration| {
            progress.send_replace(Progress {
                done: time.as_secs(),
                total: total.map(|t| t.as_secs()),
                bytes: live.then(|| std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0)),
            });
        };
        if live {
            context.capture.started(&dest);
        }
        // The streams are written as they are. One MP4 cannot carry is found out from
        // ffmpeg refusing it, and that stream alone is encoded on the way in.
        let mut treatments = (Treatment::Copy, Treatment::Copy);
        let (output, ending) = loop {
            let stop = context.stop.clone();
            let attempt = self
                .ffmpeg
                .record(
                    build(treatments.0, treatments.1),
                    report,
                    self.quiet_after,
                    async move { stop.cancelled().await },
                )
                .await;
            match attempt {
                Ok(done) => break done,
                Err(error) if live => {
                    let Some((audio, codec)) = refused_codec(&error.to_string()) else {
                        return Err(DownloadError::Process(error.to_string()));
                    };
                    let already = if audio { treatments.1 } else { treatments.0 };
                    if already == Treatment::Encode {
                        return Err(DownloadError::Process(error.to_string()));
                    }
                    tracing::info!(%codec, audio, "MP4 does not carry the stream's codec. Encoding it on the way in.");
                    if audio {
                        treatments.1 = Treatment::Encode;
                    } else {
                        treatments.0 = Treatment::Encode;
                    }
                }
                Err(error) => return Err(DownloadError::Process(error.to_string())),
            }
        };
        let captured = output.last_time.unwrap_or(Duration::ZERO);
        let file = LocalFile::from_path(dest).await?;
        if file.size == 0 {
            return Err(DownloadError::Empty);
        }
        if !live && file.size > context.max_bytes {
            return Err(DownloadError::TooLarge {
                size: file.size,
                limit: context.max_bytes,
            });
        }
        let secs = captured.as_secs_f64();
        let mut notes = Vec::new();
        match ending {
            Ending::Stopped => notes.push(format!("Capture stopped at {secs:.0} s.")),
            Ending::Stalled => notes.push(format!(
                "Stream idle for {} s. Capture ended at {secs:.0} s.",
                self.quiet_after.as_secs()
            )),
            Ending::Finished if live => {
                let limit = context.max_live.as_secs_f64();
                if secs + 1.0 >= limit {
                    notes.push(format!("Capture ended at the {limit:.0} s limit."));
                } else if file.size >= context.max_bytes {
                    notes.push(format!(
                        "Capture ended at the {} byte limit.",
                        context.max_bytes
                    ));
                } else {
                    notes.push(format!("Stream ended at {secs:.0} s."));
                }
            }
            Ending::Finished => {}
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
    use std::path::PathBuf;
    use std::sync::Arc;

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, UdpSocket};
    use tokio::process::Command;
    use url::Url;

    use super::*;
    use crate::http::transport::site::Site;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("discoclip-stream-{tag}-{}", uuid::Uuid::now_v7()))
    }

    /// A port nothing listens on right now.
    async fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    /// Three seconds of test picture and tone, encoded once as ffmpeg input arguments.
    fn source_args(seconds: u32, audio: bool) -> Vec<String> {
        let mut args: Vec<String> = [
            "-loglevel",
            "error",
            "-re",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc=size=64x64:rate=10:duration={seconds}"),
        ]
        .map(String::from)
        .to_vec();
        if audio {
            args.extend(
                [
                    "-f",
                    "lavfi",
                    "-i",
                    &format!("sine=frequency=440:duration={seconds}"),
                ]
                .map(String::from),
            );
        }
        args.extend(
            [
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-pix_fmt",
                "yuv420p",
                "-g",
                "10",
            ]
            .map(String::from),
        );
        if audio {
            args.extend(["-c:a", "aac"].map(String::from));
        }
        args
    }

    /// ffmpeg running in the background with `args`, killed when dropped.
    fn spawn_ffmpeg(ffmpeg: &Ffmpeg, args: &[String]) -> tokio::process::Child {
        Command::new(ffmpeg.ffmpeg_path())
            .args(["-hide_banner", "-nostdin", "-y"])
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap()
    }

    async fn record(
        site: &Arc<Site>,
        ffmpeg: &Ffmpeg,
        url: &str,
        kind: VariantKind,
        dir: &Path,
        context: &DownloadContext,
    ) -> (Result<Downloaded, DownloadError>, Progress) {
        let http = Http::with_transport(site.clone(), Http::test_config());
        let downloader =
            StreamDownloader::new(http, ffmpeg.clone()).quiet_after(Duration::from_secs(3));
        let mut variant = Variant::new(Url::parse(url).unwrap(), kind);
        variant.live = true;
        let (progress, watched) = tokio::sync::watch::channel(Progress::default());
        let result = downloader.download(&variant, dir, context, progress).await;
        let last = *watched.borrow();
        (result, last)
    }

    fn near(actual: f64, expected: f64) -> bool {
        (actual - expected).abs() <= 0.8
    }

    #[tokio::test]
    async fn rtmp_streams_are_recorded_until_they_end() {
        let dir = temp_dir("rtmp");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let port = free_port().await;
        let url = format!("rtmp://127.0.0.1:{port}/live/app");
        let mut args = source_args(3, true);
        args.extend(["-f", "flv", "-listen", "1", &url].map(String::from));
        let _server = spawn_ffmpeg(&ffmpeg, &args);
        let site = Site::new();
        let mut context = DownloadContext::new(50_000_000);
        context.max_live = Duration::from_secs(60);
        // The server takes a moment to listen. A refused connection is tried again.
        let mut attempt = 0;
        let (downloaded, progress) = loop {
            attempt += 1;
            let (result, progress) = record(
                &site,
                &ffmpeg,
                &url,
                VariantKind::Rtmp,
                &dir.join(format!("job{attempt}")),
                &context,
            )
            .await;
            match result {
                Ok(downloaded) => break (downloaded, progress),
                Err(error) if attempt < 20 => {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                    let _ = error;
                }
                Err(error) => panic!("{error}"),
            }
        };
        let info = ffmpeg.probe(&downloaded.file.path).await.unwrap();
        assert!(info.video.is_some(), "{info:?}");
        assert!(info.audio.is_some(), "{info:?}");
        assert!(near(info.duration.unwrap().as_secs_f64(), 3.0), "{info:?}");
        assert!(
            downloaded
                .notes
                .iter()
                .any(|n| n.to_lowercase().contains("stream ended")),
            "{:?}",
            downloaded.notes
        );
        assert_eq!(progress.total, Some(60));
        assert!(progress.done >= 2, "{progress:?}");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn rtp_sessions_described_by_an_sdp_are_recorded() {
        let dir = temp_dir("rtp");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let port = free_port().await & !1;
        let sdp_path = dir.join("sender.sdp");
        let mut args = source_args(4, false);
        args.extend(
            [
                "-f",
                "rtp",
                "-sdp_file",
                sdp_path.to_str().unwrap(),
                &format!("rtp://127.0.0.1:{port}"),
            ]
            .map(String::from),
        );
        let _sender = spawn_ffmpeg(&ffmpeg, &args);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let sdp = loop {
            if let Ok(text) = tokio::fs::read_to_string(&sdp_path).await
                && text.contains("m=video")
            {
                break text;
            }
            assert!(tokio::time::Instant::now() < deadline, "no SDP written");
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        let site = Site::new();
        site.put_text("https://cam.test/session.sdp", "application/sdp", &sdp);
        site.put_text(
            "https://cam.test/page.sdp",
            "application/sdp",
            "v=0\no=- 0 0 IN IP4 127.0.0.1\ns=nothing\n",
        );
        let mut context = DownloadContext::new(50_000_000);
        context.max_live = Duration::from_secs(60);
        let (result, _) = record(
            &site,
            &ffmpeg,
            "https://cam.test/session.sdp",
            VariantKind::Rtp,
            &dir.join("job"),
            &context,
        )
        .await;
        let downloaded = result.unwrap();
        let info = ffmpeg.probe(&downloaded.file.path).await.unwrap();
        assert!(info.video.is_some(), "{info:?}");
        assert!(info.duration.unwrap().as_secs_f64() >= 1.5, "{info:?}");
        assert!(
            downloaded
                .notes
                .iter()
                .any(|n| n.contains("Stream idle for")),
            "{:?}",
            downloaded.notes
        );
        assert!(dir.join("job").join("session.sdp").exists());
        // A document without a media line is not a session.
        let (result, _) = record(
            &site,
            &ffmpeg,
            "https://cam.test/page.sdp",
            VariantKind::Rtp,
            &dir.join("bad"),
            &context,
        )
        .await;
        assert!(matches!(result.unwrap_err(), DownloadError::Manifest(_)));
        // The wrong scheme for the kind is refused before ffmpeg runs.
        let (result, _) = record(
            &site,
            &ffmpeg,
            "https://cam.test/x",
            VariantKind::Rtsp,
            &dir.join("scheme"),
            &context,
        )
        .await;
        assert!(matches!(result.unwrap_err(), DownloadError::Unsupported(_)));
        let (result, _) = record(
            &site,
            &ffmpeg,
            "ftp://cam.test/x",
            VariantKind::Rtp,
            &dir.join("scheme2"),
            &context,
        )
        .await;
        assert!(matches!(result.unwrap_err(), DownloadError::Unsupported(_)));
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// The RTP packets and session description of `seconds` of test picture, captured
    /// from ffmpeg sending them over the loopback.
    async fn captured_rtp(ffmpeg: &Ffmpeg, dir: &Path, seconds: u32) -> (String, Vec<Vec<u8>>) {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = socket.local_addr().unwrap().port();
        let sdp_path = dir.join("capture.sdp");
        let mut args = source_args(seconds, false);
        args.extend(
            [
                "-f",
                "rtp",
                "-sdp_file",
                sdp_path.to_str().unwrap(),
                &format!("rtp://127.0.0.1:{port}"),
            ]
            .map(String::from),
        );
        let mut sender = spawn_ffmpeg(ffmpeg, &args);
        let mut packets = Vec::new();
        let mut buf = vec![0u8; 2000];
        loop {
            tokio::select! {
                received = socket.recv_from(&mut buf) => {
                    let (n, _) = received.unwrap();
                    packets.push(buf[..n].to_vec());
                }
                _ = sender.wait() => break,
            }
        }
        // Whatever is still queued on the socket.
        while let Ok(Ok((n, _))) =
            tokio::time::timeout(Duration::from_millis(200), socket.recv_from(&mut buf)).await
        {
            packets.push(buf[..n].to_vec());
        }
        let sdp = tokio::fs::read_to_string(&sdp_path).await.unwrap();
        (sdp, packets)
    }

    /// A small RTSP server: one presentation, served over the RTSP connection itself as
    /// interleaved frames, paced by the packets' timestamps.
    async fn serve_rtsp(listener: TcpListener, sdp: String, packets: Vec<Vec<u8>>) {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        loop {
            let Ok(Some(request)) = lines.next_line().await else {
                return;
            };
            if request.trim().is_empty() {
                continue;
            }
            let method = request.split_whitespace().next().unwrap_or("").to_string();
            let mut cseq = String::from("0");
            while let Ok(Some(line)) = lines.next_line().await {
                if line.trim().is_empty() {
                    break;
                }
                if let Some(value) = line.strip_prefix("CSeq:") {
                    cseq = value.trim().to_string();
                }
            }
            let reply = match method.as_str() {
                "OPTIONS" => format!(
                    "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nPublic: OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN\r\n\r\n"
                ),
                "DESCRIBE" => format!(
                    "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Base: rtsp://127.0.0.1/cam/\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}",
                    sdp.len()
                ),
                "SETUP" => format!(
                    "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: 1;timeout=60\r\nTransport: RTP/AVP/TCP;unicast;interleaved=0-1\r\n\r\n"
                ),
                "PLAY" => format!(
                    "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: 1\r\nRange: npt=0.000-\r\n\r\n"
                ),
                _ => format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: 1\r\n\r\n"),
            };
            write.write_all(reply.as_bytes()).await.unwrap();
            if method == "PLAY" {
                let mut first_time: Option<(u32, tokio::time::Instant)> = None;
                for packet in &packets {
                    if packet.len() < 12 {
                        continue;
                    }
                    let timestamp = u32::from_be_bytes(packet[4..8].try_into().unwrap());
                    let (start, at) =
                        *first_time.get_or_insert((timestamp, tokio::time::Instant::now()));
                    let due = at
                        + Duration::from_secs_f64(
                            f64::from(timestamp.wrapping_sub(start)) / 90_000.0,
                        );
                    tokio::time::sleep_until(due).await;
                    let mut frame = vec![b'$', 0];
                    frame.extend_from_slice(&(packet.len() as u16).to_be_bytes());
                    frame.extend_from_slice(packet);
                    if write.write_all(&frame).await.is_err() {
                        return;
                    }
                }
                let _ = write.shutdown().await;
                return;
            }
            if method == "TEARDOWN" {
                return;
            }
        }
    }

    #[tokio::test]
    async fn rtsp_streams_are_recorded_until_the_server_hangs_up() {
        let dir = temp_dir("rtsp");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let (sdp, packets) = captured_rtp(&ffmpeg, &dir, 3).await;
        assert!(packets.len() > 20, "{} packets", packets.len());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(serve_rtsp(listener, sdp, packets));
        let site = Site::new();
        let mut context = DownloadContext::new(50_000_000);
        context.max_live = Duration::from_secs(60);
        let (result, progress) = record(
            &site,
            &ffmpeg,
            &format!("rtsp://127.0.0.1:{port}/cam"),
            VariantKind::Rtsp,
            &dir.join("job"),
            &context,
        )
        .await;
        let downloaded = result.unwrap();
        let info = ffmpeg.probe(&downloaded.file.path).await.unwrap();
        assert!(info.video.is_some(), "{info:?}");
        assert!(near(info.duration.unwrap().as_secs_f64(), 3.0), "{info:?}");
        assert!(downloaded.notes.iter().any(|n| n.to_lowercase().contains("stream ended") || n.contains("Stream idle for")), "{:?}", downloaded.notes);
        assert_eq!(progress.total, Some(60));
        server.abort();

        // A capture limit cuts a stream that would go on.
        let (sdp, packets) = captured_rtp(&ffmpeg, &dir, 4).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(serve_rtsp(listener, sdp, packets));
        context.max_live = Duration::from_secs(2);
        let (result, _) = record(
            &site,
            &ffmpeg,
            &format!("rtsp://127.0.0.1:{port}/cam"),
            VariantKind::Rtsp,
            &dir.join("cut"),
            &context,
        )
        .await;
        let downloaded = result.unwrap();
        let info = ffmpeg.probe(&downloaded.file.path).await.unwrap();
        assert!(near(info.duration.unwrap().as_secs_f64(), 2.0), "{info:?}");
        assert!(
            downloaded
                .notes
                .iter()
                .any(|n| n.contains("Capture ended at the 2 s limit")),
            "{:?}",
            downloaded.notes
        );
        server.abort();

        // A person stopping the capture keeps what was recorded and ends it there.
        let (sdp, packets) = captured_rtp(&ffmpeg, &dir, 6).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(serve_rtsp(listener, sdp, packets));
        context.max_live = Duration::from_secs(60);
        let stop = context.stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(2500)).await;
            stop.cancel();
        });
        let (result, _) = record(
            &site,
            &ffmpeg,
            &format!("rtsp://127.0.0.1:{port}/cam"),
            VariantKind::Rtsp,
            &dir.join("stopped"),
            &context,
        )
        .await;
        let downloaded = result.unwrap();
        assert_eq!(downloaded.file.path.file_name().unwrap(), RECORDING);
        let info = ffmpeg.probe(&downloaded.file.path).await.unwrap();
        let recorded = info.duration.unwrap().as_secs_f64();
        assert!((1.0..4.5).contains(&recorded), "{info:?}");
        assert!(
            downloaded
                .notes
                .iter()
                .any(|n| n.starts_with("Capture stopped at")),
            "{:?}",
            downloaded.notes
        );
        server.abort();
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn the_codec_ffmpeg_refused_is_read_from_its_complaint() {
        let (audio, name) = refused_codec(
            "x | Could not find tag for codec pcm_mulaw in stream #1, codec not currently supported in container | y",
        )
        .unwrap();
        assert!(audio);
        assert_eq!(name, "pcm_mulaw");
        let (audio, name) =
            refused_codec("Could not find tag for codec flv1 in stream #0, codec not currently supported in container")
                .unwrap();
        assert!(!audio);
        assert_eq!(name, "flv1");
        assert_eq!(refused_codec("exit status 1: Connection refused"), None);
    }
}
