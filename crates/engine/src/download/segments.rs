//! Shared pieces for segment based downloads: a size limited sink, ranged fetches, and the
//! local mux step that turns the fetched streams into one Matroska file.

use std::ffi::OsString;
use std::path::Path;

use bytes::Bytes;
use url::Url;

use super::DownloadError;
use crate::ffmpeg::Ffmpeg;
use crate::http::Http;
use crate::media::LocalFile;

pub struct Sink {
    file: tokio::fs::File,
    written: u64,
    limit: u64,
}

impl Sink {
    pub async fn create(path: &Path, limit: u64) -> Result<Self, DownloadError> {
        Ok(Self {
            file: tokio::fs::File::create(path).await?,
            written: 0,
            limit,
        })
    }

    pub async fn write(&mut self, bytes: &[u8]) -> Result<(), DownloadError> {
        use tokio::io::AsyncWriteExt;
        self.written += bytes.len() as u64;
        if self.written > self.limit {
            return Err(DownloadError::TooLarge {
                size: self.written,
                limit: self.limit,
            });
        }
        self.file.write_all(bytes).await?;
        Ok(())
    }

    pub fn written(&self) -> u64 {
        self.written
    }

    pub async fn finish(mut self) -> Result<u64, DownloadError> {
        use tokio::io::AsyncWriteExt;
        self.file.flush().await?;
        Ok(self.written)
    }
}

/// GETs `url`, or the byte range `range` of it, as one buffer.
pub async fn fetch_bytes(
    http: &Http,
    url: &Url,
    platform: &str,
    headers: &[(String, String)],
    range: Option<(u64, u64)>,
) -> Result<Bytes, DownloadError> {
    let mut request = http
        .get(url.clone())
        .platform(platform)
        .headers(headers)
        .media();
    if let Some((start, end)) = range {
        request = request.header("range", &format!("bytes={start}-{end}"));
    }
    let response = request.send().await?;
    let status = response.status;
    if !status.is_success() {
        return Err(DownloadError::Status {
            status: status.as_u16(),
            url: url.to_string(),
        });
    }
    Ok(response.bytes(usize::MAX).await?)
}

/// GETs a manifest or playlist as text, failing when longer than `limit`.
pub async fn fetch_text(
    http: &Http,
    url: &Url,
    platform: &str,
    headers: &[(String, String)],
    limit: usize,
) -> Result<(Url, String), DownloadError> {
    let response = http
        .get(url.clone())
        .platform(platform)
        .headers(headers)
        .send()
        .await?;
    let status = response.status;
    if !status.is_success() {
        return Err(DownloadError::Status {
            status: status.as_u16(),
            url: url.to_string(),
        });
    }
    let final_url = response.url.clone();
    let bytes = response.bytes(limit).await.map_err(|e| match e {
        crate::http::HttpError::BodyTooLarge { .. } => {
            DownloadError::Manifest(format!("manifest at {url} exceeds {limit} bytes"))
        }
        other => DownloadError::Http(other),
    })?;
    Ok((final_url, String::from_utf8_lossy(&bytes).into_owned()))
}

/// Remuxes locally downloaded streams into `dest` without touching the network.
pub async fn mux(
    ffmpeg: &Ffmpeg,
    video: &Path,
    audio: Option<&Path>,
    dest: &Path,
) -> Result<LocalFile, DownloadError> {
    let mut args: Vec<OsString> = vec![
        "-loglevel".into(),
        "warning".into(),
        "-i".into(),
        video.as_os_str().to_owned(),
    ];
    match audio {
        Some(audio) => {
            args.extend([
                "-i".into(),
                audio.as_os_str().to_owned(),
                "-map".into(),
                "0:v:0".into(),
                "-map".into(),
                "1:a:0".into(),
            ]);
        }
        None => {
            args.extend([
                "-map".into(),
                "0:v:0?".into(),
                "-map".into(),
                "0:a:0?".into(),
            ]);
        }
    }
    args.extend([
        "-c".into(),
        "copy".into(),
        "-sn".into(),
        "-dn".into(),
        "-f".into(),
        "matroska".into(),
        dest.as_os_str().to_owned(),
    ]);
    ffmpeg
        .run(args, |_| {})
        .await
        .map_err(|e| DownloadError::Process(e.to_string()))?;
    let file = LocalFile::from_path(dest.to_path_buf()).await?;
    if file.size == 0 {
        return Err(DownloadError::Empty);
    }
    Ok(file)
}
