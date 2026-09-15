//! Discord attachments: links to files on Discord's CDN, and links to messages, whose
//! attachments are read through a bot that can see the channel. A message with several
//! videos becomes a playlist of them, and a message whose only video is an embed of
//! another platform is handed to that platform's resolver.

use std::sync::Arc;

use async_trait::async_trait;
use jiff::Timestamp;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, Variant, VariantKind, clean_title, essence, fetch, probe_file,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "discord";
const API: &str = "https://discord.com/api/v10/";
/// The user agent Discord asks bots to send.
const BOT_UA: &str = concat!("DiscordBot (discoclip, ", env!("CARGO_PKG_VERSION"), ")");

/// Where the bot tokens that read message links come from: the applications the app runs
/// bots for, as they stand.
pub trait BotTokens: Send + Sync {
    fn tokens(&self) -> Vec<String>;
}

impl BotTokens for Vec<String> {
    fn tokens(&self) -> Vec<String> {
        self.clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A file on the CDN.
    Attachment(Url),
    /// A message, by channel and id.
    Message { channel: u64, message: u64 },
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    match host.as_str() {
        "cdn.discordapp.com" | "media.discordapp.net" => match segments.as_slice() {
            ["attachments", _channel, _attachment, _name] => Some(Link::Attachment(url.clone())),
            ["ephemeral-attachments", _, _, _] => Some(Link::Attachment(url.clone())),
            _ => None,
        },
        "discord.com" | "discordapp.com" | "ptb.discord.com" | "canary.discord.com" | "www.discord.com" => {
            match segments.as_slice() {
                ["channels", _guild, channel, message] => Some(Link::Message {
                    channel: channel.parse().ok()?,
                    message: message.parse().ok()?,
                }),
                _ => None,
            }
        }
        _ => None,
    }
}

/// When a signed attachment link stops working: its `ex` parameter, hex seconds.
pub fn expiry_of(url: &Url) -> Option<Timestamp> {
    let ex = url.query_pairs().find(|(k, _)| k == "ex")?.1.into_owned();
    let seconds = i64::from_str_radix(&ex, 16).ok()?;
    Timestamp::from_second(seconds).ok()
}

/// The CDN link with the media proxy's transformations dropped, so the file arrives as
/// it was uploaded.
fn plain_attachment(url: &Url) -> Url {
    let mut clean = url.clone();
    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| matches!(k.as_ref(), "ex" | "is" | "hm"))
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    clean.set_query(None);
    if !kept.is_empty() {
        let mut pairs = clean.query_pairs_mut();
        for (k, v) in &kept {
            pairs.append_pair(k, v);
        }
    }
    clean
}

fn file_name(url: &Url) -> String {
    let name = url.path().rsplit('/').next().unwrap_or_default();
    percent_encoding::percent_decode_str(name)
        .decode_utf8_lossy()
        .into_owned()
}

fn is_video_attachment(attachment: &Value) -> bool {
    let content_type = essence(attachment["content_type"].as_str());
    if content_type.starts_with("video/") || content_type == "image/gif" {
        return true;
    }
    attachment["filename"]
        .as_str()
        .and_then(|n| n.rsplit('.').next())
        .and_then(Container::from_extension)
        .is_some()
}

/// A file variant for an attachment as the API describes it.
pub fn attachment_variant(attachment: &Value) -> Option<Variant> {
    let url = attachment["url"].as_str().and_then(|u| Url::parse(u).ok())?;
    let mut v = Variant::new(url, VariantKind::File);
    let content_type = essence(attachment["content_type"].as_str());
    v.container = Container::from_mime(&content_type).or_else(|| {
        attachment["filename"]
            .as_str()
            .and_then(|n| n.rsplit('.').next())
            .and_then(Container::from_extension)
    });
    if v.container == Some(Container::Mp4) {
        v.video = Some(VideoCodec::H264);
        v.audio = Some(AudioCodec::Aac);
    }
    v.width = attachment["width"].as_u64().map(|w| w as u32);
    v.height = attachment["height"].as_u64().map(|h| h as u32);
    v.size = attachment["size"].as_u64();
    v.duration = attachment["duration_secs"]
        .as_f64()
        .filter(|d| *d > 0.0)
        .map(std::time::Duration::from_secs_f64);
    v.format_id = attachment["id"].as_str().map(String::from);
    Some(v)
}

pub struct DiscordResolver {
    http: Http,
    tokens: Arc<dyn BotTokens>,
}

impl DiscordResolver {
    pub fn new(http: Http, tokens: Arc<dyn BotTokens>) -> Self {
        Self { http, tokens }
    }

