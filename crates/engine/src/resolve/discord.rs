//! Discord attachments: links to files on Discord's CDN, and links to messages, whose
//! attachments are read through a bot that can see the channel. An attachment is whatever
//! it is, a video, an image, an audio file or any other file, by the type the CDN serves
//! it as and then by its name. A message with several attachments becomes a playlist of
//! them, and a message whose only media is an embed of another platform is handed to
//! that platform's resolver.

use std::sync::Arc;

use async_trait::async_trait;
use jiff::Timestamp;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, Tag, Variant, VariantKind, clean_title, essence, fetch, probe_file,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

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
        "discord.com" | "discordapp.com" | "ptb.discord.com" | "canary.discord.com"
        | "www.discord.com" => match segments.as_slice() {
            ["channels", _guild, channel, message] => Some(Link::Message {
                channel: channel.parse().ok()?,
                message: message.parse().ok()?,
            }),
            _ => None,
        },
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

/// What a file is, from the type it is served as first and its name second: the format
/// names its kind when either is one this crate knows, else the served type's family
/// (`image/*`, `audio/*`, `video/*`) does, and anything else is a file.
pub fn classify(content_type: &str, name: &str) -> (Option<Container>, MediaKind) {
    let container = Container::from_mime(content_type).or_else(|| Container::from_name(name));
    let kind = match &container {
        Some(container) => match MediaKind::from_mime(content_type) {
            MediaKind::File => container.kind(),
            served => served,
        },
        None => MediaKind::from_mime(content_type),
    };
    (container, kind)
}

