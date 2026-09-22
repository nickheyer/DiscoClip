//! Record WHEP streams through a receive-only WebRTC peer. Send decrypted RTP to ffmpeg
//! over loopback for remuxing.
//!
//! Stop at the capture limit, sender disconnect or media timeout. Delete the WHEP session
//! when recording ends.

use std::ffi::OsString;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use str0m::change::SdpAnswer;
use str0m::format::{Codec, PayloadParams};
use str0m::media::{Direction, KeyframeRequestKind, MediaKind, Mid};
use str0m::net::{Protocol, Receive};
use str0m::rtp::RtpPacket;
use str0m::rtp::rtcp::SenderInfo;
use str0m::{Candidate, Event, IceConnectionState, Input, Output, Rtc};
use tokio::net::UdpSocket;
use tokio::sync::oneshot;
use url::Url;

use super::{DownloadContext, DownloadError, Downloaded, Downloader};
use crate::event::{Progress, ProgressSender};
use crate::ffmpeg::{Ending, Ffmpeg};
use crate::http::Http;
use crate::media::LocalFile;
use crate::resolve::{Variant, VariantKind};

const MAX_ANSWER: usize = 1024 * 1024;
/// How long the peer connection may take to come up.
const CONNECT: Duration = Duration::from_secs(20);
/// How long media may take to start arriving once connected, and how long it may pause.
const STALL: Duration = Duration::from_secs(20);
/// How long a connected session waits for audio to show up beside video before the
/// recording starts without it.
const AUDIO_GRACE: Duration = Duration::from_secs(3);

pub struct WhepDownloader {
    http: Http,
    ffmpeg: Ffmpeg,
    quiet_after: Duration,
}

impl WhepDownloader {
    pub fn new(http: Http, ffmpeg: Ffmpeg) -> Self {
        Self {
            http,
            ffmpeg,
            quiet_after: STALL,
        }
    }

    /// How long media may take to start arriving, and how long it may pause, before the
    /// session counts as over.
    pub fn quiet_after(mut self, quiet_after: Duration) -> Self {
        self.quiet_after = quiet_after;
        self
    }
}

fn process(message: impl Into<String>) -> DownloadError {
    DownloadError::Process(message.into())
}

/// The address of this host that reaches `endpoint`, as the OS would route it.
async fn local_ip_for(endpoint: &Url) -> IpAddr {
    let host = endpoint.host_str().unwrap_or("127.0.0.1");
    let port = endpoint.port_or_known_default().unwrap_or(443);
    let target = tokio::net::lookup_host((host, port))
        .await
        .ok()
        .and_then(|mut addrs| addrs.next());
    if let Some(target) = target
        && let Ok(probe) = std::net::UdpSocket::bind(if target.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })
        && probe.connect(target).is_ok()
        && let Ok(local) = probe.local_addr()
    {
        return local.ip();
    }
    IpAddr::V4(Ipv4Addr::LOCALHOST)
}

/// An even loopback port with the odd one above it free, as an RTP session wants for its
/// RTP and RTCP. Both are released for ffmpeg to bind.
fn loopback_port_pair() -> Result<u16, DownloadError> {
    for _ in 0..64 {
        let first = std::net::UdpSocket::bind("127.0.0.1:0")?;
        let port = first.local_addr()?.port();
        if port % 2 == 1 || port == u16::MAX {
            continue;
        }
        if std::net::UdpSocket::bind(("127.0.0.1", port + 1)).is_ok() {
            return Ok(port);
        }
    }
    Err(process("no free loopback port pair for the RTP session"))
}

/// The name ffmpeg knows a codec by in an `rtpmap`.
fn rtpmap_name(codec: Codec) -> Option<&'static str> {
    Some(match codec {
        Codec::H264 => "H264",
        Codec::H265 => "H265",
        Codec::Vp8 => "VP8",
        Codec::Vp9 => "VP9",
        Codec::Opus => "opus",
        Codec::PCMU => "PCMU",
        Codec::PCMA => "PCMA",
        _ => return None,
    })
}

/// One media of the session as ffmpeg is to read it.
struct Feed {
    kind: MediaKind,
    port: u16,
    params: PayloadParams,
}

