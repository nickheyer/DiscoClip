//! Download DASH SegmentBase, SegmentList and SegmentTemplate representations, including
//! timelines and text tracks. Join periods into one recording.
//!
//! Reload dynamic manifests until completion, capture limit or failure. Reject Common
//! Encryption as DRM.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use dash_mpd::{AdaptationSet, MPD, Period, Representation, SegmentBase, SegmentList, SegmentTemplate};
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
use crate::resolve::dash::{presentation_protection, protection_of};
use crate::resolve::{SubtitleFormat, SubtitleTrack, Variant, VariantKind, parse_codecs};

const MAX_MANIFEST: usize = 16 * 1024 * 1024;
const CONCURRENCY: usize = 4;
/// Reloads of a dynamic manifest that may fail in a row before the stream counts as over.
const RELOAD_FAILURES: u32 = 3;

pub struct DashDownloader {
    http: Http,
    ffmpeg: Ffmpeg,
}

impl DashDownloader {
    pub fn new(http: Http, ffmpeg: Ffmpeg) -> Self {
        Self { http, ffmpeg }
    }
}

fn manifest_error(message: impl Into<String>) -> DownloadError {
    DownloadError::Manifest(message.into())
}

/// A byte range as manifests write them: `first-last`.
fn parse_range(text: &str) -> Option<(u64, u64)> {
    let (start, end) = text.trim().split_once('-')?;
    let start = start.trim().parse().ok()?;
    let end = end.trim().parse().ok()?;
    (end >= start).then_some((start, end))
}

/// `parent` with the first of `bases` joined onto it.
fn joined(parent: &Url, bases: &[dash_mpd::BaseURL]) -> Result<Url, DownloadError> {
    match bases.first() {
        None => Ok(parent.clone()),
        Some(base) => parent
            .join(base.base.trim())
            .map_err(|e| manifest_error(format!("bad BaseURL {}: {e}", base.base))),
    }
}

/// A segment template with its `$Name$` and `$Name%0Nd$` identifiers filled in.
fn fill_template(template: &str, id: &str, bandwidth: u64, number: Option<u64>, time: Option<u64>) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    while let Some(start) = rest.find('$') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('$') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let token = &after[..end];
        rest = &after[end + 1..];
        if token.is_empty() {
            out.push('$');
            continue;
        }
        let (name, format) = token
            .split_once('%')
            .map_or((token, None), |(n, f)| (n, Some(f)));
        let width = format
            .and_then(|f| f.trim_start_matches('0').trim_end_matches('d').parse::<usize>().ok())
            .unwrap_or(0);
        let value = match name {
            "RepresentationID" => id.to_string(),
            "Bandwidth" => bandwidth.to_string(),
            "Number" => number.map(|n| n.to_string()).unwrap_or_default(),
            "Time" => time.map(|t| t.to_string()).unwrap_or_default(),
            other => format!("${other}$"),
        };
        if width > 0 && name != "RepresentationID" {
            out.push_str(&format!("{value:0>width$}"));
        } else {
            out.push_str(&value);
        }
    }
    out.push_str(rest);
    out
}

/// A segment template as the representation sees it: every attribute from the most
/// specific of the period's, the adaptation set's and the representation's templates.
fn merged_template(levels: [Option<&SegmentTemplate>; 3]) -> Option<SegmentTemplate> {
    let mut merged: Option<SegmentTemplate> = None;
    for level in levels.into_iter().flatten() {
        let out = merged.get_or_insert_with(SegmentTemplate::default);
        macro_rules! take {
            ($($field:ident),*) => {
                $(if level.$field.is_some() { out.$field = level.$field.clone(); })*
            };
        }
        take!(
            media,
            index,
            initialization,
            indexRange,
            startNumber,
            endNumber,
            duration,
            timescale,
            presentationTimeOffset,
            availabilityTimeOffset,
            Initialization,
            SegmentTimeline
        );
    }
    merged
}

/// Whether the manifest describes a stream still going on.
fn is_dynamic(mpd: &MPD) -> bool {
    mpd.mpdtype
        .as_deref()
        .is_some_and(|t| t.eq_ignore_ascii_case("dynamic"))
}

/// Each period's start and length in seconds: from its own attributes, else from its
/// neighbours and the presentation's length.
fn period_bounds(mpd: &MPD) -> Vec<(f64, Option<f64>)> {
    let mut bounds = Vec::with_capacity(mpd.periods.len());
    let mut cursor = 0.0f64;
    for period in &mpd.periods {
        let start = period.start.map_or(cursor, |s| s.as_secs_f64());
        bounds.push((start, period.duration.map(|d| d.as_secs_f64())));
        cursor = start + period.duration.map_or(0.0, |d| d.as_secs_f64());
    }
    let total = mpd.mediaPresentationDuration.map(|d| d.as_secs_f64());
    for index in 0..bounds.len() {
        if bounds[index].1.is_none() {
            let end = bounds.get(index + 1).map(|b| b.0).or(total);
            bounds[index].1 = end.map(|e| (e - bounds[index].0).max(0.0));
        }
    }
    bounds
}

/// When segments of a live stream become available: the wall clock now and the
/// presentation's availability start, in seconds since the epoch, and how far back the
/// stream keeps them.
#[derive(Debug, Clone, Copy)]
struct Clock {
    now: f64,
    start: f64,
    time_shift: Option<f64>,
}

fn clock_of(mpd: &MPD) -> Option<Clock> {
    let start = mpd.availabilityStartTime?;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs_f64();
    Some(Clock {
        now,
        start: start.timestamp() as f64 + f64::from(start.timestamp_subsec_millis()) / 1000.0,
        time_shift: mpd.timeShiftBufferDepth.map(|d| d.as_secs_f64()),
    })
}

/// A segment to fetch: where it is, when it plays, and the initialization section it
/// needs.
#[derive(Debug, Clone)]
struct Piece {
    period: usize,
    /// Its place in the period's sequence.
    number: u64,
    /// When it starts in the presentation, in seconds, and how long it plays.
    start: f64,
    duration: f64,
    url: Url,
    range: Option<(u64, u64)>,
    init: Option<(Url, Option<(u64, u64)>)>,
}

fn init_id(init: &(Url, Option<(u64, u64)>)) -> String {
    match init.1 {
        Some((start, end)) => format!("{}#{start}-{end}", init.0),
        None => init.0.to_string(),
    }
}

/// One representation in one period, with the addressing it inherits.
struct Track<'a> {
    period: usize,
    period_start: f64,
    period_duration: Option<f64>,
    set: &'a AdaptationSet,
    rep: &'a Representation,
    base: Url,
    template: Option<SegmentTemplate>,
    list: Option<&'a SegmentList>,
    segment_base: Option<&'a SegmentBase>,
}

impl<'a> Track<'a> {
    fn new(
        manifest_base: &Url,
        mpd: &'a MPD,
        period_index: usize,
        bounds: (f64, Option<f64>),
        set: &'a AdaptationSet,
        rep: &'a Representation,
    ) -> Result<Self, DownloadError> {
        let period = &mpd.periods[period_index];
        let base = joined(manifest_base, &mpd.base_url)?;
        let base = joined(&base, &period.BaseURL)?;
        let base = joined(&base, &set.BaseURL)?;
        let base = joined(&base, &rep.BaseURL)?;
        Ok(Self {
            period: period_index,
            period_start: bounds.0,
            period_duration: bounds.1,
            set,
            rep,
            base,
            template: merged_template([
                period.SegmentTemplate.as_ref(),
                set.SegmentTemplate.as_ref(),
                rep.SegmentTemplate.as_ref(),
            ]),
            list: rep
                .SegmentList
                .as_ref()
                .or(set.SegmentList.as_ref())
                .or(period.SegmentList.as_ref()),
            segment_base: rep
                .SegmentBase
                .as_ref()
                .or(set.SegmentBase.as_ref())
                .or(period.SegmentBase.as_ref()),
        })
    }

    fn id(&self) -> String {
        self.rep.id.clone().unwrap_or_default()
    }

    fn bandwidth(&self) -> u64 {
        self.rep.bandwidth.unwrap_or(0)
    }

    fn codecs(&self) -> Option<&str> {
        self.rep.codecs.as_deref().or(self.set.codecs.as_deref())
    }

    fn mime(&self) -> Option<&str> {
        self.rep.mimeType.as_deref().or(self.set.mimeType.as_deref())
    }

    fn language(&self) -> Option<&str> {
        self.rep.lang.as_deref().or(self.set.lang.as_deref())
    }

    /// Whether the representation says where its segments are. Without that it is one
    /// file at its base URL.
    fn segmented(&self) -> bool {
        self.template.as_ref().is_some_and(|t| t.media.is_some())
            || self.list.is_some()
            || self.segment_base.is_some()
    }

