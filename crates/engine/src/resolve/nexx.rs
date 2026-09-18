//! Nexx (3Q) videos, through the arc catalogue and the player's session API, where the
//! Azure, free and 3Q CDN layouts each name their manifests and progressive files. An
//! `embed.nexx.cloud` player page names the video id its hash stands for.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport,
    SubtitleFormat, SubtitleTrack, Tag, Variant, clean_title, dash, fetch, fetch_ok, hls, ism,
    navigation_headers, path_extension, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "nexx";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A video by id, with the domain (customer) it belongs to when the link names one.
    Video {
        domain_id: Option<String>,
        video_id: String,
    },
    /// An `embed.nexx.cloud` player page: the domain, the stream type when the link
    /// names one (`video`, `audio`, …; the host no longer serves a page without one),
    /// and the media hash.
    Embed {
        domain_id: String,
        stream_type: Option<String>,
        hash: String,
    },
}

/// `/v3/{domain}/videos/byid/{id}` and `/v3.1/…` on the API hosts.
static RE_API: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/v3(?:\.\d)?/(\d+)/videos/byid/(\d+)").unwrap());
/// `/api/video/{id}` and `/api/video/{id}.json` on the arc host.
static RE_ARC: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/api/video/(\d+)").unwrap());
/// `/{domain}/{hash}` and `/{domain}/{stream type}/{hash}` on the embed hosts.
static RE_EMBED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(\d+)/(?:([a-z]+)/)?([^/?#&]+)").unwrap());
/// `nexx:{domain}:{id}` and `nexx:{id}`.
static RE_SCHEME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:(\d+):)?(\d+)$").unwrap());
/// The player script tag naming the page's domain.
static RE_DOMAIN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<script\b[^>]+\bsrc=["'](?:https?:)?//(?:require|arc)\.nexx(?:\.cloud|cdn\.com)/(?:sdk/)?(\d+)"#).unwrap()
});
/// The handler a page's JavaScript integration sets up its players in.
static RE_READY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)onPLAYReady").unwrap());
/// One player call in that handler, `_play.init(…)` or `_play.control.addPlayer(…)`,
/// whose second argument is the video id.
static RE_PLAY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)_play\.(?:init|(?:control\.)?addPlayer)\s*\([^)]*?,\s*["']?(\d+)"#).unwrap()
});
/// The player call of an `embed.nexx.cloud` page,
/// `_play.control.addPlayer('nxpembedplayer', '{media id}', '{stream type}', …)`: the
/// id is `0` when the hash names no media and empty when the host set up no player.
static RE_EMBED_PLAYER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"_play\.control\.addPlayer\s*\(\s*['"][^'"]*['"]\s*,\s*['"]([^'"]*)['"]"#).unwrap()
});
/// Player iframes on other sites' pages.
static RE_IFRAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<iframe[^>]+\bsrc=(?:"((?:https?:)?//embed\.nexx(?:\.cloud|cdn\.com)/\d+/[^"]+)"|'((?:https?:)?//embed\.nexx(?:\.cloud|cdn\.com)/\d+/[^']+)')"#).unwrap()
});

pub fn parse_link(url: &Url) -> Option<Link> {
    match url.scheme() {
        "nexx" => {
            let caps = RE_SCHEME.captures(url.path())?;
            return Some(Link::Video {
                domain_id: caps.get(1).map(|m| m.as_str().to_string()),
                video_id: caps[2].to_string(),
            });
        }
        "http" | "https" => {}
        _ => return None,
    }
    let host = url.host_str()?.to_ascii_lowercase();
    match host.as_str() {
        "api.nexx.cloud" | "api.nexxcdn.com" => RE_API.captures(url.path()).map(|c| Link::Video {
            domain_id: Some(c[1].to_string()),
            video_id: c[2].to_string(),
        }),
        "arc.nexx.cloud" => RE_ARC.captures(url.path()).map(|c| Link::Video {
            domain_id: None,
            video_id: c[1].to_string(),
        }),
        "embed.nexx.cloud" | "embed.nexxcdn.com" => {
            RE_EMBED.captures(url.path()).map(|c| Link::Embed {
                domain_id: c[1].to_string(),
                stream_type: c.get(2).map(|m| m.as_str().to_string()),
                hash: c[3].to_string(),
            })
        }
        _ => None,
    }
}

/// The `nexx:{domain}:{id}` link this resolver reads, for resolvers that hand a nexx
/// video on (funk hands over `nexx:741:{id}`).
pub fn video_link(domain_id: Option<&str>, video_id: &str) -> Url {
    let text = match domain_id {
        Some(domain) => format!("nexx:{domain}:{video_id}"),
        None => format!("nexx:{video_id}"),
    };
    Url::parse(&text).expect("a nexx link is a valid URL")
}

/// The player page the embed host serves for a hash: the link's stream type, or `video`
/// for the links made before the host asked for one.
pub fn embed_page_link(domain_id: &str, stream_type: Option<&str>, hash: &str) -> Url {
    let stream_type = stream_type.unwrap_or("video");
    Url::parse(&format!(
        "https://embed.nexx.cloud/{domain_id}/{stream_type}/{hash}"
    ))
    .expect("an embed page link is a valid URL")
}

/// The media id an `embed.nexx.cloud` page's player call names, as written: digits for a
/// video, `0` for a hash the host does not know, empty when it set up no player.
pub fn embed_media_id(html: &str) -> Option<String> {
    util::search(&RE_EMBED_PLAYER, html)
}

/// The API links of the videos a page plays through the JavaScript integration: the
/// domain from the player script tag, the ids from every player call once the page's
/// `onPLAYReady` handler starts.
pub fn player_urls(html: &str) -> Vec<Url> {
    let Some(domain_id) = util::search(&RE_DOMAIN, html) else {
        return Vec::new();
    };
    let Some(ready) = RE_READY.find(html) else {
        return Vec::new();
    };
    RE_PLAY
        .captures_iter(&html[ready.end()..])
        .filter_map(|caps| {
            Url::parse(&format!(
                "https://api.nexx.cloud/v3/{domain_id}/videos/byid/{}",
                &caps[1]
            ))
            .ok()
        })
        .collect()
}

/// The `embed.nexx.cloud` player iframes on a page.
pub fn embed_urls(html: &str) -> Vec<Url> {
    RE_IFRAME
        .captures_iter(html)
        .filter_map(|caps| {
            let raw = caps.get(1).or_else(|| caps.get(2))?.as_str();
            let text = if raw.starts_with("//") {
                format!("https:{raw}")
            } else {
                raw.to_string()
            };
            Url::parse(&text).ok()
        })
        .collect()
}

