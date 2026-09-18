//! ABC (Australia): news, BTN and Listen pages on abc.net.au, whose Next.js data carries
//! the media's renditions, and ABC iview, whose catalogue API lists shows and series and
//! whose programs API names an episode's HLS streams, played with the token the apps
//! sign for. iview streams only inside Australia and, for most episodes, to an account.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionSupport, SubtitleFormat, SubtitleTrack, Tag, Variant, clean_title, fetch, hls,
    navigation_headers, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "abc";
const IVIEW_SITE: &str = "https://iview.abc.net.au";
const IVIEW_API: &str = "https://api.iview.abc.net.au/v3/";
const IVIEW_PROGRAMS: &str = "https://iview.abc.net.au/api/programs/";
/// The key the apps sign HLS token requests with.
const IVIEW_SIGN_KEY: &[u8] = b"android.content.res.Resources";
/// The renditions the programs API lists, best first.
const IVIEW_QUALITIES: &[&str] = &["1080", "720", "sd", "sd-low"];

/// `/news|btn|listen/…/{id}` on abc.net.au, the id being five or more digits.
static RE_PAGE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(?:news|btn|listen)/(?:[^/?#]+/){1,4}(\d{5,})/?$").unwrap());
/// `/(…/)video/{id}` on iview.abc.net.au.
static RE_IVIEW_VIDEO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(?:[^/]+/)*video/([^/?#]+)").unwrap());
/// `/show/{slug}` or `/show/{slug}/series/{n}` on iview.abc.net.au.
static RE_IVIEW_SHOW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/show/([^/?#]+)(?:/series/(\d+))?/?$").unwrap());
static RE_YOUTUBE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:https?:)?//(?:www\.)?youtube(?:-nocookie)?\.com/(?:watch\?v=|embed/)([A-Za-z0-9_-]{11})"#)
        .unwrap()
});
/// `_720.mp4` or `_1500k.mp4` at the end of a rendition's link.
static RE_RENDITION_SUFFIX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"_(?:(\d+)|(\d+)k)\.mp4$").unwrap());
/// `<a href="…" data-duration="…" title="Download audio directly">`: the audio file an
/// older Listen page offers.
static RE_AUDIO_ANCHOR: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<a\s+href="([^"]+)"\s+data-duration="(\d+)"\s+title="Download audio directly">"#)
        .unwrap()
});
/// `"sources": [ … ]`, `"files": [ … ]` or `"renditions": [ … ]` in a page's scripts.
static RE_MEDIA_ARRAY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""(?:sources|files|renditions)":\s*(\[[^\]]+\])"#).unwrap());
/// `inlineVideoData.push({…});`, `inlineAudioData.push(…)`, `inlineYouTubeData.push(…)`:
/// the media of older article markup.
static RE_INLINE_DATA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"inline(Video|Audio|YouTube)Data\.push\(([^)]+)\);").unwrap());
/// `class="expired-video">…<span>message</span>`: media the site has taken down.
static RE_EXPIRED: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)class="expired-(?:video|audio)".+?<span>(.+?)</span>"#).unwrap()
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A news, BTN or Listen page carrying a video or audio.
    Page { id: String },
    /// An iview episode or live stream.
    IviewVideo { id: String },
    /// An iview show, or one series of it.
    IviewShow { slug: String, series: Option<u32> },
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let path = url.path();
    match host.as_str() {
        "abc.net.au" | "www.abc.net.au" => RE_PAGE.captures(path).map(|caps| Link::Page {
            id: caps[1].to_string(),
        }),
        "iview.abc.net.au" | "www.iview.abc.net.au" => {
            if let Some(caps) = RE_IVIEW_VIDEO.captures(path) {
                return Some(Link::IviewVideo {
                    id: caps[1].to_string(),
                });
            }
            RE_IVIEW_SHOW.captures(path).map(|caps| Link::IviewShow {
                slug: caps[1].to_string(),
                series: caps.get(2).and_then(|m| m.as_str().parse().ok()),
            })
        }
        _ => None,
    }
}

/// The renditions a page's Next.js data carries, with the object they belong to (the
/// media's document, which names its title and length): the first found, depth first.
pub fn find_renditions(data: &Value) -> Option<(Vec<Value>, Value)> {
    fn walk<'a>(value: &'a Value, owner: &'a Value) -> Option<(Vec<Value>, Value)> {
        match value {
            Value::Object(map) => {
                if let Some(renditions) = map.get("renditions") {
                    let files = match renditions {
                        Value::Array(list) => list.clone(),
                        Value::Object(inner) => inner
                            .get("files")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default(),
                        _ => Vec::new(),
                    };
                    if !files.is_empty() {
                        let document = if map.contains_key("title") || map.contains_key("duration")
                        {
                            value
                        } else {
                            owner
                        };
                        return Some((files, document.clone()));
                    }
                }
                let next_owner = if map.contains_key("title") || map.contains_key("duration") {
                    value
                } else {
                    owner
                };
                map.values().find_map(|child| walk(child, next_owner))
            }
            Value::Array(list) => list.iter().find_map(|child| walk(child, owner)),
            _ => None,
        }
    }
    walk(data, data)
}

