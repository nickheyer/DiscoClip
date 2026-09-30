//! Expands HLS playlists into downloadable variants, with their audio and subtitle
//! renditions.

use std::time::Duration;

use m3u8_rs::{AlternativeMediaType, Playlist};
use url::Url;

use super::{
    ResolveError, SubtitleFormat, SubtitleTrack, Variant, VariantKind, check_status, fetch,
    parse_codecs, signed_url,
};
use crate::http::Http;

pub(super) const MAX_PLAYLIST: usize = 4 * 1024 * 1024;

pub struct Expanded {
    pub variants: Vec<Variant>,
    pub subtitles: Vec<SubtitleTrack>,
    pub duration: Option<Duration>,
    /// The playlist has no end: a live stream.
    pub live: bool,
}

/// Reads the master playlist at `url` as `platform`, returning one variant per stream,
/// one audio-only variant per audio rendition a stream can pair with, and every subtitle
/// rendition. A media playlist is one variant.
pub async fn expand(
    http: &Http,
    url: &Url,
    platform: &str,
    user_agent: &str,
    headers: &[(String, String)],
) -> Result<Expanded, ResolveError> {
    expand_signed(http, url, platform, user_agent, headers, &[]).await
}

/// [`expand`] for a host that checks a signature on every playlist: `query` is carried by
/// the master's request, by every playlist and rendition the master names, and by the
/// variants, so the download signs their playlists and segments the same way.
pub async fn expand_signed(
    http: &Http,
    url: &Url,
    platform: &str,
    user_agent: &str,
    headers: &[(String, String)],
    query: &[(String, String)],
) -> Result<Expanded, ResolveError> {
    let url = signed_url(url, query);
    let response = http
        .get(url.clone())
        .platform(platform)
        .user_agent(user_agent)
        .headers(headers)
        .send()
        .await?;
    check_status(&response, &url)?;
    let base = response.url.clone();
    let body = response.bytes(MAX_PLAYLIST).await?;
    expand_playlist_signed(
        http, &url, &base, &body, platform, user_agent, headers, query,
    )
    .await
}

/// [`expand`] for a playlist already read: `body` came from `base`, and `url` names the
/// playlist in errors.
pub async fn expand_playlist(
    http: &Http,
    url: &Url,
    base: &Url,
    body: &[u8],
    platform: &str,
    user_agent: &str,
    headers: &[(String, String)],
) -> Result<Expanded, ResolveError> {
    expand_playlist_signed(http, url, base, body, platform, user_agent, headers, &[]).await
}

