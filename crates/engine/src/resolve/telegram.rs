//! Telegram posts in public channels, through the embed the site renders for them: the
//! video file, the channel's name, the text and the time, with an album of several videos
//! as a playlist of its items.

use std::sync::LazyLock;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use scraper::{Html, Selector};
use url::Url;

use super::{
    ClipRange, MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionSupport, Variant, VariantKind, clean_title, fetch, parse_time_stamp,
    timestamp_hint,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "telegram";
const SITE: &str = "https://t.me/";

static RE_CHANNEL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z][A-Za-z0-9_]{3,}$").unwrap());
static RE_STYLE_URL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"url\('?([^')]+)'?\)").unwrap());

/// A post in a public channel, and whether the link picks one item of an album.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostRef {
    pub channel: String,
    pub id: u64,
    pub single: bool,
}

pub fn parse_link(url: &Url) -> Option<PostRef> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !matches!(host.as_str(), "t.me" | "telegram.me" | "telegram.dog" | "www.t.me") {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let (channel, id) = match segments.as_slice() {
        ["s", channel, id] | [channel, id] => (*channel, *id),
        _ => return None,
    };
    if !RE_CHANNEL.is_match(channel) || channel.eq_ignore_ascii_case("c") {
        return None;
    }
    let id: u64 = id.parse().ok()?;
    Some(PostRef {
        channel: channel.to_string(),
        id,
        single: url.query_pairs().any(|(k, _)| k == "single"),
    })
}

fn selector(text: &str) -> Selector {
    Selector::parse(text).expect("selectors in this module are valid")
}

/// One video of a post as the embed shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddedVideo {
    pub url: Url,
    /// The post id of the item, which an album gives each of its videos.
    pub post_id: Option<u64>,
    pub duration: Option<std::time::Duration>,
    pub thumbnail: Option<Url>,
}

/// What the embed says about a post.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Embed {
    pub videos: Vec<EmbeddedVideo>,
    pub owner: Option<String>,
    pub text: Option<String>,
    pub time: Option<Timestamp>,
    pub error: Option<String>,
    /// The post is there, with no video in it.
    pub has_message: bool,
}

/// Reads the embed page of a post.
pub fn parse_embed(html: &str, base: &Url) -> Embed {
    let document = Html::parse_document(html);
    let mut embed = Embed::default();
    if let Some(error) = document
        .select(&selector(".tgme_widget_message_error"))
        .next()
    {
        embed.error = clean_title(&error.text().collect::<String>());
    }
    embed.has_message = document
        .select(&selector(".tgme_widget_message"))
        .next()
        .is_some();
    embed.owner = document
        .select(&selector(".tgme_widget_message_owner_name"))
        .next()
        .and_then(|o| clean_title(&o.text().collect::<String>()));
    embed.text = document
        .select(&selector(".tgme_widget_message_text"))
        .next()
        .and_then(|t| clean_title(&t.text().collect::<String>()));
    embed.time = document
        .select(&selector(".tgme_widget_message_date time, time[datetime]"))
        .next()
        .and_then(|t| t.value().attr("datetime"))
        .and_then(|t| t.parse::<Timestamp>().ok());
    for player in document.select(&selector(
        ".tgme_widget_message_video_player, .tgme_widget_message_video_wrap, .tgme_widget_message_roundvideo_player",
    )) {
        let Some(video) = player.select(&selector("video[src]")).next() else {
            continue;
        };
        let Some(url) = video.value().attr("src").and_then(|s| base.join(s).ok()) else {
            continue;
        };
        if embed.videos.iter().any(|v| v.url == url) {
            continue;
        }
        let post_id = player
            .value()
            .attr("href")
            .and_then(|href| base.join(href).ok())
            .and_then(|link| parse_link(&link))
            .map(|post| post.id);
        let duration = player
            .select(&selector(".tgme_widget_message_video_duration, .message_video_duration"))
            .next()
            .and_then(|d| parse_time_stamp(&d.text().collect::<String>()));
        let thumbnail = player
            .select(&selector(".tgme_widget_message_video_thumb, .message_video_thumb, .tgme_widget_message_roundvideo_thumb"))
            .next()
            .and_then(|t| t.value().attr("style"))
            .and_then(|style| RE_STYLE_URL.captures(style))
            .and_then(|caps| base.join(&caps[1]).ok());
        embed.videos.push(EmbeddedVideo {
            url,
            post_id,
            duration,
            thumbnail,
        });
    }
    if embed.videos.is_empty() {
        for video in document.select(&selector("video[src]")) {
            if let Some(url) = video.value().attr("src").and_then(|s| base.join(s).ok())
                && !embed.videos.iter().any(|v| v.url == url)
            {
                embed.videos.push(EmbeddedVideo {
                    url,
                    post_id: None,
                    duration: None,
                    thumbnail: None,
                });
            }
        }
    }
    embed
}