/// The session description ffmpeg reads the loopback RTP session from.
fn session_description(feeds: &[Feed]) -> String {
    let mut sdp = String::from("v=0\r\no=- 0 0 IN IP4 127.0.0.1\r\ns=whep\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\n");
    for feed in feeds {
        let spec = feed.params.spec();
        let pt = *feed.params.pt();
        let media = match feed.kind {
            MediaKind::Video => "video",
            MediaKind::Audio => "audio",
        };
        let name = rtpmap_name(spec.codec).unwrap_or("unknown");
        let channels = match (feed.kind, spec.channels) {
            (MediaKind::Audio, Some(channels)) => format!("/{channels}"),
            (MediaKind::Audio, None) if spec.codec == Codec::Opus => "/2".into(),
            _ => String::new(),
        };
        sdp.push_str(&format!("m={media} {} RTP/AVP {pt}\r\n", feed.port));
        sdp.push_str(&format!("a=rtpmap:{pt} {name}/{}{channels}\r\n", spec.clock_rate.get()));
        let fmtp = spec.format.to_string();
        if !fmtp.is_empty() {
            sdp.push_str(&format!("a=fmtp:{pt} {fmtp}\r\n"));
        }
        sdp.push_str("a=recvonly\r\n");
    }
    sdp
}

/// The packet as it went over the wire, with a plain twelve-byte header: the payload
/// str0m hands over is already free of extensions and padding.
fn rtp_bytes(packet: &RtpPacket) -> Vec<u8> {
    let header = &packet.header;
    let mut out = Vec::with_capacity(12 + packet.payload.len());
    out.push(0x80);
    out.push((*header.payload_type & 0x7f) | if header.marker { 0x80 } else { 0 });
    out.extend_from_slice(&header.sequence_number.to_be_bytes());
    out.extend_from_slice(&header.timestamp.to_be_bytes());
    out.extend_from_slice(&(*header.ssrc).to_be_bytes());
    out.extend_from_slice(&packet.payload);
    out
}

/// A sender report as the sender last gave it, so ffmpeg lines the tracks up by wall
/// clock rather than by whichever packet arrived first.
fn sender_report(info: &SenderInfo) -> Vec<u8> {
    let since_epoch = info
        .ntp_time
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO);
    // NTP counts from 1900, seventy years ahead of the Unix epoch.
    let ntp_seconds = (since_epoch.as_secs() as u32).wrapping_add(2_208_988_800);
    let ntp_fraction = ((u64::from(since_epoch.subsec_nanos()) << 32) / 1_000_000_000) as u32;
    let mut out = vec![0x80, 200, 0, 6];
    out.extend_from_slice(&(*info.ssrc).to_be_bytes());
    out.extend_from_slice(&ntp_seconds.to_be_bytes());
    out.extend_from_slice(&ntp_fraction.to_be_bytes());
    out.extend_from_slice(&(info.rtp_time.numer() as u32).to_be_bytes());
    out.extend_from_slice(&info.sender_packet_count.to_be_bytes());
    out.extend_from_slice(&info.sender_octet_count.to_be_bytes());
    out
}

/// What the peer connection has taken in for one media.
#[derive(Default)]
struct Incoming {
    /// Packets that arrived before ffmpeg was listening.
    held: Vec<Vec<u8>>,
    /// The sender report last forwarded, by its wall clock time.
    reported: Option<SystemTime>,
}

#[async_trait]
impl Downloader for WhepDownloader {
    fn handles(&self, kind: VariantKind) -> bool {
        kind == VariantKind::Whep
    }