/// The domain secret a request token is hashed with: the session's domain token with
/// as many leading characters cut as the device id's first digit says and as many
/// trailing ones as its last digit says.
pub fn domain_secret(domain_token: &str, device_id: &str) -> String {
    let digit = |c: Option<char>| c.and_then(|c| c.to_digit(10)).unwrap_or(0) as usize;
    let head = digit(device_id.chars().next());
    let tail = digit(device_id.chars().last());
    let chars: Vec<char> = domain_token.chars().collect();
    let cut = chars.get(head..).unwrap_or(&[]);
    let keep = cut.len().saturating_sub(tail);
    cut[..keep].iter().collect()
}

/// A device id as the player's `getDeviceID` makes one.
fn device_id() -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    format!(
        "{}:{}:{}{}",
        rng.random_range(1..=4),
        jiff::Timestamp::now().as_second(),
        rng.random_range(10000..=99999),
        rng.random_range(1..=9)
    )
}

/// Python's `template % values` for `%s` placeholders.
fn fill(template: &str, values: &[&str]) -> String {
    let mut out = String::new();
    let mut rest = template;
    for value in values {
        match rest.split_once("%s") {
            Some((head, tail)) => {
                out.push_str(head);
                out.push_str(value);
                rest = tail;
            }
            None => break,
        }
    }
    out.push_str(rest);
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ManifestKind {
    Hls,
    Dash,
    Ism,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Manifest {
    url: String,
    kind: ManifestKind,
    format_id: String,
}

/// The manifests and progressive files a CDN layout names for a video.
#[derive(Debug, Default)]
struct Streams {
    manifests: Vec<Manifest>,
    files: Vec<Variant>,
}

fn required<'a>(video: &'a Value, path: &[&str], origin: &Url) -> Result<&'a Value, ResolveError> {
    let mut value = video;
    for key in path {
        value = &value[*key];
    }
    if value.is_null() {
        return Err(ResolveError::malformed(
            origin,
            format!("the video's stream data has no {}", path.join(".")),
        ));
    }
    Ok(value)
}

fn required_text(video: &Value, path: &[&str], origin: &Url) -> Result<String, ResolveError> {
    util::text(required(video, path, origin)?).ok_or_else(|| {
        ResolveError::malformed(
            origin,
            format!("the video's stream data has no {}", path.join(".")),
        )
    })
}

/// A progressive file with the bitrate (kbit/s) and `WxH` text the CDN layout names.
fn file_variant(
    url: &str,
    format_id: String,
    tbr: Option<i64>,
    size: Option<&str>,
) -> Option<Variant> {
    let url = Url::parse(url).ok()?;
    let container = path_extension(&url)
        .and_then(|ext| Container::from_extension(&ext))
        .unwrap_or(Container::Mp4);
    let mut variant = Variant::file(url);
    if container == Container::Mp4 {
        variant.video = Some(VideoCodec::H264);
        variant.audio = Some(AudioCodec::Aac);
    }
    variant.container = Some(container);
    variant.bitrate = tbr.filter(|t| *t > 0).map(|t| t as u64 * 1000);
    if let Some(size) = size {
        let (width, height, _) = util::parse_resolution(size);
        variant.width = width;
        variant.height = height;
    }
    variant.label = variant.height.map(|h| format!("{h}p"));
    variant.format_id = Some(format_id);
    Some(variant)
}

/// The CDN shield host a stream data names for `kind` (`""` or `"Prog"`), preferring
/// the plain one over the TLS one as the player does.
fn shield_host(stream: &Value, kind: &str) -> Option<String> {
    ["", "s"].iter().find_map(|secure| {
        let key = format!("cdnShield{kind}HTTP{}", secure.to_ascii_uppercase());
        stream[&key]
            .as_str()
            .filter(|host| !host.is_empty())
            .map(|host| format!("http{secure}://{host}"))
    })
}

/// Azure Media Services layout: Smooth Streaming manifests in three flavours behind an
/// Akamai host numbered after the account, and `_src_{WxH}_{kbps}.mp4` files.
fn azure_streams(video: &Value, video_id: &str, origin: &Url) -> Result<Streams, ResolveError> {
    let stream = &video["streamdata"];
    let locator = required_text(video, &["streamdata", "azureLocator"], origin)?;
    let account = required_text(video, &["streamdata", "azureAccount"], origin)?;
    let akamai = |static_files: bool| -> Result<String, ResolveError> {
        let prefix = match (account.contains("fb"), static_files) {
            (true, true) => "df",
            (true, false) => "f",
            (false, true) => "d",
            (false, false) => "p",
        };
        let number: u32 = account
            .replace("nexxplayplus", "")
            .replace("nexxplayfb", "")
            .parse()
            .map_err(|_| {
                ResolveError::malformed(
                    origin,
                    format!("the Azure account {account} is not numbered"),
                )
            })?;
        Ok(format!("http://nx-{prefix}{number:02}.akamaized.net/"))
    };
    let stream_base = match shield_host(stream, "") {
        Some(host) => host,
        None => akamai(false)?,
    };
    let language = video["general"]["language_raw"].as_str().unwrap_or("");
    let manifest = format!(
        "{stream_base}{locator}/{video_id}_src{}.ism/Manifest",
        if language.contains(',') {
            "_manifest"
        } else {
            ""
        }
    );
    let token = video["protectiondata"]["token"]
        .as_str()
        .filter(|t| !t.is_empty())
        .map(|t| format!("?hdnts={t}"))
        .unwrap_or_default();
    let mut streams = Streams {
        manifests: vec![
            Manifest {
                url: format!("{manifest}(format=m3u8-aapl){token}"),
                kind: ManifestKind::Hls,
                format_id: "azure-hls".into(),
            },
            Manifest {
                url: format!("{manifest}(format=mpd-time-csf){token}"),
                kind: ManifestKind::Dash,
                format_id: "azure-dash".into(),
            },
            Manifest {
                url: format!("{manifest}{token}"),
                kind: ManifestKind::Ism,
                format_id: "azure-mss".into(),
            },
        ],
        files: Vec::new(),
    };
    let progressive_base = match shield_host(stream, "Prog") {
        Some(host) => host,
        None => akamai(true)?,
    };
    for entry in stream["azureFileDistribution"]
        .as_str()
        .unwrap_or("")
        .split(',')
    {
        let parts: Vec<&str> = entry.split(':').collect();
        if parts.len() != 2 {
            continue;
        }
        let Some(tbr) = parts[0].trim().parse::<i64>().ok().filter(|t| *t > 0) else {
            continue;
        };
        let url = format!(
            "{progressive_base}{locator}/{video_id}_src_{}_{tbr}.mp4",
            parts[1]
        );
        streams.files.extend(file_variant(
            &url,
            format!("azure-http-{tbr}"),
            Some(tbr),
            Some(parts[1]),
        ));
    }
    Ok(streams)
}