pub struct TelegramResolver {
    http: Http,
}

impl TelegramResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    async fn embed(&self, post: &PostRef, origin: &Url) -> Result<Embed, ResolveError> {
        let mut url = Url::parse(&format!("{SITE}{}/{}", post.channel, post.id)).expect("valid");
        url.query_pairs_mut()
            .append_pair("embed", "1")
            .append_pair("mode", "tme");
        if post.single {
            url.query_pairs_mut().append_pair("single", "");
        }
        let fetched = fetch(&self.http, &url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        match fetched.status.as_u16() {
            200..=299 => {}
            404 | 410 => return Err(ResolveError::NotFound(origin.clone())),
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the embed answered HTTP {status}"),
                ));
            }
        }
        let embed = parse_embed(&fetched.text(), &url);
        if let Some(error) = &embed.error {
            let lower = error.to_ascii_lowercase();
            return Err(if lower.contains("not found") {
                ResolveError::NotFound(origin.clone())
            } else if lower.contains("private") || lower.contains("restricted") {
                ResolveError::unavailable(origin, format!("{error}; the channel's posts are not public"))
            } else {
                ResolveError::unavailable(origin, error.clone())
            });
        }
        if !embed.has_message {
            return Err(ResolveError::unavailable(
                origin,
                "the embed shows no post; the channel may be private or restricted",
            ));
        }
        Ok(embed)
    }
}

