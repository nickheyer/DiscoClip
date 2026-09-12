//! Imgur videos and animated images, with galleries and albums as playlists, through the
//! API the site's own pages read.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, Variant, VariantKind, clean_title, fetch,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "imgur";
const API: &str = "https://api.imgur.com/post/v1/";
/// The client id the web site itself sends.
const CLIENT_ID: &str = "546c25a59c58ad7";

static RE_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9]{5,}$").unwrap());

/// Pages under these first path segments are not media.
const RESERVED: &[&str] = &[
    "about", "account", "apps", "blog", "emerald", "help", "hot", "memegen", "new", "privacy",
    "register", "search", "settings", "signin", "tos", "top", "upload", "user", "vidgif",
];

/// What a link names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// One image or video.
    Media(String),
    /// A gallery post or an album, which may hold several.
    Post(String),
}

/// The id at the end of `some-title-words-abc1234`, or the whole segment.
fn id_of(segment: &str) -> Option<String> {
    let stem = segment.split('.').next().unwrap_or(segment);
    let candidate = stem.rsplit('-').next().unwrap_or(stem);
    RE_ID.is_match(candidate).then(|| candidate.to_string())
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "imgur.com" && !host.ends_with(".imgur.com") {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    match segments.as_slice() {
        ["a", post] | ["gallery", post] => id_of(post).map(Link::Post),
        ["t" | "topic" | "r", _, post] => id_of(post).map(Link::Post),
        [media] if !RESERVED.contains(&media.to_ascii_lowercase().as_str()) => {
            id_of(media).map(Link::Media)
        }
        _ => None,
    }
}

pub struct ImgurResolver {
    http: Http,
}

impl ImgurResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// `media/{id}` or `albums/{id}`: the post, or `None` when the API has nothing there.
    async fn api(
        &self,
        endpoint: &str,
        id: &str,
        origin: &Url,
    ) -> Result<Option<Value>, ResolveError> {
        let api = Url::parse(&format!(
            "{API}{endpoint}/{id}?client_id={CLIENT_ID}&include=media,account"
        ))
        .expect("valid");
        let fetched = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        match fetched.status.as_u16() {
            200..=299 => Ok(Some(fetched.json(origin)?)),
            404 | 410 => Ok(None),
            429 => Err(ResolveError::RateLimited(origin.clone())),
            status => {
                let detail = fetched
                    .json(origin)
                    .ok()
                    .and_then(|v| v["errors"][0]["detail"].as_str().map(String::from))
                    .unwrap_or_else(|| format!("the API answered HTTP {status}"));
                Err(ResolveError::unavailable(origin, detail))
            }
        }
    }

    async fn post(&self, link: &Link, origin: &Url) -> Result<Value, ResolveError> {
        let found = match link {
            Link::Media(id) => match self.api("media", id, origin).await? {
                Some(post) => Some(post),
                // Thumbnail links carry a size letter after the id: `abc1234h.mp4`.
                None if id.len() == 8 => self.api("media", &id[..7], origin).await?,
                None => self.api("albums", id, origin).await?,
            },
            Link::Post(id) => match self.api("albums", id, origin).await? {
                Some(post) => Some(post),
                None => self.api("media", id, origin).await?,
            },
        };
        found.ok_or_else(|| ResolveError::NotFound(origin.clone()))
    }
}

/// A post's media that plays: a video, or an animated image Imgur also serves as MP4.
fn playable(media: &Value) -> bool {
    media["type"].as_str() == Some("video")
        || media["metadata"]["is_animated"].as_bool() == Some(true)
}

fn variant_of(media: &Value) -> Option<Variant> {
    let id = media["id"].as_str()?;
    let is_video = media["type"].as_str() == Some("video");
    let url = if is_video {
        media["url"]
            .as_str()
            .and_then(|u| Url::parse(u).ok())
            .filter(|u| u.path().ends_with(".mp4"))
            .or_else(|| Url::parse(&format!("https://i.imgur.com/{id}.mp4")).ok())?
    } else {
        Url::parse(&format!("https://i.imgur.com/{id}.mp4")).ok()?
    };
    let mut v = Variant::new(url, VariantKind::File);
    v.container = Some(Container::Mp4);
    v.video = Some(VideoCodec::H264);
    v.audio = media["metadata"]["has_sound"]
        .as_bool()
        .filter(|s| *s)
        .map(|_| AudioCodec::Aac);
    v.width = media["width"].as_u64().filter(|w| *w > 0).map(|w| w as u32);
    v.height = media["height"]
        .as_u64()
        .filter(|h| *h > 0)
        .map(|h| h as u32);
    if is_video {
        v.size = media["size"].as_u64().filter(|s| *s > 0);
    }
    v.duration = media["metadata"]["duration"]
        .as_f64()
        .filter(|d| *d > 0.0)
        .map(Duration::from_secs_f64);
    v.label = v.height.map(|h| format!("{h}p"));
    Some(v)
}

