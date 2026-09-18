//! Akamai Adaptive Media Player feeds: the JSON (often JSONP-wrapped) feed an Akamai AMP
//! player reads, whose `channel.item` carries the media files and playlists, thumbnails
//! and captions. The news sites built on it, Fox News (with Fox Business) and ABC News,
//! number their videos, and a video page, player embed or feed link names that number;
//! [`feed_info`] reads any AMP feed.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport,
    SubtitleFormat, SubtitleTrack, Tag, Variant, VariantKind, clean_title, fetch, hls,
    path_extension, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "amp";

/// The AMP-based news sites, each with its own feed and video pages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Site {
    FoxNews,
    FoxBusiness,
    AbcNews,
}

/// What a link names: the site's video page, its player embed, its feed, or a story
/// page whose videos are named in its data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Video,
    Embed,
    Feed,
    Story,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub site: Site,
    pub kind: Kind,
    /// The video's number.
    pub id: String,
}

/// `/v/{id}` on Fox's video hosts.
static RE_FOX_PLAYER: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/v/(\d+)").unwrap());
/// `/video/{id}` on the Fox sites.
static RE_FOX_PAGE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/video/(\d+)").unwrap());
/// `/v3/video-player/{id}`, Fox's feed.
static RE_FOX_FEED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/v3/video-player/(\d+)").unwrap());
/// `/video/{id}/` as abcnews.com writes it, or `/{section}/video/{slug}-{id}` as
/// abcnews.go.com did (it redirects to the former).
static RE_ABC_PAGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:/[^/]+)*/video/(?:[0-9a-z-]+-)?(\d+)/?$").unwrap());
/// `/{section…}/{slug}/story` with `?id={id}`: an ABC News story, whose lead and inline
/// videos are named in the page's data.
static RE_ABC_STORY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(?:[^/]+/)+[0-9a-z-]+/story/?$").unwrap());
/// `window['__abcnews__'] = {…};`: the data an ABC News page renders from.
static RE_ABC_DATA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"window\['__abcnews__'\]\s*=\s*").unwrap());
/// The `src` of a script, iframe or AMP iframe.
static RE_EMBED_SRC: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?is)<(?:script|iframe|amp-iframe)\b[^>]*?\bsrc\s*=\s*(?:"([^"]+)"|'([^']+)')"#)
        .unwrap()
});
/// `data-video-id="{id}"`, how Fox's own pages name the video their player loads.
static RE_DATA_VIDEO_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\bdata-video-id\s*=\s*["'](\d+)["']"#).unwrap());

fn digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// The video a Fox News, Fox Business or ABC News link names.
pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let path = url.path();
    let query_id = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| util::query_param(url, key))
            .filter(|id| digits(id))
    };
    let link = |site, kind, id: String| Some(Link { site, kind, id });
    match host.as_str() {
        "video.foxnews.com" | "video.insider.foxnews.com" | "video.foxbusiness.com" => {
            let site = if host == "video.foxbusiness.com" {
                Site::FoxBusiness
            } else {
                Site::FoxNews
            };
            if matches!(path, "/v/video-embed.html" | "/v/embed.js") {
                return query_id(&["video_id", "id"]).and_then(|id| link(site, Kind::Embed, id));
            }
            let caps = RE_FOX_PLAYER.captures(path)?;
            link(site, Kind::Video, caps[1].to_string())
        }
        "foxnews.com" | "www.foxnews.com" => {
            let caps = RE_FOX_PAGE.captures(path)?;
            link(Site::FoxNews, Kind::Video, caps[1].to_string())
        }
        "foxbusiness.com" | "www.foxbusiness.com" => {
            let caps = RE_FOX_PAGE.captures(path)?;
            link(Site::FoxBusiness, Kind::Video, caps[1].to_string())
        }
        "api.foxnews.com" => {
            let caps = RE_FOX_FEED.captures(path)?;
            link(Site::FoxNews, Kind::Feed, caps[1].to_string())
        }
        "abcnews.com" | "www.abcnews.com" | "abcnews.go.com" => {
            let kind = match path {
                "/video/embed" => Kind::Embed,
                "/video/itemfeed" => Kind::Feed,
                _ if RE_ABC_STORY.is_match(path) => Kind::Story,
                _ => {
                    let caps = RE_ABC_PAGE.captures(path)?;
                    return link(Site::AbcNews, Kind::Video, caps[1].to_string());
                }
            };
            query_id(&["id"]).and_then(|id| link(Site::AbcNews, kind, id))
        }
        "fivethirtyeight.abcnews.go.com" => {
            // `/video/embed/{video id}/{story id}`
            let mut segments = path.trim_matches('/').split('/');
            if segments.next() != Some("video") || segments.next() != Some("embed") {
                return None;
            }
            let id = segments.next().filter(|id| digits(id))?;
            link(Site::AbcNews, Kind::Embed, id.to_string())
        }
        _ => None,
    }
}

/// The AMP feed the site serves for the video.
pub fn feed_url(link: &Link) -> Url {
    let id = &link.id;
    let raw = match link.site {
        Site::FoxNews | Site::FoxBusiness => {
            format!("https://api.foxnews.com/v3/video-player/{id}?callback=uid_{id}")
        }
        Site::AbcNews => format!("https://abcnews.go.com/video/itemfeed?id={id}"),
    };
    Url::parse(&raw).expect("ids are digits")
}

/// The site's page for the video.
pub fn page_url(link: &Link) -> Url {
    let id = &link.id;
    let raw = match link.site {
        Site::FoxNews => format!("https://www.foxnews.com/video/{id}"),
        Site::FoxBusiness => format!("https://www.foxbusiness.com/video/{id}"),
        Site::AbcNews => format!("https://abcnews.com/video/{id}/"),
    };
    Url::parse(&raw).expect("ids are digits")
}

/// The site's player embed for the video, the link an embedding page is read as.
pub fn embed_url(link: &Link) -> Url {
    let id = &link.id;
    let raw = match link.site {
        Site::FoxNews | Site::FoxBusiness => {
            format!("https://video.foxnews.com/v/video-embed.html?video_id={id}")
        }
        Site::AbcNews => format!("https://abcnews.com/video/embed?id={id}"),
    };
    Url::parse(&raw).expect("ids are digits")
}

