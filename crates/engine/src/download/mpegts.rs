//! Parse MPEG-TS segments and decrypt SAMPLE-AES elementary streams.
//!
//! H.264 leaves the first 32 bytes clear, then encrypts one 16-byte block in each group
//! of ten. Audio leaves the first 16 bytes clear. Each unit starts a separate CBC chain
//! using the segment IV.
//!
//! Remove and restore emulation-prevention bytes around decryption. Rebuild transport
//! packets when payload lengths change.

use std::collections::{HashMap, HashSet};

use aes::Aes128;
use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};

use super::DownloadError;

const PACKET: usize = 188;
const SYNC: u8 = 0x47;

/// Stream types the program map uses for sample-encrypted streams, and the plain ones
/// they stand for.
const SAMPLE_AES_TYPES: [(u8, u8); 4] = [(0xdb, 0x1b), (0xcf, 0x0f), (0xc1, 0x81), (0xc2, 0x87)];

fn bad(message: impl Into<String>) -> DownloadError {
    DownloadError::Segment(message.into())
}

/// Whether `bytes` are a transport stream: sync bytes at the start of the first packets.
pub fn is_transport_stream(bytes: &[u8]) -> bool {
    bytes.len() >= PACKET
        && bytes[0] == SYNC
        && (bytes.len() < 2 * PACKET || bytes[PACKET] == SYNC)
}

struct Packet<'a> {
    pid: u16,
    pusi: bool,
    cc: u8,
    /// The adaptation field with its length byte. Empty when there is none.
    adaptation: &'a [u8],
    payload: &'a [u8],
    has_payload: bool,
}

fn packets(data: &[u8]) -> Result<Vec<Packet<'_>>, DownloadError> {
    if data.is_empty() || data.len() % PACKET != 0 {
        return Err(bad(format!(
            "not a transport stream: {} bytes is not a whole number of packets",
            data.len()
        )));
    }
    let mut out = Vec::with_capacity(data.len() / PACKET);
    for (index, chunk) in data.chunks(PACKET).enumerate() {
        if chunk[0] != SYNC {
            return Err(bad(format!("packet {index} has no sync byte")));
        }
        let pid = (u16::from(chunk[1] & 0x1f) << 8) | u16::from(chunk[2]);
        let pusi = chunk[1] & 0x40 != 0;
        let afc = (chunk[3] >> 4) & 3;
        let cc = chunk[3] & 0x0f;
        let mut pos = 4;
        let adaptation: &[u8] = if afc & 2 != 0 {
            let len = usize::from(chunk[4]);
            if 5 + len > PACKET {
                return Err(bad(format!("packet {index} has an adaptation field of {len} bytes")));
            }
            pos = 5 + len;
            &chunk[4..pos]
        } else {
            &[]
        };
        let has_payload = afc & 1 != 0;
        let payload: &[u8] = if has_payload { &chunk[pos..] } else { &[] };
        out.push(Packet {
            pid,
            pusi,
            cc,
            adaptation,
            payload,
            has_payload,
        });
    }
    Ok(out)
}

/// A program specific information section put together from the packets of one PID.
struct Section {
    bytes: Vec<u8>,
    /// The packets it came from.
    packets: Vec<usize>,
}

