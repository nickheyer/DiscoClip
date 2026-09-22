//! Downloads HLS: media playlists segment by segment, on demand or live with the
//! playlist reloaded as it grows and the capture cut at the limit or ended when the
//! stream ends. AES-128 and SAMPLE-AES undone. The stream split where it says it is
//! discontinuous and the parts joined back. And the subtitle renditions that go with it,
//! followed alongside and aligned to the media.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aes::cipher::{BlockModeDecrypt, KeyIvInit, block_padding::Pkcs7};
use async_trait::async_trait;
use base64::Engine as _;
use futures::StreamExt;
use m3u8_rs::{AlternativeMediaType, KeyMethod, MasterPlaylist, MediaPlaylist, Playlist};
use md5::{Digest, Md5};
use url::Url;

use super::segments::{
    Budget, Consumer, InitSection, Meter, PartKey, Timing, TrackWriter, fetch_bytes,
    fetch_text, mux_parts,
};
use super::{
    DownloadContext, DownloadError, Downloaded, Downloader, LocalSubtitle, mp4, mpegts,
    subtitles,
};
use crate::event::{Progress, ProgressSender};
use crate::ffmpeg::Ffmpeg;
use crate::http::{BROWSER_UA, Http};
use crate::resolve::{Keepalive, SubtitleFormat, SubtitleTrack, Variant, VariantKind, signed_url};

const MAX_PLAYLIST: usize = 8 * 1024 * 1024;
const CONCURRENCY: usize = 4;
/// Reloads of a live playlist that may fail in a row before the stream counts as over.
const RELOAD_FAILURES: u32 = 3;

pub struct HlsDownloader {
    http: Http,
    ffmpeg: Ffmpeg,
}

impl HlsDownloader {
    pub fn new(http: Http, ffmpeg: Ffmpeg) -> Self {
        Self { http, ffmpeg }
    }
}

/// How a segment is encrypted, from the `EXT-X-KEY` assigned for it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Encryption {
    /// The whole segment, AES-128 in CBC with PKCS#7 padding.
    Aes128,
    /// Inside the elementary streams, as Apple's SAMPLE-AES: CBC in a transport stream,
    /// `cbcs` in fragmented MP4.
    SampleAes,
    /// Inside fragmented MP4 samples as `cenc`.
    SampleAesCtr,
}

#[derive(Debug, Clone)]
struct KeySpec {
    method: Encryption,
    url: Url,
    /// The IV the playlist gave. Without one, the media sequence number serves.
    iv: Option<[u8; 16]>,
}

impl KeySpec {
    fn iv_for(&self, sequence: u64) -> [u8; 16] {
        self.iv.unwrap_or_else(|| sequence_iv(sequence))
    }
}

/// An initialization section named by `EXT-X-MAP`.
#[derive(Debug, Clone)]
struct MapSpec {
    url: Url,
    range: Option<(u64, u64)>,
    key: Option<KeySpec>,
}

impl MapSpec {
    fn id(&self) -> String {
        match self.range {
            Some((start, end)) => format!("{}#{start}-{end}", self.url),
            None => self.url.to_string(),
        }
    }
}

/// One segment of a media playlist, with everything needed to fetch and decrypt it.
#[derive(Debug, Clone)]
struct Piece {
    sequence: u64,
    url: Url,
    range: Option<(u64, u64)>,
    key: Option<KeySpec>,
    map: Option<MapSpec>,
    /// The discontinuity sequence the segment falls in.
    discontinuity: u64,
    duration: f64,
}

/// A media playlist loaded, with what a master playlist said should go with it.
struct Loaded {
    base: Url,
    playlist: MediaPlaylist,
    /// The default audio rendition of the chosen stream, when a master named one.
    audio: Option<Url>,
    /// The subtitle renditions a master listed.
    subtitles: Vec<SubtitleTrack>,
}

/// The stream of a master playlist to take: the tallest that fits `max_height`, the
/// smallest when none does, the highest bandwidth among equals.
fn choose_stream(master: &MasterPlaylist, max_height: u32) -> Option<&m3u8_rs::VariantStream> {
    master
        .variants
        .iter()
        .filter(|v| !v.is_i_frame)
        .max_by_key(|v| {
            let height = v.resolution.map_or(0, |r| r.height as u32);
            let fits = height <= max_height;
            (
                fits,
                if fits { i64::from(height) } else { -i64::from(height) },
                v.average_bandwidth.unwrap_or(v.bandwidth),
            )
        })
}

async fn load_media_playlist(
    http: &Http,
    url: &Url,
    platform: &str,
    headers: &[(String, String)],
    query: &[(String, String)],
    max_height: u32,
    depth: u8,
) -> Result<Loaded, DownloadError> {
    let url = &signed_url(url, query);
    let (final_url, text) = fetch_text(http, url, platform, headers, MAX_PLAYLIST).await?;
    match m3u8_rs::parse_playlist_res(text.as_bytes()) {
        Ok(Playlist::MediaPlaylist(media)) => Ok(Loaded {
            base: final_url,
            playlist: media,
            audio: None,
            subtitles: Vec::new(),
        }),
        Ok(Playlist::MasterPlaylist(master)) => {
            if depth == 0 {
                return Err(DownloadError::Manifest("nested master playlists".into()));
            }
            let best = choose_stream(&master, max_height)
                .ok_or_else(|| DownloadError::Manifest("master playlist has no variants".into()))?;
            let next = final_url
                .join(&best.uri)
                .map_err(|e| DownloadError::Manifest(format!("bad variant uri: {e}")))?;
            let audio = match &best.audio {
                Some(group) => master
                    .alternatives
                    .iter()
                    .filter(|a| a.media_type == AlternativeMediaType::Audio && a.group_id == *group)
                    .max_by_key(|a| (a.default, a.autoselect))
                    .and_then(|a| a.uri.as_deref())
                    .map(|uri| {
                        final_url
                            .join(uri)
                            .map_err(|e| DownloadError::Manifest(format!("bad audio uri: {e}")))
                    })
                    .transpose()?,
                None => None,
            };
            let mut subtitles = Vec::new();
            for alternative in master.alternatives.iter().filter(|a| {
                a.media_type == AlternativeMediaType::Subtitles
                    && best.subtitles.as_ref().is_none_or(|g| *g == a.group_id)
            }) {
                if let Some(uri) = &alternative.uri
                    && let Ok(track_url) = final_url.join(uri)
                {
                    subtitles.push(SubtitleTrack {
                        url: track_url,
                        language: alternative.language.clone().unwrap_or_else(|| "und".into()),
                        name: Some(alternative.name.clone()),
                        format: SubtitleFormat::HlsVtt,
                        auto: false,
                        headers: headers.to_vec(),
                    });
                }
            }
            let mut loaded = Box::pin(load_media_playlist(
                http,
                &next,
                platform,
                headers,
                query,
                max_height,
                depth - 1,
            ))
            .await?;
            loaded.audio = audio;
            loaded.subtitles = subtitles;
            Ok(loaded)
        }
        Err(error) => Err(DownloadError::Manifest(format!(
            "invalid playlist at {url}: {error}"
        ))),
    }
}

fn parse_iv(iv: &str) -> Option<[u8; 16]> {
    let iv = iv.trim();
    let digits = iv
        .strip_prefix("0x")
        .or_else(|| iv.strip_prefix("0X"))
        .unwrap_or(iv);
    hex::decode(digits).ok()?.try_into().ok()
}