fn resolved_of(post: &Value, media: &Value, origin: &Url) -> Result<Resolved, ResolveError> {
    let variant = variant_of(media).ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
    let id = media["id"].as_str().unwrap_or_default().to_string();
    let mut resolved = Resolved::new(PLATFORM);
    resolved.title = post["title"]
        .as_str()
        .and_then(clean_title)
        .or_else(|| media["metadata"]["title"].as_str().and_then(clean_title))
        .or_else(|| media["name"].as_str().and_then(clean_title));
    resolved.description = post["description"]
        .as_str()
        .and_then(clean_title)
        .or_else(|| {
            media["metadata"]["description"]
                .as_str()
                .and_then(clean_title)
        });
    resolved.uploader = post["account"]["username"].as_str().and_then(clean_title);
    resolved.uploader_url = post["account"]["username"]
        .as_str()
        .and_then(|u| Url::parse(&format!("https://imgur.com/user/{u}")).ok());
    resolved.uploaded_at = media["created_at"]
        .as_str()
        .or_else(|| post["created_at"].as_str())
        .and_then(|t| t.parse::<Timestamp>().ok());
    resolved.duration = variant.duration;
    resolved.thumbnail = Url::parse(&format!("https://i.imgur.com/{id}l.jpg")).ok();
    resolved.webpage_url = post["url"]
        .as_str()
        .and_then(|u| Url::parse(u).ok())
        .or_else(|| Url::parse(&format!("https://imgur.com/{id}")).ok());
    resolved.age_limit = post["is_mature"].as_bool().filter(|m| *m).map(|_| 18);
    resolved.id = Some(id);
    resolved.variants = vec![variant];
    Ok(resolved)
}