/// Whether a page is one of Fox's own, whose markup names videos by `data-video-id`.
fn fox_page(url: &Url) -> Option<Site> {
    let host = url.host_str()?.to_ascii_lowercase();
    if host == "foxnews.com" || host.ends_with(".foxnews.com") {
        Some(Site::FoxNews)
    } else if host == "foxbusiness.com" || host.ends_with(".foxbusiness.com") {
        Some(Site::FoxBusiness)
    } else {
        None
    }
}

/// The JSON inside a JSONP answer such as `uid_123({…});` or `cb && cb({…}) // done`,
/// as yt-dlp's `strip_jsonp` reads it; text that is not a call comes back as it is.
pub fn strip_jsonp(text: &str) -> &str {
    let text = text.trim();
    let rest = text.strip_prefix("window.").unwrap_or(text);
    let name_len = rest
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'$'))
        .count();
    let (name, mut rest) = rest.split_at(name_len);
    let guarded = rest.trim_start();
    if let Some(after_and) = guarded.strip_prefix("&&") {
        match after_and.trim_start().strip_prefix(name) {
            Some(tail) => rest = tail,
            None => return text,
        }
    }
    let Some(inner) = rest.trim_start().strip_prefix('(') else {
        return text;
    };
    let inner = inner.trim_start();
    inner
        .rmatch_indices(')')
        .find(|(at, _)| closes_call(&inner[at + 1..]))
        .map(|(at, _)| inner[..at].trim())
        .unwrap_or(text)
}

/// What may follow the closing parenthesis of a JSONP call: a semicolon, whitespace and
/// `//` comments.
fn closes_call(tail: &str) -> bool {
    let mut rest = tail.strip_prefix(';').unwrap_or(tail).trim_start();
    while let Some(comment) = rest.strip_prefix("//") {
        rest = match comment.find('\n') {
            Some(newline) => comment[newline..].trim(),
            None => "",
        };
    }
    rest.is_empty()
}

/// yt-dlp's `mimetype2ext` table: the extension a MIME type or subtype stands for.
const MIME_EXTENSIONS: &[(&str, &str)] = &[
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

/// The file extension a MIME type stands for, as yt-dlp's `mimetype2ext`: `video/mp4`
/// is `mp4`, `application/vnd.apple.mpegurl` is `m3u8`; a type the table does not know
/// is its subtype, with `+` written as `.`.
pub fn mime_extension(mime: &str) -> Option<String> {
    let mimetype = mime.split(';').next()?.trim().to_ascii_lowercase();
    let subtype = mimetype.rsplit('/').next().unwrap_or("");
    let last = subtype.rsplit('+').next().unwrap_or(subtype);
    for key in [mimetype.as_str(), subtype, last] {
        if let Some((_, ext)) = MIME_EXTENSIONS.iter().find(|(known, _)| *known == key) {
            return Some((*ext).to_string());
        }
    }
    let ext = subtype.replace('+', ".");
    (!ext.is_empty()).then_some(ext)
}

/// The subtitle format a file extension names; other extensions are not subtitles the
/// engine converts.
pub fn subtitle_format(ext: &str) -> Option<SubtitleFormat> {
    match ext.to_ascii_lowercase().as_str() {
        "vtt" | "webvtt" => Some(SubtitleFormat::Vtt),
        "srt" => Some(SubtitleFormat::Srt),
        "ttml" | "dfxp" | "tt" | "xml" | "ttaf" => Some(SubtitleFormat::Ttml),
        "ass" | "ssa" => Some(SubtitleFormat::Ass),
        _ => None,
    }
}

/// Python truthiness of a JSON value: `null`, `false`, `0`, `""`, `[]` and `{}` are not.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

static NULL: Value = Value::Null;

/// The item's `media-<name>` node, looked up in its `media-group` first, then on the
/// item itself, then under the bare name.
fn media_node<'a>(item: &'a Value, name: &str) -> &'a Value {
    let media_name = format!("media-{name}");
    let group = if truthy(&item["media-group"]) {
        &item["media-group"]
    } else {
        item
    };
    [&group[&media_name], &item[&media_name], &item[name]]
        .into_iter()
        .find(|v| truthy(v))
        .unwrap_or(&NULL)
}

/// A node the feed writes either as one object or as a list of them.
fn entries(value: &Value) -> Vec<&Value> {
    match value {
        Value::Array(items) => items.iter().collect(),
        Value::Object(_) => vec![value],
        _ => Vec::new(),
    }
}

/// A media or picture link as the feed writes it: absolute, or `//host/path` (which the
/// player reads over plain `http`).
fn media_url(raw: &str) -> Option<Url> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let candidate = match raw.strip_prefix("//") {
        Some(rest) => format!("http://{rest}"),
        None => raw.to_string(),
    };
    let url = Url::parse(&candidate).ok()?;
    matches!(
        url.scheme(),
        "http"
            | "https"
            | "rtmp"
            | "rtmpt"
            | "rtmpe"
            | "rtmps"
            | "rtmpte"
            | "rtmpts"
            | "rtmfp"
            | "rtsp"
            | "rtsps"
            | "rtspu"
            | "mms"
            | "ftp"
            | "ftps"
    )
    .then_some(url)
}

