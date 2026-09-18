//! Bandcamp tracks and albums: every page carries the release in its `data-tralbum`
//! attribute, with each track's streaming MP3, and a track offered as a free download
//! lists its lossless and high-bitrate files on its download page; an album page lists
//! its tracks, each a page of its own, an artist's page lists their discography, and
//! the Bandcamp Weekly radio shows stream through the player API.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionSupport, Tag, Variant, clean_title, fetch_as_browser, navigation_headers,
    status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind};

pub const PLATFORM: &str = "bandcamp";

const PLAYER_API: &str = "https://bandcamp.com/api/player/2/player_data_web";

static RE_HOST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:([a-z0-9-]+)\.)?bandcamp\.com$").unwrap());
static RE_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(track|album)/([^/?#]+)/?$").unwrap());
/// `mp3-128`: the encoding a streaming file is keyed by.
static RE_ENCODING: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^([a-z0-9]+)-(\d+)$").unwrap());
/// `<li data-item-id="…"><a href="/album/x">`: a release in a discography (merch items
/// are listed the same way and left out by their path).
static RE_DISCOGRAPHY_ITEM: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<li data-item-id=["'][^>]+>\s*<a href=["']([^"']+)"#).unwrap());
/// `<div class="trackTitle" href="/track/x">`: a release of an older discography layout.
static RE_DISCOGRAPHY_TRACK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<div[^>]+trackTitle["'][^"']+["']([^"']+)"#).unwrap());
/// `<h3 class="albumTitle">… by <span><a href="…">Artist</a>`: the album's artist.
static RE_ALBUM_ARTIST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<h3 class="albumTitle">[\S\s]*?by\s*<span>\s*<a href="[^>]+">\s*([^>]+?)\s*</a>"#)
        .unwrap()
});
/// `<meta property="og:url" content="https://x.bandcamp.com/track/y">`: how other pages
/// embed a release.
static RE_OG_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<meta[^>]+property=["']og:url["'][^>]+content=["'](https?://[^/"']+\.bandcamp\.com/(?:track|album)/[^"'?#]+)["']"#)
        .unwrap()
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Track {
        artist: String,
        slug: String,
    },
    /// An album, on an artist's subdomain or on the main site.
    Album {
        artist: Option<String>,
        slug: String,
    },
    /// A Bandcamp Weekly radio show.
    Weekly {
        show: u64,
    },
    /// An artist's discography.
    User {
        artist: String,
    },
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let artist = RE_HOST
        .captures(&host)?
        .get(1)
        .map(|m| m.as_str().to_string())
        .filter(|a| a != "www");
    let path = url.path().trim_end_matches('/');
    if path == "/radio" {
        let show = util::query_param(url, "show")?.parse().ok()?;
        return Some(Link::Weekly { show });
    }
    if let Some(caps) = RE_PATH.captures(url.path()) {
        let slug = caps[2].to_string();
        return Some(match &caps[1] {
            "track" => Link::Track {
                artist: artist?,
                slug,
            },
            _ => Link::Album { artist, slug },
        });
    }
    if matches!(path, "" | "/music") {
        return artist.map(|artist| Link::User { artist });
    }
    None
}

/// The JSON a page carries in its `data-{attr}` attribute, HTML-escaped: `tralbum` for
/// the release, `embed` for the player, `blob` on a download page.
pub fn data_attr(html: &str, attr: &str) -> Option<Value> {
    let re = Regex::new(&format!(r#"data-{attr}=(?:"([^"]+)"|'([^']+)')"#)).ok()?;
    let caps = re.captures(html)?;
    let escaped = caps.get(1).or_else(|| caps.get(2))?.as_str();
    serde_json::from_str(&util::html_unescape(escaped)).ok()
}

/// The release a page carries: `data-tralbum="{…}"`, HTML-escaped.
pub fn tralbum(html: &str) -> Option<Value> {
    data_attr(html, "tralbum")
}

/// The releases an artist's page lists, as paths or links, each once.
pub fn discography_of(html: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    let mut push = |item: String| {
        if !found.contains(&item) {
            found.push(item);
        }
    };
    let items: Vec<String> = RE_DISCOGRAPHY_ITEM
        .captures_iter(html)
        .map(|c| c[1].to_string())
        .filter(|item| !item.contains("/merch"))
        .collect();
    let items = if items.is_empty() {
        RE_DISCOGRAPHY_TRACK
            .captures_iter(html)
            .map(|c| c[1].to_string())
            .collect()
    } else {
        items
    };
    for item in items {
        push(util::html_unescape(&item));
    }
    let grid_tag = html.find(r#"id="music-grid""#).and_then(|start| {
        let tag_start = html[..start].rfind('<')?;
        let tag_end = html[tag_start..].find('>')? + tag_start + 1;
        Some(html[tag_start..tag_end].to_string())
    });
    if let Some(grid) = grid_tag {
        let attributes = util::extract_attributes(&grid);
        if let Some(items) = attributes
            .iter()
            .find(|(name, _)| name == "data-client-items")
            .and_then(|(_, value)| serde_json::from_str::<Value>(&util::html_unescape(value)).ok())
        {
            for item in items.as_array().into_iter().flatten() {
                if let Some(page) = item["page_url"].as_str() {
                    push(page.to_string());
                }
            }
        }
    }
    found
}

/// The container an encoding name or file extension names.
fn audio_container(name: &str) -> Container {
    Container::from_extension(name).unwrap_or_else(|| Container::Other(name.to_ascii_lowercase()))
}

/// The codec an encoding name names.
fn audio_codec(codec: &str) -> AudioCodec {
    match codec {
        "mp3" => AudioCodec::Mp3,
        "aac" | "m4a" => AudioCodec::Aac,
        "flac" => AudioCodec::Flac,
        "vorbis" | "ogg" => AudioCodec::Vorbis,
        "opus" => AudioCodec::Opus,
        other => AudioCodec::Other(other.to_string()),
    }
}

/// A track's streaming files, one variant per encoding.
fn track_variants(track: &Value) -> Vec<Variant> {
    let mut variants = Vec::new();
    for (encoding, link) in track["file"].as_object().into_iter().flatten() {
        let Some(link) = util::url_of(link, None) else {
            continue;
        };
        let mut variant = Variant::file(link);
        variant.audio_only = true;
        let (codec, kbps) = RE_ENCODING
            .captures(encoding)
            .map(|caps| (caps[1].to_string(), caps[2].parse::<u64>().ok()))
            .unwrap_or((encoding.clone(), None));
        variant.container = Some(audio_container(&codec));
        variant.audio = Some(audio_codec(&codec));
        variant.bitrate = kbps.map(|k| k * 1000);
        variant.duration = util::seconds(&track["duration"]);
        variant.format_id = Some(encoding.clone());
        variant.label = Some(encoding.clone());
        variants.push(variant);
    }
    variants
}

/// The cover art a release names.
fn art_url(release: &Value) -> Option<Url> {
    let art_id =
        util::uint(&release["art_id"]).or_else(|| util::uint(&release["current"]["art_id"]))?;
    Url::parse(&format!("https://f4.bcbits.com/img/a{art_id:010}_10.jpg")).ok()
}

/// `03 Apr 2014 00:00:00 GMT`, how releases are dated.
fn release_date(value: &Value) -> Option<jiff::Timestamp> {
    value.as_str().and_then(util::parse_timestamp)
}

pub struct BandcampResolver {
    http: Http,
}

impl BandcampResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// A page, read the way a browser would: the site refuses other clients.
    async fn page(&self, url: &Url) -> Result<String, ResolveError> {
        let fetched =
            fetch_as_browser(&self.http, url, PLATFORM, &navigation_headers(), MAX_PAGE).await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        Ok(fetched.text())
    }

    /// The files a free download offers: the download page's blob names each encoding's
    /// download link, whose `statdownload` twin answers with the link to fetch.
    async fn free_downloads(&self, download_page: &Url) -> Vec<Variant> {
        let mut variants = Vec::new();
        let html = match self.page(download_page).await {
            Ok(html) => html,
            Err(error) => {
                tracing::warn!(url = %download_page, "Bandcamp download page not read: {error}");
                return variants;
            }
        };
        let Some(blob) = data_attr(&html, "blob") else {
            return variants;
        };
        let item = if blob["digital_items"][0].is_object() {
            &blob["digital_items"][0]
        } else {
            &blob["download_items"][0]
        };
        let extensions: Vec<(String, String)> = blob["download_formats"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|f| {
                Some((
                    f["name"].as_str()?.to_string(),
                    f["file_extension"]
                        .as_str()?
                        .trim_start_matches('.')
                        .to_string(),
                ))
            })
            .collect();
        let Some(downloads) = item["downloads"].as_object() else {
            return variants;
        };
        for (key, download) in downloads {
            let Some(link) = util::url_of(&download["url"], None) else {
                continue;
            };
            let format_id = download["encoding_name"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| key.clone());
            // The stat link answers with the link to fetch, as the site's download
            // script asks for it.
            let mut stat = Url::parse(&link.as_str().replace("/download/", "/statdownload/"))
                .unwrap_or_else(|_| link.clone());
            stat.query_pairs_mut().append_pair(
                ".rand",
                &format!(
                    "{}",
                    jiff::Timestamp::now().as_millisecond() % 1_000_000_007
                ),
            );
            let answer = match fetch_as_browser(&self.http, &stat, PLATFORM, &[], MAX_PAGE).await {
                Ok(fetched) if fetched.status.is_success() => fetched.text(),
                Ok(fetched) => {
                    tracing::warn!(url = %stat, "Bandcamp download stat answered HTTP {}", fetched.status);
                    continue;
                }
                Err(error) => {
                    tracing::warn!(url = %stat, "Bandcamp download stat not read: {error}");
                    continue;
                }
            };
            let (Some(start), Some(end)) = (answer.find('{'), answer.rfind('}')) else {
                continue;
            };
            let Ok(stat) = serde_json::from_str::<Value>(&answer[start..=end]) else {
                continue;
            };
            let Some(retry) = util::url_of(&stat["retry_url"], None) else {
                continue;
            };
            let mut variant = Variant::file(retry);
            variant.audio_only = true;
            let codec = format_id
                .split('-')
                .next()
                .unwrap_or(&format_id)
                .to_string();
            let ext = extensions
                .iter()
                .find(|(name, _)| *name == format_id)
                .map(|(_, ext)| ext.clone())
                .unwrap_or_else(|| codec.clone());
            variant.container = Some(audio_container(&ext));
            variant.audio = Some(audio_codec(&codec));
            variant.bitrate = format_id
                .rsplit('-')
                .next()
                .and_then(|b| b.parse::<u64>().ok())
                .map(|k| k * 1000);
            variant.size = download["size_mb"]
                .as_str()
                .and_then(|m| m.trim_end_matches("MB").trim().parse::<f64>().ok())
                .map(|mb| (mb * 1_000_000.0) as u64);
            variant.format_id = Some(format_id.clone());
            variant.label = download["description"]
                .as_str()
                .and_then(clean_title)
                .or(Some(format_id));
            variants.push(variant);
        }
        variants
    }

    async fn resolve_track(
        &self,
        artist: &str,
        slug: &str,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let html = self.page(url).await?;
        let release = tralbum(&html).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let track = release["trackinfo"]
            .as_array()
            .and_then(|tracks| {
                tracks
                    .iter()
                    .find(|t| {
                        t["title_link"]
                            .as_str()
                            .is_some_and(|link| link.ends_with(slug))
                    })
                    .or_else(|| tracks.first())
            })
            .cloned()
            .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let mut variants = track_variants(&track);
        if let Some(download_page) = util::url_of(&release["freeDownloadPage"], Some(url)) {
            variants.extend(self.free_downloads(&download_page).await);
        }
        if variants.is_empty() {
            let reason = if track["streaming"].as_i64() == Some(0)
                || track["unreleased_track"].as_bool() == Some(true)
            {
                "the track is not streamable"
            } else {
                "the page carries no streaming file"
            };
            return Err(ResolveError::unavailable(url, reason));
        }
        let (description, thumbnail, meta_duration) = {
            let page = Page::parse(&html, url);
            (
                page.meta("og:description").and_then(|d| clean_title(&d)),
                page.meta("og:image")
                    .and_then(|t| util::join_url(Some(url), &t)),
                page.meta("duration")
                    .and_then(|d| d.trim().parse::<f64>().ok())
                    .filter(|d| *d > 0.0)
                    .map(std::time::Duration::from_secs_f64),
            )
        };
        let embed = data_attr(&html, "embed").unwrap_or(Value::Null);
        let artist_name = track["artist"]
            .as_str()
            .or(embed["artist"].as_str())
            .or(release["current"]["artist"].as_str())
            .or(release["artist"].as_str())
            .and_then(clean_title)
            .or_else(|| util::search(&RE_ALBUM_ARTIST, &html).and_then(|a| clean_title(&a)));
        let mut resolved = Resolved::of(PLATFORM, MediaKind::Audio);
        resolved.id = util::text(&track["track_id"])
            .or_else(|| util::text(&track["id"]))
            .or_else(|| Some(slug.to_string()));
        resolved.title = track["title"].as_str().and_then(clean_title);
        resolved.description = description;
        resolved.thumbnail = art_url(&release).or(thumbnail);
        resolved.duration = util::seconds(&track["duration"]).or(meta_duration);
        resolved.uploaded_at = release_date(&release["current"]["release_date"])
            .or_else(|| release_date(&release["album_release_date"]))
            .or_else(|| release_date(&release["current"]["publish_date"]));
        resolved.uploader = artist_name;
        resolved.uploader_url = Url::parse(&format!("https://{artist}.bandcamp.com/")).ok();
        resolved.webpage_url = Some(url.clone());
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn resolve_album(&self, slug: &str, url: &Url) -> Result<Resolution, ResolveError> {
        let html = self.page(url).await?;
        let release = tralbum(&html).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let base = url.clone();
        // Only tracks with a length have a song to play; the rest are placeholders.
        let entries: Vec<PlaylistEntry> = release["trackinfo"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|track| util::seconds(&track["duration"]).is_some())
            .filter_map(|track| {
                let link = track["title_link"].as_str()?;
                Some(PlaylistEntry {
                    url: base.join(link).ok()?,
                    title: track["title"].as_str().and_then(clean_title),
                    duration: util::seconds(&track["duration"]),
                })
            })
            .collect();
        if entries.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let title = match (
            release["artist"].as_str().and_then(clean_title),
            release["current"]["title"].as_str().and_then(clean_title),
        ) {
            (Some(artist), Some(album)) => Some(format!("{artist} - {album}")),
            (artist, album) => album.or(artist),
        };
        let total = entries.len();
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: util::text(&release["current"]["id"])
                .or_else(|| util::text(&release["id"]))
                .or_else(|| Some(slug.to_string())),
            title,
            entries,
            total: Some(total),
        }))
    }
}

