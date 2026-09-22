//! Fragmented MP4 as HLS segments carry it: the tracks and sample protection an
//! initialization section declares, Common Encryption undone in place in the media
//! segments, in the `cbcs` and `cbc1` schemes SAMPLE-AES uses and the `cenc` and `cens`
//! ones SAMPLE-AES-CTR does, and the time a segment starts at.

use std::collections::HashMap;
use std::ops::Range;

use aes::Aes128;
use aes::cipher::{BlockCipherDecrypt, KeyInit, KeyIvInit, StreamCipher};

use super::DownloadError;

fn bad(message: impl Into<String>) -> DownloadError {
    DownloadError::Segment(message.into())
}

/// One box: where it sits in its buffer.
#[derive(Debug, Clone, Copy)]
struct BoxRef {
    kind: [u8; 4],
    start: usize,
    header: usize,
    end: usize,
}

impl BoxRef {
    fn body(&self) -> Range<usize> {
        self.start + self.header..self.end
    }

    fn name(&self) -> String {
        String::from_utf8_lossy(&self.kind).into_owned()
    }
}

fn u16_at(data: &[u8], at: usize) -> Option<u16> {
    data.get(at..at + 2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
}

fn u32_at(data: &[u8], at: usize) -> Option<u32> {
    data.get(at..at + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn u64_at(data: &[u8], at: usize) -> Option<u64> {
    data.get(at..at + 8)
        .map(|b| u64::from_be_bytes(b.try_into().unwrap()))
}

/// The boxes laid end to end in `data[range]`.
fn boxes(data: &[u8], range: Range<usize>) -> Result<Vec<BoxRef>, DownloadError> {
    let mut out = Vec::new();
    let mut pos = range.start;
    while pos + 8 <= range.end {
        let size = u32_at(data, pos).unwrap();
        let kind: [u8; 4] = data[pos + 4..pos + 8].try_into().unwrap();
        let (size, header) = match size {
            0 => (range.end - pos, 8),
            1 => {
                let large = u64_at(data, pos + 8)
                    .ok_or_else(|| bad("box with a large size cut short"))?;
                (usize::try_from(large).map_err(|_| bad("box too large"))?, 16)
            }
            n => (n as usize, 8),
        };
        if size < header || pos + size > range.end {
            return Err(bad(format!(
                "box {} of {size} bytes overruns its parent",
                String::from_utf8_lossy(&kind)
            )));
        }
        out.push(BoxRef {
            kind,
            start: pos,
            header,
            end: pos + size,
        });
        pos += size;
    }
    Ok(out)
}

fn child(boxes: &[BoxRef], kind: &[u8; 4]) -> Option<BoxRef> {
    boxes.iter().copied().find(|b| &b.kind == kind)
}

/// Where a sample entry's child boxes begin: after the fixed fields of its kind.
fn sample_entry_children(data: &[u8], entry: &BoxRef) -> Result<usize, DownloadError> {
    let body = entry.start + entry.header;
    match &entry.kind {
        b"encv" | b"avc1" | b"avc3" | b"hvc1" | b"hev1" | b"av01" | b"vp09" | b"mp4v" => {
            Ok(body + 78)
        }
        b"enca" | b"mp4a" | b"ac-3" | b"ec-3" | b"Opus" | b"fLaC" | b"alac" => {
            match u16_at(data, body + 8).unwrap_or(0) {
                0 => Ok(body + 28),
                1 => Ok(body + 44),
                2 => Ok(body + 64),
                version => Err(bad(format!(
                    "audio sample entry {} has version {version}",
                    entry.name()
                ))),
            }
        }
        _ => Err(bad(format!(
            "sample entry {} is neither video nor audio",
            entry.name()
        ))),
    }
}

/// How a track's samples are protected, from a `tenc` box or a `seig` sample group entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Protection {
    /// `cbcs`, `cbc1`, `cenc` or `cens`.
    pub scheme: [u8; 4],
    pub protected: bool,
    /// Bytes of IV each sample carries. Zero when `constant_iv` serves every sample.
    pub iv_size: u8,
    pub constant_iv: Option<Vec<u8>>,
    /// The pattern, in 16-byte blocks: so many encrypted, then so many clear. Both zero
    /// when every whole block is encrypted.
    pub crypt_blocks: u8,
    pub skip_blocks: u8,
}

impl Protection {
    /// Reads the fields `tenc` and `seig` entries share, from `at`: reserved, pattern,
    /// isProtected, Per_Sample_IV_Size, KID, and the constant IV when there is one.
    fn parse(data: &[u8], at: usize, scheme: [u8; 4]) -> Result<Self, DownloadError> {
        let pattern = *data.get(at + 1).ok_or_else(|| bad("protection entry cut short"))?;
        let protected = data[at + 2] == 1;
        let iv_size = data[at + 3];
        let constant_iv = if protected && iv_size == 0 {
            let size = usize::from(
                *data
                    .get(at + 20)
                    .ok_or_else(|| bad("protection entry without its constant IV"))?,
            );
            Some(
                data.get(at + 21..at + 21 + size)
                    .ok_or_else(|| bad("protection entry cut inside its constant IV"))?
                    .to_vec(),
            )
        } else {
            None
        };
        Ok(Self {
            scheme,
            protected,
            iv_size,
            constant_iv,
            crypt_blocks: pattern >> 4,
            skip_blocks: pattern & 0x0f,
        })
    }

    /// The entry's length in bytes.
    fn len(&self) -> usize {
        20 + self.constant_iv.as_ref().map_or(0, |iv| 1 + iv.len())
    }
}

/// A track the initialization section declares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub id: u32,
    pub timescale: u32,
    /// The sample size fragments fall back on, from `trex`.
    pub default_sample_size: u32,
    /// The sample duration fragments fall back on, from `trex`.
    pub default_sample_duration: u32,
    pub protection: Option<Protection>,
    /// The `seig` sample group entries of the movie box, in order.
    pub seig: Vec<Protection>,
}

/// An initialization section read: its tracks, and its bytes with every protected
/// sample entry turned back into the plain one it wraps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Init {
    pub cleared: Vec<u8>,
    pub tracks: Vec<Track>,
}

/// Renames the box at `at` to `free`, so a demuxer passes over it.
fn free(out: &mut [u8], at: &BoxRef) {
    out[at.start + 4..at.start + 8].copy_from_slice(b"free");
}