/// Reads the Akamai AMP feed at `feed_url` as `platform` (the caller's cookie jar) and
/// turns its item into media: every file and HLS playlist it lists, the largest
/// thumbnail, its captions, title, description, publication time and duration. `origin`
/// is the link the caller is resolving, named in errors. The caller sets `id` and
/// `webpage_url` to what its own link names.
pub async fn feed_info(
    http: &Http,
    feed_url: &Url,
    platform: &str,
    origin: &Url,
) -> Result<Resolved, ResolveError> {
    let fetched = fetch(http, feed_url, platform, BROWSER_UA, &[], MAX_PAGE).await?;
    if let Some(error) = status_error(fetched.status, origin) {
        return Err(error);
    }
    let text = fetched.text();
    let feed: Value = serde_json::from_str(strip_jsonp(&text))
        .map_err(|e| ResolveError::malformed(origin, format!("AMP feed JSON: {e}")))?;
    let item = &feed["channel"]["item"];
    if !truthy(item) {
        let said = util::text(&feed["error"]).unwrap_or_else(|| "the feed has no item".into());
        return Err(ResolveError::unavailable(
            origin,
            format!("AMP said: {said}"),
        ));
    }
    let id = util::text(&item["guid"])
        .ok_or_else(|| ResolveError::malformed(origin, "the feed item has no guid"))?;

    let mut thumbnail: Option<(u64, Url)> = None;
    for data in entries(media_node(item, "thumbnail")) {
        let attrs = &data["@attributes"];
        let Some(url) = attrs["url"].as_str().and_then(media_url) else {
            continue;
        };
        let area =
            util::uint(&attrs["width"]).unwrap_or(0) * util::uint(&attrs["height"]).unwrap_or(0);
        if thumbnail
            .as_ref()
            .is_none_or(|(largest, _)| area > *largest)
        {
            thumbnail = Some((area, url));
        }
    }

    let mut subtitles = Vec::new();
    for data in entries(media_node(item, "subTitle")) {
        let attrs = &data["@attributes"];
        let Some(href) = attrs["href"].as_str().and_then(media_url) else {
            continue;
        };
        let ext = attrs["type"]
            .as_str()
            .and_then(mime_extension)
            .or_else(|| path_extension(&href));
        let Some(format) = ext.as_deref().and_then(subtitle_format) else {
            continue;
        };
        subtitles.push(SubtitleTrack {
            url: href,
            language: util::text(&attrs["lang"]).unwrap_or_else(|| "en".into()),
            name: None,
            format,
            auto: false,
            headers: Vec::new(),
        });
    }

    let contents = entries(media_node(item, "content"));
    if contents.is_empty() {
        return Err(ResolveError::unavailable(origin, "the feed lists no media"));
    }
    let mut variants = Vec::new();
    let mut manifest_error = None;
    let mut offered_hds = false;
    let mut offered_mms = false;
    for data in &contents {
        let attrs = &data["@attributes"];
        let Some(url) = attrs["url"].as_str().and_then(media_url) else {
            continue;
        };
        let ext = attrs["type"]
            .as_str()
            .and_then(mime_extension)
            .or_else(|| path_extension(&url));
        match ext.as_deref() {
            Some("f4m") => offered_hds = true,
            Some("m3u8") => match hls::expand(http, &url, platform, BROWSER_UA, &[]).await {
                Ok(expanded) => {
                    for mut variant in expanded.variants {
                        variant.format_id = Some(match variant.bitrate {
                            Some(bits) => format!("hls-{}", bits / 1000),
                            None => "hls".into(),
                        });
                        variants.push(variant);
                    }
                    subtitles.extend(expanded.subtitles);
                }
                Err(error) => manifest_error = Some(error),
            },
            _ => {
                let kind = match url.scheme() {
                    "http" | "https" => VariantKind::File,
                    "rtsp" | "rtsps" | "rtspu" => VariantKind::Rtsp,
                    "mms" => {
                        offered_mms = true;
                        continue;
                    }
                    "ftp" | "ftps" => continue,
                    _ => VariantKind::Rtmp,
                };
                let mut variant = Variant::new(url, kind);
                variant.format_id = util::text(&data["media-category"]["@attributes"]["label"]);
                variant.bitrate = util::uint(&attrs["bitrate"]).map(|kbps| kbps * 1000);
                variant.size = util::uint(&attrs["fileSize"]);
                variant.duration = util::int(&attrs["duration"])
                    .filter(|d| *d > 0)
                    .map(|d| Duration::from_secs(d as u64));
                variant.container = ext.as_deref().map(|e| {
                    Container::from_extension(e).unwrap_or_else(|| Container::Other(e.to_string()))
                });
                if variant.container == Some(Container::Mp4) {
                    variant.video = variant.video.take().or(Some(VideoCodec::H264));
                    variant.audio = variant.audio.take().or(Some(AudioCodec::Aac));
                }
                variants.push(variant);
            }
        }
    }
    if variants.is_empty() {
        if let Some(error) = manifest_error {
            return Err(error);
        }
        if offered_hds {
            return Err(ResolveError::unavailable(
                origin,
                "only an HDS (f4m) stream is offered",
            ));
        }
        if offered_mms {
            return Err(ResolveError::unavailable(
                origin,
                "the stream is MMS, which is not supported",
            ));
        }
        return Err(ResolveError::unavailable(
            origin,
            "the feed lists no playable media",
        ));
    }

    let mut resolved = Resolved::new(platform);
    resolved.id = Some(id);
    resolved.title = util::text(media_node(item, "title"))
        .as_deref()
        .and_then(clean_title);
    resolved.description = util::text(media_node(item, "description"));
    resolved.thumbnail = thumbnail.map(|(_, url)| url);
    resolved.uploaded_at = item["pubDate"]
        .as_str()
        .and_then(util::parse_timestamp)
        .or_else(|| item["dc-date"].as_str().and_then(util::parse_timestamp));
    resolved.duration = util::int(&contents[0]["@attributes"]["duration"])
        .filter(|d| *d > 0)
        .map(|d| Duration::from_secs(d as u64))
        .or_else(|| variants.iter().find_map(|v| v.duration));
    resolved.subtitles = subtitles;
    // Feeds list the same file more than once, under several media groups.
    let mut seen: Vec<Url> = Vec::new();
    resolved.variants = variants
        .into_iter()
        .filter(|v| {
            if seen.contains(&v.url) {
                false
            } else {
                seen.push(v.url.clone());
                true
            }
        })
        .collect();
    Ok(resolved)
}

/// A video an ABC News story names: its number and, when the story says, its title.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoryVideo {
    pub id: String,
    pub title: Option<String>,
}

/// What an ABC News story page names: its headline and standfirst, and its videos in
/// order, the lead video first, then the inline ones and the players its body frames
/// (as page links, for whichever resolver knows them).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Story {
    pub headline: Option<String>,
    pub description: Option<String>,
    pub videos: Vec<StoryVideo>,
    pub frames: Vec<Url>,
}

/// The data `window['__abcnews__']` is assigned in a story page.
pub fn abcnews_data(html: &str) -> Option<Value> {
    let start = RE_ABC_DATA.find(html)?.end();
    let rest = &html[start..];
    let end = util::balanced_js_end(rest)?;
    serde_json::from_str(&rest[..end]).ok()
}