    fn init_of(&self, source: Option<&str>, range: Option<&str>) -> Result<Option<(Url, Option<(u64, u64)>)>, DownloadError> {
        let range = match range {
            Some(text) => Some(parse_range(text).ok_or_else(|| manifest_error(format!("bad range {text}")))?),
            None => None,
        };
        match source {
            Some(source) => {
                let filled = fill_template(source, &self.id(), self.bandwidth(), None, None);
                let url = self
                    .base
                    .join(&filled)
                    .map_err(|e| manifest_error(format!("bad initialization URL {filled}: {e}")))?;
                Ok(Some((url, range)))
            }
            None => Ok(range.map(|r| (self.base.clone(), Some(r)))),
        }
    }

    /// The segment times a timeline lists: `(time, duration)` in the timescale, an
    /// open-ended last entry running to the period's end, or to now for a live stream.
    fn expand_timeline(
        &self,
        timeline: &dash_mpd::SegmentTimeline,
        timescale: f64,
        pto: f64,
        clock: Option<Clock>,
    ) -> Result<Vec<(u64, u64)>, DownloadError> {
        let mut out = Vec::new();
        let mut time = 0u64;
        for s in &timeline.segments {
            if let Some(t) = s.t {
                time = t;
            }
            if s.d == 0 {
                return Err(manifest_error("timeline entry with no duration"));
            }
            let count = match s.r {
                Some(r) if r < 0 => {
                    let end_secs = match (self.period_duration, clock) {
                        (Some(duration), _) => duration + pto,
                        (None, Some(clock)) => clock.now - clock.start - self.period_start + pto,
                        (None, None) => {
                            return Err(manifest_error(
                                "open-ended timeline without a period length or availability start",
                            ));
                        }
                    };
                    let end = (end_secs * timescale).max(0.0) as u64;
                    (end.saturating_sub(time) / s.d).max(1)
                }
                Some(r) => r as u64 + 1,
                None => 1,
            };
            for _ in 0..count {
                out.push((time, s.d));
                time += s.d;
            }
        }
        Ok(out)
    }
}

/// What every manifest load of one download shares: the client and headers, the byte
/// budget, the initialization sections fetched, and the notes for the job log.
struct Session<'a> {
    http: &'a Http,
    platform: &'a str,
    headers: &'a [(String, String)],
    manifest: &'a Url,
    max_live: Duration,
    inits: Mutex<HashMap<String, Arc<InitSection>>>,
    notes: Mutex<Vec<String>>,
}