/// The `seig` sample group descriptions in an `sgpd` box, in order. Empty for any other
/// grouping.
fn seig_entries(data: &[u8], sgpd: &BoxRef, scheme: [u8; 4]) -> Result<Vec<Protection>, DownloadError> {
    let body = sgpd.start + sgpd.header;
    let version = data[body];
    if data.get(body + 4..body + 8) != Some(b"seig") {
        return Ok(Vec::new());
    }
    let mut pos = body + 8;
    let default_length = if version >= 1 {
        pos += 4;
        u32_at(data, pos - 4).unwrap_or(0)
    } else {
        0
    };
    if version >= 2 {
        pos += 4;
    }
    let count = u32_at(data, pos).ok_or_else(|| bad("sgpd cut short"))?;
    pos += 4;
    let mut entries = Vec::new();
    for _ in 0..count {
        let length = if version >= 1 && default_length == 0 {
            pos += 4;
            u32_at(data, pos - 4).ok_or_else(|| bad("sgpd cut short"))? as usize
        } else {
            0
        };
        let entry = Protection::parse(data, pos, scheme)?;
        pos += if length > 0 {
            length
        } else if default_length > 0 {
            default_length as usize
        } else {
            entry.len()
        };
        entries.push(entry);
    }
    Ok(entries)
}

/// Reads the initialization section: every track with its protection, and the bytes
/// with protected sample entries renamed to the format they wrap, their `sinf` boxes
/// and any `pssh` boxes freed, so a demuxer reads the decrypted samples as plain media.
pub fn read_init(data: &[u8]) -> Result<Init, DownloadError> {
    let mut out = data.to_vec();
    let top = boxes(data, 0..data.len())?;
    let moov = child(&top, b"moov").ok_or_else(|| bad("initialization section has no moov"))?;
    let moov_children = boxes(data, moov.body())?;
    let mut defaults: HashMap<u32, (u32, u32)> = HashMap::new();
    for item in &moov_children {
        match &item.kind {
            b"pssh" => free(&mut out, item),
            b"mvex" => {
                for trex in boxes(data, item.body())?
                    .iter()
                    .filter(|b| &b.kind == b"trex")
                {
                    let body = trex.start + trex.header;
                    if let (Some(id), Some(duration), Some(size)) = (
                        u32_at(data, body + 4),
                        u32_at(data, body + 12),
                        u32_at(data, body + 16),
                    ) {
                        defaults.insert(id, (size, duration));
                    }
                }
            }
            _ => {}
        }
    }
    let mut tracks = Vec::new();
    for trak in moov_children.iter().filter(|b| &b.kind == b"trak") {
        let parts = boxes(data, trak.body())?;
        let tkhd = child(&parts, b"tkhd").ok_or_else(|| bad("trak without tkhd"))?;
        let tkhd_body = tkhd.start + tkhd.header;
        let id = u32_at(
            data,
            tkhd_body + if data[tkhd_body] == 1 { 20 } else { 12 },
        )
        .ok_or_else(|| bad("tkhd cut short"))?;
        let mdia = child(&parts, b"mdia").ok_or_else(|| bad("trak without mdia"))?;
        let mdia_parts = boxes(data, mdia.body())?;
        let mdhd = child(&mdia_parts, b"mdhd").ok_or_else(|| bad("mdia without mdhd"))?;
        let mdhd_body = mdhd.start + mdhd.header;
        let timescale = u32_at(
            data,
            mdhd_body + if data[mdhd_body] == 1 { 20 } else { 12 },
        )
        .ok_or_else(|| bad("mdhd cut short"))?;
        let minf = child(&mdia_parts, b"minf").ok_or_else(|| bad("mdia without minf"))?;
        let stbl = child(&boxes(data, minf.body())?, b"stbl")
            .ok_or_else(|| bad("minf without stbl"))?;
        let stbl_parts = boxes(data, stbl.body())?;
        let stsd = child(&stbl_parts, b"stsd").ok_or_else(|| bad("stbl without stsd"))?;
        let mut protection = None;
        for entry in boxes(data, stsd.start + stsd.header + 8..stsd.end)? {
            if !entry.kind.starts_with(b"enc") {
                continue;
            }
            let children_start = sample_entry_children(data, &entry)?;
            let children = boxes(data, children_start..entry.end)?;
            let sinf = child(&children, b"sinf").ok_or_else(|| {
                bad(format!("protected sample entry {} without sinf", entry.name()))
            })?;
            let sinf_parts = boxes(data, sinf.body())?;
            let frma = child(&sinf_parts, b"frma")
                .ok_or_else(|| bad("sinf without frma"))?;
            let format: [u8; 4] = data[frma.start + frma.header..frma.start + frma.header + 4]
                .try_into()
                .unwrap();
            let schm = child(&sinf_parts, b"schm").ok_or_else(|| bad("sinf without schm"))?;
            let scheme: [u8; 4] = data[schm.start + schm.header + 4..schm.start + schm.header + 8]
                .try_into()
                .unwrap();
            if !matches!(&scheme, b"cbcs" | b"cbc1" | b"cenc" | b"cens") {
                return Err(bad(format!(
                    "samples are protected with the {} scheme",
                    String::from_utf8_lossy(&scheme)
                )));
            }
            let schi = child(&sinf_parts, b"schi").ok_or_else(|| bad("sinf without schi"))?;
            let tenc = child(&boxes(data, schi.body())?, b"tenc")
                .ok_or_else(|| bad("schi without tenc"))?;
            protection = Some(Protection::parse(data, tenc.start + tenc.header + 4, scheme)?);
            out[entry.start + 4..entry.start + 8].copy_from_slice(&format);
            free(&mut out, &sinf);
        }
        let mut seig = Vec::new();
        if let Some(protection) = &protection {
            for sgpd in stbl_parts.iter().filter(|b| &b.kind == b"sgpd") {
                let entries = seig_entries(data, sgpd, protection.scheme)?;
                if !entries.is_empty() {
                    seig = entries;
                    free(&mut out, sgpd);
                }
            }
        }
        tracks.push(Track {
            id,
            timescale,
            default_sample_size: defaults.get(&id).map_or(0, |d| d.0),
            default_sample_duration: defaults.get(&id).map_or(0, |d| d.1),
            protection,
            seig,
        });
    }
    Ok(Init {
        cleared: out,
        tracks,
    })
}

/// One sample's encryption: its IV and its clear and encrypted byte runs.
struct SampleInfo {
    iv: Vec<u8>,
    subsamples: Vec<(usize, usize)>,
}

/// Reads sample auxiliary information laid out as `senc` carries it, from `pos`, for
/// `count` samples whose IV sizes `iv_size_of` gives.
fn sample_infos(
    data: &[u8],
    mut pos: usize,
    count: usize,
    subsamples: bool,
    iv_size_of: impl Fn(usize) -> usize,
) -> Result<Vec<SampleInfo>, DownloadError> {
    let mut infos = Vec::with_capacity(count);
    for index in 0..count {
        let iv_size = iv_size_of(index);
        let iv = data
            .get(pos..pos + iv_size)
            .ok_or_else(|| bad("sample encryption information cut short"))?
            .to_vec();
        pos += iv_size;
        let mut runs = Vec::new();
        if subsamples {
            let n = u16_at(data, pos).ok_or_else(|| bad("sample encryption information cut short"))?;
            pos += 2;
            for _ in 0..n {
                let clear = u16_at(data, pos)
                    .ok_or_else(|| bad("sample encryption information cut short"))?;
                let encrypted = u32_at(data, pos + 2)
                    .ok_or_else(|| bad("sample encryption information cut short"))?;
                runs.push((clear as usize, encrypted as usize));
                pos += 6;
            }
        }
        infos.push(SampleInfo {
            iv,
            subsamples: runs,
        });
    }
    Ok(infos)
}