/// A rendition as a variant: a video file with its size, or an audio file.
fn rendition_variant(file: &Value) -> Option<Variant> {
    let url = util::url_of(&file["url"], None)?;
    let mime = file["MIMEType"]
        .as_str()
        .or(file["contentType"].as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let audio_only = mime.starts_with("audio/");
    let mut variant = Variant::file(url.clone());
    variant.width = util::u32_of(&file["width"]).filter(|w| *w > 0);
    variant.height = util::u32_of(&file["height"]).filter(|h| *h > 0);
    let kbps = util::uint(&file["bitRate"]).or_else(|| util::uint(&file["bitrate"]));
    if let Some(caps) = RE_RENDITION_SUFFIX.captures(url.path()) {
        if let Some(height) = caps.get(1).and_then(|m| m.as_str().parse().ok()) {
            variant.height = variant.height.or(Some(height));
        }
    }
    variant.bitrate = kbps.filter(|k| *k > 0).map(|k| k * 1000);
    variant.size = util::uint(&file["size"]).or_else(|| util::uint(&file["fileSize"]));
    variant.audio_only = audio_only;
    if audio_only {
        variant.container = Some(Container::Mp3);
        variant.audio = Some(AudioCodec::Mp3);
        variant.label = kbps.map(|k| format!("{k}k"));
    } else {
        variant.container = Some(Container::Mp4);
        variant.video = Some(
            match file["codec"]
                .as_str()
                .unwrap_or("")
                .to_ascii_uppercase()
                .as_str()
            {
                "HEVC" | "H265" => VideoCodec::H265,
                _ => VideoCodec::H264,
            },
        );
        variant.audio = Some(AudioCodec::Aac);
        variant.label = variant.height.map(|h| format!("{h}p"));
    }
    variant.format_id = file["name"].as_str().map(str::to_string);
    Some(variant)
}

/// The media an older page carries outside its Next.js data: a `sources`, `files` or
/// `renditions` array in a script, or the objects `inline{Video,Audio,YouTube}Data.push`
/// is called with. The flag says whether they are YouTube links rather than files.
pub fn legacy_media(html: &str) -> Option<(Vec<Value>, bool)> {
    if let Some(caps) = RE_MEDIA_ARRAY.captures(html)
        && let Some(Value::Array(list)) = util::parse_js(&caps[1])
        && !list.is_empty()
    {
        return Some((list, false));
    }
    let mut files = Vec::new();
    let mut youtube = false;
    for caps in RE_INLINE_DATA.captures_iter(html) {
        let Some(data) = util::parse_js(&caps[2]) else {
            continue;
        };
        youtube |= &caps[1] == "YouTube";
        match data {
            Value::Array(list) => files.extend(list),
            other => files.push(other),
        }
    }
    (!files.is_empty()).then_some((files, youtube))
}

/// Every YouTube video a page links to or embeds, in order, each once.
pub fn youtube_links(html: &str) -> Vec<Url> {
    let mut ids: Vec<String> = Vec::new();
    for caps in RE_YOUTUBE.captures_iter(html) {
        let id = caps[1].to_string();
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    ids.iter()
        .map(|id| Url::parse(&format!("https://www.youtube.com/watch?v={id}")).expect("valid"))
        .collect()
}

/// The message a page shows for media the site has taken down.
pub fn expired_message(html: &str) -> Option<String> {
    RE_EXPIRED
        .captures(html)
        .and_then(|caps| clean_title(&util::clean_html(&caps[1])))
}

/// The YouTube videos a page embeds: one is handed to the YouTube resolver, several are
/// a playlist.
fn youtube_result(
    url: &Url,
    mut entries: Vec<PlaylistEntry>,
    title: Option<String>,
) -> Result<Resolution, ResolveError> {
    match entries.len() {
        0 => Err(ResolveError::NotFound(url.clone())),
        1 => Err(ResolveError::Redirect(entries.remove(0).url)),
        count => Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.into(),
            id: parse_link(url).and_then(|link| match link {
                Link::Page { id } => Some(id),
                _ => None,
            }),
            title,
            entries,
            total: Some(count),
        })),
    }
}

/// The Next.js data a page carries.
fn next_data(page: &Page) -> Option<Value> {
    let json = util::element_by_id(page.html(), "__NEXT_DATA__")?;
    serde_json::from_str(json.trim()).ok()
}

/// The signed request for an episode's HLS token, as the apps make it.
pub fn sign_path(house_number: &str, timestamp: i64) -> String {
    let path = format!("/auth/hls/sign?ts={timestamp}&hn={house_number}&d=android-tablet");
    let signature = hex::encode(util::hmac_sha256(IVIEW_SIGN_KEY, path.as_bytes()));
    format!("{path}&sig={signature}")
}

/// The largest 16:9 image of an iview entry.
fn iview_thumbnail(entry: &Value) -> Option<Url> {
    let images = entry["images"].as_array()?;
    images
        .iter()
        .filter(|image| {
            image["aspectRatio"]
                .as_str()
                .is_none_or(|ratio| ratio == "16:9")
        })
        .max_by_key(|image| util::uint(&image["width"]).unwrap_or(0))
        .or(images.first())
        .and_then(|image| util::url_of(&image["url"], None))
        .or_else(|| util::url_of(&entry["thumbnail"], None))
}