/// The videos an ABC News story's data names: every `featuredVideo` (the lead video, in
/// whichever component carries it), every object typed `video` with a number, and, in
/// the older `everscroll` layout, the inline videos and frames of the article contents.
pub fn story_of(data: &Value) -> Story {
    let mut story = Story::default();
    let content = &data["page"]["content"]["story"];
    story.headline = content["story"]["headline"]
        .as_str()
        .or(content["data"]["SEO"]["JSONLD"]["article"]["headline"].as_str())
        .or(content["everscroll"][0]["articleContents"]["headline"].as_str())
        .and_then(clean_title);
    story.description = content["story"]["description"]
        .as_str()
        .or(content["data"]["SEO"]["JSONLD"]["article"]["description"].as_str())
        .or(content["everscroll"][0]["articleContents"]["subHead"].as_str())
        .and_then(clean_title);
    fn push(story: &mut Story, id: &str, title: Option<&str>) {
        if !digits(id) || story.videos.iter().any(|v| v.id == id) {
            return;
        }
        story.videos.push(StoryVideo {
            id: id.to_string(),
            title: title.and_then(clean_title),
        });
    }
    fn walk(value: &Value, story: &mut Story) {
        match value {
            Value::Object(map) => {
                if let Some(featured) = map.get("featuredVideo")
                    && let Some(id) = featured["id"]
                        .as_str()
                        .or_else(|| featured["video"]["id"].as_str())
                {
                    let title = featured["headline"]
                        .as_str()
                        .or(featured["name"].as_str())
                        .or(featured["title"].as_str());
                    push(story, id, title);
                }
                if map.get("type").and_then(Value::as_str) == Some("video")
                    && let Some(id) = map.get("id").and_then(Value::as_str)
                {
                    let title = map
                        .get("headline")
                        .or_else(|| map.get("title"))
                        .or_else(|| map.get("name"))
                        .and_then(Value::as_str);
                    push(story, id, title);
                }
                if map.get("type").and_then(Value::as_str) == Some("iframe")
                    && let Some(src) = map["attrs"]["src"].as_str()
                    && let Ok(frame) = Url::parse(src)
                    && !story.frames.contains(&frame)
                {
                    story.frames.push(frame);
                }
                for child in map.values() {
                    walk(child, story);
                }
            }
            Value::Array(items) => {
                for child in items {
                    walk(child, story);
                }
            }
            _ => {}
        }
    }
    // The lead video first, wherever the layout carries it, then everything else in
    // the order the data lists it.
    for lead in [
        &content["story"]["featuredVideo"],
        &content["story"]["leadMediaVideo"]["video"]["featuredVideo"],
        &content["everscroll"][0]["featuredVideo"],
    ] {
        if let Some(id) = lead["id"].as_str().or_else(|| lead["video"]["id"].as_str()) {
            let title = lead["headline"]
                .as_str()
                .or(lead["name"].as_str())
                .or(lead["title"].as_str());
            push(&mut story, id, title);
        }
    }
    walk(content, &mut story);
    story
}

pub struct AmpResolver {
    http: Http,
}

impl AmpResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// An ABC News story: its one video resolves through the site's feed with the
    /// story's own headline; several are a playlist of the site's player embeds and the
    /// frames the story carries.
    async fn resolve_story(&self, link: &Link, url: &Url) -> Result<Resolution, ResolveError> {
        let fetched = fetch(
            &self.http,
            url,
            PLATFORM,
            BROWSER_UA,
            &super::navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        let html = fetched.text();
        let data = abcnews_data(&html)
            .ok_or_else(|| ResolveError::malformed(url, "the story page carries no data"))?;
        let story = story_of(&data);
        let mut entries: Vec<super::PlaylistEntry> = story
            .videos
            .iter()
            .map(|video| super::PlaylistEntry {
                url: embed_url(&Link {
                    site: Site::AbcNews,
                    kind: Kind::Embed,
                    id: video.id.clone(),
                }),
                title: video.title.clone(),
                duration: None,
            })
            .chain(story.frames.iter().map(|frame| super::PlaylistEntry {
                url: frame.clone(),
                title: None,
                duration: None,
            }))
            .collect();
        match entries.len() {
            0 => Err(ResolveError::NotFound(url.clone())),
            1 if !story.videos.is_empty() => {
                let video = Link {
                    site: Site::AbcNews,
                    kind: Kind::Video,
                    id: story.videos[0].id.clone(),
                };
                let mut resolved = feed_info(&self.http, &feed_url(&video), PLATFORM, url).await?;
                resolved.id = Some(video.id.clone());
                resolved.webpage_url = Some(fetched.url.clone());
                if resolved.title.is_none() {
                    resolved.title = story.videos[0].title.clone().or(story.headline);
                }
                if resolved.description.is_none() {
                    resolved.description = story.description;
                }
                Ok(Resolution::from(resolved))
            }
            1 => Err(ResolveError::Redirect(entries.remove(0).url)),
            count => Ok(Resolution::Playlist(super::Playlist {
                resolver: PLATFORM.into(),
                id: Some(link.id.clone()),
                title: story.headline,
                entries,
                total: Some(count),
            })),
        }
    }
}

