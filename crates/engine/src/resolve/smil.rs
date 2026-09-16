//! SMIL manifests as yt-dlp reads them: every `video`, `audio` and `media` entry becomes
//! a variant (RTMP, HLS, DASH, Smooth Streaming or a plain file, by its source) and every
//! `textstream` a subtitle track. thePlatform and Turner both publish SMIL.

use std::sync::LazyLock;

use regex::Regex;
use url::Url;

use super::{
    MAX_PAGE, ResolveError, SubtitleFormat, SubtitleTrack, Variant, VariantKind, dash, fetch_ok,
    hls, ism, probe_file, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

/// What a SMIL names.
#[derive(Debug, Default)]
pub struct Expanded {
    pub variants: Vec<Variant>,
    pub subtitles: Vec<SubtitleTrack>,
}

static RE_ISM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\.ism/[Mm]anifest").unwrap());

/// One `video`, `audio` or `media` element.
struct Medium {
    src: String,
    /// Bits per second.
    bitrate: Option<u64>,
    size: Option<u64>,
    width: Option<u32>,
    height: Option<u32>,
    proto: Option<String>,
    ext: Option<String>,
    streamer: Option<String>,
}

/// One `textstream` element.
struct TextStream {
    src: String,
    ext: Option<String>,
    language: String,
}

/// Fetches the SMIL at `url` as `platform` and expands it; `headers` go with that
/// request. `rtmp_play_path_prefix` goes before RTMP play paths (thePlatform's `mp4:`).
pub async fn expand(
    http: &Http,
    platform: &str,
    url: &Url,
    headers: &[(String, String)],
    rtmp_play_path_prefix: &str,
) -> Result<Expanded, ResolveError> {
    let fetched = fetch_ok(http, url, platform, BROWSER_UA, headers, MAX_PAGE).await?;
    let text = fetched.text();
    parse(http, platform, &text, &fetched.url, rtmp_play_path_prefix).await
}

/// [`expand`] for a SMIL already read: `text` came from `smil_url`, which relative
/// sources resolve against unless a `head/meta` names another base.
pub async fn parse(
    http: &Http,
    platform: &str,
    text: &str,
    smil_url: &Url,
    rtmp_play_path_prefix: &str,
) -> Result<Expanded, ResolveError> {
    let (base, media, textstreams) = read(text, smil_url)?;
    let mut expanded = Expanded::default();
    let mut rtmp_count = 0u64;
    let mut http_count = 0u64;
    let mut m3u8_count = 0u64;
    for medium in media {
        let tbr = medium.bitrate.map(|b| b / 1000);
        let streamer = medium.streamer.clone().unwrap_or_else(|| base.clone());
        if medium.proto.as_deref() == Some("rtmp") || streamer.starts_with("rtmp") {
            rtmp_count += 1;
            let play_path = format!("{rtmp_play_path_prefix}{}", medium.src);
            let Ok(url) = Url::parse(&format!("{}/{play_path}", streamer.trim_end_matches('/')))
            else {
                continue;
            };
            let mut variant = Variant::new(url, VariantKind::Rtmp);
            variant.container = Some(Container::Flv);
            variant.format_id = Some(format!("rtmp-{}", tbr.unwrap_or(rtmp_count)));
            variant.bitrate = medium.bitrate;
            variant.size = medium.size;
            variant.width = medium.width;
            variant.height = medium.height;
            expanded.variants.push(variant);
            continue;
        }
        let Some(src_url) = source_url(&base, &medium.src) else {
            continue;
        };
        let mut src_ext = extension_of(&medium.src).or_else(|| medium.ext.clone());
        if src_ext.is_none() && medium.proto.as_deref() != Some("m3u8") {
            src_ext = probed_extension(http, platform, &src_url).await;
        }
        if medium.proto.as_deref() == Some("m3u8") || src_ext.as_deref() == Some("m3u8") {
            match hls::expand(http, &src_url, platform, BROWSER_UA, &[]).await {
                Ok(streams) => {
                    let mut variants = streams.variants;
                    if variants.len() == 1 {
                        m3u8_count += 1;
                        let only = &mut variants[0];
                        only.format_id = Some(format!("hls-{}", tbr.unwrap_or(m3u8_count)));
                        only.bitrate = medium.bitrate.or(only.bitrate);
                        only.width = medium.width.or(only.width);
                        only.height = medium.height.or(only.height);
                    }
                    for variant in &mut variants {
                        if variant.format_id.is_none() {
                            variant.format_id =
                                Some(format!("hls-{}", variant.bitrate.unwrap_or(0) / 1000));
                        }
                        variant.container = medium
                            .ext
                            .as_deref()
                            .and_then(Container::from_extension)
                            .or(Some(Container::Mp4));
                    }
                    expanded.variants.extend(variants);
                    expanded.subtitles.extend(streams.subtitles);
                }
                Err(error) => tracing::warn!(url = %src_url, %error, "SMIL HLS entry skipped"),
            }
        } else if src_ext.as_deref() == Some("f4m") {
            continue;
        } else if src_ext.as_deref() == Some("mpd") {
            match dash::expand(http, &src_url, platform, BROWSER_UA, &[]).await {
                Ok(streams) => {
                    expanded.variants.extend(streams.variants);
                    expanded.subtitles.extend(streams.subtitles);
                }
                Err(error) => tracing::warn!(url = %src_url, %error, "SMIL DASH entry skipped"),
            }
        } else if RE_ISM.is_match(src_url.as_str()) {
            match ism::expand(http, &src_url, platform, BROWSER_UA, &[]).await {
                Ok(streams) => expanded.variants.extend(streams.variants),
                Err(error) => tracing::warn!(url = %src_url, %error, "SMIL ISM entry skipped"),
            }
        } else if matches!(src_url.scheme(), "http" | "https") {
            // A file the SMIL links in full is asked for first, as yt-dlp does, and dropped
            // when its host refuses it; one named under the base is taken as it is.
            let probed = if medium.src.trim().starts_with("http") {
                match probe_file(http, &src_url, platform, BROWSER_UA, &[]).await {
                    Ok(probed) if probed.status.is_success() => Some(probed),
                    Ok(probed) => {
                        tracing::warn!(url = %src_url, status = %probed.status, "SMIL file skipped");
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(url = %src_url, %error, "SMIL file skipped");
                        continue;
                    }
                }
            } else {
                None
            };
            http_count += 1;
            let ext = medium
                .ext
                .clone()
                .or(src_ext)
                .unwrap_or_else(|| "flv".to_string());
            let mut variant = Variant::file(src_url);
            variant.container = Container::from_extension(&ext)
                .or_else(|| {
                    probed
                        .as_ref()
                        .and_then(|p| p.content_type.as_deref())
                        .and_then(Container::from_mime)
                })
                .or_else(|| Some(Container::Other(ext.clone())));
            if matches!(variant.container, Some(Container::Mp4)) {
                variant.video = Some(VideoCodec::H264);
                variant.audio = Some(AudioCodec::Aac);
            }
            variant.format_id = Some(format!("http-{}", tbr.unwrap_or(http_count)));
            variant.bitrate = medium.bitrate;
            variant.size = medium.size.or(probed.as_ref().and_then(|p| p.size));
            variant.width = medium.width;
            variant.height = medium.height;
            expanded.variants.push(variant);
        }
    }
    for stream in textstreams {
        let Some(format) = stream.ext.as_deref().and_then(subtitle_format) else {
            continue;
        };
        let Some(url) = source_url(&base, &stream.src) else {
            continue;
        };
        expanded.subtitles.push(SubtitleTrack {
            url,
            language: stream.language,
            name: None,
            format,
            auto: false,
            headers: Vec::new(),
        });
    }
    Ok(expanded)
}

/// The base and the media and text entries of a SMIL, in yt-dlp's order: every `video`,
/// then every `audio`, then every `media`, without repeated sources.
fn read(
    text: &str,
    smil_url: &Url,
) -> Result<(String, Vec<Medium>, Vec<TextStream>), ResolveError> {
    let document =
        util::xml(text).ok_or_else(|| ResolveError::malformed(smil_url, "the SMIL is not XML"))?;
    let root = document.root_element();
    let base = util::xml_find(root, "head")
        .into_iter()
        .flat_map(|head| head.children())
        .filter(|node| node.is_element() && node.tag_name().name() == "meta")
        .find_map(|meta| {
            meta.attribute("base")
                .or_else(|| meta.attribute("httpBase"))
                .map(str::trim)
                .filter(|b| !b.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| smil_url.to_string());
    let attr = |node: roxmltree::Node<'_, '_>, name: &str| -> Option<String> {
        node.attribute(name)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let mut seen = std::collections::HashSet::new();
    let mut media = Vec::new();
    for name in ["video", "audio", "media"] {
        for node in util::xml_find_all(root, name) {
            let Some(src) = attr(node, "src") else {
                continue;
            };
            if !seen.insert(src.clone()) {
                continue;
            }
            let bitrate = attr(node, "system-bitrate")
                .or_else(|| attr(node, "systemBitrate"))
                .and_then(|b| b.parse::<f64>().ok())
                .filter(|b| *b > 0.0)
                .map(|b| b as u64);
            media.push(Medium {
                src,
                bitrate,
                size: attr(node, "size")
                    .or_else(|| attr(node, "fileSize"))
                    .and_then(|s| s.parse().ok()),
                width: attr(node, "width").and_then(|w| w.parse().ok()),
                height: attr(node, "height").and_then(|h| h.parse().ok()),
                proto: attr(node, "proto"),
                ext: attr(node, "ext"),
                streamer: attr(node, "streamer"),
            });
        }
    }
    let mut textstreams = Vec::new();
    for node in util::xml_find_all(root, "textstream") {
        let Some(src) = attr(node, "src") else {
            continue;
        };
        if !seen.insert(src.clone()) {
            continue;
        }
        let ext = attr(node, "ext")
            .or_else(|| attr(node, "type").and_then(|t| mime_extension(&t)))
            .or_else(|| extension_of(&src));
        let language = attr(node, "systemLanguage")
            .or_else(|| attr(node, "systemLanguageName"))
            .or_else(|| attr(node, "lang"))
            .unwrap_or_else(|| "en".to_string());
        textstreams.push(TextStream { src, ext, language });
    }
    Ok((base, media, textstreams))
}

/// `src` as a link: absolute ones as they are, others under `base/`.
fn source_url(base: &str, src: &str) -> Option<Url> {
    let src = src.trim();
    if src.starts_with("http") {
        return Url::parse(src).ok();
    }
    let base = Url::parse(&format!("{}/", base.trim_end_matches('/'))).ok()?;
    base.join(src).ok()
}

/// The extension a file answers with: from its `Content-Disposition` name, else its
/// content type.
async fn probed_extension(http: &Http, platform: &str, url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let probed = probe_file(http, url, platform, BROWSER_UA, &[])
        .await
        .ok()?;
    probed
        .filename
        .as_deref()
        .and_then(extension_of)
        .or_else(|| probed.content_type.as_deref().and_then(mime_extension))
}

const KNOWN_EXTENSIONS: &[&str] = &[
    "mp4", "m4a", "m4p", "m4b", "m4r", "m4v", "aac", "flv", "f4v", "f4a", "f4b", "webm", "ogg",
    "ogv", "oga", "ogx", "spx", "opus", "mkv", "mka", "mk3d", "avi", "divx", "mov", "asf", "wmv",
    "wma", "3gp", "3g2", "mp3", "flac", "ape", "wav", "f4f", "f4m", "m3u8", "smil", "mpd", "ism",
    "ts", "vtt", "srt", "ttml", "dfxp", "xml", "scc", "png", "jpg", "jpeg", "gif",
];

/// The extension of a link or file name (yt-dlp's `determine_ext` without a default):
/// what follows the last dot before any query, when it is a plain word or a known
/// extension followed by `/`.
pub fn extension_of(source: &str) -> Option<String> {
    let path = source.split('?').next().unwrap_or("");
    if !path.contains('.') {
        return None;
    }
    let guess = path.rsplit('.').next().unwrap_or("");
    if !guess.is_empty() && guess.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Some(guess.to_string());
    }
    let trimmed = guess.trim_end_matches('/');
    KNOWN_EXTENSIONS
        .contains(&trimmed)
        .then(|| trimmed.to_string())
}

/// The extension a media type maps to, as yt-dlp's `mimetype2ext`.
pub fn mime_extension(mime: &str) -> Option<String> {
    let mimetype = mime
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    if mimetype.is_empty() {
        return None;
    }
    let subtype = mimetype.rsplit('/').next().unwrap_or("").to_string();
    let after_plus = subtype.rsplit('+').next().unwrap_or("").to_string();
    const MAP: &[(&str, &str)] = &[
        ("3gpp", "3gp"),
        ("mp2t", "ts"),
        ("mp4", "mp4"),
        ("mpeg", "mpeg"),
        ("mpegurl", "m3u8"),
        ("quicktime", "mov"),
        ("webm", "webm"),
        ("vp9", "vp9"),
        ("video/ogg", "ogv"),
        ("x-flv", "flv"),
        ("x-m4v", "m4v"),
        ("x-matroska", "mkv"),
        ("x-mng", "mng"),
        ("x-mp4-fragmented", "mp4"),
        ("x-ms-asf", "asf"),
        ("x-ms-wmv", "wmv"),
        ("x-msvideo", "avi"),
        ("vnd.dlna.mpeg-tts", "mpeg"),
        ("dash+xml", "mpd"),
        ("f4m+xml", "f4m"),
        ("hds+xml", "f4m"),
        ("vnd.apple.mpegurl", "m3u8"),
        ("vnd.ms-sstr+xml", "ism"),
        ("x-mpegurl", "m3u8"),
        ("audio/mp4", "m4a"),
        ("audio/mpeg", "mp3"),
        ("audio/webm", "webm"),
        ("audio/x-matroska", "mka"),
        ("audio/x-mpegurl", "m3u"),
        ("aacp", "aac"),
        ("flac", "flac"),
        ("midi", "mid"),
        ("ogg", "ogg"),
        ("wav", "wav"),
        ("wave", "wav"),
        ("x-aac", "aac"),
        ("x-flac", "flac"),
        ("x-m4a", "m4a"),
        ("x-realaudio", "ra"),
        ("x-wav", "wav"),
        ("avif", "avif"),
        ("bmp", "bmp"),
        ("gif", "gif"),
        ("jpeg", "jpg"),
        ("png", "png"),
        ("svg+xml", "svg"),
        ("tiff", "tif"),
        ("vnd.wap.wbmp", "wbmp"),
        ("webp", "webp"),
        ("x-icon", "ico"),
        ("x-jng", "jng"),
        ("x-ms-bmp", "bmp"),
        ("filmstrip+json", "fs"),
        ("smptett+xml", "tt"),
        ("ttaf+xml", "dfxp"),
        ("ttml+xml", "ttml"),
        ("x-ms-sami", "sami"),
        ("x-subrip", "srt"),
        ("x-srt", "srt"),
        ("gzip", "gz"),
        ("json", "json"),
        ("xml", "xml"),
        ("zip", "zip"),
    ];
    let lookup = |key: &str| {
        MAP.iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.to_string())
    };
    lookup(&mimetype)
        .or_else(|| lookup(&subtype))
        .or_else(|| lookup(&after_plus))
        .or_else(|| (!subtype.is_empty()).then(|| subtype.replace('+', ".")))
}

/// The subtitle format an extension names; formats the downloader has no reader for
/// give `None`.
pub fn subtitle_format(ext: &str) -> Option<SubtitleFormat> {
    Some(match ext.trim().to_ascii_lowercase().as_str() {
        "vtt" | "webvtt" => SubtitleFormat::Vtt,
        "srt" => SubtitleFormat::Srt,
        "ttml" | "dfxp" | "tt" | "xml" | "ttaf" => SubtitleFormat::Ttml,
        "ass" | "ssa" => SubtitleFormat::Ass,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};

    fn exchange(method: &str, url: &str, status: u16, content_type: &str, body: &str) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: method.into(),
                url: url.into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status,
                url: url.into(),
                headers: vec![("content-type".into(), content_type.into())],
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    #[test]
    fn extensions_and_mime_types_are_read() {
        assert_eq!(extension_of("http://h/a/b.mp4?x=1").as_deref(), Some("mp4"));
        assert_eq!(
            extension_of("http://h/a/b.mp4/?download").as_deref(),
            Some("mp4")
        );
        assert_eq!(extension_of("http://h/a/b"), None);
        assert_eq!(extension_of("http://h/a.b/c"), None);
        assert_eq!(
            mime_extension("application/vnd.apple.mpegurl").as_deref(),
            Some("m3u8")
        );
        assert_eq!(
            mime_extension("video/mp4; codecs=avc1").as_deref(),
            Some("mp4")
        );
        assert_eq!(
            mime_extension("application/ttml+xml").as_deref(),
            Some("ttml")
        );
        assert_eq!(mime_extension("text/srt").as_deref(), Some("srt"));
        assert_eq!(
            mime_extension("text/x-foo+bar").as_deref(),
            Some("x-foo.bar")
        );
        assert_eq!(mime_extension(""), None);
        assert_eq!(subtitle_format("VTT"), Some(SubtitleFormat::Vtt));
        assert_eq!(subtitle_format("dfxp"), Some(SubtitleFormat::Ttml));
        assert_eq!(subtitle_format("scc"), None);
    }

    #[tokio::test]
    async fn every_kind_of_entry_is_expanded() {
        let smil = r#"<?xml version="1.0"?>
<smil xmlns="http://www.w3.org/2005/SMIL21/Language">
  <head><meta base="http://cdn.example.net"/></head>
  <body>
    <switch>
      <video src="2013/clip_1024x576_1500.mp4" streamer="rtmp://cp.example.net/ondemand" system-bitrate="1500000" width="1024" height="576" size="12345"/>
      <video src="http://cdn.example.net/clip_640.mp4" system-bitrate="800000" width="640" height="360" proto="http"/>
      <video src="http://cdn.example.net/clip_640.mp4" system-bitrate="800000"/>
      <video src="http://cdn.example.net/dead.mp4" system-bitrate="500000"/>
      <video src="http://cdn.example.net/master.m3u8" proto="m3u8"/>
      <video src="http://cdn.example.net/manifest.f4m"/>
      <video src="noext" system-bitrate="300000"/>
      <textstream src="http://cdn.example.net/caps.srt" lang="en"/>
      <textstream src="http://cdn.example.net/caps.xml" type="application/ttml+xml" systemLanguage="fr"/>
      <textstream src="http://cdn.example.net/caps.scc"/>
    </switch>
  </body>
</smil>"#;
        let mut fixture = Fixture::new("smil", None);
        fixture.exchanges.push(exchange(
            "GET",
            "http://cdn.example.net/clip_640.mp4",
            206,
            "video/mp4",
            "",
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "http://cdn.example.net/dead.mp4",
            404,
            "text/html",
            "",
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "http://cdn.example.net/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=1280x720\n720.m3u8\n",
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "http://cdn.example.net/720.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n0.ts\n#EXT-X-ENDLIST\n",
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "http://cdn.example.net/noext",
            200,
            "video/webm",
            "",
        ));
        let http = Http::replay(fixture);
        let smil_url = Url::parse("http://link.example.net/s/a/b?mbr=true").unwrap();
        let expanded = parse(&http, "smil", smil, &smil_url, "mp4:").await.unwrap();
        let ids: Vec<&str> = expanded
            .variants
            .iter()
            .map(|v| v.format_id.as_deref().unwrap())
            .collect();
        assert_eq!(
            ids,
            ["rtmp-1500", "http-800", "hls-1", "http-300"],
            "{ids:?}"
        );
        let rtmp = &expanded.variants[0];
        assert_eq!(rtmp.kind, VariantKind::Rtmp);
        assert_eq!(
            rtmp.url.as_str(),
            "rtmp://cp.example.net/ondemand/mp4:2013/clip_1024x576_1500.mp4"
        );
        assert_eq!(rtmp.size, Some(12345));
        assert_eq!(rtmp.height, Some(576));
        let file = &expanded.variants[1];
        assert_eq!(file.kind, VariantKind::File);
        assert_eq!(file.container, Some(Container::Mp4));
        assert_eq!(file.bitrate, Some(800000));
        assert_eq!(file.width, Some(640));
        assert_eq!(expanded.variants[2].kind, VariantKind::Hls);
        assert_eq!(expanded.variants[2].height, Some(720));
        assert_eq!(expanded.variants[3].container, Some(Container::Webm));
        assert_eq!(expanded.subtitles.len(), 2);
        assert_eq!(expanded.subtitles[0].format, SubtitleFormat::Srt);
        assert_eq!(expanded.subtitles[0].language, "en");
        assert_eq!(expanded.subtitles[1].format, SubtitleFormat::Ttml);
        assert_eq!(expanded.subtitles[1].language, "fr");
    }

    #[tokio::test]
    async fn relative_files_resolve_against_the_manifest() {
        let smil = r#"<smil><body><video src="clip.mp4" systemBitrate="400000"/></body></smil>"#;
        let http = Http::replay(Fixture::new("smil", None));
        let smil_url = Url::parse("http://link.example.net/s/a/b.smil").unwrap();
        let expanded = expand_text(&http, smil, &smil_url).await;
        assert_eq!(expanded.variants.len(), 1);
        assert_eq!(
            expanded.variants[0].url.as_str(),
            "http://link.example.net/s/a/b.smil/clip.mp4"
        );
        assert_eq!(expanded.variants[0].format_id.as_deref(), Some("http-400"));
        assert!(
            parse(&http, "smil", "not xml <", &smil_url, "")
                .await
                .is_err()
        );
    }

    async fn expand_text(http: &Http, smil: &str, smil_url: &Url) -> Expanded {
        parse(http, "smil", smil, smil_url, "").await.unwrap()
    }
}