fn sequence_iv(sequence: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&sequence.to_be_bytes());
    iv
}

/// The DRM system a key belongs to, when its format or URI names one the key cannot be
/// fetched from.
fn drm_system(key: &m3u8_rs::Key) -> Option<String> {
    if let Some(uri) = &key.uri
        && uri.starts_with("skd://")
    {
        return Some("FairPlay".into());
    }
    match key.keyformat.as_deref().map(str::trim) {
        None | Some("identity") => None,
        Some("com.apple.streamingkeydelivery") => Some("FairPlay".into()),
        Some("urn:uuid:edef8ba9-79d6-4a62-a3c8-27dcd51d21ed") => Some("Widevine".into()),
        Some("com.microsoft.playready") => Some("PlayReady".into()),
        Some(other) => Some(other.to_string()),
    }
}

fn key_spec(
    key: &m3u8_rs::Key,
    base: &Url,
    query: &[(String, String)],
    playlist_url: &Url,
) -> Result<Option<KeySpec>, DownloadError> {
    let method = match &key.method {
        KeyMethod::None => return Ok(None),
        KeyMethod::AES128 => Encryption::Aes128,
        KeyMethod::SampleAES => Encryption::SampleAes,
        KeyMethod::Other(name) if name.eq_ignore_ascii_case("SAMPLE-AES-CTR") => {
            Encryption::SampleAesCtr
        }
        KeyMethod::Other(name) => {
            return Err(DownloadError::Manifest(format!(
                "unsupported segment encryption {name}"
            )));
        }
    };
    if let Some(system) = drm_system(key) {
        return Err(DownloadError::Drm(playlist_url.to_string(), system));
    }
    let uri = key
        .uri
        .as_deref()
        .ok_or_else(|| DownloadError::Manifest("encryption key without URI".into()))?;
    let url = base
        .join(uri)
        .map(|url| signed_url(&url, query))
        .map_err(|e| DownloadError::Manifest(format!("bad key uri: {e}")))?;
    let iv = match key.iv.as_deref() {
        Some(iv) => Some(parse_iv(iv).ok_or_else(|| {
            DownloadError::Manifest(format!("unreadable key IV {iv}"))
        })?),
        None => None,
    };
    Ok(Some(KeySpec { method, url, iv }))
}

/// The playlist's segments as pieces to fetch, in order.
fn pieces_of(
    base: &Url,
    media: &MediaPlaylist,
    query: &[(String, String)],
    playlist_url: &Url,
) -> Result<Vec<Piece>, DownloadError> {
    let mut pieces = Vec::with_capacity(media.segments.len());
    let mut key: Option<KeySpec> = None;
    let mut map: Option<MapSpec> = None;
    let mut next_offset = 0u64;
    let mut discontinuity = media.discontinuity_sequence;
    for (index, segment) in media.segments.iter().enumerate() {
        // `#EXT-X-KEY:METHOD=NONE` ends encryption. The parser leaves it among the
        // unknown tags, as it wants an IV of every key tag but those.
        if segment.unknown_tags.iter().any(|t| {
            t.tag == "X-KEY"
                && t.rest.as_deref().is_some_and(|rest| {
                    rest.split(',').any(|attr| attr.trim().eq_ignore_ascii_case("METHOD=NONE"))
                })
        }) {
            key = None;
        }
        if let Some(k) = &segment.key {
            key = key_spec(k, base, query, playlist_url)?;
        }
        if segment.discontinuity {
            discontinuity += 1;
        }
        if let Some(m) = &segment.map {
            let url = base
                .join(&m.uri)
                .map(|url| signed_url(&url, query))
                .map_err(|e| DownloadError::Manifest(format!("bad init uri: {e}")))?;
            let range = m.byte_range.as_ref().map(|r| {
                let start = r.offset.unwrap_or(0);
                (start, start + r.length.saturating_sub(1))
            });
            let same = map.as_ref().is_some_and(|m| m.url == url && m.range == range);
            if !same {
                map = Some(MapSpec {
                    url,
                    range,
                    key: key.clone().filter(|k| k.method == Encryption::Aes128),
                });
            }
        }
        let url = base
            .join(&segment.uri)
            .map(|url| signed_url(&url, query))
            .map_err(|e| DownloadError::Manifest(format!("bad segment uri: {e}")))?;
        let range = segment.byte_range.as_ref().map(|r| {
            let start = r.offset.unwrap_or(next_offset);
            next_offset = start + r.length;
            (start, start + r.length.saturating_sub(1))
        });
        if range.is_none() {
            next_offset = 0;
        }
        pieces.push(Piece {
            sequence: media.media_sequence + index as u64,
            url,
            range,
            key: key.clone(),
            map: map.clone(),
            discontinuity,
            duration: f64::from(segment.duration).max(0.0),
        });
    }
    Ok(pieces)
}

/// The playback report a host wants while its stream is fetched, made every two seconds
/// until the download ends.
struct KeepaliveTask {
    handle: tokio::task::JoinHandle<()>,
}

impl KeepaliveTask {
    fn start(
        http: Http,
        keepalive: Keepalive,
        platform: String,
        headers: Vec<(String, String)>,
    ) -> Self {
        let handle = tokio::spawn(async move {
            let Keepalive::BunnyPing {
                url,
                secret,
                context_id,
            } = keepalive;
            let mut elapsed = 0u64;
            loop {
                tokio::time::sleep(Duration::from_secs(2)).await;
                elapsed += 2;
                let time = format!("{elapsed}.{:06}", rand::random::<u32>() % 1_000_000);
                let hash = hex::encode(Md5::digest(
                    format!("{secret}_{context_id}_{time}_false_1080").as_bytes(),
                ));
                let mut ping = url.clone();
                ping.query_pairs_mut()
                    .append_pair("hash", &hash)
                    .append_pair("time", &time)
                    .append_pair("paused", "false")
                    .append_pair("resolution", "1080");
                match http
                    .get(ping.clone())
                    .platform(&platform)
                    .user_agent(BROWSER_UA)
                    .headers(&headers)
                    .send()
                    .await
                {
                    Ok(response) => {
                        let _ = response.bytes_up_to(4096).await;
                    }
                    Err(error) => tracing::debug!(%ping, "keepalive ping failed: {error}"),
                }
            }
        });
        Self { handle }
    }
}

impl Drop for KeepaliveTask {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn decrypt_aes128(data: &[u8], key: &[u8], iv: &[u8; 16]) -> Result<Vec<u8>, DownloadError> {
    let decryptor = cbc::Decryptor::<aes::Aes128>::new_from_slices(key, iv)
        .map_err(|e| DownloadError::Manifest(format!("bad AES-128 key: {e}")))?;
    decryptor
        .decrypt_padded_vec::<Pkcs7>(data)
        .map_err(|e| DownloadError::Segment(format!("segment decryption failed: {e}")))
}

/// What every playlist of one download shares: the client, the keys and initialization
/// sections fetched so far, the byte budget and the notes for the job log.
struct Session<'a> {
    http: &'a Http,
    platform: &'a str,
    headers: &'a [(String, String)],
    query: &'a [(String, String)],
    max_height: u32,
    max_live: Duration,
    keys: Mutex<HashMap<String, [u8; 16]>>,
    inits: Mutex<HashMap<String, Arc<InitSection>>>,
    notes: Mutex<Vec<String>>,
}