    async fn attachment(&self, link: &Url, origin: &Url) -> Result<Resolved, ResolveError> {
        let file = plain_attachment(link);
        let probed = probe_file(&self.http, &file, PLATFORM, BROWSER_UA, &[]).await?;
        match probed.status.as_u16() {
            200 | 206 => {}
            403 | 404 | 410 => {
                return Err(match expiry_of(link) {
                    Some(at) if at < Timestamp::now() => ResolveError::unavailable(
                        origin,
                        format!(
                            "the attachment link expired at {at}; a link to its message resolves through a running bot"
                        ),
                    ),
                    _ => ResolveError::NotFound(origin.clone()),
                });
            }
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the CDN answered HTTP {status}"),
                ));
            }
        }
        let name = file_name(&file);
        let content_type = essence(probed.content_type.as_deref());
        let container = Container::from_mime(&content_type).or_else(|| {
            name.rsplit('.')
                .next()
                .and_then(Container::from_extension)
        });
        if container.is_none() && !content_type.starts_with("video/") {
            return Err(ResolveError::unavailable(
                origin,
                format!(
                    "the attachment is {}, not a video",
                    if content_type.is_empty() { "of an unknown type".to_string() } else { content_type }
                ),
            ));
        }
        let mut v = Variant::new(file.clone(), VariantKind::File);
        v.container = container;
        if v.container == Some(Container::Mp4) {
            v.video = Some(VideoCodec::H264);
            v.audio = Some(AudioCodec::Aac);
        }
        v.size = probed.size;
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = file.path_segments().and_then(|mut s| s.nth(2)).map(String::from);
        resolved.title = clean_title(name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s));
        resolved.webpage_url = Some(link.clone());
        resolved.variants = vec![v];
        Ok(resolved)
    }

    /// The message as the first bot that can see its channel reads it.
    async fn message(&self, channel: u64, message: u64, origin: &Url) -> Result<Value, ResolveError> {
        let tokens = self.tokens.tokens();
        if tokens.is_empty() {
            return Err(ResolveError::unavailable(
                origin,
                "no Discord bot is running to read the message; a link to the attachment itself resolves",
            ));
        }
        let api = Url::parse(&format!("{API}channels/{channel}/messages/{message}")).expect("valid");
        let mut refusal = None;
        for token in tokens {
            let headers = [("authorization".to_string(), format!("Bot {token}"))];
            let fetched = fetch(&self.http, &api, PLATFORM, BOT_UA, &headers, MAX_PAGE).await?;
            match fetched.status.as_u16() {
                200..=299 => return fetched.json(origin),
                401 => refusal = Some("the bot token was rejected".to_string()),
                403 => refusal = Some("no running bot can see the channel".to_string()),
                404 => refusal = Some("the message is gone, or no running bot can see its channel".to_string()),
                429 => return Err(ResolveError::RateLimited(origin.clone())),
                status => refusal = Some(format!("the API answered HTTP {status}")),
            }
        }
        Err(ResolveError::unavailable(
            origin,
            refusal.unwrap_or_else(|| "the message could not be read".into()),
        ))
    }
}