/// Free CDN layout: the file path is built from the original domain, an optional folder
/// hierarchy taken from the reversed id, the id and the video hash; Akamai (`ak`) serves
/// an HLS master over a `.csmil` bundle, CenturyLink (`ce`) an `asset.ism` in DASH and
/// HLS beside the progressive files.
fn free_streams(video: &Value, video_id: &str, origin: &Url) -> Result<Streams, ResolveError> {
    let stream = &video["streamdata"];
    let hash = required_text(video, &["general", "hash"], origin)?;
    let mut path = required_text(video, &["streamdata", "originalDomain"], origin)?;
    if util::int(&stream["applyFolderHierarchy"]) == Some(1) {
        let number: u64 = video_id.parse().map_err(|_| {
            ResolveError::malformed(origin, format!("the video id {video_id} is not a number"))
        })?;
        let reversed: Vec<char> = format!("{number:04}").chars().rev().collect();
        let first: String = reversed[0..2].iter().collect();
        let second: String = reversed[2..4].iter().collect();
        path.push_str(&format!("/{first}/{second}"));
    }
    path.push_str(&format!("/{video_id}/{hash}_"));
    let mut template = format!("http://%s{path}");
    let distribution = required_text(video, &["streamdata", "azureFileDistribution"], origin)?;
    let entries: Vec<Vec<&str>> = distribution
        .split(',')
        .map(|entry| entry.split(':').collect())
        .filter(|parts: &Vec<&str>| parts.len() >= 2)
        .collect();
    let azure_structure = util::int(&stream["applyAzureStructure"]) == Some(1);
    let suffix = |tbr: i64| {
        if azure_structure {
            format!("_{tbr}")
        } else {
            String::new()
        }
    };
    let provider = required_text(video, &["streamdata", "cdnProvider"], origin)?;
    let mut streams = Streams::default();
    match provider.as_str() {
        "ak" => {
            template.push(',');
            for parts in &entries {
                let tbr = parts[0].trim().parse::<i64>().unwrap_or(0);
                template.push_str(parts[1]);
                template.push_str(&suffix(tbr));
                template.push(',');
            }
            template.push_str(".mp4.csmil/master.%s");
        }
        "ce" => {
            let mut segments: Vec<&str> = template.split('/').collect();
            let file_prefix = segments.pop().unwrap_or_default().to_string();
            let base_template = segments.join("/");
            let http_path = required_text(video, &["streamdata", "cdnPathHTTP"], origin)?;
            let http_base = fill(&base_template, &[&http_path]);
            let mut manifest = base_template.clone();
            manifest.push_str("/asset.ism/manifest.%s?dcp_ver=aos4&videostream=");
            let mut last_file = None;
            for parts in &entries {
                let tbr = parts[0].trim().parse::<i64>().unwrap_or(0);
                let filename = format!("{file_prefix}{}{}.mp4", parts[1], suffix(tbr));
                streams.files.extend(file_variant(
                    &format!("{http_base}/{filename}"),
                    format!("free-http-{tbr}"),
                    Some(tbr),
                    Some(parts[1]),
                ));
                manifest.push_str(&format!("{filename}:{},", tbr * 1000));
                last_file = Some(filename);
            }
            let audio = last_file.ok_or_else(|| {
                ResolveError::malformed(origin, "the video's file distribution is empty")
            })?;
            manifest.pop();
            manifest.push_str(&format!("&audiostream={audio}"));
            template = manifest;
            let dash_path = required_text(video, &["streamdata", "cdnPathDASH"], origin)?;
            streams.manifests.push(Manifest {
                url: fill(&template, &[&dash_path, "mpd"]),
                kind: ManifestKind::Dash,
                format_id: "free-dash".into(),
            });
        }
        other => {
            return Err(ResolveError::malformed(
                origin,
                format!("unknown free CDN provider {other}"),
            ));
        }
    }
    let hls_path = required_text(video, &["streamdata", "cdnPathHLS"], origin)?;
    streams.manifests.push(Manifest {
        url: fill(&template, &[&hls_path, "m3u8"]),
        kind: ManifestKind::Hls,
        format_id: "free-hls".into(),
    });
    Ok(streams)
}

/// 3Q SDN layout: HLS and DASH manifests behind the streaming cache, WebM uploads and
/// MP4 files behind the progressive cache, both optionally keyed by the protection key.
fn threeq_streams(video: &Value, origin: &Url) -> Result<Streams, ResolveError> {
    let stream = &video["streamdata"];
    let account = required_text(video, &["streamdata", "qAccount"], origin)?;
    let prefix = required_text(video, &["streamdata", "qPrefix"], origin)?;
    let locator = required_text(video, &["streamdata", "qLocator"], origin)?;
    let hash = required_text(video, &["streamdata", "qHash"], origin)?;
    let key = video["protectiondata"]["key"]
        .as_str()
        .filter(|k| !k.is_empty())
        .map(|k| format!("s/{k}/"))
        .unwrap_or_default();
    let cache = |kind: &str| {
        shield_host(stream, kind).unwrap_or_else(|| {
            let name = if kind.eq_ignore_ascii_case("prog") {
                "prog"
            } else {
                "streaming"
            };
            format!("http://sdn-global-{name}-cache.3qsdn.com/{key}")
        })
    };
    let stream_base = cache("");
    let hevc_hash = stream["qHEVCHash"]
        .as_str()
        .filter(|h| !h.is_empty())
        .unwrap_or(&hash);
    let mut streams = Streams {
        manifests: vec![
            Manifest {
                url: format!(
                    "{stream_base}{account}/files/{prefix}/{locator}/{account}-{hevc_hash}.ism/manifest.m3u8"
                ),
                kind: ManifestKind::Hls,
                format_id: "3q-hls".into(),
            },
            Manifest {
                url: format!(
                    "{stream_base}{account}/files/{prefix}/{locator}/{account}-{hash}.ism/manifest.mpd"
                ),
                kind: ManifestKind::Dash,
                format_id: "3q-dash".into(),
            },
        ],
        files: Vec::new(),
    };
    let progressive_base = cache("Prog");
    for entry in stream["qReferences"].as_str().unwrap_or("").split(',') {
        let parts: Vec<&str> = entry.split(':').collect();
        if parts.len() != 3 {
            continue;
        }
        let tbr = parts[1].trim().parse::<i64>().ok().map(|bits| bits / 1000);
        let format_id = match tbr.filter(|t| *t > 0) {
            Some(tbr) => format!("3q-{}-{tbr}", parts[0]),
            None => format!("3q-{}", parts[0]),
        };
        streams.files.extend(file_variant(
            &format!(
                "{progressive_base}{account}/uploads/{account}-{}.webm",
                parts[2]
            ),
            format_id,
            tbr,
            None,
        ));
    }
    for entry in stream["azureFileDistribution"]
        .as_str()
        .unwrap_or("")
        .split(',')
    {
        let parts: Vec<&str> = entry.split(':').collect();
        if parts.len() != 3 {
            continue;
        }
        let tbr = parts[0].trim().parse::<i64>().ok();
        let format_id = match tbr.filter(|t| *t > 0) {
            Some(tbr) => format!("3q-http-{tbr}"),
            None => "3q-http".to_string(),
        };
        streams.files.extend(file_variant(
            &format!(
                "{progressive_base}{account}/files/{prefix}/{locator}/{}.mp4",
                parts[2]
            ),
            format_id,
            tbr,
            Some(parts[1]),
        ));
    }
    Ok(streams)
}