/// The complete sections carried on `pid`, in order.
fn sections(packets: &[Packet<'_>], pid: u16) -> Vec<Section> {
    let mut out = Vec::new();
    let mut open: Option<Section> = None;
    for (index, packet) in packets.iter().enumerate() {
        if packet.pid != pid || !packet.has_payload {
            continue;
        }
        if packet.pusi {
            if let Some(section) = open.take() {
                out.push(section);
            }
            let pointer = usize::from(packet.payload[0]);
            if 1 + pointer > packet.payload.len() {
                continue;
            }
            open = Some(Section {
                bytes: packet.payload[1 + pointer..].to_vec(),
                packets: vec![index],
            });
        } else if let Some(section) = open.as_mut() {
            section.bytes.extend_from_slice(packet.payload);
            section.packets.push(index);
        }
        if let Some(section) = open.as_ref()
            && section.bytes.len() >= 3
        {
            let length = 3 + (usize::from(section.bytes[1] & 0x0f) << 8 | usize::from(section.bytes[2]));
            if section.bytes.len() >= length {
                let mut section = open.take().unwrap();
                section.bytes.truncate(length);
                out.push(section);
            }
        }
    }
    if let Some(section) = open {
        out.push(section);
    }
    out
}

/// One program's map: its section and the elementary streams it lists.
struct ProgramMap {
    section: Section,
    /// `(stream type, elementary PID, offset of the stream type byte in the section)`.
    streams: Vec<(u8, u16, usize)>,
}

fn program_maps(packets: &[Packet<'_>]) -> Vec<ProgramMap> {
    let mut pmt_pids = Vec::new();
    for section in sections(packets, 0) {
        let bytes = &section.bytes;
        if bytes.len() < 12 || bytes[0] != 0 {
            continue;
        }
        let length = 3 + (usize::from(bytes[1] & 0x0f) << 8 | usize::from(bytes[2]));
        let end = length.saturating_sub(4).min(bytes.len());
        let mut pos = 8;
        while pos + 4 <= end {
            let program = u16::from(bytes[pos]) << 8 | u16::from(bytes[pos + 1]);
            let pid = (u16::from(bytes[pos + 2] & 0x1f) << 8) | u16::from(bytes[pos + 3]);
            if program != 0 && !pmt_pids.contains(&pid) {
                pmt_pids.push(pid);
            }
            pos += 4;
        }
    }
    let mut maps = Vec::new();
    for pid in pmt_pids {
        for section in sections(packets, pid) {
            let bytes = &section.bytes;
            if bytes.len() < 16 || bytes[0] != 2 {
                continue;
            }
            let length = 3 + (usize::from(bytes[1] & 0x0f) << 8 | usize::from(bytes[2]));
            let end = length.saturating_sub(4).min(bytes.len());
            let info_length = usize::from(bytes[10] & 0x0f) << 8 | usize::from(bytes[11]);
            let mut pos = 12 + info_length;
            let mut streams = Vec::new();
            while pos + 5 <= end {
                let stream_type = bytes[pos];
                let elementary = (u16::from(bytes[pos + 1] & 0x1f) << 8) | u16::from(bytes[pos + 2]);
                let es_info = usize::from(bytes[pos + 3] & 0x0f) << 8 | usize::from(bytes[pos + 4]);
                streams.push((stream_type, elementary, pos));
                pos += 5 + es_info;
            }
            maps.push(ProgramMap { section, streams });
        }
    }
    maps
}

/// The CRC the program specific information sections carry.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= u32::from(byte) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_1DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// What a sample-encrypted elementary stream carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    H264,
    Aac,
    /// AC-3 and E-AC-3 alike: frames told apart by their sync information.
    Dolby,
}

fn kind_of(stream_type: u8) -> Option<Kind> {
    match stream_type {
        0x1b | 0xdb => Some(Kind::H264),
        0x0f | 0xcf => Some(Kind::Aac),
        0x81 | 0x87 | 0xc1 | 0xc2 => Some(Kind::Dolby),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Decrypt,
    /// What a packager does. Only the tests package.
    #[cfg_attr(not(test), allow(dead_code))]
    Encrypt,
}

/// A CBC chain over the encrypted blocks of one NAL unit or frame.
struct Chain<'a> {
    cipher: &'a Aes128,
    direction: Direction,
    previous: [u8; 16],
}

impl<'a> Chain<'a> {
    fn new(cipher: &'a Aes128, iv: &[u8; 16], direction: Direction) -> Self {
        Self {
            cipher,
            direction,
            previous: *iv,
        }
    }

    fn block(&mut self, data: &mut [u8]) {
        let mut block: [u8; 16] = data.try_into().expect("a 16-byte block");
        match self.direction {
            Direction::Decrypt => {
                let ciphertext = block;
                let mut inner = aes::Block::from(block);
                self.cipher.decrypt_block(&mut inner);
                for (byte, prev) in inner.iter_mut().zip(self.previous.iter()) {
                    *byte ^= prev;
                }
                block.copy_from_slice(&inner);
                self.previous = ciphertext;
            }
            Direction::Encrypt => {
                for (byte, prev) in block.iter_mut().zip(self.previous.iter()) {
                    *byte ^= prev;
                }
                let mut inner = aes::Block::from(block);
                self.cipher.encrypt_block(&mut inner);
                block.copy_from_slice(&inner);
                self.previous = block;
            }
        }
        data.copy_from_slice(&block);
    }
}

/// Runs the cipher over a slice's blocks past its clear leader: `video` in the one to
/// nine pattern that encrypts a block only while more than sixteen bytes remain, else
/// every whole block, the trailing part of a block left clear either way.
fn walk(data: &mut [u8], leader: usize, video: bool, chain: &mut Chain<'_>) {
    let len = data.len();
    let mut pos = leader.min(len);
    if video {
        while pos < len {
            if len - pos > 16 {
                chain.block(&mut data[pos..pos + 16]);
                pos += 16;
            }
            pos += 144.min(len - pos);
        }
    } else {
        while pos + 16 <= len {
            chain.block(&mut data[pos..pos + 16]);
            pos += 16;
        }
    }
}

/// A NAL unit's bytes without its emulation prevention bytes.
fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0usize;
    for &byte in nal {
        if zeros >= 2 && byte == 3 {
            zeros = 0;
            continue;
        }
        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// A NAL unit's bytes with emulation prevention bytes where a start code would otherwise
/// appear inside it.
fn escape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len() + nal.len() / 64);
    let mut zeros = 0usize;
    for &byte in nal {
        if zeros >= 2 && byte <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(byte);
        zeros = if byte == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Where each NAL unit of an Annex B stream starts and ends: `(prefix start, nal start,
/// nal end)`, the prefix being the start code with any zero bytes before it.
fn nal_units(es: &[u8]) -> Vec<(usize, usize, usize)> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= es.len() {
        if es[i] == 0 && es[i + 1] == 0 && es[i + 2] == 1 {
            starts.push(i);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut units: Vec<(usize, usize, usize)> = Vec::with_capacity(starts.len());
    for (n, &code) in starts.iter().enumerate() {
        let nal_start = code + 3;
        let mut nal_end = starts.get(n + 1).copied().unwrap_or(es.len());
        while nal_end > nal_start && es[nal_end - 1] == 0 {
            nal_end -= 1;
        }
        let prefix_start = if n == 0 { 0 } else { units[n - 1].2 };
        units.push((prefix_start, nal_start, nal_end));
    }
    units
}

fn transform_h264(es: &[u8], cipher: &Aes128, iv: &[u8; 16], direction: Direction) -> Vec<u8> {
    let units = nal_units(es);
    if units.is_empty() {
        return es.to_vec();
    }
    let mut out = Vec::with_capacity(es.len() + 64);
    let mut last_end = 0;
    for (prefix_start, nal_start, nal_end) in units {
        out.extend_from_slice(&es[prefix_start..nal_start]);
        let nal = &es[nal_start..nal_end];
        let nal_type = nal.first().map_or(0, |b| b & 0x1f);
        if matches!(nal_type, 1 | 5) && nal.len() > 32 {
            let mut body = unescape(&nal[1..]);
            let mut chain = Chain::new(cipher, iv, direction);
            walk(&mut body, 31, true, &mut chain);
            out.push(nal[0]);
            out.extend_from_slice(&escape(&body));
        } else {
            out.extend_from_slice(nal);
        }
        last_end = nal_end;
    }
    out.extend_from_slice(&es[last_end..]);
    out
}

/// The AAC frames of an ADTS stream, as `(header length, frame length)` pairs in order.
fn adts_frames(es: &[u8]) -> Result<Vec<(usize, usize, usize)>, DownloadError> {
    let mut frames = Vec::new();
    let mut pos = 0;
    while pos + 7 <= es.len() {
        if es[pos] != 0xff || es[pos + 1] & 0xf0 != 0xf0 {
            return Err(bad(format!("ADTS frame expected at byte {pos} of the audio stream")));
        }
        let header = if es[pos + 1] & 1 == 0 { 9 } else { 7 };
        let length = (usize::from(es[pos + 3] & 3) << 11)
            | (usize::from(es[pos + 4]) << 3)
            | (usize::from(es[pos + 5]) >> 5);
        if length < header || pos + length > es.len() {
            return Err(bad(format!(
                "ADTS frame at byte {pos} claims {length} bytes of the {} left",
                es.len() - pos
            )));
        }
        frames.push((pos, header, length));
        pos += length;
    }
    Ok(frames)
}

/// AC-3 syncframe sizes in 16-bit words by sample rate code and frame size code.
const AC3_WORDS: [[u16; 38]; 3] = [
    [
        64, 64, 80, 80, 96, 96, 112, 112, 128, 128, 160, 160, 192, 192, 224, 224, 256, 256,
        320, 320, 384, 384, 448, 448, 512, 512, 640, 640, 768, 768, 896, 896, 1024, 1024,
        1152, 1152, 1280, 1280,
    ],
    [
        69, 70, 87, 88, 104, 105, 121, 122, 139, 140, 174, 175, 208, 209, 243, 244, 278, 279,
        348, 349, 417, 418, 487, 488, 557, 558, 696, 697, 835, 836, 975, 976, 1114, 1115,
        1253, 1254, 1393, 1394,
    ],
    [
        96, 96, 120, 120, 144, 144, 168, 168, 192, 192, 240, 240, 288, 288, 336, 336, 384,
        384, 480, 480, 576, 576, 672, 672, 768, 768, 960, 960, 1152, 1152, 1344, 1344, 1536,
        1536, 1728, 1728, 1920, 1920,
    ],
];

/// The AC-3 and E-AC-3 syncframes of a stream, as `(start, length)` pairs in order.
fn dolby_frames(es: &[u8]) -> Result<Vec<(usize, usize)>, DownloadError> {
    let mut frames = Vec::new();
    let mut pos = 0;
    while pos + 6 <= es.len() {
        if es[pos] != 0x0b || es[pos + 1] != 0x77 {
            return Err(bad(format!("AC-3 syncframe expected at byte {pos} of the audio stream")));
        }
        let bsid = es[pos + 5] >> 3;
        let length = if bsid > 10 {
            2 * ((usize::from(es[pos + 2] & 7) << 8 | usize::from(es[pos + 3])) + 1)
        } else {
            let fscod = usize::from(es[pos + 4] >> 6);
            let code = usize::from(es[pos + 4] & 0x3f);
            let words = AC3_WORDS
                .get(fscod)
                .and_then(|row| row.get(code))
                .copied()
                .ok_or_else(|| bad(format!("AC-3 syncframe at byte {pos} has no size")))?;
            2 * usize::from(words)
        };
        if pos + length > es.len() {
            return Err(bad(format!(
                "AC-3 syncframe at byte {pos} claims {length} bytes of the {} left",
                es.len() - pos
            )));
        }
        frames.push((pos, length));
        pos += length;
    }
    Ok(frames)
}

fn transform_audio(
    es: &[u8],
    kind: Kind,
    cipher: &Aes128,
    iv: &[u8; 16],
    direction: Direction,
) -> Result<Vec<u8>, DownloadError> {
    let mut out = es.to_vec();
    match kind {
        Kind::Aac => {
            for (start, header, length) in adts_frames(es)? {
                let mut chain = Chain::new(cipher, iv, direction);
                walk(&mut out[start + header..start + length], 16, false, &mut chain);
            }
        }
        Kind::Dolby => {
            for (start, length) in dolby_frames(es)? {
                let mut chain = Chain::new(cipher, iv, direction);
                walk(&mut out[start..start + length], 16, false, &mut chain);
            }
        }
        Kind::H264 => unreachable!("video goes through the NAL unit walk"),
    }
    Ok(out)
}

/// Stream ids whose PES packets carry no optional header.
fn bare_stream(id: u8) -> bool {
    matches!(id, 0xbc | 0xbe | 0xbf | 0xf0 | 0xf1 | 0xf2 | 0xf8 | 0xff)
}

/// Where a PES packet's payload starts, and the PTS it carries.
fn pes_header(pes: &[u8]) -> Result<(usize, Option<u64>), DownloadError> {
    if pes.len() < 6 || pes[0] != 0 || pes[1] != 0 || pes[2] != 1 {
        return Err(bad("packetized elementary stream without a start code"));
    }
    if bare_stream(pes[3]) {
        return Ok((6, None));
    }
    if pes.len() < 9 {
        return Err(bad("packetized elementary stream cut inside its header"));
    }
    let header_length = usize::from(pes[8]);
    let start = 9 + header_length;
    if start > pes.len() {
        return Err(bad("packetized elementary stream cut inside its header"));
    }
    let pts = if pes[7] & 0x80 != 0 && header_length >= 5 {
        let b = &pes[9..14];
        Some(
            (u64::from(b[0] >> 1 & 7) << 30)
                | (u64::from(b[1]) << 22)
                | (u64::from(b[2] >> 1) << 15)
                | (u64::from(b[3]) << 7)
                | u64::from(b[4] >> 1),
        )
    } else {
        None
    };
    Ok((start, pts))
}

/// The stuffing that fills `pad` bytes of a packet as an adaptation field.
fn stuffing(pad: usize) -> Vec<u8> {
    match pad {
        0 => Vec::new(),
        1 => vec![0],
        n => {
            let mut af = vec![(n - 1) as u8, 0];
            af.resize(n, 0xff);
            af
        }
    }
}

/// Writes `payload` as packets of `pid`, the first with the payload unit start and
/// `first_adaptation` as its adaptation field, the last padded: with an adaptation
/// field's stuffing for a PES packet, with 0xFF bytes after a section.
fn packetize(
    pid: u16,
    cc: &mut u8,
    first_adaptation: &[u8],
    payload: &[u8],
    section: bool,
    out: &mut Vec<u8>,
) {
    let mut offset = 0;
    let mut first = true;
    while offset < payload.len() || first {
        let mut af: Vec<u8> = if first {
            first_adaptation.to_vec()
        } else {
            Vec::new()
        };
        let remaining = payload.len() - offset;
        let room = PACKET - 4 - af.len();
        if remaining < room && !section {
            let pad = room - remaining;
            if af.is_empty() {
                af = stuffing(pad);
            } else if af.len() == 1 {
                af[0] += pad as u8;
                af.push(0);
                af.resize(1 + pad, 0xff);
            } else {
                af[0] += pad as u8;
                af.resize(af.len() + pad, 0xff);
            }
        }
        let take = remaining.min(PACKET - 4 - af.len());
        let afc = (u8::from(!af.is_empty()) << 1) | u8::from(take > 0 || section);
        out.push(SYNC);
        out.push((u8::from(first) << 6) | (pid >> 8) as u8);
        out.push(pid as u8);
        out.push((afc << 4) | (*cc & 0x0f));
        out.extend_from_slice(&af);
        out.extend_from_slice(&payload[offset..offset + take]);
        let written = 4 + af.len() + take;
        out.resize(out.len() + (PACKET - written), 0xff);
        if afc & 1 != 0 {
            *cc = (*cc + 1) & 0x0f;
        }
        offset += take;
        first = false;
    }
}

fn transform(
    data: &[u8],
    key: &[u8; 16],
    iv: &[u8; 16],
    direction: Direction,
) -> Result<Vec<u8>, DownloadError> {
    let packets = packets(data)?;
    let maps = program_maps(&packets);
    if maps.is_empty() {
        return Err(bad("transport stream segment has no program map"));
    }
    let cipher = Aes128::new(key.into());
    // Which elementary streams change, and how the program maps describe them after.
    let mut kinds: HashMap<u16, Kind> = HashMap::new();
    let mut map_sections: Vec<(Vec<u8>, Vec<usize>)> = Vec::new();
    for map in &maps {
        let mut section = map.section.bytes.clone();
        let mut changed = false;
        for &(stream_type, elementary, offset) in &map.streams {
            let replacement = match direction {
                Direction::Decrypt => SAMPLE_AES_TYPES
                    .iter()
                    .find(|(encrypted, _)| *encrypted == stream_type)
                    .map(|(_, plain)| *plain),
                Direction::Encrypt => SAMPLE_AES_TYPES
                    .iter()
                    .find(|(_, plain)| *plain == stream_type)
                    .map(|(encrypted, _)| *encrypted),
            };
            if let Some(replacement) = replacement
                && let Some(kind) = kind_of(stream_type)
            {
                section[offset] = replacement;
                kinds.insert(elementary, kind);
                changed = true;
            }
        }
        if changed {
            let end = section.len() - 4;
            let crc = crc32(&section[..end]);
            section[end..].copy_from_slice(&crc.to_be_bytes());
            map_sections.push((section, map.section.packets.clone()));
        }
    }
    if kinds.is_empty() {
        return Ok(data.to_vec());
    }
    // The PES packets of each changing stream, by the packet each starts in.
    struct Pes {
        pid: u16,
        packets: Vec<usize>,
        bytes: Vec<u8>,
    }
    let mut open: HashMap<u16, Pes> = HashMap::new();
    let mut groups: Vec<Pes> = Vec::new();
    for (index, packet) in packets.iter().enumerate() {
        if !kinds.contains_key(&packet.pid) || !packet.has_payload {
            continue;
        }
        if packet.pusi {
            if let Some(previous) = open.remove(&packet.pid) {
                groups.push(previous);
            }
            open.insert(
                packet.pid,
                Pes {
                    pid: packet.pid,
                    packets: vec![index],
                    bytes: packet.payload.to_vec(),
                },
            );
        } else if let Some(pes) = open.get_mut(&packet.pid) {
            pes.packets.push(index);
            pes.bytes.extend_from_slice(packet.payload);
        }
    }
    groups.extend(open.into_values());
    groups.sort_by_key(|pes| pes.packets[0]);
    let mut replaced: HashMap<usize, Vec<u8>> = HashMap::new();
    let mut consumed: HashSet<usize> = HashSet::new();
    let mut counters: HashMap<u16, u8> = HashMap::new();
    for pes in groups {
        let (start, _) = pes_header(&pes.bytes)?;
        let kind = kinds[&pes.pid];
        let es = &pes.bytes[start..];
        let transformed = match kind {
            Kind::H264 => transform_h264(es, &cipher, iv, direction),
            Kind::Aac | Kind::Dolby => transform_audio(es, kind, &cipher, iv, direction)?,
        };
        let mut rebuilt = pes.bytes[..start].to_vec();
        rebuilt.extend_from_slice(&transformed);
        let declared = u16::from(pes.bytes[4]) << 8 | u16::from(pes.bytes[5]);
        if declared != 0 {
            let length = u16::try_from(rebuilt.len() - 6).unwrap_or(0);
            rebuilt[4..6].copy_from_slice(&length.to_be_bytes());
        }
        let first = &packets[pes.packets[0]];
        let cc = counters.entry(pes.pid).or_insert(first.cc);
        let mut out = Vec::with_capacity(rebuilt.len() + rebuilt.len() / 40 + PACKET);
        packetize(pes.pid, cc, first.adaptation, &rebuilt, false, &mut out);
        replaced.insert(pes.packets[0], out);
        consumed.extend(pes.packets.iter().skip(1).copied());
    }
    for (section, indices) in &map_sections {
        let first = &packets[indices[0]];
        let mut cc = first.cc;
        let mut payload = vec![0u8];
        payload.extend_from_slice(section);
        let mut out = Vec::with_capacity(PACKET * 2);
        packetize(first.pid, &mut cc, first.adaptation, &payload, true, &mut out);
        replaced.insert(indices[0], out);
        consumed.extend(indices.iter().skip(1).copied());
    }
    let mut out = Vec::with_capacity(data.len() + data.len() / 32);
    let mut last_cc: HashMap<u16, u8> = HashMap::new();
    for (index, packet) in packets.iter().enumerate() {
        if let Some(bytes) = replaced.remove(&index) {
            if let Some(last) = bytes.get(bytes.len() - PACKET + 3) {
                last_cc.insert(packet.pid, last & 0x0f);
            }
            out.extend_from_slice(&bytes);
        } else if consumed.contains(&index) {
            continue;
        } else if kinds.contains_key(&packet.pid) && !packet.has_payload {
            // A packet of the stream carrying only an adaptation field keeps the counter
            // of the packet before it.
            let mut chunk = data[index * PACKET..(index + 1) * PACKET].to_vec();
            if let Some(cc) = last_cc.get(&packet.pid) {
                chunk[3] = (chunk[3] & 0xf0) | cc;
            }
            out.extend_from_slice(&chunk);
        } else {
            out.extend_from_slice(&data[index * PACKET..(index + 1) * PACKET]);
        }
    }
    Ok(out)
}

/// Undoes SAMPLE-AES on a transport stream segment: every sample-encrypted elementary
/// stream is decrypted with `key` and `iv` and the program map says the streams are plain.
/// A segment whose program map lists no sample-encrypted stream comes back as it was.
pub fn decrypt_sample_aes(
    data: &[u8],
    key: &[u8; 16],
    iv: &[u8; 16],
) -> Result<Vec<u8>, DownloadError> {
    transform(data, key, iv, Direction::Decrypt)
}

/// Applies SAMPLE-AES to a plain transport stream segment, as a packager would.
#[cfg(test)]
pub fn encrypt_sample_aes(
    data: &[u8],
    key: &[u8; 16],
    iv: &[u8; 16],
) -> Result<Vec<u8>, DownloadError> {
    transform(data, key, iv, Direction::Encrypt)
}

/// The earliest presentation time any elementary stream of the segment starts at, in
/// 90 kHz ticks.
pub fn start_time(data: &[u8]) -> Option<u64> {
    let packets = packets(data).ok()?;
    let maps = program_maps(&packets);
    let elementary: HashSet<u16> = maps
        .iter()
        .flat_map(|map| map.streams.iter().map(|(_, pid, _)| *pid))
        .collect();
    let mut seen: HashSet<u16> = HashSet::new();
    let mut earliest: Option<u64> = None;
    for packet in &packets {
        if !packet.pusi || !packet.has_payload || !elementary.contains(&packet.pid) {
            continue;
        }
        if !seen.insert(packet.pid) {
            continue;
        }
        if let Ok((_, Some(pts))) = pes_header(packet.payload) {
            earliest = Some(earliest.map_or(pts, |e| e.min(pts)));
        }
    }
    earliest
}

/// By elementary PID, the presentation time of each PES packet and the elementary
/// stream bytes of all of them.
#[cfg(test)]
pub type ElementaryStreams = HashMap<u16, (Vec<Option<u64>>, Vec<u8>)>;

/// The elementary stream bytes of every PES packet on each stream the program map lists,
/// with their presentation times: what the transport carries, however it is packetized.
#[cfg(test)]
pub fn elementary_streams(data: &[u8]) -> Result<ElementaryStreams, DownloadError> {
    let packets = packets(data)?;
    let maps = program_maps(&packets);
    let mut out: ElementaryStreams = HashMap::new();
    let mut open: HashMap<u16, Vec<u8>> = HashMap::new();
    let elementary: HashSet<u16> = maps
        .iter()
        .flat_map(|map| map.streams.iter().map(|(_, pid, _)| *pid))
        .collect();
    let flush = |pid: u16, pes: Vec<u8>, out: &mut ElementaryStreams| {
        let (start, pts) = pes_header(&pes)?;
        let entry = out.entry(pid).or_default();
        entry.0.push(pts);
        entry.1.extend_from_slice(&pes[start..]);
        Ok::<(), DownloadError>(())
    };
    for packet in &packets {
        if !elementary.contains(&packet.pid) || !packet.has_payload {
            continue;
        }
        if packet.pusi {
            if let Some(pes) = open.remove(&packet.pid) {
                flush(packet.pid, pes, &mut out)?;
            }
            open.insert(packet.pid, packet.payload.to_vec());
        } else if let Some(pes) = open.get_mut(&packet.pid) {
            pes.extend_from_slice(packet.payload);
        }
    }
    for (pid, pes) in open {
        flush(pid, pes, &mut out)?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn emulation_prevention_bytes_come_off_and_go_back_on() {
        let plain = [0x65, 0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 0, 0, 4, 0, 0];
        let escaped = escape(&plain);
        assert_eq!(
            escaped,
            [0x65, 0, 0, 3, 0, 1, 0, 0, 3, 2, 0, 0, 3, 3, 0, 0, 4, 0, 0]
        );
        assert_eq!(unescape(&escaped), plain);
    }

    #[test]
    fn the_video_pattern_encrypts_one_block_in_ten_past_the_leader() {
        let key = [1u8; 16];
        let iv = [2u8; 16];
        let cipher = Aes128::new(&key.into());
        let plain: Vec<u8> = (0..400u32).map(|i| i as u8).collect();
        let mut data = plain.clone();
        walk(&mut data, 31, true, &mut Chain::new(&cipher, &iv, Direction::Encrypt));
        // Bytes 31..47 and 191..207 are encrypted. 47..191 and 207..351 clear. At byte
        // 351 only 49 remain, so one more block, then the rest clear.
        assert_eq!(&data[..31], &plain[..31]);
        assert_ne!(&data[31..47], &plain[31..47]);
        assert_eq!(&data[47..191], &plain[47..191]);
        assert_ne!(&data[191..207], &plain[191..207]);
        assert_eq!(&data[207..351], &plain[207..351]);
        assert_ne!(&data[351..367], &plain[351..367]);
        assert_eq!(&data[367..], &plain[367..]);
        walk(&mut data, 31, true, &mut Chain::new(&cipher, &iv, Direction::Decrypt));
        assert_eq!(data, plain);

        // A frame shorter than its leader is left alone.
        let mut short = vec![1u8; 10];
        walk(&mut short, 16, false, &mut Chain::new(&cipher, &iv, Direction::Encrypt));
        assert_eq!(short, vec![1u8; 10]);

        // Audio: whole blocks past 16 bytes, the trailing partial block clear.
        let plain: Vec<u8> = (0..50u32).map(|i| i as u8).collect();
        let mut data = plain.clone();
        walk(&mut data, 16, false, &mut Chain::new(&cipher, &iv, Direction::Encrypt));
        assert_eq!(&data[..16], &plain[..16]);
        assert_ne!(&data[16..48], &plain[16..48]);
        assert_eq!(&data[48..], &plain[48..]);
        walk(&mut data, 16, false, &mut Chain::new(&cipher, &iv, Direction::Decrypt));
        assert_eq!(data, plain);
    }

    #[test]
    fn sections_carry_the_mpeg_crc() {
        assert_eq!(crc32(b"123456789"), 0x0376_E6E7);
    }

    #[tokio::test]
    async fn sample_aes_round_trips_through_a_real_stream() {
        use std::ffi::OsString;
        let dir = std::env::temp_dir().join(format!("discoclip-saes-{}", uuid::Uuid::now_v7()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let ffmpeg = crate::ffmpeg::Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let plain_path = dir.join("plain.ts");
        let args: Vec<OsString> = [
            "-loglevel", "error", "-f", "lavfi", "-i", "testsrc=size=64x64:rate=10:duration=2",
            "-f", "lavfi", "-i", "sine=frequency=440:duration=2", "-c:v", "libx264", "-preset",
            "ultrafast", "-pix_fmt", "yuv420p", "-g", "10", "-c:a", "aac", "-f", "mpegts",
        ]
        .iter()
        .map(OsString::from)
        .chain([plain_path.as_os_str().to_owned()])
        .collect();
        ffmpeg.run(args, |_| {}).await.unwrap();
        let plain = tokio::fs::read(&plain_path).await.unwrap();
        let key = [0x42u8; 16];
        let iv = [0x24u8; 16];
        let encrypted = encrypt_sample_aes(&plain, &key, &iv).unwrap();
        assert_ne!(encrypted, plain);
        let types: Vec<u8> = program_maps(&packets(&encrypted).unwrap())
            .iter()
            .flat_map(|m| m.streams.iter().map(|(t, _, _)| *t))
            .collect();
        assert!(types.contains(&0xdb), "{types:?}");
        assert!(types.contains(&0xcf), "{types:?}");
        let plain_es = elementary_streams(&plain).unwrap();
        let encrypted_es = elementary_streams(&encrypted).unwrap();
        assert_eq!(plain_es.len(), 2);
        for (pid, (pts, bytes)) in &plain_es {
            assert_eq!(&encrypted_es[pid].0, pts);
            assert_ne!(&encrypted_es[pid].1, bytes);
        }

        let decrypted = decrypt_sample_aes(&encrypted, &key, &iv).unwrap();
        assert_eq!(elementary_streams(&decrypted).unwrap(), plain_es);
        // Every repeat of the program map says the streams are plain again.
        let mut types: Vec<u8> = program_maps(&packets(&decrypted).unwrap())
            .iter()
            .flat_map(|m| m.streams.iter().map(|(t, _, _)| *t))
            .collect();
        types.sort();
        types.dedup();
        assert_eq!(types, vec![0x0f, 0x1b]);
        assert_eq!(start_time(&decrypted), start_time(&plain));
        assert!(start_time(&plain).is_some());
        let decrypted_path = dir.join("decrypted.ts");
        tokio::fs::write(&decrypted_path, &decrypted).await.unwrap();
        let info = ffmpeg.probe(&decrypted_path).await.unwrap();
        assert!(info.video.is_some(), "{info:?}");
        assert!(info.audio.is_some(), "{info:?}");
        let duration = info.duration.unwrap().as_secs_f64();
        assert!((1.5..=2.5).contains(&duration), "{duration}");

        // A wrong key leaves the streams scrambled, in a stream that still parses.
        let wrong = decrypt_sample_aes(&encrypted, &[1u8; 16], &iv).unwrap();
        let wrong_es = elementary_streams(&wrong).unwrap();
        assert_ne!(wrong_es, plain_es);
        // A plain stream under a SAMPLE-AES key comes back as it was.
        assert_eq!(decrypt_sample_aes(&plain, &key, &iv).unwrap(), plain);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn packets_are_rebuilt_with_stuffing_and_counters() {
        let payload: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
        let mut cc = 14;
        let mut out = Vec::new();
        packetize(0x100, &mut cc, &[1, 0x40], &payload, false, &mut out);
        assert_eq!(out.len(), 2 * PACKET);
        let parsed = packets(&out).unwrap();
        assert!(parsed[0].pusi);
        assert!(!parsed[1].pusi);
        assert_eq!(parsed[0].cc, 14);
        assert_eq!(parsed[1].cc, 15);
        assert_eq!(cc, 0);
        assert_eq!(parsed[0].adaptation, &[1, 0x40]);
        assert_eq!(parsed[0].payload, &payload[..182]);
        assert_eq!(parsed[1].payload, &payload[182..]);
        assert_eq!(parsed[1].adaptation.len(), 184 - 118);
        assert_eq!(parsed[1].adaptation[0] as usize, 184 - 118 - 1);
        assert_eq!(parsed[1].adaptation[1], 0);
    }
}