#[async_trait]
impl Resolver for TelegramResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Telegram",
            hosts: &["t.me", "telegram.me"],
            features: &["public channel posts", "albums", "round videos"],
            formats: &["mp4"],
            session: SessionSupport::None,
            examples: &["https://t.me/telegram/459"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let post = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let embed = self.embed(&post, url).await?;
        if embed.videos.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let page = Url::parse(&format!("{SITE}{}/{}", post.channel, post.id)).expect("valid");
        let title = embed
            .text
            .clone()
            .or_else(|| embed.owner.as_ref().map(|o| format!("Post by {o}")));
        if embed.videos.len() > 1 && !post.single {
            let entries = embed
                .videos
                .iter()
                .enumerate()
                .map(|(index, video)| {
                    let id = video.post_id.unwrap_or(post.id);
                    let mut entry =
                        Url::parse(&format!("{SITE}{}/{id}", post.channel)).expect("valid");
                    entry.query_pairs_mut().append_pair("single", "");
                    PlaylistEntry {
                        url: entry,
                        title: title.as_ref().map(|t| format!("{t} ({})", index + 1)),
                        duration: video.duration,
                    }
                })
                .collect::<Vec<_>>();
            return Ok(Resolution::Playlist(Playlist {
                resolver: PLATFORM.into(),
                id: Some(format!("{}/{}", post.channel, post.id)),
                title,
                total: Some(entries.len()),
                entries,
            }));
        }
        let video = embed
            .videos
            .iter()
            .find(|v| v.post_id == Some(post.id))
            .unwrap_or(&embed.videos[0]);
        let mut v = Variant::new(video.url.clone(), VariantKind::File);
        v.container = Some(Container::Mp4);
        v.video = Some(VideoCodec::H264);
        v.audio = Some(AudioCodec::Aac);
        v.duration = video.duration;
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(format!("{}/{}", post.channel, post.id));
        resolved.title = title;
        resolved.description = embed.text.clone();
        resolved.uploader = embed.owner.clone();
        resolved.uploader_url = Url::parse(&format!("{SITE}{}", post.channel)).ok();
        resolved.uploaded_at = embed.time;
        resolved.duration = video.duration;
        resolved.thumbnail = video.thumbnail.clone();
        resolved.webpage_url = Some(page);
        resolved.clip = timestamp_hint(url).map(|start| ClipRange { start, end: None });
        resolved.variants = vec![v];
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

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
                headers: vec![("content-type".into(), "text/html".into())],
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    const VIDEO_EMBED: &str = r#"<html><body><div class="tgme_widget_message" data-post="telegram/459">
      <a class="tgme_widget_message_owner_name" href="https://t.me/telegram"><span dir="auto">Telegram News</span></a>
      <a class="tgme_widget_message_video_player blured js-message_video_player" href="https://t.me/telegram/459?single">
        <i class="tgme_widget_message_video_thumb" style="background-image:url('https://cdn1.telesco.pe/file/thumb.jpg')"></i>
        <video src="https://cdn1.telesco.pe/file/6efb59a075.mp4?token=abc" class="tgme_widget_message_video js-message_video"></video>
        <time class="message_video_duration js-message_video_duration">0:26</time>
      </a>
      <div class="tgme_widget_message_text js-message_text" dir="auto">Search <b>filters</b> and more.</div>
      <span class="tgme_widget_message_date"><time datetime="2026-08-26T19:12:24+00:00" class="time">19:12</time></span>
    </div></body></html>"#;

    const ALBUM_EMBED: &str = r#"<html><body><div class="tgme_widget_message" data-post="telegram/441">
      <a class="tgme_widget_message_owner_name" href="https://t.me/telegram"><span dir="auto">Telegram News</span></a>
      <div class="tgme_widget_message_grouped_wrap">
        <a class="tgme_widget_message_video_player grouped_media_wrap" href="https://t.me/telegram/441?single"><video src="https://cdn1.telesco.pe/file/a.mp4"></video><time class="message_video_duration">0:10</time></a>
        <a class="tgme_widget_message_video_player grouped_media_wrap" href="https://t.me/telegram/442?single"><video src="https://cdn1.telesco.pe/file/b.mp4"></video><time class="message_video_duration">0:20</time></a>
      </div>
      <div class="tgme_widget_message_text" dir="auto">Two clips</div>
    </div></body></html>"#;

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://t.me/telegram/459"),
            Some(PostRef {
                channel: "telegram".into(),
                id: 459,
                single: false
            })
        );
        assert_eq!(
            link("https://t.me/s/telegram/459?single"),
            Some(PostRef {
                channel: "telegram".into(),
                id: 459,
                single: true
            })
        );
        assert_eq!(link("https://t.me/telegram"), None);
        assert_eq!(link("https://t.me/c/123456/7"), None);
        assert_eq!(link("https://t.me/joinchat/abc"), None);
    }

    #[tokio::test]
    async fn channel_posts_resolve_from_their_embed() {
        let mut fixture = Fixture::new("telegram", None);
        fixture.exchanges.push(get(
            "https://t.me/telegram/459?embed=1&mode=tme",
            200,
            VIDEO_EMBED,
        ));
        let resolver = TelegramResolver::new(Http::replay(fixture));
        let url = Url::parse("https://t.me/telegram/459").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("telegram/459"));
        assert_eq!(resolved.title.as_deref(), Some("Search filters and more."));
        assert_eq!(resolved.uploader.as_deref(), Some("Telegram News"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://t.me/telegram"
        );
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.duration, Some(std::time::Duration::from_secs(26)));
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://cdn1.telesco.pe/file/thumb.jpg"
        );
        assert_eq!(
            resolved.variants[0].url.as_str(),
            "https://cdn1.telesco.pe/file/6efb59a075.mp4?token=abc"
        );
    }

    #[tokio::test]
    async fn albums_list_their_videos_and_missing_posts_are_missing() {
        let mut fixture = Fixture::new("telegram", None);
        fixture.exchanges.push(get(
            "https://t.me/telegram/441?embed=1&mode=tme",
            200,
            ALBUM_EMBED,
        ));
        fixture.exchanges.push(get(
            "https://t.me/telegram/442?embed=1&mode=tme&single=",
            200,
            ALBUM_EMBED,
        ));
        fixture.exchanges.push(get(
            "https://t.me/telegram/99999999?embed=1&mode=tme",
            200,
            r#"<html><body><div class="tgme_widget_message_error" dir="auto">Post not found</div></body></html>"#,
        ));
        fixture.exchanges.push(get(
            "https://t.me/telegram/458?embed=1&mode=tme",
            200,
            r#"<html><body><div class="tgme_widget_message" data-post="telegram/458"><div class="tgme_widget_message_text">Text only</div></div></body></html>"#,
        ));
        let resolver = TelegramResolver::new(Http::replay(fixture));
        let playlist = match resolver
            .resolve(&Url::parse("https://t.me/telegram/441").unwrap())
            .await
            .unwrap()
        {
            Resolution::Playlist(playlist) => playlist,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(playlist.entries[1].url.as_str(), "https://t.me/telegram/442?single=");
        assert_eq!(playlist.entries[1].duration, Some(std::time::Duration::from_secs(20)));
        let second = resolver
            .resolve(&playlist.entries[1].url)
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(second.variants[0].url.as_str(), "https://cdn1.telesco.pe/file/b.mp4");
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://t.me/telegram/99999999").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://t.me/telegram/458").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
