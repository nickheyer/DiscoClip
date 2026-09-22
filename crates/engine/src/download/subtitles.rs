//! Fetches subtitle tracks next to the media, converting YouTube's `json3` timed text,
//! Bilibili's JSON cues and HLS WebVTT playlists to plain WebVTT files ffmpeg reads.

use std::path::Path;
use std::time::Duration;

use m3u8_rs::Playlist;
use url::Url;

use super::{DownloadError, LocalSubtitle};
use crate::http::Http;
use crate::resolve::{SubtitleFormat, SubtitleTrack};

const MAX_SUBTITLE: usize = 32 * 1024 * 1024;

fn extension(format: SubtitleFormat) -> &'static str {
    match format {
        SubtitleFormat::Vtt
        | SubtitleFormat::Json3
        | SubtitleFormat::BilibiliJson
        | SubtitleFormat::TiktokJson
        | SubtitleFormat::HlsVtt => "vtt",
        SubtitleFormat::Srt => "srt",
        SubtitleFormat::Ttml => "ttml",
        SubtitleFormat::Ass => "ass",
    }
}

fn stored_format(format: SubtitleFormat) -> SubtitleFormat {
    match format {
        SubtitleFormat::Json3
        | SubtitleFormat::BilibiliJson
        | SubtitleFormat::TiktokJson
        | SubtitleFormat::HlsVtt => SubtitleFormat::Vtt,
        other => other,
    }
}

fn safe_language(language: &str) -> String {
    let cleaned: String = language
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(32)
        .collect();
    if cleaned.is_empty() {
        "und".into()
    } else {
        cleaned
    }
}

/// One track per language from `tracks`, a human-made one over an automatic one, and
/// only the preferred language's when it is offered.
pub fn pick(tracks: &[SubtitleTrack], preferred: Option<&str>) -> Vec<SubtitleTrack> {
    let mut by_language: Vec<SubtitleTrack> = Vec::new();
    for track in tracks {
        match by_language
            .iter_mut()
            .find(|t| t.language.eq_ignore_ascii_case(&track.language))
        {
            Some(existing) => {
                if existing.auto && !track.auto {
                    *existing = track.clone();
                }
            }
            None => by_language.push(track.clone()),
        }
    }
    if let Some(preferred) = preferred {
        let preferred = preferred.to_ascii_lowercase();
        let chosen: Vec<_> = by_language
            .iter()
            .filter(|t| {
                let language = t.language.to_ascii_lowercase();
                language == preferred || language.starts_with(&format!("{preferred}-"))
            })
            .cloned()
            .collect();
        if !chosen.is_empty() {
            return chosen;
        }
    }
    by_language
}

/// The file name a track is stored under in the job directory.
pub fn file_name(track: &SubtitleTrack, index: usize) -> String {
    let language = safe_language(&track.language);
    let suffix = if track.auto { ".auto" } else { "" };
    format!(
        "subtitles.{language}{suffix}.{index}.{}",
        extension(track.format)
    )
}

/// Downloads every track into `dir` as `subtitles.<lang>[.auto].<ext>`.
pub async fn fetch_all(
    http: &Http,
    platform: &str,
    tracks: &[SubtitleTrack],
    dir: &Path,
) -> Result<Vec<LocalSubtitle>, DownloadError> {
    let mut out = Vec::new();
    for (index, track) in tracks.iter().enumerate() {
        let text = fetch_track(http, platform, track).await?;
        let path = dir.join(file_name(track, index));
        tokio::fs::write(&path, text).await?;
        out.push(LocalSubtitle {
            language: track.language.clone(),
            name: track.name.clone(),
            path,
            format: stored_format(track.format),
            url: Some(track.url.clone()),
        });
    }
    Ok(out)
}

async fn fetch_text(
    http: &Http,
    platform: &str,
    url: &Url,
    headers: &[(String, String)],
) -> Result<String, DownloadError> {
    let response = http
        .get(url.clone())
        .platform(platform)
        .headers(headers)
        .send()
        .await?;
    if !response.is_success() {
        return Err(DownloadError::Status {
            status: response.status.as_u16(),
            url: url.to_string(),
        });
    }
    Ok(response.text(MAX_SUBTITLE).await?)
}