/// The sample group each sample of a fragment belongs to, from an `sbgp` of type `seig`:
/// `(sample count, group description index)` runs. Empty when there is none.
fn seig_groups(data: &[u8], parts: &[BoxRef]) -> Result<Vec<(u32, u32)>, DownloadError> {
    for sbgp in parts.iter().filter(|b| &b.kind == b"sbgp") {
        let body = sbgp.start + sbgp.header;
        if data.get(body + 4..body + 8) != Some(b"seig") {
            continue;
        }
        let mut pos = body + 8;
        if data[body] >= 1 {
            pos += 4;
        }
        let count = u32_at(data, pos).ok_or_else(|| bad("sbgp cut short"))?;
        pos += 4;
        let mut runs = Vec::new();
        for _ in 0..count {
            runs.push((
                u32_at(data, pos).ok_or_else(|| bad("sbgp cut short"))?,
                u32_at(data, pos + 4).ok_or_else(|| bad("sbgp cut short"))?,
            ));
            pos += 8;
        }
        return Ok(runs);
    }
    Ok(Vec::new())
}

/// Decrypts one sample's encrypted runs in place.
fn decrypt_sample(
    sample: &mut [u8],
    protection: &Protection,
    key: &[u8; 16],
    iv: &[u8],
    subsamples: &[(usize, usize)],
) -> Result<(), DownloadError> {
    let mut counter = [0u8; 16];
    match iv.len() {
        8 => counter[..8].copy_from_slice(iv),
        16 => counter.copy_from_slice(iv),
        n => return Err(bad(format!("sample IV of {n} bytes"))),
    }
    let whole: Vec<(usize, usize)>;
    let runs = if subsamples.is_empty() {
        whole = vec![(0, sample.len())];
        &whole
    } else {
        subsamples
    };
    let (crypt, skip) = (
        usize::from(protection.crypt_blocks),
        usize::from(protection.skip_blocks),
    );
    let patterned = crypt > 0;
    match &protection.scheme {
        b"cbcs" | b"cbc1" => {
            let cipher = Aes128::new(key.into());
            let mut previous = counter;
            let mut pos = 0;
            for &(clear, encrypted) in runs {
                pos += clear;
                let end = pos + encrypted;
                if end > sample.len() {
                    return Err(bad("subsample runs past the end of its sample"));
                }
                if &protection.scheme == b"cbcs" {
                    previous = counter;
                }
                let region = &mut sample[pos..end];
                let blocks = region.len() / 16;
                let mut i = 0;
                while i < blocks {
                    let run = if patterned { crypt.min(blocks - i) } else { blocks - i };
                    for _ in 0..run {
                        let block: &mut [u8] = &mut region[i * 16..i * 16 + 16];
                        let ciphertext: [u8; 16] = block.try_into().unwrap();
                        let mut inner = aes::Block::from(ciphertext);
                        cipher.decrypt_block(&mut inner);
                        for (byte, prev) in inner.iter_mut().zip(previous.iter()) {
                            *byte ^= prev;
                        }
                        block.copy_from_slice(&inner);
                        previous = ciphertext;
                        i += 1;
                    }
                    if patterned {
                        i += skip;
                    }
                }
                pos = end;
            }
        }
        b"cenc" | b"cens" => {
            let mut ctr = ctr::Ctr128BE::<Aes128>::new(key.into(), &counter.into());
            let mut pos = 0;
            for &(clear, encrypted) in runs {
                pos += clear;
                let end = pos + encrypted;
                if end > sample.len() {
                    return Err(bad("subsample runs past the end of its sample"));
                }
                let region = &mut sample[pos..end];
                if patterned {
                    let blocks = region.len() / 16;
                    let mut i = 0;
                    while i < blocks {
                        let run = crypt.min(blocks - i);
                        ctr.apply_keystream(&mut region[i * 16..(i + run) * 16]);
                        i += run + skip;
                    }
                } else {
                    ctr.apply_keystream(region);
                }
                pos = end;
            }
        }
        _ => unreachable!("schemes are checked when the initialization section is read"),
    }
    Ok(())
}

/// A track fragment's samples: where each sits in the segment and how long it is, and
/// where the next fragment's data begins when it names no base of its own.
struct FragmentSamples {
    samples: Vec<(usize, usize)>,
    /// Each sample's duration, in the track's timescale.
    durations: Vec<u32>,
    next_base: usize,
}

fn fragment_samples(
    data: &[u8],
    parts: &[BoxRef],
    moof_start: usize,
    running_base: usize,
    track: &Track,
) -> Result<FragmentSamples, DownloadError> {
    let tfhd = child(parts, b"tfhd").ok_or_else(|| bad("traf without tfhd"))?;
    let body = tfhd.start + tfhd.header;
    let flags = u32_at(data, body).ok_or_else(|| bad("tfhd cut short"))? & 0x00ff_ffff;
    let mut pos = body + 8;
    let mut base = if flags & 0x2_0000 != 0 {
        moof_start
    } else {
        running_base
    };
    if flags & 1 != 0 {
        base = usize::try_from(u64_at(data, pos).ok_or_else(|| bad("tfhd cut short"))?)
            .map_err(|_| bad("tfhd base offset too large"))?;
        pos += 8;
    }
    if flags & 2 != 0 {
        pos += 4;
    }
    let default_duration = if flags & 8 != 0 {
        pos += 4;
        u32_at(data, pos - 4).ok_or_else(|| bad("tfhd cut short"))?
    } else {
        track.default_sample_duration
    };
    let default_size = if flags & 0x10 != 0 {
        u32_at(data, pos).ok_or_else(|| bad("tfhd cut short"))?
    } else {
        track.default_sample_size
    };
    let mut samples = Vec::new();
    let mut durations = Vec::new();
    let mut data_pos = base;
    for trun in parts.iter().filter(|b| &b.kind == b"trun") {
        let body = trun.start + trun.header;
        let flags = u32_at(data, body).ok_or_else(|| bad("trun cut short"))? & 0x00ff_ffff;
        let count = u32_at(data, body + 4).ok_or_else(|| bad("trun cut short"))?;
        let mut pos = body + 8;
        let mut offset = data_pos;
        if flags & 1 != 0 {
            let relative = u32_at(data, pos).ok_or_else(|| bad("trun cut short"))? as i32;
            offset = usize::try_from(base as i64 + i64::from(relative))
                .map_err(|_| bad("trun data offset before the segment"))?;
            pos += 4;
        }
        if flags & 4 != 0 {
            pos += 4;
        }
        for _ in 0..count {
            let duration = if flags & 0x100 != 0 {
                pos += 4;
                u32_at(data, pos - 4).ok_or_else(|| bad("trun cut short"))?
            } else {
                default_duration
            };
            durations.push(duration);
            let size = if flags & 0x200 != 0 {
                let size = u32_at(data, pos).ok_or_else(|| bad("trun cut short"))?;
                pos += 4;
                size
            } else {
                default_size
            };
            if flags & 0x400 != 0 {
                pos += 4;
            }
            if flags & 0x800 != 0 {
                pos += 4;
            }
            samples.push((offset, size as usize));
            offset += size as usize;
        }
        data_pos = offset;
    }
    Ok(FragmentSamples {
        samples,
        durations,
        next_base: data_pos,
    })
}