/// An iview episode entry as a playlist entry.
fn iview_entry(item: &Value) -> Option<PlaylistEntry> {
    let url = util::url_of(&item["shareUrl"], None)
        .or_else(|| {
            item["_links"]["deeplink"]["href"]
                .as_str()
                .and_then(|href| Url::parse(IVIEW_SITE).ok()?.join(href).ok())
        })
        .or_else(|| {
            util::text(&item["id"])
                .and_then(|id| Url::parse(&format!("{IVIEW_SITE}/video/{id}")).ok())
        })?;
    Some(PlaylistEntry {
        url,
        title: item["displaySubtitle"]
            .as_str()
            .or(item["title"].as_str())
            .and_then(clean_title),
        duration: util::seconds(&item["duration"]),
    })
}

pub struct AbcResolver {
    http: Http,
}

impl AbcResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    async fn json(&self, url: &Url, origin: &Url) -> Result<Value, ResolveError> {
        let accept = [("accept".to_string(), "application/json".to_string())];
        let fetched = fetch(&self.http, url, PLATFORM, BROWSER_UA, &accept, MAX_PAGE).await?;
        if let Some(error) = status_error(fetched.status, origin) {
            return Err(error);
        }
        fetched.json(origin)
    }

    async fn resolve_page(&self, id: &str, url: &Url) -> Result<Resolution, ResolveError> {
        let fetched = fetch(
            &self.http,
            url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        let html = fetched.text();
        let page = Page::parse(&html, url);
        let data = next_data(&page);
        let (files, document) = match data.as_ref().and_then(find_renditions) {
            Some(found) => found,
            None => {
                if let Some(caps) = RE_AUDIO_ANCHOR.captures(&html) {
                    let file = serde_json::json!({
                        "url": util::html_unescape(&caps[1]),
                        "MIMEType": "audio/mpeg",
                    });
                    let document = serde_json::json!({"duration": caps[2].parse::<u64>().ok()});
                    (vec![file], document)
                } else {
                    let youtube = youtube_links(&html);
                    let legacy = legacy_media(&html);
                    match legacy {
                        Some((links, true)) => {
                            let entries: Vec<PlaylistEntry> = links
                                .iter()
                                .filter_map(|link| util::url_of(&link["url"], Some(url)))
                                .map(|link| PlaylistEntry {
                                    url: link,
                                    title: None,
                                    duration: None,
                                })
                                .collect();
                            return youtube_result(url, entries, page.title());
                        }
                        Some((files, false)) => (files, Value::Null),
                        None if !youtube.is_empty() => {
                            let entries = youtube
                                .into_iter()
                                .map(|link| PlaylistEntry {
                                    url: link,
                                    title: None,
                                    duration: None,
                                })
                                .collect();
                            return youtube_result(url, entries, page.title());
                        }
                        None => {
                            if let Some(message) = expired_message(&html) {
                                return Err(ResolveError::unavailable(url, message));
                            }
                            return Err(ResolveError::NotFound(url.clone()));
                        }
                    }
                }
            }
        };
        let mut variants: Vec<Variant> = files.iter().filter_map(rendition_variant).collect();
        if variants.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        variants
            .sort_by_key(|v| std::cmp::Reverse((v.height.unwrap_or(0), v.bitrate.unwrap_or(0))));
        // The page's renditions say what it carries: audio files alone make it audio.
        let media = if variants.iter().all(|v| v.audio_only) {
            MediaKind::Audio
        } else {
            MediaKind::Video
        };
        let mut resolved = Resolved::of(PLATFORM, media);
        resolved.id = Some(id.to_string());
        resolved.title = document["title"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| page.meta("og:title").and_then(|t| clean_title(&t)))
            .or_else(|| page.title());
        resolved.description = document["synopsis"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| page.meta("og:description").and_then(|d| clean_title(&d)));
        resolved.thumbnail = page
            .meta("og:image")
            .and_then(|t| util::join_url(Some(url), &t));
        resolved.duration = util::seconds(&document["duration"]);
        resolved.uploaded_at = util::time(&document["dates"]["published"]).or_else(|| {
            page.meta("article:published_time")
                .and_then(|t| util::parse_timestamp(&t))
        });
        resolved.uploader = Some("ABC".to_string());
        resolved.webpage_url = Some(url.clone());
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn resolve_iview_video(&self, id: &str, url: &Url) -> Result<Resolution, ResolveError> {
        let entry_url = Url::parse(&format!("{IVIEW_API}video/{id}")).expect("valid");
        let entry = self.json(&entry_url, url).await?;
        let house_number = util::text(&entry["houseNumber"]).unwrap_or_else(|| id.to_string());
        let title = entry["title"].as_str().or(entry["seriesTitle"].as_str());
        let title = title.map(util::html_unescape).and_then(|t| clean_title(&t));
        let live = entry["type"].as_str() == Some("livestream")
            || entry["livestream"].as_str() == Some("1");

        // The token the streams are played with; the signing endpoint says why it refuses.
        let sign_url = Url::parse(&format!(
            "{IVIEW_SITE}{}",
            sign_path(&house_number, jiff::Timestamp::now().as_second())
        ))
        .expect("valid");
        let signed = fetch(&self.http, &sign_url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        let token = signed.text();
        if !signed.status.is_success() {
            let reason = token.trim();
            if reason.contains("international") || reason.contains("outside") {
                return Err(ResolveError::unavailable(url, "available only in AU"));
            }
            if entry["requiresLogin"].as_bool() == Some(true) || signed.status.as_u16() == 401 {
                return Err(ResolveError::login_required(
                    url,
                    PLATFORM,
                    format!("iview refused a playback token: {reason}"),
                ));
            }
            return Err(status_error(signed.status, url)
                .unwrap_or_else(|| ResolveError::unavailable(url, reason.to_string())));
        }
        let token = token.trim().to_string();

        let programs_url = Url::parse(&format!("{IVIEW_PROGRAMS}{id}")).expect("valid");
        let program = self.json(&programs_url, url).await?;
        let Some(stream) = program["playlist"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|entry| matches!(entry["type"].as_str(), Some("program") | Some("livestream")))
        else {
            let message = entry["unavailableMessage"].as_str().unwrap_or("");
            if message.contains("outside Australia") {
                return Err(ResolveError::unavailable(url, "available only in AU"));
            }
            if entry["requiresLogin"].as_bool() == Some(true) {
                return Err(ResolveError::login_required(
                    url,
                    PLATFORM,
                    "the episode plays only for an ABC account",
                ));
            }
            return Err(ResolveError::NotFound(url.clone()));
        };
        let mut resolved = Resolved::new(PLATFORM);
        let mut failure = None;
        for quality in IVIEW_QUALITIES {
            let Some(playlist) = util::url_of(&stream["streams"]["hls"][*quality], None) else {
                continue;
            };
            let playlist = util::with_query(&playlist, &[("hdnea", token.as_str())]);
            match hls::expand(&self.http, &playlist, PLATFORM, BROWSER_UA, &[]).await {
                Ok(expanded) => {
                    resolved.variants = expanded.variants;
                    resolved.subtitles = expanded.subtitles;
                    resolved.duration = expanded.duration;
                    resolved.live |= expanded.live;
                    break;
                }
                Err(error) => failure = Some(error),
            }
        }
        if resolved.variants.is_empty() {
            return Err(failure.unwrap_or_else(|| ResolveError::NotFound(url.clone())));
        }
        if let Some(captions) = util::url_of(&stream["captions"]["src-vtt"], None) {
            resolved.subtitles.push(SubtitleTrack {
                url: captions,
                language: "en".to_string(),
                name: Some("English".to_string()),
                format: SubtitleFormat::Vtt,
                auto: false,
                headers: Vec::new(),
            });
        }
        resolved.id = Some(id.to_string());
        resolved.title = title;
        resolved.description = entry["description"].as_str().and_then(clean_title);
        resolved.thumbnail =
            iview_thumbnail(&entry).or_else(|| util::url_of(&program["thumbnail"], None));
        resolved.duration = util::seconds(&entry["duration"])
            .or_else(|| util::seconds(&program["eventDuration"]))
            .or(resolved.duration);
        resolved.uploaded_at = entry["pubDate"]
            .as_str()
            .or(program["pubDate"].as_str())
            .and_then(|date| util::parse_timestamp(&date.replacen(' ', "T", 1)));
        resolved.uploader = entry["channelTitle"].as_str().and_then(clean_title);
        resolved.live = resolved.live || live;
        resolved.webpage_url = util::url_of(&entry["shareUrl"], None).or_else(|| Some(url.clone()));
        Ok(Resolution::from(resolved))
    }

    async fn resolve_iview_show(
        &self,
        slug: &str,
        series: Option<u32>,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let (series_entry, show_title) = match series {
            Some(number) => {
                let api = Url::parse(&format!("{IVIEW_API}series/{slug}/{number}")).expect("valid");
                let entry = self.json(&api, url).await?;
                let show = entry["showTitle"].as_str().and_then(clean_title);
                (entry, show)
            }
            None => {
                let api = util::with_query(
                    &Url::parse(&format!("{IVIEW_API}show/{slug}")).expect("valid"),
                    &[("embed", "seriesList,selectedSeries")],
                );
                let show = self.json(&api, url).await?;
                let show_title = show["title"].as_str().and_then(clean_title);
                let selected = show["_embedded"]["selectedSeries"].clone();
                if selected.is_null() {
                    return Err(ResolveError::NotFound(url.clone()));
                }
                (selected, show_title)
            }
        };
        let items = series_entry["_embedded"]["videoEpisodes"]["items"]
            .as_array()
            .or_else(|| series_entry["_embedded"]["videoEpisodes"].as_array())
            .cloned()
            .unwrap_or_default();
        let entries: Vec<PlaylistEntry> = items.iter().filter_map(iview_entry).collect();
        if entries.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let series_title = series_entry["title"]
            .as_str()
            .or(series_entry["displaySubtitle"].as_str())
            .and_then(clean_title);
        let title = match (show_title, series_title) {
            (Some(show), Some(series)) if show != series => Some(format!("{show}: {series}")),
            (Some(show), _) => Some(show),
            (None, series) => series,
        };
        let total = entries.len();
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: util::text(&series_entry["id"]).or_else(|| Some(slug.to_string())),
            title,
            entries,
            total: util::uint(&series_entry["episodeCount"])
                .map(|n| n as usize)
                .or(Some(total)),
        }))
    }
}

