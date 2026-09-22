//! Download Smooth Streaming video, audio and text fragments. Generate movie headers from
//! codec data and set fragment decode times from the manifest.
//!
//! Follow live manifests until completion, capture limit or failure. Reject declared DRM,
//! including PlayReady.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use url::Url;

use super::segments::{
    Budget, Consumer, InitSection, Meter, PartKey, Timing, TrackWriter, fetch_bytes,
    fetch_text, mux_parts,
};
use super::{
    DownloadContext, DownloadError, Downloaded, Downloader, LocalSubtitle, mp4, subtitles,
};
use crate::event::{Progress, ProgressSender};
use crate::ffmpeg::Ffmpeg;
use crate::http::Http;
use crate::resolve::util::xml;
use crate::resolve::{SubtitleFormat, SubtitleTrack, Variant, VariantKind};

const MAX_MANIFEST: usize = 8 * 1024 * 1024;
const CONCURRENCY: usize = 4;
/// Reloads of a live manifest that may fail in a row before the stream counts as over.
const RELOAD_FAILURES: u32 = 3;
const DEFAULT_TIMESCALE: u64 = 10_000_000;

pub struct IsmDownloader {
    http: Http,
    ffmpeg: Ffmpeg,
}

impl IsmDownloader {
    pub fn new(http: Http, ffmpeg: Ffmpeg) -> Self {
        Self { http, ffmpeg }
    }
}

fn manifest_error(message: impl Into<String>) -> DownloadError {
    DownloadError::Manifest(message.into())
}


// The manifest

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StreamKind {
    Video,
    Audio,
    Text,
}

/// One quality level: a bitrate of a stream, with the codec data its fragments need.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Level {
    index: u32,
    bitrate: u64,
    four_cc: String,
    width: u32,
    height: u32,
    /// `CodecPrivateData`, decoded from hex: parameter sets for video, the decoder
    /// configuration for audio.
    private: Vec<u8>,
    sampling_rate: u32,
    channels: u16,
    bits_per_sample: u16,
    audio_tag: Option<u16>,
    /// Bytes of length prefix before each NAL unit in video samples.
    nal_length: u8,
    /// Every attribute, for the `{CustomAttributes}` of a fragment URL.
    attributes: Vec<(String, String)>,
}

/// A `StreamIndex`: one kind of media with its quality levels and the chunks it is cut
/// into.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stream {
    kind: StreamKind,
    name: String,
    language: Option<String>,
    /// The URL template with `{bitrate}` and `{start time}` to fill in.
    url: String,
    timescale: u64,
    levels: Vec<Level>,
    /// Each chunk's start and length, in the stream's timescale.
    chunks: Vec<(u64, u64)>,
}

#[derive(Debug, Clone, PartialEq)]
struct Manifest {
    duration: Option<f64>,
    live: bool,
    /// The DRM system the manifest declares.
    protection: Option<String>,
    streams: Vec<Stream>,
}

fn attr_u64(node: roxmltree::Node<'_, '_>, name: &str) -> Option<u64> {
    node.attribute(name).and_then(|v| v.trim().parse().ok())
}

fn attr_u32(node: roxmltree::Node<'_, '_>, name: &str) -> Option<u32> {
    node.attribute(name).and_then(|v| v.trim().parse().ok())
}

fn decode_hex(text: &str) -> Result<Vec<u8>, DownloadError> {
    let text: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    hex::decode(&text).map_err(|e| manifest_error(format!("CodecPrivateData is not hex: {e}")))
}

/// The DRM system a `ProtectionHeader` names.
fn protection_system(system_id: Option<&str>) -> String {
    match system_id.map(|id| id.trim().to_ascii_lowercase()).as_deref() {
        Some("9a04f079-9840-4286-ab92-e65be0885f95") | None => "playready".into(),
        Some("edef8ba9-79d6-4ace-a3c8-27dcd51d21ed") => "widevine".into(),
        Some(other) => other.to_string(),
    }
}