#[async_trait]
impl Resolver for DiscordResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Discord",
            hosts: &["cdn.discordapp.com", "media.discordapp.net", "discord.com"],
            features: &["attachments", "media proxy links", "message links through the bots", "multiple attachments", "embedded players"],
            formats: &["mp4", "webm", "mov", "gif"],
            session: SessionSupport::None,
            examples: &[],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Attachment(link) => Ok(Resolution::from(self.attachment(&link, url).await?)),
            Link::Message { channel, message } => {
                let data = self.message(channel, message, url).await?;
                let videos: Vec<&Value> = data["attachments"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|a| is_video_attachment(a))
                    .collect();
                let content = data["content"].as_str().unwrap_or_default();
                let title = clean_title(content.lines().next().unwrap_or_default());
                let author = data["author"]["global_name"]
                    .as_str()
                    .or(data["author"]["username"].as_str())
                    .and_then(clean_title);
                let posted = data["timestamp"]
                    .as_str()
                    .and_then(|t| t.parse::<Timestamp>().ok());
                if videos.is_empty() {
                    // An embed of a video hosted elsewhere is that host's to resolve.
                    let embed = data["embeds"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|e| matches!(e["type"].as_str(), Some("video") | Some("gifv") | Some("rich")))
                        .find_map(|e| {
                            e["url"]
                                .as_str()
                                .or(e["video"]["url"].as_str())
                                .and_then(|u| Url::parse(u).ok())
                        });
                    return match embed {
                        Some(target) if parse_link(&target).is_none() => Err(ResolveError::Redirect(target)),
                        _ => Err(ResolveError::unavailable(url, "the message carries no video")),
                    };
                }
                if videos.len() > 1 {
                    let entries = videos
                        .iter()
                        .filter_map(|a| {
                            Some(PlaylistEntry {
                                url: a["url"].as_str().and_then(|u| Url::parse(u).ok())?,
                                title: a["filename"].as_str().and_then(clean_title),
                                duration: a["duration_secs"]
                                    .as_f64()
                                    .filter(|d| *d > 0.0)
                                    .map(std::time::Duration::from_secs_f64),
                            })
                        })
                        .collect::<Vec<_>>();
                    return Ok(Resolution::Playlist(Playlist {
                        resolver: PLATFORM.into(),
                        id: Some(message.to_string()),
                        title: title.or(author),
                        total: Some(entries.len()),
                        entries,
                    }));
                }
                let attachment = videos[0];
                let variant = attachment_variant(attachment)
                    .ok_or_else(|| ResolveError::malformed(url, "the attachment has no URL"))?;
                let mut resolved = Resolved::new(PLATFORM);
                resolved.id = attachment["id"].as_str().map(String::from);
                resolved.title = title.or_else(|| {
                    attachment["filename"]
                        .as_str()
                        .map(|n| n.rsplit_once('.').map_or(n, |(s, _)| s).to_string())
                        .and_then(|n| clean_title(&n))
                });
                resolved.description = clean_title(content);
                resolved.uploader = author;
                resolved.uploaded_at = posted;
                resolved.duration = variant.duration;
                resolved.webpage_url = Some(url.clone());
                resolved.variants = vec![variant];
                Ok(Resolution::from(resolved))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };
    use serde_json::json;

    fn get(url: &str, status: u16, content_type: &str, body: &str, headers: &[(&str, &str)]) -> Exchange {
        let mut all = vec![("content-type".to_string(), content_type.to_string())];
        all.extend(headers.iter().map(|(k, v)| (k.to_string(), v.to_string())));
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
                headers: all,
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    const ATTACHMENT: &str = "https://cdn.discordapp.com/attachments/1200/1300/clip%20one.mp4?ex=66a0b5d2&is=669f6452&hm=abc";

    fn message() -> Value {
        json!({
            "id": "1400", "channel_id": "1200", "content": "look at this\nsecond line",
            "author": {"id": "9", "username": "someone", "global_name": "Some One"},
            "timestamp": "2026-09-01T10:00:00.000000+00:00",
            "attachments": [
                {"id": "1300", "filename": "clip one.mp4", "size": 5000, "url": ATTACHMENT, "proxy_url": "https://media.discordapp.net/attachments/1200/1300/clip%20one.mp4", "width": 1280, "height": 720, "content_type": "video/mp4", "duration_secs": 12.5},
                {"id": "1301", "filename": "notes.txt", "size": 10, "url": "https://cdn.discordapp.com/attachments/1200/1301/notes.txt", "content_type": "text/plain"}
            ],
            "embeds": []
        })
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert!(matches!(link(ATTACHMENT), Some(Link::Attachment(_))));
        assert!(matches!(link("https://media.discordapp.net/attachments/1/2/a.mp4?width=400"), Some(Link::Attachment(_))));
        assert_eq!(link("https://discord.com/channels/100/1200/1400"), Some(Link::Message { channel: 1200, message: 1400 }));
        assert_eq!(link("https://discord.com/channels/@me/1200/1400"), Some(Link::Message { channel: 1200, message: 1400 }));
        assert_eq!(link("https://discord.com/channels/100/1200"), None);
        assert_eq!(link("https://cdn.discordapp.com/emojis/1.png"), None);
        assert_eq!(link("https://discord.gg/invite"), None);
        let url = Url::parse(ATTACHMENT).unwrap();
        assert_eq!(expiry_of(&url).unwrap().as_second(), 0x66a0b5d2);
        let proxied = Url::parse("https://media.discordapp.net/attachments/1/2/a.mp4?ex=1&is=2&hm=3&width=400&format=webp").unwrap();
        assert_eq!(plain_attachment(&proxied).as_str(), "https://media.discordapp.net/attachments/1/2/a.mp4?ex=1&is=2&hm=3");
    }

    #[tokio::test]
    async fn attachment_links_are_probed_and_expired_ones_say_so() {
        let mut fixture = Fixture::new("discord", None);
        fixture.exchanges.push(get(ATTACHMENT, 206, "video/mp4", "", &[("content-range", "bytes 0-0/5000")]));
        fixture.exchanges.push(get("https://cdn.discordapp.com/attachments/1200/1302/old.mp4?ex=5f000000&is=5e000000&hm=x", 404, "text/plain", "", &[]));
        fixture.exchanges.push(get("https://cdn.discordapp.com/attachments/1200/1303/notes.txt", 200, "text/plain", "hi", &[("content-length", "2")]));
        let resolver = DiscordResolver::new(Http::replay(fixture), Arc::new(Vec::new()));
        let url = Url::parse(ATTACHMENT).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.title.as_deref(), Some("clip one"));
        assert_eq!(resolved.id.as_deref(), Some("1300"));
        assert_eq!(resolved.variants[0].size, Some(5000));
        assert_eq!(resolved.variants[0].container, Some(Container::Mp4));
        let error = resolver
            .resolve(&Url::parse("https://cdn.discordapp.com/attachments/1200/1302/old.mp4?ex=5f000000&is=5e000000&hm=x").unwrap())
            .await
            .unwrap_err();
        assert!(matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("expired")), "{error}");
        let error = resolver
            .resolve(&Url::parse("https://cdn.discordapp.com/attachments/1200/1303/notes.txt").unwrap())
            .await
            .unwrap_err();
        assert!(matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("not a video")), "{error}");
    }

    #[tokio::test]
    async fn message_links_are_read_through_a_bot_that_sees_the_channel() {
        let api = "https://discord.com/api/v10/channels/1200/messages/1400";
        let mut fixture = Fixture::new("discord", None);
        fixture.exchanges.push(get(api, 403, "application/json", r#"{"message":"Missing Access","code":50001}"#, &[]));
        fixture.exchanges.push(get(api, 200, "application/json", &message().to_string(), &[]));
        let resolver = DiscordResolver::new(Http::replay(fixture), Arc::new(vec!["blind".to_string(), "seeing".to_string()]));
        let url = Url::parse("https://discord.com/channels/100/1200/1400").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.title.as_deref(), Some("look at this"));
        assert_eq!(resolved.uploader.as_deref(), Some("Some One"));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.variants.len(), 1);
        let v = &resolved.variants[0];
        assert_eq!(v.url.as_str(), ATTACHMENT);
        assert_eq!((v.width, v.height), (Some(1280), Some(720)));
        assert_eq!(v.duration, Some(std::time::Duration::from_secs_f64(12.5)));
        assert_eq!(v.size, Some(5000));
    }

    #[tokio::test]
    async fn several_videos_are_a_playlist_and_embeds_are_handed_on() {
        let api = "https://discord.com/api/v10/channels/1200/messages/1401";
        let mut two = message();
        two["attachments"][1] = json!({"id": "1305", "filename": "two.webm", "size": 7, "url": "https://cdn.discordapp.com/attachments/1200/1305/two.webm", "content_type": "video/webm"});
        let mut embed = message();
        embed["attachments"] = json!([]);
        embed["embeds"] = json!([{"type": "video", "url": "https://www.youtube.com/watch?v=jNQXAC9IVRw", "video": {"url": "https://www.youtube.com/embed/jNQXAC9IVRw"}}]);
        let mut fixture = Fixture::new("discord", None);
        fixture.exchanges.push(get(api, 200, "application/json", &two.to_string(), &[]));
        fixture.exchanges.push(get("https://discord.com/api/v10/channels/1200/messages/1402", 200, "application/json", &embed.to_string(), &[]));
        fixture.exchanges.push(get("https://discord.com/api/v10/channels/1200/messages/1403", 404, "application/json", r#"{"message":"Unknown Message","code":10008}"#, &[]));
        let resolver = DiscordResolver::new(Http::replay(fixture), Arc::new(vec!["t".to_string()]));
        let playlist = match resolver.resolve(&Url::parse("https://discord.com/channels/100/1200/1401").unwrap()).await.unwrap() {
            Resolution::Playlist(p) => p,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(playlist.entries[1].title.as_deref(), Some("two.webm"));
        let error = resolver.resolve(&Url::parse("https://discord.com/channels/100/1200/1402").unwrap()).await.unwrap_err();
        assert!(matches!(&error, ResolveError::Redirect(u) if u.as_str() == "https://www.youtube.com/watch?v=jNQXAC9IVRw"), "{error}");
        let error = resolver.resolve(&Url::parse("https://discord.com/channels/100/1200/1403").unwrap()).await.unwrap_err();
        assert!(matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("gone")), "{error}");
        let none = DiscordResolver::new(Http::replay(Fixture::new("discord", None)), Arc::new(Vec::new()));
        let error = none.resolve(&Url::parse("https://discord.com/channels/100/1200/1403").unwrap()).await.unwrap_err();
        assert!(matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("no Discord bot")), "{error}");
    }
}