#[async_trait]
impl Resolver for AmpResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Akamai AMP (Fox News, ABC News)",
            hosts: &[
                "foxnews.com",
                "video.foxnews.com",
                "foxbusiness.com",
                "video.foxbusiness.com",
                "abcnews.com",
                "abcnews.go.com",
            ],
            features: &["videos", "embeds", "feeds"],
            formats: &["hls", "mp4"],
            media: &[MediaKind::Video],
            tags: &[Tag::Players],
            session: SessionSupport::None,
            examples: &[
                "https://www.foxnews.com/video/6320653836112",
                "https://video.foxnews.com/v/video-embed.html?video_id=6320653836112",
                "https://abcnews.com/video/20411932/",
                "https://abcnews.go.com/video/itemfeed?id=20411932",
                "https://abcnews.go.com/Entertainment/peter-billingsley-child-actor-christmas-story-hollywood-power/story?id=51286501",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        if link.kind == Kind::Story {
            return self.resolve_story(&link, url).await;
        }
        let mut resolved = feed_info(&self.http, &feed_url(&link), PLATFORM, url).await?;
        resolved.id = Some(link.id.clone());
        resolved.webpage_url = Some(page_url(&link));
        Ok(Resolution::from(resolved))
    }

    /// The sites' player embeds in a page: script and iframe tags loading Fox's
    /// `video-embed.html` or `embed.js` or ABC's `video/embed`, and on Fox's own pages
    /// the `data-video-id` their player reads.
    fn embeds_in(&self, page: &Page) -> Vec<Url> {
        let mut found: Vec<Url> = Vec::new();
        let mut push = |link: Link| {
            let embed = embed_url(&link);
            if !found.contains(&embed) {
                found.push(embed);
            }
        };
        for caps in RE_EMBED_SRC.captures_iter(page.html()) {
            let Some(src) = caps.get(1).or_else(|| caps.get(2)) else {
                continue;
            };
            let Some(target) = util::join_url(Some(page.url()), &util::html_unescape(src.as_str()))
            else {
                continue;
            };
            if let Some(link) = parse_link(&target).filter(|l| l.kind == Kind::Embed) {
                push(link);
            }
        }
        if let Some(site) = fox_page(page.url()) {
            for caps in RE_DATA_VIDEO_ID.captures_iter(page.html()) {
                push(Link {
                    site,
                    kind: Kind::Embed,
                    id: caps[1].to_string(),
                });
            }
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};
    use serde_json::json;

    fn get(url: &str, status: u16, content_type: &str, body: String) -> Exchange {
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
                headers: vec![("content-type".into(), content_type.into())],
                body: RecordedBody::Text(body),
                truncated: false,
            },
        }
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    const FOX_VIDEO: &str = "https://www.foxnews.com/video/6320653836112";
    const FOX_EMBED: &str = "https://video.foxnews.com/v/video-embed.html?video_id=6320653836112";
    const ABC_VIDEO: &str = "https://abcnews.com/video/20411932/";
    const ABC_FEED: &str = "https://abcnews.go.com/video/itemfeed?id=20411932";

    fn recorded() -> AmpResolver {
        let fixture = Fixture::parse(include_str!("amp_fixture.json")).unwrap();
        AmpResolver::new(Http::replay(fixture))
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&url(s));
        let fox = |kind, id: &str| {
            Some(Link {
                site: Site::FoxNews,
                kind,
                id: id.into(),
            })
        };
        let abc = |kind, id: &str| {
            Some(Link {
                site: Site::AbcNews,
                kind,
                id: id.into(),
            })
        };
        assert_eq!(link(FOX_VIDEO), fox(Kind::Video, "6320653836112"));
        assert_eq!(
            link("https://video.foxnews.com/v/6320653836112/"),
            fox(Kind::Video, "6320653836112")
        );
        assert_eq!(
            link("http://video.foxnews.com/v/3937480/frozen-in-time/#sp=show-clips"),
            fox(Kind::Video, "3937480")
        );
        assert_eq!(link(FOX_EMBED), fox(Kind::Embed, "6320653836112"));
        assert_eq!(
            link(
                "http://video.insider.foxnews.com/v/video-embed.html?video_id=5099377331001&autoplay=true"
            ),
            fox(Kind::Embed, "5099377331001")
        );
        assert_eq!(
            link("https://video.foxnews.com/v/embed.js?autoplay=false&id=6189191231001"),
            fox(Kind::Embed, "6189191231001")
        );
        assert_eq!(
            link(
                "https://api.foxnews.com/v3/video-player/6320653836112?callback=uid_6320653836112"
            ),
            fox(Kind::Feed, "6320653836112")
        );
        assert_eq!(
            link("http://video.foxbusiness.com/v/4442309889001"),
            Some(Link {
                site: Site::FoxBusiness,
                kind: Kind::Video,
                id: "4442309889001".into()
            })
        );
        assert_eq!(
            link("https://www.foxbusiness.com/video/5599024308001"),
            Some(Link {
                site: Site::FoxBusiness,
                kind: Kind::Video,
                id: "5599024308001".into()
            })
        );
        assert_eq!(link(ABC_VIDEO), abc(Kind::Video, "20411932"));
        assert_eq!(
            link(
                "http://abcnews.go.com/ThisWeek/video/week-exclusive-irans-foreign-minister-zarif-20411932"
            ),
            abc(Kind::Video, "20411932")
        );
        assert_eq!(
            link("https://abcnews.com/video/embed?id=20411932"),
            abc(Kind::Embed, "20411932")
        );
        assert_eq!(link(ABC_FEED), abc(Kind::Feed, "20411932"));
        assert_eq!(
            link(
                "https://abcnews.go.com/Entertainment/peter-billingsley-child-actor-christmas-story-hollywood-power/story?id=51286501"
            ),
            abc(Kind::Story, "51286501")
        );
        assert_eq!(
            link(
                "http://abcnews.go.com/Technology/exclusive-apple-ceo-tim-cook-iphone-cracking-software/story?id=37173343"
            ),
            abc(Kind::Story, "37173343")
        );
        assert_eq!(
            link("https://abcnews.go.com/Entertainment/story?id=1"),
            None
        );
        assert_eq!(
            link("http://fivethirtyeight.abcnews.go.com/video/embed/35606406/25628179"),
            abc(Kind::Embed, "35606406")
        );
        assert_eq!(
            link("https://video.foxnews.com/v/video-embed.html?d=video.foxnews.com"),
            None
        );
        assert_eq!(
            link("https://www.foxnews.com/politics/some-article-slug"),
            None
        );
        assert_eq!(link("https://abcnews.go.com/US/story?id=136285118"), None);
        assert_eq!(
            link(
                "http://vid.bleacherreport.com/videos/8fd44c2f-3dc5-4821-9118-2c825a98c0e1.akamai"
            ),
            None
        );
        assert_eq!(link("ftp://www.foxnews.com/video/6320653836112"), None);
    }

    #[test]
    fn links_name_their_feed_page_and_embed() {
        let fox = parse_link(&url(FOX_EMBED)).unwrap();
        assert_eq!(
            feed_url(&fox).as_str(),
            "https://api.foxnews.com/v3/video-player/6320653836112?callback=uid_6320653836112"
        );
        assert_eq!(page_url(&fox).as_str(), FOX_VIDEO);
        assert_eq!(embed_url(&fox).as_str(), FOX_EMBED);
        let business = parse_link(&url("https://video.foxbusiness.com/v/5599024308001")).unwrap();
        assert_eq!(
            feed_url(&business).as_str(),
            "https://api.foxnews.com/v3/video-player/5599024308001?callback=uid_5599024308001"
        );
        assert_eq!(
            page_url(&business).as_str(),
            "https://www.foxbusiness.com/video/5599024308001"
        );
        let abc = parse_link(&url(ABC_VIDEO)).unwrap();
        assert_eq!(feed_url(&abc).as_str(), ABC_FEED);
        assert_eq!(page_url(&abc).as_str(), ABC_VIDEO);
        assert_eq!(
            embed_url(&abc).as_str(),
            "https://abcnews.com/video/embed?id=20411932"
        );
    }

    #[test]
    fn jsonp_wrappers_are_stripped() {
        assert_eq!(strip_jsonp(r#"uid_123({"a":1});"#), r#"{"a":1}"#);
        assert_eq!(strip_jsonp(r#"window.cb({"a":1})"#), r#"{"a":1}"#);
        assert_eq!(
            strip_jsonp("cb && cb({\"u\":\"http://x/\"}); // done\n"),
            r#"{"u":"http://x/"}"#
        );
        assert_eq!(strip_jsonp(r#"{"a":1}"#), r#"{"a":1}"#);
        assert_eq!(
            strip_jsonp(r#"cb && other({"a":1})"#),
            r#"cb && other({"a":1})"#
        );
        assert_eq!(strip_jsonp(r#"({"a":"b)"})"#), r#"{"a":"b)"}"#);
    }

    #[test]
    fn mime_types_name_extensions() {
        assert_eq!(mime_extension("video/mp4").as_deref(), Some("mp4"));
        assert_eq!(
            mime_extension("application/x-mpegURL").as_deref(),
            Some("m3u8")
        );
        assert_eq!(mime_extension("audio/mpeg").as_deref(), Some("mp3"));
        assert_eq!(
            mime_extension("application/ttml+xml; charset=utf-8").as_deref(),
            Some("ttml")
        );
        assert_eq!(mime_extension("text/vtt").as_deref(), Some("vtt"));
        assert_eq!(
            mime_extension("application/vnd.example+json").as_deref(),
            Some("json")
        );
        assert_eq!(mime_extension("x/made+up").as_deref(), Some("made.up"));
        assert_eq!(mime_extension(""), None);
        assert_eq!(subtitle_format("dfxp"), Some(SubtitleFormat::Ttml));
        assert_eq!(subtitle_format("plain"), None);
    }

    #[tokio::test]
    async fn fox_video_pages_resolve() {
        let resolver = recorded();
        assert!(resolver.matches(&url(FOX_VIDEO)));
        let resolved = resolver
            .resolve(&url(FOX_VIDEO))
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("6320653836112"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Tucker Carlson joins 'Gutfeld!' to discuss his new documentary")
        );
        assert!(resolved.description.is_some());
        assert!(resolved.thumbnail.is_some());
        assert_eq!(resolved.uploaded_at.unwrap().as_second(), 1676611344);
        assert_eq!(resolved.duration, Some(Duration::from_secs(404)));
        assert_eq!(
            resolved.webpage_url.as_ref().map(Url::as_str),
            Some(FOX_VIDEO)
        );
        assert!(!resolved.variants.is_empty());
        assert!(resolved.variants.iter().all(|v| v.kind == VariantKind::Hls));
        assert!(resolved.variants.iter().all(|v| v.height.is_some()));
        assert!(resolved.variants.iter().all(|v| {
            v.format_id
                .as_deref()
                .is_some_and(|f| f.starts_with("hls-"))
        }));
    }

    #[tokio::test]
    async fn fox_embeds_resolve_to_the_same_video() {
        let resolver = recorded();
        let resolved = resolver
            .resolve(&url(FOX_EMBED))
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("6320653836112"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Tucker Carlson joins 'Gutfeld!' to discuss his new documentary")
        );
        assert_eq!(
            resolved.webpage_url.as_ref().map(Url::as_str),
            Some(FOX_VIDEO)
        );
        assert!(!resolved.variants.is_empty());
    }

    #[tokio::test]
    async fn abc_video_pages_resolve_with_playlists_files_and_captions() {
        let resolver = recorded();
        assert!(resolver.matches(&url(ABC_VIDEO)));
        let resolved = resolver
            .resolve(&url(ABC_VIDEO))
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("20411932"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("'This Week' Exclusive: Iran's Foreign Minister Zarif")
        );
        assert_eq!(resolved.uploaded_at.unwrap().as_second(), 1380454200);
        assert_eq!(resolved.duration, Some(Duration::from_secs(180)));
        assert_eq!(
            resolved.webpage_url.as_ref().map(Url::as_str),
            Some(ABC_VIDEO)
        );
        assert!(resolved.thumbnail.is_some());
        assert!(resolved.variants.iter().any(|v| v.kind == VariantKind::Hls));
        let files: Vec<&Variant> = resolved
            .variants
            .iter()
            .filter(|v| v.kind == VariantKind::File)
            .collect();
        assert!(!files.is_empty());
        assert!(files.iter().all(|v| v.container == Some(Container::Mp4)));
        assert!(files.iter().all(|v| v.url.path().ends_with(".mp4")));
        assert!(
            resolved
                .subtitles
                .iter()
                .any(|s| s.format == SubtitleFormat::Ttml)
        );
    }

    #[tokio::test]
    async fn abc_feed_links_resolve() {
        let resolver = recorded();
        let resolved = resolver
            .resolve(&url(ABC_FEED))
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("20411932"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("'This Week' Exclusive: Iran's Foreign Minister Zarif")
        );
        assert_eq!(
            resolved.webpage_url.as_ref().map(Url::as_str),
            Some(ABC_VIDEO)
        );
        assert!(!resolved.variants.is_empty());
    }

    #[test]
    fn embedded_players_are_found() {
        let resolver = AmpResolver::new(Http::replay(Fixture::new(PLATFORM, None)));
        let html = r#"<div class="video-container" data-video-id="6405098729112"></div>
            <iframe src="//video.foxnews.com/v/video-embed.html?video_id=6189191231001&amp;d=video.foxnews.com" allowfullscreen></iframe>
            <script src='https://video.foxnews.com/v/embed.js?autoplay=false&id=6189191231001'></script>
            <amp-iframe src="https://video.foxnews.com/v/video-embed.html?video_id=5748266721001"></amp-iframe>
            <iframe src="https://abcnews.go.com/video/embed?id=20411932"></iframe>
            <iframe src="https://www.youtube.com/embed/abc"></iframe>
            <iframe src="https://www.foxnews.com/video/6320653836112"></iframe>"#;
        let page = Page::parse(html, &url("https://www.foxnews.com/politics/an-article"));
        let found = resolver.embeds_in(&page);
        let embeds: Vec<&str> = found.iter().map(Url::as_str).collect();
        assert_eq!(
            embeds,
            vec![
                "https://video.foxnews.com/v/video-embed.html?video_id=6189191231001",
                "https://video.foxnews.com/v/video-embed.html?video_id=5748266721001",
                "https://abcnews.com/video/embed?id=20411932",
                "https://video.foxnews.com/v/video-embed.html?video_id=6405098729112",
            ]
        );
        let elsewhere = Page::parse(html, &url("https://example.com/a-page"));
        let embeds = resolver.embeds_in(&elsewhere);
        assert_eq!(embeds.len(), 3);
        assert!(embeds.iter().all(|u| !u.as_str().contains("6405098729112")));
    }

    const MASTER: &str = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1280x720\nhttps://a.akamaihd.net/hls/720/index.m3u8\n";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\n0.ts\n#EXT-X-ENDLIST\n";

    #[tokio::test]
    async fn jsonp_feeds_with_media_groups_resolve() {
        let feed = json!({"channel": {"item": {
            "guid": "123",
            "pubDate": "Fri, 17 Feb 2023 14:22:24 GMT",
            "media-group": {
                "media-title": "  A title\n with spaces ",
                "media-description": "About it",
                "media-thumbnail": [
                    {"@attributes": {"url": "//a.akamaihd.net/t1.jpg", "width": "320", "height": "180"}},
                    {"@attributes": {"url": "https://a.akamaihd.net/t2.jpg", "width": "1280", "height": "720"}}
                ],
                "media-subTitle": [
                    {"@attributes": {"href": "https://a.akamaihd.net/s.vtt", "lang": "es", "type": "text/vtt"}},
                    {"@attributes": {"href": "", "lang": "en-us", "type": "text/vtt"}}
                ],
                "media-content": [
                    {"@attributes": {"url": "https://a.akamaihd.net/v.mp4", "type": "video/mp4", "bitrate": "1200", "fileSize": "23000000", "duration": "155"},
                     "media-category": {"@attributes": {"label": "Video"}}},
                    {"@attributes": {"url": "https://a.akamaihd.net/master.m3u8", "type": "application/x-mpegURL"}},
                    {"@attributes": {"url": "https://a.akamaihd.net/z.f4m", "type": "application/f4m+xml"}},
                    {"@attributes": {"type": "video/mp4"}}
                ]
            }
        }}});
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://api.foxnews.com/v3/video-player/123?callback=uid_123",
            200,
            "application/javascript",
            format!("uid_123({feed});"),
        ));
        fixture.exchanges.push(get(
            "https://a.akamaihd.net/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MASTER.into(),
        ));
        fixture.exchanges.push(get(
            "https://a.akamaihd.net/hls/720/index.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MEDIA.into(),
        ));
        let resolver = AmpResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&url("https://video.foxnews.com/v/123"))
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.resolver, PLATFORM);
        assert_eq!(resolved.id.as_deref(), Some("123"));
        assert_eq!(resolved.title.as_deref(), Some("A title with spaces"));
        assert_eq!(resolved.description.as_deref(), Some("About it"));
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://a.akamaihd.net/t2.jpg"
        );
        assert_eq!(resolved.uploaded_at.unwrap().as_second(), 1676643744);
        assert_eq!(resolved.duration, Some(Duration::from_secs(155)));
        assert_eq!(
            resolved.webpage_url.as_ref().map(Url::as_str),
            Some("https://www.foxnews.com/video/123")
        );
        assert_eq!(resolved.subtitles.len(), 1);
        assert_eq!(resolved.subtitles[0].language, "es");
        assert_eq!(resolved.subtitles[0].format, SubtitleFormat::Vtt);
        assert_eq!(resolved.variants.len(), 2);
        let file = &resolved.variants[0];
        assert_eq!(file.kind, VariantKind::File);
        assert_eq!(file.url.as_str(), "https://a.akamaihd.net/v.mp4");
        assert_eq!(file.format_id.as_deref(), Some("Video"));
        assert_eq!(file.bitrate, Some(1_200_000));
        assert_eq!(file.size, Some(23_000_000));
        assert_eq!(file.container, Some(Container::Mp4));
        assert_eq!(file.video, Some(VideoCodec::H264));
        let stream = &resolved.variants[1];
        assert_eq!(stream.kind, VariantKind::Hls);
        assert_eq!(stream.height, Some(720));
        assert_eq!(stream.format_id.as_deref(), Some("hls-2500"));
    }

    #[tokio::test]
    async fn single_node_feeds_are_read() {
        let feed = json!({"channel": {"item": {
            "guid": "77",
            "dc-date": "2015-07-23T19:17:12Z",
            "title": "Plain title",
            "description": "Plain description",
            "media-thumbnail": {"@attributes": {"url": "//a.akamaihd.net/one.jpg"}},
            "media-content": {"@attributes": {"url": "rtmp://cp1234.edgefcs.net/ondemand/mp4:clip", "type": "video/mp4", "duration": "42"}}
        }}});
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://abcnews.go.com/video/itemfeed?id=77",
            200,
            "application/json",
            feed.to_string(),
        ));
        let resolver = AmpResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&url("https://abcnews.com/video/embed?id=77"))
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("77"));
        assert_eq!(resolved.title.as_deref(), Some("Plain title"));
        assert_eq!(resolved.description.as_deref(), Some("Plain description"));
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "http://a.akamaihd.net/one.jpg"
        );
        assert_eq!(resolved.uploaded_at.unwrap().as_second(), 1437679032);
        assert_eq!(resolved.duration, Some(Duration::from_secs(42)));
        assert_eq!(
            resolved.webpage_url.as_ref().map(Url::as_str),
            Some("https://abcnews.com/video/77/")
        );
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].kind, VariantKind::Rtmp);
        assert_eq!(resolved.variants[0].duration, Some(Duration::from_secs(42)));
    }

    #[tokio::test]
    async fn missing_and_unplayable_feeds_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://abcnews.go.com/video/itemfeed?id=1",
            404,
            "text/plain",
            "not found".into(),
        ));
        fixture.exchanges.push(get(
            "https://abcnews.go.com/video/itemfeed?id=2",
            200,
            "application/json",
            json!({"error": "Video not found"}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://abcnews.go.com/video/itemfeed?id=3",
            200,
            "application/json",
            json!({"channel": {"item": {"guid": "3", "media-content": [
                {"@attributes": {"url": "https://a.akamaihd.net/z/manifest.f4m", "type": "application/f4m+xml"}}
            ]}}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.foxnews.com/v3/video-player/4?callback=uid_4",
            200,
            "application/javascript",
            "uid_4(not json);".into(),
        ));
        let resolver = AmpResolver::new(Http::replay(fixture));
        let resolve = |s: &str| {
            let resolver = &resolver;
            let url = url(s);
            async move { resolver.resolve(&url).await.unwrap_err() }
        };
        assert!(matches!(
            resolve("https://abcnews.com/video/1/").await,
            ResolveError::NotFound(_)
        ));
        let error = resolve("https://abcnews.com/video/2/").await;
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "AMP said: Video not found"),
            "{error}"
        );
        let error = resolve("https://abcnews.com/video/3/").await;
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("HDS")),
            "{error}"
        );
        assert!(matches!(
            resolve("https://www.foxnews.com/video/4").await,
            ResolveError::Malformed { .. }
        ));
    }

    #[test]
    fn story_data_names_the_lead_and_inline_videos_and_frames() {
        let current = r#"<html><script>window['__abcnews__'] = {"page":{"content":{"story":{"story":{"headline":"Peter Billingsley","description":"From child actor to power player.","featuredVideo":{"id":"51205814","type":"video","headline":"Tim Allen describes his costume"},"leadMediaVideo":{"video":{"featuredVideo":{"id":"51205814","headline":"Tim Allen describes his costume"}}}},"data":{"mainComponents":[{"name":"Body","props":{"body":[{"type":"video","id":"51205900","headline":"Inline clip"},{"type":"iframe","attrs":{"src":"https://www.youtube.com/embed/aaaaaaaaaaa"}},{"type":"p"}]}}]}}}}};</script></html>"#;
        let story = story_of(&abcnews_data(current).unwrap());
        assert_eq!(story.headline.as_deref(), Some("Peter Billingsley"));
        assert_eq!(
            story.description.as_deref(),
            Some("From child actor to power player.")
        );
        assert_eq!(
            story.videos,
            vec![
                StoryVideo {
                    id: "51205814".into(),
                    title: Some("Tim Allen describes his costume".into())
                },
                StoryVideo {
                    id: "51205900".into(),
                    title: Some("Inline clip".into())
                },
            ]
        );
        assert_eq!(story.frames.len(), 1);
        assert_eq!(
            story.frames[0].as_str(),
            "https://www.youtube.com/embed/aaaaaaaaaaa"
        );

        let older = r#"window['__abcnews__'] = {"page":{"content":{"story":{"everscroll":[{"featuredVideo":{"id":"38897857","name":"Justin Timberlake Drops Hints","video":{"feed":"http://abcnews.go.com/video/itemfeed?id=38897857"}},"articleContents":{"headline":"JT at Eurovision","subHead":"Pop News","inlines":[{"type":"iframe","attrs":{"src":"https://www.youtube.com/embed/bbbbbbbbbbb"}},{"type":"video","id":"38897999"}]}}]}}}};"#;
        let story = story_of(&abcnews_data(older).unwrap());
        assert_eq!(story.headline.as_deref(), Some("JT at Eurovision"));
        assert_eq!(story.description.as_deref(), Some("Pop News"));
        assert_eq!(
            story
                .videos
                .iter()
                .map(|v| v.id.as_str())
                .collect::<Vec<_>>(),
            vec!["38897857", "38897999"]
        );
        assert_eq!(
            story.videos[0].title.as_deref(),
            Some("Justin Timberlake Drops Hints")
        );
        assert_eq!(
            story.frames[0].as_str(),
            "https://www.youtube.com/embed/bbbbbbbbbbb"
        );
        assert!(abcnews_data("<html>no data</html>").is_none());
    }

    #[tokio::test]
    async fn stories_resolve_their_one_video_or_list_several() {
        let one = r#"<html><script>window['__abcnews__'] = {"page":{"content":{"story":{"story":{"headline":"One video","featuredVideo":{"id":"20411932","type":"video"}}}}}};</script></html>"#;
        let many = r#"<html><script>window['__abcnews__'] = {"page":{"content":{"story":{"story":{"headline":"Two videos","featuredVideo":{"id":"20411932","type":"video","headline":"Lead"}},"data":{"mainComponents":[{"props":{"body":[{"type":"video","id":"20411933","headline":"Second"},{"type":"iframe","attrs":{"src":"https://www.youtube.com/embed/aaaaaaaaaaa"}}]}}]}}}}};</script></html>"#;
        let feed = json!({"channel": {"title": "ABC News", "item": {"title": "'This Week' Exclusive", "guid": "20411932", "media-content": [{"@attributes": {"url": "https://ondemand.abcnews.com/x_700.mp4", "type": "video/mp4", "duration": "180"}}]}}}).to_string();
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://abcnews.go.com/Politics/one/story?id=1",
            200,
            "text/html",
            one.into(),
        ));
        fixture.exchanges.push(get(
            "https://abcnews.go.com/Politics/many/story?id=2",
            200,
            "text/html",
            many.into(),
        ));
        fixture.exchanges.push(get("https://abcnews.go.com/Politics/none/story?id=3", 200, "text/html", "<html><script>window['__abcnews__'] = {\"page\":{\"content\":{\"story\":{}}}};</script></html>".into()));
        fixture
            .exchanges
            .push(get(ABC_FEED, 200, "application/json", feed));
        let resolver = AmpResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&url("https://abcnews.go.com/Politics/one/story?id=1"))
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("20411932"));
        assert_eq!(resolved.title.as_deref(), Some("'This Week' Exclusive"));
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(
            resolved.webpage_url.as_ref().map(|u| u.as_str()),
            Some("https://abcnews.go.com/Politics/one/story?id=1")
        );

        let Resolution::Playlist(playlist) = resolver
            .resolve(&url("https://abcnews.go.com/Politics/many/story?id=2"))
            .await
            .unwrap()
        else {
            panic!("several videos are a playlist");
        };
        assert_eq!(playlist.id.as_deref(), Some("2"));
        assert_eq!(playlist.title.as_deref(), Some("Two videos"));
        assert_eq!(
            playlist
                .entries
                .iter()
                .map(|e| e.url.as_str())
                .collect::<Vec<_>>(),
            vec![
                "https://abcnews.com/video/embed?id=20411932",
                "https://abcnews.com/video/embed?id=20411933",
                "https://www.youtube.com/embed/aaaaaaaaaaa",
            ]
        );
        assert_eq!(playlist.entries[1].title.as_deref(), Some("Second"));
        assert!(matches!(
            resolver
                .resolve(&url("https://abcnews.go.com/Politics/none/story?id=3"))
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