fn parse_manifest(text: &str) -> Result<Manifest, DownloadError> {
    let document = xml(text).ok_or_else(|| manifest_error("invalid Smooth Streaming manifest"))?;
    let root = document.root_element();
    if root.tag_name().name() != "SmoothStreamingMedia" {
        return Err(manifest_error(format!(
            "not a Smooth Streaming manifest: <{}>",
            root.tag_name().name()
        )));
    }
    let timescale = attr_u64(root, "TimeScale").filter(|t| *t > 0).unwrap_or(DEFAULT_TIMESCALE);
    let duration = attr_u64(root, "Duration")
        .filter(|d| *d > 0)
        .map(|ticks| ticks as f64 / timescale as f64);
    let live = root
        .attribute("IsLive")
        .is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let protection = root
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "Protection")
        .map(|protection| {
            protection_system(
                protection
                    .children()
                    .find(|n| n.is_element() && n.tag_name().name() == "ProtectionHeader")
                    .and_then(|h| h.attribute("SystemID")),
            )
        });
    let mut streams = Vec::new();
    for node in root
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "StreamIndex")
    {
        let kind = match node.attribute("Type").map(|t| t.to_ascii_lowercase()).as_deref() {
            Some("video") => StreamKind::Video,
            Some("audio") => StreamKind::Audio,
            Some("text") => StreamKind::Text,
            _ => continue,
        };
        let stream_timescale = attr_u64(node, "TimeScale").filter(|t| *t > 0).unwrap_or(timescale);
        let mut levels = Vec::new();
        for (position, level) in node
            .children()
            .filter(|n| n.is_element() && n.tag_name().name() == "QualityLevel")
            .enumerate()
        {
            let attributes: Vec<(String, String)> = level
                .children()
                .find(|n| n.is_element() && n.tag_name().name() == "CustomAttributes")
                .map(|custom| {
                    custom
                        .children()
                        .filter(|n| n.is_element() && n.tag_name().name() == "Attribute")
                        .filter_map(|a| {
                            Some((a.attribute("Name")?.to_string(), a.attribute("Value")?.to_string()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            levels.push(Level {
                index: attr_u32(level, "Index").unwrap_or(position as u32),
                bitrate: attr_u64(level, "Bitrate").unwrap_or(0),
                four_cc: level.attribute("FourCC").unwrap_or("").trim().to_ascii_uppercase(),
                width: attr_u32(level, "MaxWidth")
                    .or_else(|| attr_u32(level, "Width"))
                    .unwrap_or(0),
                height: attr_u32(level, "MaxHeight")
                    .or_else(|| attr_u32(level, "Height"))
                    .unwrap_or(0),
                private: decode_hex(level.attribute("CodecPrivateData").unwrap_or(""))?,
                sampling_rate: attr_u32(level, "SamplingRate").unwrap_or(0),
                channels: attr_u32(level, "Channels").unwrap_or(2) as u16,
                bits_per_sample: attr_u32(level, "BitsPerSample").unwrap_or(16) as u16,
                audio_tag: attr_u32(level, "AudioTag").map(|t| t as u16),
                nal_length: attr_u32(level, "NALUnitLengthField")
                    .filter(|n| matches!(n, 1 | 2 | 4))
                    .unwrap_or(4) as u8,
                attributes,
            });
        }
        // Chunks: `t` is the start when given, else the previous chunk's end. `d` the
        // length. `r` how many chunks of that length follow one another.
        let entries: Vec<roxmltree::Node<'_, '_>> = node
            .children()
            .filter(|n| n.is_element() && n.tag_name().name() == "c")
            .collect();
        let mut chunks: Vec<(u64, u64)> = Vec::new();
        let mut cursor = 0u64;
        for (index, c) in entries.iter().enumerate() {
            let start = attr_u64(*c, "t").unwrap_or(cursor);
            let duration = match attr_u64(*c, "d") {
                Some(d) => d,
                None => {
                    // A chunk without a length runs to the next one, or to the end.
                    let next = entries
                        .get(index + 1)
                        .and_then(|n| attr_u64(*n, "t"));
                    let end = next.or_else(|| {
                        duration.map(|d| (d * stream_timescale as f64) as u64)
                    });
                    match end {
                        Some(end) if end > start => end - start,
                        _ => return Err(manifest_error("chunk without a length")),
                    }
                }
            };
            let repeats = attr_u64(*c, "r").unwrap_or(1).max(1);
            let mut at = start;
            for _ in 0..repeats {
                chunks.push((at, duration));
                at += duration;
            }
            cursor = at;
        }
        streams.push(Stream {
            kind,
            name: node.attribute("Name").unwrap_or("").to_string(),
            language: node
                .attribute("Language")
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .map(str::to_string),
            url: node.attribute("Url").unwrap_or("").to_string(),
            timescale: stream_timescale,
            levels,
            chunks,
        });
    }
    Ok(Manifest {
        duration,
        live,
        protection,
        streams,
    })
}

/// The URL of the fragment of `level` starting at `time`.
fn fragment_url(manifest: &Url, stream: &Stream, level: &Level, time: u64) -> Result<Url, DownloadError> {
    let attributes = level
        .attributes
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(",");
    let filled = stream
        .url
        .replace("{bitrate}", &level.bitrate.to_string())
        .replace("{Bitrate}", &level.bitrate.to_string())
        .replace("{start time}", &time.to_string())
        .replace("{start_time}", &time.to_string())
        .replace("{CustomAttributes}", &attributes);
    manifest
        .join(&filled)
        .map_err(|e| manifest_error(format!("bad fragment URL {filled}: {e}")))
}


// Boxes

fn plain_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}

fn full_box(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
    let mut inner = vec![version];
    inner.extend_from_slice(&flags.to_be_bytes()[1..]);
    inner.extend_from_slice(body);
    plain_box(kind, &inner)
}

/// A movie box for one stream of `level`, so the fragments that follow it demux as a
/// plain fragmented MP4.
fn init_segment(stream: &Stream, level: &Level, track_id: u32) -> Result<Vec<u8>, DownloadError> {
    let timescale = u32::try_from(stream.timescale).unwrap_or(u32::MAX);
    let (handler, name, header_box, entry) = match stream.kind {
        StreamKind::Video => (
            *b"vide",
            "VideoHandler",
            full_box(b"vmhd", 0, 1, &[0u8; 8]),
            video_entry(level)?,
        ),
        StreamKind::Audio => (
            *b"soun",
            "SoundHandler",
            full_box(b"smhd", 0, 0, &[0u8; 4]),
            audio_entry(level)?,
        ),
        StreamKind::Text => {
            return Err(DownloadError::Unsupported(
                "text streams are collected as documents, not muxed".into(),
            ));
        }
    };
    let matrix: [u8; 36] = {
        let mut m = [0u8; 36];
        m[0..4].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        m[16..20].copy_from_slice(&0x0001_0000u32.to_be_bytes());
        m[32..36].copy_from_slice(&0x4000_0000u32.to_be_bytes());
        m
    };
    let mut mvhd = Vec::new();
    mvhd.extend_from_slice(&[0u8; 8]); // creation and modification time
    mvhd.extend_from_slice(&timescale.to_be_bytes());
    mvhd.extend_from_slice(&0u32.to_be_bytes()); // duration: fragments say
    mvhd.extend_from_slice(&0x0001_0000u32.to_be_bytes()); // rate
    mvhd.extend_from_slice(&0x0100u16.to_be_bytes()); // volume
    mvhd.extend_from_slice(&[0u8; 10]);
    mvhd.extend_from_slice(&matrix);
    mvhd.extend_from_slice(&[0u8; 24]);
    mvhd.extend_from_slice(&(track_id + 1).to_be_bytes());

    let mut tkhd = Vec::new();
    tkhd.extend_from_slice(&[0u8; 8]);
    tkhd.extend_from_slice(&track_id.to_be_bytes());
    tkhd.extend_from_slice(&[0u8; 4]);
    tkhd.extend_from_slice(&0u32.to_be_bytes()); // duration
    tkhd.extend_from_slice(&[0u8; 8]);
    tkhd.extend_from_slice(&0u16.to_be_bytes()); // layer
    tkhd.extend_from_slice(&0u16.to_be_bytes()); // alternate group
    tkhd.extend_from_slice(
        &(if stream.kind == StreamKind::Audio { 0x0100u16 } else { 0 }).to_be_bytes(),
    );
    tkhd.extend_from_slice(&[0u8; 2]);
    tkhd.extend_from_slice(&matrix);
    let (width, height) = if stream.kind == StreamKind::Video {
        (level.width, level.height)
    } else {
        (0, 0)
    };
    tkhd.extend_from_slice(&(width << 16).to_be_bytes());
    tkhd.extend_from_slice(&(height << 16).to_be_bytes());

    let mut mdhd = Vec::new();
    mdhd.extend_from_slice(&[0u8; 8]);
    mdhd.extend_from_slice(&timescale.to_be_bytes());
    mdhd.extend_from_slice(&0u32.to_be_bytes());
    mdhd.extend_from_slice(&language_code(stream.language.as_deref()).to_be_bytes());
    mdhd.extend_from_slice(&[0u8; 2]);

    let mut hdlr = vec![0u8; 4];
    hdlr.extend_from_slice(&handler);
    hdlr.extend_from_slice(&[0u8; 12]);
    hdlr.extend_from_slice(name.as_bytes());
    hdlr.push(0);

    let dref = {
        let mut body = 1u32.to_be_bytes().to_vec();
        body.extend_from_slice(&full_box(b"url ", 0, 1, &[]));
        full_box(b"dref", 0, 0, &body)
    };
    let dinf = plain_box(b"dinf", &dref);
    let stsd = {
        let mut body = 1u32.to_be_bytes().to_vec();
        body.extend_from_slice(&entry);
        full_box(b"stsd", 0, 0, &body)
    };
    let mut stbl = stsd;
    stbl.extend_from_slice(&full_box(b"stts", 0, 0, &0u32.to_be_bytes()));
    stbl.extend_from_slice(&full_box(b"stsc", 0, 0, &0u32.to_be_bytes()));
    stbl.extend_from_slice(&full_box(b"stsz", 0, 0, &[0u8; 8]));
    stbl.extend_from_slice(&full_box(b"stco", 0, 0, &0u32.to_be_bytes()));
    let mut minf = header_box;
    minf.extend_from_slice(&dinf);
    minf.extend_from_slice(&plain_box(b"stbl", &stbl));
    let mut mdia = full_box(b"mdhd", 0, 0, &mdhd);
    mdia.extend_from_slice(&full_box(b"hdlr", 0, 0, &hdlr));
    mdia.extend_from_slice(&plain_box(b"minf", &minf));
    let mut trak = full_box(b"tkhd", 0, 7, &tkhd);
    trak.extend_from_slice(&plain_box(b"mdia", &mdia));
    let mut trex = track_id.to_be_bytes().to_vec();
    trex.extend_from_slice(&1u32.to_be_bytes());
    trex.extend_from_slice(&[0u8; 12]);
    let mut moov = full_box(b"mvhd", 0, 0, &mvhd);
    moov.extend_from_slice(&plain_box(b"trak", &trak));
    moov.extend_from_slice(&plain_box(b"mvex", &full_box(b"trex", 0, 0, &trex)));
    let mut out = plain_box(b"ftyp", b"isom\0\0\x02\0isomiso6piffmp41");
    out.extend_from_slice(&plain_box(b"moov", &moov));
    Ok(out)
}

/// An ISO 639-2 language packed as `mdhd` carries it. `und` for anything else.
fn language_code(language: Option<&str>) -> u16 {
    let code = language
        .map(|l| l.trim().to_ascii_lowercase())
        .filter(|l| l.len() == 3 && l.bytes().all(|b| b.is_ascii_lowercase()))
        .unwrap_or_else(|| "und".into());
    code.bytes()
        .fold(0u16, |packed, b| (packed << 5) | u16::from(b - 0x60))
}

/// The NAL units of Annex B data: what follows each start code.
fn annex_b_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut units = Vec::new();
    for (index, start) in starts.iter().enumerate() {
        let mut end = starts.get(index + 1).map_or(data.len(), |next| next - 3);
        while end > *start && data[end - 1] == 0 {
            end -= 1;
        }
        if end > *start {
            units.push(&data[*start..end]);
        }
    }
    units
}

/// A visual sample entry for the level's codec.
fn video_entry(level: &Level) -> Result<Vec<u8>, DownloadError> {
    let (kind, config) = match level.four_cc.as_str() {
        "H264" | "AVC1" | "X264" | "DAVC" => (*b"avc1", plain_box(b"avcC", &avcc(level)?)),
        "HEVC" | "HVC1" | "HEV1" | "H265" => (*b"hvc1", plain_box(b"hvcC", &hvcc(level)?)),
        other => {
            return Err(DownloadError::Unsupported(format!(
                "Smooth Streaming video codec {other} cannot be remuxed"
            )));
        }
    };
    let mut body = vec![0u8; 6];
    body.extend_from_slice(&1u16.to_be_bytes()); // data reference index
    body.extend_from_slice(&[0u8; 16]);
    body.extend_from_slice(&(level.width.min(0xFFFF) as u16).to_be_bytes());
    body.extend_from_slice(&(level.height.min(0xFFFF) as u16).to_be_bytes());
    body.extend_from_slice(&0x0048_0000u32.to_be_bytes());
    body.extend_from_slice(&0x0048_0000u32.to_be_bytes());
    body.extend_from_slice(&[0u8; 4]);
    body.extend_from_slice(&1u16.to_be_bytes()); // frame count
    body.extend_from_slice(&[0u8; 32]); // compressor name
    body.extend_from_slice(&0x0018u16.to_be_bytes()); // depth
    body.extend_from_slice(&0xFFFFu16.to_be_bytes());
    body.extend_from_slice(&config);
    Ok(plain_box(&kind, &body))
}

/// An `avcC` record from the level's SPS and PPS.
fn avcc(level: &Level) -> Result<Vec<u8>, DownloadError> {
    let units = annex_b_units(&level.private);
    let sps: Vec<&[u8]> = units.iter().copied().filter(|u| u[0] & 0x1f == 7).collect();
    let pps: Vec<&[u8]> = units.iter().copied().filter(|u| u[0] & 0x1f == 8).collect();
    let first = sps
        .first()
        .filter(|s| s.len() >= 4)
        .ok_or_else(|| manifest_error("H264 level without a sequence parameter set"))?;
    if pps.is_empty() {
        return Err(manifest_error("H264 level without a picture parameter set"));
    }
    let mut out = vec![1, first[1], first[2], first[3], 0xFC | (level.nal_length - 1)];
    out.push(0xE0 | sps.len() as u8);
    for unit in &sps {
        out.extend_from_slice(&(unit.len() as u16).to_be_bytes());
        out.extend_from_slice(unit);
    }
    out.push(pps.len() as u8);
    for unit in &pps {
        out.extend_from_slice(&(unit.len() as u16).to_be_bytes());
        out.extend_from_slice(unit);
    }
    Ok(out)
}

/// Bits of an RBSP, emulation prevention bytes removed.
struct Bits {
    data: Vec<u8>,
    pos: usize,
}

impl Bits {
    fn new(nal: &[u8]) -> Self {
        let mut data = Vec::with_capacity(nal.len());
        let mut zeros = 0;
        for &b in nal {
            if zeros >= 2 && b == 3 {
                zeros = 0;
                continue;
            }
            data.push(b);
            zeros = if b == 0 { zeros + 1 } else { 0 };
        }
        Self { data, pos: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let bit = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(u32::from(bit))
    }

    fn bits(&mut self, count: usize) -> Option<u64> {
        let mut out = 0u64;
        for _ in 0..count {
            out = (out << 1) | u64::from(self.bit()?);
        }
        Some(out)
    }

    fn skip(&mut self, count: usize) -> Option<()> {
        self.pos += count;
        (self.pos <= self.data.len() * 8).then_some(())
    }

    /// An Exp-Golomb coded unsigned number.
    fn ue(&mut self) -> Option<u64> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros > 32 {
                return None;
            }
        }
        Some((1u64 << zeros) - 1 + self.bits(zeros)?)
    }
}

/// An `hvcC` record from the level's VPS, SPS and PPS.
fn hvcc(level: &Level) -> Result<Vec<u8>, DownloadError> {
    let units = annex_b_units(&level.private);
    let of = |kind: u8| -> Vec<&[u8]> {
        units
            .iter()
            .copied()
            .filter(|u| u.len() > 2 && (u[0] >> 1) & 0x3f == kind)
            .collect()
    };
    let (vps, sps, pps) = (of(32), of(33), of(34));
    let first = sps
        .first()
        .ok_or_else(|| manifest_error("HEVC level without a sequence parameter set"))?;
    if pps.is_empty() {
        return Err(manifest_error("HEVC level without a picture parameter set"));
    }
    let short = || manifest_error("HEVC sequence parameter set cut short");
    let mut bits = Bits::new(&first[2..]);
    bits.skip(4).ok_or_else(short)?; // sps_video_parameter_set_id
    let max_sub_layers_minus1 = bits.bits(3).ok_or_else(short)? as usize;
    let temporal_id_nesting = bits.bit().ok_or_else(short)? as u8;
    let profile_byte = bits.bits(8).ok_or_else(short)? as u8;
    let compatibility = bits.bits(32).ok_or_else(short)? as u32;
    let constraints = bits.bits(48).ok_or_else(short)?;
    let level_idc = bits.bits(8).ok_or_else(short)? as u8;
    let mut present = Vec::new();
    for _ in 0..max_sub_layers_minus1 {
        let profile = bits.bit().ok_or_else(short)? == 1;
        let level = bits.bit().ok_or_else(short)? == 1;
        present.push((profile, level));
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            bits.skip(2).ok_or_else(short)?;
        }
    }
    for (profile, level) in present {
        if profile {
            bits.skip(88).ok_or_else(short)?;
        }
        if level {
            bits.skip(8).ok_or_else(short)?;
        }
    }
    bits.ue().ok_or_else(short)?; // sps_seq_parameter_set_id
    let chroma_format = bits.ue().ok_or_else(short)?;
    if chroma_format == 3 {
        bits.skip(1).ok_or_else(short)?;
    }
    bits.ue().ok_or_else(short)?; // width
    bits.ue().ok_or_else(short)?; // height
    if bits.bit().ok_or_else(short)? == 1 {
        for _ in 0..4 {
            bits.ue().ok_or_else(short)?;
        }
    }
    let luma = bits.ue().ok_or_else(short)?;
    let chroma = bits.ue().ok_or_else(short)?;
    let mut out = vec![1, profile_byte];
    out.extend_from_slice(&compatibility.to_be_bytes());
    out.extend_from_slice(&constraints.to_be_bytes()[2..]);
    out.push(level_idc);
    out.extend_from_slice(&[0xF0, 0x00, 0xFC]);
    out.push(0xFC | (chroma_format as u8 & 3));
    out.push(0xF8 | (luma as u8 & 7));
    out.push(0xF8 | (chroma as u8 & 7));
    out.extend_from_slice(&[0, 0]);
    out.push(((max_sub_layers_minus1 as u8 + 1) << 3) | (temporal_id_nesting << 2) | (level.nal_length - 1));
    let arrays: Vec<(u8, &Vec<&[u8]>)> = [(32u8, &vps), (33, &sps), (34, &pps)]
        .into_iter()
        .filter(|(_, units)| !units.is_empty())
        .collect();
    out.push(arrays.len() as u8);
    for (kind, units) in arrays {
        out.push(0x80 | kind);
        out.extend_from_slice(&(units.len() as u16).to_be_bytes());
        for unit in units {
            out.extend_from_slice(&(unit.len() as u16).to_be_bytes());
            out.extend_from_slice(unit);
        }
    }
    Ok(out)
}

/// An audio sample entry for the level's codec.
fn audio_entry(level: &Level) -> Result<Vec<u8>, DownloadError> {
    let tag = level.audio_tag;
    let four_cc = level.four_cc.as_str();
    let (kind, config) = if matches!(four_cc, "AACL" | "AACH" | "AACP" | "AAC") || (four_cc.is_empty() && tag == Some(255)) {
        let specific = if level.private.is_empty() {
            audio_specific_config(level)?
        } else {
            level.private.clone()
        };
        (*b"mp4a", esds(0x40, &specific, level.bitrate))
    } else if four_cc == "MP3" || (four_cc.is_empty() && tag == Some(85)) {
        (*b"mp4a", esds(0x6B, &[], level.bitrate))
    } else if matches!(four_cc, "AC-3" | "AC3" | "DD") {
        (*b"ac-3", if level.private.is_empty() { Vec::new() } else { plain_box(b"dac3", &level.private) })
    } else if matches!(four_cc, "EC-3" | "EC3" | "DDP") {
        (*b"ec-3", if level.private.is_empty() { Vec::new() } else { plain_box(b"dec3", &level.private) })
    } else {
        return Err(DownloadError::Unsupported(format!(
            "Smooth Streaming audio codec {} cannot be remuxed",
            if four_cc.is_empty() { format!("tag {}", tag.unwrap_or(0)) } else { four_cc.to_string() }
        )));
    };
    let mut body = vec![0u8; 6];
    body.extend_from_slice(&1u16.to_be_bytes());
    body.extend_from_slice(&[0u8; 8]);
    body.extend_from_slice(&level.channels.max(1).to_be_bytes());
    body.extend_from_slice(&level.bits_per_sample.max(16).to_be_bytes());
    body.extend_from_slice(&[0u8; 4]);
    body.extend_from_slice(&(level.sampling_rate.min(0xFFFF) << 16).to_be_bytes());
    body.extend_from_slice(&config);
    Ok(plain_box(&kind, &body))
}

/// An AAC-LC AudioSpecificConfig from the level's sampling rate and channels, for a level
/// that carries none.
fn audio_specific_config(level: &Level) -> Result<Vec<u8>, DownloadError> {
    const RATES: [u32; 13] = [
        96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
    ];
    let index = RATES
        .iter()
        .position(|r| *r == level.sampling_rate)
        .ok_or_else(|| manifest_error(format!("AAC sampling rate {} has no index", level.sampling_rate)))?
        as u16;
    let channels = level.channels.min(7);
    let packed: u16 = (2 << 11) | (index << 7) | (channels << 3);
    Ok(packed.to_be_bytes().to_vec())
}

/// An `esds` box for an MPEG-4 audio object of `object_type` with `specific` decoder
/// configuration.
fn esds(object_type: u8, specific: &[u8], bitrate: u64) -> Vec<u8> {
    fn descriptor(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        let mut length = body.len();
        let mut sizes = vec![(length & 0x7f) as u8];
        length >>= 7;
        while length > 0 {
            sizes.push(0x80 | (length & 0x7f) as u8);
            length >>= 7;
        }
        sizes.reverse();
        out.extend_from_slice(&sizes);
        out.extend_from_slice(body);
        out
    }
    let bitrate = u32::try_from(bitrate).unwrap_or(u32::MAX);
    let mut decoder = vec![object_type, 0x15, 0, 0, 0];
    decoder.extend_from_slice(&bitrate.to_be_bytes());
    decoder.extend_from_slice(&bitrate.to_be_bytes());
    if !specific.is_empty() {
        decoder.extend_from_slice(&descriptor(0x05, specific));
    }
    let mut es = vec![0, 1, 0];
    es.extend_from_slice(&descriptor(0x04, &decoder));
    es.extend_from_slice(&descriptor(0x06, &[0x02]));
    full_box(b"esds", 0, 0, &descriptor(0x03, &es))
}

/// The fragment with a `tfdt` of `time` in each track fragment that has none, so its
/// samples are placed where the manifest says rather than run on from zero.
fn with_decode_time(fragment: &[u8], time: u64) -> Result<Vec<u8>, DownloadError> {
    fn size_at(data: &[u8], at: usize) -> Option<usize> {
        Some(u32::from_be_bytes(data.get(at..at + 4)?.try_into().ok()?) as usize)
    }
    let bad = || DownloadError::Segment("Smooth Streaming fragment has malformed boxes".into());
    let mut out = Vec::with_capacity(fragment.len() + 32);
    let mut pos = 0;
    while pos + 8 <= fragment.len() {
        let size = size_at(fragment, pos).ok_or_else(bad)?;
        if size < 8 || pos + size > fragment.len() {
            return Err(bad());
        }
        let kind = &fragment[pos + 4..pos + 8];
        if kind != b"moof" {
            out.extend_from_slice(&fragment[pos..pos + size]);
            pos += size;
            continue;
        }
        let mut moof_body = Vec::with_capacity(size);
        let mut inner = pos + 8;
        let moof_end = pos + size;
        while inner + 8 <= moof_end {
            let child_size = size_at(fragment, inner).ok_or_else(bad)?;
            if child_size < 8 || inner + child_size > moof_end {
                return Err(bad());
            }
            let child_kind = &fragment[inner + 4..inner + 8];
            if child_kind != b"traf" {
                moof_body.extend_from_slice(&fragment[inner..inner + child_size]);
                inner += child_size;
                continue;
            }
            let traf_end = inner + child_size;
            let mut parts: Vec<&[u8]> = Vec::new();
            let mut at = inner + 8;
            while at + 8 <= traf_end {
                let part_size = size_at(fragment, at).ok_or_else(bad)?;
                if part_size < 8 || at + part_size > traf_end {
                    return Err(bad());
                }
                parts.push(&fragment[at..at + part_size]);
                at += part_size;
            }
            let has_tfdt = parts.iter().any(|p| &p[4..8] == b"tfdt");
            let mut traf_body = Vec::with_capacity(child_size + 20);
            for part in parts {
                if &part[4..8] == b"trun" && !has_tfdt && part.len() >= 16 && part[11] & 1 == 1 {
                    // The data offset counts from the start of the moof, which grows.
                    let mut trun = part.to_vec();
                    let offset = i32::from_be_bytes(trun[16..20].try_into().unwrap());
                    trun[16..20].copy_from_slice(&(offset + 20).to_be_bytes());
                    traf_body.extend_from_slice(&trun);
                } else {
                    traf_body.extend_from_slice(part);
                }
                if &part[4..8] == b"tfhd" && !has_tfdt {
                    traf_body.extend_from_slice(&full_box(b"tfdt", 1, 0, &time.to_be_bytes()));
                }
            }
            moof_body.extend_from_slice(&plain_box(b"traf", &traf_body));
            inner = traf_end;
        }
        out.extend_from_slice(&plain_box(b"moof", &moof_body));
        pos = moof_end;
    }
    Ok(out)
}

/// The TTML documents a text fragment carries: the contents of its `mdat` boxes.
fn text_documents(fragment: &[u8]) -> Vec<String> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 8 <= fragment.len() {
        let size = u32::from_be_bytes(fragment[pos..pos + 4].try_into().unwrap()) as usize;
        if size < 8 || pos + size > fragment.len() {
            break;
        }
        if &fragment[pos + 4..pos + 8] == b"mdat" {
            let text = String::from_utf8_lossy(&fragment[pos + 8..pos + size]);
            let text = text.trim_matches(char::from(0)).trim();
            if !text.is_empty() {
                out.push(text.to_string());
            }
        }
        pos += size;
    }
    out
}


