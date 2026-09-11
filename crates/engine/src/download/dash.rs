//! Downloads MPEG-DASH presentations with `dash-mpd`, which fetches the chosen video and
//! audio representations and muxes them with the embedded ffmpeg.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::segments::fetch_text;
use super::{DownloadContext, DownloadError, Downloaded, Downloader};
use crate::event::{Progress, ProgressSender};
use crate::ffmpeg::Ffmpeg;
use crate::http::Http;
use crate::media::LocalFile;
use crate::resolve::{Variant, VariantKind};
use async_trait::async_trait;
use dash_mpd::fetch::{DashDownloader as Fetcher, ProgressObserver};
use dash_mpd::{MPD, Period, Representation, is_audio_adaptation, is_video_adaptation};

const MAX_MANIFEST: usize = 16 * 1024 * 1024;

pub struct DashDownloader {
    http: Http,
    ffmpeg: Ffmpeg,
    max_height: u32,
}

impl DashDownloader {
    pub fn new(http: Http, ffmpeg: Ffmpeg, max_height: u32) -> Self {
        Self {
            http,
            ffmpeg,
            max_height,
        }
    }
}

struct Observer(ProgressSender);

impl ProgressObserver for Observer {
    fn update(&self, percent: u32, _bandwidth: u64, _message: &str) {
        self.0.send_replace(Progress {
            done: u64::from(percent.min(100)),
            total: Some(100),
        });
    }
}

/// The video representation `dash-mpd` will pick for `max_height`: the height closest to it,
/// then the highest bandwidth.
fn chosen_video(period: &Period, max_height: u32) -> Option<&Representation> {
    let candidates: Vec<&Representation> = period
        .adaptations
        .iter()
        .filter(is_video_adaptation)
        .flat_map(|set| set.representations.iter())
        .collect();
    let distance = |rep: &Representation| {
        rep.height
            .map_or(u64::MAX, |h| u64::from(max_height).abs_diff(h))
    };
    let closest = candidates.iter().map(|r| distance(r)).min()?;
    candidates
        .into_iter()
        .filter(|r| distance(r) == closest)
        .max_by_key(|r| r.bandwidth.unwrap_or(0))
}

fn best_audio_bandwidth(period: &Period) -> u64 {
    period
        .adaptations
        .iter()
        .filter(is_audio_adaptation)
        .flat_map(|set| set.representations.iter())
        .filter_map(|r| r.bandwidth)
        .max()
        .unwrap_or(0)
}

/// Upper bound on the bytes the download will produce: the highest combined bitrate of any
/// period over the whole presentation. `None` when the manifest states no duration.
fn estimated_bytes(mpd: &MPD, max_height: u32, hint: Option<Duration>) -> Option<u64> {
    let period_sum: Option<Duration> = mpd
        .periods
        .iter()
        .map(|p| p.duration)
        .try_fold(Duration::ZERO, |acc, d| Some(acc + d?));
    let total = mpd.mediaPresentationDuration.or(period_sum).or(hint)?;
    let bits_per_second = mpd
        .periods
        .iter()
        .map(|period| {
            chosen_video(period, max_height)
                .and_then(|r| r.bandwidth)
                .unwrap_or(0)
                + best_audio_bandwidth(period)
        })
        .max()
        .unwrap_or(0);
    Some((bits_per_second as f64 * total.as_secs_f64() / 8.0) as u64)
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
        let (manifest_url, xml) = fetch_text(
            &self.http,
            &variant.url,
            &context.platform,
            &variant.headers,
            MAX_MANIFEST,
        )
        .await?;
        let mpd = dash_mpd::parse(&xml)?;
        if let Some(size) = estimated_bytes(&mpd, self.max_height, variant.duration)
            && size > context.max_bytes
        {
            return Err(DownloadError::TooLarge {
                size,
                limit: context.max_bytes,
            });
        }
        let mut headers = reqwest::header::HeaderMap::new();
        for (name, value) in &variant.headers {
            if let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                reqwest::header::HeaderValue::from_str(value),
            ) {
                headers.insert(name, value);
            }
        }
        let client = reqwest::Client::builder()
            .default_headers(headers)
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(60))
            .build()
            .map_err(|e| DownloadError::Process(e.to_string()))?;
        let ffmpeg = self.ffmpeg.ffmpeg_path().to_str().ok_or_else(|| {
            DownloadError::Process(format!(
                "ffmpeg path {} is not valid UTF-8",
                self.ffmpeg.ffmpeg_path().display()
            ))
        })?;
        progress.send_replace(Progress {
            done: 0,
            total: Some(100),
        });
        let dest = dest_dir.join("source.mkv");
        let path = Fetcher::new(manifest_url.as_str())
            .with_http_client(client)
            .with_ffmpeg(ffmpeg)
            .with_muxer_preference("mkv", "ffmpeg")
            .best_quality()
            .prefer_video_height(u64::from(self.max_height))
            .fetch_subtitles(false)
            .add_progress_observer(Arc::new(Observer(progress)))
            .download_to(&dest)
            .await?;
        let file = LocalFile::from_path(path).await?;
        if file.size == 0 {
            return Err(DownloadError::Empty);
        }
        if file.size > context.max_bytes {
            return Err(DownloadError::TooLarge {
                size: file.size,
                limit: context.max_bytes,
            });
        }
        Ok(Downloaded::file(file))
    }
}

#[cfg(test)]
mod tests {
    use super::estimated_bytes;
    use std::time::Duration;

    const MPD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S"
     profiles="urn:mpeg:dash:profile:isoff-on-demand:2011">
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <Representation id="v720" bandwidth="1000000" width="1280" height="720"/>
      <Representation id="v1080" bandwidth="2000000" width="1920" height="1080"/>
    </AdaptationSet>
    <AdaptationSet contentType="audio" mimeType="audio/mp4">
      <Representation id="a" bandwidth="128000"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

    #[test]
    fn estimates_from_the_representation_closest_to_max_height() {
        let mpd = dash_mpd::parse(MPD).unwrap();
        assert_eq!(estimated_bytes(&mpd, 1080, None), Some(2_660_000));
        assert_eq!(estimated_bytes(&mpd, 720, None), Some(1_410_000));
    }

    #[test]
    fn uses_the_duration_hint_when_the_manifest_has_none() {
        let mpd =
            dash_mpd::parse(&MPD.replace(r#" mediaPresentationDuration="PT10S""#, "")).unwrap();
        assert_eq!(estimated_bytes(&mpd, 1080, None), None);
        assert_eq!(
            estimated_bytes(&mpd, 1080, Some(Duration::from_secs(20))),
            Some(5_320_000)
        );
    }
}