impl Session<'_> {
    fn note(&self, message: String) {
        tracing::info!("{message}");
        self.notes.lock().unwrap_or_else(|e| e.into_inner()).push(message);
    }

    /// The 16 bytes at `url`: fetched, or carried in a `data:` URL.
    async fn key(&self, url: &Url) -> Result<[u8; 16], DownloadError> {
        if let Some(key) = self
            .keys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(url.as_str())
        {
            return Ok(*key);
        }
        let bytes: Vec<u8> = if url.scheme() == "data" {
            let body = url.path();
            let (meta, data) = body.split_once(',').ok_or_else(|| {
                DownloadError::Manifest(format!("key data URL {url} has no data"))
            })?;
            if meta.ends_with(";base64") {
                base64::engine::general_purpose::STANDARD
                    .decode(data.trim())
                    .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(data.trim()))
                    .map_err(|e| DownloadError::Manifest(format!("key data URL {url}: {e}")))?
            } else {
                percent_encoding::percent_decode_str(data).collect()
            }
        } else {
            fetch_bytes(self.http, url, self.platform, self.headers, None)
                .await?
                .to_vec()
        };
        let key: [u8; 16] = bytes.as_slice().try_into().map_err(|_| {
            DownloadError::Manifest(format!(
                "key at {url} is {} bytes, not 16",
                bytes.len()
            ))
        })?;
        self.keys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(url.to_string(), key);
        Ok(key)
    }

    async fn init_section(&self, map: &MapSpec, sequence: u64) -> Result<Arc<InitSection>, DownloadError> {
        let id = map.id();
        if let Some(init) = self
            .inits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&id)
        {
            return Ok(init.clone());
        }
        let mut bytes = fetch_bytes(self.http, &map.url, self.platform, self.headers, map.range)
            .await?
            .to_vec();
        if let Some(key) = &map.key {
            let iv = key.iv.ok_or_else(|| {
                DownloadError::Manifest(format!(
                    "initialization section {} is encrypted without an IV",
                    map.url
                ))
            })?;
            let _ = sequence;
            bytes = decrypt_aes128(&bytes, &self.key(&key.url).await?, &iv)?;
        }
        let parsed = if mp4::is_mp4(&bytes) {
            let init = mp4::read_init(&bytes)?;
            bytes = init.cleared.clone();
            Some(init)
        } else {
            None
        };
        let section = Arc::new(InitSection { bytes, parsed });
        self.inits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, section.clone());
        Ok(section)
    }

    /// Fetches and decrypts one segment: its initialization section, when it has one,
    /// and its bytes.
    async fn fetch_piece(&self, piece: &Piece) -> Result<(Option<Arc<InitSection>>, Vec<u8>), DownloadError> {
        let init = match &piece.map {
            Some(map) => Some(self.init_section(map, piece.sequence).await?),
            None => None,
        };
        let bytes = fetch_bytes(self.http, &piece.url, self.platform, self.headers, piece.range).await?;
        let Some(key_spec) = &piece.key else {
            return Ok((init, bytes.to_vec()));
        };
        let key = self.key(&key_spec.url).await?;
        let iv = key_spec.iv_for(piece.sequence);
        let bytes = match key_spec.method {
            Encryption::Aes128 => decrypt_aes128(&bytes, &key, &iv)?,
            Encryption::SampleAes | Encryption::SampleAesCtr => {
                if mpegts::is_transport_stream(&bytes) {
                    if key_spec.method == Encryption::SampleAesCtr {
                        return Err(DownloadError::Segment(format!(
                            "{} is a transport stream under SAMPLE-AES-CTR, which only fragmented MP4 carries",
                            piece.url
                        )));
                    }
                    mpegts::decrypt_sample_aes(&bytes, &key, &iv)?
                } else if mp4::is_mp4(&bytes) {
                    let parsed = init
                        .as_ref()
                        .and_then(|i| i.parsed.as_ref())
                        .ok_or_else(|| {
                            DownloadError::Segment(format!(
                                "{} is sample-encrypted fragmented MP4 without an initialization section",
                                piece.url
                            ))
                        })?;
                    mp4::decrypt_fragment(&bytes, parsed, &key)?
                } else {
                    return Err(DownloadError::Segment(format!(
                        "{} is neither a transport stream nor fragmented MP4, which SAMPLE-AES needs",
                        piece.url
                    )));
                }
            }
        };
        Ok((init, bytes))
    }
}

/// A subtitle rendition collected as the text of its segments.
struct TextCollector<'a> {
    budget: &'a Budget,
    segments: Vec<String>,
}

#[async_trait]
impl Consumer for TextCollector<'_> {
    async fn take(
        &mut self,
        _key: &PartKey,
        _timing: Timing,
        _init: Option<&InitSection>,
        bytes: &[u8],
    ) -> Result<(), DownloadError> {
        self.budget.take(bytes.len() as u64)?;
        self.segments
            .push(String::from_utf8_lossy(bytes).into_owned());
        Ok(())
    }

    fn gap(&mut self) {}
}