// Choosing and fetching

/// Which streams and levels the download wants.
struct Wanted {
    level: Option<String>,
    audio_only: bool,
    max_height: u32,
    language: Option<String>,
}

/// The level id a variant names, as the resolver wrote it.
fn wanted_level(variant: &Variant) -> Option<String> {
    variant.format_id.clone()
}

fn best_video<'a>(manifest: &'a Manifest, wanted: &Wanted) -> Option<(&'a Stream, &'a Level)> {
    let candidates: Vec<(&Stream, &Level)> = manifest
        .streams
        .iter()
        .filter(|s| s.kind == StreamKind::Video)
        .flat_map(|s| s.levels.iter().map(move |l| (s, l)))
        .collect();
    if let Some(id) = &wanted.level
        && let Some(found) = candidates
            .iter()
            .find(|(_, l)| format!("video-{}", l.index) == *id)
    {
        return Some(*found);
    }
    candidates.into_iter().max_by_key(|(_, l)| {
        let fits = l.height <= wanted.max_height;
        (
            fits,
            if fits { i64::from(l.height) } else { -i64::from(l.height) },
            l.bitrate,
        )
    })
}

fn best_audio<'a>(manifest: &'a Manifest, wanted: &Wanted) -> Option<(&'a Stream, &'a Level)> {
    manifest
        .streams
        .iter()
        .filter(|s| s.kind == StreamKind::Audio)
        .flat_map(|s| s.levels.iter().map(move |l| (s, l)))
        .max_by_key(|(s, l)| {
            let preferred = wanted
                .language
                .as_deref()
                .is_some_and(|w| s.language.as_deref().is_some_and(|have| have.eq_ignore_ascii_case(w)));
            (preferred, l.bitrate)
        })
}