    async fn download(
        &self,
        variant: &Variant,
        dest_dir: &Path,
        context: &DownloadContext,
        progress: ProgressSender,
    ) -> Result<Downloaded, DownloadError> {
        tokio::fs::create_dir_all(dest_dir).await?;
        let endpoint = &variant.url;
        if !matches!(endpoint.scheme(), "http" | "https") {
            return Err(DownloadError::Unsupported(format!(
                "{endpoint} is not a WHEP endpoint"
            )));
        }
        let local_ip = local_ip_for(endpoint).await;
        let socket = UdpSocket::bind(SocketAddr::new(
            if local_ip.is_ipv4() { IpAddr::V4(Ipv4Addr::UNSPECIFIED) } else { "::".parse().unwrap() },
            0,
        ))
        .await?;
        let local_addr = SocketAddr::new(local_ip, socket.local_addr()?.port());

        // The offer: every codec ffmpeg can take from RTP, to receive only.
        let mut rtc = Rtc::builder()
            .set_rtp_mode(true)
            .clear_codecs()
            .enable_h264(true)
            .enable_h265(true)
            .enable_vp8(true)
            .enable_vp9(true)
            .enable_opus(true)
            .enable_pcmu(true)
            .enable_pcma(true)
            .build(Instant::now());
        rtc.add_local_candidate(
            Candidate::host(local_addr, "udp").map_err(|e| process(format!("local candidate: {e}")))?,
        );
        let mut api = rtc.sdp_api();
        let video_mid = api.add_media(MediaKind::Video, Direction::RecvOnly, None, None, None);
        let audio_mid = api.add_media(MediaKind::Audio, Direction::RecvOnly, None, None, None);
        let (offer, pending) = api
            .apply()
            .ok_or_else(|| process("the offer has nothing to negotiate"))?;

        let response = self
            .http
            .post(endpoint.clone())
            .platform(&context.platform)
            .headers(&variant.headers)
            .header("content-type", "application/sdp")
            .header("accept", "application/sdp")
            .body(offer.to_sdp_string())
            .send()
            .await?;
        if !response.status.is_success() {
            return Err(DownloadError::Status {
                status: response.status.as_u16(),
                url: endpoint.to_string(),
            });
        }
        let resource: Option<Url> = response
            .header("location")
            .and_then(|location| response.url.join(location).ok());
        let answer_text = String::from_utf8_lossy(&response.bytes(MAX_ANSWER).await?).into_owned();
        let answer = SdpAnswer::from_sdp_string(&answer_text)
            .map_err(|e| process(format!("the WHEP answer is not an SDP: {e}")))?;
        rtc.sdp_api()
            .accept_answer(pending, answer)
            .map_err(|e| process(format!("the WHEP answer was refused: {e}")))?;
        let active = |rtc: &Rtc, mid: Mid| {
            rtc.media(mid)
                .is_some_and(|m| m.direction() != Direction::Inactive && !m.disabled())
        };
        let video_active = active(&rtc, video_mid);
        let audio_active = active(&rtc, audio_mid);
        if !video_active && !audio_active {
            return Err(process("the endpoint answered without any media"));
        }
        let params_for = |rtc: &Rtc, pt: u8| -> Option<PayloadParams> {
            rtc.codec_config()
                .iter()
                .find(|p| *p.pt() == pt && p.spec().codec != Codec::Rtx)
                .cloned()
        };

        // The session runs until ffmpeg, fed over the loopback, is done.
        let video_port = loopback_port_pair()?;
        let audio_port = loopback_port_pair()?;
        let feeder = UdpSocket::bind("127.0.0.1:0").await?;
        let sdp_path = dest_dir.join("session.sdp");
        let dest = dest_dir.join("source.mkv");
        let max_live = context.max_live;
        let quiet_after = self.quiet_after;
        progress.send_replace(Progress {
            done: 0,
            total: Some(max_live.as_secs()),
        });
        let mut video = Incoming::default();
        let mut audio = Incoming::default();
        let mut feeds: Vec<Feed> = Vec::new();
        type Recorded = Result<(crate::ffmpeg::Output, Ending), crate::ffmpeg::FfmpegError>;
        let mut recorder: Option<tokio::task::JoinHandle<Recorded>> = None;
        let mut recorded: Option<Recorded> = None;
        let mut stop: Option<oneshot::Sender<()>> = None;
        let mut connected_at: Option<Instant> = None;
        let mut last_media = Instant::now();
        let mut keyframe_asked: Option<Instant> = None;
        let mut buf = vec![0u8; 2000];
        let started = Instant::now();
        let mut ended_by: Option<String> = None;
        let mut notes = Vec::new();

        'session: loop {
            // Drain the engine.
            let timeout = loop {
                match rtc.poll_output().map_err(|e| process(format!("WebRTC: {e}")))? {
                    Output::Timeout(t) => break t,
                    Output::Transmit(t) => {
                        let _ = socket.send_to(&t.contents, t.destination).await;
                    }
                    Output::Event(event) => match event {
                        Event::Connected => {
                            connected_at = Some(Instant::now());
                            last_media = Instant::now();
                        }
                        Event::IceConnectionStateChange(IceConnectionState::Disconnected) => {
                            ended_by = Some("the sender went away".into());
                            break 'session;
                        }
                        Event::RtpPacket(packet) => {
                            let mid = packet.header.ext_vals.mid;
                            let kind = if mid == Some(video_mid) {
                                MediaKind::Video
                            } else if mid == Some(audio_mid) {
                                MediaKind::Audio
                            } else if let Some(feed) = feeds.iter().find(|f| *f.params.pt() == *packet.header.payload_type) {
                                feed.kind
                            } else if audio_active && !video_active {
                                MediaKind::Audio
                            } else if let Some(params) = params_for(&rtc, *packet.header.payload_type) {
                                params.spec().codec.kind()
                            } else {
                                continue;
                            };
                            last_media = Instant::now();
                            let incoming = if kind == MediaKind::Video { &mut video } else { &mut audio };
                            if recorder.is_none() && !feeds.iter().any(|f| f.kind == kind)
                                && let Some(params) = params_for(&rtc, *packet.header.payload_type)
                                && rtpmap_name(params.spec().codec).is_some()
                            {
                                feeds.push(Feed {
                                    kind,
                                    port: if kind == MediaKind::Video { video_port } else { audio_port },
                                    params,
                                });
                            }
                            let bytes = rtp_bytes(&packet);
                            let report = packet.last_sender_info.as_ref().filter(|info| incoming.reported != Some(info.ntp_time)).map(sender_report);
                            if let Some(info) = &packet.last_sender_info {
                                incoming.reported = Some(info.ntp_time);
                            }
                            match feeds.iter().find(|f| f.kind == kind) {
                                Some(feed) if recorder.is_some() => {
                                    if let Some(report) = report {
                                        let _ = feeder.send_to(&report, ("127.0.0.1", feed.port + 1)).await;
                                    }
                                    let _ = feeder.send_to(&bytes, ("127.0.0.1", feed.port)).await;
                                }
                                _ => {
                                    if let Some(report) = report {
                                        incoming.held.push(report);
                                    }
                                    incoming.held.push(bytes);
                                }
                            }
                        }
                        Event::Closed => {
                            ended_by = Some("the session was closed".into());
                            break 'session;
                        }
                        _ => {}
                    },
                }
            };

            // Start ffmpeg once the media it needs has shown itself.
            if recorder.is_none() && connected_at.is_some() {
                let has_video = feeds.iter().any(|f| f.kind == MediaKind::Video);
                let has_audio = feeds.iter().any(|f| f.kind == MediaKind::Audio);
                let waited = connected_at.map_or(Duration::ZERO, |at| at.elapsed());
                let ready = (!video_active || has_video)
                    && (!audio_active || has_audio || (has_video && waited >= AUDIO_GRACE));
                if ready && !feeds.is_empty() {
                    if audio_active && !has_audio {
                        notes.push("No audio received. Recording contains video only.".into());
                    }
                    if video_active && !has_video {
                        notes.push("No video received. Recording contains audio only.".into());
                    }
                    feeds.sort_by_key(|f| if f.kind == MediaKind::Video { 0 } else { 1 });
                    tokio::fs::write(&sdp_path, session_description(&feeds)).await?;
                    let mut args: Vec<OsString> = vec![
                        "-loglevel".into(),
                        "warning".into(),
                        "-protocol_whitelist".into(),
                        "file,udp,rtp".into(),
                        "-i".into(),
                        sdp_path.as_os_str().to_owned(),
                        "-t".into(),
                        format!("{:.3}", max_live.as_secs_f64()).into(),
                        "-fs".into(),
                        context.max_bytes.to_string().into(),
                    ];
                    args.extend(
                        ["-map", "0:v:0?", "-map", "0:a:0?", "-c", "copy", "-sn", "-dn", "-f", "matroska"]
                            .map(OsString::from),
                    );
                    args.push(dest.as_os_str().to_owned());
                    let (tx, rx) = oneshot::channel();
                    stop = Some(tx);
                    let ffmpeg = self.ffmpeg.clone();
                    let progress = progress.clone();
                    recorder = Some(tokio::spawn(async move {
                        ffmpeg
                            .record(
                                args,
                                move |time| {
                                    progress.send_replace(Progress {
                                        done: time.as_secs(),
                                        total: Some(max_live.as_secs()),
                                    });
                                },
                                quiet_after,
                                Some(rx),
                            )
                            .await
                    }));
                    // ffmpeg binds its ports on start. The held packets follow shortly.
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    for feed in &feeds {
                        let incoming = if feed.kind == MediaKind::Video { &mut video } else { &mut audio };
                        for bytes in incoming.held.drain(..) {
                            let port = if bytes.len() >= 2 && bytes[1] == 200 { feed.port + 1 } else { feed.port };
                            let _ = feeder.send_to(&bytes, ("127.0.0.1", port)).await;
                        }
                    }
                    if has_video {
                        if let Some(stream) = rtc.direct_api().stream_rx_by_mid(video_mid, None) {
                            stream.request_keyframe(KeyframeRequestKind::Pli);
                        }
                        keyframe_asked = Some(Instant::now());
                        continue;
                    }
                }
            }
            // A keyframe is asked for again once, in case the first request was lost.
            if let Some(asked) = keyframe_asked
                && asked.elapsed() >= Duration::from_secs(3)
            {
                keyframe_asked = None;
                if let Some(stream) = rtc.direct_api().stream_rx_by_mid(video_mid, None) {
                    stream.request_keyframe(KeyframeRequestKind::Pli);
                }
                continue;
            }

            // Give up on a connection or media that never comes.
            if connected_at.is_none() && started.elapsed() > CONNECT {
                return Err(process(format!(
                    "the WebRTC connection to {endpoint} did not come up within {} s",
                    CONNECT.as_secs()
                )));
            }
            if connected_at.is_some() && last_media.elapsed() > quiet_after {
                if recorder.is_none() {
                    return Err(process(format!(
                        "no media arrived from {endpoint} within {} s of connecting",
                        quiet_after.as_secs()
                    )));
                }
                ended_by = Some(format!("the media stopped arriving for {} s", quiet_after.as_secs()));
                break 'session;
            }
            if recorded.is_some() {
                break 'session;
            }

            // Wait for the next thing: a packet, the engine's timeout, or ffmpeg finishing.
            let wait = tokio::time::sleep_until(tokio::time::Instant::from_std(timeout.max(Instant::now())));
            tokio::pin!(wait);
            tokio::select! {
                received = socket.recv_from(&mut buf) => {
                    let (n, source) = received?;
                    let now = Instant::now();
                    if let Ok(receive) = Receive::new(Protocol::Udp, source, local_addr, &buf[..n]) {
                        let input = Input::Receive(now, receive);
                        if rtc.accepts(&input) {
                            rtc.handle_input(input).map_err(|e| process(format!("WebRTC: {e}")))?;
                        }
                    }
                }
                _ = &mut wait => {
                    rtc.handle_input(Input::Timeout(Instant::now()))
                        .map_err(|e| process(format!("WebRTC: {e}")))?;
                }
                joined = async {
                    match recorder.as_mut() {
                        Some(task) => task.await,
                        None => std::future::pending().await,
                    }
                }, if recorder.is_some() => {
                    recorder = None;
                    recorded = Some(joined.map_err(|e| process(format!("the recorder task failed: {e}")))?);
                    // Taken up at the top of the loop, once the engine is drained.
                    rtc.handle_input(Input::Timeout(Instant::now()))
                        .map_err(|e| process(format!("WebRTC: {e}")))?;
                }
            }
        }