impl Session<'_> {
    fn note(&self, message: String) {
        tracing::info!("{message}");
        self.notes.lock().unwrap_or_else(|e| e.into_inner()).push(message);
    }

    /// The pieces of `track`, in order, that can be fetched now.
    async fn pieces(&self, track: &Track<'_>, clock: Option<Clock>) -> Result<Vec<Piece>, DownloadError> {
        let mut pieces = Vec::new();
        if let Some(template) = track.template.as_ref().filter(|t| t.media.is_some()) {
            let media = template.media.as_deref().unwrap();
            let timescale = template.timescale.unwrap_or(1).max(1) as f64;
            let pto = template.presentationTimeOffset.unwrap_or(0) as f64 / timescale;
            let start_number = template.startNumber.unwrap_or(1);
            let init = track.init_of(
                template
                    .initialization
                    .as_deref()
                    .or(template.Initialization.as_ref().and_then(|i| i.sourceURL.as_deref())),
                template.Initialization.as_ref().and_then(|i| i.range.as_deref()),
            )?;
            let times: Vec<(u64, u64, u64)> = if let Some(timeline) = &template.SegmentTimeline {
                track
                    .expand_timeline(timeline, timescale, pto, clock)?
                    .into_iter()
                    .enumerate()
                    .map(|(index, (time, duration))| (start_number + index as u64, time, duration))
                    .collect()
            } else {
                let duration = template
                    .duration
                    .filter(|d| *d > 0.0)
                    .ok_or_else(|| manifest_error("segment template without a duration or timeline"))?;
                let segment_secs = duration / timescale;
                let (first, last) = if let Some(period_duration) = track.period_duration {
                    let count = (period_duration / segment_secs).ceil().max(1.0) as u64;
                    (start_number, start_number + count - 1)
                } else if let Some(clock) = clock {
                    let elapsed = clock.now - clock.start - track.period_start;
                    if elapsed < segment_secs {
                        (start_number, 0)
                    } else {
                        let latest = start_number + (elapsed / segment_secs).floor() as u64 - 1;
                        let earliest = clock
                            .time_shift
                            .map(|shift| start_number + ((elapsed - shift) / segment_secs).floor().max(0.0) as u64)
                            .unwrap_or(start_number);
                        (earliest, latest)
                    }
                } else {
                    return Err(manifest_error(
                        "segment template without a period length or availability start",
                    ));
                };
                let last = template.endNumber.map_or(last, |end| last.min(end));
                (first..=last)
                    .map(|n| (n, ((n - start_number) as f64 * duration) as u64, duration as u64))
                    .collect()
            };
            for (number, time, duration) in times {
                let filled = fill_template(media, &track.id(), track.bandwidth(), Some(number), Some(time));
                let url = track
                    .base
                    .join(&filled)
                    .map_err(|e| manifest_error(format!("bad segment URL {filled}: {e}")))?;
                pieces.push(Piece {
                    period: track.period,
                    number,
                    start: track.period_start + time as f64 / timescale - pto,
                    duration: duration as f64 / timescale,
                    url,
                    range: None,
                    init: init.clone(),
                });
            }
        } else if let Some(list) = track.list {
            let timescale = list.timescale.unwrap_or(1).max(1) as f64;
            let init = track.init_of(
                list.Initialization.as_ref().and_then(|i| i.sourceURL.as_deref()),
                list.Initialization.as_ref().and_then(|i| i.range.as_deref()),
            )?;
            let count = list.segment_urls.len();
            let durations: Vec<f64> = if let Some(timeline) = &list.SegmentTimeline {
                track
                    .expand_timeline(timeline, timescale, 0.0, clock)?
                    .into_iter()
                    .map(|(_, d)| d as f64 / timescale)
                    .collect()
            } else if let Some(duration) = list.duration {
                vec![duration as f64 / timescale; count]
            } else {
                let each = track.period_duration.unwrap_or(0.0) / count.max(1) as f64;
                vec![each; count]
            };
            let mut start = track.period_start;
            for (index, item) in list.segment_urls.iter().enumerate() {
                let url = match &item.media {
                    Some(media) => track
                        .base
                        .join(media)
                        .map_err(|e| manifest_error(format!("bad segment URL {media}: {e}")))?,
                    None => track.base.clone(),
                };
                let range = match &item.mediaRange {
                    Some(text) => Some(parse_range(text).ok_or_else(|| manifest_error(format!("bad range {text}")))?),
                    None => None,
                };
                let duration = durations.get(index).copied().unwrap_or(0.0);
                pieces.push(Piece {
                    period: track.period,
                    number: index as u64 + 1,
                    start,
                    duration,
                    url,
                    range,
                    init: init.clone(),
                });
                start += duration;
            }
        } else if let Some(segment_base) = track.segment_base {
            let index_range = match segment_base.indexRange.as_deref() {
                Some(text) => Some(parse_range(text).ok_or_else(|| manifest_error(format!("bad index range {text}")))?),
                None => None,
            };
            let init_range = segment_base
                .Initialization
                .as_ref()
                .and_then(|i| i.range.as_deref())
                .map(|text| parse_range(text).ok_or_else(|| manifest_error(format!("bad range {text}"))))
                .transpose()?
                .or(index_range.map(|(start, _)| (0, start.saturating_sub(1))));
            let init = init_range.map(|range| (track.base.clone(), Some(range)));
            match index_range {
                None => pieces.push(Piece {
                    period: track.period,
                    number: 1,
                    start: track.period_start,
                    duration: track.period_duration.unwrap_or(0.0),
                    url: track.base.clone(),
                    range: None,
                    init,
                }),
                Some(index_range) => {
                    self.index_pieces(track, index_range, &init, &mut pieces).await?;
                }
            }
        } else {
            pieces.push(Piece {
                period: track.period,
                number: 1,
                start: track.period_start,
                duration: track.period_duration.unwrap_or(0.0),
                url: track.base.clone(),
                range: None,
                init: None,
            });
        }
        if let Some(clock) = clock {
            let offset = track
                .template
                .as_ref()
                .and_then(|t| t.availabilityTimeOffset)
                .unwrap_or(0.0);
            pieces.retain(|p| clock.start + p.start + p.duration - offset <= clock.now);
        }
        Ok(pieces)
    }

    /// The subsegments a segment index at `index_range` of the track's file lists, an
    /// index it points at expanded in turn.
    async fn index_pieces(
        &self,
        track: &Track<'_>,
        index_range: (u64, u64),
        init: &Option<(Url, Option<(u64, u64)>)>,
        pieces: &mut Vec<Piece>,
    ) -> Result<(), DownloadError> {
        let bytes = fetch_bytes(self.http, &track.base, self.platform, self.headers, Some(index_range)).await?;
        let sidx = mp4::parse_sidx(&bytes)?;
        let timescale = sidx.timescale.max(1) as f64;
        let mut offset = index_range.0 + sidx.len as u64 + sidx.first_offset;
        let mut time = sidx.earliest_presentation_time as f64 / timescale;
        for reference in &sidx.references {
            let range = (offset, offset + u64::from(reference.size) - 1);
            if reference.is_index {
                Box::pin(self.index_pieces(track, range, init, pieces)).await?;
            } else {
                let duration = f64::from(reference.duration) / timescale;
                pieces.push(Piece {
                    period: track.period,
                    number: pieces.len() as u64 + 1,
                    start: track.period_start + time,
                    duration,
                    url: track.base.clone(),
                    range: Some(range),
                    init: init.clone(),
                });
                time += duration;
            }
            offset += u64::from(reference.size);
        }
        Ok(())
    }

    /// The initialization section `init` names, fetched once. A section whose samples
    /// are protected is DRM.
    async fn init_section(&self, init: &(Url, Option<(u64, u64)>)) -> Result<Arc<InitSection>, DownloadError> {
        let id = init_id(init);
        if let Some(section) = self.inits.lock().unwrap_or_else(|e| e.into_inner()).get(&id) {
            return Ok(section.clone());
        }
        let bytes = fetch_bytes(self.http, &init.0, self.platform, self.headers, init.1)
            .await?
            .to_vec();
        let parsed = if mp4::is_mp4(&bytes) {
            let parsed = mp4::read_init(&bytes)?;
            if let Some(protection) = parsed
                .tracks
                .iter()
                .find_map(|t| t.protection.as_ref().filter(|p| p.protected))
            {
                let scheme = String::from_utf8_lossy(&protection.scheme).into_owned();
                let system = if scheme == "cenc" {
                    scheme
                } else {
                    format!("cenc ({scheme})")
                };
                return Err(DownloadError::Drm(self.manifest.to_string(), system));
            }
            Some(parsed)
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

    async fn fetch_piece(&self, piece: &Piece) -> Result<(Option<Arc<InitSection>>, Vec<u8>), DownloadError> {
        let init = match &piece.init {
            Some(init) => Some(self.init_section(init).await?),
            None => None,
        };
        let bytes = fetch_bytes(self.http, &piece.url, self.platform, self.headers, piece.range).await?;
        Ok((init, bytes.to_vec()))
    }

    /// Fetches `pieces` in order into `consumer`, for a `live` stream skipping one that
    /// cannot be fetched. Whether the capture reached its limit.
    async fn fetch_track(
        &self,
        name: &str,
        pieces: Vec<Piece>,
        live: bool,
        consumer: &mut dyn Consumer,
        cursor: &mut Cursor,
        mut meter: Option<&mut Meter<'_>>,
    ) -> Result<bool, DownloadError> {
        let max_live = self.max_live.as_secs_f64();
        let fetches = futures::stream::iter(pieces.into_iter().map(|piece| async move {
            let result = self.fetch_piece(&piece).await;
            (piece, result)
        }))
        .buffered(CONCURRENCY);
        futures::pin_mut!(fetches);
        while let Some((piece, result)) = fetches.next().await {
            let (init, bytes) = match result {
                Ok(fetched) => fetched,
                Err(error) if live && !matches!(error, DownloadError::Drm(..)) => {
                    self.note(format!(
                        "{name}: segment {} of period {} skipped: {error}",
                        piece.number, piece.period
                    ));
                    consumer.gap();
                    cursor.last = Some((piece.period, piece.number));
                    continue;
                }
                Err(error) => return Err(error),
            };
            let key = PartKey {
                run: piece.period as u64,
                init: piece.init.as_ref().map(init_id),
            };
            let timing = Timing {
                start: piece.start,
                origin: cursor.period_start(piece.period, piece.start),
            };
            consumer.take(&key, timing, init.as_deref(), &bytes).await?;
            cursor.last = Some((piece.period, piece.number));
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

/// How far along a track is: the last piece taken, the media time captured, and when
/// the first piece started.
#[derive(Debug, Default)]
struct Cursor {
    last: Option<(usize, u64)>,
    captured: f64,
    first_start: Option<f64>,
    /// Each period's start, for the times a segment's own timestamps count from.
    periods: Vec<f64>,
}

impl Cursor {
    fn period_start(&self, period: usize, fallback: f64) -> f64 {
        self.periods.get(period).copied().unwrap_or(fallback)
    }

    fn wants(&self, piece: &Piece) -> bool {
        self.last.is_none_or(|(period, number)| (piece.period, piece.number) > (period, number))
    }
}

/// What a text track is made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextKind {
    /// WebVTT samples in fragmented MP4.
    Wvtt,
    /// TTML documents as samples in fragmented MP4.
    Stpp,
    /// WebVTT segments.
    Vtt,
    /// TTML document segments.
    Ttml,
}

fn text_kind(track: &Track<'_>) -> Option<TextKind> {
    let mime = track.mime().unwrap_or("").to_ascii_lowercase();
    let codecs = track.codecs().unwrap_or("").to_ascii_lowercase();
    if mime.contains("mp4") {
        if codecs.contains("wvtt") {
            Some(TextKind::Wvtt)
        } else if codecs.contains("stpp") {
            Some(TextKind::Stpp)
        } else {
            None
        }
    } else if mime.contains("vtt") {
        Some(TextKind::Vtt)
    } else if mime.contains("ttml") || mime.contains("xml") {
        Some(TextKind::Ttml)
    } else {
        None
    }
}

/// A text track collected as cues or documents as its segments arrive.
struct TextSink<'a> {
    kind: TextKind,
    budget: &'a Budget,
    cues: Vec<subtitles::Cue>,
    documents: Vec<String>,
}

#[async_trait]
impl Consumer for TextSink<'_> {
    async fn take(
        &mut self,
        _key: &PartKey,
        timing: Timing,
        init: Option<&InitSection>,
        bytes: &[u8],
    ) -> Result<(), DownloadError> {
        self.budget.take(bytes.len() as u64)?;
        match self.kind {
            TextKind::Wvtt | TextKind::Stpp => {
                let parsed = init.and_then(|i| i.parsed.as_ref()).ok_or_else(|| {
                    DownloadError::Segment("text segment without an initialization section".into())
                })?;
                let (timescale, samples) = mp4::text_samples(bytes, parsed)?;
                let timescale = timescale.max(1) as f64;
                let first = samples.first().map_or(0, |s| s.time) as f64;
                for sample in samples {
                    let start = timing.start + (sample.time as f64 - first) / timescale;
                    let end = start + f64::from(sample.duration) / timescale;
                    match self.kind {
                        TextKind::Wvtt => {
                            for cue in mp4::wvtt_cues(&sample.bytes)? {
                                self.cues.push(subtitles::Cue {
                                    start,
                                    end,
                                    id: cue.id,
                                    settings: cue.settings,
                                    text: cue.text,
                                });
                            }
                        }
                        _ => {
                            let document = String::from_utf8_lossy(&sample.bytes).into_owned();
                            if !document.trim().is_empty() {
                                self.documents
                                    .push(subtitles::shift_ttml(&document, timing.origin));
                            }
                        }
                    }
                }
            }
            TextKind::Vtt => {
                let text = String::from_utf8_lossy(bytes);
                self.cues
                    .extend(subtitles::parse_vtt_cues(&text, timing.origin));
            }
            TextKind::Ttml => {
                let document = String::from_utf8_lossy(bytes).into_owned();
                if !document.trim().is_empty() {
                    self.documents
                        .push(subtitles::shift_ttml(&document, timing.origin));
                }
            }
        }
        Ok(())
    }

    fn gap(&mut self) {}
}

/// Which representations the download wants.
struct Wanted {
    rep_id: Option<String>,
    audio_only: bool,
    max_height: u32,
    language: Option<String>,
}

/// The representations chosen from one period.
struct Chosen<'a> {
    /// The main track: video, or audio for an audio-only presentation.
    primary: Track<'a>,
    /// A separate audio track beside the video.
    audio: Option<Track<'a>>,
    /// Text tracks by language.
    texts: Vec<(String, Track<'a>)>,
}

fn best_video<'a>(period: &'a Period, wanted: &Wanted) -> Option<(&'a AdaptationSet, &'a Representation)> {
    let candidates: Vec<(&AdaptationSet, &Representation)> = period
        .adaptations
        .iter()
        .filter(dash_mpd::is_video_adaptation)
        .flat_map(|set| set.representations.iter().map(move |r| (set, r)))
        .collect();
    if let Some(id) = &wanted.rep_id
        && let Some(found) = candidates.iter().find(|(_, r)| r.id.as_deref() == Some(id))
    {
        return Some(*found);
    }
    candidates.into_iter().max_by_key(|(set, r)| {
        let height = r.height.or(set.height).unwrap_or(0) as u32;
        let fits = height <= wanted.max_height;
        (
            fits,
            if fits { i64::from(height) } else { -i64::from(height) },
            r.bandwidth.unwrap_or(0),
        )
    })
}