async fn fetch_track(
    http: &Http,
    platform: &str,
    track: &SubtitleTrack,
) -> Result<String, DownloadError> {
    let text = fetch_text(http, platform, &track.url, &track.headers).await?;
    match track.format {
        SubtitleFormat::Json3 => json3_to_vtt(&text).ok_or_else(|| {
            DownloadError::Manifest(format!("{} is not json3 timed text", track.url))
        }),
        SubtitleFormat::BilibiliJson => bilibili_json_to_vtt(&text).ok_or_else(|| {
            DownloadError::Manifest(format!("{} is not a Bilibili subtitle", track.url))
        }),
        SubtitleFormat::TiktokJson => tiktok_json_to_vtt(&text).ok_or_else(|| {
            DownloadError::Manifest(format!("{} is not a TikTok caption file", track.url))
        }),
        SubtitleFormat::HlsVtt => {
            let playlist = match m3u8_rs::parse_playlist_res(text.as_bytes()) {
                Ok(Playlist::MediaPlaylist(media)) => media,
                Ok(Playlist::MasterPlaylist(_)) => {
                    return Err(DownloadError::Manifest(format!(
                        "{} is a master playlist, not a subtitle rendition",
                        track.url
                    )));
                }
                Err(error) => {
                    return Err(DownloadError::Manifest(format!(
                        "invalid subtitle playlist at {}: {error}",
                        track.url
                    )));
                }
            };
            let mut merged = String::from("WEBVTT\n\n");
            for segment in &playlist.segments {
                let segment_url = track.url.join(&segment.uri).map_err(|e| {
                    DownloadError::Manifest(format!("bad subtitle segment uri: {e}"))
                })?;
                let piece = fetch_text(http, platform, &segment_url, &track.headers).await?;
                merged.push_str(&hls_vtt_cues(&piece, None));
                merged.push('\n');
            }
            Ok(merged)
        }
        _ => Ok(text),
    }
}