impl BandcampResolver {
    /// A Bandcamp Weekly show: the player API's compiled track, streamed as one file.
    async fn resolve_weekly(&self, show: u64, url: &Url) -> Result<Resolution, ResolveError> {
        let response = self
            .http
            .post(Url::parse(PLAYER_API).expect("valid"))
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "application/json")
            .header("content-type", "application/json")
            .json(&serde_json::json!({"item_id": show, "item_type": "radio"}))
            .send()
            .await?;
        if let Some(error) = status_error(response.status, url) {
            return Err(error);
        }
        let answer: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(url, format!("player JSON: {e}")))?;
        let data = &answer["tracklist"];
        if data.is_null() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let audio = &data["compiledTrack"];
        let stream = util::url_of(&audio["streamUrl"], None)
            .ok_or_else(|| ResolveError::unavailable(url, "the show names no stream"))?;
        let format_id = util::query_param(&stream, "enc");
        let mut variant = Variant::file(stream);
        variant.audio_only = true;
        let (codec, kbps) = format_id
            .as_deref()
            .and_then(|id| RE_ENCODING.captures(id))
            .map(|caps| (caps[1].to_string(), caps[2].parse::<u64>().ok()))
            .unwrap_or_else(|| ("mp3".to_string(), None));
        variant.container = Some(audio_container(&codec));
        variant.audio = Some(audio_codec(&codec));
        variant.bitrate = kbps.map(|k| k * 1000);
        variant.duration = util::seconds(&audio["duration"]);
        variant.format_id = format_id.clone();
        variant.label = format_id;
        let released =
            release_date(&data["date"]).or_else(|| release_date(&data["published_date"]));
        let mut resolved = Resolved::of(PLATFORM, MediaKind::Audio);
        resolved.id = Some(show.to_string());
        resolved.title = match (data["subtitle"].as_str().and_then(clean_title), released) {
            (Some(series), Some(at)) => Some(format!("{series}, {}", at.strftime("%Y-%m-%d"))),
            (Some(series), None) => Some(series),
            (None, _) => data["title"].as_str().and_then(clean_title),
        };
        resolved.description = data["desc"]
            .as_str()
            .or(data["description"].as_str())
            .and_then(clean_title);
        resolved.thumbnail = util::uint(&data["show_image_id"])
            .or_else(|| util::uint(&data["imageId"]))
            .and_then(|id| Url::parse(&format!("https://f4.bcbits.com/img/{id}_0.jpg")).ok());
        resolved.duration = variant.duration;
        resolved.uploaded_at = released;
        resolved.uploader = Some("Bandcamp Weekly".to_string());
        resolved.uploader_url = Url::parse("https://bandcamp.com/radio").ok();
        resolved.webpage_url = Some(url.clone());
        resolved.variants = vec![variant];
        Ok(Resolution::from(resolved))
    }

    /// An artist's discography: every release their page lists.
    async fn resolve_user(&self, artist: &str, url: &Url) -> Result<Resolution, ResolveError> {
        let page_url = Url::parse(&format!("https://{artist}.bandcamp.com/music")).expect("valid");
        let html = self.page(&page_url).await?;
        let entries: Vec<PlaylistEntry> = discography_of(&html)
            .into_iter()
            .filter_map(|item| page_url.join(&item).ok())
            .filter(|link| {
                matches!(
                    parse_link(link),
                    Some(Link::Track { .. } | Link::Album { .. })
                )
            })
            .map(|link| PlaylistEntry {
                url: link,
                title: None,
                duration: None,
            })
            .collect();
        if entries.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let title = Page::parse(&html, &page_url)
            .meta("og:title")
            .and_then(|t| clean_title(&t))
            .map(|name| format!("Discography of {name}"))
            .unwrap_or_else(|| format!("Discography of {artist}"));
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(artist.to_string()),
            title: Some(title),
            total: Some(entries.len()),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for BandcampResolver {
    /// Releases other pages embed: the `og:url` of a track or album page.
    fn embeds_in(&self, page: &Page) -> Vec<Url> {
        RE_OG_URL
            .captures_iter(page.html())
            .filter_map(|caps| Url::parse(&util::html_unescape(&caps[1])).ok())
            .filter(|link| parse_link(link).is_some())
            .collect()
    }

    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Bandcamp",
            hosts: &["bandcamp.com"],
            features: &[
                "audio",
                "tracks",
                "albums",
                "free downloads",
                "discographies",
                "weekly shows",
            ],
            formats: &["mp3", "flac", "aac", "ogg", "wav", "aiff", "alac"],
            media: &[MediaKind::Audio],
            tags: &[Tag::Music],
            session: SessionSupport::None,
            examples: &[
                "https://benprunty.bandcamp.com/track/lanius-battle",
                "https://benprunty.bandcamp.com/album/ftl-advanced-edition-soundtrack",
                "https://benprunty.bandcamp.com/music",
                "https://bandcamp.com/radio?show=224",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Track { artist, slug } => self.resolve_track(&artist, &slug, url).await,
            Link::Album { slug, .. } => self.resolve_album(&slug, url).await,
            Link::Weekly { show } => self.resolve_weekly(show, url).await,
            Link::User { artist } => self.resolve_user(&artist, url).await,
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

    fn page(release: &Value) -> String {
        let escaped = release
            .to_string()
            .replace('&', "&amp;")
            .replace('"', "&quot;");
        format!(
            r#"<html><head><meta property="og:description" content="from FTL"><meta property="og:image" content="https://f4.bcbits.com/img/a1270682128_16.jpg"></head><body><script data-tralbum="{escaped}"></script></body></html>"#
        )
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("http://benprunty.bandcamp.com/track/lanius-battle"),
            Some(Link::Track {
                artist: "benprunty".into(),
                slug: "lanius-battle".into()
            })
        );
        assert_eq!(
            link("https://blazo.bandcamp.com/album/jazz-format-mixtape-vol-1"),
            Some(Link::Album {
                artist: Some("blazo".into()),
                slug: "jazz-format-mixtape-vol-1".into()
            })
        );
        assert_eq!(link("https://bandcamp.com/track/x"), None);
        assert_eq!(
            link("https://bandcamp.com/album/x"),
            Some(Link::Album {
                artist: None,
                slug: "x".into()
            })
        );
        assert_eq!(
            link("https://benprunty.bandcamp.com/music"),
            Some(Link::User {
                artist: "benprunty".into()
            })
        );
        assert_eq!(
            link("https://benprunty.bandcamp.com/"),
            Some(Link::User {
                artist: "benprunty".into()
            })
        );
        assert_eq!(link("https://benprunty.bandcamp.com/merch"), None);
        assert_eq!(
            link("https://bandcamp.com/?show=224"),
            None,
            "the radio page is /radio"
        );
        assert_eq!(
            link("https://bandcamp.com/radio?show=224"),
            Some(Link::Weekly { show: 224 })
        );
        assert_eq!(
            link("https://www.bandcamp.com/radio/?blah=1&show=228"),
            Some(Link::Weekly { show: 228 })
        );
    }

    #[tokio::test]
    async fn tracks_resolve_to_their_stream() {
        let release = json!({
            "artist": "Ben Prunty", "art_id": 1270682128, "album_release_date": "03 Apr 2014 00:00:00 GMT", "album_url": "/album/ftl",
            "current": {"title": "Lanius (Battle)", "release_date": null, "id": 2650410135u64, "type": "track"},
            "trackinfo": [{"id": 2650410135u64, "track_id": 2650410135u64, "title": "Lanius (Battle)", "artist": null, "duration": 260.877, "title_link": "/track/lanius-battle",
                           "file": {"mp3-128": "https://t4.bcbits.com/stream/9b90/mp3-128/2650410135?p=0&ts=1"}, "streaming": 1}]
        });
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://benprunty.bandcamp.com/track/lanius-battle",
            200,
            "text/html",
            page(&release),
        ));
        let unreleased = json!({"artist": "X", "current": {"title": "Soon"}, "trackinfo": [{"title": "Soon", "title_link": "/track/soon", "file": null, "streaming": 0, "unreleased_track": true}]});
        fixture.exchanges.push(get(
            "https://x.bandcamp.com/track/soon",
            200,
            "text/html",
            page(&unreleased),
        ));
        let resolver = BandcampResolver::new(Http::replay(fixture));
        let url = Url::parse("https://benprunty.bandcamp.com/track/lanius-battle").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("2650410135"));
        assert_eq!(resolved.title.as_deref(), Some("Lanius (Battle)"));
        assert_eq!(resolved.uploader.as_deref(), Some("Ben Prunty"));
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(260.877)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1396483200)
        );
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://f4.bcbits.com/img/a1270682128_10.jpg"
        );
        assert_eq!(resolved.variants.len(), 1);
        let audio = &resolved.variants[0];
        assert!(audio.audio_only);
        assert_eq!(resolved.media, MediaKind::Audio);
        assert_eq!(audio.container, Some(Container::Mp3));
        assert_eq!(audio.audio, Some(AudioCodec::Mp3));
        assert_eq!(audio.bitrate, Some(128_000));
        assert_eq!(audio.format_id.as_deref(), Some("mp3-128"));
        let error = resolver
            .resolve(&Url::parse("https://x.bandcamp.com/track/soon").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "the track is not streamable"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn albums_list_their_tracks() {
        let release = json!({
            "artist": "Ben Prunty", "art_id": 1270682128,
            "current": {"title": "FTL: Advanced Edition Soundtrack", "release_date": "03 Apr 2014 00:00:00 GMT", "id": 1745633271, "type": "album"},
            "trackinfo": [
                {"title": "Lanius (Battle)", "title_link": "/track/lanius-battle", "duration": 260.877, "track_num": 1},
                {"title": "Lanius (Explore)", "title_link": "/track/lanius-explore", "duration": 300.0, "track_num": 2},
                {"title": "No page", "title_link": null}
            ]
        });
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://benprunty.bandcamp.com/album/ftl-advanced-edition-soundtrack",
            200,
            "text/html",
            page(&release),
        ));
        fixture.exchanges.push(get(
            "https://benprunty.bandcamp.com/album/nothing",
            404,
            "text/html",
            "<html>gone</html>".into(),
        ));
        let resolver = BandcampResolver::new(Http::replay(fixture));
        let Resolution::Playlist(album) = resolver
            .resolve(
                &Url::parse("https://benprunty.bandcamp.com/album/ftl-advanced-edition-soundtrack")
                    .unwrap(),
            )
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(
            album.title.as_deref(),
            Some("Ben Prunty - FTL: Advanced Edition Soundtrack")
        );
        assert_eq!(album.id.as_deref(), Some("1745633271"));
        assert_eq!(album.entries.len(), 2);
        assert_eq!(
            album.entries[1].url.as_str(),
            "https://benprunty.bandcamp.com/track/lanius-explore"
        );
        assert_eq!(
            album.entries[0].duration,
            Some(Duration::from_secs_f64(260.877))
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://benprunty.bandcamp.com/album/nothing").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn recorded_pages_resolve_tracks_downloads_albums_discographies_and_shows() {
        let fixture = Fixture::parse(include_str!("bandcamp_fixture.json")).unwrap();
        let resolver = BandcampResolver::new(Http::replay(fixture));
        let track = resolver
            .resolve(&Url::parse("https://benprunty.bandcamp.com/track/lanius-battle").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(track.title.as_deref(), Some("Lanius (Battle)"));
        assert_eq!(track.uploader.as_deref(), Some("Ben Prunty"));
        assert_eq!(
            track.variants.len(),
            9,
            "the stream and eight free downloads"
        );
        assert!(track.variants.iter().all(|v| v.audio_only));
        assert_eq!(track.media, MediaKind::Audio);
        assert_eq!(track.variants[0].format_id.as_deref(), Some("mp3-128"));
        let flac = track
            .variants
            .iter()
            .find(|v| v.format_id.as_deref() == Some("flac"))
            .unwrap();
        assert_eq!(flac.container, Some(Container::Flac));
        assert_eq!(flac.audio, Some(AudioCodec::Flac));
        assert_eq!(flac.label.as_deref(), Some("FLAC"));
        assert!(flac.url.path().starts_with("/download/track"));
        let mp3_320 = track
            .variants
            .iter()
            .find(|v| v.format_id.as_deref() == Some("mp3-320"))
            .unwrap();
        assert_eq!(mp3_320.bitrate, Some(320_000));
        assert_eq!(mp3_320.audio, Some(AudioCodec::Mp3));

        let Resolution::Playlist(album) = resolver
            .resolve(
                &Url::parse("https://benprunty.bandcamp.com/album/ftl-advanced-edition-soundtrack")
                    .unwrap(),
            )
            .await
            .unwrap()
        else {
            panic!("an album is a playlist");
        };
        assert_eq!(album.entries.len(), 9);
        assert!(album.entries.iter().all(|e| e.duration.is_some()));

        let Resolution::Playlist(discography) = resolver
            .resolve(&Url::parse("https://benprunty.bandcamp.com/music").unwrap())
            .await
            .unwrap()
        else {
            panic!("a discography is a playlist");
        };
        assert_eq!(
            discography.title.as_deref(),
            Some("Discography of Ben Prunty")
        );
        assert!(
            discography.entries.len() >= 20,
            "{}",
            discography.entries.len()
        );
        assert!(discography.entries.iter().all(|e| e.url.path().starts_with("/album/") || e.url.path().starts_with("/track/")));

        let show = resolver
            .resolve(&Url::parse("https://bandcamp.com/radio?show=224").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(show.id.as_deref(), Some("224"));
        assert_eq!(show.title.as_deref(), Some("Bandcamp Weekly, 2017-04-04"));
        assert!(show.description.is_some());
        assert!(show.thumbnail.is_some());
        assert_eq!(show.duration.map(|d| d.as_secs()), Some(5829));
        assert_eq!(show.variants.len(), 1);
        assert_eq!(show.variants[0].format_id.as_deref(), Some("mp3-128"));
        assert!(show.variants[0].audio_only);
        assert_eq!(show.media, MediaKind::Audio);

        assert!(matches!(
            resolver
                .resolve(
                    &Url::parse("https://benprunty.bandcamp.com/album/no-such-album-here").unwrap()
                )
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[test]
    fn discographies_and_embeds_are_read_from_markup() {
        let html = r#"<ol id="music-grid" data-client-items="[{&quot;page_url&quot;:&quot;/album/late&quot;}]"><li data-item-id="album-1"><a href="/album/first">x</a></li><li data-item-id="merch-2"><a href="/merch/shirt">y</a></li><li data-item-id="track-3"><a href="/track/single">z</a></li></ol>"#;
        assert_eq!(
            discography_of(html),
            vec!["/album/first", "/track/single", "/album/late"]
        );
        let older = r#"<div class="trackTitle" href="/track/only">"#;
        assert_eq!(discography_of(older), vec!["/track/only"]);
        let page = Page::parse(
            r#"<html><head><meta property="og:url" content="https://blazo.bandcamp.com/album/jazz-format-mixtape-vol-1"></head></html>"#,
            &Url::parse("https://example.com/post").unwrap(),
        );
        let resolver = BandcampResolver::new(Http::replay(Fixture::new(PLATFORM, None)));
        assert_eq!(
            resolver
                .embeds_in(&page)
                .iter()
                .map(|u| u.as_str())
                .collect::<Vec<_>>(),
            vec!["https://blazo.bandcamp.com/album/jazz-format-mixtape-vol-1"]
        );
    }
}
