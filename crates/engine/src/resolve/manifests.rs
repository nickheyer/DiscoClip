//! Expands the HLS and DASH manifests a platform lists among its renditions into the
//! streams they carry, so a download can pick a stream by size and bitrate.

use std::time::Duration;

use super::{ResolveError, SubtitleTrack, Variant, VariantKind, dash, hls, status_error};
use crate::http::{BROWSER_UA, Http, StatusCode};

/// A manifest that could not be read: the rendition as the platform listed it, the
/// status its host answered with when it answered at all, and what went wrong.
pub struct Unread {
    pub variant: Variant,
    pub status: Option<StatusCode>,
    pub error: ResolveError,
    /// How many of the expansion's streams were listed before this manifest.
    pub position: usize,
}

impl Unread {
    /// The host refused the request, with 401 or 403: the manifest is there, served to
    /// other clients or from other addresses.
    pub fn refused(&self) -> bool {
        matches!(self.status.map(|s| s.as_u16()), Some(401 | 403))
    }

    /// The host answered that the manifest is not there or may not be read, an answer
    /// asking again will not change: a 4xx other than rate limiting.
    pub fn definitive(&self) -> bool {
        self.status
            .is_some_and(|s| s.is_client_error() && s.as_u16() != 429)
    }
}

/// What the listed renditions expanded to, and the manifests that could not be read.
pub struct Expansion {
    /// The streams every readable manifest carries, and every rendition that is not a
    /// manifest, in the order listed.
    pub variants: Vec<Variant>,
    /// The manifests that could not be read, in the order listed.
    pub unread: Vec<Unread>,
}

/// Expands every HLS and DASH manifest among `variants` into its streams, read as
/// `platform`: the manifest's renditions, with their sizes and bitrates, take its place,
/// and the subtitle renditions it lists join `subtitles`. A manifest that cannot be read
/// is reported apart, with the status its host answered, for the caller to decide about.
pub async fn expand_each(
    http: &Http,
    platform: &str,
    variants: Vec<Variant>,
    subtitles: &mut Vec<SubtitleTrack>,
    duration: Option<Duration>,
) -> Expansion {
    let mut expansion = Expansion {
        variants: Vec::with_capacity(variants.len()),
        unread: Vec::new(),
    };
    for variant in variants {
        if !matches!(variant.kind, VariantKind::Hls | VariantKind::Dash) {
            expansion.variants.push(variant);
            continue;
        }
        match read(http, platform, &variant).await {
            Ok((streams, tracks)) if !streams.is_empty() => {
                for mut stream in streams {
                    if stream.duration.is_none() {
                        stream.duration = duration;
                    }
                    if stream.format_id.is_none() {
                        stream.format_id = variant.format_id.clone();
                    }
                    expansion.variants.push(stream);
                }
                for track in tracks {
                    if !subtitles.iter().any(|t| t.url == track.url) {
                        subtitles.push(track);
                    }
                }
            }
            Ok(_) => expansion.variants.push(variant),
            Err((status, error)) => expansion.unread.push(Unread {
                position: expansion.variants.len(),
                variant,
                status,
                error: *error,
            }),
        }
    }
    expansion
}

/// [`expand_each`], keeping a manifest that cannot be read as it was listed, since the
/// download picks a stream from it directly.
pub async fn expand_all(
    http: &Http,
    platform: &str,
    variants: Vec<Variant>,
    subtitles: &mut Vec<SubtitleTrack>,
    duration: Option<Duration>,
) -> Vec<Variant> {
    let Expansion {
        mut variants,
        unread,
    } = expand_each(http, platform, variants, subtitles, duration).await;
    // Later positions first, so each insertion leaves the earlier ones where they were.
    for unread in unread.into_iter().rev() {
        tracing::warn!(
            platform,
            url = %unread.variant.url,
            "{} manifest not expanded: {}",
            unread.variant.kind.as_str().to_ascii_uppercase(),
            unread.error
        );
        variants.insert(unread.position, unread.variant);
    }
    variants
}

/// The streams and subtitle renditions `variant`'s manifest carries. A failure comes with
/// the status the host answered, when the request got that far. The error is boxed so
/// the failure stays small next to the streams a success carries.
async fn read(
    http: &Http,
    platform: &str,
    variant: &Variant,
) -> Result<(Vec<Variant>, Vec<SubtitleTrack>), (Option<StatusCode>, Box<ResolveError>)> {
    let limit = match variant.kind {
        VariantKind::Hls => hls::MAX_PLAYLIST,
        _ => dash::MAX_MANIFEST,
    };
    let response = http
        .get(variant.url.clone())
        .platform(platform)
        .user_agent(BROWSER_UA)
        .headers(&variant.headers)
        .send()
        .await
        .map_err(|e| (None, Box::new(e.into())))?;
    let status = response.status;
    if let Some(error) = status_error(status, &variant.url) {
        return Err((Some(status), Box::new(error)));
    }
    let base = response.url.clone();
    let body = response
        .bytes(limit)
        .await
        .map_err(|e| (Some(status), Box::new(e.into())))?;
    let expanded = match variant.kind {
        VariantKind::Hls => hls::expand_playlist(
            http,
            &variant.url,
            &base,
            &body,
            platform,
            BROWSER_UA,
            &variant.headers,
        )
        .await
        .map(|e| (e.variants, e.subtitles)),
        _ => dash::expand_manifest(
            &variant.url,
            &base,
            &String::from_utf8_lossy(&body),
            &variant.headers,
        )
        .map(|e| (e.variants, e.subtitles)),
    };
    expanded.map_err(|e| (Some(status), Box::new(e)))
}