/// One sample of a text track: when it starts and how long it lasts, in the track's
/// timescale, and its bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextSample {
    pub time: u64,
    pub duration: u32,
    pub bytes: Vec<u8>,
}

/// The samples of the segment's track fragments with their times: the track's timescale
/// and the samples in order.
pub fn text_samples(data: &[u8], init: &Init) -> Result<(u32, Vec<TextSample>), DownloadError> {
    let mut out = Vec::new();
    let mut timescale = 0u32;
    for moof in boxes(data, 0..data.len())?
        .iter()
        .filter(|b| &b.kind == b"moof")
    {
        let mut running_base = moof.start;
        for traf in boxes(data, moof.body())?
            .iter()
            .filter(|b| &b.kind == b"traf")
        {
            let parts = boxes(data, traf.body())?;
            let tfhd = child(&parts, b"tfhd").ok_or_else(|| bad("traf without tfhd"))?;
            let track_id = u32_at(data, tfhd.start + tfhd.header + 4)
                .ok_or_else(|| bad("tfhd cut short"))?;
            let track = init
                .tracks
                .iter()
                .find(|t| t.id == track_id)
                .ok_or_else(|| {
                    bad(format!(
                        "fragment for track {track_id}, which the initialization section does not list"
                    ))
                })?;
            timescale = track.timescale;
            let tfdt = child(&parts, b"tfdt").ok_or_else(|| bad("traf without tfdt"))?;
            let body = tfdt.start + tfdt.header;
            let mut time = if data[body] == 1 {
                u64_at(data, body + 4).ok_or_else(|| bad("tfdt cut short"))?
            } else {
                u64::from(u32_at(data, body + 4).ok_or_else(|| bad("tfdt cut short"))?)
            };
            let FragmentSamples {
                samples,
                durations,
                next_base,
            } = fragment_samples(data, &parts, moof.start, running_base, track)?;
            running_base = next_base;
            for ((offset, size), duration) in samples.iter().zip(durations) {
                let bytes = data
                    .get(*offset..offset + size)
                    .ok_or_else(|| bad("sample lies outside the segment"))?
                    .to_vec();
                out.push(TextSample {
                    time,
                    duration,
                    bytes,
                });
                time += u64::from(duration);
            }
        }
    }
    Ok((timescale, out))
}

/// A WebVTT cue as a `wvtt` sample carries it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebvttCue {
    pub id: Option<String>,
    pub settings: Option<String>,
    pub text: String,
}

/// The cues of one `wvtt` sample: none for an empty sample.
pub fn wvtt_cues(sample: &[u8]) -> Result<Vec<WebvttCue>, DownloadError> {
    let mut cues = Vec::new();
    for item in boxes(sample, 0..sample.len())? {
        if &item.kind != b"vttc" {
            continue;
        }
        let mut cue = WebvttCue {
            id: None,
            settings: None,
            text: String::new(),
        };
        for part in boxes(sample, item.body())? {
            let text = String::from_utf8_lossy(&sample[part.body()]).into_owned();
            match &part.kind {
                b"iden" => cue.id = Some(text),
                b"sttg" => cue.settings = Some(text),
                b"payl" => cue.text = text,
                _ => {}
            }
        }
        cues.push(cue);
    }
    Ok(cues)
}

/// The top-level boxes of a file or segment: each kind with where it starts and ends.
pub fn top_level(data: &[u8]) -> Result<Vec<([u8; 4], usize, usize)>, DownloadError> {
    Ok(boxes(data, 0..data.len())?
        .into_iter()
        .map(|b| (b.kind, b.start, b.end))
        .collect())
}

/// One reference of a segment index: a subsegment, or another index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidxReference {
    pub is_index: bool,
    pub size: u32,
    /// In the index's timescale.
    pub duration: u32,
}

/// A `sidx` box: what lies after it and how long each piece plays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sidx {
    pub timescale: u32,
    pub earliest_presentation_time: u64,
    /// Bytes between the end of the index box and the first piece it references.
    pub first_offset: u64,
    /// The index box's own length.
    pub len: usize,
    pub references: Vec<SidxReference>,
}

/// Reads the `sidx` box `data` begins with.
pub fn parse_sidx(data: &[u8]) -> Result<Sidx, DownloadError> {
    let top = boxes(data, 0..data.len())?;
    let sidx = top
        .first()
        .filter(|b| &b.kind == b"sidx")
        .ok_or_else(|| bad("segment index expected where the manifest points"))?;
    let body = sidx.start + sidx.header;
    let version = data[body];
    let timescale = u32_at(data, body + 8).ok_or_else(|| bad("sidx cut short"))?;
    let mut pos = body + 12;
    let (earliest, first_offset) = if version == 0 {
        let e = u32_at(data, pos).ok_or_else(|| bad("sidx cut short"))?;
        let f = u32_at(data, pos + 4).ok_or_else(|| bad("sidx cut short"))?;
        pos += 8;
        (u64::from(e), u64::from(f))
    } else {
        let e = u64_at(data, pos).ok_or_else(|| bad("sidx cut short"))?;
        let f = u64_at(data, pos + 8).ok_or_else(|| bad("sidx cut short"))?;
        pos += 16;
        (e, f)
    };
    let count = u16_at(data, pos + 2).ok_or_else(|| bad("sidx cut short"))?;
    pos += 4;
    let mut references = Vec::with_capacity(usize::from(count));
    for _ in 0..count {
        let first = u32_at(data, pos).ok_or_else(|| bad("sidx cut short"))?;
        let duration = u32_at(data, pos + 4).ok_or_else(|| bad("sidx cut short"))?;
        references.push(SidxReference {
            is_index: first & 0x8000_0000 != 0,
            size: first & 0x7fff_ffff,
            duration,
        });
        pos += 12;
    }
    Ok(Sidx {
        timescale,
        earliest_presentation_time: earliest,
        first_offset,
        len: sidx.end - sidx.start,
        references,
    })
}