/// Expands every manifest the layout named, keeping the progressive files; a manifest
/// the CDN does not serve is skipped, and the first failure is the error when nothing
/// plays. Every variant of a manifest carries the manifest's id, then its own: the DASH
/// representation's id, the Smooth Streaming level's, or the HLS rendition's bitrate.
async fn expand_streams(
    http: &Http,
    platform: &str,
    streams: Streams,
    origin: &Url,
) -> Result<Vec<Variant>, ResolveError> {
    let mut variants = streams.files;
    let mut failure = None;
    for manifest in streams.manifests {
        let Ok(url) = Url::parse(&manifest.url) else {
            continue;
        };
        let expanded = match manifest.kind {
            ManifestKind::Hls => hls::expand(http, &url, platform, BROWSER_UA, &[])
                .await
                .map(|e| e.variants),
            ManifestKind::Dash => dash::expand(http, &url, platform, BROWSER_UA, &[])
                .await
                .map(|e| e.variants),
            ManifestKind::Ism => ism::expand(http, &url, platform, BROWSER_UA, &[])
                .await
                .map(|e| e.variants),
        };
        match expanded {
            Ok(found) => {
                for mut variant in found {
                    variant.format_id = Some(match (variant.format_id.take(), variant.bitrate) {
                        (Some(id), _) => format!("{}-{id}", manifest.format_id),
                        (None, Some(bitrate)) => {
                            format!("{}-{}", manifest.format_id, bitrate / 1000)
                        }
                        (None, None) => manifest.format_id.clone(),
                    });
                    variants.push(variant);
                }
            }
            Err(error) => {
                if failure.is_none() {
                    failure = Some(error);
                }
            }
        }
    }
    if variants.is_empty() {
        return Err(failure.unwrap_or_else(|| {
            ResolveError::unavailable(origin, "the CDN names no playable stream")
        }));
    }
    Ok(variants)
}

/// The video of a catalogue answer: the object itself, or in a list the entry whose
/// `general.ID` is the id asked for.
fn find_video(result: &Value, video_id: &str) -> Option<Value> {
    match result {
        Value::Object(_) => Some(result.clone()),
        Value::Array(items) => {
            let wanted: i64 = video_id.parse().ok()?;
            items
                .iter()
                .find(|item| util::int(&item["general"]["ID"]) == Some(wanted))
                .cloned()
        }
        _ => None,
    }
}

/// The video as the arc catalogue lists it, when it does.
async fn arc_video(http: &Http, platform: &str, video_id: &str, origin: &Url) -> Option<Value> {
    let url = Url::parse(&format!("https://arc.nexx.cloud/api/video/{video_id}.json")).ok()?;
    let fetched = fetch(http, &url, platform, BROWSER_UA, &[], MAX_PAGE)
        .await
        .ok()?;
    if !fetched.status.is_success() {
        return None;
    }
    let answer = fetched.json(origin).ok()?;
    let result = &answer["result"];
    let empty = match result {
        Value::Object(map) => map.is_empty(),
        Value::Array(items) => items.is_empty(),
        _ => true,
    };
    if empty {
        return None;
    }
    find_video(result, video_id)
}

/// A call of the player API: the `result` of the answer, or what its metadata says went
/// wrong.
async fn call_api(
    http: &Http,
    platform: &str,
    domain_id: &str,
    path: &str,
    fields: &[(&str, &str)],
    headers: &[(&str, &str)],
    origin: &Url,
) -> Result<Value, ResolveError> {
    let url = Url::parse(&format!("https://api.nexx.cloud/v3/{domain_id}/{path}"))
        .map_err(|e| ResolveError::malformed(origin, e.to_string()))?;
    let mut request = http
        .post(url.clone())
        .platform(platform)
        .user_agent(BROWSER_UA)
        .form(fields)
        .header(
            "content-type",
            "application/x-www-form-urlencoded; charset=UTF-8",
        );
    for (name, value) in headers {
        request = request.header(name, value);
    }
    let response = request.send().await?;
    if let Some(error) = status_error(response.status, origin) {
        return Err(error);
    }
    let answer: Value = response
        .json(MAX_PAGE)
        .await
        .map_err(|e| ResolveError::malformed(origin, format!("{path} JSON: {e}")))?;
    let status = util::int(&answer["metadata"]["status"]).unwrap_or(200);
    if !(200..300).contains(&status) {
        let hint = answer["metadata"]["errorhint"]
            .as_str()
            .or_else(|| answer["metadata"]["notice"].as_str())
            .map(str::to_string)
            .unwrap_or_else(|| format!("status {status}"));
        return Err(ResolveError::unavailable(
            origin,
            format!("Nexx said: {hint}"),
        ));
    }
    Ok(answer["result"].clone())
}

/// The video through the player's session API: a session is opened for a fresh device,
/// and its domain token signs the `byid` request.
async fn session_video(
    http: &Http,
    platform: &str,
    domain_id: &str,
    video_id: &str,
    origin: &Url,
) -> Result<Value, ResolveError> {
    let device = device_id();
    let session = call_api(
        http,
        platform,
        domain_id,
        "session/init",
        &[
            ("nxp_devh", device.as_str()),
            ("nxp_userh", ""),
            ("precid", "0"),
            ("playlicense", "0"),
            ("screenx", "1920"),
            ("screeny", "1080"),
            ("playerversion", "6.0.00"),
            ("gateway", "html5"),
            ("adGateway", ""),
            ("explicitlanguage", "en-US"),
            ("addTextTemplates", "1"),
            ("addDomainData", "1"),
            ("addAdModel", "1"),
        ],
        &[("x-request-enable-auth-fallback", "1")],
        origin,
    )
    .await?;
    let cid = util::text(&session["general"]["cid"])
        .ok_or_else(|| ResolveError::malformed(origin, "the session names no cid"))?;
    let domain_token = session["device"]["domaintoken"]
        .as_str()
        .ok_or_else(|| ResolveError::malformed(origin, "the session names no domain token"))?;
    let secret = domain_secret(domain_token, &device);
    let request_token = util::md5_hex(format!("byid{domain_id}{secret}").as_bytes());
    let result = call_api(
        http,
        platform,
        domain_id,
        &format!("videos/byid/{video_id}"),
        &[
            (
                "additionalfields",
                "language,channel,format,licenseby,slug,fileversion,episode,season",
            ),
            ("addInteractionOptions", "1"),
            ("addStatusDetails", "1"),
            ("addStreamDetails", "1"),
            ("addFeatures", "1"),
            ("addCaptions", "vtt"),
            ("addScenes", "1"),
            ("addChapters", "1"),
            ("addHotSpots", "1"),
            ("addConnectedMedia", "persons"),
            ("addBumpers", "1"),
        ],
        &[
            ("x-request-cid", cid.as_str()),
            ("x-request-token", request_token.as_str()),
        ],
        origin,
    )
    .await?;
    find_video(&result, video_id).ok_or_else(|| ResolveError::NotFound(origin.clone()))
}

