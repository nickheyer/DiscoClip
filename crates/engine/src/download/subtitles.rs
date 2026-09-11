//! Fetches subtitle tracks next to the media, converting YouTube's `json3` timed text and
//! HLS WebVTT playlists to plain WebVTT files ffmpeg reads.

use std::path::Path;

use m3u8_rs::Playlist;
use url::Url;

use super::{DownloadError, LocalSubtitle};
use crate::http::Http;
use crate::resolve::{SubtitleFormat, SubtitleTrack};

const MAX_SUBTITLE: usize = 32 * 1024 * 1024;

fn extension(format: SubtitleFormat) -> &'static str {
    match format {
        SubtitleFormat::Vtt | SubtitleFormat::Json3 | SubtitleFormat::HlsVtt => "vtt",
        SubtitleFormat::Srt => "srt",
        SubtitleFormat::Ttml => "ttml",
        SubtitleFormat::Ass => "ass",
    }
}

fn stored_format(format: SubtitleFormat) -> SubtitleFormat {
    match format {
        SubtitleFormat::Json3 | SubtitleFormat::HlsVtt => SubtitleFormat::Vtt,
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
        let language = safe_language(&track.language);
        let suffix = if track.auto { ".auto" } else { "" };
        let path = dir.join(format!(
            "subtitles.{language}{suffix}.{index}.{}",
            extension(track.format)
        ));
        tokio::fs::write(&path, text).await?;
        out.push(LocalSubtitle {
            language: track.language.clone(),
            name: track.name.clone(),
            path,
            format: stored_format(track.format),
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
                merged.push_str(&strip_vtt_header(&piece));
                merged.push('\n');
            }
            Ok(merged)
        }
        _ => Ok(text),
    }
}

/// A WebVTT segment's cues, without its header block.
fn strip_vtt_header(text: &str) -> String {
    let mut out = String::new();
    let mut in_header = true;
    for line in text.lines() {
        if in_header {
            if line.trim().is_empty() {
                in_header = false;
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
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

/// Converts YouTube `json3` timed text to WebVTT; `None` when the text is not json3.
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

#[cfg(test)]
mod tests {
    use super::*;

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
            strip_vtt_header(piece),
            "00:00:01.000 --> 00:00:02.000\nHi\n"
        );
        assert_eq!(safe_language("en-US"), "en-US");
        assert_eq!(safe_language("../x"), "x");
        assert_eq!(safe_language(""), "und");
    }
}