/// Decrypts every protected sample of a media segment with `key`, in place, and frees
/// the boxes that described the encryption, so the segment reads as plain media after
/// `init.cleared`.
pub fn decrypt_fragment(data: &[u8], init: &Init, key: &[u8; 16]) -> Result<Vec<u8>, DownloadError> {
    let mut out = data.to_vec();
    for moof in boxes(data, 0..data.len())?
        .iter()
        .filter(|b| &b.kind == b"moof")
    {
        let mut running_base = moof.start;
        for traf in boxes(data, moof.body())?
            .iter()
            .filter(|b| &b.kind == b"traf")
        {
            let parts = boxes(data, traf.body())?;
            let tfhd = child(&parts, b"tfhd").ok_or_else(|| bad("traf without tfhd"))?;
            let track_id = u32_at(data, tfhd.start + tfhd.header + 4)
                .ok_or_else(|| bad("tfhd cut short"))?;
            let track = init
                .tracks
                .iter()
                .find(|t| t.id == track_id)
                .ok_or_else(|| {
                    bad(format!(
                        "fragment for track {track_id}, which the initialization section does not list"
                    ))
                })?;
            let FragmentSamples {
                samples, next_base, ..
            } = fragment_samples(data, &parts, moof.start, running_base, track)?;
            running_base = next_base;
            let Some(default) = &track.protection else {
                continue;
            };
            let mut local_seig = Vec::new();
            for sgpd in parts.iter().filter(|b| &b.kind == b"sgpd") {
                let entries = seig_entries(data, sgpd, default.scheme)?;
                if !entries.is_empty() {
                    local_seig = entries;
                    free(&mut out, sgpd);
                }
            }
            let groups = seig_groups(data, &parts)?;
            let mut per_sample: Vec<&Protection> = Vec::with_capacity(samples.len());
            for (count, index) in &groups {
                let protection = match *index {
                    0 => default,
                    i if i > 0x1_0000 => local_seig
                        .get((i - 0x1_0001) as usize)
                        .ok_or_else(|| bad("sample group names a description the fragment lacks"))?,
                    i => track
                        .seig
                        .get((i - 1) as usize)
                        .ok_or_else(|| bad("sample group names a description the movie lacks"))?,
                };
                per_sample.extend(std::iter::repeat_n(protection, *count as usize));
            }
            per_sample.resize(samples.len(), default);
            let iv_size_of = |index: usize| usize::from(per_sample[index].iv_size);
            let infos = if let Some(senc) = child(&parts, b"senc") {
                let body = senc.start + senc.header;
                let flags = u32_at(data, body).ok_or_else(|| bad("senc cut short"))? & 0x00ff_ffff;
                let count = u32_at(data, body + 4).ok_or_else(|| bad("senc cut short"))? as usize;
                if count != samples.len() {
                    return Err(bad(format!(
                        "senc describes {count} samples, the fragment has {}",
                        samples.len()
                    )));
                }
                sample_infos(data, body + 8, count, flags & 2 != 0, iv_size_of)?
            } else if let (Some(saio), Some(saiz)) = (child(&parts, b"saio"), child(&parts, b"saiz")) {
                let saio_body = saio.start + saio.header;
                let saio_flags = u32_at(data, saio_body).ok_or_else(|| bad("saio cut short"))? & 0x00ff_ffff;
                let mut pos = saio_body + 4 + if saio_flags & 1 != 0 { 8 } else { 0 };
                let entries = u32_at(data, pos).ok_or_else(|| bad("saio cut short"))?;
                pos += 4;
                if entries == 0 {
                    return Err(bad("saio names no offset"));
                }
                let offset = if data[saio_body] == 1 {
                    u64_at(data, pos).ok_or_else(|| bad("saio cut short"))? as usize
                } else {
                    u32_at(data, pos).ok_or_else(|| bad("saio cut short"))? as usize
                };
                let saiz_body = saiz.start + saiz.header;
                let saiz_flags = u32_at(data, saiz_body).ok_or_else(|| bad("saiz cut short"))? & 0x00ff_ffff;
                let pos = saiz_body + 4 + if saiz_flags & 1 != 0 { 8 } else { 0 };
                let default_size = data[pos];
                let count = u32_at(data, pos + 1).ok_or_else(|| bad("saiz cut short"))? as usize;
                let subsampled = if default_size == 0 {
                    data.get(pos + 5..pos + 5 + count)
                        .ok_or_else(|| bad("saiz cut short"))?
                        .iter()
                        .enumerate()
                        .any(|(i, size)| usize::from(*size) > iv_size_of(i))
                } else {
                    usize::from(default_size) > iv_size_of(0)
                };
                sample_infos(data, moof.start + offset, count, subsampled, iv_size_of)?
            } else if per_sample.iter().any(|p| p.protected) {
                return Err(bad("protected fragment without sample encryption information"));
            } else {
                Vec::new()
            };
            for (index, ((offset, size), protection)) in samples.iter().zip(&per_sample).enumerate() {
                if !protection.protected {
                    continue;
                }
                let info = &infos[index];
                let iv: &[u8] = if protection.iv_size == 0 {
                    protection
                        .constant_iv
                        .as_deref()
                        .ok_or_else(|| bad("protected sample with neither IV nor constant IV"))?
                } else {
                    &info.iv
                };
                let sample = out
                    .get_mut(*offset..offset + size)
                    .ok_or_else(|| bad("sample lies outside the segment"))?;
                decrypt_sample(sample, protection, key, iv, &info.subsamples)?;
            }
            for name in [b"senc", b"saio", b"saiz"] {
                if let Some(item) = child(&parts, name) {
                    free(&mut out, &item);
                }
            }
            for sbgp in parts.iter().filter(|b| &b.kind == b"sbgp") {
                let body = sbgp.start + sbgp.header;
                if data.get(body + 4..body + 8) == Some(b"seig") {
                    free(&mut out, sbgp);
                }
            }
        }
    }
    Ok(out)
}

/// The earliest time any track fragment of the segment starts at, in seconds.
pub fn start_time(data: &[u8], init: &Init) -> Option<f64> {
    let mut earliest: Option<f64> = None;
    for moof in boxes(data, 0..data.len()).ok()?.iter().filter(|b| &b.kind == b"moof") {
        for traf in boxes(data, moof.body()).ok()?.iter().filter(|b| &b.kind == b"traf") {
            let parts = boxes(data, traf.body()).ok()?;
            let tfhd = child(&parts, b"tfhd")?;
            let track_id = u32_at(data, tfhd.start + tfhd.header + 4)?;
            let tfdt = child(&parts, b"tfdt")?;
            let body = tfdt.start + tfdt.header;
            let time = if data[body] == 1 {
                u64_at(data, body + 4)? as f64
            } else {
                f64::from(u32_at(data, body + 4)?)
            };
            let timescale = init.tracks.iter().find(|t| t.id == track_id)?.timescale;
            if timescale == 0 {
                continue;
            }
            let seconds = time / f64::from(timescale);
            earliest = Some(earliest.map_or(seconds, |e| e.min(seconds)));
        }
    }
    earliest
}

/// Whether `bytes` are an ISO base media file: a box of a known top-level kind first.
pub fn is_mp4(bytes: &[u8]) -> bool {
    bytes.len() >= 8
        && matches!(
            &bytes[4..8],
            b"ftyp" | b"styp" | b"moov" | b"moof" | b"sidx" | b"free" | b"skip" | b"prft" | b"emsg"
        )
}

