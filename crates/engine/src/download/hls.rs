//! Downloads HLS media playlists segment by segment, decrypting AES-128 when present,
//! then muxes video and audio locally.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use aes::cipher::{BlockModeDecrypt, KeyIvInit, block_padding::Pkcs7};
use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use m3u8_rs::{KeyMethod, MediaPlaylist, Playlist};
use url::Url;

use super::segments::{Sink, fetch_bytes, fetch_text, mux};
use super::{DownloadContext, DownloadError, Downloaded, Downloader};
use crate::event::{Progress, ProgressSender};
use crate::ffmpeg::Ffmpeg;
use crate::http::Http;
use crate::resolve::{Variant, VariantKind};

const MAX_PLAYLIST: usize = 8 * 1024 * 1024;
const CONCURRENCY: usize = 4;

pub struct HlsDownloader {
    http: Http,
    ffmpeg: Ffmpeg,
}

impl HlsDownloader {
    pub fn new(http: Http, ffmpeg: Ffmpeg) -> Self {
        Self { http, ffmpeg }
    }
}

struct Piece {
    url: Url,
    range: Option<(u64, u64)>,
    key: Option<KeySpec>,
}

#[derive(Clone)]
struct KeySpec {
    url: Url,
    iv: [u8; 16],
}

struct Track {
    init: Option<Piece>,
    pieces: Vec<Piece>,
    extension: &'static str,
}

type OwnedPiece = (Url, Option<(u64, u64)>, Option<KeySpec>);

struct Counter<'a> {
    progress: &'a ProgressSender,
    done: u64,
    total: u64,
}

impl Counter<'_> {
    fn tick(&mut self) {
        self.done += 1;
        self.progress.send_replace(Progress {
            done: self.done,
            total: Some(self.total),
        });
    }
}