#[async_trait]
impl Resolver for ImgurResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Imgur",
            hosts: &["imgur.com"],
            features: &[
                "videos",
                "animated images",
                "galleries",
                "albums",
                "direct links",
            ],
            formats: &["mp4"],
            session: SessionSupport::None,
            examples: &[
                "https://imgur.com/A61SaA1",
                "https://i.imgur.com/jxBXAMC.gifv",
                "https://imgur.com/gallery/YcAQlkx",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let post = self.post(&link, url).await?;
        let items: Vec<&Value> = post["media"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|m| playable(m))
            .collect();
        match items.as_slice() {
            [] => Err(ResolveError::NotFound(url.clone())),
            [only] => Ok(Resolution::from(resolved_of(&post, only, url)?)),
            many => {
                let entries = many
                    .iter()
                    .filter_map(|media| {
                        let id = media["id"].as_str()?;
                        Some(PlaylistEntry {
                            url: Url::parse(&format!("https://imgur.com/{id}")).ok()?,
                            title: media["metadata"]["title"]
                                .as_str()
                                .and_then(clean_title)
                                .or_else(|| media["name"].as_str().and_then(clean_title)),
                            duration: media["metadata"]["duration"]
                                .as_f64()
                                .filter(|d| *d > 0.0)
                                .map(Duration::from_secs_f64),
                        })
                    })
                    .collect();
                Ok(Resolution::Playlist(Playlist {
                    resolver: PLATFORM.to_string(),
                    id: post["id"].as_str().map(String::from),
                    title: post["title"].as_str().and_then(clean_title),
                    entries,
                    total: None,
                }))
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

    fn get(url: &str, status: u16, body: String) -> Exchange {
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
                headers: vec![("content-type".into(), "application/json".into())],
                body: RecordedBody::Text(body),
                truncated: false,
            },
        }
    }

    fn api(endpoint: &str, id: &str) -> String {
        format!(
            "https://api.imgur.com/post/v1/{endpoint}/{id}?client_id=546c25a59c58ad7&include=media,account"
        )
    }

    fn media(
        id: &str,
        kind: &str,
        mime: &str,
        animated: bool,
        sound: bool,
        duration: f64,
    ) -> Value {
        json!({
            "id": id, "type": kind, "mime_type": mime, "name": format!("clip {id}"),
            "url": format!("https://i.imgur.com/{id}.{}", if kind == "video" { "mp4" } else { "gif" }),
            "width": 720, "height": 720, "size": 4990315, "created_at": "2024-01-01T00:00:00Z",
            "metadata": {"title": "", "description": "", "is_animated": animated, "duration": duration, "has_sound": sound}
        })
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://imgur.com/A61SaA1"),
            Some(Link::Media("A61SaA1".into()))
        );
        assert_eq!(
            link("https://i.imgur.com/A61SaA1.gifv"),
            Some(Link::Media("A61SaA1".into()))
        );
        assert_eq!(
            link("https://imgur.com/mrw-gifv-is-up-running-without-any-bugs-A61SaA1"),
            Some(Link::Media("A61SaA1".into()))
        );
        assert_eq!(
            link("https://imgur.com/gallery/YcAQlkx"),
            Some(Link::Post("YcAQlkx".into()))
        );
        assert_eq!(
            link("https://imgur.com/gallery/classic-steve-carell-YcAQlkx"),
            Some(Link::Post("YcAQlkx".into()))
        );
        assert_eq!(
            link("https://imgur.com/a/j6Orj"),
            Some(Link::Post("j6Orj".into()))
        );
        assert_eq!(
            link("https://imgur.com/t/unmuted/6lAn9VQ"),
            Some(Link::Post("6lAn9VQ".into()))
        );
        assert_eq!(
            link("https://imgur.com/r/aww/YcAQlkx"),
            Some(Link::Post("YcAQlkx".into()))
        );
        assert_eq!(link("https://imgur.com/user/someone"), None);
        assert_eq!(link("https://imgur.com/upload"), None);
        assert_eq!(link("https://imgur.com/"), None);
    }

    #[tokio::test]
    async fn videos_and_animated_images_resolve_to_mp4() {
        let mut fixture = Fixture::new("imgur", None);
        fixture.exchanges.push(get(&api("media", "AgztyaZ"), 200, json!({
            "id": "AgztyaZ", "title": "A clip", "description": "", "is_album": false, "is_mature": true,
            "created_at": "2024-02-02T00:00:00Z", "url": "https://imgur.com/AgztyaZ", "account": {"username": "someone"},
            "media": [media("AgztyaZ", "video", "video/mp4", true, true, 56.433)]
        }).to_string()));
        fixture.exchanges.push(get(&api("media", "A61SaA1"), 200, json!({
            "id": "A61SaA1", "title": "MRW gifv works", "is_album": false, "is_mature": false, "account": null,
            "media": [media("A61SaA1", "image", "image/gif", true, false, 0.0)]
        }).to_string()));
        let resolver = ImgurResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://imgur.com/AgztyaZ").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.title.as_deref(), Some("A clip"));
        assert_eq!(resolved.uploader.as_deref(), Some("someone"));
        assert_eq!(resolved.age_limit, Some(18));
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(56.433)));
        assert_eq!(
            resolved.variants[0].url.as_str(),
            "https://i.imgur.com/AgztyaZ.mp4"
        );
        assert_eq!(resolved.variants[0].audio, Some(AudioCodec::Aac));
        assert_eq!(resolved.variants[0].size, Some(4990315));
        assert_eq!(
            resolved.thumbnail.unwrap().as_str(),
            "https://i.imgur.com/AgztyaZl.jpg"
        );

        let resolved = resolver
            .resolve(&Url::parse("https://i.imgur.com/A61SaA1.gifv").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.title.as_deref(), Some("MRW gifv works"));
        assert_eq!(
            resolved.variants[0].url.as_str(),
            "https://i.imgur.com/A61SaA1.mp4"
        );
        assert_eq!(resolved.variants[0].audio, None);
        assert_eq!(resolved.variants[0].size, None);
        assert_eq!(resolved.duration, None);
    }

    #[tokio::test]
    async fn galleries_become_playlists_or_their_only_video() {
        let mut fixture = Fixture::new("imgur", None);
        fixture.exchanges.push(get(&api("albums", "nEKBog5"), 200, json!({
            "id": "nEKBog5", "title": "Curated", "is_album": true, "is_mature": false, "account": {"username": "curator"},
            "media": [
                media("YNdIK4K", "video", "video/mp4", true, false, 6.367),
                media("2ixBxKK", "image", "image/jpeg", false, false, 0.0),
                media("6FzDctn", "video", "video/mp4", true, false, 9.8)
            ]
        }).to_string()));
        fixture.exchanges.push(get(&api("albums", "Q95ko"), 404, json!({"errors": [{"code": "404", "status": "Not Found", "detail": "album not found"}]}).to_string()));
        fixture.exchanges.push(get(
            &api("media", "Q95ko"),
            200,
            json!({
                "id": "Q95ko", "title": "", "is_album": false, "is_mature": false, "account": null,
                "media": [media("Q95ko", "image", "image/gif", true, false, 0.0)]
            })
            .to_string(),
        ));
        fixture.exchanges.push(get(&api("albums", "j6Orj"), 200, json!({
            "id": "j6Orj", "title": "Only stills", "is_album": true, "media": [media("2ixBxKK", "image", "image/jpeg", false, false, 0.0)]
        }).to_string()));
        fixture.exchanges.push(get(
            &api("media", "zzzzzzz"),
            404,
            json!({"errors": [{"detail": "media not found"}]}).to_string(),
        ));
        fixture.exchanges.push(get(
            &api("albums", "zzzzzzz"),
            404,
            json!({"errors": [{"detail": "album not found"}]}).to_string(),
        ));
        let resolver = ImgurResolver::new(Http::replay(fixture));
        let playlist = match resolver
            .resolve(&Url::parse("https://imgur.com/gallery/carefully-curated-nEKBog5").unwrap())
            .await
            .unwrap()
        {
            Resolution::Playlist(playlist) => playlist,
            other => panic!("{other:?}"),
        };
        assert_eq!(playlist.title.as_deref(), Some("Curated"));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://imgur.com/YNdIK4K"
        );
        assert_eq!(
            playlist.entries[1].duration,
            Some(Duration::from_secs_f64(9.8))
        );

        let resolved = resolver
            .resolve(&Url::parse("https://imgur.com/gallery/Q95ko").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(
            resolved.variants[0].url.as_str(),
            "https://i.imgur.com/Q95ko.mp4"
        );

        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://imgur.com/a/j6Orj").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://imgur.com/zzzzzzz").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
