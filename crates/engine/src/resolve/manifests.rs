//! Expands the HLS and DASH manifests a platform lists among its renditions into the
//! streams they carry, so a download can pick a stream by size and bitrate.

use std::time::Duration;

use super::{SubtitleTrack, Variant, VariantKind, dash, hls};
use crate::http::{BROWSER_UA, Http};

/// Expands every HLS and DASH manifest among `variants` into its streams, read as
/// `platform`: the manifest's renditions, with their sizes and bitrates, replace it, and
/// the subtitle renditions it lists join `subtitles`. A manifest that cannot be read
/// stays as it is, since the download picks a stream from it directly.
pub async fn expand_all(
    http: &Http,
    platform: &str,
    variants: Vec<Variant>,
    subtitles: &mut Vec<SubtitleTrack>,
    duration: Option<Duration>,
) -> Vec<Variant> {
    let mut out = Vec::with_capacity(variants.len());
    for variant in variants {
        match variant.kind {
            VariantKind::Hls => {
                match hls::expand(http, &variant.url, platform, BROWSER_UA, &variant.headers).await
                {
                    Ok(expanded) if !expanded.variants.is_empty() => {
                        for mut stream in expanded.variants {
                            if stream.duration.is_none() {
                                stream.duration = duration;
                            }
                            if stream.format_id.is_none() {
                                stream.format_id = variant.format_id.clone();
                            }
                            out.push(stream);
                        }
                        for track in expanded.subtitles {
                            if !subtitles.iter().any(|t| t.url == track.url) {
                                subtitles.push(track);
                            }
                        }
                    }
                    Ok(_) => out.push(variant),
                    Err(error) => {
                        tracing::warn!(platform, url = %variant.url, "HLS manifest not expanded: {error}");
                        out.push(variant);
                    }
                }
            }
            VariantKind::Dash => {
                match dash::expand(http, &variant.url, platform, BROWSER_UA, &variant.headers).await
                {
                    Ok(expanded) if !expanded.variants.is_empty() => {
                        for mut stream in expanded.variants {
                            if stream.duration.is_none() {
                                stream.duration = duration;
                            }
                            if stream.format_id.is_none() {
                                stream.format_id = variant.format_id.clone();
                            }
                            out.push(stream);
                        }
                        for track in expanded.subtitles {
                            if !subtitles.iter().any(|t| t.url == track.url) {
                                subtitles.push(track);
                            }
                        }
                    }
                    Ok(_) => out.push(variant),
                    Err(error) => {
                        tracing::warn!(platform, url = %variant.url, "DASH manifest not expanded: {error}");
                        out.push(variant);
                    }
                }
            }
            _ => out.push(variant),
        }
    }
    out
}