fn subtitle_format(name: &str) -> Option<SubtitleFormat> {
    match name.to_ascii_lowercase().as_str() {
        "vtt" | "webvtt" => Some(SubtitleFormat::Vtt),
        "srt" => Some(SubtitleFormat::Srt),
        "ttml" | "dfxp" | "xml" => Some(SubtitleFormat::Ttml),
        "ass" | "ssa" => Some(SubtitleFormat::Ass),
        _ => None,
    }
}

/// The caption files a video's `captiondata` links to, in the format each names or
/// that its file extension does.
pub fn subtitles_of(video: &Value) -> Vec<SubtitleTrack> {
    let mut tracks = Vec::new();
    for caption in video["captiondata"].as_array().into_iter().flatten() {
        let Some(url) = caption["url"].as_str().and_then(|u| Url::parse(u).ok()) else {
            continue;
        };
        let format = caption["format"]
            .as_str()
            .and_then(subtitle_format)
            .or_else(|| path_extension(&url).and_then(|ext| subtitle_format(&ext)));
        let Some(format) = format else {
            continue;
        };
        tracks.push(SubtitleTrack {
            url,
            language: caption["language"]
                .as_str()
                .filter(|l| !l.is_empty())
                .unwrap_or("en")
                .to_string(),
            name: caption["language_long"]
                .as_str()
                .or_else(|| caption["title"].as_str())
                .and_then(clean_title),
            format,
            auto: false,
            headers: Vec::new(),
        });
    }
    tracks
}

/// A video by id, as `platform` (the calling resolver's cookie jar and proxy apply):
/// the arc catalogue first, the player's session API for videos the catalogue does not
/// list, which needs the domain id.
pub async fn resolve_video(
    http: &Http,
    platform: &'static str,
    domain_id: Option<&str>,
    video_id: &str,
    origin: &Url,
) -> Result<Resolved, ResolveError> {
    let video = match arc_video(http, platform, video_id, origin).await {
        Some(video) => video,
        None => {
            let domain_id = domain_id.ok_or_else(|| {
                ResolveError::unavailable(
                    origin,
                    "the arc catalogue does not list the video and the link names no nexx domain",
                )
            })?;
            session_video(http, platform, domain_id, video_id, origin).await?
        }
    };
    let general = &video["general"];
    let title = general["title"]
        .as_str()
        .and_then(clean_title)
        .ok_or_else(|| ResolveError::malformed(origin, "the video has no title"))?;
    let cdn = video["streamdata"]["cdnType"].as_str().unwrap_or("");
    let streams = match cdn {
        "azure" => azure_streams(&video, video_id, origin)?,
        "free" => free_streams(&video, video_id, origin)?,
        "3q" => threeq_streams(&video, origin)?,
        other => {
            return Err(ResolveError::unavailable(
                origin,
                format!("{other} formats are currently not supported"),
            ));
        }
    };
    let variants = expand_streams(http, platform, streams, origin).await?;
    let mut resolved = Resolved::new(platform);
    resolved.id = Some(video_id.to_string());
    resolved.title = Some(title);
    resolved.description = general["description"].as_str().and_then(clean_title);
    resolved.uploader = general["studio"]
        .as_str()
        .filter(|s| !s.is_empty())
        .or_else(|| general["studio_adref"].as_str().filter(|s| !s.is_empty()))
        .and_then(clean_title);
    resolved.thumbnail = video["imagedata"]["thumb"]
        .as_str()
        .and_then(|t| Url::parse(t).ok());
    resolved.duration = util::seconds(&general["runtime"])
        .or_else(|| util::text(&general["runtime"]).and_then(|t| util::parse_duration(&t)))
        .filter(|d| *d > Duration::ZERO)
        .or_else(|| variants.iter().find_map(|v| v.duration));
    resolved.uploaded_at = util::epoch(&general["uploaded"]);
    resolved.live = variants.iter().any(|v| v.live);
    resolved.subtitles = subtitles_of(&video);
    resolved.variants = variants;
    Ok(resolved)
}

pub struct NexxResolver {
    http: Http,
}

impl NexxResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for NexxResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Nexx",
            hosts: &["nexx.cloud", "nexxcdn.com"],
            features: &["videos", "embeds"],
            formats: &["mp4", "webm", "hls", "dash", "ism"],
            media: &[MediaKind::Video],
            tags: &[Tag::Players],
            session: SessionSupport::None,
            examples: &[
                "https://embed.nexx.cloud/741/video/71269984GMIR7QA",
                "https://api.nexx.cloud/v3.1/741/videos/byid/1701834",
                "nexx:741:1269984",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Video {
                domain_id,
                video_id,
            } => {
                let resolved =
                    resolve_video(&self.http, PLATFORM, domain_id.as_deref(), &video_id, url)
                        .await?;
                Ok(Resolution::from(resolved))
            }
            Link::Embed {
                domain_id,
                stream_type,
                hash,
            } => {
                let page = embed_page_link(&domain_id, stream_type.as_deref(), &hash);
                let fetched = fetch_ok(
                    &self.http,
                    &page,
                    PLATFORM,
                    BROWSER_UA,
                    &navigation_headers(),
                    MAX_PAGE,
                )
                .await?;
                let media_id = embed_media_id(&fetched.text()).ok_or_else(|| {
                    ResolveError::malformed(url, "the embed page has no player call")
                })?;
                match media_id.as_str() {
                    "" => Err(ResolveError::unavailable(
                        url,
                        "the embed host set up no player for the link",
                    )),
                    "0" => Err(ResolveError::NotFound(url.clone())),
                    id if id.bytes().all(|b| b.is_ascii_digit()) => Err(ResolveError::Redirect(
                        Url::parse(&format!(
                            "https://api.nexx.cloud/v3/{domain_id}/videos/byid/{id}"
                        ))
                        .expect("an API link is a valid URL"),
                    )),
                    other => Err(ResolveError::malformed(
                        url,
                        format!("the embed page's player names media {other:?}"),
                    )),
                }
            }
        }
    }

    fn embeds_in(&self, page: &Page) -> Vec<Url> {
        let mut found = player_urls(page.html());
        found.extend(embed_urls(page.html()));
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};
    use serde_json::json;

    fn exchange(
        method: &str,
        url: &str,
        status: u16,
        content_type: &str,
        body: String,
    ) -> Exchange {
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
                body: RecordedBody::Text(body),
                truncated: false,
            },
        }
    }

    fn get(url: &str, status: u16, content_type: &str, body: String) -> Exchange {
        exchange("GET", url, status, content_type, body)
    }

    fn post(url: &str, status: u16, body: String) -> Exchange {
        exchange("POST", url, status, "application/json", body)
    }

    const MASTER: &str = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720,CODECS=\"avc1.64001f,mp4a.40.2\"\nhttps://stream.test/720p/index.m3u8\n";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n0.ts\n#EXTINF:5.0,\n1.ts\n#EXT-X-ENDLIST\n";
    const MPD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10M7S" profiles="urn:mpeg:dash:profile:isoff-on-demand:2011">
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <Representation id="v720" bandwidth="1500000" width="1280" height="720" codecs="avc1.64001f"/>
    </AdaptationSet>
    <AdaptationSet mimeType="audio/mp4" contentType="audio" lang="de">
      <Representation id="a128" bandwidth="128000" codecs="mp4a.40.2"/>
    </AdaptationSet>
  </Period>