/// [`expand_playlist`] with `query` carried by every link the playlist names, as
/// [`expand_signed`] carries it.
#[allow(clippy::too_many_arguments)]
async fn expand_playlist_signed(
    http: &Http,
    url: &Url,
    base: &Url,
    body: &[u8],
    platform: &str,
    user_agent: &str,
    headers: &[(String, String)],
    query: &[(String, String)],
) -> Result<Expanded, ResolveError> {
    let base = base.clone();
    let join = |uri: &str| base.join(uri).map(|joined| signed_url(&joined, query));
    match m3u8_rs::parse_playlist_res(body) {
        Ok(Playlist::MasterPlaylist(master)) => {
            let mut variants = Vec::new();
            let mut audio_variants: Vec<Variant> = Vec::new();
            for stream in master.variants.iter().filter(|v| !v.is_i_frame) {
                let stream_url = join(&stream.uri).map_err(|e| {
                    ResolveError::malformed(url, format!("bad variant uri {}: {e}", stream.uri))
                })?;
                let mut variant = Variant::new(stream_url, VariantKind::Hls);
                variant.bitrate = Some(stream.average_bandwidth.unwrap_or(stream.bandwidth));
                if let Some(res) = &stream.resolution {
                    variant.width = Some(res.width as u32);
                    variant.height = Some(res.height as u32);
                }
                variant.fps = stream.frame_rate;
                if let Some(codecs) = &stream.codecs {
                    let (video, audio) = parse_codecs(Some(codecs));
                    variant.audio_only =
                        video.is_none() && audio.is_some() && stream.resolution.is_none();
                    variant.video = video;
                    variant.audio = audio;
                    variant.codecs = Some(codecs.clone());
                }
                // The renditions of the stream's audio group, each an audio-only variant
                // the stream is paired with by language when the job is planned.
                let mut renditions = 0;
                if let Some(group) = &stream.audio {
                    for alternative in master.alternatives.iter().filter(|a| {
                        a.media_type == AlternativeMediaType::Audio && a.group_id == *group
                    }) {
                        let Some(uri) = alternative.uri.as_deref() else {
                            continue;
                        };
                        let rendition_url = join(uri).map_err(|e| {
                            ResolveError::malformed(url, format!("bad audio uri {uri}: {e}"))
                        })?;
                        renditions += 1;
                        if audio_variants.iter().any(|v| v.url == rendition_url) {
                            continue;
                        }
                        let mut audio = Variant::new(rendition_url, VariantKind::Hls);
                        audio.audio_only = true;
                        audio.language = alternative.language.clone();
                        audio.label = Some(alternative.name.clone());
                        audio.audio_track = Some(format!("{group}:{}", alternative.name));
                        audio.audio_default = alternative.default;
                        audio.headers = headers.to_vec();
                        audio.query = query.to_vec();
                        audio_variants.push(audio);
                    }
                }
                if let Some(res) = &stream.resolution {
                    variant.label = Some(match stream.frame_rate {
                        Some(fps) if fps > 30.5 => format!("{}p{}", res.height, fps.round()),
                        _ => format!("{}p", res.height),
                    });
                } else if variant.audio_only {
                    variant.label = Some(match variant.bitrate {
                        Some(bitrate) => format!("audio {}k", bitrate / 1000),
                        None => "audio".to_string(),
                    });
                }
                // A stream with audio renditions to pair with is taken as the picture
                // alone, so the sound comes from the rendition in the language wanted.
                // So is one whose codecs name no audio.
                variant.video_only = renditions > 0
                    || (variant.video.is_some()
                        && variant.audio.is_none()
                        && stream.codecs.is_some());
                variant.headers = headers.to_vec();
                variant.query = query.to_vec();
                variants.push(variant);
            }
            let mut subtitles = Vec::new();
            for alternative in master
                .alternatives
                .iter()
                .filter(|a| a.media_type == AlternativeMediaType::Subtitles)
            {
                if let Some(uri) = &alternative.uri
                    && let Ok(track_url) = join(uri)
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
            if variants.is_empty() {
                return Err(ResolveError::NotFound(url.clone()));
            }
            let probe = variants
                .iter()
                .max_by_key(|v| v.bitrate.unwrap_or(0))
                .map(|v| v.url.clone());
            let (duration, live) = match probe {
                Some(media) => media_duration(http, &media, platform, user_agent, headers).await?,
                None => (None, false),
            };
            variants.extend(audio_variants);
            for v in &mut variants {
                v.duration = duration;
                v.live = live;
            }
            Ok(Expanded {
                variants,
                subtitles,
                duration,
                live,
            })
        }
        Ok(Playlist::MediaPlaylist(media)) => {
            let duration = sum_segments(&media);
            let live = !media.end_list;
            let mut variant = Variant::new(signed_url(&base, query), VariantKind::Hls);
            variant.duration = duration;
            variant.live = live;
            variant.headers = headers.to_vec();
            variant.query = query.to_vec();
            Ok(Expanded {
                variants: vec![variant],
                subtitles: Vec::new(),
                duration,
                live,
            })
        }
        Err(error) => Err(ResolveError::malformed(
            url,
            format!("invalid playlist: {error}"),
        )),
    }
}

async fn media_duration(
    http: &Http,
    url: &Url,
    platform: &str,
    user_agent: &str,
    headers: &[(String, String)],
) -> Result<(Option<Duration>, bool), ResolveError> {
    let fetched = fetch(http, url, platform, user_agent, headers, MAX_PLAYLIST).await?;
    check_status_code(fetched.status, url)?;
    match m3u8_rs::parse_playlist_res(&fetched.body) {
        Ok(Playlist::MediaPlaylist(media)) => Ok((sum_segments(&media), !media.end_list)),
        Ok(Playlist::MasterPlaylist(_)) => Ok((None, false)),
        Err(error) => Err(ResolveError::malformed(
            url,
            format!("invalid media playlist: {error}"),
        )),
    }
}

fn check_status_code(status: crate::http::StatusCode, url: &Url) -> Result<(), ResolveError> {
    super::status_error(status, url).map_or(Ok(()), Err)
}

fn sum_segments(media: &m3u8_rs::MediaPlaylist) -> Option<Duration> {
    if media.segments.is_empty() {
        return None;
    }
    let total: f64 = media.segments.iter().map(|s| s.duration as f64).sum();
    Some(Duration::from_secs_f64(total.max(0.0)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

    fn fixture(pairs: &[(&str, &str)]) -> Fixture {
        let mut fixture = Fixture::new("hls", None);
        for (url, body) in pairs {
            fixture.exchanges.push(Exchange {
                request: RecordedRequest {
                    method: "GET".into(),
                    url: url.to_string(),
                    headers: Vec::new(),
                    body: None,
                },
                response: RecordedResponse {
                    status: 200,
                    url: url.to_string(),
                    headers: vec![(
                        "content-type".into(),
                        "application/vnd.apple.mpegurl".into(),
                    )],
                    body: RecordedBody::Text(body.to_string()),
                    truncated: false,
                },
            });
        }
        fixture
    }

    #[tokio::test]
    async fn master_playlists_expand_with_audio_and_subtitles() {
        let master = "#EXTM3U\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",DEFAULT=YES,URI=\"audio/en.m3u8\"\n\
            #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"Deutsch\",LANGUAGE=\"de\",URI=\"subs/de.m3u8\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=1280x720,CODECS=\"avc1.64001f,mp4a.40.2\",AUDIO=\"aud\",FRAME-RATE=60.000\n\
            720.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=500000,RESOLUTION=640x360,CODECS=\"avc1.64001e,mp4a.40.2\",AUDIO=\"aud\"\n\
            360.m3u8\n";
        let media = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\ns1.ts\n#EXTINF:5.5,\ns2.ts\n#EXT-X-ENDLIST\n";
        let http = Http::replay(fixture(&[
            ("https://cdn.test/v/master.m3u8", master),
            ("https://cdn.test/v/720.m3u8", media),
        ]));
        let expanded = expand(
            &http,
            &Url::parse("https://cdn.test/v/master.m3u8").unwrap(),
            "web",
            "ua",
            &[("referer".into(), "https://site.test/".into())],
        )
        .await
        .unwrap();
        // Two streams, and the one audio rendition they share as a variant of its own.
        assert_eq!(expanded.variants.len(), 3);
        let best = &expanded.variants[0];
        assert_eq!(best.url.as_str(), "https://cdn.test/v/720.m3u8");
        assert!(
            best.video_only,
            "a stream with renditions pairs by language"
        );
        assert!(best.audio_url.is_none());
        let rendition = &expanded.variants[2];
        assert!(rendition.audio_only);
        assert_eq!(rendition.url.as_str(), "https://cdn.test/v/audio/en.m3u8");
        assert_eq!(rendition.audio_track.as_deref(), Some("aud:English"));
        assert!(rendition.audio_default);
        assert_eq!(rendition.label.as_deref(), Some("English"));
        let paired = crate::plan::select_variant(
            &expanded.variants,
            &crate::config::Limits::default(),
            "web",
            crate::media::MediaKind::Video,
            "en",
        )
        .unwrap();
        assert_eq!(paired.url.as_str(), "https://cdn.test/v/720.m3u8");
        assert_eq!(
            paired.audio_url.as_ref().unwrap().as_str(),
            "https://cdn.test/v/audio/en.m3u8"
        );
        assert_eq!(best.height, Some(720));
        assert_eq!(best.label.as_deref(), Some("720p60"));
        assert_eq!(best.video, Some(crate::media::VideoCodec::H264));
        assert_eq!(best.headers[0].0, "referer");
        assert_eq!(expanded.duration, Some(Duration::from_secs_f64(15.5)));
        assert!(!expanded.live);
        assert_eq!(expanded.subtitles.len(), 1);
        assert_eq!(expanded.subtitles[0].language, "de");
        assert_eq!(expanded.subtitles[0].format, SubtitleFormat::HlsVtt);
    }

    #[tokio::test]
    async fn media_playlists_without_end_are_live() {
        let media = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXT-X-MEDIA-SEQUENCE:100\n#EXTINF:6.0,\n100.ts\n#EXTINF:6.0,\n101.ts\n";
        let http = Http::replay(fixture(&[("https://cdn.test/live.m3u8", media)]));
        let expanded = expand(
            &http,
            &Url::parse("https://cdn.test/live.m3u8").unwrap(),
            "web",
            "ua",
            &[],
        )
        .await
        .unwrap();
        assert_eq!(expanded.variants.len(), 1);
        assert!(expanded.live);
        assert!(expanded.variants[0].live);
        assert_eq!(expanded.duration, Some(Duration::from_secs(12)));
    }
}