fn best_audio<'a>(period: &'a Period, wanted: &Wanted) -> Option<(&'a AdaptationSet, &'a Representation)> {
    let candidates: Vec<(&AdaptationSet, &Representation)> = period
        .adaptations
        .iter()
        .filter(dash_mpd::is_audio_adaptation)
        .flat_map(|set| set.representations.iter().map(move |r| (set, r)))
        .collect();
    if wanted.audio_only
        && let Some(id) = &wanted.rep_id
        && let Some(found) = candidates.iter().find(|(_, r)| r.id.as_deref() == Some(id))
    {
        return Some(*found);
    }
    candidates.into_iter().max_by_key(|(set, r)| {
        let language = r.lang.as_deref().or(set.lang.as_deref());
        let preferred = wanted
            .language
            .as_deref()
            .is_some_and(|l| language.is_some_and(|have| have.eq_ignore_ascii_case(l)));
        (preferred, r.bandwidth.unwrap_or(0))
    })
}

fn choose<'a>(
    manifest_base: &Url,
    manifest: &Url,
    mpd: &'a MPD,
    period_index: usize,
    bounds: (f64, Option<f64>),
    wanted: &Wanted,
    text_languages: &[String],
) -> Result<Chosen<'a>, DownloadError> {
    let period = &mpd.periods[period_index];
    if let Some(system) = presentation_protection(mpd, period) {
        return Err(DownloadError::Drm(manifest.to_string(), system));
    }
    let video = if wanted.audio_only {
        None
    } else {
        best_video(period, wanted)
    };
    let (primary, audio) = match video {
        Some((set, rep)) => {
            let muxed = parse_codecs(rep.codecs.as_deref().or(set.codecs.as_deref())).1.is_some();
            let audio = if muxed { None } else { best_audio(period, wanted) };
            ((set, rep), audio)
        }
        None => (
            best_audio(period, wanted).ok_or_else(|| {
                manifest_error(format!("period {period_index} has no video or audio representation"))
            })?,
            None,
        ),
    };
    for (set, rep) in [Some(primary), audio].into_iter().flatten() {
        if let Some(system) = protection_of(set, rep) {
            return Err(DownloadError::Drm(manifest.to_string(), system));
        }
    }
    let primary = Track::new(manifest_base, mpd, period_index, bounds, primary.0, primary.1)?;
    let audio = match audio {
        Some((set, rep)) => Some(Track::new(manifest_base, mpd, period_index, bounds, set, rep)?),
        None => None,
    };
    let mut texts = Vec::new();
    for language in text_languages {
        let found = period
            .adaptations
            .iter()
            .filter(dash_mpd::is_subtitle_adaptation)
            .flat_map(|set| set.representations.iter().map(move |r| (set, r)))
            .map(|(set, rep)| Track::new(manifest_base, mpd, period_index, bounds, set, rep))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .find(|track| {
                track.segmented()
                    && text_kind(track).is_some()
                    && track.language().unwrap_or("und").eq_ignore_ascii_case(language)
            });
        if let Some(track) = found {
            texts.push((language.clone(), track));
        }
    }
    Ok(Chosen {
        primary,
        audio,
        texts,
    })
}

/// The segmented text tracks a manifest offers, one per language, as the subtitle
/// tracks the job can choose among.
fn text_tracks(manifest_base: &Url, manifest: &Url, mpd: &MPD, headers: &[(String, String)]) -> Result<Vec<SubtitleTrack>, DownloadError> {
    let bounds = period_bounds(mpd);
    let mut tracks: Vec<SubtitleTrack> = Vec::new();
    for (index, period) in mpd.periods.iter().enumerate() {
        for set in period.adaptations.iter().filter(dash_mpd::is_subtitle_adaptation) {
            for rep in &set.representations {
                let track = Track::new(manifest_base, mpd, index, bounds[index], set, rep)?;
                let Some(kind) = text_kind(&track) else {
                    continue;
                };
                if !track.segmented() {
                    continue;
                }
                let language = track.language().unwrap_or("und").to_string();
                if tracks.iter().any(|t| t.language.eq_ignore_ascii_case(&language)) {
                    continue;
                }
                let mut url = manifest.clone();
                url.set_fragment(Some(&format!("text-{language}")));
                tracks.push(SubtitleTrack {
                    url,
                    language,
                    name: set.Label.first().map(|l| l.content.clone()),
                    format: match kind {
                        TextKind::Wvtt | TextKind::Vtt => SubtitleFormat::Vtt,
                        TextKind::Stpp | TextKind::Ttml => SubtitleFormat::Ttml,
                    },
                    auto: false,
                    headers: headers.to_vec(),
                });
            }
        }
    }
    Ok(tracks)
}

async fn load_manifest(
    http: &Http,
    url: &Url,
    platform: &str,
    headers: &[(String, String)],
) -> Result<(Url, MPD), DownloadError> {
    let (base, xml) = fetch_text(http, url, platform, headers, MAX_MANIFEST).await?;
    let mpd = dash_mpd::parse(&xml)?;
    Ok((base, mpd))
}