/// A file variant for an attachment as the API describes it, with what it is.
pub fn attachment_variant(attachment: &Value) -> Option<(Variant, MediaKind)> {
    let url = attachment["url"]
        .as_str()
        .and_then(|u| Url::parse(u).ok())?;
    let mut v = Variant::new(url, VariantKind::File);
    let content_type = essence(attachment["content_type"].as_str());
    let name = attachment["filename"].as_str().unwrap_or_default();
    let (container, kind) = classify(&content_type, name);
    v.container = container;
    if v.container == Some(Container::Mp4) {
        v.video = Some(VideoCodec::H264);
        v.audio = Some(AudioCodec::Aac);
    }
    v.audio_only = kind == MediaKind::Audio;
    v.width = attachment["width"].as_u64().map(|w| w as u32);
    v.height = attachment["height"].as_u64().map(|h| h as u32);
    v.size = attachment["size"].as_u64();
    v.duration = attachment["duration_secs"]
        .as_f64()
        .filter(|d| *d > 0.0)
        .map(std::time::Duration::from_secs_f64);
    v.format_id = attachment["id"].as_str().map(String::from);
    Some((v, kind))
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
        let (container, kind) = classify(&content_type, &name);
        let mut v = Variant::new(file.clone(), VariantKind::File);
        v.container = container;
        if v.container == Some(Container::Mp4) {
            v.video = Some(VideoCodec::H264);
            v.audio = Some(AudioCodec::Aac);
        }
        v.audio_only = kind == MediaKind::Audio;
        v.size = probed.size;
        let mut resolved = Resolved::of(PLATFORM, kind);
        resolved.id = file
            .path_segments()
            .and_then(|mut s| s.nth(2))
            .map(String::from);
        resolved.title = clean_title(name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s));
        resolved.webpage_url = Some(link.clone());
        resolved.variants = vec![v];
        Ok(resolved)
    }

    /// The message as the first bot that can see its channel reads it.
    async fn message(
        &self,
        channel: u64,
        message: u64,
        origin: &Url,
    ) -> Result<Value, ResolveError> {
        let tokens = self.tokens.tokens();
        if tokens.is_empty() {
            return Err(ResolveError::unavailable(
                origin,
                "no Discord bot is running to read the message; a link to the attachment itself resolves",
            ));
        }
        let api =
            Url::parse(&format!("{API}channels/{channel}/messages/{message}")).expect("valid");
        let mut refusal = None;
        for token in tokens {
            let headers = [("authorization".to_string(), format!("Bot {token}"))];
            let fetched = fetch(&self.http, &api, PLATFORM, BOT_UA, &headers, MAX_PAGE).await?;
            match fetched.status.as_u16() {
                200..=299 => return fetched.json(origin),
                401 => refusal = Some("the bot token was rejected".to_string()),
                403 => refusal = Some("no running bot can see the channel".to_string()),
                404 => {
                    refusal = Some(
                        "the message is gone, or no running bot can see its channel".to_string(),
                    )
                }
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
            features: &[
                "attachments",
                "media proxy links",
                "message links through the bots",
                "multiple attachments",
                "images",
                "audio files",
                "other files",
                "embedded players",
            ],
            formats: &[
                "mp4", "webm", "mov", "gif", "jpg", "png", "webp", "mp3", "m4a", "ogg", "flac",
                "wav", "any file",
            ],
            media: &[
                MediaKind::Video,
                MediaKind::Audio,
                MediaKind::Image,
                MediaKind::File,
            ],
            tags: &[Tag::Basic, Tag::Social, Tag::Files, Tag::Images],
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
                let attachments: Vec<&Value> = data["attachments"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|a| a["url"].as_str().is_some())
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
                if attachments.is_empty() {
                    // An embed of a video hosted elsewhere is that host's to resolve.
                    let embed = data["embeds"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|e| {
                            matches!(
                                e["type"].as_str(),
                                Some("video") | Some("gifv") | Some("rich")
                            )
                        })
                        .find_map(|e| {
                            e["url"]
                                .as_str()
                                .or(e["video"]["url"].as_str())
                                .and_then(|u| Url::parse(u).ok())
                        });
                    return match embed {
                        Some(target) if parse_link(&target).is_none() => {
                            Err(ResolveError::Redirect(target))
                        }
                        _ => Err(ResolveError::unavailable(
                            url,
                            "the message carries no attachment",
                        )),
                    };
                }
                if attachments.len() > 1 {
                    let entries = attachments
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
                let attachment = attachments[0];
                let (variant, kind) = attachment_variant(attachment)
                    .ok_or_else(|| ResolveError::malformed(url, "the attachment has no URL"))?;
                let mut resolved = Resolved::of(PLATFORM, kind);
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

    fn get(
        url: &str,
        status: u16,
        content_type: &str,
        body: &str,
        headers: &[(&str, &str)],
    ) -> Exchange {
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
                {"id": "1300", "filename": "clip one.mp4", "size": 5000, "url": ATTACHMENT, "proxy_url": "https://media.discordapp.net/attachments/1200/1300/clip%20one.mp4", "width": 1280, "height": 720, "content_type": "video/mp4", "duration_secs": 12.5}
            ],
            "embeds": []
        })
    }

    #[test]
    fn files_are_told_apart_by_type_then_name() {
        assert_eq!(
            classify("video/mp4", "a.bin"),
            (Some(Container::Mp4), MediaKind::Video)
        );
        assert_eq!(
            classify("image/png", "a.png"),
            (Some(Container::Png), MediaKind::Image)
        );
        assert_eq!(
            classify("image/gif", "a.gif"),
            (Some(Container::Gif), MediaKind::Video)
        );
        assert_eq!(
            classify("audio/mpeg", "song"),
            (Some(Container::Mp3), MediaKind::Audio)
        );
        assert_eq!(
            classify("application/octet-stream", "song.flac"),
            (Some(Container::Flac), MediaKind::Audio)
        );
        assert_eq!(
            classify("", "photo.jpeg"),
            (Some(Container::Jpeg), MediaKind::Image)
        );
        assert_eq!(
            classify("image/heic", "photo.heic"),
            (Some(Container::Other("heic".into())), MediaKind::Image)
        );
        assert_eq!(
            classify("application/pdf", "paper.pdf"),
            (Some(Container::Other("pdf".into())), MediaKind::File)
        );
        assert_eq!(
            classify("text/plain", "notes.txt"),
            (Some(Container::Other("txt".into())), MediaKind::File)
        );
        assert_eq!(classify("", "noext"), (None, MediaKind::File));
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert!(matches!(link(ATTACHMENT), Some(Link::Attachment(_))));
        assert!(matches!(
            link("https://media.discordapp.net/attachments/1/2/a.mp4?width=400"),
            Some(Link::Attachment(_))
        ));
        assert_eq!(
            link("https://discord.com/channels/100/1200/1400"),
            Some(Link::Message {
                channel: 1200,
                message: 1400
            })
        );
        assert_eq!(
            link("https://discord.com/channels/@me/1200/1400"),
            Some(Link::Message {
                channel: 1200,
                message: 1400
            })
        );
        assert_eq!(link("https://discord.com/channels/100/1200"), None);
        assert_eq!(link("https://cdn.discordapp.com/emojis/1.png"), None);
        assert_eq!(link("https://discord.gg/invite"), None);
        let url = Url::parse(ATTACHMENT).unwrap();
        assert_eq!(expiry_of(&url).unwrap().as_second(), 0x66a0b5d2);
        let proxied = Url::parse("https://media.discordapp.net/attachments/1/2/a.mp4?ex=1&is=2&hm=3&width=400&format=webp").unwrap();
        assert_eq!(
            plain_attachment(&proxied).as_str(),
            "https://media.discordapp.net/attachments/1/2/a.mp4?ex=1&is=2&hm=3"
        );
    }

    #[tokio::test]
    async fn attachment_links_are_probed_and_expired_ones_say_so() {
        let mut fixture = Fixture::new("discord", None);
        fixture.exchanges.push(get(
            ATTACHMENT,
            206,
            "video/mp4",
            "",
            &[("content-range", "bytes 0-0/5000")],
        ));
        fixture.exchanges.push(get(
            "https://cdn.discordapp.com/attachments/1200/1302/old.mp4?ex=5f000000&is=5e000000&hm=x",
            404,
            "text/plain",
            "",
            &[],
        ));
        fixture.exchanges.push(get(
            "https://cdn.discordapp.com/attachments/1200/1303/notes.txt",
            200,
            "text/plain",
            "hi",
            &[("content-length", "2")],
        ));
        fixture.exchanges.push(get("https://media.discordapp.net/attachments/1200/1304/photo.png?ex=66a0b5d2&is=669f6452&hm=abc&width=400&height=300", 206, "image/png", "", &[("content-range", "bytes 0-0/4321")]));
        fixture.exchanges.push(get(
            "https://cdn.discordapp.com/attachments/1200/1305/voice-message.ogg",
            206,
            "audio/ogg",
            "",
            &[("content-range", "bytes 0-0/999")],
        ));
        fixture.exchanges.push(get(
            "https://cdn.discordapp.com/attachments/1200/1306/archive.zip",
            206,
            "application/zip",
            "",
            &[("content-range", "bytes 0-0/70000")],
        ));
        let resolver = DiscordResolver::new(Http::replay(fixture), Arc::new(Vec::new()));
        let url = Url::parse(ATTACHMENT).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.media, MediaKind::Video);
        assert_eq!(resolved.title.as_deref(), Some("clip one"));
        assert_eq!(resolved.id.as_deref(), Some("1300"));
        assert_eq!(resolved.variants[0].size, Some(5000));
        assert_eq!(resolved.variants[0].container, Some(Container::Mp4));
        let error = resolver
            .resolve(&Url::parse("https://cdn.discordapp.com/attachments/1200/1302/old.mp4?ex=5f000000&is=5e000000&hm=x").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("expired")),
            "{error}"
        );
        let notes = resolver
            .resolve(
                &Url::parse("https://cdn.discordapp.com/attachments/1200/1303/notes.txt").unwrap(),
            )
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(notes.media, MediaKind::File);
        assert_eq!(notes.title.as_deref(), Some("notes"));
        assert_eq!(
            notes.variants[0].container,
            Some(Container::Other("txt".into()))
        );
        assert_eq!(notes.variants[0].size, Some(2));
        // The media proxy's resizing is dropped so the file arrives as it was uploaded.
        let photo = resolver
            .resolve(&Url::parse("https://media.discordapp.net/attachments/1200/1304/photo.png?ex=66a0b5d2&is=669f6452&hm=abc&width=400&height=300").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(photo.media, MediaKind::Image);
        assert_eq!(
            photo.variants[0].url.as_str(),
            "https://media.discordapp.net/attachments/1200/1304/photo.png?ex=66a0b5d2&is=669f6452&hm=abc"
        );
        assert_eq!(photo.variants[0].container, Some(Container::Png));
        assert_eq!(photo.variants[0].size, Some(4321));
        assert_eq!(
            (photo.variants[0].width, photo.variants[0].height),
            (None, None)
        );
        let voice = resolver
            .resolve(
                &Url::parse("https://cdn.discordapp.com/attachments/1200/1305/voice-message.ogg")
                    .unwrap(),
            )
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(voice.media, MediaKind::Audio);
        assert_eq!(voice.variants[0].container, Some(Container::Ogg));
        assert!(voice.variants[0].audio_only);
        let archive = resolver
            .resolve(
                &Url::parse("https://cdn.discordapp.com/attachments/1200/1306/archive.zip")
                    .unwrap(),
            )
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(archive.media, MediaKind::File);
        assert_eq!(
            archive.variants[0].container,
            Some(Container::Other("zip".into()))
        );
        assert_eq!(archive.variants[0].size, Some(70000));
    }

    #[tokio::test]
    async fn message_links_are_read_through_a_bot_that_sees_the_channel() {
        let api = "https://discord.com/api/v10/channels/1200/messages/1400";
        let mut fixture = Fixture::new("discord", None);
        fixture.exchanges.push(get(
            api,
            403,
            "application/json",
            r#"{"message":"Missing Access","code":50001}"#,
            &[],
        ));
        fixture.exchanges.push(get(
            api,
            200,
            "application/json",
            &message().to_string(),
            &[],
        ));
        let resolver = DiscordResolver::new(
            Http::replay(fixture),
            Arc::new(vec!["blind".to_string(), "seeing".to_string()]),
        );
        let url = Url::parse("https://discord.com/channels/100/1200/1400").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.media, MediaKind::Video);
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
    async fn image_and_file_attachments_in_messages_are_what_they_are() {
        let mut photo = message();
        photo["attachments"] = json!([{"id": "1310", "filename": "sunset.jpg", "size": 300, "url": "https://cdn.discordapp.com/attachments/1200/1310/sunset.jpg", "width": 4032, "height": 3024, "content_type": "image/jpeg"}]);
        let mut sound = message();
        sound["content"] = json!("");
        sound["attachments"] = json!([{"id": "1311", "filename": "voice-message.ogg", "size": 300, "url": "https://cdn.discordapp.com/attachments/1200/1311/voice-message.ogg", "content_type": "audio/ogg", "duration_secs": 3.2, "waveform": "AAA="}]);
        let mut paper = message();
        paper["attachments"] = json!([{"id": "1312", "filename": "paper.pdf", "size": 3000, "url": "https://cdn.discordapp.com/attachments/1200/1312/paper.pdf", "content_type": "application/pdf"}]);
        let mut fixture = Fixture::new("discord", None);
        fixture.exchanges.push(get(
            "https://discord.com/api/v10/channels/1200/messages/1410",
            200,
            "application/json",
            &photo.to_string(),
            &[],
        ));
        fixture.exchanges.push(get(
            "https://discord.com/api/v10/channels/1200/messages/1411",
            200,
            "application/json",
            &sound.to_string(),
            &[],
        ));
        fixture.exchanges.push(get(
            "https://discord.com/api/v10/channels/1200/messages/1412",
            200,
            "application/json",
            &paper.to_string(),
            &[],
        ));
        let resolver = DiscordResolver::new(Http::replay(fixture), Arc::new(vec!["t".to_string()]));
        let photo = resolver
            .resolve(&Url::parse("https://discord.com/channels/100/1200/1410").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(photo.media, MediaKind::Image);
        assert_eq!(photo.variants[0].container, Some(Container::Jpeg));
        assert_eq!(
            (photo.variants[0].width, photo.variants[0].height),
            (Some(4032), Some(3024))
        );
        assert_eq!(photo.variants[0].size, Some(300));
        let sound = resolver
            .resolve(&Url::parse("https://discord.com/channels/100/1200/1411").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(sound.media, MediaKind::Audio);
        assert_eq!(sound.title.as_deref(), Some("voice-message"));
        assert_eq!(sound.variants[0].container, Some(Container::Ogg));
        assert!(sound.variants[0].audio_only);
        assert_eq!(
            sound.duration,
            Some(std::time::Duration::from_secs_f64(3.2))
        );
        let paper = resolver
            .resolve(&Url::parse("https://discord.com/channels/100/1200/1412").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(paper.media, MediaKind::File);
        assert_eq!(
            paper.variants[0].container,
            Some(Container::Other("pdf".into()))
        );
    }

    #[tokio::test]
    async fn several_attachments_are_a_playlist_and_embeds_are_handed_on() {
        let api = "https://discord.com/api/v10/channels/1200/messages/1401";
        let mut two = message();
        two["attachments"].as_array_mut().unwrap().push(json!({"id": "1305", "filename": "notes.txt", "size": 7, "url": "https://cdn.discordapp.com/attachments/1200/1305/notes.txt", "content_type": "text/plain"}));
        let mut embed = message();
        embed["attachments"] = json!([]);
        embed["embeds"] = json!([{"type": "video", "url": "https://www.youtube.com/watch?v=jNQXAC9IVRw", "video": {"url": "https://www.youtube.com/embed/jNQXAC9IVRw"}}]);
        let mut fixture = Fixture::new("discord", None);
        fixture
            .exchanges
            .push(get(api, 200, "application/json", &two.to_string(), &[]));
        fixture.exchanges.push(get(
            "https://discord.com/api/v10/channels/1200/messages/1402",
            200,
            "application/json",
            &embed.to_string(),
            &[],
        ));
        fixture.exchanges.push(get(
            "https://discord.com/api/v10/channels/1200/messages/1403",
            404,
            "application/json",
            r#"{"message":"Unknown Message","code":10008}"#,
            &[],
        ));
        let resolver = DiscordResolver::new(Http::replay(fixture), Arc::new(vec!["t".to_string()]));
        let playlist = match resolver
            .resolve(&Url::parse("https://discord.com/channels/100/1200/1401").unwrap())
            .await
            .unwrap()
        {
            Resolution::Playlist(p) => p,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(playlist.entries[1].title.as_deref(), Some("notes.txt"));
        let error = resolver
            .resolve(&Url::parse("https://discord.com/channels/100/1200/1402").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Redirect(u) if u.as_str() == "https://www.youtube.com/watch?v=jNQXAC9IVRw"),
            "{error}"
        );
        let error = resolver
            .resolve(&Url::parse("https://discord.com/channels/100/1200/1403").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("gone")),
            "{error}"
        );
        let mut empty = message();
        empty["attachments"] = json!([]);
        let mut fixture = Fixture::new("discord", None);
        fixture.exchanges.push(get(
            "https://discord.com/api/v10/channels/1200/messages/1404",
            200,
            "application/json",
            &empty.to_string(),
            &[],
        ));
        let resolver = DiscordResolver::new(Http::replay(fixture), Arc::new(vec!["t".to_string()]));
        let error = resolver
            .resolve(&Url::parse("https://discord.com/channels/100/1200/1404").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("no attachment")),
            "{error}"
        );
        let none = DiscordResolver::new(
            Http::replay(Fixture::new("discord", None)),
            Arc::new(Vec::new()),
        );
        let error = none
            .resolve(&Url::parse("https://discord.com/channels/100/1200/1403").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("no Discord bot")),
            "{error}"
        );
    }
}