impl Session<'_> {
    /// Follows the media playlist at `url` until it ends: every segment in order into
    /// `consumer`, a live playlist reloaded as the stream goes on until it ends, the
    /// capture limit is reached, or the playlist can no longer be loaded. What happened
    /// to a live stream goes into the notes.
    async fn follow(
        &self,
        url: &Url,
        name: &str,
        consumer: &mut dyn Consumer,
        mut meter: Option<&mut Meter<'_>>,
        mut first: Option<Loaded>,
    ) -> Result<(), DownloadError> {
        let mut next: Option<u64> = None;
        let mut captured = 0.0f64;
        let mut failures = 0u32;
        let mut was_live: Option<bool> = None;
        let max_live = self.max_live.as_secs_f64();
        loop {
            let loaded = match first.take() {
                Some(loaded) => Ok(loaded),
                None => {
                    load_media_playlist(
                        self.http,
                        url,
                        self.platform,
                        self.headers,
                        self.query,
                        self.max_height,
                        2,
                    )
                    .await
                }
            };
            let Loaded { base, playlist, .. } = match loaded {
                Ok(loaded) => {
                    failures = 0;
                    loaded
                }
                Err(error) if was_live == Some(true) => {
                    failures += 1;
                    if failures < RELOAD_FAILURES {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        continue;
                    }
                    self.note(format!(
                        "{name}: reload failed {failures} times ({error}). Recording stopped after {:.0} s.",
                        captured
                    ));
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            let live = !playlist.end_list;
            if was_live.is_none() {
                was_live = Some(live);
                if let Some(meter) = meter.as_deref_mut() {
                    let total = if live {
                        max_live
                    } else {
                        playlist.segments.iter().map(|s| f64::from(s.duration)).sum()
                    };
                    meter.set_total(total);
                }
            }
            let mut pieces = pieces_of(&base, &playlist, self.query, url)?;
            if next.is_none() && live {
                // The stream is captured from now: of what the playlist still offers,
                // only as much as the limit allows, counted back from its end.
                let mut kept = 0.0;
                let mut first = pieces.len();
                for (index, piece) in pieces.iter().enumerate().rev() {
                    if kept + piece.duration > max_live && first < pieces.len() {
                        break;
                    }
                    kept += piece.duration;
                    first = index;
                }
                if first > 0 {
                    self.note(format!(
                        "{name}: {first} earlier segment(s) of the live playlist left out to keep the capture within the limit"
                    ));
                }
                pieces.drain(..first);
            }
            let fresh: Vec<Piece> = pieces
                .into_iter()
                .filter(|p| next.is_none_or(|n| p.sequence >= n))
                .collect();
            if let (Some(n), Some(first)) = (next, fresh.first())
                && first.sequence > n
            {
                self.note(format!(
                    "{name}: {} segment(s) passed out of the live playlist before they could be fetched",
                    first.sequence - n
                ));
                consumer.gap();
            }
            let count = fresh.len();
            let mut cut = false;
            let fetches = futures::stream::iter(fresh.into_iter().map(|piece| async move {
                let result = self.fetch_piece(&piece).await;
                (piece, result)
            }))
            .buffered(CONCURRENCY);
            futures::pin_mut!(fetches);
            while let Some((piece, result)) = fetches.next().await {
                let (init, bytes) = match result {
                    Ok(fetched) => fetched,
                    Err(error) if live => {
                        self.note(format!(
                            "{name}: segment {} of the live stream skipped: {error}",
                            piece.sequence
                        ));
                        consumer.gap();
                        next = Some(piece.sequence + 1);
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                let key = PartKey {
                    run: piece.discontinuity,
                    init: piece.map.as_ref().map(|m| m.id()),
                };
                let timing = Timing {
                    start: captured,
                    origin: 0.0,
                };
                consumer.take(&key, timing, init.as_deref(), &bytes).await?;
                captured += piece.duration;
                next = Some(piece.sequence + 1);
                if let Some(meter) = meter.as_deref_mut() {
                    meter.add(piece.duration);
                }
                if live && captured >= max_live {
                    cut = true;
                    break;
                }
            }
            if cut {
                self.note(format!(
                    "{name}: capture cut at the limit of {:.0} s while the stream goes on",
                    max_live
                ));
                return Ok(());
            }
            if !live {
                if was_live == Some(true) {
                    self.note(format!(
                        "{name}: stream ended. Recorded {:.0} s.",
                        captured
                    ));
                }
                return Ok(());
            }
            let target = Duration::from_secs(playlist.target_duration.max(1));
            let wait = if count > 0 { target } else { target / 2 };
            tokio::time::sleep(wait).await;
        }
    }
}

#[async_trait]
impl Downloader for HlsDownloader {
    fn handles(&self, kind: VariantKind) -> bool {
        kind == VariantKind::Hls
    }

    async fn download(
        &self,
        variant: &Variant,
        dest_dir: &Path,
        context: &DownloadContext,
        progress: ProgressSender,
    ) -> Result<Downloaded, DownloadError> {
        tokio::fs::create_dir_all(dest_dir).await?;
        let headers = &variant.headers;
        let platform = context.platform.as_str();
        let query = &variant.query;
        let _keepalive = variant.keepalive.clone().map(|keepalive| {
            KeepaliveTask::start(
                self.http.clone(),
                keepalive,
                platform.to_string(),
                headers.clone(),
            )
        });
        let budget = Budget::new(context.max_bytes);
        let session = Session {
            http: &self.http,
            platform,
            headers,
            query,
            max_height: context.max_height,
            max_live: context.max_live,
            keys: Mutex::new(HashMap::new()),
            inits: Mutex::new(HashMap::new()),
            notes: Mutex::new(Vec::new()),
        };
        // The first load says what goes with the stream when the link is a master
        // playlist: its audio rendition and subtitle renditions.
        let mut first = load_media_playlist(
            &self.http,
            &variant.url,
            platform,
            headers,
            query,
            context.max_height,
            2,
        )
        .await?;
        let video_url = first.base.clone();
        let audio_url = variant.audio_url.clone().or(first.audio.take());
        let mut subtitle_tracks: Vec<SubtitleTrack> = Vec::new();
        if let Some(choice) = &context.subtitles {
            subtitle_tracks.extend(
                choice
                    .tracks
                    .iter()
                    .filter(|t| t.format == SubtitleFormat::HlsVtt)
                    .cloned(),
            );
            for track in subtitles::pick(&first.subtitles, choice.language.as_deref()) {
                if !subtitle_tracks.iter().any(|t| t.url == track.url) {
                    subtitle_tracks.push(track);
                }
            }
        }
        first.subtitles.clear();
        progress.send_replace(Progress {
            done: 0,
            total: None,
        });
        let mut meter = Meter::new(&progress);

        let mut video = TrackWriter::new(dest_dir, "video", &budget);
        let mut audio = TrackWriter::new(dest_dir, "audio", &budget);
        let mut texts: Vec<TextCollector<'_>> = subtitle_tracks
            .iter()
            .map(|_| TextCollector {
                budget: &budget,
                segments: Vec::new(),
            })
            .collect();
        let video_run = session.follow(&video_url, "video", &mut video, Some(&mut meter), Some(first));
        let audio_run = async {
            match &audio_url {
                Some(url) => session.follow(url, "audio", &mut audio, None, None).await,
                None => Ok(()),
            }
        };
        let text_runs = futures::future::join_all(
            subtitle_tracks
                .iter()
                .zip(texts.iter_mut())
                .map(|(track, collector)| session.follow(&track.url, "subtitles", collector, None, None)),
        );
        let (video_result, audio_result, text_results) =
            tokio::join!(video_run, audio_run, text_runs);
        video_result?;
        audio_result?;
        let (video_parts, start_time) = video.finish().await?;
        let (audio_parts, _) = audio.finish().await?;
        if video_parts.is_empty() {
            return Err(DownloadError::Empty);
        }
        if video_parts.len() > 1 {
            session.note(format!(
                "the stream is discontinuous: {} parts joined",
                video_parts.len()
            ));
        }

        let mut local_subtitles = Vec::new();
        for ((track, collector), result) in subtitle_tracks.iter().zip(texts).zip(text_results) {
            let index = local_subtitles.len();
            if let Err(error) = result {
                session.note(format!(
                    "subtitles {} not fetched: {error}",
                    track.name.as_deref().unwrap_or(&track.language)
                ));
                continue;
            }
            let mut text = String::from("WEBVTT\n\n");
            for segment in &collector.segments {
                text.push_str(&subtitles::hls_vtt_cues(segment, start_time));
                text.push('\n');
            }
            let path = dest_dir.join(subtitles::file_name(track, index));
            tokio::fs::write(&path, &text).await?;
            local_subtitles.push(LocalSubtitle {
                language: track.language.clone(),
                name: track.name.clone(),
                path,
                format: SubtitleFormat::Vtt,
                url: Some(track.url.clone()),
            });
        }

        let dest = dest_dir.join("source.mkv");
        let file = mux_parts(
            &self.ffmpeg,
            &video_parts,
            audio_url.as_ref().map(|_| audio_parts.as_slice()),
            &dest,
        )
        .await?;
        for part in video_parts.iter().chain(audio_parts.iter()) {
            let _ = tokio::fs::remove_file(part).await;
        }
        if file.size > context.max_bytes {
            return Err(DownloadError::TooLarge {
                size: file.size,
                limit: context.max_bytes,
            });
        }
        Ok(Downloaded {
            file,
            subtitles: local_subtitles,
            notes: session.notes.into_inner().unwrap_or_else(|e| e.into_inner()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_iv_with_or_without_prefix() {
        let expected: [u8; 16] = core::array::from_fn(|i| i as u8);
        assert_eq!(
            parse_iv("0x000102030405060708090a0b0c0d0e0f"),
            Some(expected)
        );
        assert_eq!(parse_iv("000102030405060708090A0B0C0D0E0F"), Some(expected));
    }

    #[test]
    fn rejects_wrong_length_or_non_hex() {
        assert_eq!(parse_iv("0x0001"), None);
        assert_eq!(parse_iv("0xzz0102030405060708090a0b0c0d0e0f"), None);
    }

    #[test]
    fn streams_are_chosen_by_height_then_bandwidth() {
        let master = "#EXTM3U\n\
            #EXT-X-STREAM-INF:BANDWIDTH=8000000,RESOLUTION=1920x1080\n1080.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1280x720\n720a.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=4000000,RESOLUTION=1280x720\n720b.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=640x360\n360.m3u8\n";
        let Playlist::MasterPlaylist(master) = m3u8_rs::parse_playlist_res(master.as_bytes()).unwrap() else {
            panic!("master expected");
        };
        assert_eq!(choose_stream(&master, 1080).unwrap().uri, "1080.m3u8");
        assert_eq!(choose_stream(&master, 720).unwrap().uri, "720b.m3u8");
        assert_eq!(choose_stream(&master, 480).unwrap().uri, "360.m3u8");
        assert_eq!(choose_stream(&master, 240).unwrap().uri, "360.m3u8");
    }

    #[test]
    fn keys_name_their_drm() {
        let key = |method: &str, uri: &str, format: Option<&str>| m3u8_rs::Key {
            method: match method {
                "AES-128" => KeyMethod::AES128,
                "SAMPLE-AES" => KeyMethod::SampleAES,
                other => KeyMethod::Other(other.into()),
            },
            uri: Some(uri.into()),
            iv: None,
            keyformat: format.map(str::to_string),
            keyformatversions: None,
        };
        let base = Url::parse("https://cdn.test/v/").unwrap();
        let spec = key_spec(&key("AES-128", "k.bin", None), &base, &[], &base).unwrap().unwrap();
        assert_eq!(spec.method, Encryption::Aes128);
        assert_eq!(spec.url.as_str(), "https://cdn.test/v/k.bin");
        assert_eq!(spec.iv_for(7)[15], 7);
        let spec = key_spec(&key("SAMPLE-AES-CTR", "k.bin", Some("identity")), &base, &[], &base)
            .unwrap()
            .unwrap();
        assert_eq!(spec.method, Encryption::SampleAesCtr);
        assert!(matches!(
            key_spec(&key("SAMPLE-AES", "skd://x", Some("com.apple.streamingkeydelivery")), &base, &[], &base),
            Err(DownloadError::Drm(_, system)) if system == "FairPlay"
        ));
        assert!(matches!(
            key_spec(&key("SAMPLE-AES", "data:x", Some("urn:uuid:edef8ba9-79d6-4a62-a3c8-27dcd51d21ed")), &base, &[], &base),
            Err(DownloadError::Drm(_, system)) if system == "Widevine"
        ));
        assert!(matches!(
            key_spec(&key("ROT13", "k", None), &base, &[], &base),
            Err(DownloadError::Manifest(_))
        ));
    }

    #[test]
    fn pieces_carry_keys_maps_and_discontinuities() {
        let text = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:10\n#EXT-X-DISCONTINUITY-SEQUENCE:3\n\
            #EXT-X-MAP:URI=\"init.mp4\"\n\
            #EXT-X-KEY:METHOD=AES-128,URI=\"k1\",IV=0x000102030405060708090a0b0c0d0e0f\n\
            #EXTINF:4.0,\ns10.m4s\n#EXTINF:4.0,\ns11.m4s\n\
            #EXT-X-DISCONTINUITY\n#EXT-X-MAP:URI=\"init2.mp4\"\n#EXT-X-KEY:METHOD=NONE\n\
            #EXTINF:2.5,\ns12.m4s\n#EXT-X-BYTERANGE:100@50\n#EXTINF:1,\nall.m4s\n#EXT-X-BYTERANGE:200\n#EXTINF:1,\nall.m4s\n";
        let Playlist::MediaPlaylist(media) = m3u8_rs::parse_playlist_res(text.as_bytes()).unwrap() else {
            panic!("media expected");
        };
        let base = Url::parse("https://cdn.test/v/x.m3u8").unwrap();
        let pieces = pieces_of(&base, &media, &[], &base).unwrap();
        assert_eq!(pieces.len(), 5);
        assert_eq!(pieces[0].sequence, 10);
        assert_eq!(pieces[0].discontinuity, 3);
        assert_eq!(pieces[0].map.as_ref().unwrap().url.as_str(), "https://cdn.test/v/init.mp4");
        assert_eq!(pieces[0].key.as_ref().unwrap().url.as_str(), "https://cdn.test/v/k1");
        assert_eq!(pieces[1].key.as_ref().unwrap().iv_for(11)[0], 0);
        assert_eq!(pieces[2].discontinuity, 4);
        assert!(pieces[2].key.is_none());
        assert_eq!(pieces[2].map.as_ref().unwrap().url.as_str(), "https://cdn.test/v/init2.mp4");
        assert_eq!(pieces[3].range, Some((50, 149)));
        assert_eq!(pieces[4].range, Some((150, 349)));
        assert_eq!(pieces[4].sequence, 14);
    }

    #[test]
    fn parts_are_told_by_their_first_bytes() {
        use super::super::segments::part_extension;
        let mut ts = vec![0u8; 188 * 2];
        ts[0] = 0x47;
        ts[188] = 0x47;
        assert_eq!(part_extension(&ts), "ts");
        let mut mp4 = 24u32.to_be_bytes().to_vec();
        mp4.extend_from_slice(b"styp");
        assert_eq!(part_extension(&mp4), "mp4");
        assert_eq!(part_extension(b"ID3\x04"), "aac");
        assert_eq!(part_extension(&[0xff, 0xf1, 0x50]), "aac");
        assert_eq!(part_extension(b"hello"), "bin");
    }

    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::Arc;

    use aes::cipher::BlockModeEncrypt;

    use super::super::SubtitleChoice;
    use crate::http::transport::site::{Reply, Site};

    const BASE: &str = "https://cdn.test/v/";
    const M3U8: &str = "application/vnd.apple.mpegurl";

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("discoclip-hls-{tag}-{}", uuid::Uuid::now_v7()))
    }

    /// Encodes `seconds` of test picture and tone with `extra` arguments ahead of `out`.
    async fn encode(ffmpeg: &Ffmpeg, seconds: u32, video: bool, audio: bool, extra: &[&str], out: &Path) {
        let mut args: Vec<String> = vec!["-loglevel".into(), "error".into()];
        if video {
            args.extend(["-f", "lavfi", "-i", &format!("testsrc=size=64x64:rate=10:duration={seconds}")].map(String::from));
        }
        if audio {
            args.extend(["-f", "lavfi", "-i", &format!("sine=frequency=440:duration={seconds}")].map(String::from));
        }
        if video {
            args.extend(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-g", "10"].map(String::from));
        }
        if audio {
            args.extend(["-c:a", "aac"].map(String::from));
        }
        args.extend(extra.iter().map(|a| a.to_string()));
        let args: Vec<OsString> = args
            .into_iter()
            .map(OsString::from)
            .chain([out.as_os_str().to_owned()])
            .collect();
        ffmpeg.run(args, |_| {}).await.unwrap();
    }

    /// An HLS presentation as ffmpeg packages it: the playlist text and every file it
    /// names, by name.
    struct Packaged {
        playlist: String,
        files: Vec<(String, Vec<u8>)>,
    }

    impl Packaged {
        fn segments(&self) -> Vec<(String, f32)> {
            let Playlist::MediaPlaylist(media) = m3u8_rs::parse_playlist_res(self.playlist.as_bytes()).unwrap() else {
                panic!("media playlist expected");
            };
            media.segments.iter().map(|s| (s.uri.clone(), s.duration)).collect()
        }

        fn serve(&self, site: &Site, name: &str) {
            site.put_text(&format!("{BASE}{name}.m3u8"), M3U8, &self.playlist);
            for (file, bytes) in &self.files {
                site.put_bytes(&format!("{BASE}{file}"), "application/octet-stream", bytes);
            }
        }
    }

    async fn package(ffmpeg: &Ffmpeg, dir: &Path, name: &str, seconds: u32, fmp4: bool, video: bool, audio: bool) -> Packaged {
        tokio::fs::create_dir_all(dir).await.unwrap();
        let segment_type = if fmp4 { "fmp4" } else { "mpegts" };
        let pattern = dir.join(format!("{name}%d.{}", if fmp4 { "m4s" } else { "ts" }));
        let init = format!("{name}.init.mp4");
        let extra = [
            "-f", "hls", "-hls_time", "1", "-hls_playlist_type", "vod", "-hls_segment_type", segment_type,
            "-hls_fmp4_init_filename", &init, "-hls_segment_filename", pattern.to_str().unwrap(),
        ];
        encode(ffmpeg, seconds, video, audio, &extra, &dir.join(format!("{name}.m3u8"))).await;
        let playlist = tokio::fs::read_to_string(dir.join(format!("{name}.m3u8"))).await.unwrap();
        let mut files = Vec::new();
        for line in playlist.lines() {
            let file = if let Some(rest) = line.strip_prefix("#EXT-X-MAP:URI=\"") {
                rest.split('"').next().unwrap().to_string()
            } else if !line.starts_with('#') && !line.trim().is_empty() {
                line.trim().to_string()
            } else {
                continue;
            };
            let bytes = tokio::fs::read(dir.join(&file)).await.unwrap();
            files.push((file, bytes));
        }
        Packaged { playlist, files }
    }

    fn media_playlist(sequence: u64, segments: &[(String, f32)], end: bool, extra_head: &str) -> String {
        let mut text = format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:{sequence}\n{extra_head}");
        for (uri, duration) in segments {
            text.push_str(&format!("#EXTINF:{duration:.3},\n{uri}\n"));
        }
        if end {
            text.push_str("#EXT-X-ENDLIST\n");
        }
        text
    }

    async fn download_hls(site: &Arc<Site>, ffmpeg: &Ffmpeg, name: &str, dir: &Path, context: &DownloadContext) -> (Result<Downloaded, DownloadError>, Progress) {
        let http = Http::with_transport(site.clone(), Http::test_config());
        let downloader = HlsDownloader::new(http, ffmpeg.clone());
        let variant = Variant::new(Url::parse(&format!("{BASE}{name}.m3u8")).unwrap(), VariantKind::Hls);
        let (progress, watched) = tokio::sync::watch::channel(Progress::default());
        let result = downloader.download(&variant, dir, context, progress).await;
        let last = *watched.borrow();
        (result, last)
    }

    async fn probed_duration(ffmpeg: &Ffmpeg, downloaded: &Downloaded) -> f64 {
        assert!(downloaded.file.path.ends_with("source.mkv"));
        let info = ffmpeg.probe(&downloaded.file.path).await.unwrap();
        assert!(info.video.is_some(), "{info:?}");
        info.duration.unwrap().as_secs_f64()
    }

    fn near(actual: f64, expected: f64) -> bool {
        (actual - expected).abs() <= 0.6
    }

    #[tokio::test]
    async fn vod_playlists_download_plain_and_under_aes_128() {
        let dir = temp_dir("vod");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let packaged = package(&ffmpeg, &dir.join("pkg"), "plain", 3, false, true, true).await;
        let site = Site::new();
        packaged.serve(&site, "plain");
        let (result, progress) = download_hls(&site, &ffmpeg, "plain", &dir.join("job"), &DownloadContext::new(50_000_000)).await;
        let downloaded = result.unwrap();
        let duration = probed_duration(&ffmpeg, &downloaded).await;
        assert!(near(duration, 3.0), "{duration}");
        assert!(downloaded.notes.is_empty(), "{:?}", downloaded.notes);
        assert_eq!(progress.done, 3);
        assert_eq!(progress.total, Some(3));
        assert!(!dir.join("job").join("video.0.ts").exists());

        // The same segments encrypted whole with AES-128, the IV from the sequence.
        let key = [0x5au8; 16];
        let mut files = Vec::new();
        for (index, (name, bytes)) in packaged.files.iter().enumerate() {
            let iv = sequence_iv(index as u64);
            let encrypted = cbc::Encryptor::<aes::Aes128>::new(&key.into(), &iv.into())
                .encrypt_padded_vec::<Pkcs7>(bytes);
            files.push((name.clone(), encrypted));
        }
        let mut playlist = String::new();
        for line in packaged.playlist.lines() {
            if line.starts_with("#EXTINF") && !playlist.contains("EXT-X-KEY") {
                playlist.push_str("#EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n");
            }
            playlist.push_str(line);
            playlist.push('\n');
        }
        let locked = Packaged { playlist, files };
        let site = Site::new();
        locked.serve(&site, "locked");
        site.put_bytes(&format!("{BASE}key.bin"), "application/octet-stream", &key);
        let (result, _) = download_hls(&site, &ffmpeg, "locked", &dir.join("locked"), &DownloadContext::new(50_000_000)).await;
        let downloaded = result.unwrap();
        let duration = probed_duration(&ffmpeg, &downloaded).await;
        assert!(near(duration, 3.0), "{duration}");
        assert_eq!(site.hits(&format!("{BASE}key.bin")), 1);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn sample_aes_transport_segments_are_decrypted() {
        let dir = temp_dir("saes-ts");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let packaged = package(&ffmpeg, &dir.join("pkg"), "ts", 3, false, true, true).await;
        let key = [0x11u8; 16];
        let iv: [u8; 16] = core::array::from_fn(|i| i as u8);
        let files: Vec<(String, Vec<u8>)> = packaged
            .files
            .iter()
            .map(|(name, bytes)| (name.clone(), mpegts::encrypt_sample_aes(bytes, &key, &iv).unwrap()))
            .collect();
        let mut playlist = String::new();
        for line in packaged.playlist.lines() {
            if line.starts_with("#EXTINF") && !playlist.contains("EXT-X-KEY") {
                playlist.push_str(&format!(
                    "#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"data:text/plain;base64,{}\",IV=0x{},KEYFORMAT=\"identity\"\n",
                    base64::engine::general_purpose::STANDARD.encode(key),
                    hex::encode(iv)
                ));
            }
            playlist.push_str(line);
            playlist.push('\n');
        }
        let site = Site::new();
        Packaged { playlist, files }.serve(&site, "saes");
        let (result, _) = download_hls(&site, &ffmpeg, "saes", &dir.join("job"), &DownloadContext::new(50_000_000)).await;
        let downloaded = result.unwrap();
        let info = ffmpeg.probe(&downloaded.file.path).await.unwrap();
        assert!(info.audio.is_some(), "{info:?}");
        let duration = probed_duration(&ffmpeg, &downloaded).await;
        assert!(near(duration, 3.0), "{duration}");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn sample_aes_fragmented_segments_are_decrypted() {
        use super::super::mp4::build::{append_within, rename, sinf, tenc};
        let dir = temp_dir("saes-mp4");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let packaged = package(&ffmpeg, &dir.join("pkg"), "f", 3, true, true, true).await;
        let key = [0x77u8; 16];
        let iv = [0x99u8; 16];
        let mut files = Vec::new();
        for (name, bytes) in &packaged.files {
            if name.ends_with(".init.mp4") {
                // Both tracks become protected sample entries with their sinf.
                let mut init = bytes.clone();
                let stsd = |trak: usize, entry: &'static [u8; 4]| {
                    vec![(b"moov", 0usize), (b"trak", trak), (b"mdia", 0), (b"minf", 0), (b"stbl", 0), (b"stsd", 0), (entry, 0)]
                };
                let tenc = tenc(1, 9, 16, None);
                init = append_within(&init, &stsd(0, b"avc1"), &sinf(b"avc1", b"cbcs", &tenc));
                rename(&mut init, &stsd(0, b"avc1"), b"encv");
                init = append_within(&init, &stsd(1, b"mp4a"), &sinf(b"mp4a", b"cbcs", &tenc));
                rename(&mut init, &stsd(1, b"mp4a"), b"enca");
                files.push((name.clone(), init));
            } else {
                let mut segment = mp4::build::encrypt_fragment_cbcs(bytes, &key, &iv, 32);
                // The index box no longer describes the grown fragment. A packager
                // rewrites it, this test drops it.
                if segment.windows(4).any(|w| w == b"sidx") {
                    rename(&mut segment, &[(b"sidx", 0)], b"free");
                }
                files.push((name.clone(), segment));
            }
        }
        let mut playlist = String::new();
        for line in packaged.playlist.lines() {
            if line.starts_with("#EXT-X-MAP") {
                playlist.push_str("#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"key.bin\"\n");
            }
            playlist.push_str(line);
            playlist.push('\n');
        }
        let site = Site::new();
        Packaged { playlist, files }.serve(&site, "fmp4");
        site.put_bytes(&format!("{BASE}key.bin"), "application/octet-stream", &key);
        let (result, _) = download_hls(&site, &ffmpeg, "fmp4", &dir.join("job"), &DownloadContext::new(50_000_000)).await;
        let downloaded = result.unwrap();
        let info = ffmpeg.probe(&downloaded.file.path).await.unwrap();
        assert!(info.audio.is_some(), "{info:?}");
        let duration = probed_duration(&ffmpeg, &downloaded).await;
        assert!(near(duration, 3.0), "{duration}");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn a_live_playlist_is_followed_until_the_stream_ends() {
        let dir = temp_dir("live");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let packaged = package(&ffmpeg, &dir.join("pkg"), "live", 5, false, true, true).await;
        let segments = packaged.segments();
        assert_eq!(segments.len(), 5, "{segments:?}");
        let site = Site::new();
        packaged.serve(&site, "unused");
        site.put_series(
            &format!("{BASE}live.m3u8"),
            vec![
                Reply::new(M3U8, media_playlist(0, &segments[0..2], false, "")),
                Reply::new(M3U8, media_playlist(1, &segments[1..4], false, "")),
                Reply::new(M3U8, media_playlist(2, &segments[2..5], true, "")),
            ],
        );
        let mut context = DownloadContext::new(50_000_000);
        context.max_live = Duration::from_secs(60);
        let (result, progress) = download_hls(&site, &ffmpeg, "live", &dir.join("job"), &context).await;
        let downloaded = result.unwrap();
        let duration = probed_duration(&ffmpeg, &downloaded).await;
        assert!(near(duration, 5.0), "{duration}");
        assert!(downloaded.notes.iter().any(|n| n.contains("the live stream ended")), "{:?}", downloaded.notes);
        assert_eq!(site.hits(&format!("{BASE}live.m3u8")), 3);
        for (uri, _) in &segments {
            assert_eq!(site.hits(&format!("{BASE}{uri}")), 1, "{uri}");
        }
        assert_eq!(progress.total, Some(60));
        assert_eq!(progress.done, 5);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn a_live_capture_is_cut_at_the_limit_and_starts_within_it() {
        let dir = temp_dir("cut");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let packaged = package(&ffmpeg, &dir.join("pkg"), "cut", 5, false, true, true).await;
        let segments = packaged.segments();
        let site = Site::new();
        packaged.serve(&site, "unused");
        // The window keeps growing. The capture stops once 2.5 s are in hand.
        site.put_series(
            &format!("{BASE}cut.m3u8"),
            vec![
                Reply::new(M3U8, media_playlist(0, &segments[0..2], false, "")),
                Reply::new(M3U8, media_playlist(1, &segments[1..4], false, "")),
                Reply::new(M3U8, media_playlist(2, &segments[2..5], false, "")),
            ],
        );
        let mut context = DownloadContext::new(50_000_000);
        context.max_live = Duration::from_millis(2_500);
        let (result, _) = download_hls(&site, &ffmpeg, "cut", &dir.join("job"), &context).await;
        let downloaded = result.unwrap();
        let duration = probed_duration(&ffmpeg, &downloaded).await;
        assert!(near(duration, 3.0), "{duration}");
        assert!(downloaded.notes.iter().any(|n| n.contains("capture cut at the limit")), "{:?}", downloaded.notes);
        assert_eq!(site.hits(&format!("{BASE}{}", segments[4].0)), 0);

        // A live window longer than the limit is taken from its end.
        let site = Site::new();
        packaged.serve(&site, "unused");
        site.put_series(
            &format!("{BASE}tail.m3u8"),
            vec![
                Reply::new(M3U8, media_playlist(0, &segments, false, "")),
                Reply::new(M3U8, media_playlist(0, &segments, true, "")),
            ],
        );
        let (result, _) = download_hls(&site, &ffmpeg, "tail", &dir.join("tail"), &context).await;
        let downloaded = result.unwrap();
        let duration = probed_duration(&ffmpeg, &downloaded).await;
        assert!(near(duration, 2.0), "{duration}");
        assert!(downloaded.notes.iter().any(|n| n.contains("3 earlier segment(s)")), "{:?}", downloaded.notes);
        assert_eq!(site.hits(&format!("{BASE}{}", segments[0].0)), 0);
        assert_eq!(site.hits(&format!("{BASE}{}", segments[3].0)), 1);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn a_live_stream_whose_playlist_goes_away_ends_with_what_was_captured() {
        let dir = temp_dir("gone");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let packaged = package(&ffmpeg, &dir.join("pkg"), "gone", 3, false, true, true).await;
        let segments = packaged.segments();
        let site = Site::new();
        packaged.serve(&site, "unused");
        site.put_series(
            &format!("{BASE}gone.m3u8"),
            vec![
                Reply::new(M3U8, media_playlist(0, &segments[0..2], false, "")),
                Reply::new(M3U8, "gone").status(404),
            ],
        );
        let (result, _) = download_hls(&site, &ffmpeg, "gone", &dir.join("job"), &DownloadContext::new(50_000_000)).await;
        let downloaded = result.unwrap();
        let duration = probed_duration(&ffmpeg, &downloaded).await;
        assert!(near(duration, 2.0), "{duration}");
        assert!(downloaded.notes.iter().any(|n| n.contains("could not be reloaded 3 times")), "{:?}", downloaded.notes);
        assert_eq!(site.hits(&format!("{BASE}gone.m3u8")), 4);

        // A missing segment of a live stream is skipped with a note. Of a recording it
        // fails the download.
        let site = Site::new();
        packaged.serve(&site, "unused");
        site.remove(&format!("{BASE}{}", segments[1].0));
        site.put_series(
            &format!("{BASE}hole.m3u8"),
            vec![
                Reply::new(M3U8, media_playlist(0, &segments[0..3], false, "")),
                Reply::new(M3U8, media_playlist(0, &segments[0..3], true, "")),
            ],
        );
        let (result, _) = download_hls(&site, &ffmpeg, "hole", &dir.join("hole"), &DownloadContext::new(50_000_000)).await;
        let downloaded = result.unwrap();
        assert!(downloaded.notes.iter().any(|n| n.contains("segment 1 of the live stream skipped")), "{:?}", downloaded.notes);
        assert!(downloaded.notes.iter().any(|n| n.contains("2 parts joined")), "{:?}", downloaded.notes);
        site.put_text(&format!("{BASE}vodhole.m3u8"), M3U8, &media_playlist(0, &segments[0..3], true, ""));
        let (result, _) = download_hls(&site, &ffmpeg, "vodhole", &dir.join("vodhole"), &DownloadContext::new(50_000_000)).await;
        assert!(matches!(result.unwrap_err(), DownloadError::Status { status: 404, .. }));
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn discontinuities_split_the_stream_into_parts_that_are_joined() {
        let dir = temp_dir("disc");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let first = package(&ffmpeg, &dir.join("a"), "a", 2, false, true, true).await;
        let second = package(&ffmpeg, &dir.join("b"), "b", 2, false, true, true).await;
        let site = Site::new();
        first.serve(&site, "a");
        second.serve(&site, "b");
        let mut segments = first.segments();
        let joined_at = segments.len();
        segments.extend(second.segments());
        let mut playlist = String::from("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n");
        for (index, (uri, duration)) in segments.iter().enumerate() {
            if index == joined_at {
                playlist.push_str("#EXT-X-DISCONTINUITY\n");
            }
            playlist.push_str(&format!("#EXTINF:{duration:.3},\n{uri}\n"));
        }
        playlist.push_str("#EXT-X-ENDLIST\n");
        site.put_text(&format!("{BASE}disc.m3u8"), M3U8, &playlist);
        let (result, _) = download_hls(&site, &ffmpeg, "disc", &dir.join("job"), &DownloadContext::new(50_000_000)).await;
        let downloaded = result.unwrap();
        let duration = probed_duration(&ffmpeg, &downloaded).await;
        assert!(near(duration, 4.0), "{duration}");
        assert!(downloaded.notes.iter().any(|n| n == "the stream is discontinuous: 2 parts joined"), "{:?}", downloaded.notes);
        assert!(!dir.join("job").join("video.parts.txt").exists());
        assert!(!dir.join("job").join("video.1.ts").exists());
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn a_master_playlist_brings_its_audio_and_subtitle_renditions() {
        let dir = temp_dir("master");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let video = package(&ffmpeg, &dir.join("v"), "video", 3, false, true, false).await;
        let audio = package(&ffmpeg, &dir.join("a"), "audio", 3, false, false, true).await;
        let site = Site::new();
        video.serve(&site, "video");
        audio.serve(&site, "audio");
        let master = "#EXTM3U\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",DEFAULT=YES,URI=\"audio.m3u8\"\n\
            #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"subs.m3u8\"\n\
            #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"Deutsch\",LANGUAGE=\"de\",URI=\"subs-de.m3u8\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=500000,RESOLUTION=64x64,CODECS=\"avc1.42e00a,mp4a.40.2\",AUDIO=\"aud\",SUBTITLES=\"subs\"\n\
            video.m3u8\n";
        site.put_text(&format!("{BASE}master.m3u8"), M3U8, master);
        site.put_text(
            &format!("{BASE}subs.m3u8"),
            M3U8,
            "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:1.5,\ns0.vtt\n#EXTINF:1.5,\ns1.vtt\n#EXT-X-ENDLIST\n",
        );
        // The map says local zero is MPEG-TS time 2.4 s.
        site.put_text(
            &format!("{BASE}s0.vtt"),
            "text/vtt",
            "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:216000,LOCAL:00:00:00.000\n\n00:00:00.000 --> 00:00:01.000\nOne\n",
        );
        site.put_text(
            &format!("{BASE}s1.vtt"),
            "text/vtt",
            "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:216000,LOCAL:00:00:00.000\n\n00:00:01.500 --> 00:00:02.500\nTwo\n",
        );
        let mut context = DownloadContext::new(50_000_000);
        context.subtitles = Some(SubtitleChoice {
            tracks: vec![SubtitleTrack {
                url: Url::parse(&format!("{BASE}subs.m3u8")).unwrap(),
                language: "en".into(),
                name: Some("English".into()),
                format: SubtitleFormat::HlsVtt,
                auto: false,
                headers: Vec::new(),
            }],
            language: Some("en".into()),
        });
        let (result, _) = download_hls(&site, &ffmpeg, "master", &dir.join("job"), &context).await;
        let downloaded = result.unwrap();
        let info = ffmpeg.probe(&downloaded.file.path).await.unwrap();
        assert!(info.video.is_some(), "{info:?}");
        assert!(info.audio.is_some(), "{info:?}");
        assert!(near(info.duration.unwrap().as_secs_f64(), 3.0), "{info:?}");
        // One track: the one the job named, which is also the master's English one.
        // the German rendition is not the preferred language.
        assert_eq!(downloaded.subtitles.len(), 1, "{:?}", downloaded.subtitles);
        let track = &downloaded.subtitles[0];
        assert_eq!(track.language, "en");
        assert_eq!(track.format, SubtitleFormat::Vtt);
        assert_eq!(track.url.as_ref().unwrap().as_str(), format!("{BASE}subs.m3u8"));
        assert_eq!(site.hits(&format!("{BASE}subs-de.m3u8")), 0);
        let first_segment = &video.files[0].1;
        let media_start = mpegts::start_time(first_segment).unwrap() as f64 / 90_000.0;
        let shift = 2.4 - media_start;
        let text = tokio::fs::read_to_string(&track.path).await.unwrap();
        let expected_one = format!("{} --> {}\nOne\n", format_time(shift), format_time(1.0 + shift));
        let expected_two = format!("{} --> {}\nTwo\n", format_time(1.5 + shift), format_time(2.5 + shift));
        assert!(text.starts_with("WEBVTT\n\n"), "{text}");
        assert!(text.contains(&expected_one), "{text}\nexpected {expected_one}");
        assert!(text.contains(&expected_two), "{text}\nexpected {expected_two}");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    fn format_time(seconds: f64) -> String {
        let ms = (seconds.max(0.0) * 1000.0).round() as u64;
        format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, (ms / 60_000) % 60, (ms / 1000) % 60, ms % 1000)
    }

    #[tokio::test]
    async fn drm_keys_are_reported_not_fetched() {
        let dir = temp_dir("drm");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let site = Site::new();
        site.put_text(
            &format!("{BASE}drm.m3u8"),
            M3U8,
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://asset\",KEYFORMAT=\"com.apple.streamingkeydelivery\",KEYFORMATVERSIONS=\"1\"\n#EXTINF:4.0,\ns0.ts\n#EXT-X-ENDLIST\n",
        );
        let (result, _) = download_hls(&site, &ffmpeg, "drm", &dir.join("job"), &DownloadContext::new(50_000_000)).await;
        assert!(matches!(result.unwrap_err(), DownloadError::Drm(_, system) if system == "FairPlay"));
        assert_eq!(site.hits(&format!("{BASE}s0.ts")), 0);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