/// Building blocks for tests: boxes written by hand, and fragments encrypted the way a
/// packager would.
#[cfg(test)]
pub mod build {
    use super::*;
    use aes::cipher::BlockCipherEncrypt;

    pub fn full_box(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
        let mut inner = vec![version];
        inner.extend_from_slice(&flags.to_be_bytes()[1..]);
        inner.extend_from_slice(body);
        plain_box(kind, &inner)
    }

    pub fn plain_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    /// A `tenc` box for `scheme` with a pattern and a constant or per-sample IV.
    pub fn tenc(crypt: u8, skip: u8, iv_size: u8, constant_iv: Option<&[u8]>) -> Vec<u8> {
        let mut body = vec![0u8, (crypt << 4) | skip, 1, iv_size];
        body.extend_from_slice(&[0x11u8; 16]);
        if let Some(iv) = constant_iv {
            body.push(iv.len() as u8);
            body.extend_from_slice(iv);
        }
        full_box(b"tenc", if crypt > 0 || skip > 0 { 1 } else { 0 }, 0, &body)
    }

    /// A `sinf` box wrapping `format` under `scheme`.
    pub fn sinf(format: &[u8; 4], scheme: &[u8; 4], tenc: &[u8]) -> Vec<u8> {
        let mut body = plain_box(b"frma", format);
        let mut schm = scheme.to_vec();
        schm.extend_from_slice(&0x10000u32.to_be_bytes());
        body.extend_from_slice(&full_box(b"schm", 0, 0, &schm));
        body.extend_from_slice(&plain_box(b"schi", tenc));
        plain_box(b"sinf", &body)
    }