/// A WebVTT time, `hh:mm:ss.mmm` or `mm:ss.mmm`, in seconds.
fn parse_vtt_time(text: &str) -> Option<f64> {
    let mut parts = text.trim().rsplit(':');
    let seconds: f64 = parts.next()?.parse().ok()?;
    let minutes: f64 = parts.next().map(str::parse).transpose().ok()??;
    let hours: f64 = match parts.next() {
        Some(h) => h.parse().ok()?,
        None => 0.0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some(hours * 3600.0 + minutes * 60.0 + seconds)
}

fn format_vtt_time(seconds: f64) -> String {
    vtt_time((seconds.max(0.0) * 1000.0).round() as u64)
}

/// Map WebVTT cue times to the recording timeline using X-TIMESTAMP-MAP and media_start.
/// Preserve timing when either value is missing.
///
/// Drop cues ending before the recording and omit the header block.
pub fn hls_vtt_cues(text: &str, media_start: Option<f64>) -> String {
    let mut shift = 0.0;
    let mut out = String::new();
    let mut in_header = true;
    let mut skipping = false;
    for line in text.lines() {
        if in_header {
            if line.trim().is_empty() {
                in_header = false;
                continue;
            }
            if let (Some(media_start), Some(map)) = (media_start, line.trim().strip_prefix("X-TIMESTAMP-MAP=")) {
                let mut mpegts = None;
                let mut local = None;
                for field in map.split(',') {
                    if let Some(ticks) = field.trim().strip_prefix("MPEGTS:") {
                        mpegts = ticks.trim().parse::<u64>().ok();
                    } else if let Some(time) = field.trim().strip_prefix("LOCAL:") {
                        local = parse_vtt_time(time);
                    }
                }
                if let (Some(mpegts), Some(local)) = (mpegts, local) {
                    shift = mpegts as f64 / 90_000.0 - local - media_start;
                }
            }
            continue;
        }
        if line.trim().is_empty() {
            if skipping {
                skipping = false;
            } else {
                out.push('\n');
            }
            continue;
        }
        if skipping {
            continue;
        }
        if let Some((times, settings)) = line.split_once("-->")
            && let (Some(start), Some(end)) = (
                parse_vtt_time(times),
                parse_vtt_time(settings.split_whitespace().next().unwrap_or("")),
            )
        {
            let (start, end) = (start + shift, end + shift);
            if end <= 0.0 {
                skipping = true;
                // The cue's identifier line, if one preceded it, goes as well.
                if let Some(stripped) = out.strip_suffix('\n')
                    && !stripped.is_empty()
                    && !stripped.ends_with('\n')
                {
                    let cut = stripped.rfind('\n').map_or(0, |i| i + 1);
                    out.truncate(cut);
                }
                continue;
            }
            let rest: Vec<&str> = settings.split_whitespace().skip(1).collect();
            out.push_str(&format_vtt_time(start));
            out.push_str(" --> ");
            out.push_str(&format_vtt_time(end));
            for setting in rest {
                out.push(' ');
                out.push_str(setting);
            }
            out.push('\n');
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// One WebVTT cue with its times in seconds.
#[derive(Debug, Clone, PartialEq)]
pub struct Cue {
    pub start: f64,
    pub end: f64,
    pub id: Option<String>,
    pub settings: Option<String>,
    pub text: String,
}

/// The cues of a WebVTT text, header and all, with `shift` seconds added to their times.
pub fn parse_vtt_cues(text: &str, shift: f64) -> Vec<Cue> {
    let mut cues = Vec::new();
    let mut in_header = text.trim_start().starts_with("WEBVTT");
    let mut pending: Option<String> = None;
    let mut current: Option<Cue> = None;
    for line in text.lines() {
        if in_header {
            if line.trim().is_empty() {
                in_header = false;
            }
            continue;
        }
        if line.trim().is_empty() {
            if let Some(cue) = current.take() {
                cues.push(cue);
            }
            pending = None;
            continue;
        }
        if let Some(cue) = current.as_mut() {
            if !cue.text.is_empty() {
                cue.text.push('\n');
            }
            cue.text.push_str(line);
            continue;
        }
        if let Some((times, rest)) = line.split_once("-->")
            && let (Some(start), Some(end)) = (
                parse_vtt_time(times),
                parse_vtt_time(rest.split_whitespace().next().unwrap_or("")),
            )
        {
            let settings: Vec<&str> = rest.split_whitespace().skip(1).collect();
            current = Some(Cue {
                start: start + shift,
                end: end + shift,
                id: pending.take(),
                settings: (!settings.is_empty()).then(|| settings.join(" ")),
                text: String::new(),
            });
            continue;
        }
        if line.starts_with("NOTE") || line.starts_with("STYLE") || line.starts_with("REGION") {
            pending = None;
            continue;
        }
        pending = Some(line.to_string());
    }
    if let Some(cue) = current {
        cues.push(cue);
    }
    cues
}

/// A WebVTT file of `cues`, in order, with a cue that continues the one before it, same
/// text and settings and no gap, folded into it, and cues ending before zero dropped.
pub fn vtt_from_cues(cues: &[Cue]) -> String {
    let mut merged: Vec<Cue> = Vec::new();
    for cue in cues {
        if cue.end <= 0.0 || cue.text.trim().is_empty() {
            continue;
        }
        if let Some(last) = merged.last_mut()
            && last.text == cue.text
            && last.settings == cue.settings
            && (cue.start - last.end).abs() < 0.001
        {
            last.end = cue.end;
            continue;
        }
        merged.push(cue.clone());
    }
    let mut out = String::from("WEBVTT\n\n");
    for cue in merged {
        if let Some(id) = &cue.id {
            out.push_str(id);
            out.push('\n');
        }
        out.push_str(&format_vtt_time(cue.start));
        out.push_str(" --> ");
        out.push_str(&format_vtt_time(cue.end));
        if let Some(settings) = &cue.settings {
            out.push(' ');
            out.push_str(settings);
        }
        out.push('\n');
        out.push_str(cue.text.trim_end());
        out.push_str("\n\n");
    }
    out
}

/// Where the body of a TTML document's content lies: the inside of its `body` element.
fn ttml_body(document: &str) -> Option<(usize, usize)> {
    let lower = document.to_ascii_lowercase();
    let mut search = 0;
    let open = loop {
        let at = lower[search..].find("body")? + search;
        let before = lower[..at].chars().next_back();
        let tag_start = lower[..at].rfind('<')?;
        let between = &lower[tag_start + 1..at];
        if before != Some('/') && (between.is_empty() || (between.ends_with(':') && !between.contains(' '))) {
            break at;
        }
        search = at + 4;
    };
    let inner_start = lower[open..].find('>')? + open + 1;
    if lower[open..inner_start].ends_with("/>") {
        return None;
    }
    // The last closing body tag, with or without a namespace prefix.
    let mut limit = lower.len();
    while let Some(found) = lower[inner_start..limit].rfind("body") {
        let name = found + inner_start;
        if let Some(lt) = lower[..name].rfind("</") {
            let between = &lower[lt + 2..name];
            if between.is_empty() || (between.ends_with(':') && !between.contains(' ')) {
                return Some((inner_start, lt));
            }
        }
        limit = name;
    }
    None
}

/// One TTML document from several consecutive ones: the first document with the content
/// of every later document's body added to its own.
pub fn merge_ttml(documents: &[String]) -> String {
    let Some(first) = documents.first() else {
        return String::new();
    };
    let Some((_, close)) = ttml_body(first) else {
        return documents.join("\n");
    };
    let mut out = first[..close].to_string();
    for document in &documents[1..] {
        if let Some((start, end)) = ttml_body(document) {
            out.push_str(&document[start..end]);
        }
    }
    out.push_str(&first[close..]);
    out
}

/// A TTML time expression in seconds: a clock time `hh:mm:ss(.fff)` or an offset time
/// such as `12.5s`, `1500ms`, `2m` or `1h`. Frame-based times are not counted.
fn parse_ttml_time(text: &str) -> Option<f64> {
    let text = text.trim();
    if text.contains(':') {
        if text.matches(':').count() != 2 || text.rsplit(':').next()?.contains(':') {
            return None;
        }
        return parse_vtt_time(text);
    }
    let digits_end = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let value: f64 = text[..digits_end].parse().ok()?;
    match &text[digits_end..] {
        "h" => Some(value * 3600.0),
        "m" => Some(value * 60.0),
        "s" => Some(value),
        "ms" => Some(value / 1000.0),
        _ => None,
    }
}

/// A TTML document with every `begin` and `end` attribute moved by `shift` seconds,
/// clamped at zero. Times the document counts in frames or ticks are left as they are.
pub fn shift_ttml(document: &str, shift: f64) -> String {
    if shift == 0.0 {
        return document.to_string();
    }
    let mut out = String::with_capacity(document.len());
    let mut rest = document;
    while let Some(at) = rest.find(|c: char| c == 'b' || c == 'e') {
        out.push_str(&rest[..at]);
        let tail = &rest[at..];
        let attribute = ["begin=\"", "end=\""]
            .into_iter()
            .find(|a| tail.starts_with(a));
        let preceded = out.ends_with(|c: char| c.is_whitespace());
        match attribute {
            Some(attribute) if preceded => {
                let value_start = attribute.len();
                if let Some(len) = tail[value_start..].find('"')
                    && let Some(seconds) = parse_ttml_time(&tail[value_start..value_start + len])
                {
                    out.push_str(attribute);
                    out.push_str(&format_vtt_time(seconds + shift));
                    out.push('"');
                    rest = &tail[value_start + len + 1..];
                    continue;
                }
                out.push_str(attribute);
                rest = &tail[value_start..];
            }
            _ => {
                out.push_str(&tail[..1]);
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// How long the cues of a WebVTT text run: the latest cue end.
pub fn vtt_length(text: &str) -> Duration {
    let mut latest = 0.0f64;
    for line in text.lines() {
        if let Some((_, rest)) = line.split_once("-->")
            && let Some(end) = parse_vtt_time(rest.split_whitespace().next().unwrap_or(""))
        {
            latest = latest.max(end);
        }
    }
    Duration::from_secs_f64(latest)
}

fn vtt_time(ms: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}.{:03}",
        ms / 3_600_000,
        (ms / 60_000) % 60,
        (ms / 1000) % 60,
        ms % 1000
    )
}

/// Converts YouTube `json3` timed text to WebVTT. `None` when the text is not json3.
pub fn json3_to_vtt(text: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let events = value.get("events")?.as_array()?;
    let mut out = String::from("WEBVTT\n\n");
    let mut count = 0;
    for event in events {
        let start = event.get("tStartMs")?.as_u64()?;
        let segments = match event.get("segs").and_then(|s| s.as_array()) {
            Some(segments) => segments,
            None => continue,
        };
        let line: String = segments
            .iter()
            .filter_map(|s| s.get("utf8").and_then(|u| u.as_str()))
            .collect::<String>()
            .replace('\n', " ");
        if line.trim().is_empty() {
            continue;
        }
        let duration = event
            .get("dDurationMs")
            .and_then(|d| d.as_u64())
            .unwrap_or(2000);
        count += 1;
        out.push_str(&format!(
            "{count}\n{} --> {}\n{}\n\n",
            vtt_time(start),
            vtt_time(start + duration),
            line.trim()
        ));
    }
    Some(out)
}

/// TikTok's automatic captions as WebVTT: every utterance with text becomes a cue.
fn tiktok_json_to_vtt(text: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let utterances = value["utterances"].as_array()?;
    let mut out = String::from("WEBVTT\n\n");
    for line in utterances {
        let (Some(start), Some(end), Some(cue)) = (
            line["start_time"].as_f64(),
            line["end_time"].as_f64(),
            line["text"]
                .as_str()
                .map(str::trim)
                .filter(|t| !t.is_empty()),
        ) else {
            continue;
        };
        out.push_str(&format!(
            "{} --> {}\n{}\n\n",
            vtt_time(start.round().max(0.0) as u64),
            vtt_time(end.round().max(0.0) as u64),
            cue
        ));
    }
    Some(out)
}

/// Converts Bilibili's JSON subtitles, a `body` of cues with `from` and `to` in seconds,
/// to WebVTT. `None` when the text is not one.
pub fn bilibili_json_to_vtt(text: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let cues = value.get("body")?.as_array()?;
    let mut out = String::from("WEBVTT\n\n");
    let mut count = 0;
    for cue in cues {
        let from = cue.get("from")?.as_f64()?;
        let to = cue
            .get("to")
            .and_then(|t| t.as_f64())
            .filter(|to| *to > from)
            .unwrap_or(from + 2.0);
        let content = cue
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .trim();
        if content.is_empty() {
            continue;
        }
        count += 1;
        out.push_str(&format!(
            "{count}\n{} --> {}\n{}\n\n",
            vtt_time((from.max(0.0) * 1000.0).round() as u64),
            vtt_time((to.max(0.0) * 1000.0).round() as u64),
            content
        ));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bilibili_json_becomes_vtt_cues() {
        let json = r#"{"font_size":0.4,"body":[{"from":0.5,"to":2.25,"location":2,"content":"第一句"},{"from":3,"to":3,"content":"  "},{"from":4.1,"content":"最后一句"}]}"#;
        let vtt = bilibili_json_to_vtt(json).unwrap();
        assert!(
            vtt.starts_with("WEBVTT\n\n1\n00:00:00.500 --> 00:00:02.250\n第一句\n"),
            "{vtt}"
        );
        assert!(
            vtt.contains("2\n00:00:04.100 --> 00:00:06.100\n最后一句\n"),
            "{vtt}"
        );
        assert!(bilibili_json_to_vtt("{}").is_none());
        assert!(bilibili_json_to_vtt("nope").is_none());
    }

    #[test]
    fn json3_becomes_vtt_cues() {
        let json = r#"{"events":[{"tStartMs":0,"dDurationMs":1500,"segs":[{"utf8":"Hello "},{"utf8":"world"}]},{"tStartMs":3661000,"dDurationMs":1000,"segs":[{"utf8":"\n"}]},{"tStartMs":5000,"segs":[{"utf8":"End"}]}]}"#;
        let vtt = json3_to_vtt(json).unwrap();
        assert!(vtt.starts_with("WEBVTT\n\n1\n00:00:00.000 --> 00:00:01.500\nHello world\n"));
        assert!(vtt.contains("2\n00:00:05.000 --> 00:00:07.000\nEnd\n"));
        assert!(json3_to_vtt("not json").is_none());
        assert!(json3_to_vtt("{}").is_none());
    }

    #[test]
    fn vtt_segments_lose_their_headers() {
        let piece = "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000\n\n00:00:01.000 --> 00:00:02.000\nHi\n";
        assert_eq!(
            hls_vtt_cues(piece, None),
            "00:00:01.000 --> 00:00:02.000\nHi\n"
        );
        assert_eq!(safe_language("en-US"), "en-US");
        assert_eq!(safe_language("../x"), "x");
        assert_eq!(safe_language(""), "und");
    }

    #[test]
    fn cues_are_parsed_merged_and_written() {
        let text = "WEBVTT\n\nNOTE nothing\n\n1\n00:00:00.000 --> 00:00:01.000 line:90%\nOne\nmore\n\n00:00:01.000 --> 00:00:02.000 line:90%\nOne\nmore\n\n00:00:03.000 --> 00:00:04.000\nTwo\n";
        let cues = parse_vtt_cues(text, 0.5);
        assert_eq!(cues.len(), 3);
        assert_eq!(cues[0].id.as_deref(), Some("1"));
        assert_eq!(cues[0].settings.as_deref(), Some("line:90%"));
        assert_eq!(cues[0].text, "One\nmore");
        assert_eq!(cues[0].start, 0.5);
        assert_eq!(
            vtt_from_cues(&cues),
            "WEBVTT\n\n1\n00:00:00.500 --> 00:00:02.500 line:90%\nOne\nmore\n\n00:00:03.500 --> 00:00:04.500\nTwo\n\n"
        );
        let early = parse_vtt_cues("00:00:00.000 --> 00:00:01.000\nGone\n", -2.0);
        assert_eq!(vtt_from_cues(&early), "WEBVTT\n\n");
    }

    #[test]
    fn ttml_documents_merge_their_bodies() {
        let a = "<tt xmlns=\"http://www.w3.org/ns/ttml\"><head/><body><div><p begin=\"0s\" end=\"1s\">A</p></div></body></tt>";
        let b = "<tt xmlns=\"http://www.w3.org/ns/ttml\"><head/><body><div><p begin=\"1s\" end=\"2s\">B</p></div></body></tt>";
        assert_eq!(
            merge_ttml(&[a.to_string(), b.to_string()]),
            "<tt xmlns=\"http://www.w3.org/ns/ttml\"><head/><body><div><p begin=\"0s\" end=\"1s\">A</p></div><div><p begin=\"1s\" end=\"2s\">B</p></div></body></tt>"
        );
        let prefixed = "<tt:tt xmlns:tt=\"http://www.w3.org/ns/ttml\"><tt:body><tt:p>X</tt:p></tt:body></tt:tt>";
        assert_eq!(ttml_body(prefixed), Some((53, 67)));
        assert_eq!(merge_ttml(&[a.to_string()]), a);
        assert_eq!(
            shift_ttml("<p begin=\"1.5s\" end=\"00:00:03.000\" dur=\"1s\">A</p><p begin=\"10f\">B</p>", -1.0),
            "<p begin=\"00:00:00.500\" end=\"00:00:02.000\" dur=\"1s\">A</p><p begin=\"10f\">B</p>"
        );
        assert_eq!(parse_ttml_time("1500ms"), Some(1.5));
        assert_eq!(parse_ttml_time("2m"), Some(120.0));
        assert_eq!(parse_ttml_time("00:01:00:12"), None);
    }

    #[test]
    fn vtt_cues_are_shifted_by_the_timestamp_map_onto_the_media_start() {
        // The map says local 0 is MPEG-TS 900000 (10 s). The media starts at 12 s, so
        // cues move back by two seconds and one ending before the start is dropped.
        let piece = "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:00.000\n\n1\n00:00:00.500 --> 00:00:01.500\nGone\n\n2\n00:00:03.000 --> 00:00:04.250 line:90%\nKept\n\n00:05.000 --> 00:06.000\nShort form\n";
        assert_eq!(
            hls_vtt_cues(piece, Some(12.0)),
            "2\n00:00:01.000 --> 00:00:02.250 line:90%\nKept\n\n00:00:03.000 --> 00:00:04.000\nShort form\n"
        );
        // Without a media start the map is left alone.
        assert!(hls_vtt_cues(piece, None).starts_with("1\n00:00:00.500 --> 00:00:01.500\nGone\n"));
        assert_eq!(vtt_length(piece), Duration::from_secs(6));
        assert_eq!(parse_vtt_time("01:02:03.500"), Some(3723.5));
        assert_eq!(parse_vtt_time("02:03.500"), Some(123.5));
        assert_eq!(parse_vtt_time("x"), None);
    }
}