</MPD>"#;

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let video = |domain: Option<&str>, id: &str| {
            Some(Link::Video {
                domain_id: domain.map(str::to_string),
                video_id: id.to_string(),
            })
        };
        assert_eq!(
            link("https://api.nexx.cloud/v3/748/videos/byid/128907"),
            video(Some("748"), "128907")
        );
        assert_eq!(
            link("https://api.nexx.cloud/v3.1/741/videos/byid/1701834"),
            video(Some("741"), "1701834")
        );
        assert_eq!(
            link("https://api.nexxcdn.com/v3/748/videos/byid/128907"),
            video(Some("748"), "128907")
        );
        assert_eq!(link("nexx:741:1269984"), video(Some("741"), "1269984"));
        assert_eq!(link("nexx:748:128907"), video(Some("748"), "128907"));
        assert_eq!(link("nexx:128907"), video(None, "128907"));
        assert_eq!(
            link("https://arc.nexx.cloud/api/video/128907.json"),
            video(None, "128907")
        );
        let embed = |domain: &str, stream_type: Option<&str>, hash: &str| {
            Some(Link::Embed {
                domain_id: domain.to_string(),
                stream_type: stream_type.map(str::to_string),
                hash: hash.to_string(),
            })
        };
        assert_eq!(
            link("http://embed.nexx.cloud/748/KC1614647Z27Y7T?autoplay=1"),
            embed("748", None, "KC1614647Z27Y7T")
        );
        assert_eq!(
            link("https://embed.nexx.cloud/11888/video/DSRTO7UVOX06S7"),
            embed("11888", Some("video"), "DSRTO7UVOX06S7")
        );
        assert_eq!(
            link("https://embed.nexxcdn.com/741/audio/71269984GMIR7QA"),
            embed("741", Some("audio"), "71269984GMIR7QA")
        );
        assert_eq!(link("https://embed.nexx.cloud/741/"), None);
        assert_eq!(
            embed_page_link("748", None, "KC1614647Z27Y7T").as_str(),
            "https://embed.nexx.cloud/748/video/KC1614647Z27Y7T"
        );
        assert_eq!(
            embed_page_link("741", Some("audio"), "H").as_str(),
            "https://embed.nexx.cloud/741/audio/H"
        );
        assert_eq!(link("https://api.nexx.cloud/v3/748/videos/"), None);
        assert_eq!(link("https://www.nexx.cloud/"), None);
        assert_eq!(link("nexx:abc"), None);
        assert_eq!(
            video_link(Some("741"), "1"),
            Url::parse("nexx:741:1").unwrap()
        );
        assert_eq!(video_link(None, "1"), Url::parse("nexx:1").unwrap());
    }

    #[test]
    fn secrets_and_templates_follow_the_player() {
        assert_eq!(
            domain_secret("abcdefghij", "2:1700000000:123451"),
            "cdefghi"
        );
        assert_eq!(domain_secret("abc", "4:1:9"), "");
        assert_eq!(fill("http://%s/x/%s", &["a", "b"]), "http://a/x/b");
        assert_eq!(fill("no holes", &["a"]), "no holes");
    }

    #[test]
    fn embeds_are_found_in_pages() {
        let html = r#"<script src="//require.nexx.cloud/748"></script>
            <script>window.onPLAYReady = function() { _play.init("player", 128907, {}); _play.control.addPlayer("p2", '161464'); }</script>
            <iframe src="//embed.nexx.cloud/748/KC1614647Z27Y7T?autoplay=1"></iframe>
            <iframe src='https://embed.nexxcdn.com/11888/video/DSRTO7UVOX06S7'></iframe>"#;
        let players: Vec<String> = player_urls(html).iter().map(|u| u.to_string()).collect();
        assert_eq!(
            players,
            vec![
                "https://api.nexx.cloud/v3/748/videos/byid/128907",
                "https://api.nexx.cloud/v3/748/videos/byid/161464"
            ]
        );
        let embeds: Vec<String> = embed_urls(html).iter().map(|u| u.to_string()).collect();
        assert_eq!(
            embeds,
            vec![
                "https://embed.nexx.cloud/748/KC1614647Z27Y7T?autoplay=1",
                "https://embed.nexxcdn.com/11888/video/DSRTO7UVOX06S7"
            ]
        );
        assert!(player_urls("<script>_play.init('p', 1)</script>").is_empty());
    }

    #[test]
    fn akamai_free_and_3q_layouts_name_their_streams() {
        let origin = Url::parse("nexx:1").unwrap();
        let video = json!({
            "general": {"hash": "H4SH"},
            "streamdata": {"cdnType": "free", "cdnProvider": "ak", "originalDomain": "free.example",
                "applyFolderHierarchy": 0, "applyAzureStructure": 0, "azureFileDistribution": "1500:1280x720,700:640x360",
                "cdnPathHLS": "hls."}
        });
        let streams = free_streams(&video, "12", &origin).unwrap();
        assert!(streams.files.is_empty());
        assert_eq!(streams.manifests.len(), 1);
        assert_eq!(streams.manifests[0].kind, ManifestKind::Hls);
        assert_eq!(
            streams.manifests[0].url,
            "http://hls.free.example/12/H4SH_,1280x720,640x360,.mp4.csmil/master.m3u8"
        );

        let video = json!({
            "streamdata": {"cdnType": "3q", "qAccount": "acc", "qPrefix": "pre", "qLocator": "loc", "qHash": "hash",
                "qReferences": "webm:1500000:ref1,broken", "azureFileDistribution": "1200:1280x720:file720"},
            "protectiondata": {"key": "K"}
        });
        let streams = threeq_streams(&video, &origin).unwrap();
        let manifests: Vec<&str> = streams.manifests.iter().map(|m| m.url.as_str()).collect();
        assert_eq!(
            manifests,
            vec![
                "http://sdn-global-streaming-cache.3qsdn.com/s/K/acc/files/pre/loc/acc-hash.ism/manifest.m3u8",
                "http://sdn-global-streaming-cache.3qsdn.com/s/K/acc/files/pre/loc/acc-hash.ism/manifest.mpd"
            ]
        );
        assert_eq!(streams.files.len(), 2);
        assert_eq!(
            streams.files[0].url.as_str(),
            "http://sdn-global-prog-cache.3qsdn.com/s/K/acc/uploads/acc-ref1.webm"
        );
        assert_eq!(streams.files[0].format_id.as_deref(), Some("3q-webm-1500"));
        assert_eq!(streams.files[0].container, Some(Container::Webm));
        assert_eq!(streams.files[0].bitrate, Some(1_500_000));
        assert_eq!(
            streams.files[1].url.as_str(),
            "http://sdn-global-prog-cache.3qsdn.com/s/K/acc/files/pre/loc/file720.mp4"
        );
        assert_eq!(streams.files[1].height, Some(720));
        assert_eq!(streams.files[1].format_id.as_deref(), Some("3q-http-1200"));

        let video = json!({
            "streamdata": {"cdnType": "3q", "qAccount": "acc", "qPrefix": "pre", "qLocator": "loc", "qHash": "hash",
                "cdnShieldHTTPS": "shield.example/", "cdnShieldProgHTTP": "prog.example/"}
        });
        let streams = threeq_streams(&video, &origin).unwrap();
        assert!(
            streams.manifests[0]
                .url
                .starts_with("https://shield.example/acc/")
        );
        assert!(streams.files.is_empty());
    }

    #[tokio::test]
    async fn catalogued_azure_videos_resolve() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get("https://arc.nexx.cloud/api/video/161464.json", 200, "application/json", json!({
            "result": [{"general": {"ID": 161464, "title": "Nervenkitzel Achterbahn", "description": "Karussellbauer", "studio": "SPIEGEL TV",
                "runtime": "00:46:01", "uploaded": 1394021479, "language_raw": "de"},
                "imagedata": {"thumb": "https://assets.nexx.cloud/media/1.jpg"},
                "streamdata": {"cdnType": "azure", "azureLocator": "loc1", "azureAccount": "nexxplayplus01",
                    "azureFileDistribution": "1200:1280x720,600:640x360,bad"},
                "protectiondata": {"token": "tok"},
                "captiondata": [
                    {"language": "de", "language_long": "Deutsch", "format": "vtt", "url": "https://assets.nexx.cloud/c/de.vtt"},
                    {"language": "en", "data": [{"fromms": 0, "toms": 1000, "caption": "hi"}]}
                ]}]
        }).to_string()));
        fixture.exchanges.push(get(
            "http://nx-p01.akamaized.net/loc1/161464_src.ism/Manifest(format=m3u8-aapl)?hdnts=tok",
            200,
            "application/vnd.apple.mpegurl",
            MASTER.into(),
        ));
        fixture.exchanges.push(get(
            "https://stream.test/720p/index.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MEDIA.into(),
        ));
        fixture.exchanges.push(get(
            "http://nx-p01.akamaized.net/loc1/161464_src.ism/Manifest(format=mpd-time-csf)?hdnts=tok",
            404,
            "text/plain",
            String::new(),
        ));
        fixture.exchanges.push(get(
            "http://nx-p01.akamaized.net/loc1/161464_src.ism/Manifest?hdnts=tok",
            404,
            "text/plain",
            String::new(),
        ));
        let resolver = NexxResolver::new(Http::replay(fixture));
        let url = Url::parse("https://api.nexx.cloud/v3/748/videos/byid/161464").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("161464"));
        assert_eq!(resolved.title.as_deref(), Some("Nervenkitzel Achterbahn"));
        assert_eq!(resolved.uploader.as_deref(), Some("SPIEGEL TV"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(2761)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1394021479)
        );
        assert_eq!(
            resolved.thumbnail.as_ref().map(|u| u.as_str()),
            Some("https://assets.nexx.cloud/media/1.jpg")
        );
        assert_eq!(resolved.subtitles.len(), 1);
        assert_eq!(resolved.subtitles[0].language, "de");
        assert_eq!(resolved.subtitles[0].format, SubtitleFormat::Vtt);
        assert_eq!(resolved.subtitles[0].name.as_deref(), Some("Deutsch"));
        assert_eq!(resolved.variants.len(), 3);
        assert_eq!(
            resolved.variants[0].url.as_str(),
            "http://nx-d01.akamaized.net/loc1/161464_src_1280x720_1200.mp4"
        );
        assert_eq!(resolved.variants[0].height, Some(720));
        assert_eq!(resolved.variants[0].bitrate, Some(1_200_000));
        assert_eq!(
            resolved.variants[0].format_id.as_deref(),
            Some("azure-http-1200")
        );
        assert_eq!(resolved.variants[0].video, Some(VideoCodec::H264));
        assert_eq!(resolved.variants[1].height, Some(360));
        let stream = &resolved.variants[2];
        assert_eq!(stream.url.as_str(), "https://stream.test/720p/index.m3u8");
        assert_eq!(stream.format_id.as_deref(), Some("azure-hls-2000"));
        assert_eq!(stream.duration, Some(Duration::from_secs(15)));
    }

    #[tokio::test]
    async fn uncatalogued_videos_go_through_a_session() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://arc.nexx.cloud/api/video/1269984.json",
            404,
            "application/json",
            json!({"result": null}).to_string(),
        ));
        fixture.exchanges.push(post(
            "https://api.nexx.cloud/v3/741/session/init",
            200,
            json!({
                "metadata": {"status": 200},
                "result": {"general": {"cid": 555}, "device": {"domaintoken": "abcdefghijklmnop"}}
            })
            .to_string(),
        ));
        fixture.exchanges.push(post("https://api.nexx.cloud/v3/741/videos/byid/1269984", 200, json!({
            "metadata": {"status": 200},
            "result": {"general": {"ID": 1269984, "title": "1 TAG ohne KLO", "hash": "H4SH", "runtime": "10:07", "studio_adref": "funk"},
                "streamdata": {"cdnType": "free", "cdnProvider": "ce", "originalDomain": "free.example",
                    "applyFolderHierarchy": 1, "applyAzureStructure": 1,
                    "azureFileDistribution": "1500:1280x720,700:640x360",
                    "cdnPathHTTP": "http.", "cdnPathDASH": "dash.", "cdnPathHLS": "hls."}}
        }).to_string()));
        fixture.exchanges.push(get(
            "http://dash.free.example/48/99/1269984/asset.ism/manifest.mpd?dcp_ver=aos4&videostream=H4SH_1280x720_1500.mp4:1500000,H4SH_640x360_700.mp4:700000&audiostream=H4SH_640x360_700.mp4",
            200,
            "application/dash+xml",
            MPD.into(),
        ));
        fixture.exchanges.push(get(
            "http://hls.free.example/48/99/1269984/asset.ism/manifest.m3u8?dcp_ver=aos4&videostream=H4SH_1280x720_1500.mp4:1500000,H4SH_640x360_700.mp4:700000&audiostream=H4SH_640x360_700.mp4",
            200,
            "application/vnd.apple.mpegurl",
            MASTER.into(),
        ));
        fixture.exchanges.push(get(
            "https://stream.test/720p/index.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MEDIA.into(),
        ));
        let resolver = NexxResolver::new(Http::replay(fixture));
        let url = Url::parse("nexx:741:1269984").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.title.as_deref(), Some("1 TAG ohne KLO"));
        assert_eq!(resolved.uploader.as_deref(), Some("funk"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(607)));
        let urls: Vec<&str> = resolved.variants.iter().map(|v| v.url.as_str()).collect();
        assert_eq!(
            urls[0],
            "http://http.free.example/48/99/1269984/H4SH_1280x720_1500.mp4"
        );
        assert_eq!(
            urls[1],
            "http://http.free.example/48/99/1269984/H4SH_640x360_700.mp4"
        );
        assert_eq!(
            resolved.variants[0].format_id.as_deref(),
            Some("free-http-1500")
        );
        assert_eq!(resolved.variants[0].width, Some(1280));
        assert!(
            resolved
                .variants
                .iter()
                .any(|v| v.kind == crate::resolve::VariantKind::Dash)
        );
        assert!(
            resolved
                .variants
                .iter()
                .any(|v| v.kind == crate::resolve::VariantKind::Hls)
        );
        assert!(
            resolved
                .variants
                .iter()
                .any(|v| v.format_id.as_deref() == Some("free-dash-v720"))
        );
    }

    #[tokio::test]
    async fn refusals_and_unknown_cdns_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://arc.nexx.cloud/api/video/1.json",
            200,
            "application/json",
            json!({"result": {}}).to_string(),
        ));
        fixture.exchanges.push(post(
            "https://api.nexx.cloud/v3/741/session/init",
            200,
            json!({
                "metadata": {"status": 403, "errorhint": "domain is not allowed"}
            })
            .to_string(),
        ));
        fixture.exchanges.push(get(
            "https://arc.nexx.cloud/api/video/2.json",
            200,
            "application/json",
            json!({"result": {"general": {"ID": 2, "title": "Locked"}, "streamdata": {"cdnType": "vimeo"}}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://arc.nexx.cloud/api/video/3.json",
            500,
            "text/plain",
            String::new(),
        ));
        let resolver = NexxResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("nexx:741:1").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("domain is not allowed")),
            "{error}"
        );
        let error = resolver
            .resolve(&Url::parse("nexx:2").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("vimeo formats")),
            "{error}"
        );
        let error = resolver
            .resolve(&Url::parse("nexx:3").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("names no nexx domain")),
            "{error}"
        );
    }

    fn embed_page(media_id: &str, stream_type: &str, domain_id: &str) -> String {
        format!(
            "<html><head><script type='text/javascript'>function onPlayReady(){{_play.control.addPlayer('nxpembedplayer','{media_id}','{stream_type}',new _play.PlayerConfiguration({{}}));}}</script>\n<script src='https://arc.nexx.cloud/sdk/{domain_id}.play' type='text/javascript' fetchpriority=\"high\" crossorigin=\"anonymous\"></script></head><body dir=\"ltr\"><div id='nxpembedplayer' style='width:100%;height:100%;'></div></body></html>"
        )
    }

    #[test]
    fn embed_pages_name_their_media() {
        assert_eq!(
            embed_media_id(&embed_page("1269984", "video", "741")).as_deref(),
            Some("1269984")
        );
        assert_eq!(
            embed_media_id(&embed_page("0", "video", "741")).as_deref(),
            Some("0")
        );
        assert_eq!(
            embed_media_id(&embed_page("", "", "748")).as_deref(),
            Some("")
        );
        assert_eq!(
            embed_media_id("<html><body>nothing here</body></html>"),
            None
        );
    }

    #[tokio::test]
    async fn embed_pages_hand_over_to_the_api_link() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://embed.nexx.cloud/741/video/71269984GMIR7QA",
            200,
            "text/html",
            embed_page("1269984", "video", "741"),
        ));
        fixture.exchanges.push(get(
            "https://embed.nexx.cloud/748/video/KC1614647Z27Y7T",
            200,
            "text/html",
            embed_page("161464", "video", "748"),
        ));
        fixture.exchanges.push(get(
            "https://embed.nexx.cloud/741/video/NOTAHASH",
            200,
            "text/html",
            embed_page("0", "video", "741"),
        ));
        fixture.exchanges.push(get(
            "https://embed.nexx.cloud/11888/video/DSRTO7UVOX06S7",
            200,
            "text/html",
            embed_page("", "", "11888"),
        ));
        fixture.exchanges.push(get(
            "https://embed.nexx.cloud/11888/video/BLANK",
            200,
            "text/html",
            "<html><body>nothing here</body></html>".into(),
        ));
        let resolver = NexxResolver::new(Http::replay(fixture));
        let resolve = |link: &str| {
            let url = Url::parse(link).unwrap();
            let resolver = &resolver;
            async move { resolver.resolve(&url).await.unwrap_err() }
        };
        let error = resolve("https://embed.nexx.cloud/741/video/71269984GMIR7QA").await;
        assert!(
            matches!(&error, ResolveError::Redirect(u) if u.as_str() == "https://api.nexx.cloud/v3/741/videos/byid/1269984"),
            "{error}"
        );
        // A link from before the host asked for a stream type is read as a video's.
        let error = resolve("http://embed.nexx.cloud/748/KC1614647Z27Y7T?autoplay=1").await;
        assert!(
            matches!(&error, ResolveError::Redirect(u) if u.as_str() == "https://api.nexx.cloud/v3/748/videos/byid/161464"),
            "{error}"
        );
        let error = resolve("https://embed.nexx.cloud/741/video/NOTAHASH").await;
        assert!(matches!(error, ResolveError::NotFound(_)), "{error}");
        let error = resolve("https://embed.nexx.cloud/11888/video/DSRTO7UVOX06S7").await;
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("no player")),
            "{error}"
        );
        let error = resolve("https://embed.nexx.cloud/11888/video/BLANK").await;
        assert!(
            matches!(&error, ResolveError::Malformed { detail, .. } if detail.contains("no player call")),
            "{error}"
        );
        let page = Page::parse(
            r#"<iframe src="//embed.nexx.cloud/748/KC1614647Z27Y7T"></iframe>"#,
            &Url::parse("https://example.com/").unwrap(),
        );
        let found = resolver.embeds_in(&page);
        assert_eq!(found.len(), 1);
        assert!(resolver.matches(&found[0]));
    }
}