#[cfg(test)]
mod tests {
    use url::Url;

    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};

    const MASTER: &str = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=3500000,RESOLUTION=1280x720,CODECS=\"avc1.42E01E,mp4a.40.2\"\n720.m3u8\n";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n0.ts\n#EXT-X-ENDLIST\n";

    fn get(url: &str, status: u16, body: &str) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: "GET".into(),
                url: url.into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status,
                url: url.into(),
                headers: vec![("content-type".into(), "application/x-mpegURL".into())],
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    fn listed() -> Vec<Variant> {
        vec![
            Variant::hls(Url::parse("https://cdn.example/refused/master.m3u8").unwrap()),
            Variant::hls(Url::parse("https://cdn.example/open/master.m3u8").unwrap()),
            Variant::file(Url::parse("https://cdn.example/file.mp4").unwrap()),
            Variant::hls(Url::parse("https://cdn.example/gone/master.m3u8").unwrap()),
        ]
    }

    fn fixture() -> Fixture {
        let mut fixture = Fixture::new("test", None);
        fixture.exchanges.push(get(
            "https://cdn.example/refused/master.m3u8",
            403,
            "<html>Access Denied</html>",
        ));
        fixture
            .exchanges
            .push(get("https://cdn.example/open/master.m3u8", 200, MASTER));
        fixture
            .exchanges
            .push(get("https://cdn.example/open/720.m3u8", 200, MEDIA));
        fixture
            .exchanges
            .push(get("https://cdn.example/gone/master.m3u8", 404, ""));
        fixture
    }

    #[tokio::test]
    async fn unreadable_manifests_are_reported_with_the_status_their_host_answered() {
        let http = Http::replay(fixture());
        let mut subtitles = Vec::new();
        let expansion = expand_each(&http, "test", listed(), &mut subtitles, None).await;
        let urls: Vec<&str> = expansion.variants.iter().map(|v| v.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://cdn.example/open/720.m3u8",
                "https://cdn.example/file.mp4"
            ]
        );
        assert_eq!(expansion.variants[0].height, Some(720));
        assert_eq!(expansion.unread.len(), 2);
        let refused = &expansion.unread[0];
        assert_eq!(
            refused.variant.url.as_str(),
            "https://cdn.example/refused/master.m3u8"
        );
        assert_eq!(refused.status.map(|s| s.as_u16()), Some(403));
        assert!(refused.refused());
        assert!(refused.definitive());
        assert_eq!(refused.position, 0);
        assert!(
            matches!(&refused.error, ResolveError::Unavailable { reason, .. } if reason == "HTTP 403 Forbidden"),
            "{}",
            refused.error
        );
        let gone = &expansion.unread[1];
        assert_eq!(gone.status.map(|s| s.as_u16()), Some(404));
        assert!(!gone.refused());
        assert!(gone.definitive());
        assert_eq!(gone.position, 2);
        assert!(matches!(&gone.error, ResolveError::NotFound(_)));
    }

    #[tokio::test]
    async fn expand_all_keeps_unreadable_manifests_where_they_were_listed() {
        let http = Http::replay(fixture());
        let mut subtitles = Vec::new();
        let variants = expand_all(&http, "test", listed(), &mut subtitles, None).await;
        let urls: Vec<&str> = variants.iter().map(|v| v.url.as_str()).collect();
        assert_eq!(
            urls,
            [
                "https://cdn.example/refused/master.m3u8",
                "https://cdn.example/open/720.m3u8",
                "https://cdn.example/file.mp4",
                "https://cdn.example/gone/master.m3u8",
            ]
        );
        assert_eq!(variants[0].kind, VariantKind::Hls);
        assert_eq!(variants[3].kind, VariantKind::Hls);
    }

    #[tokio::test]
    async fn a_host_that_does_not_answer_leaves_the_status_unknown() {
        // The replay knows no exchange for the link, which the transport reports as
        // an error before any status.
        let http = Http::replay(Fixture::new("test", None));
        let mut subtitles = Vec::new();
        let listed = vec![Variant::hls(
            Url::parse("https://cdn.example/silent/master.m3u8").unwrap(),
        )];
        let expansion = expand_each(&http, "test", listed, &mut subtitles, None).await;
        assert!(expansion.variants.is_empty());
        assert_eq!(expansion.unread.len(), 1);
        assert_eq!(expansion.unread[0].status, None);
        assert!(!expansion.unread[0].refused());
        assert!(!expansion.unread[0].definitive());
    }
}