    /// A step down the box tree: the kind, and which box of that kind among its siblings.
    pub type Step<'a> = (&'a [u8; 4], usize);

    /// Appends `extra` inside the box found by following `path` from the top of `data`,
    /// where a sample entry or `stsd` on the path is entered past its fixed fields, and
    /// grows every box on the path to fit.
    pub fn append_within(data: &[u8], path: &[Step<'_>], extra: &[u8]) -> Vec<u8> {
        fn locate(data: &[u8], range: Range<usize>, path: &[Step<'_>]) -> Vec<BoxRef> {
            let Some(((first, nth), rest)) = path.split_first() else {
                return Vec::new();
            };
            let found = boxes(data, range)
                .unwrap()
                .into_iter()
                .filter(|b| &&b.kind == first)
                .nth(*nth)
                .unwrap_or_else(|| panic!("no {} box", String::from_utf8_lossy(*first)));
            let inner = if &found.kind == b"stsd" {
                found.start + found.header + 8..found.end
            } else if rest.is_empty() {
                found.body()
            } else {
                match sample_entry_children(data, &found) {
                    Ok(at) => at..found.end,
                    Err(_) => found.body(),
                }
            };
            let mut chain = vec![found];
            chain.extend(locate(data, inner, rest));
            chain
        }
        let chain = locate(data, 0..data.len(), path);
        let target = *chain.last().unwrap();
        let mut out = data[..target.end].to_vec();
        out.extend_from_slice(extra);
        out.extend_from_slice(&data[target.end..]);
        for item in &chain {
            let size = u32_at(&out, item.start).unwrap() + extra.len() as u32;
            out[item.start..item.start + 4].copy_from_slice(&size.to_be_bytes());
        }
        out
    }

    /// Renames the box at `path` to `kind`.
    pub fn rename(data: &mut [u8], path: &[Step<'_>], kind: &[u8; 4]) {
        let mut range = 0..data.len();
        let mut found = None;
        for (depth, (step, nth)) in path.iter().enumerate() {
            let item = boxes(data, range.clone())
                .unwrap()
                .into_iter()
                .filter(|b| &&b.kind == step)
                .nth(*nth)
                .unwrap_or_else(|| panic!("no {} box", String::from_utf8_lossy(*step)));
            range = if &item.kind == b"stsd" {
                item.start + item.header + 8..item.end
            } else if depth + 1 < path.len() {
                sample_entry_children(data, &item)
                    .map(|at| at..item.end)
                    .unwrap_or(item.body())
            } else {
                item.body()
            };
            found = Some(item);
        }
        let item = found.unwrap();
        data[item.start + 4..item.start + 8].copy_from_slice(kind);
    }

    /// Encrypts one sample's runs with the `cbcs` pattern, as the packager's side of
    /// [`decrypt_sample`].
    pub fn encrypt_cbcs(sample: &mut [u8], key: &[u8; 16], iv: &[u8; 16], crypt: usize, skip: usize, subsamples: &[(usize, usize)]) {
        let cipher = Aes128::new(key.into());
        let mut pos = 0;
        for &(clear, encrypted) in subsamples {
            pos += clear;
            let region = &mut sample[pos..pos + encrypted];
            let mut previous = *iv;
            let blocks = region.len() / 16;
            let mut i = 0;
            while i < blocks {
                let run = if crypt > 0 { crypt.min(blocks - i) } else { blocks - i };
                for _ in 0..run {
                    let block = &mut region[i * 16..i * 16 + 16];
                    let mut inner: [u8; 16] = block.try_into().unwrap();
                    for (byte, prev) in inner.iter_mut().zip(previous.iter()) {
                        *byte ^= prev;
                    }
                    let mut b = aes::Block::from(inner);
                    cipher.encrypt_block(&mut b);
                    block.copy_from_slice(&b);
                    previous = b.into();
                    i += 1;
                }
                if crypt > 0 {
                    i += skip;
                }
            }
            pos += encrypted;
        }
    }

    /// Turns a plain fragment (moof + mdat, with default-base-is-moof) into a `cbcs`
    /// one: every sample encrypted past `clear_lead` bytes with the 1:9 pattern and a
    /// per-sample IV, described by a `senc` box added to each track fragment.
    pub fn encrypt_fragment_cbcs(data: &[u8], key: &[u8; 16], iv: &[u8; 16], clear_lead: usize) -> Vec<u8> {
        let track = Track {
            id: 0,
            timescale: 1,
            default_sample_size: 0,
            default_sample_duration: 0,
            protection: None,
            seig: Vec::new(),
        };
        let mut out = data.to_vec();
        let top = boxes(data, 0..data.len()).unwrap();
        // Every sample is encrypted where it lies before any box is added, since adding
        // one moves the data after it.
        let mut sencs: Vec<(usize, Vec<u8>)> = Vec::new();
        for moof in top.iter().filter(|b| &b.kind == b"moof") {
            let mut running_base = moof.start;
            for traf in boxes(data, moof.body()).unwrap().iter().filter(|b| &b.kind == b"traf") {
                let parts = boxes(data, traf.body()).unwrap();
                let FragmentSamples {
                    samples, next_base, ..
                } = fragment_samples(data, &parts, moof.start, running_base, &track).unwrap();
                running_base = next_base;
                let mut senc_body = (samples.len() as u32).to_be_bytes().to_vec();
                for (offset, size) in &samples {
                    let clear = clear_lead.min(*size);
                    let subsamples = [(clear, size - clear)];
                    encrypt_cbcs(&mut out[*offset..offset + size], key, iv, 1, 9, &subsamples);
                    senc_body.extend_from_slice(iv);
                    senc_body.extend_from_slice(&1u16.to_be_bytes());
                    senc_body.extend_from_slice(&(clear as u16).to_be_bytes());
                    senc_body.extend_from_slice(&((size - clear) as u32).to_be_bytes());
                }
                sencs.push((traf.start, full_box(b"senc", 0, 2, &senc_body)));
            }
        }
        // Work from the last moof backwards so earlier offsets stay valid.
        for moof in top.iter().filter(|b| &b.kind == b"moof").rev() {
            let trafs: Vec<BoxRef> = boxes(&out, moof.body())
                .unwrap()
                .into_iter()
                .filter(|b| &b.kind == b"traf")
                .collect();
            let mut inserted = 0usize;
            for traf in trafs.iter().rev() {
                let senc = sencs
                    .iter()
                    .find(|(start, _)| *start == traf.start)
                    .map(|(_, senc)| senc.clone())
                    .unwrap();
                let mut grown = out[..traf.end].to_vec();
                grown.extend_from_slice(&senc);
                grown.extend_from_slice(&out[traf.end..]);
                out = grown;
                for item in [traf, moof] {
                    let size = u32_at(&out, item.start).unwrap() + senc.len() as u32;
                    out[item.start..item.start + 4].copy_from_slice(&size.to_be_bytes());
                }
                inserted += senc.len();
            }
            // Sample data moved by what was inserted before it. Every trun's offset
            // is relative to the moof, so it grows by the same amount.
            let moof_now = boxes(&out, moof.start..out.len()).unwrap()[0];
            for traf in boxes(&out, moof_now.body()).unwrap().iter().filter(|b| &b.kind == b"traf") {
                for trun in boxes(&out, traf.body()).unwrap().iter().filter(|b| &b.kind == b"trun") {
                    let body = trun.start + trun.header;
                    if u32_at(&out, body).unwrap() & 1 != 0 {
                        let offset = u32_at(&out, body + 8).unwrap() as i32 + inserted as i32;
                        out[body + 8..body + 12].copy_from_slice(&offset.to_be_bytes());
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::build::*;
    use super::*;

    fn pattern(len: usize) -> Vec<u8> {
        (0..len as u64)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect()
    }

    fn cbcs_init(iv_size: u8, constant_iv: Option<&[u8]>) -> Init {
        Init {
            cleared: Vec::new(),
            tracks: vec![Track {
                id: 1,
                timescale: 90_000,
                default_sample_size: 0,
                default_sample_duration: 0,
                protection: Some(Protection {
                    scheme: *b"cbcs",
                    protected: true,
                    iv_size,
                    constant_iv: constant_iv.map(|iv| iv.to_vec()),
                    crypt_blocks: 1,
                    skip_blocks: 9,
                }),
                seig: Vec::new(),
            }],
        }
    }

    /// A moof with one traf of `sizes` samples and a trun pointing at the mdat after it.
    fn fragment(sizes: &[usize], samples: &[u8], tfdt: u64) -> Vec<u8> {
        let tfhd = full_box(b"tfhd", 0, 0x2_0000, &1u32.to_be_bytes());
        let tfdt = full_box(b"tfdt", 1, 0, &tfdt.to_be_bytes());
        let mut trun_body = (sizes.len() as u32).to_be_bytes().to_vec();
        trun_body.extend_from_slice(&0i32.to_be_bytes());
        for size in sizes {
            trun_body.extend_from_slice(&(*size as u32).to_be_bytes());
        }
        let trun = full_box(b"trun", 0, 0x201, &trun_body);
        let mut traf_body = tfhd;
        traf_body.extend_from_slice(&tfdt);
        traf_body.extend_from_slice(&trun);
        let traf = plain_box(b"traf", &traf_body);
        let mut moof_body = full_box(b"mfhd", 0, 0, &1u32.to_be_bytes());
        moof_body.extend_from_slice(&traf);
        let mut moof = plain_box(b"moof", &moof_body);
        let data_offset = (moof.len() + 8) as i32;
        // The trun's data_offset sits 8 bytes into its body.
        let trun_at = moof.len() - trun.len() + 8 + 8;
        moof[trun_at..trun_at + 4].copy_from_slice(&data_offset.to_be_bytes());
        moof.extend_from_slice(&plain_box(b"mdat", samples));
        moof
    }

    #[test]
    fn cbcs_samples_are_decrypted_per_subsample_with_the_pattern() {
        let key = [3u8; 16];
        let iv = [9u8; 16];
        let sizes = [500usize, 40, 700];
        let plain = pattern(sizes.iter().sum());
        let fragment = fragment(&sizes, &plain, 180_000);
        let encrypted = encrypt_fragment_cbcs(&fragment, &key, &iv, 32);
        assert_ne!(encrypted, fragment);
        let init = cbcs_init(16, None);
        let decrypted = decrypt_fragment(&encrypted, &init, &key).unwrap();
        let top = boxes(&decrypted, 0..decrypted.len()).unwrap();
        let mdat = child(&top, b"mdat").unwrap();
        assert_eq!(&decrypted[mdat.body()], &plain[..]);
        // The encryption boxes are gone from the demuxer's view.
        let moof = child(&top, b"moof").unwrap();
        let traf = child(&boxes(&decrypted, moof.body()).unwrap(), b"traf").unwrap();
        let parts = boxes(&decrypted, traf.body()).unwrap();
        assert!(child(&parts, b"senc").is_none());
        assert!(child(&parts, b"free").is_some());
        assert_eq!(start_time(&decrypted, &init), Some(2.0));

        // A wrong key leaves the samples scrambled rather than failing.
        let wrong = decrypt_fragment(&encrypted, &init, &[4u8; 16]).unwrap();
        assert_ne!(&wrong[mdat.body()], &plain[..]);
    }

    #[test]
    fn a_constant_iv_serves_every_sample() {
        let key = [5u8; 16];
        let iv = [7u8; 16];
        let sizes = [100usize, 300];
        let plain = pattern(400);
        let mut samples = plain.clone();
        encrypt_cbcs(&mut samples[..100], &key, &iv, 1, 9, &[(0, 100)]);
        encrypt_cbcs(&mut samples[100..], &key, &iv, 1, 9, &[(0, 300)]);
        let mut fragment = fragment(&sizes, &samples, 0);
        // A senc with no IVs and no subsamples: two samples, wholly encrypted.
        let senc = full_box(b"senc", 0, 0, &2u32.to_be_bytes());
        fragment = append_within(&fragment, &[(b"moof", 0), (b"traf", 0)], &senc);
        // The trun's data offset moved by the senc.
        let moof = boxes(&fragment, 0..fragment.len()).unwrap()[0];
        let traf = child(&boxes(&fragment, moof.body()).unwrap(), b"traf").unwrap();
        let trun = child(&boxes(&fragment, traf.body()).unwrap(), b"trun").unwrap();
        let at = trun.start + trun.header + 8;
        let offset = u32_at(&fragment, at).unwrap() + senc.len() as u32;
        fragment[at..at + 4].copy_from_slice(&offset.to_be_bytes());
        let init = cbcs_init(0, Some(&iv));
        let decrypted = decrypt_fragment(&fragment, &init, &key).unwrap();
        let top = boxes(&decrypted, 0..decrypted.len()).unwrap();
        let mdat = child(&top, b"mdat").unwrap();
        assert_eq!(&decrypted[mdat.body()], &plain[..]);
    }

    #[test]
    fn cenc_samples_are_one_counter_stream_across_subsamples() {
        use aes::cipher::KeyIvInit;
        let key = [8u8; 16];
        let iv = [1, 2, 3, 4, 5, 6, 7, 8];
        let plain = pattern(200);
        let mut sample = plain.clone();
        // Clear 20, encrypted 70, clear 10, encrypted 100: one keystream over the 170.
        let mut counter = [0u8; 16];
        counter[..8].copy_from_slice(&iv);
        let mut ctr = ctr::Ctr128BE::<Aes128>::new(&key.into(), &counter.into());
        ctr.apply_keystream(&mut sample[20..90]);
        ctr.apply_keystream(&mut sample[100..200]);
        let protection = Protection {
            scheme: *b"cenc",
            protected: true,
            iv_size: 8,
            constant_iv: None,
            crypt_blocks: 0,
            skip_blocks: 0,
        };
        decrypt_sample(&mut sample, &protection, &key, &iv, &[(20, 70), (10, 100)]).unwrap();
        assert_eq!(sample, plain);
    }

    #[test]
    fn an_initialization_section_is_read_and_cleared() {
        // moov: mvhd-less, one trak with tkhd, mdia (mdhd, minf (stbl (stsd (encv (sinf))))),
        // and an mvex with a trex default size, plus a pssh.
        let tkhd_body = {
            let mut b = vec![0u8; 8];
            b.extend_from_slice(&1u32.to_be_bytes());
            b.resize(80, 0);
            b
        };
        let tkhd = full_box(b"tkhd", 0, 7, &tkhd_body);
        let mut mdhd_body = vec![0u8; 8];
        mdhd_body.extend_from_slice(&90_000u32.to_be_bytes());
        mdhd_body.resize(20, 0);
        let mdhd = full_box(b"mdhd", 0, 0, &mdhd_body);
        let mut visual = vec![0u8; 78];
        visual[7] = 1;
        let avc1 = plain_box(b"avc1", &visual);
        let mut stsd_body = 1u32.to_be_bytes().to_vec();
        stsd_body.extend_from_slice(&avc1);
        let stsd = full_box(b"stsd", 0, 0, &stsd_body);
        let stbl = plain_box(b"stbl", &stsd);
        let minf = plain_box(b"minf", &stbl);
        let mut mdia_body = mdhd;
        mdia_body.extend_from_slice(&minf);
        let mdia = plain_box(b"mdia", &mdia_body);
        let mut trak_body = tkhd;
        trak_body.extend_from_slice(&mdia);
        let trak = plain_box(b"trak", &trak_body);
        let mut trex_body = 1u32.to_be_bytes().to_vec();
        trex_body.extend_from_slice(&1u32.to_be_bytes());
        trex_body.extend_from_slice(&0u32.to_be_bytes());
        trex_body.extend_from_slice(&1234u32.to_be_bytes());
        trex_body.extend_from_slice(&0u32.to_be_bytes());
        let mvex = plain_box(b"mvex", &full_box(b"trex", 0, 0, &trex_body));
        let mut moov_body = trak;
        moov_body.extend_from_slice(&mvex);
        moov_body.extend_from_slice(&full_box(b"pssh", 0, 0, &[0u8; 20]));
        let plain = plain_box(b"moov", &moov_body);

        let iv = [6u8; 16];
        let sinf = sinf(b"avc1", b"cbcs", &tenc(1, 9, 0, Some(&iv)));
        let mut protected = append_within(
            &plain,
            &[(b"moov", 0), (b"trak", 0), (b"mdia", 0), (b"minf", 0), (b"stbl", 0), (b"stsd", 0), (b"avc1", 0)],
            &sinf,
        );
        rename(
            &mut protected,
            &[(b"moov", 0), (b"trak", 0), (b"mdia", 0), (b"minf", 0), (b"stbl", 0), (b"stsd", 0), (b"avc1", 0)],
            b"encv",
        );
        let init = read_init(&protected).unwrap();
        assert_eq!(init.tracks.len(), 1);
        let track = &init.tracks[0];
        assert_eq!(track.id, 1);
        assert_eq!(track.timescale, 90_000);
        assert_eq!(track.default_sample_size, 1234);
        assert_eq!(
            track.protection,
            Some(Protection {
                scheme: *b"cbcs",
                protected: true,
                iv_size: 0,
                constant_iv: Some(iv.to_vec()),
                crypt_blocks: 1,
                skip_blocks: 9,
            })
        );
        // Cleared: the entry is avc1 again, the sinf and pssh are free boxes.
        let moov = boxes(&init.cleared, 0..init.cleared.len()).unwrap()[0];
        let kinds: Vec<String> = boxes(&init.cleared, moov.body())
            .unwrap()
            .iter()
            .map(|b| b.name())
            .collect();
        assert_eq!(kinds, vec!["trak", "mvex", "free"]);
        assert!(init.cleared.windows(4).any(|w| w == b"avc1"));
        assert!(!init.cleared.windows(4).any(|w| w == b"encv"));
        assert!(!init.cleared.windows(4).any(|w| w == b"sinf"));
        assert_eq!(init.cleared.len(), protected.len());

        // A plain section reads as unprotected. Only its pssh is freed.
        let plain_init = read_init(&plain).unwrap();
        assert!(plain_init.tracks[0].protection.is_none());
        let mut expected = plain.clone();
        rename(&mut expected, &[(b"moov", 0), (b"pssh", 0)], b"free");
        assert_eq!(plain_init.cleared, expected);

        // An unknown scheme is refused.
        let mut other = append_within(
            &plain,
            &[(b"moov", 0), (b"trak", 0), (b"mdia", 0), (b"minf", 0), (b"stbl", 0), (b"stsd", 0), (b"avc1", 0)],
            &super::build::sinf(b"avc1", b"abcd", &tenc(0, 0, 16, None)),
        );
        rename(
            &mut other,
            &[(b"moov", 0), (b"trak", 0), (b"mdia", 0), (b"minf", 0), (b"stbl", 0), (b"stsd", 0), (b"avc1", 0)],
            b"encv",
        );
        assert!(matches!(read_init(&other), Err(DownloadError::Segment(m)) if m.contains("abcd")));
    }
}