/// A fragment to fetch.
#[derive(Debug, Clone)]
struct Piece {
    /// Its start in the stream's timescale, which orders it and names it.
    time: u64,
    start: f64,
    duration: f64,
    url: Url,
}

fn pieces_of(manifest_url: &Url, stream: &Stream, level: &Level) -> Result<Vec<Piece>, DownloadError> {
    stream
        .chunks
        .iter()
        .map(|(time, duration)| {
            Ok(Piece {
                time: *time,
                start: *time as f64 / stream.timescale as f64,
                duration: *duration as f64 / stream.timescale as f64,
                url: fragment_url(manifest_url, stream, level, *time)?,
            })
        })
        .collect()
}

/// The text streams a manifest offers, one per language, as subtitle tracks.
fn text_tracks(manifest_url: &Url, manifest: &Manifest, headers: &[(String, String)]) -> Vec<SubtitleTrack> {
    let mut tracks: Vec<SubtitleTrack> = Vec::new();
    for stream in manifest.streams.iter().filter(|s| s.kind == StreamKind::Text) {
        let Some(level) = stream.levels.first() else {
            continue;
        };
        if !matches!(level.four_cc.as_str(), "TTML" | "DFXP" | "") {
            continue;
        }
        let language = stream.language.clone().unwrap_or_else(|| "und".into());
        if tracks.iter().any(|t| t.language.eq_ignore_ascii_case(&language)) {
            continue;
        }
        let mut url = manifest_url.clone();
        url.set_fragment(Some(&format!("text-{language}")));
        tracks.push(SubtitleTrack {
            url,
            language,
            name: (!stream.name.is_empty()).then(|| stream.name.clone()),
            format: SubtitleFormat::Ttml,
            auto: false,
            headers: headers.to_vec(),
        });
    }
    tracks
}

/// A text track collected as documents as its fragments arrive.
struct TextSink<'a> {
    budget: &'a Budget,
    documents: Vec<String>,
}

#[async_trait]
impl Consumer for TextSink<'_> {
    async fn take(
        &mut self,
        _key: &PartKey,
        _timing: Timing,
        _init: Option<&InitSection>,
        bytes: &[u8],
    ) -> Result<(), DownloadError> {
        self.budget.take(bytes.len() as u64)?;
        self.documents.extend(text_documents(bytes));
        Ok(())
    }

    fn gap(&mut self) {}
}

/// How far along a track is: the last fragment taken and the media time captured.
#[derive(Debug, Default)]
struct Cursor {
    last: Option<u64>,
    captured: f64,
    first_start: Option<f64>,
}

impl Cursor {
    fn wants(&self, piece: &Piece) -> bool {
        self.last.is_none_or(|last| piece.time > last)
    }
}

/// What every manifest load of one download shares.
struct Session<'a> {
    http: &'a Http,
    platform: &'a str,
    headers: &'a [(String, String)],
    max_live: Duration,
    notes: Mutex<Vec<String>>,
}