        // Wind down: the recording is closed, then the session at the endpoint.
        if let Some(tx) = stop.take() {
            let _ = tx.send(());
        }
        rtc.disconnect();
        let outcome = match (recorded.take(), recorder.take()) {
            (Some(result), _) => Some(result.map_err(|e| DownloadError::Process(e.to_string()))?),
            (None, Some(task)) => Some(
                task.await
                    .map_err(|e| process(format!("the recorder task failed: {e}")))?
                    .map_err(|e| DownloadError::Process(e.to_string()))?,
            ),
            (None, None) => None,
        };
        if let Some(resource) = resource {
            let _ = self
                .http
                .delete(resource)
                .platform(&context.platform)
                .headers(&variant.headers)
                .timeout(Duration::from_secs(5))
                .send()
                .await;
        }
        let Some((output, ending)) = outcome else {
            return Err(process(
                ended_by.unwrap_or_else(|| "the session ended before any media arrived".into()),
            ));
        };
        let captured = output.last_time.unwrap_or(Duration::ZERO).as_secs_f64();
        let limit = max_live.as_secs_f64();
        match ending {
            Ending::Finished if captured + 1.0 >= limit => notes.push(format!(
                "capture cut at the limit of {limit:.0} s while the stream goes on"
            )),
            Ending::Stalled => notes.push(format!(
                "No media received for {} s. Recorded {captured:.0} s.",
                quiet_after.as_secs()
            )),
            _ => notes.push(format!(
                "{}. Recorded {captured:.0} s.",
                ended_by.unwrap_or_else(|| "the stream ended".into())
            )),
        }
        let file = LocalFile::from_path(dest).await?;
        if file.size == 0 {
            return Err(DownloadError::Empty);
        }
        if file.size > context.max_bytes {
            return Err(DownloadError::TooLarge {
                size: file.size,
                limit: context.max_bytes,
            });
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
    use std::sync::{Arc, Mutex};

    use bytes::Bytes;
    use http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
    use str0m::change::SdpOffer;
    use str0m::media::MediaKind;
    use str0m::rtp::{RtpWrite, SeqNo};
    use tokio::process::Command;

    use super::*;
    use crate::http::HttpError;
    use crate::http::transport::{Transport, TransportRequest, TransportResponse};

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("discoclip-whep-{tag}-{}", uuid::Uuid::now_v7()))
    }

    /// One RTP packet as ffmpeg sent it: when, of which kind, and its bytes.
    #[derive(Clone)]
    struct Sent {
        at: Duration,
        kind: MediaKind,
        bytes: Vec<u8>,
    }

    /// `seconds` of test picture as H264 and tone as Opus, captured as the RTP packets
    /// ffmpeg sends over the loopback, in the order and at the times they left.
    async fn captured_rtp(ffmpeg: &Ffmpeg, seconds: u32) -> Vec<Sent> {
        let video = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let audio = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let mut child = Command::new(ffmpeg.ffmpeg_path())
            .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-re"])
            .args(["-f", "lavfi", "-i", &format!("testsrc=size=64x64:rate=10:duration={seconds}")])
            .args(["-f", "lavfi", "-i", &format!("sine=frequency=440:sample_rate=48000:duration={seconds}")])
            .args(["-map", "0:v", "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-g", "10", "-f", "rtp"])
            .arg(format!("rtp://127.0.0.1:{}", video.local_addr().unwrap().port()))
            .args(["-map", "1:a", "-c:a", "libopus", "-b:a", "48k", "-f", "rtp"])
            .arg(format!("rtp://127.0.0.1:{}", audio.local_addr().unwrap().port()))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let started = Instant::now();
        let mut sent = Vec::new();
        let mut vbuf = vec![0u8; 2000];
        let mut abuf = vec![0u8; 2000];
        loop {
            tokio::select! {
                received = video.recv_from(&mut vbuf) => {
                    let (n, _) = received.unwrap();
                    if vbuf[1] & 0x7f < 72 || vbuf[1] & 0x7f > 76 {
                        sent.push(Sent { at: started.elapsed(), kind: MediaKind::Video, bytes: vbuf[..n].to_vec() });
                    }
                }
                received = audio.recv_from(&mut abuf) => {
                    let (n, _) = received.unwrap();
                    if abuf[1] & 0x7f < 72 || abuf[1] & 0x7f > 76 {
                        sent.push(Sent { at: started.elapsed(), kind: MediaKind::Audio, bytes: abuf[..n].to_vec() });
                    }
                }
                _ = child.wait() => break,
            }
        }
        sent
    }

    /// What the endpoint saw: sessions opened and closed.
    #[derive(Default)]
    struct Seen {
        posts: usize,
        deletes: usize,
        offer_had_video: bool,
        offer_had_audio: bool,
    }

    /// A WHEP endpoint over the fake transport: a POST negotiates a peer connection that
    /// plays the captured packets to the client. A DELETE ends the session.
    struct Endpoint {
        packets: Vec<Sent>,
        seen: Arc<Mutex<Seen>>,
    }

    fn text_response(status: u16, content_type: &str, body: String, extra: &[(&str, &str)]) -> TransportResponse {
        let mut headers = HeaderMap::new();
        headers.insert("content-type", HeaderValue::from_str(content_type).unwrap());
        for (name, value) in extra {
            headers.insert(HeaderName::from_bytes(name.as_bytes()).unwrap(), HeaderValue::from_str(value).unwrap());
        }
        TransportResponse {
            status: StatusCode::from_u16(status).unwrap(),
            url: Url::parse("https://whep.test/whep").unwrap(),
            headers,
            body: Box::pin(futures::stream::once(async move { Ok(Bytes::from(body)) })),
        }
    }

    #[async_trait]
    impl Transport for Endpoint {
        async fn send(&self, request: TransportRequest) -> Result<TransportResponse, HttpError> {
            match (request.method.as_str(), request.url.path()) {
                ("POST", "/whep") => {
                    assert_eq!(request.headers.get("content-type").unwrap(), "application/sdp");
                    let offer_text = String::from_utf8(request.body.unwrap().to_vec()).unwrap();
                    {
                        let mut seen = self.seen.lock().unwrap();
                        seen.posts += 1;
                        seen.offer_had_video = offer_text.contains("m=video");
                        seen.offer_had_audio = offer_text.contains("m=audio");
                    }
                    let offer = SdpOffer::from_sdp_string(&offer_text).unwrap();
                    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
                    let local = socket.local_addr().unwrap();
                    let mut rtc = Rtc::builder().set_rtp_mode(true).set_ice_lite(true).build(Instant::now());
                    rtc.add_local_candidate(Candidate::host(local, "udp").unwrap());
                    let answer = rtc.sdp_api().accept_offer(offer).unwrap();
                    tokio::spawn(serve(rtc, socket, local, self.packets.clone()));
                    Ok(text_response(201, "application/sdp", answer.to_sdp_string(), &[("location", "/whep/session/1")]))
                }
                ("DELETE", "/whep/session/1") => {
                    self.seen.lock().unwrap().deletes += 1;
                    Ok(text_response(200, "text/plain", String::new(), &[]))
                }
                other => panic!("unexpected request {other:?}"),
            }
        }

        fn name(&self) -> &'static str {
            "whep-endpoint"
        }
    }

    /// The server side of one session: once connected, the packets go out as they were
    /// captured, on the payload types the client and server agreed.
    async fn serve(mut rtc: Rtc, socket: UdpSocket, local: SocketAddr, packets: Vec<Sent>) {
        let mut mids: Vec<(MediaKind, Mid)> = Vec::new();
        let mut connected = false;
        let mut next = 0usize;
        let mut started: Option<Instant> = None;
        let mut seq: Vec<(MediaKind, u64)> = vec![(MediaKind::Video, 1000), (MediaKind::Audio, 5000)];
        let mut buf = vec![0u8; 2000];
        loop {
            let timeout = loop {
                match rtc.poll_output().unwrap() {
                    Output::Timeout(t) => break t,
                    Output::Transmit(t) => {
                        let _ = socket.send_to(&t.contents, t.destination).await;
                    }
                    Output::Event(Event::MediaAdded(added)) => mids.push((added.kind, added.mid)),
                    Output::Event(Event::Connected) => {
                        connected = true;
                        started = Some(Instant::now());
                    }
                    Output::Event(Event::IceConnectionStateChange(IceConnectionState::Disconnected)) => return,
                    Output::Event(_) => {}
                }
            };
            if !rtc.is_alive() {
                return;
            }
            let due = match (connected, packets.get(next), started) {
                (true, Some(packet), Some(started)) => Some(started + packet.at),
                _ => None,
            };
            let wait_until = match due {
                Some(due) if due < timeout => due,
                _ => timeout,
            };
            tokio::select! {
                received = socket.recv_from(&mut buf) => {
                    let (n, source) = received.unwrap();
                    if let Ok(receive) = Receive::new(Protocol::Udp, source, local, &buf[..n]) {
                        let input = Input::Receive(Instant::now(), receive);
                        if rtc.accepts(&input) {
                            rtc.handle_input(input).unwrap();
                        }
                    }
                }
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(wait_until.max(Instant::now()))) => {
                    if due.is_some_and(|d| d <= Instant::now()) {
                        let packet = &packets[next];
                        next += 1;
                        let Some((_, mid)) = mids.iter().find(|(kind, _)| *kind == packet.kind) else {
                            continue;
                        };
                        let wanted = if packet.kind == MediaKind::Video { Codec::H264 } else { Codec::Opus };
                        let remote: Vec<_> = rtc.media(*mid).unwrap().remote_pts().to_vec();
                        let Some(params) = rtc
                            .codec_config()
                            .iter()
                            .find(|p| remote.contains(&p.pt()) && p.spec().codec != Codec::Rtx && p.spec().codec == wanted)
                            .cloned()
                        else {
                            continue;
                        };
                        let bytes = &packet.bytes;
                        let marker = bytes[1] & 0x80 != 0;
                        let timestamp = u32::from_be_bytes(bytes[4..8].try_into().unwrap());
                        let counter = seq.iter_mut().find(|(kind, _)| *kind == packet.kind).unwrap();
                        counter.1 += 1;
                        let seq_no: SeqNo = counter.1.into();
                        let payload = bytes[12..].to_vec();
                        if let Some(stream) = rtc.direct_api().stream_tx_by_mid(*mid, None) {
                            stream.write_rtp(
                                RtpWrite::new(params.pt(), seq_no, timestamp, Instant::now(), payload)
                                    .marker(marker)
                                    .nackable(true),
                            );
                        }
                    } else {
                        rtc.handle_input(Input::Timeout(Instant::now())).unwrap();
                    }
                }
            }
        }
    }

    #[tokio::test]
    async fn whep_sessions_are_recorded_and_torn_down() {
        let dir = temp_dir("session");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let packets = captured_rtp(&ffmpeg, 5).await;
        assert!(packets.iter().any(|p| p.kind == MediaKind::Video));
        assert!(packets.iter().any(|p| p.kind == MediaKind::Audio));
        let seen = Arc::new(Mutex::new(Seen::default()));
        let endpoint = Arc::new(Endpoint {
            packets,
            seen: seen.clone(),
        });
        let http = Http::with_transport(endpoint, Http::test_config());
        let downloader = WhepDownloader::new(http, ffmpeg.clone()).quiet_after(Duration::from_secs(4));
        let mut variant = Variant::new(Url::parse("https://whep.test/whep").unwrap(), VariantKind::Whep);
        variant.live = true;
        let mut context = DownloadContext::new(50_000_000);
        context.max_live = Duration::from_secs(3);
        let (progress, watched) = tokio::sync::watch::channel(Progress::default());
        let downloaded = downloader.download(&variant, &dir.join("job"), &context, progress).await.unwrap();
        let last = *watched.borrow();
        let info = ffmpeg.probe(&downloaded.file.path).await.unwrap();
        assert!(info.video.is_some(), "{info:?}");
        assert!(info.audio.is_some(), "{info:?}");
        let duration = info.duration.unwrap().as_secs_f64();
        assert!((2.0..=3.6).contains(&duration), "{info:?}");
        assert!(downloaded.notes.iter().any(|n| n.contains("capture cut at the limit")), "{:?}", downloaded.notes);
        assert_eq!(last.total, Some(3));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.posts, 1);
        assert_eq!(seen.deletes, 1);
        assert!(seen.offer_had_video && seen.offer_had_audio);
        drop(seen);
        let sdp = tokio::fs::read_to_string(dir.join("job").join("session.sdp")).await.unwrap();
        assert!(sdp.contains("H264/90000"), "{sdp}");
        assert!(sdp.contains("opus/48000"), "{sdp}");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn sessions_are_described_for_ffmpeg() {
        let rtc = Rtc::builder().set_rtp_mode(true).build(Instant::now());
        let params = |codec: Codec| {
            rtc.codec_config()
                .iter()
                .find(|p| p.spec().codec == codec && p.spec().codec != Codec::Rtx)
                .cloned()
                .unwrap()
        };
        let feeds = vec![
            Feed { kind: MediaKind::Video, port: 5004, params: params(Codec::H264) },
            Feed { kind: MediaKind::Audio, port: 5006, params: params(Codec::Opus) },
        ];
        let sdp = session_description(&feeds);
        assert!(sdp.starts_with("v=0\r\n"), "{sdp}");
        assert!(sdp.contains(&format!("m=video 5004 RTP/AVP {}\r\n", *feeds[0].params.pt())), "{sdp}");
        assert!(sdp.contains(&format!("a=rtpmap:{} H264/90000\r\n", *feeds[0].params.pt())), "{sdp}");
        assert!(sdp.contains("packetization-mode=1"), "{sdp}");
        assert!(sdp.contains(&format!("m=audio 5006 RTP/AVP {}\r\n", *feeds[1].params.pt())), "{sdp}");
        assert!(sdp.contains(&format!("a=rtpmap:{} opus/48000/2\r\n", *feeds[1].params.pt())), "{sdp}");
        assert_eq!(rtpmap_name(Codec::Av1), None);
        let port = loopback_port_pair().unwrap();
        assert_eq!(port % 2, 0);
    }
}