async fn load_media_playlist(
    http: &Http,
    url: &Url,
    platform: &str,
    headers: &[(String, String)],
    depth: u8,
) -> Result<(Url, MediaPlaylist), DownloadError> {
    let (final_url, text) = fetch_text(http, url, platform, headers, MAX_PLAYLIST).await?;
    match m3u8_rs::parse_playlist_res(text.as_bytes()) {
        Ok(Playlist::MediaPlaylist(media)) => Ok((final_url, media)),
        Ok(Playlist::MasterPlaylist(master)) => {
            if depth == 0 {
                return Err(DownloadError::Manifest("nested master playlists".into()));
            }
            let best = master
                .variants
                .iter()
                .filter(|v| !v.is_i_frame)
                .max_by_key(|v| v.bandwidth)
                .ok_or_else(|| DownloadError::Manifest("master playlist has no variants".into()))?;
            let next = final_url
                .join(&best.uri)
                .map_err(|e| DownloadError::Manifest(format!("bad variant uri: {e}")))?;
            Box::pin(load_media_playlist(
                http,
                &next,
                platform,
                headers,
                depth - 1,
            ))
            .await
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

fn build_track(base: &Url, media: &MediaPlaylist) -> Result<Track, DownloadError> {
    let mut pieces = Vec::with_capacity(media.segments.len());
    let mut init = None;
    let mut key: Option<(Url, Option<[u8; 16]>)> = None;
    let mut next_offset = 0u64;
    let mut fragmented = false;
    for (index, segment) in media.segments.iter().enumerate() {
        if let Some(k) = &segment.key {
            match k.method {
                KeyMethod::None => key = None,
                KeyMethod::AES128 => {
                    let uri = k
                        .uri
                        .as_deref()
                        .ok_or_else(|| DownloadError::Manifest("AES-128 key without URI".into()))?;
                    let key_url = base
                        .join(uri)
                        .map_err(|e| DownloadError::Manifest(format!("bad key uri: {e}")))?;
                    let iv = k.iv.as_deref().and_then(parse_iv);
                    key = Some((key_url, iv));
                }
                ref other => {
                    return Err(DownloadError::Manifest(format!(
                        "unsupported segment encryption {other:?}"
                    )));
                }
            }
        }
        if let Some(map) = &segment.map
            && init.is_none()
        {
            fragmented = true;
            let url = base
                .join(&map.uri)
                .map_err(|e| DownloadError::Manifest(format!("bad init uri: {e}")))?;
            let range = map.byte_range.as_ref().map(|r| {
                let start = r.offset.unwrap_or(0);
                (start, start + r.length.saturating_sub(1))
            });
            init = Some(Piece {
                url,
                range,
                key: None,
            });
        }
        let url = base
            .join(&segment.uri)
            .map_err(|e| DownloadError::Manifest(format!("bad segment uri: {e}")))?;
        let range = segment.byte_range.as_ref().map(|r| {
            let start = r.offset.unwrap_or(next_offset);
            next_offset = start + r.length;
            (start, start + r.length.saturating_sub(1))
        });
        if range.is_none() {
            next_offset = 0;
        }
        let spec = key.as_ref().map(|(url, iv)| KeySpec {
            url: url.clone(),
            iv: iv.unwrap_or_else(|| sequence_iv(media.media_sequence + index as u64)),
        });
        if segment.uri.ends_with(".m4s") || segment.uri.ends_with(".mp4") {
            fragmented = true;
        }
        pieces.push(Piece {
            url,
            range,
            key: spec,
        });
    }
    if pieces.is_empty() {
        return Err(DownloadError::Manifest("playlist has no segments".into()));
    }
    Ok(Track {
        init,
        pieces,
        extension: if fragmented { "mp4" } else { "ts" },
    })
}

fn decrypt(data: &[u8], key: &[u8], iv: &[u8; 16]) -> Result<Vec<u8>, DownloadError> {
    let decryptor = cbc::Decryptor::<aes::Aes128>::new_from_slices(key, iv)
        .map_err(|e| DownloadError::Manifest(format!("bad AES-128 key: {e}")))?;
    decryptor
        .decrypt_padded_vec::<Pkcs7>(data)
        .map_err(|e| DownloadError::Manifest(format!("segment decryption failed: {e}")))
}

impl HlsDownloader {
    async fn download_track(
        &self,
        track: &Track,
        platform: &str,
        headers: &[(String, String)],
        path: &Path,
        limit: u64,
        counter: &mut Counter<'_>,
    ) -> Result<u64, DownloadError> {
        let mut sink = Sink::create(path, limit).await?;
        if let Some(init) = &track.init {
            let bytes = fetch_bytes(&self.http, &init.url, platform, headers, init.range).await?;
            sink.write(&bytes).await?;
        }
        let mut keys: HashMap<String, Bytes> = HashMap::new();
        let http = self.http.clone();
        let platform_owned = platform.to_string();
        let headers_owned = headers.to_vec();
        let owned: Vec<OwnedPiece> = track
            .pieces
            .iter()
            .map(|p| (p.url.clone(), p.range, p.key.clone()))
            .collect();
        let fetches = futures::stream::iter(owned.into_iter().map(move |(url, range, key)| {
            let http = http.clone();
            let platform = platform_owned.clone();
            let headers = headers_owned.clone();
            async move {
                let bytes = fetch_bytes(&http, &url, &platform, &headers, range).await?;
                Ok::<(Bytes, Option<KeySpec>), DownloadError>((bytes, key))
            }
        }))
        .buffered(CONCURRENCY);
        futures::pin_mut!(fetches);
        while let Some(item) = fetches.next().await {
            let (bytes, key) = item?;
            match key {
                Some(spec) => {
                    let key_bytes = match keys.get(spec.url.as_str()) {
                        Some(k) => k.clone(),
                        None => {
                            let k =
                                fetch_bytes(&self.http, &spec.url, platform, headers, None).await?;
                            keys.insert(spec.url.to_string(), k.clone());
                            k
                        }
                    };
                    let plain = decrypt(&bytes, &key_bytes, &spec.iv)?;
                    sink.write(&plain).await?;
                }
                None => sink.write(&bytes).await?,
            }
            counter.tick();
        }
        sink.finish().await
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
        let (video_base, video_playlist) =
            load_media_playlist(&self.http, &variant.url, platform, headers, 2).await?;
        let video = build_track(&video_base, &video_playlist)?;
        let audio = match &variant.audio_url {
            Some(url) => {
                let (base, playlist) =
                    load_media_playlist(&self.http, url, platform, headers, 2).await?;
                Some(build_track(&base, &playlist)?)
            }
            None => None,
        };
        let total = (video.pieces.len() + audio.as_ref().map_or(0, |a| a.pieces.len())) as u64;
        progress.send_replace(Progress {
            done: 0,
            total: Some(total),
        });
        let mut counter = Counter {
            progress: &progress,
            done: 0,
            total,
        };

        let video_path: PathBuf = dest_dir.join(format!("video.{}", video.extension));
        let written = self
            .download_track(
                &video,
                platform,
                headers,
                &video_path,
                context.max_bytes,
                &mut counter,
            )
            .await?;
        let audio_path = match &audio {
            Some(track) => {
                let path = dest_dir.join(format!("audio.{}", track.extension));
                let remaining = context.max_bytes.saturating_sub(written).max(1);
                self.download_track(track, platform, headers, &path, remaining, &mut counter)
                    .await?;
                Some(path)
            }
            None => None,
        };

        let dest = dest_dir.join("source.mkv");
        let file = mux(&self.ffmpeg, &video_path, audio_path.as_deref(), &dest).await?;
        let _ = tokio::fs::remove_file(&video_path).await;
        if let Some(path) = audio_path {
            let _ = tokio::fs::remove_file(path).await;
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
    use super::parse_iv;

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
}