impl Session<'_> {
    fn note(&self, message: String) {
        tracing::info!("{message}");
        self.notes.lock().unwrap_or_else(|e| e.into_inner()).push(message);
    }

    /// Fetches `pieces` in order into `consumer`, each given its decode time when it is
    /// media. For a `live` stream skipping one that cannot be fetched. Whether the
    /// capture reached its limit.
    #[allow(clippy::too_many_arguments)]
    async fn fetch_track(
        &self,
        name: &str,
        pieces: Vec<Piece>,
        live: bool,
        init: Option<&InitSection>,
        consumer: &mut dyn Consumer,
        cursor: &mut Cursor,
        mut meter: Option<&mut Meter<'_>>,
    ) -> Result<bool, DownloadError> {
        let max_live = self.max_live.as_secs_f64();
        let fetches = futures::stream::iter(pieces.into_iter().map(|piece| async move {
            let result = fetch_bytes(self.http, &piece.url, self.platform, self.headers, None).await;
            (piece, result)
        }))
        .buffered(CONCURRENCY);
        futures::pin_mut!(fetches);
        while let Some((piece, result)) = fetches.next().await {
            let bytes = match result {
                Ok(bytes) => bytes,
                Err(error) if live => {
                    self.note(format!("{name}: fragment at {:.1} s skipped: {error}", piece.start));
                    consumer.gap();
                    cursor.last = Some(piece.time);
                    continue;
                }
                Err(error) => return Err(error),
            };
            let bytes = if init.is_some() {
                with_decode_time(&bytes, piece.time)?
            } else {
                bytes.to_vec()
            };
            let key = PartKey {
                run: 0,
                init: Some(name.to_string()),
            };
            let timing = Timing {
                start: piece.start,
                origin: 0.0,
            };
            consumer.take(&key, timing, init, &bytes).await?;
            cursor.last = Some(piece.time);
            cursor.captured += piece.duration;
            if cursor.first_start.is_none() {
                cursor.first_start = Some(piece.start);
            }
            if let Some(meter) = meter.as_deref_mut() {
                meter.add(piece.duration);
            }
            if live && cursor.captured >= max_live {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

async fn load_manifest(
    http: &Http,
    url: &Url,
    platform: &str,
    headers: &[(String, String)],
) -> Result<Manifest, DownloadError> {
    let (_, text) = fetch_text(http, url, platform, headers, MAX_MANIFEST).await?;
    parse_manifest(&text)
}

fn init_section(stream: &Stream, level: &Level) -> Result<InitSection, DownloadError> {
    let bytes = init_segment(stream, level, 1)?;
    let parsed = mp4::read_init(&bytes)?;
    Ok(InitSection {
        bytes,
        parsed: Some(parsed),
    })
}

#[async_trait]
impl Downloader for IsmDownloader {
    fn handles(&self, kind: VariantKind) -> bool {
        kind == VariantKind::Ism
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
        let manifest_url = &variant.url;
        let budget = Budget::new(context.max_bytes);
        let session = Session {
            http: &self.http,
            platform,
            headers,
            max_live: context.max_live,
            notes: Mutex::new(Vec::new()),
        };
        let wanted = Wanted {
            level: wanted_level(variant),
            audio_only: variant.audio_only,
            max_height: context.max_height,
            language: variant.language.clone(),
        };
        let mut loaded = Some(load_manifest(&self.http, manifest_url, platform, headers).await?);
        let first = loaded.as_ref().unwrap();
        if let Some(system) = &first.protection {
            return Err(DownloadError::Drm(manifest_url.to_string(), system.clone()));
        }
        let text_choices: Vec<SubtitleTrack> = match &context.subtitles {
            Some(choice) => subtitles::pick(
                &text_tracks(manifest_url, first, headers),
                choice.language.as_deref(),
            ),
            None => Vec::new(),
        };
        progress.send_replace(Progress {
            done: 0,
            total: None,
        });
        let mut meter = Meter::new(&progress);

        let mut primary = TrackWriter::new(dest_dir, "video", &budget);
        let mut audio = TrackWriter::new(dest_dir, "audio", &budget);
        let mut texts: Vec<TextSink<'_>> = text_choices
            .iter()
            .map(|_| TextSink {
                budget: &budget,
                documents: Vec::new(),
            })
            .collect();
        let mut cursors = (Cursor::default(), Cursor::default(), Vec::<Cursor>::new());
        cursors.2.resize_with(text_choices.len(), Cursor::default);
        let mut inits: HashMap<String, InitSection> = HashMap::new();
        let mut failures = 0u32;
        let mut was_live: Option<bool> = None;
        let mut has_audio = false;
        let max_live = context.max_live.as_secs_f64();
        loop {
            let manifest = match loaded.take() {
                Some(manifest) => manifest,
                None => match load_manifest(&self.http, manifest_url, platform, headers).await {
                    Ok(manifest) => {
                        failures = 0;
                        manifest
                    }
                    Err(error) if was_live == Some(true) => {
                        failures += 1;
                        if failures < RELOAD_FAILURES {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            continue;
                        }
                        session.note(format!(
                            "Manifest reload failed {failures} times ({error}). Recording stopped after {:.0} s.",
                            cursors.0.captured
                        ));
                        break;
                    }
                    Err(error) => return Err(error),
                },
            };
            if let Some(system) = &manifest.protection {
                return Err(DownloadError::Drm(manifest_url.to_string(), system.clone()));
            }
            let live = manifest.live;
            if was_live.is_none() {
                was_live = Some(live);
                meter.set_total(if live {
                    max_live
                } else {
                    manifest.duration.unwrap_or_else(|| {
                        manifest
                            .streams
                            .iter()
                            .map(|s| s.chunks.iter().map(|(_, d)| *d as f64 / s.timescale as f64).sum::<f64>())
                            .fold(0.0, f64::max)
                    })
                });
            }
            let video = if wanted.audio_only {
                None
            } else {
                best_video(&manifest, &wanted)
            };
            let (primary_choice, audio_choice) = match video {
                Some(video) => (video, best_audio(&manifest, &wanted)),
                None => (
                    best_audio(&manifest, &wanted).ok_or_else(|| {
                        manifest_error("the manifest has no video or audio stream")
                    })?,
                    None,
                ),
            };
            if !inits.contains_key("video") {
                inits.insert("video".into(), init_section(primary_choice.0, primary_choice.1)?);
            }
            if let Some((stream, level)) = audio_choice
                && !inits.contains_key("audio")
            {
                inits.insert("audio".into(), init_section(stream, level)?);
            }
            let mut primary_pieces = pieces_of(manifest_url, primary_choice.0, primary_choice.1)?;
            let mut audio_pieces = match audio_choice {
                Some((stream, level)) => {
                    has_audio = true;
                    pieces_of(manifest_url, stream, level)?
                }
                None => Vec::new(),
            };
            let mut text_pieces: Vec<Vec<Piece>> = Vec::new();
            for track in &text_choices {
                let stream = manifest
                    .streams
                    .iter()
                    .find(|s| {
                        s.kind == StreamKind::Text
                            && s.language.as_deref().unwrap_or("und").eq_ignore_ascii_case(&track.language)
                    });
                text_pieces.push(match stream.and_then(|s| s.levels.first().map(|l| (s, l))) {
                    Some((stream, level)) => pieces_of(manifest_url, stream, level)?,
                    None => Vec::new(),
                });
            }
            // The stream is captured from now: of what a live manifest still offers, only
            // as much as the limit allows, counted back from its end.
            let trim = |pieces: &mut Vec<Piece>, note: bool| {
                let mut kept = 0.0;
                let mut first = pieces.len();
                for (index, piece) in pieces.iter().enumerate().rev() {
                    if kept + piece.duration > max_live && first < pieces.len() {
                        break;
                    }
                    kept += piece.duration;
                    first = index;
                }
                if first > 0 && note {
                    session.note(format!(
                        "{first} earlier fragment(s) of the live stream left out to keep the capture within the limit"
                    ));
                }
                pieces.drain(..first);
            };
            if live && cursors.0.last.is_none() {
                trim(&mut primary_pieces, true);
                trim(&mut audio_pieces, false);
                for pieces in &mut text_pieces {
                    trim(pieces, false);
                }
            }
            let fresh = |pieces: Vec<Piece>, cursor: &Cursor| -> Vec<Piece> {
                pieces.into_iter().filter(|p| cursor.wants(p)).collect()
            };
            let primary_fresh = fresh(primary_pieces, &cursors.0);
            let audio_fresh = fresh(audio_pieces, &cursors.1);
            let text_fresh: Vec<Vec<Piece>> = text_pieces
                .into_iter()
                .zip(&cursors.2)
                .map(|(pieces, cursor)| fresh(pieces, cursor))
                .collect();
            let new_count = primary_fresh.len();
            let last_duration = primary_choice
                .0
                .chunks
                .last()
                .map(|(_, d)| *d as f64 / primary_choice.0.timescale as f64)
                .unwrap_or(2.0);
            let primary_run = session.fetch_track(
                "video",
                primary_fresh,
                live,
                inits.get("video"),
                &mut primary,
                &mut cursors.0,
                Some(&mut meter),
            );
            let audio_run = session.fetch_track(
                "audio",
                audio_fresh,
                live,
                inits.get("audio"),
                &mut audio,
                &mut cursors.1,
                None,
            );
            let text_runs = futures::future::join_all(
                text_fresh
                    .into_iter()
                    .zip(texts.iter_mut())
                    .zip(cursors.2.iter_mut())
                    .map(|((pieces, sink), cursor)| {
                        session.fetch_track("subtitles", pieces, live, None, sink, cursor, None)
                    }),
            );
            let (cut, audio_result, text_results) = tokio::join!(primary_run, audio_run, text_runs);
            let cut = cut?;
            audio_result?;
            for result in text_results {
                if let Err(error) = result {
                    session.note(format!("subtitles not fetched: {error}"));
                }
            }
            if cut {
                session.note(format!(
                    "capture cut at the limit of {max_live:.0} s while the stream goes on"
                ));
                break;
            }
            if !live {
                if was_live == Some(true) {
                    session.note(format!(
                        "Stream ended. Recorded {:.0} s.",
                        cursors.0.captured
                    ));
                }
                break;
            }
            let wait = Duration::from_secs_f64(last_duration.clamp(1.0, 10.0));
            tokio::time::sleep(if new_count > 0 { wait } else { wait / 2 }).await;
        }

        let (primary_parts, _) = primary.finish().await?;
        let (audio_parts, _) = audio.finish().await?;
        if primary_parts.is_empty() {
            return Err(DownloadError::Empty);
        }
        if primary_parts.len() > 1 {
            session.note(format!(
                "the recording is joined from {} parts",
                primary_parts.len()
            ));
        }
        let media_start = cursors.0.first_start.unwrap_or(0.0);
        let mut local_subtitles = Vec::new();
        for (index, (track, sink)) in text_choices.iter().zip(texts).enumerate() {
            let shifted: Vec<String> = sink
                .documents
                .iter()
                .map(|d| subtitles::shift_ttml(d, -media_start))
                .collect();
            let text = subtitles::merge_ttml(&shifted);
            if text.trim().is_empty() {
                continue;
            }
            let path = dest_dir.join(subtitles::file_name(track, index));
            tokio::fs::write(&path, &text).await?;
            local_subtitles.push(LocalSubtitle {
                language: track.language.clone(),
                name: track.name.clone(),
                path,
                format: SubtitleFormat::Ttml,
                url: Some(track.url.clone()),
            });
        }

        let dest = dest_dir.join("source.mkv");
        let file = mux_parts(
            &self.ffmpeg,
            &primary_parts,
            has_audio.then_some(audio_parts.as_slice()),
            &dest,
        )
        .await?;
        for part in primary_parts.iter().chain(audio_parts.iter()) {
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
    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::super::SubtitleChoice;
    use super::*;
    use crate::http::transport::site::{Reply, Site};

    const BASE: &str = "https://cdn.test/s/";
    const MANIFEST_TYPE: &str = "application/vnd.ms-sstr+xml";

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("discoclip-ism-{tag}-{}", uuid::Uuid::now_v7()))
    }

    async fn run_ffmpeg(ffmpeg: &Ffmpeg, args: &[String]) {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        ffmpeg.run(args, |_| {}).await.unwrap();
    }

    /// A presentation as ffmpeg's Smooth Streaming muxer writes it: the fragments of each
    /// stream by start time, and the manifest rewritten to name those times, since the
    /// muxer's own chunk list leaves them out.
    struct Packaged {
        manifest: String,
        /// `QualityLevels(...)/Fragments(kind=time)` paths and their bytes.
        files: Vec<(String, Vec<u8>)>,
    }

    impl Packaged {
        fn serve(&self, site: &Site, prefix: &str) {
            for (name, bytes) in &self.files {
                site.put_bytes(&format!("{BASE}{prefix}{name}"), "video/mp4", bytes);
            }
        }

        fn video_times(&self) -> Vec<u64> {
            let mut times: Vec<u64> = self
                .files
                .iter()
                .filter_map(|(name, _)| name.split("video=").nth(1))
                .filter_map(|rest| rest.trim_end_matches(')').parse().ok())
                .collect();
            times.sort_unstable();
            times
        }
    }

    /// Packages `seconds` of test picture and tone as Smooth Streaming with one-second
    /// fragments.
    async fn package(ffmpeg: &Ffmpeg, dir: &Path, seconds: u32) -> Packaged {
        tokio::fs::create_dir_all(dir).await.unwrap();
        let out = dir.join("pkg");
        let args: Vec<String> = [
            "-loglevel", "error", "-f", "lavfi", "-i", &format!("testsrc=size=64x64:rate=10:duration={seconds}"),
            "-f", "lavfi", "-i", &format!("sine=frequency=440:duration={seconds}"),
            "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-g", "10", "-c:a", "aac",
            "-f", "smoothstreaming", "-min_frag_duration", "1000000", out.to_str().unwrap(),
        ]
        .map(String::from)
        .to_vec();
        run_ffmpeg(ffmpeg, &args).await;
        let mut files = Vec::new();
        let mut levels = tokio::fs::read_dir(&out).await.unwrap();
        while let Some(level) = levels.next_entry().await.unwrap() {
            if !level.file_type().await.unwrap().is_dir() {
                continue;
            }
            let level_name = level.file_name().to_string_lossy().into_owned();
            let mut entries = tokio::fs::read_dir(level.path()).await.unwrap();
            while let Some(entry) = entries.next_entry().await.unwrap() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with("Fragments(") {
                    files.push((format!("{level_name}/{name}"), tokio::fs::read(entry.path()).await.unwrap()));
                }
            }
        }
        let raw = tokio::fs::read_to_string(out.join("Manifest")).await.unwrap();
        let manifest = with_chunk_times(&raw, &files);
        Packaged { manifest, files }
    }

    /// The manifest with each stream's chunk list replaced by the start times its
    /// fragments are named for, and lengths running from one to the next.
    fn with_chunk_times(manifest: &str, files: &[(String, Vec<u8>)]) -> String {
        let duration: u64 = manifest
            .split("Duration=\"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .and_then(|d| d.parse().ok())
            .unwrap();
        let mut out = String::new();
        let mut kind: Option<&str> = None;
        for line in manifest.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("<StreamIndex") {
                kind = if trimmed.contains("Type=\"video\"") { Some("video") } else { Some("audio") };
                out.push_str(line);
                out.push('\n');
                continue;
            }
            if trimmed.starts_with("<c ") {
                let Some(current) = kind.take() else {
                    continue;
                };
                let mut times: Vec<u64> = files
                    .iter()
                    .filter_map(|(name, _)| name.split(&format!("{current}=")).nth(1))
                    .filter_map(|rest| rest.trim_end_matches(')').parse().ok())
                    .collect();
                times.sort_unstable();
                for (index, time) in times.iter().enumerate() {
                    let end = times.get(index + 1).copied().unwrap_or(duration.max(*time + 1));
                    out.push_str(&format!("<c t=\"{time}\" d=\"{}\" />\n", end - time));
                }
                continue;
            }
            if trimmed.starts_with("<c ") || (kind.is_none() && trimmed.starts_with("<c")) {
                continue;
            }
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    async fn download_ism(
        site: &Arc<Site>,
        ffmpeg: &Ffmpeg,
        name: &str,
        dir: &Path,
        context: &DownloadContext,
        format_id: Option<&str>,
    ) -> (Result<Downloaded, DownloadError>, Progress) {
        let http = Http::with_transport(site.clone(), Http::test_config());
        let downloader = IsmDownloader::new(http, ffmpeg.clone());
        let mut variant = Variant::new(Url::parse(&format!("{BASE}{name}.ism/Manifest")).unwrap(), VariantKind::Ism);
        variant.format_id = format_id.map(str::to_string);
        let (progress, watched) = tokio::sync::watch::channel(Progress::default());
        let result = downloader.download(&variant, dir, context, progress).await;
        let last = *watched.borrow();
        (result, last)
    }

    fn near(actual: f64, expected: f64) -> bool {
        (actual - expected).abs() <= 0.6
    }

    const HAND_WRITTEN: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<SmoothStreamingMedia MajorVersion="2" MinorVersion="2" Duration="60000000" TimeScale="10000000">
  <StreamIndex Type="video" Name="video" Chunks="3" QualityLevels="2" Url="QualityLevels({bitrate})/Fragments(video={start time},{CustomAttributes})">
    <QualityLevel Index="0" Bitrate="2962000" FourCC="H264" MaxWidth="1280" MaxHeight="720" CodecPrivateData="000000016742c00ada109b011000000300100000030140f1226a0000000168ce0fc8">
      <CustomAttributes><Attribute Name="hardwareProfile" Value="10000"/></CustomAttributes>
    </QualityLevel>
    <QualityLevel Index="1" Bitrate="1000000" FourCC="H264" MaxWidth="640" MaxHeight="360" CodecPrivateData="000000016742c00ada109b011000000300100000030140f1226a0000000168ce0fc8"/>
    <c t="0" d="20000000" r="2"/>
    <c d="20000000"/>
  </StreamIndex>
  <StreamIndex Type="audio" Language="eng" Chunks="1" QualityLevels="1" Url="QualityLevels({bitrate})/Fragments(audio={start time})">
    <QualityLevel Index="0" Bitrate="128000" FourCC="AACL" SamplingRate="48000" Channels="2" AudioTag="255" CodecPrivateData=""/>
    <c t="0" d="60000000"/>
  </StreamIndex>
  <StreamIndex Type="text" Language="deu" Subtype="CAPT" QualityLevels="1" Url="QualityLevels({bitrate})/Fragments(textstream_deu={start time})">
    <QualityLevel Index="0" Bitrate="1000" FourCC="TTML"/>
    <c t="0" d="60000000"/>
  </StreamIndex>
</SmoothStreamingMedia>"#;

    #[test]
    fn manifests_are_read_into_streams_and_chunks() {
        let manifest = parse_manifest(HAND_WRITTEN).unwrap();
        assert_eq!(manifest.duration, Some(6.0));
        assert!(!manifest.live);
        assert!(manifest.protection.is_none());
        assert_eq!(manifest.streams.len(), 3);
        let video = &manifest.streams[0];
        assert_eq!(video.kind, StreamKind::Video);
        assert_eq!(video.levels.len(), 2);
        assert_eq!(video.levels[0].width, 1280);
        assert_eq!(video.levels[0].attributes, vec![("hardwareProfile".to_string(), "10000".to_string())]);
        assert_eq!(video.chunks, vec![(0, 20_000_000), (20_000_000, 20_000_000), (40_000_000, 20_000_000)]);
        let url = fragment_url(
            &Url::parse("https://h/p/v.ism/Manifest").unwrap(),
            video,
            &video.levels[0],
            20_000_000,
        )
        .unwrap();
        assert_eq!(
            url.as_str(),
            "https://h/p/v.ism/QualityLevels(2962000)/Fragments(video=20000000,hardwareProfile=10000)"
        );
        let audio = &manifest.streams[1];
        assert_eq!(audio.language.as_deref(), Some("eng"));
        assert_eq!(audio.levels[0].channels, 2);
        assert!(audio.levels[0].private.is_empty());
        let text = &manifest.streams[2];
        assert_eq!(text.kind, StreamKind::Text);
        let tracks = text_tracks(&Url::parse("https://h/v.ism/Manifest").unwrap(), &manifest, &[]);
        assert_eq!(tracks.len(), 1);
        assert_eq!(tracks[0].language, "deu");
        assert_eq!(tracks[0].format, SubtitleFormat::Ttml);

        let live = HAND_WRITTEN
            .replace("TimeScale=\"10000000\">", "TimeScale=\"10000000\" IsLive=\"TRUE\" DVRWindowLength=\"300000000\"><Protection><ProtectionHeader SystemID=\"9A04F079-9840-4286-AB92-E65BE0885F95\">AAAA</ProtectionHeader></Protection>");
        let manifest = parse_manifest(&live).unwrap();
        assert!(manifest.live);
        assert_eq!(manifest.protection.as_deref(), Some("playready"));
        assert!(parse_manifest("<MPD/>").is_err());
        assert!(parse_manifest("nonsense").is_err());
        // A last chunk without a length runs to the end. Without an end it cannot.
        let open_ended = parse_manifest(&HAND_WRITTEN.replace("<c d=\"20000000\"/>", "<c/>")).unwrap();
        assert_eq!(open_ended.streams[0].chunks[2], (40_000_000, 20_000_000));
        assert!(matches!(
            parse_manifest(
                &HAND_WRITTEN
                    .replace("<c d=\"20000000\"/>", "<c/>")
                    .replace("Duration=\"60000000\" ", "")
            )
            .unwrap_err(),
            DownloadError::Manifest(_)
        ));
    }

    #[test]
    fn headers_are_built_for_each_codec() {
        let manifest = parse_manifest(HAND_WRITTEN).unwrap();
        let video = &manifest.streams[0];
        let record = avcc(&video.levels[0]).unwrap();
        assert_eq!(&record[..6], &[1, 0x42, 0xc0, 0x0a, 0xff, 0xe1]);
        let sps_len = u16::from_be_bytes([record[6], record[7]]) as usize;
        assert_eq!(record[8] & 0x1f, 7);
        assert_eq!(record[8 + sps_len], 1);
        assert_eq!(record[8 + sps_len + 3] & 0x1f, 8);
        let init = init_segment(video, &video.levels[0], 1).unwrap();
        let parsed = mp4::read_init(&init).unwrap();
        assert_eq!(parsed.tracks.len(), 1);
        assert_eq!(parsed.tracks[0].id, 1);
        assert_eq!(parsed.tracks[0].timescale, 10_000_000);
        assert!(parsed.tracks[0].protection.is_none());
        assert!(init.windows(4).any(|w| w == b"avc1"));

        let audio = &manifest.streams[1];
        assert_eq!(audio_specific_config(&audio.levels[0]).unwrap(), vec![0x11, 0x90]);
        let entry = audio_entry(&audio.levels[0]).unwrap();
        assert_eq!(&entry[4..8], b"mp4a");
        assert!(entry.windows(4).any(|w| w == b"esds"));
        let init = init_segment(audio, &audio.levels[0], 1).unwrap();
        assert_eq!(mp4::read_init(&init).unwrap().tracks[0].timescale, 10_000_000);
        let mut mp3 = audio.levels[0].clone();
        mp3.four_cc = String::new();
        mp3.audio_tag = Some(85);
        assert_eq!(&audio_entry(&mp3).unwrap()[4..8], b"mp4a");
        let mut ec3 = audio.levels[0].clone();
        ec3.four_cc = "EC-3".into();
        ec3.private = vec![0x0c, 0x00, 0x20, 0x0f, 0x00];
        let entry = audio_entry(&ec3).unwrap();
        assert_eq!(&entry[4..8], b"ec-3");
        assert!(entry.windows(4).any(|w| w == b"dec3"));
        let mut wma = audio.levels[0].clone();
        wma.four_cc = "WMAP".into();
        assert!(matches!(audio_entry(&wma).unwrap_err(), DownloadError::Unsupported(_)));
        let mut vc1 = video.levels[0].clone();
        vc1.four_cc = "WVC1".into();
        assert!(matches!(video_entry(&vc1).unwrap_err(), DownloadError::Unsupported(_)));
        assert_eq!(language_code(Some("eng")), 0x15c7);
        assert_eq!(language_code(None), language_code(Some("und")));
    }

    /// The `hvcC` ffmpeg writes for an HEVC stream, and the same record built from the
    /// parameter sets it carries, agree on everything but the SEI ffmpeg also keeps.
    #[tokio::test]
    async fn hevc_headers_match_what_ffmpeg_writes() {
        let dir = temp_dir("hevc");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let path = dir.join("h.mp4");
        run_ffmpeg(
            &ffmpeg,
            &[
                "-loglevel", "error", "-f", "lavfi", "-i", "testsrc=size=64x64:rate=10:duration=1",
                "-c:v", "libx265", "-preset", "ultrafast", "-x265-params", "log-level=none",
                "-pix_fmt", "yuv420p", "-tag:v", "hvc1", path.to_str().unwrap(),
            ]
            .map(String::from),
        )
        .await;
        let bytes = tokio::fs::read(&path).await.unwrap();
        let at = bytes.windows(4).position(|w| w == b"hvcC").unwrap();
        let size = u32::from_be_bytes(bytes[at - 4..at].try_into().unwrap()) as usize;
        let reference = &bytes[at + 4..at - 4 + size];
        // The parameter sets, as Annex B, from the record's arrays.
        let mut private = Vec::new();
        let mut pos = 23;
        for _ in 0..reference[22] {
            let kind = reference[pos] & 0x3f;
            let count = u16::from_be_bytes([reference[pos + 1], reference[pos + 2]]) as usize;
            pos += 3;
            for _ in 0..count {
                let len = u16::from_be_bytes([reference[pos], reference[pos + 1]]) as usize;
                if matches!(kind, 32 | 33 | 34) {
                    private.extend_from_slice(&[0, 0, 0, 1]);
                    private.extend_from_slice(&reference[pos + 2..pos + 2 + len]);
                }
                pos += 2 + len;
            }
        }
        let level = Level {
            index: 0,
            bitrate: 100_000,
            four_cc: "HEVC".into(),
            width: 64,
            height: 64,
            private,
            sampling_rate: 0,
            channels: 0,
            bits_per_sample: 0,
            audio_tag: None,
            nal_length: 4,
            attributes: Vec::new(),
        };
        let built = hvcc(&level).unwrap();
        assert_eq!(&built[..22], &reference[..22], "built {} reference {}", hex::encode(&built[..22]), hex::encode(&reference[..22]));
        assert_eq!(built[22], 3);
        assert_eq!(built[23], 0x80 | 32);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn fragments_are_given_their_decode_time() {
        let dir = temp_dir("tfdt");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let packaged = package(&ffmpeg, &dir, 2).await;
        let manifest = parse_manifest(&packaged.manifest).unwrap();
        let video = manifest.streams.iter().find(|s| s.kind == StreamKind::Video).unwrap();
        let times = packaged.video_times();
        let second = &packaged.files.iter().find(|(name, _)| name.contains(&format!("video={}", times[1]))).unwrap().1;
        let fixed = with_decode_time(second, times[1]).unwrap();
        assert_eq!(fixed.len(), second.len() + 20);
        let tfhd = fixed.windows(4).position(|w| w == b"tfhd").unwrap();
        assert_eq!(&fixed[tfhd + 20..tfhd + 24], b"tfdt");
        assert_eq!(fixed[tfhd + 24], 1);
        assert_eq!(u64::from_be_bytes(fixed[tfhd + 28..tfhd + 36].try_into().unwrap()), times[1]);
        // The same again changes nothing, and ffmpeg places the fragment where it says.
        assert_eq!(with_decode_time(&fixed, times[1]).unwrap(), fixed);
        let init = init_section(video, &video.levels[0]).unwrap();
        let path = dir.join("one.mp4");
        let mut file = init.bytes.clone();
        file.extend_from_slice(&fixed);
        tokio::fs::write(&path, &file).await.unwrap();
        let info = ffmpeg.probe(&path).await.unwrap();
        assert!(info.video.is_some(), "{info:?}");
        assert!(near(mp4::start_time(&fixed, init.parsed.as_ref().unwrap()).unwrap(), times[1] as f64 / 10_000_000.0));
        // The file's timeline runs from zero to the fragment's end.
        assert!(near(info.duration.unwrap().as_secs_f64(), times[1] as f64 / 10_000_000.0 + 1.0), "{info:?}");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn presentations_download_as_recordings() {
        let dir = temp_dir("vod");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let packaged = package(&ffmpeg, &dir, 3).await;
        let site = Site::new();
        packaged.serve(&site, "v.ism/");
        site.put_text(&format!("{BASE}v.ism/Manifest"), MANIFEST_TYPE, &packaged.manifest);
        let (result, progress) = download_ism(&site, &ffmpeg, "v", &dir.join("job"), &DownloadContext::new(50_000_000), Some("video-0")).await;
        let downloaded = result.unwrap();
        assert!(downloaded.file.path.ends_with("source.mkv"));
        let info = ffmpeg.probe(&downloaded.file.path).await.unwrap();
        assert!(info.video.is_some(), "{info:?}");
        assert!(info.audio.is_some(), "{info:?}");
        assert!(near(info.duration.unwrap().as_secs_f64(), 3.0), "{info:?}");
        assert!(downloaded.notes.is_empty(), "{:?}", downloaded.notes);
        assert_eq!(progress.done, 3);
        assert_eq!(progress.total, Some(3));
        assert!(!dir.join("job").join("video.0.mp4").exists());
        let bitrate = level_bitrate(&packaged.manifest, "video");
        for time in packaged.video_times() {
            let url = format!("{BASE}v.ism/QualityLevels({bitrate})/Fragments(video={time})");
            assert_eq!(site.hits(&url), 1, "fragment {time}");
        }

        // Audio alone, as an audio-only variant asks.
        let http = Http::with_transport(site.clone(), Http::test_config());
        let downloader = IsmDownloader::new(http, ffmpeg.clone());
        let mut variant = Variant::new(Url::parse(&format!("{BASE}v.ism/Manifest")).unwrap(), VariantKind::Ism);
        variant.format_id = Some("audio".into());
        variant.audio_only = true;
        let (progress, _) = tokio::sync::watch::channel(Progress::default());
        let downloaded = downloader.download(&variant, &dir.join("audio"), &DownloadContext::new(50_000_000), progress).await.unwrap();
        let info = ffmpeg.probe(&downloaded.file.path).await.unwrap();
        assert!(info.video.is_none(), "{info:?}");
        assert!(info.audio.is_some(), "{info:?}");
        assert!(near(info.duration.unwrap().as_secs_f64(), 3.0), "{info:?}");

        // Over the byte budget, the download stops.
        let (result, _) = download_ism(&site, &ffmpeg, "v", &dir.join("small"), &DownloadContext::new(10_000), Some("video-0")).await;
        assert!(matches!(result.unwrap_err(), DownloadError::TooLarge { .. }));
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    fn level_bitrate(manifest: &str, kind: &str) -> String {
        let stream = manifest.split(&format!("Type=\"{kind}\"")).nth(1).unwrap();
        stream.split("Bitrate=\"").nth(1).unwrap().split('"').next().unwrap().to_string()
    }

    /// A live manifest of the packaged presentation offering its first `count` fragments
    /// of each stream.
    fn live_window(packaged: &Packaged, count: usize, ended: bool) -> String {
        let mut out = String::new();
        let mut seen = 0usize;
        for line in packaged.manifest.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("<StreamIndex") {
                seen = 0;
            }
            if trimmed.starts_with("<c ") {
                seen += 1;
                if seen > count {
                    continue;
                }
            }
            out.push_str(line);
            out.push('\n');
        }
        if ended {
            out
        } else {
            out.replace("MinorVersion=\"0\"", "MinorVersion=\"0\" IsLive=\"TRUE\" LookAheadFragmentCount=\"2\"")
                .replace(&format!("Duration=\"{}\"", duration_of(&packaged.manifest)), "Duration=\"0\"")
        }
    }

    fn duration_of(manifest: &str) -> String {
        manifest.split("Duration=\"").nth(1).unwrap().split('"').next().unwrap().to_string()
    }

    #[tokio::test]
    async fn a_live_manifest_is_followed_until_it_turns_on_demand() {
        let dir = temp_dir("live");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let packaged = package(&ffmpeg, &dir, 4).await;
        let site = Site::new();
        packaged.serve(&site, "l.ism/");
        site.put_series(
            &format!("{BASE}l.ism/Manifest"),
            vec![
                Reply::new(MANIFEST_TYPE, live_window(&packaged, 2, false)),
                Reply::new(MANIFEST_TYPE, live_window(&packaged, 3, false)),
                Reply::new(MANIFEST_TYPE, live_window(&packaged, 4, true)),
            ],
        );
        let mut context = DownloadContext::new(50_000_000);
        context.max_live = Duration::from_secs(60);
        let (result, progress) = download_ism(&site, &ffmpeg, "l", &dir.join("job"), &context, Some("video-0")).await;
        let downloaded = result.unwrap();
        let info = ffmpeg.probe(&downloaded.file.path).await.unwrap();
        assert!(info.video.is_some() && info.audio.is_some(), "{info:?}");
        assert!(near(info.duration.unwrap().as_secs_f64(), 4.0), "{info:?}");
        assert!(downloaded.notes.iter().any(|n| n.contains("the live stream ended")), "{:?}", downloaded.notes);
        assert_eq!(site.hits(&format!("{BASE}l.ism/Manifest")), 3);
        for (name, _) in &packaged.files {
            assert_eq!(site.hits(&format!("{BASE}l.ism/{name}")), 1, "{name}");
        }
        assert_eq!(progress.total, Some(60));
        assert_eq!(progress.done, 4);

        // A window that keeps growing is cut at the limit.
        let site = Site::new();
        packaged.serve(&site, "c.ism/");
        site.put_series(
            &format!("{BASE}c.ism/Manifest"),
            vec![
                Reply::new(MANIFEST_TYPE, live_window(&packaged, 2, false)),
                Reply::new(MANIFEST_TYPE, live_window(&packaged, 4, false)),
            ],
        );
        context.max_live = Duration::from_millis(2_500);
        let (result, _) = download_ism(&site, &ffmpeg, "c", &dir.join("cut"), &context, Some("video-0")).await;
        let downloaded = result.unwrap();
        let info = ffmpeg.probe(&downloaded.file.path).await.unwrap();
        assert!(near(info.duration.unwrap().as_secs_f64(), 3.0), "{info:?}");
        assert!(downloaded.notes.iter().any(|n| n.contains("capture cut at the limit")), "{:?}", downloaded.notes);
        let last = *packaged.video_times().last().unwrap();
        assert_eq!(site.hits(&format!("{BASE}c.ism/QualityLevels({})/Fragments(video={last})", level_bitrate(&packaged.manifest, "video"))), 0);

        // A manifest that stops answering ends the capture with what was taken.
        let site = Site::new();
        packaged.serve(&site, "g.ism/");
        site.put_series(
            &format!("{BASE}g.ism/Manifest"),
            vec![
                Reply::new(MANIFEST_TYPE, live_window(&packaged, 2, false)),
                Reply::new(MANIFEST_TYPE, "gone").status(404),
            ],
        );
        context.max_live = Duration::from_secs(60);
        let (result, _) = download_ism(&site, &ffmpeg, "g", &dir.join("gone"), &context, Some("video-0")).await;
        let downloaded = result.unwrap();
        let info = ffmpeg.probe(&downloaded.file.path).await.unwrap();
        assert!(near(info.duration.unwrap().as_secs_f64(), 2.0), "{info:?}");
        assert!(downloaded.notes.iter().any(|n| n.contains("could not be reloaded 3 times")), "{:?}", downloaded.notes);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn protected_presentations_are_drm_and_never_fetched() {
        let dir = temp_dir("drm");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let packaged = package(&ffmpeg, &dir, 2).await;
        let site = Site::new();
        packaged.serve(&site, "p.ism/");
        let locked = packaged.manifest.replacen(
            "<StreamIndex",
            "<Protection><ProtectionHeader SystemID=\"9A04F079-9840-4286-AB92-E65BE0885F95\">AAAA</ProtectionHeader></Protection>\n<StreamIndex",
            1,
        );
        assert!(locked.contains("<Protection>"));
        site.put_text(&format!("{BASE}p.ism/Manifest"), MANIFEST_TYPE, &locked);
        let (result, _) = download_ism(&site, &ffmpeg, "p", &dir.join("job"), &DownloadContext::new(50_000_000), Some("video-0")).await;
        assert!(matches!(result.unwrap_err(), DownloadError::Drm(_, system) if system == "playready"));
        for (name, _) in &packaged.files {
            assert_eq!(site.hits(&format!("{BASE}p.ism/{name}")), 0);
        }
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn text_tracks_come_with_the_media() {
        let dir = temp_dir("text");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let packaged = package(&ffmpeg, &dir, 2).await;
        let site = Site::new();
        packaged.serve(&site, "t.ism/");
        let ttml = |begin: &str, end: &str, text: &str| {
            plain_box(
                b"mdat",
                format!("<tt xmlns=\"http://www.w3.org/ns/ttml\"><body><div><p begin=\"{begin}\" end=\"{end}\">{text}</p></div></body></tt>").as_bytes(),
            )
        };
        site.put_bytes(&format!("{BASE}t.ism/QualityLevels(1000)/Fragments(textstream_deu=0)"), "application/mp4", &ttml("0.5s", "1s", "Eins"));
        site.put_bytes(&format!("{BASE}t.ism/QualityLevels(1000)/Fragments(textstream_deu=10000000)"), "application/mp4", &ttml("1s", "2s", "Zwei"));
        let manifest = packaged.manifest.replace(
            "</SmoothStreamingMedia>",
            "<StreamIndex Type=\"text\" Name=\"Untertitel\" Language=\"deu\" Subtype=\"CAPT\" QualityLevels=\"1\" Chunks=\"2\" Url=\"QualityLevels({bitrate})/Fragments(textstream_deu={start time})\">\n<QualityLevel Index=\"0\" Bitrate=\"1000\" FourCC=\"TTML\"/>\n<c t=\"0\" d=\"10000000\"/><c t=\"10000000\" d=\"10000000\"/>\n</StreamIndex>\n</SmoothStreamingMedia>",
        );
        site.put_text(&format!("{BASE}t.ism/Manifest"), MANIFEST_TYPE, &manifest);
        let mut context = DownloadContext::new(50_000_000);
        context.subtitles = Some(SubtitleChoice {
            tracks: Vec::new(),
            language: None,
        });
        let (result, _) = download_ism(&site, &ffmpeg, "t", &dir.join("job"), &context, Some("video-0")).await;
        let downloaded = result.unwrap();
        assert!(near(ffmpeg.probe(&downloaded.file.path).await.unwrap().duration.unwrap().as_secs_f64(), 2.0));
        assert_eq!(downloaded.subtitles.len(), 1, "{:?}", downloaded.subtitles);
        let track = &downloaded.subtitles[0];
        assert_eq!(track.language, "deu");
        assert_eq!(track.name.as_deref(), Some("Untertitel"));
        assert_eq!(track.format, SubtitleFormat::Ttml);
        assert_eq!(track.url.as_ref().unwrap().as_str(), format!("{BASE}t.ism/Manifest#text-deu"));
        let text = tokio::fs::read_to_string(&track.path).await.unwrap();
        assert!(text.contains("Eins") && text.contains("Zwei"), "{text}");
        assert_eq!(text.matches("<tt ").count(), 1, "{text}");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