#[async_trait]
impl Resolver for AbcResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "ABC (Australia)",
            hosts: &["abc.net.au", "iview.abc.net.au"],
            features: &["videos", "audio", "shows", "series", "live"],
            formats: &["mp4", "mp3", "hls"],
            media: &[MediaKind::Video, MediaKind::Audio],
            tags: &[Tag::News, Tag::Video],
            session: SessionSupport::Optional,
            examples: &[
                "https://www.abc.net.au/news/2026-09-13/wa-government-to-build-new-rental-apartment-in-cbd/107148268",
                "https://www.abc.net.au/listen/programs/the-followers-madness-of-two/presents-followers-madness-of-two/105697646",
                "https://iview.abc.net.au/show/utopia",
                "https://iview.abc.net.au/show/utopia/series/1",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Page { id } => self.resolve_page(&id, url).await,
            Link::IviewVideo { id } => self.resolve_iview_video(&id, url).await,
            Link::IviewShow { slug, series } => self.resolve_iview_show(&slug, series, url).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

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

    const NEWS: &str = "https://www.abc.net.au/news/2026-09-13/wa-government-to-build-new-rental-apartment-in-cbd/107148268";

    fn news_page() -> String {
        let data = json!({"props": {"pageProps": {"document": {"loaders": {"articledetail": {"headlinePrepared": {
        "featureMediaPrepared": {"heroContent": {"descriptor": {"props": {"document": {
            "id": "107148312", "title": "Social housing tower", "synopsis": "The WA government will build it.",
            "duration": 59, "dates": {"published": "2026-09-13T02:00:00+00:00"},
            "media": {"video": {"renditions": {"files": [
                {"DeliveryType": "DOWNLOAD", "MIMEType": "video/mp4", "bitRate": 15000, "codec": "AVC", "height": 1080, "name": "SocialRentalApp_1309.mp4", "size": 19856818, "url": "https://mediacore-live-production.akamaized.net/video/02/oe/Z/ud.mp4", "width": 1920},
                {"DeliveryType": "DOWNLOAD", "MIMEType": "video/mp4", "bitRate": 1500, "codec": "AVC", "name": "small_720.mp4", "url": "https://mediacore-live-production.akamaized.net/video/02/oe/Z/ud_720.mp4"}
            ]}}}
        }}}}}}}}}}}});
        format!(
            r#"<html><head><meta property="og:title" content="WA government to build new rental apartment"><meta property="og:description" content="Desc"><meta property="og:image" content="https://live-production.wcms.abc-cdn.net.au/c33bb5?width=862"></head><body><script id="__NEXT_DATA__" type="application/json">{data}</script></body></html>"#
        )
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link(NEWS),
            Some(Link::Page {
                id: "107148268".into()
            })
        );
        assert_eq!(
            link("https://www.abc.net.au/btn/classroom/wwi-centenary/10527914"),
            Some(Link::Page {
                id: "10527914".into()
            })
        );
        assert_eq!(
            link(
                "https://www.abc.net.au/listen/programs/the-followers-madness-of-two/presents-followers-madness-of-two/105697646"
            ),
            Some(Link::Page {
                id: "105697646".into()
            })
        );
        assert_eq!(
            link("http://www.abc.net.au/news/2015-10-19/6866214"),
            Some(Link::Page {
                id: "6866214".into()
            })
        );
        assert_eq!(link("https://www.abc.net.au/news/"), None);
        assert_eq!(
            link("https://iview.abc.net.au/show/utopia/series/1/video/CO1211V001S00"),
            Some(Link::IviewVideo {
                id: "CO1211V001S00".into()
            })
        );
        assert_eq!(
            link("https://iview.abc.net.au/video/NC2203H039S00"),
            Some(Link::IviewVideo {
                id: "NC2203H039S00".into()
            })
        );
        assert_eq!(
            link("https://iview.abc.net.au/show/utopia"),
            Some(Link::IviewShow {
                slug: "utopia".into(),
                series: None
            })
        );
        assert_eq!(
            link("https://iview.abc.net.au/show/utopia/series/2"),
            Some(Link::IviewShow {
                slug: "utopia".into(),
                series: Some(2)
            })
        );
        assert_eq!(link("https://iview.abc.net.au/channel/abc1"), None);
        assert_eq!(link("https://example.com/news/2015-10-19/6866214"), None);
    }

    #[test]
    fn token_requests_are_signed_as_the_apps_do() {
        let path = sign_path("CO1211V001S00", 1700000000);
        assert!(
            path.starts_with("/auth/hls/sign?ts=1700000000&hn=CO1211V001S00&d=android-tablet&sig=")
        );
        let sig = path.rsplit("sig=").next().unwrap();
        assert_eq!(sig.len(), 64);
        assert_eq!(
            sig,
            hex::encode(util::hmac_sha256(
                b"android.content.res.Resources",
                b"/auth/hls/sign?ts=1700000000&hn=CO1211V001S00&d=android-tablet"
            ))
        );
    }

    #[tokio::test]
    async fn news_pages_resolve_to_their_renditions() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture
            .exchanges
            .push(get(NEWS, 200, "text/html", news_page()));
        let resolver = AbcResolver::new(Http::replay(fixture));
        let url = Url::parse(NEWS).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("107148268"));
        assert_eq!(resolved.title.as_deref(), Some("Social housing tower"));
        assert_eq!(
            resolved.description.as_deref(),
            Some("The WA government will build it.")
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(59)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1789264800)
        );
        assert_eq!(resolved.variants.len(), 2);
        let best = &resolved.variants[0];
        assert_eq!(best.height, Some(1080));
        assert_eq!(best.width, Some(1920));
        assert_eq!(best.bitrate, Some(15_000_000));
        assert_eq!(best.size, Some(19856818));
        assert_eq!(best.container, Some(Container::Mp4));
        assert_eq!(resolved.media, MediaKind::Video);
        assert_eq!(best.video, Some(VideoCodec::H264));
        assert_eq!(
            resolved.variants[1].height,
            Some(720),
            "the height in the file name"
        );
    }

    #[tokio::test]
    async fn listen_pages_resolve_to_audio() {
        let page_url = "https://www.abc.net.au/listen/programs/the-followers-madness-of-two/presents-followers-madness-of-two/105697646";
        let data = json!({"props": {"pageProps": {"data": {"documentProps": {
            "title": "Presents: The Followers", "duration": 1871,
            "renditions": [{"bitrate": 192, "codec": "MPEG Audio", "MIMEType": "audio/mpeg", "fileSize": 4989272, "height": null, "url": "https://mediacore-live-production.akamaized.net/audio/02/cj/Z/gr.mp3"}]
        }}}}});
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(page_url, 200, "text/html", format!(
            r#"<html><head><meta property="og:title" content="The Followers"></head><body><script id="__NEXT_DATA__" type="application/json">{data}</script></body></html>"#
        )));
        let resolver = AbcResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse(page_url).unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.title.as_deref(), Some("Presents: The Followers"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(1871)));
        assert_eq!(resolved.variants.len(), 1);
        let audio = &resolved.variants[0];
        assert!(audio.audio_only);
        assert_eq!(audio.container, Some(Container::Mp3));
        assert_eq!(resolved.media, MediaKind::Audio);
        assert_eq!(audio.bitrate, Some(192_000));
        assert_eq!(audio.size, Some(4989272));
    }

    #[tokio::test]
    async fn pages_without_media_hand_youtube_on_or_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.abc.net.au/news/2015-10-19/6866214",
            200,
            "text/html",
            r#"<html><body><iframe width="100%" src="//www.youtube-nocookie.com/embed/dQw4w9WgXcQ?rel=0"></iframe></body></html>"#.into(),
        ));
        fixture.exchanges.push(get(
            "https://www.abc.net.au/news/2015-10-19/6866215",
            200,
            "text/html",
            "<html><body><p>Just words.</p></body></html>".into(),
        ));
        let resolver = AbcResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://www.abc.net.au/news/2015-10-19/6866214").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Redirect(u) if u.as_str() == "https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            "{error}"
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.abc.net.au/news/2015-10-19/6866215").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn iview_shows_and_series_list_their_episodes() {
        let episode = |id: &str, title: &str| {
            json!({"id": id, "title": format!("S1 {title}"), "displaySubtitle": title, "duration": 1584,
                   "shareUrl": format!("https://iview.abc.net.au/show/utopia/series/1/video/{id}"),
                   "_links": {"deeplink": {"href": format!("/show/utopia/series/1/video/{id}")}}})
        };
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://api.iview.abc.net.au/v3/show/utopia?embed=seriesList%2CselectedSeries",
            200,
            "application/json",
            json!({"id": 124124, "slug": "utopia", "title": "Utopia", "_embedded": {
                "seriesList": [{"id": "124124-1", "title": "Season 1"}],
                "selectedSeries": {"id": "124124-1", "title": "Season 1", "episodeCount": 2, "_embedded": {"videoEpisodes": {"items": [
                    episode("CO1211V001S00", "Episode 1 Wood For The Trees"), episode("CO1211V002S00", "Episode 2 Arts And Minds")
                ]}}}
            }}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.iview.abc.net.au/v3/series/utopia/2",
            200,
            "application/json",
            json!({"id": "124124-2", "title": "Season 2", "showTitle": "Utopia", "episodeCount": 1,
                   "_embedded": {"videoEpisodes": {"items": [{"id": "CO1211V003S00", "title": "S2 Episode 1", "_links": {"deeplink": {"href": "/show/utopia/series/2/video/CO1211V003S00"}}}]}}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.iview.abc.net.au/v3/show/nothing?embed=seriesList%2CselectedSeries",
            404,
            "application/json",
            json!({"status": "error", "code": 404}).to_string(),
        ));
        let resolver = AbcResolver::new(Http::replay(fixture));
        let Resolution::Playlist(show) = resolver
            .resolve(&Url::parse("https://iview.abc.net.au/show/utopia").unwrap())
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(show.title.as_deref(), Some("Utopia: Season 1"));
        assert_eq!(show.id.as_deref(), Some("124124-1"));
        assert_eq!(show.total, Some(2));
        assert_eq!(show.entries.len(), 2);
        assert_eq!(
            show.entries[0].url.as_str(),
            "https://iview.abc.net.au/show/utopia/series/1/video/CO1211V001S00"
        );
        assert_eq!(
            show.entries[1].title.as_deref(),
            Some("Episode 2 Arts And Minds")
        );
        assert_eq!(show.entries[1].duration, Some(Duration::from_secs(1584)));
        let Resolution::Playlist(series) = resolver
            .resolve(&Url::parse("https://iview.abc.net.au/show/utopia/series/2").unwrap())
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(series.title.as_deref(), Some("Utopia: Season 2"));
        assert_eq!(
            series.entries[0].url.as_str(),
            "https://iview.abc.net.au/show/utopia/series/2/video/CO1211V003S00"
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://iview.abc.net.au/show/nothing").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn iview_episodes_play_with_a_signed_token_or_say_why_not() {
        let entry = |requires_login: bool| {
            json!({"id": "CO1211V001S00", "houseNumber": "CO1211V001S00", "type": "episode", "title": "S1 Episode 1 Wood For The Trees",
                   "seriesTitle": "Utopia", "channelTitle": "ABC TV", "description": "Rhonda redesigns a logo.", "duration": 1584,
                   "pubDate": "2025-02-18 08:00:00", "requiresLogin": requires_login, "shareUrl": "https://iview.abc.net.au/show/utopia/series/1/video/CO1211V001S00",
                   "images": [{"url": "https://cdn.iview.abc.net.au/thumbs/i/small.jpg", "aspectRatio": "16:9", "width": 640},
                              {"url": "https://cdn.iview.abc.net.au/thumbs/i/big.jpg", "aspectRatio": "16:9", "width": 1280},
                              {"url": "https://cdn.iview.abc.net.au/thumbs/i/portrait.jpg", "aspectRatio": "2:3", "width": 1440}]})
        };
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://api.iview.abc.net.au/v3/video/CO1211V001S00",
            200,
            "application/json",
            entry(false).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://iview.abc.net.au/auth/hls/sign",
            200,
            "text/plain",
            "tok3n".into(),
        ));
        fixture.exchanges.push(get(
            "https://iview.abc.net.au/api/programs/CO1211V001S00",
            200,
            "application/json",
            json!({"playlist": [
                {"type": "preroll", "streams": {"hls": {"720": "https://ads.test/x.m3u8"}}},
                {"type": "program", "streams": {"hls": {"720": "https://iviewhls.akamaized.net/i/co/CO1211V001S00/master.m3u8"}},
                 "captions": {"src-vtt": "https://iview.abc.net.au/cc/co/CO1211V001S00.vtt"}}
            ]}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://iviewhls.akamaized.net/i/co/CO1211V001S00/master.m3u8?hdnea=tok3n",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1280x720\n720.m3u8?hdnea=tok3n\n".into(),
        ));
        fixture.exchanges.push(get(
            "https://iviewhls.akamaized.net/i/co/CO1211V001S00/720.m3u8?hdnea=tok3n",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:8.0,\n0.ts\n#EXT-X-ENDLIST\n".into(),
        ));
        // A second episode, refused for the caller's region.
        fixture.exchanges.push(get(
            "https://api.iview.abc.net.au/v3/video/CO1211V002S00",
            200,
            "application/json",
            entry(true).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://iview.abc.net.au/auth/hls/sign",
            401,
            "text/plain",
            "episode-not-cleared-for-international-viewing".into(),
        ));
        // A third, refused for want of an account.
        fixture.exchanges.push(get(
            "https://api.iview.abc.net.au/v3/video/CO1211V003S00",
            200,
            "application/json",
            entry(true).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://iview.abc.net.au/auth/hls/sign",
            401,
            "text/plain",
            "login-required".into(),
        ));
        let resolver = AbcResolver::new(Http::replay(fixture));
        let resolve = |id: &str| {
            let url = Url::parse(&format!(
                "https://iview.abc.net.au/show/utopia/series/1/video/{id}"
            ))
            .unwrap();
            let resolver = &resolver;
            async move { resolver.resolve(&url).await }
        };
        let resolved = resolve("CO1211V001S00").await.unwrap().media().unwrap();
        assert_eq!(
            resolved.title.as_deref(),
            Some("S1 Episode 1 Wood For The Trees")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("ABC TV"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(1584)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1739865600)
        );
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://cdn.iview.abc.net.au/thumbs/i/big.jpg"
        );
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].height, Some(720));
        assert!(resolved.variants[0].url.as_str().contains("hdnea=tok3n"));
        assert_eq!(resolved.subtitles.len(), 1);
        assert_eq!(resolved.subtitles[0].format, SubtitleFormat::Vtt);
        let geo = resolve("CO1211V002S00").await.unwrap_err();
        assert!(
            matches!(&geo, ResolveError::Unavailable { reason, .. } if reason == "available only in AU"),
            "{geo}"
        );
        assert!(matches!(
            resolve("CO1211V003S00").await.unwrap_err(),
            ResolveError::LoginRequired { .. }
        ));
    }

    #[tokio::test]
    async fn older_pages_carry_their_media_in_scripts_anchors_and_youtube_embeds() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.abc.net.au/news/2015-08-18/legacy/6704570",
            200,
            "text/html",
            r#"<html><script>var x = {"sources": [{"url": "https://mediacore.abc.net.au/v/2015/08/x_360.mp4", "contentType": "video/mp4", "width": 640, "height": 360, "bitrate": 500}, {"url": "https://mediacore.abc.net.au/v/2015/08/x_720.mp4", "contentType": "video/mp4", "width": 1280, "height": 720, "bitrate": 1500}]};</script><title>Legacy story - ABC News</title></html>"#.into(),
        ));
        fixture.exchanges.push(get(
            "https://www.abc.net.au/news/2015-08-18/inline/6704571",
            200,
            "text/html",
            r#"<html><script>inlineVideoData.push({"url": "https://mediacore.abc.net.au/v/2015/08/y_1000k.mp4", "contentType": "video/mp4", "width": 1024, "height": 576});</script></html>"#.into(),
        ));
        fixture.exchanges.push(get(
            "https://www.abc.net.au/listen/programs/legacy/audio/6704572",
            200,
            "text/html",
            r#"<html><a href="https://abcmedia.akamaized.net/rn/podcast/2015/08/z.mp3" data-duration="1234" title="Download audio directly">Download</a></html>"#.into(),
        ));
        fixture.exchanges.push(get(
            "https://www.abc.net.au/news/2015-08-18/two-videos/6704573",
            200,
            "text/html",
            r#"<html><iframe src="//www.youtube-nocookie.com/embed/aaaaaaaaaaa?rel=0"></iframe><p><a href="http://www.youtube.com/watch?v=bbbbbbbbbbb"><span><strong>External Link:</strong></span></a></p><title>Two - ABC News</title></html>"#.into(),
        ));
        fixture.exchanges.push(get(
            "https://www.abc.net.au/news/2015-08-18/one-video/6704574",
            200,
            "text/html",
            r#"<html><script>inlineYouTubeData.push({"url": "https://www.youtube.com/watch?v=ccccccccccc"});</script></html>"#.into(),
        ));
        fixture.exchanges.push(get(
            "https://www.abc.net.au/news/2015-08-18/expired/6704575",
            200,
            "text/html",
            r#"<html><div class="expired-video"><span>This video has expired.</span></div></html>"#
                .into(),
        ));
        let resolver = AbcResolver::new(Http::replay(fixture));
        let page = |s: &str| Url::parse(s).unwrap();

        let legacy = resolver
            .resolve(&page(
                "https://www.abc.net.au/news/2015-08-18/legacy/6704570",
            ))
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(legacy.id.as_deref(), Some("6704570"));
        assert_eq!(legacy.title.as_deref(), Some("Legacy story - ABC News"));
        assert_eq!(legacy.variants.len(), 2);
        assert_eq!(legacy.media, MediaKind::Video);
        assert_eq!(legacy.variants[0].height, Some(720));
        assert_eq!(legacy.variants[0].bitrate, Some(1_500_000));

        let inline = resolver
            .resolve(&page(
                "https://www.abc.net.au/news/2015-08-18/inline/6704571",
            ))
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(inline.variants.len(), 1);
        assert_eq!(inline.variants[0].height, Some(576));

        let audio = resolver
            .resolve(&page(
                "https://www.abc.net.au/listen/programs/legacy/audio/6704572",
            ))
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(audio.duration, Some(Duration::from_secs(1234)));
        assert_eq!(audio.variants.len(), 1);
        assert!(audio.variants[0].audio_only);
        assert_eq!(audio.media, MediaKind::Audio);
        assert_eq!(
            audio.variants[0].url.as_str(),
            "https://abcmedia.akamaized.net/rn/podcast/2015/08/z.mp3"
        );

        let Resolution::Playlist(two) = resolver
            .resolve(&page(
                "https://www.abc.net.au/news/2015-08-18/two-videos/6704573",
            ))
            .await
            .unwrap()
        else {
            panic!("two embeds are a playlist");
        };
        assert_eq!(two.id.as_deref(), Some("6704573"));
        assert_eq!(
            two.entries
                .iter()
                .map(|e| e.url.as_str())
                .collect::<Vec<_>>(),
            vec![
                "https://www.youtube.com/watch?v=aaaaaaaaaaa",
                "https://www.youtube.com/watch?v=bbbbbbbbbbb"
            ]
        );

        assert!(matches!(
            resolver
                .resolve(&page("https://www.abc.net.au/news/2015-08-18/one-video/6704574"))
                .await
                .unwrap_err(),
            ResolveError::Redirect(to) if to.as_str() == "https://www.youtube.com/watch?v=ccccccccccc"
        ));

        assert!(matches!(
            resolver
                .resolve(&page("https://www.abc.net.au/news/2015-08-18/expired/6704575"))
                .await
                .unwrap_err(),
            ResolveError::Unavailable { reason, .. } if reason == "This video has expired."
        ));
    }
}