/// The representation id a variant names, without the period prefix the resolver adds.
fn wanted_id(variant: &Variant) -> Option<String> {
    let id = variant.format_id.as_deref()?;
    let stripped = id
        .strip_prefix('p')
        .and_then(|rest| rest.split_once('-'))
        .filter(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
        .map(|(_, rest)| rest)
        .unwrap_or(id);
    Some(stripped.to_string())
}

#[async_trait]
impl Downloader for DashDownloader {
    fn handles(&self, kind: VariantKind) -> bool {
        kind == VariantKind::Dash
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
        let manifest = &variant.url;
        let budget = Budget::new(context.max_bytes);
        let session = Session {
            http: &self.http,
            platform,
            headers,
            manifest,
            max_live: context.max_live,
            inits: Mutex::new(HashMap::new()),
            notes: Mutex::new(Vec::new()),
        };
        let wanted = Wanted {
            rep_id: wanted_id(variant),
            audio_only: variant.audio_only,
            max_height: context.max_height,
            language: variant.language.clone(),
        };
        let mut loaded = Some(load_manifest(&self.http, manifest, platform, headers).await?);
        let first_mpd = &loaded.as_ref().unwrap().1;
        let first_base = loaded.as_ref().unwrap().0.clone();
        let text_choices: Vec<SubtitleTrack> = match &context.subtitles {
            Some(choice) => subtitles::pick(
                &text_tracks(&first_base, manifest, first_mpd, headers)?,
                choice.language.as_deref(),
            ),
            None => Vec::new(),
        };
        let text_languages: Vec<String> = text_choices.iter().map(|t| t.language.clone()).collect();
        if first_mpd.periods.len() > 1 {
            session.note(format!(
                "the presentation has {} periods, joined in turn",
                first_mpd.periods.len()
            ));
        }
        progress.send_replace(Progress {
            done: 0,
            total: None,
        });
        let mut meter = Meter::new(&progress);

        let mut primary = TrackWriter::new(dest_dir, "video", &budget);
        let mut audio = TrackWriter::new(dest_dir, "audio", &budget);
        let mut texts: Vec<TextSink<'_>> = Vec::new();
        let mut cursors = (Cursor::default(), Cursor::default(), Vec::<Cursor>::new());
        let mut reload_url = manifest.clone();
        let mut failures = 0u32;
        let mut was_live: Option<bool> = None;
        let mut has_audio = false;
        let max_live = context.max_live.as_secs_f64();
        loop {
            let (base, mpd) = match loaded.take() {
                Some(loaded) => loaded,
                None => match load_manifest(&self.http, &reload_url, platform, headers).await {
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
                        session.note(format!(
                            "Manifest reload failed {failures} times ({error}). Recording stopped after {:.0} s.",
                            cursors.0.captured
                        ));
                        break;
                    }
                    Err(error) => return Err(error),
                },
            };
            let live = is_dynamic(&mpd);
            let clock = if live { clock_of(&mpd) } else { None };
            let bounds = period_bounds(&mpd);
            if was_live.is_none() {
                was_live = Some(live);
                let total = if live {
                    max_live
                } else {
                    mpd.mediaPresentationDuration
                        .map(|d| d.as_secs_f64())
                        .unwrap_or_else(|| bounds.iter().filter_map(|b| b.1).sum())
                };
                meter.set_total(total);
            }
            if let Some(location) = mpd.locations.first()
                && let Ok(url) = base.join(location.url.trim())
            {
                reload_url = url;
            }
            let period_starts: Vec<f64> = bounds.iter().map(|b| b.0).collect();
            cursors.0.periods = period_starts.clone();
            cursors.1.periods = period_starts.clone();
            let mut primary_pieces = Vec::new();
            let mut audio_pieces = Vec::new();
            let mut text_pieces: Vec<Vec<Piece>> = vec![Vec::new(); text_languages.len()];
            let mut text_kinds: Vec<Option<TextKind>> = vec![None; text_languages.len()];
            for period_index in 0..mpd.periods.len() {
                let chosen = choose(&base, manifest, &mpd, period_index, bounds[period_index], &wanted, &text_languages)?;
                primary_pieces.extend(session.pieces(&chosen.primary, clock).await?);
                if let Some(track) = &chosen.audio {
                    has_audio = true;
                    audio_pieces.extend(session.pieces(track, clock).await?);
                }
                for (language, track) in &chosen.texts {
                    let index = text_languages.iter().position(|l| l == language).unwrap();
                    text_kinds[index] = text_kind(track);
                    text_pieces[index].extend(session.pieces(track, clock).await?);
                }
            }
            if texts.is_empty() {
                for kind in &text_kinds {
                    texts.push(TextSink {
                        kind: kind.unwrap_or(TextKind::Vtt),
                        budget: &budget,
                        cues: Vec::new(),
                        documents: Vec::new(),
                    });
                    cursors.2.push(Cursor {
                        periods: period_starts.clone(),
                        ..Cursor::default()
                    });
                }
            }
            for (sink, kind) in texts.iter_mut().zip(&text_kinds) {
                if let Some(kind) = kind {
                    sink.kind = *kind;
                }
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
                        "{first} earlier segment(s) of the live stream left out to keep the capture within the limit"
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
            let primary_run = session.fetch_track(
                "video",
                primary_fresh,
                live,
                &mut primary,
                &mut cursors.0,
                Some(&mut meter),
            );
            let audio_run = session.fetch_track("audio", audio_fresh, live, &mut audio, &mut cursors.1, None);
            let text_runs = futures::future::join_all(
                text_fresh
                    .into_iter()
                    .zip(texts.iter_mut())
                    .zip(cursors.2.iter_mut())
                    .map(|((pieces, sink), cursor)| session.fetch_track("subtitles", pieces, live, sink, cursor, None)),
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
            let wait = mpd
                .minimumUpdatePeriod
                .filter(|d| !d.is_zero())
                .map(|d| d.clamp(Duration::from_secs(1), Duration::from_secs(30)))
                .unwrap_or_else(|| {
                    mpd.maxSegmentDuration
                        .unwrap_or(Duration::from_secs(2))
                        .clamp(Duration::from_secs(1), Duration::from_secs(10))
                });
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
            let (text, format) = match sink.kind {
                TextKind::Wvtt | TextKind::Vtt => {
                    let shifted: Vec<subtitles::Cue> = sink
                        .cues
                        .into_iter()
                        .map(|mut cue| {
                            cue.start -= media_start;
                            cue.end -= media_start;
                            cue
                        })
                        .collect();
                    (subtitles::vtt_from_cues(&shifted), SubtitleFormat::Vtt)
                }
                TextKind::Stpp | TextKind::Ttml => {
                    let shifted: Vec<String> = sink
                        .documents
                        .iter()
                        .map(|d| subtitles::shift_ttml(d, -media_start))
                        .collect();
                    (subtitles::merge_ttml(&shifted), SubtitleFormat::Ttml)
                }
            };
            if text.trim().is_empty() {
                continue;
            }
            let mut track = track.clone();
            track.format = format;
            let path = dest_dir.join(subtitles::file_name(&track, index));
            tokio::fs::write(&path, &text).await?;
            local_subtitles.push(LocalSubtitle {
                language: track.language.clone(),
                name: track.name.clone(),
                path,
                format,
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
    use super::*;

    #[test]
    fn templates_fill_their_identifiers() {
        assert_eq!(
            fill_template("$RepresentationID$/$Number%05d$_$Time$_$Bandwidth$$$.m4s", "v1", 500, Some(7), Some(9000)),
            "v1/00007_9000_500$.m4s"
        );
        assert_eq!(fill_template("plain.m4s", "v1", 0, None, None), "plain.m4s");
        assert_eq!(fill_template("odd$Number", "v1", 0, Some(1), None), "odd$Number");
        assert_eq!(fill_template("$Unknown$", "v1", 0, None, None), "$Unknown$");
    }

    #[test]
    fn ranges_and_ids_are_read() {
        assert_eq!(parse_range("805-8668"), Some((805, 8668)));
        assert_eq!(parse_range("5-4"), None);
        let mut variant = Variant::new(Url::parse("https://cdn.test/m.mpd").unwrap(), VariantKind::Dash);
        variant.format_id = Some("p2-v1080".into());
        assert_eq!(wanted_id(&variant).as_deref(), Some("v1080"));
        variant.format_id = Some("p-x".into());
        assert_eq!(wanted_id(&variant).as_deref(), Some("p-x"));
        variant.format_id = Some("video=1000".into());
        assert_eq!(wanted_id(&variant).as_deref(), Some("video=1000"));
    }

    #[test]
    fn periods_take_their_bounds_from_their_neighbours() {
        let mpd = dash_mpd::parse(
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT30S">
  <Period id="a" start="PT0S"><AdaptationSet/></Period>
  <Period id="b" start="PT10S" duration="PT5S"><AdaptationSet/></Period>
  <Period id="c"><AdaptationSet/></Period>
</MPD>"#,
        )
        .unwrap();
        assert_eq!(
            period_bounds(&mpd),
            vec![(0.0, Some(10.0)), (10.0, Some(5.0)), (15.0, Some(15.0))]
        );
    }

    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::sync::Arc;

    use super::super::SubtitleChoice;
    use super::super::mp4::build::{full_box, plain_box};
    use crate::http::transport::site::{Reply, Site};

    const BASE: &str = "https://cdn.test/d/";
    const MPD_TYPE: &str = "application/dash+xml";

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("discoclip-dash-{tag}-{}", uuid::Uuid::now_v7()))
    }

    async fn run_ffmpeg(ffmpeg: &Ffmpeg, args: &[String]) {
        let args: Vec<OsString> = args.iter().map(OsString::from).collect();
        ffmpeg.run(args, |_| {}).await.unwrap();
    }

    /// Everything ffmpeg wrote for a presentation: the manifest and the files beside it.
    struct Packaged {
        mpd: String,
        files: Vec<(String, Vec<u8>)>,
    }

    impl Packaged {
        fn serve(&self, site: &Site, prefix: &str) {
            for (name, bytes) in &self.files {
                site.put_bytes(&format!("{BASE}{prefix}{name}"), "application/octet-stream", bytes);
            }
        }
    }

    /// Packages `seconds` of test picture and tone as DASH with `extra` muxer arguments.
    async fn package(ffmpeg: &Ffmpeg, dir: &Path, seconds: u32, extra: &[&str]) -> Packaged {
        tokio::fs::create_dir_all(dir).await.unwrap();
        let mut args: Vec<String> = [
            "-loglevel", "error", "-f", "lavfi", "-i", &format!("testsrc=size=64x64:rate=10:duration={seconds}"),
            "-f", "lavfi", "-i", &format!("sine=frequency=440:duration={seconds}"),
            "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-g", "10", "-c:a", "aac",
            "-f", "dash", "-seg_duration", "1",
        ]
        .map(String::from)
        .to_vec();
        args.extend(extra.iter().map(|a| a.to_string()));
        args.push(dir.join("m.mpd").to_str().unwrap().to_string());
        run_ffmpeg(ffmpeg, &args).await;
        let mut files = Vec::new();
        let mut entries = tokio::fs::read_dir(dir).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name != "m.mpd" {
                files.push((name, tokio::fs::read(entry.path()).await.unwrap()));
            }
        }
        let mpd = tokio::fs::read_to_string(dir.join("m.mpd")).await.unwrap();
        Packaged { mpd, files }
    }

    async fn download_dash(site: &Arc<Site>, ffmpeg: &Ffmpeg, name: &str, dir: &Path, context: &DownloadContext, format_id: Option<&str>) -> (Result<Downloaded, DownloadError>, Progress) {
        let http = Http::with_transport(site.clone(), Http::test_config());
        let downloader = DashDownloader::new(http, ffmpeg.clone());
        let mut variant = Variant::new(Url::parse(&format!("{BASE}{name}.mpd")).unwrap(), VariantKind::Dash);
        variant.format_id = format_id.map(str::to_string);
        let (progress, watched) = tokio::sync::watch::channel(Progress::default());
        let result = downloader.download(&variant, dir, context, progress).await;
        let last = *watched.borrow();
        (result, last)
    }

    async fn probed(ffmpeg: &Ffmpeg, downloaded: &Downloaded) -> crate::media::MediaInfo {
        assert!(downloaded.file.path.ends_with("source.mkv"));
        ffmpeg.probe(&downloaded.file.path).await.unwrap()
    }

    fn near(actual: f64, expected: f64) -> bool {
        (actual - expected).abs() <= 0.6
    }

    #[tokio::test]
    async fn every_addressing_form_downloads() {
        let dir = temp_dir("forms");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let forms: [(&str, &[&str]); 4] = [
            ("timeline", &[]),
            ("number", &["-use_timeline", "0"]),
            ("list", &["-use_template", "0"]),
            ("ranged", &["-single_file", "1"]),
        ];
        for (name, extra) in forms {
            let packaged = package(&ffmpeg, &dir.join(name), 3, extra).await;
            let site = Site::new();
            packaged.serve(&site, "");
            site.put_text(&format!("{BASE}{name}.mpd"), MPD_TYPE, &packaged.mpd);
            let (result, progress) = download_dash(&site, &ffmpeg, name, &dir.join(format!("job-{name}")), &DownloadContext::new(50_000_000), Some("0")).await;
            let downloaded = result.unwrap_or_else(|e| panic!("{name}: {e}"));
            let info = probed(&ffmpeg, &downloaded).await;
            assert!(info.video.is_some(), "{name}: {info:?}");
            assert!(info.audio.is_some(), "{name}: {info:?}");
            let duration = info.duration.unwrap().as_secs_f64();
            assert!(near(duration, 3.0), "{name}: {duration}");
            assert!(downloaded.notes.is_empty(), "{name}: {:?}", downloaded.notes);
            assert_eq!(progress.done, 3, "{name}");
            assert_eq!(progress.total, Some(3), "{name}");
            assert!(!dir.join(format!("job-{name}")).join("video.0.mp4").exists());
        }
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// One file with a global segment index, as `SegmentBase` addresses it.
    async fn indexed_file(ffmpeg: &Ffmpeg, path: &Path, video: bool) -> (Vec<u8>, (u64, u64), (u64, u64)) {
        let mut args: Vec<String> = vec!["-loglevel".into(), "error".into()];
        if video {
            args.extend(["-f", "lavfi", "-i", "testsrc=size=64x64:rate=10:duration=3", "-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-g", "10"].map(String::from));
        } else {
            args.extend(["-f", "lavfi", "-i", "sine=frequency=440:duration=3", "-c:a", "aac"].map(String::from));
        }
        args.extend(["-movflags", "frag_keyframe+empty_moov+default_base_moof+global_sidx", "-frag_duration", "1000000", "-f", "mp4"].map(String::from));
        args.push(path.to_str().unwrap().to_string());
        run_ffmpeg(ffmpeg, &args).await;
        let bytes = tokio::fs::read(path).await.unwrap();
        let sidx = mp4::top_level(&bytes)
            .unwrap()
            .into_iter()
            .find(|(kind, _, _)| kind == b"sidx")
            .unwrap();
        (bytes, (0, sidx.1 as u64 - 1), (sidx.1 as u64, sidx.2 as u64 - 1))
    }

    #[tokio::test]
    async fn segment_base_files_are_fetched_by_their_index() {
        let dir = temp_dir("sidx");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let (video, video_init, video_index) = indexed_file(&ffmpeg, &dir.join("v.mp4"), true).await;
        let (audio, audio_init, audio_index) = indexed_file(&ffmpeg, &dir.join("a.mp4"), false).await;
        let mpd = format!(
            r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT3S" profiles="urn:mpeg:dash:profile:isoff-on-demand:2011">
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <Representation id="v" bandwidth="60000" width="64" height="64" codecs="avc1.42c00a">
        <BaseURL>v.mp4</BaseURL>
        <SegmentBase indexRange="{}-{}"><Initialization range="{}-{}"/></SegmentBase>
      </Representation>
    </AdaptationSet>
    <AdaptationSet mimeType="audio/mp4" contentType="audio" lang="en">
      <Representation id="a" bandwidth="69000" codecs="mp4a.40.2">
        <BaseURL>a.mp4</BaseURL>
        <SegmentBase indexRange="{}-{}"><Initialization range="{}-{}"/></SegmentBase>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#,
            video_index.0, video_index.1, video_init.0, video_init.1,
            audio_index.0, audio_index.1, audio_init.0, audio_init.1
        );
        let site = Site::new();
        site.put_bytes(&format!("{BASE}v.mp4"), "video/mp4", &video);
        site.put_bytes(&format!("{BASE}a.mp4"), "audio/mp4", &audio);
        site.put_text(&format!("{BASE}sidx.mpd"), MPD_TYPE, &mpd);
        let (result, _) = download_dash(&site, &ffmpeg, "sidx", &dir.join("job"), &DownloadContext::new(50_000_000), Some("v")).await;
        let downloaded = result.unwrap();
        let info = probed(&ffmpeg, &downloaded).await;
        assert!(info.video.is_some() && info.audio.is_some(), "{info:?}");
        assert!(near(info.duration.unwrap().as_secs_f64(), 3.0), "{info:?}");
        // The index, the initialization section and each subsegment were ranged requests.
        let ranges = site.ranges_seen(&format!("{BASE}v.mp4"));
        assert!(ranges.len() >= 4, "{ranges:?}");
        assert_eq!(ranges[0], format!("bytes={}-{}", video_index.0, video_index.1));
        assert!(ranges.contains(&format!("bytes={}-{}", video_init.0, video_init.1)), "{ranges:?}");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// The `Period` element of a packaged manifest, with `attributes` on it and its
    /// files under `prefix`.
    fn period_of(mpd: &str, attributes: &str, prefix: &str) -> String {
        let start = mpd.find("<Period").unwrap();
        let end = mpd.find("</Period>").unwrap() + "</Period>".len();
        let period = &mpd[start..end];
        let open_end = period.find('>').unwrap();
        format!(
            "<Period {attributes}><BaseURL>{prefix}</BaseURL>{}",
            &period[open_end + 1..]
        )
    }

    fn manifest(head: &str, periods: &[String]) -> String {
        format!(
            "<MPD xmlns=\"urn:mpeg:dash:schema:mpd:2011\" {head} profiles=\"urn:mpeg:dash:profile:isoff-live:2011\" minBufferTime=\"PT1S\">\n{}\n</MPD>",
            periods.join("\n")
        )
    }

    #[tokio::test]
    async fn periods_are_joined_in_turn() {
        let dir = temp_dir("periods");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let first = package(&ffmpeg, &dir.join("a"), 2, &[]).await;
        let second = package(&ffmpeg, &dir.join("b"), 2, &[]).await;
        let site = Site::new();
        first.serve(&site, "a/");
        second.serve(&site, "b/");
        let mpd = manifest(
            "type=\"static\" mediaPresentationDuration=\"PT4S\"",
            &[
                period_of(&first.mpd, "id=\"a\" start=\"PT0S\" duration=\"PT2S\"", "a/"),
                period_of(&second.mpd, "id=\"b\" start=\"PT2S\" duration=\"PT2S\"", "b/"),
            ],
        );
        site.put_text(&format!("{BASE}periods.mpd"), MPD_TYPE, &mpd);
        let (result, progress) = download_dash(&site, &ffmpeg, "periods", &dir.join("job"), &DownloadContext::new(50_000_000), Some("p1-0")).await;
        let downloaded = result.unwrap();
        let info = probed(&ffmpeg, &downloaded).await;
        assert!(info.audio.is_some(), "{info:?}");
        assert!(near(info.duration.unwrap().as_secs_f64(), 4.0), "{info:?}");
        assert!(downloaded.notes.iter().any(|n| n.contains("2 periods")), "{:?}", downloaded.notes);
        assert!(downloaded.notes.iter().any(|n| n.contains("joined from 2 parts")), "{:?}", downloaded.notes);
        assert_eq!(progress.total, Some(4));
        assert_eq!(progress.done, 4);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// A dynamic manifest of the timeline-packaged presentation offering its first
    /// `count` video and audio segments.
    fn live_window(packaged: &Packaged, count: usize, ended: bool) -> String {
        let video_line = format!("<S t=\"0\" d=\"10240\" r=\"{}\" />", count - 1);
        let mut mpd = packaged
            .mpd
            .replace("<S t=\"0\" d=\"10240\" r=\"2\" />", &video_line)
            .replace("<S t=\"0\" d=\"10240\" r=\"4\" />", &video_line);
        // The audio timeline lists one entry per segment. Keep the first `count`.
        let start = mpd.find("timescale=\"44100\"").unwrap();
        let tl_start = mpd[start..].find("<SegmentTimeline>").unwrap() + start + "<SegmentTimeline>".len();
        let tl_end = mpd[tl_start..].find("</SegmentTimeline>").unwrap() + tl_start;
        let entries: Vec<&str> = mpd[tl_start..tl_end]
            .lines()
            .map(str::trim)
            .filter(|l| l.starts_with("<S "))
            .collect();
        let kept = entries[..count.min(entries.len())].join("\n");
        mpd.replace_range(tl_start..tl_end, &format!("\n{kept}\n"));
        if ended {
            mpd
        } else {
            mpd.replace("type=\"static\"", "type=\"dynamic\" availabilityStartTime=\"1970-01-01T00:00:00Z\" minimumUpdatePeriod=\"PT1S\"")
                .replace("mediaPresentationDuration=\"PT5.0S\"", "")
                .replace("mediaPresentationDuration=\"PT3.0S\"", "")
        }
    }

    #[tokio::test]
    async fn a_dynamic_manifest_is_followed_until_it_turns_static() {
        let dir = temp_dir("live");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let packaged = package(&ffmpeg, &dir.join("pkg"), 5, &[]).await;
        let site = Site::new();
        packaged.serve(&site, "");
        site.put_series(
            &format!("{BASE}live.mpd"),
            vec![
                Reply::new(MPD_TYPE, live_window(&packaged, 2, false)),
                Reply::new(MPD_TYPE, live_window(&packaged, 4, false)),
                Reply::new(MPD_TYPE, live_window(&packaged, 5, true)),
            ],
        );
        let mut context = DownloadContext::new(50_000_000);
        context.max_live = Duration::from_secs(60);
        let (result, progress) = download_dash(&site, &ffmpeg, "live", &dir.join("job"), &context, Some("0")).await;
        let downloaded = result.unwrap();
        let info = probed(&ffmpeg, &downloaded).await;
        assert!(info.audio.is_some(), "{info:?}");
        assert!(near(info.duration.unwrap().as_secs_f64(), 5.0), "{info:?}");
        assert!(downloaded.notes.iter().any(|n| n.contains("the live stream ended")), "{:?}", downloaded.notes);
        assert_eq!(site.hits(&format!("{BASE}live.mpd")), 3);
        for n in 1..=5 {
            assert_eq!(site.hits(&format!("{BASE}chunk-stream0-{n:05}.m4s")), 1, "segment {n}");
        }
        assert_eq!(progress.total, Some(60));
        assert_eq!(progress.done, 5);

        // A window that keeps growing is cut at the limit. The last segment is never asked for.
        let site = Site::new();
        packaged.serve(&site, "");
        site.put_series(
            &format!("{BASE}cut.mpd"),
            vec![
                Reply::new(MPD_TYPE, live_window(&packaged, 2, false)),
                Reply::new(MPD_TYPE, live_window(&packaged, 4, false)),
                Reply::new(MPD_TYPE, live_window(&packaged, 5, false)),
            ],
        );
        context.max_live = Duration::from_millis(2_500);
        let (result, _) = download_dash(&site, &ffmpeg, "cut", &dir.join("cut"), &context, Some("0")).await;
        let downloaded = result.unwrap();
        let info = probed(&ffmpeg, &downloaded).await;
        assert!(near(info.duration.unwrap().as_secs_f64(), 3.0), "{info:?}");
        assert!(downloaded.notes.iter().any(|n| n.contains("capture cut at the limit")), "{:?}", downloaded.notes);
        assert_eq!(site.hits(&format!("{BASE}chunk-stream0-00005.m4s")), 0);

        // A manifest that stops answering ends the capture with what was taken.
        let site = Site::new();
        packaged.serve(&site, "");
        site.put_series(
            &format!("{BASE}gone.mpd"),
            vec![
                Reply::new(MPD_TYPE, live_window(&packaged, 2, false)),
                Reply::new(MPD_TYPE, "gone").status(404),
            ],
        );
        context.max_live = Duration::from_secs(60);
        let (result, _) = download_dash(&site, &ffmpeg, "gone", &dir.join("gone"), &context, Some("0")).await;
        let downloaded = result.unwrap();
        let info = probed(&ffmpeg, &downloaded).await;
        assert!(near(info.duration.unwrap().as_secs_f64(), 2.0), "{info:?}");
        assert!(downloaded.notes.iter().any(|n| n.contains("could not be reloaded 3 times")), "{:?}", downloaded.notes);
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn numbered_live_segments_are_taken_by_the_clock() {
        let dir = temp_dir("clock");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let packaged = package(&ffmpeg, &dir.join("pkg"), 3, &["-use_timeline", "0"]).await;
        let site = Site::new();
        // The stream started a hundred seconds ago and keeps three seconds of segments:
        // by the clock, segments 98 to 100 are the ones to fetch, so the packaged
        // segments are served under those numbers.
        for (name, bytes) in &packaged.files {
            let served = match name.rsplit_once('-') {
                Some((stem, rest)) if stem.starts_with("chunk-stream") => {
                    let number: u64 = rest.trim_end_matches(".m4s").parse().unwrap();
                    format!("{stem}-{:05}.m4s", number + 97)
                }
                _ => name.clone(),
            };
            site.put_bytes(&format!("{BASE}{served}"), "application/octet-stream", bytes);
        }
        let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let started = now - 100;
        let start_time = format!(
            "{}",
            chrono_free_rfc3339(started)
        );
        let dynamic = packaged
            .mpd
            .replace("type=\"static\"", &format!("type=\"dynamic\" availabilityStartTime=\"{start_time}\" minimumUpdatePeriod=\"PT1S\" timeShiftBufferDepth=\"PT3S\""))
            .replace("mediaPresentationDuration=\"PT3.0S\"", "");
        // The stream then ends: the manifest turns static, numbered as before.
        let ended = packaged.mpd.replace("startNumber=\"1\"", "startNumber=\"98\"");
        site.put_series(
            &format!("{BASE}clock.mpd"),
            vec![Reply::new(MPD_TYPE, dynamic), Reply::new(MPD_TYPE, ended)],
        );
        let mut context = DownloadContext::new(50_000_000);
        context.max_live = Duration::from_secs(60);
        let (result, _) = download_dash(&site, &ffmpeg, "clock", &dir.join("job"), &context, Some("0")).await;
        let downloaded = result.unwrap();
        let info = probed(&ffmpeg, &downloaded).await;
        assert!(info.audio.is_some(), "{info:?}");
        assert!(near(info.duration.unwrap().as_secs_f64(), 3.0), "{info:?}");
        assert!(downloaded.notes.iter().any(|n| n.contains("the live stream ended")), "{:?}", downloaded.notes);
        let asked: Vec<String> = site
            .seen()
            .into_iter()
            .map(|s| s.url)
            .filter(|u| u.contains("chunk-stream0-"))
            .collect();
        assert_eq!(asked.len(), 3, "{asked:?}");
        for n in 98..=100 {
            assert_eq!(site.hits(&format!("{BASE}chunk-stream0-{n:05}.m4s")), 1);
        }
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// `seconds` since the epoch as an `xs:dateTime`, without a calendar crate.
    fn chrono_free_rfc3339(seconds: u64) -> String {
        let days = seconds / 86_400;
        let rem = seconds % 86_400;
        // Civil-from-days (Howard Hinnant's algorithm).
        let z = days as i64 + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        format!(
            "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
            rem / 3600,
            (rem / 60) % 60,
            rem % 60
        )
    }

    #[tokio::test]
    async fn common_encryption_is_reported_as_drm() {
        use super::super::mp4::build::{append_within, rename, sinf, tenc};
        let dir = temp_dir("drm");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let packaged = package(&ffmpeg, &dir.join("pkg"), 2, &[]).await;
        let site = Site::new();
        packaged.serve(&site, "");
        let declared = packaged.mpd.replace(
            "<Representation id=\"0\"",
            "<ContentProtection schemeIdUri=\"urn:mpeg:dash:mp4protection:2011\" value=\"cenc\"/><ContentProtection schemeIdUri=\"urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed\"/><Representation id=\"0\"",
        );
        site.put_text(&format!("{BASE}declared.mpd"), MPD_TYPE, &declared);
        let (result, _) = download_dash(&site, &ffmpeg, "declared", &dir.join("declared"), &DownloadContext::new(50_000_000), Some("0")).await;
        assert!(matches!(result.unwrap_err(), DownloadError::Drm(_, system) if system == "widevine"));
        assert_eq!(site.hits(&format!("{BASE}init-stream0.m4s")), 0);

        // Nothing declared, but the initialization section says the samples are protected.
        let init = packaged
            .files
            .iter()
            .find(|(name, _)| name == "init-stream0.m4s")
            .map(|(_, bytes)| bytes.clone())
            .unwrap();
        let path = [(b"moov", 0usize), (b"trak", 0), (b"mdia", 0), (b"minf", 0), (b"stbl", 0), (b"stsd", 0), (b"avc1", 0)];
        let mut locked = append_within(&init, &path, &sinf(b"avc1", b"cbcs", &tenc(1, 9, 16, None)));
        rename(&mut locked, &path, b"encv");
        site.put_bytes(&format!("{BASE}init-stream0.m4s"), "video/mp4", &locked);
        site.put_text(&format!("{BASE}silent.mpd"), MPD_TYPE, &packaged.mpd);
        let (result, _) = download_dash(&site, &ffmpeg, "silent", &dir.join("silent"), &DownloadContext::new(50_000_000), Some("0")).await;
        assert!(matches!(result.unwrap_err(), DownloadError::Drm(_, system) if system == "cenc (cbcs)"));
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    /// An initialization section for a `wvtt` text track with id 3 at timescale 1000.
    fn wvtt_init() -> Vec<u8> {
        let mut tkhd_body = vec![0u8; 8];
        tkhd_body.extend_from_slice(&3u32.to_be_bytes());
        tkhd_body.resize(80, 0);
        let tkhd = full_box(b"tkhd", 0, 7, &tkhd_body);
        let mut mdhd_body = vec![0u8; 8];
        mdhd_body.extend_from_slice(&1000u32.to_be_bytes());
        mdhd_body.resize(20, 0);
        let mdhd = full_box(b"mdhd", 0, 0, &mdhd_body);
        let mut entry = vec![0u8; 8];
        entry[7] = 1;
        entry.extend_from_slice(&plain_box(b"vttC", b"WEBVTT"));
        let wvtt = plain_box(b"wvtt", &entry);
        let mut stsd_body = 1u32.to_be_bytes().to_vec();
        stsd_body.extend_from_slice(&wvtt);
        let stbl = plain_box(b"stbl", &full_box(b"stsd", 0, 0, &stsd_body));
        let minf = plain_box(b"minf", &stbl);
        let mut mdia_body = mdhd;
        mdia_body.extend_from_slice(&minf);
        let mut trak_body = tkhd;
        trak_body.extend_from_slice(&plain_box(b"mdia", &mdia_body));
        let trak = plain_box(b"trak", &trak_body);
        let mut trex_body = 3u32.to_be_bytes().to_vec();
        trex_body.extend_from_slice(&1u32.to_be_bytes());
        trex_body.extend_from_slice(&[0u8; 12]);
        let mut moov_body = trak;
        moov_body.extend_from_slice(&plain_box(b"mvex", &full_box(b"trex", 0, 0, &trex_body)));
        let mut out = plain_box(b"ftyp", b"iso6\0\0\0\0iso6");
        out.extend_from_slice(&plain_box(b"moov", &moov_body));
        out
    }

    /// A `wvtt` fragment starting at `time` milliseconds with `cues` of
    /// `(duration ms, text)`, an empty text being an empty sample.
    fn wvtt_fragment(time: u64, cues: &[(u32, &str)]) -> Vec<u8> {
        let mut samples = Vec::new();
        let mut sizes = Vec::new();
        for (_, text) in cues {
            let sample = if text.is_empty() {
                plain_box(b"vtte", &[])
            } else {
                plain_box(b"vttc", &plain_box(b"payl", text.as_bytes()))
            };
            sizes.push(sample.len() as u32);
            samples.extend_from_slice(&sample);
        }
        let tfhd = full_box(b"tfhd", 0, 0x2_0000, &3u32.to_be_bytes());
        let tfdt = full_box(b"tfdt", 1, 0, &time.to_be_bytes());
        let mut trun_body = (cues.len() as u32).to_be_bytes().to_vec();
        trun_body.extend_from_slice(&0i32.to_be_bytes());
        for ((duration, _), size) in cues.iter().zip(&sizes) {
            trun_body.extend_from_slice(&duration.to_be_bytes());
            trun_body.extend_from_slice(&size.to_be_bytes());
        }
        let trun = full_box(b"trun", 0, 0x301, &trun_body);
        let mut traf_body = tfhd;
        traf_body.extend_from_slice(&tfdt);
        traf_body.extend_from_slice(&trun);
        let mut moof_body = full_box(b"mfhd", 0, 0, &1u32.to_be_bytes());
        moof_body.extend_from_slice(&plain_box(b"traf", &traf_body));
        let mut moof = plain_box(b"moof", &moof_body);
        let data_offset = (moof.len() + 8) as i32;
        let trun_at = moof.len() - trun.len() + 16;
        moof[trun_at..trun_at + 4].copy_from_slice(&data_offset.to_be_bytes());
        let mut out = plain_box(b"styp", b"msdh\0\0\0\0msdh");
        out.extend_from_slice(&moof);
        out.extend_from_slice(&plain_box(b"mdat", &samples));
        out
    }

    #[tokio::test]
    async fn segmented_text_tracks_come_with_the_media() {
        let dir = temp_dir("text");
        let ffmpeg = Ffmpeg::provision(&dir.join("tools")).await.unwrap();
        let packaged = package(&ffmpeg, &dir.join("pkg"), 3, &[]).await;
        let site = Site::new();
        packaged.serve(&site, "");
        site.put_bytes(&format!("{BASE}wvtt-init.mp4"), "application/mp4", &wvtt_init());
        site.put_bytes(&format!("{BASE}wvtt-1.m4s"), "application/mp4", &wvtt_fragment(0, &[(1000, "One"), (500, ""), (1000, "Two")]));
        site.put_bytes(&format!("{BASE}wvtt-2.m4s"), "application/mp4", &wvtt_fragment(2500, &[(500, "Three")]));
        site.put_text(&format!("{BASE}de-1.vtt"), "text/vtt", "WEBVTT\n\n00:00:00.250 --> 00:00:01.000\nEins\n");
        site.put_text(&format!("{BASE}de-2.vtt"), "text/vtt", "WEBVTT\n\n00:00:02.000 --> 00:00:02.750\nZwei\n");
        site.put_text(&format!("{BASE}fr-1.ttml"), "application/ttml+xml", "<tt xmlns=\"http://www.w3.org/ns/ttml\"><body><div><p begin=\"0.5s\" end=\"1s\">Un</p></div></body></tt>");
        site.put_text(&format!("{BASE}fr-2.ttml"), "application/ttml+xml", "<tt xmlns=\"http://www.w3.org/ns/ttml\"><body><div><p begin=\"2s\" end=\"3s\">Deux</p></div></body></tt>");
        let text_sets = r#"
    <AdaptationSet mimeType="application/mp4" codecs="wvtt" contentType="text" lang="en">
      <Label>English</Label>
      <Representation id="t-en" bandwidth="1000">
        <SegmentList timescale="1000" duration="2500">
          <Initialization sourceURL="wvtt-init.mp4"/>
          <SegmentURL media="wvtt-1.m4s"/>
          <SegmentURL media="wvtt-2.m4s"/>
        </SegmentList>
      </Representation>
    </AdaptationSet>
    <AdaptationSet mimeType="text/vtt" contentType="text" lang="de">
      <Representation id="t-de" bandwidth="1000">
        <SegmentTemplate media="de-$Number$.vtt" duration="2" timescale="1" startNumber="1"/>
      </Representation>
    </AdaptationSet>
    <AdaptationSet mimeType="application/ttml+xml" contentType="text" lang="fr">
      <Representation id="t-fr" bandwidth="1000">
        <SegmentTemplate media="fr-$Number$.ttml" duration="2" timescale="1" startNumber="1"/>
      </Representation>
    </AdaptationSet>
  </Period>"#;
        let mpd = packaged.mpd.replacen("</Period>", text_sets, 1);
        site.put_text(&format!("{BASE}text.mpd"), MPD_TYPE, &mpd);
        let mut context = DownloadContext::new(50_000_000);
        context.subtitles = Some(SubtitleChoice {
            tracks: Vec::new(),
            language: None,
        });
        let (result, _) = download_dash(&site, &ffmpeg, "text", &dir.join("job"), &context, Some("0")).await;
        let downloaded = result.unwrap();
        assert!(near(probed(&ffmpeg, &downloaded).await.duration.unwrap().as_secs_f64(), 3.0));
        assert_eq!(downloaded.subtitles.len(), 3, "{:?}", downloaded.subtitles);
        let by_language = |language: &str| {
            downloaded
                .subtitles
                .iter()
                .find(|s| s.language == language)
                .unwrap_or_else(|| panic!("no {language} track"))
        };
        let en = by_language("en");
        assert_eq!(en.format, SubtitleFormat::Vtt);
        assert_eq!(en.name.as_deref(), Some("English"));
        assert_eq!(en.url.as_ref().unwrap().as_str(), format!("{BASE}text.mpd#text-en"));
        let text = tokio::fs::read_to_string(&en.path).await.unwrap();
        assert_eq!(
            text,
            "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nOne\n\n00:00:01.500 --> 00:00:02.500\nTwo\n\n00:00:02.500 --> 00:00:03.000\nThree\n\n"
        );
        let de = tokio::fs::read_to_string(&by_language("de").path).await.unwrap();
        assert_eq!(de, "WEBVTT\n\n00:00:00.250 --> 00:00:01.000\nEins\n\n00:00:02.000 --> 00:00:02.750\nZwei\n\n");
        let fr = by_language("fr");
        assert_eq!(fr.format, SubtitleFormat::Ttml);
        let text = tokio::fs::read_to_string(&fr.path).await.unwrap();
        assert_eq!(
            text,
            "<tt xmlns=\"http://www.w3.org/ns/ttml\"><body><div><p begin=\"0.5s\" end=\"1s\">Un</p></div><div><p begin=\"2s\" end=\"3s\">Deux</p></div></body></tt>"
        );

        // Only the preferred language when it is offered.
        context.subtitles = Some(SubtitleChoice {
            tracks: Vec::new(),
            language: Some("de".into()),
        });
        let (result, _) = download_dash(&site, &ffmpeg, "text", &dir.join("de"), &context, Some("0")).await;
        let downloaded = result.unwrap();
        assert_eq!(downloaded.subtitles.len(), 1);
        assert_eq!(downloaded.subtitles[0].language, "de");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
