//! Snapchat Spotlight snaps and public stories, from the data the web pages render:
//! the snap's media file, its title, creator, length and time, with a story of several
//! snaps as a playlist.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use scraper::Selector;
use serde_json::Value;
use url::Url;

use super::page::Page;
use super::{
    ClipRange, MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionSupport, Variant, VariantKind, clean_title, fetch, timestamp_hint,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "snapchat";
const SITE: &str = "https://www.snapchat.com/";

static RE_SPOTLIGHT_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]{20,}$").unwrap());
static RE_USERNAME: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9._-]{3,}$").unwrap());

/// What a link names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Spotlight(String),
    /// A user's public story, or one snap of it by its position.
    Story { user: String, snap: Option<usize> },
    Short(Url),
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !(host == "snapchat.com" || host.ends_with(".snapchat.com")) {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let snap = url
        .fragment()
        .and_then(|f| f.strip_prefix("snap-"))
        .and_then(|n| n.parse::<usize>().ok())
        .filter(|n| *n >= 1);
    match segments.as_slice() {
        ["spotlight", id] if RE_SPOTLIGHT_ID.is_match(id) => Some(Link::Spotlight(id.to_string())),
        ["t", code] if !code.is_empty() => Some(Link::Short(url.clone())),
        ["add", user] | [user] if user.starts_with('@') || RE_USERNAME.is_match(user) => {
            let user = user.trim_start_matches('@');
            let reserved = ["spotlight", "t", "add", "stories", "discover", "lens", "lenses", "p", "explore", "l", "ads", "privacy", "terms", "download", "web"];
            (!reserved.contains(&user)).then(|| Link::Story {
                user: user.to_string(),
                snap,
            })
        }
        _ => None,
    }
}

/// The `__NEXT_DATA__` a page carries.
pub fn next_data(page: &Page) -> Option<Value> {
    let selector = Selector::parse("script#__NEXT_DATA__").expect("valid");
    let text: String = page.document().select(&selector).next()?.text().collect();
    serde_json::from_str(&text).ok()
}

/// A value the pages wrap as `{"value": …}`, or give plainly.
fn unwrap(value: &Value) -> &Value {
    if value.is_object() && value.get("value").is_some() && value.as_object().map(|m| m.len()) == Some(1) {
        &value["value"]
    } else {
        value
    }
}

fn as_str(value: &Value) -> Option<&str> {
    unwrap(value).as_str().filter(|s| !s.is_empty())
}

fn as_u64(value: &Value) -> Option<u64> {
    let value = unwrap(value);
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()))
        .filter(|n| *n > 0)
}

/// One snap of a story, as the page lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct Snap {
    pub id: Option<String>,
    pub media_url: Url,
    pub is_video: bool,
    pub preview: Option<Url>,
    pub timestamp: Option<Timestamp>,
    pub title: Option<String>,
}

/// The snaps of a `snapList`.
pub fn snaps_of(list: &Value) -> Vec<Snap> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|snap| {
            let media_url = as_str(&snap["snapUrls"]["mediaUrl"])
                .and_then(|u| Url::parse(u).ok())?;
            Some(Snap {
                id: as_str(&snap["snapId"]).map(String::from),
                media_url,
                is_video: snap["snapMediaType"].as_u64() == Some(1)
                    || snap["snapMediaType"].as_str() == Some("VIDEO"),
                preview: as_str(&snap["snapUrls"]["mediaPreviewUrl"]).and_then(|u| Url::parse(u).ok()),
                timestamp: as_u64(&snap["timestampInSec"])
                    .and_then(|t| Timestamp::from_second(t as i64).ok()),
                title: as_str(&snap["snapTitle"]).and_then(clean_title),
            })
        })
        .collect()
}

fn video_variant(url: Url, metadata: &Value) -> Variant {
    let mut v = Variant::new(url, VariantKind::File);
    v.container = Some(Container::Mp4);
    v.video = Some(VideoCodec::H264);
    v.audio = Some(AudioCodec::Aac);
    v.width = metadata["width"].as_u64().map(|w| w as u32).filter(|w| *w > 0);
    v.height = metadata["height"].as_u64().map(|h| h as u32).filter(|h| *h > 0);
    v.duration = as_u64(&metadata["durationMs"]).map(Duration::from_millis);
    v.headers = vec![("referer".to_string(), SITE.to_string())];
    v
}

pub struct SnapchatResolver {
    http: Http,
}

impl SnapchatResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    async fn page_props(&self, page_url: &Url, origin: &Url) -> Result<Value, ResolveError> {
        let fetched = fetch(&self.http, page_url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        match fetched.status.as_u16() {
            200..=299 => {}
            404 | 410 => return Err(ResolveError::NotFound(origin.clone())),
            429 => return Err(ResolveError::RateLimited(origin.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("the page answered HTTP {status}"),
                ));
            }
        }
        let html = fetched.text();
        let page = Page::parse(&html, &fetched.url);
        let data = next_data(&page)
            .ok_or_else(|| ResolveError::malformed(origin, "the page has no data"))?;
        Ok(data["props"]["pageProps"].clone())
    }

    async fn spotlight(&self, id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let page_url = Url::parse(&format!("{SITE}spotlight/{id}")).expect("valid");
        let props = self.page_props(&page_url, origin).await?;
        let metadata = &props["videoMetadata"];
        let stories = props["spotlightFeed"]["spotlightStories"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let story = stories
            .iter()
            .find(|s| as_str(&s["story"]["storyId"]) == Some(id))
            .or_else(|| stories.first())
            .cloned()
            .unwrap_or(Value::Null);
        let story_metadata = &story["metadata"]["videoMetadata"];
        let metadata = if as_str(&metadata["contentUrl"]).is_some() {
            metadata
        } else {
            story_metadata
        };
        let snaps = snaps_of(&story["story"]["snapList"]);
        let content_url = as_str(&metadata["contentUrl"])
            .and_then(|u| Url::parse(u).ok())
            .or_else(|| snaps.first().map(|s| s.media_url.clone()));
        let Some(content_url) = content_url else {
            return Err(if props["restricted"].as_bool() == Some(true) {
                ResolveError::unavailable(origin, "the snap is restricted")
            } else {
                ResolveError::NotFound(origin.clone())
            });
        };
        let creator = &metadata["creator"];
        let person = if creator["personCreator"].is_object() {
            &creator["personCreator"]
        } else if creator["publisherCreator"].is_object() {
            &creator["publisherCreator"]
        } else {
            creator
        };
        let variant = video_variant(content_url, metadata);
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.to_string());
        resolved.title = as_str(&metadata["description"])
            .and_then(clean_title)
            .filter(|d| !d.starts_with('#') || d.len() > 12)
            .or_else(|| as_str(&metadata["name"]).and_then(clean_title))
            .or_else(|| as_str(&metadata["description"]).and_then(clean_title));
        resolved.description = as_str(&metadata["description"]).and_then(clean_title);
        resolved.uploader = as_str(&person["name"])
            .and_then(clean_title)
            .or_else(|| as_str(&person["username"]).map(|u| format!("@{u}")));
        resolved.uploader_url = as_str(&person["url"])
            .and_then(|u| Url::parse(u).ok())
            .or_else(|| {
                as_str(&person["username"])
                    .and_then(|u| Url::parse(&format!("{SITE}add/{u}")).ok())
            });
        resolved.uploaded_at = as_u64(&metadata["uploadDateMs"])
            .and_then(|t| Timestamp::from_millisecond(t as i64).ok())
            .or_else(|| snaps.first().and_then(|s| s.timestamp));
        resolved.duration = variant.duration;
        resolved.thumbnail = as_str(&metadata["thumbnailUrl"])
            .and_then(|u| Url::parse(u).ok())
            .or_else(|| snaps.first().and_then(|s| s.preview.clone()));
        resolved.webpage_url = Some(page_url);
        resolved.clip = timestamp_hint(origin).map(|start| ClipRange { start, end: None });
        resolved.variants = vec![variant];
        Ok(Resolution::from(resolved))
    }

    async fn story(&self, user: &str, snap: Option<usize>, origin: &Url) -> Result<Resolution, ResolveError> {
        let page_url = Url::parse(&format!("{SITE}add/{user}")).expect("valid");
        let props = self.page_props(&page_url, origin).await?;
        let mut snaps: Vec<Snap> = snaps_of(&props["story"]["snapList"]);
        for highlight in props["curatedHighlights"].as_array().into_iter().flatten() {
            snaps.extend(snaps_of(&highlight["snapList"]));
        }
        let videos: Vec<Snap> = snaps.into_iter().filter(|s| s.is_video).collect();
        if videos.is_empty() {
            return Err(if props["userProfile"].is_object() || props["story"].is_object() {
                ResolveError::unavailable(origin, "the profile shows no public video snaps")
            } else {
                ResolveError::NotFound(origin.clone())
            });
        }
        let profile = &props["userProfile"];
        let display = as_str(&profile["displayName"])
            .or_else(|| as_str(&profile["publicProfileInfo"]["title"]))
            .and_then(clean_title);
        let page = Url::parse(&format!("{SITE}add/{user}")).expect("valid");
        if videos.len() > 1 && snap.is_none() {
            let entries = videos
                .iter()
                .enumerate()
                .map(|(index, s)| {
                    let mut entry = page.clone();
                    entry.set_fragment(Some(&format!("snap-{}", index + 1)));
                    PlaylistEntry {
                        url: entry,
                        title: s.title.clone().or_else(|| {
                            Some(format!("{} ({})", display.clone().unwrap_or_else(|| format!("@{user}")), index + 1))
                        }),
                        duration: None,
                    }
                })
                .collect::<Vec<_>>();
            return Ok(Resolution::Playlist(Playlist {
                resolver: PLATFORM.into(),
                id: Some(format!("story-{user}")),
                title: display.map(|d| format!("{d}'s story")).or_else(|| Some(format!("@{user}'s story"))),
                total: Some(entries.len()),
                entries,
            }));
        }
        let index = snap.unwrap_or(1);
        let chosen = videos
            .get(index - 1)
            .ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
        let variant = video_variant(chosen.media_url.clone(), &Value::Null);
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = chosen
            .id
            .clone()
            .or_else(|| Some(format!("story-{user}-{index}")));
        resolved.title = chosen
            .title
            .clone()
            .or_else(|| display.clone().map(|d| format!("{d}'s story ({index})")))
            .or_else(|| Some(format!("@{user}'s story ({index})")));
        resolved.uploader = display.or_else(|| Some(format!("@{user}")));
        resolved.uploader_url = Some(page.clone());
        resolved.uploaded_at = chosen.timestamp;
        resolved.thumbnail = chosen.preview.clone();
        resolved.webpage_url = Some(page);
        resolved.clip = timestamp_hint(origin).map(|start| ClipRange { start, end: None });
        resolved.variants = vec![variant];
        Ok(Resolution::from(resolved))
    }
}

#[async_trait]
impl Resolver for SnapchatResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Snapchat",
            hosts: &["snapchat.com"],
            features: &["spotlight", "public stories", "short links"],
            formats: &["mp4"],
            session: SessionSupport::None,
            examples: &[
                "https://www.snapchat.com/spotlight/W7_EDlXWTBiXAEEniNoMPwAAYb2lpY3VwYWFlAZ6JW-cbAZ6JW-bhAAAAAQ",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Spotlight(id) => self.spotlight(&id, url).await,
            Link::Story { user, snap } => self.story(&user, snap, url).await,
            Link::Short(short) => {
                let target = self
                    .http
                    .unwrap_redirects(&short, Some(PLATFORM), BROWSER_UA)
                    .await?;
                match parse_link(&target) {
                    Some(Link::Spotlight(id)) => self.spotlight(&id, url).await,
                    Some(Link::Story { user, snap }) => self.story(&user, snap, url).await,
                    _ => Err(ResolveError::NotFound(url.clone())),
                }
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

    fn get(url: &str, body: &str) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: "GET".into(),
                url: url.into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status: 200,
                url: url.into(),
                headers: vec![("content-type".into(), "text/html".into())],
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    fn page(props: Value) -> String {
        let data = json!({"props": {"pageProps": props}, "page": "/spotlight/[id]"});
        format!(r#"<html><body><script id="__NEXT_DATA__" type="application/json">{data}</script></body></html>"#)
    }

    const ID: &str = "W7_EDlXWTBiXAEEniNoMPwAAYb2lpY3VwYWFlAZ6JW-cbAZ6JW-bhAAAAAQ";

    fn spotlight_props() -> Value {
        let metadata = json!({"name": "Spotlight Snap", "description": "#483", "thumbnailUrl": "https://cf-st.sc-cdn.net/d/thumb.256.jpg", "uploadDateMs": "1780420962017",
            "contentUrl": "https://cf-st.sc-cdn.net/d/wMPCwEBDbiMcT9padGGaM.27.IRZXSOY?mo=x", "creator": {"$case": "personCreator", "personCreator": {"username": "alessandraxox0", "url": "https://www.snapchat.com/@alessandraxox0", "name": "Alessandra Cruz"}},
            "durationMs": "8620", "width": 540, "height": 960});
        json!({"videoMetadata": metadata, "restricted": false, "spotlightFeed": {"spotlightStories": [
            {"story": {"storyId": {"value": ID}, "snapList": [{"snapId": {"value": ID}, "snapMediaType": 1, "snapUrls": {"mediaUrl": "https://cf-st.sc-cdn.net/d/wMPCwEBDbiMcT9padGGaM.27.IRZXSOY?mo=x", "mediaPreviewUrl": {"value": "https://cf-st.sc-cdn.net/d/thumb.256.jpg"}}, "timestampInSec": {"value": "1780420962"}}]}, "metadata": {"videoMetadata": metadata}}
        ]}})
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link(&format!("https://www.snapchat.com/spotlight/{ID}")),
            Some(Link::Spotlight(ID.into()))
        );
        assert_eq!(
            link("https://www.snapchat.com/add/snapchat"),
            Some(Link::Story {
                user: "snapchat".into(),
                snap: None
            })
        );
        assert_eq!(
            link("https://www.snapchat.com/@snapchat#snap-2"),
            Some(Link::Story {
                user: "snapchat".into(),
                snap: Some(2)
            })
        );
        assert!(matches!(link("https://www.snapchat.com/t/abc123"), Some(Link::Short(_))));
        assert_eq!(link("https://www.snapchat.com/spotlight/short"), None);
        assert_eq!(link("https://www.snapchat.com/discover"), None);
    }

    #[tokio::test]
    async fn spotlight_snaps_resolve_from_the_page_data() {
        let mut fixture = Fixture::new("snapchat", None);
        fixture.exchanges.push(get(&format!("https://www.snapchat.com/spotlight/{ID}"), &page(spotlight_props())));
        let resolver = SnapchatResolver::new(Http::replay(fixture));
        let url = Url::parse(&format!("https://www.snapchat.com/spotlight/{ID}")).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some(ID));
        assert_eq!(resolved.title.as_deref(), Some("Spotlight Snap"));
        assert_eq!(resolved.description.as_deref(), Some("#483"));
        assert_eq!(resolved.uploader.as_deref(), Some("Alessandra Cruz"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://www.snapchat.com/@alessandraxox0"
        );
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.duration, Some(Duration::from_millis(8620)));
        assert_eq!(resolved.variants[0].height, Some(960));
        assert!(resolved.thumbnail.is_some());
    }

    #[tokio::test]
    async fn stories_list_their_video_snaps_and_empty_pages_are_missing() {
        let story = json!({"userProfile": {"displayName": "Snapchat", "username": "snapchat"}, "story": {"snapList": [
            {"snapId": {"value": "s1"}, "snapMediaType": 1, "snapUrls": {"mediaUrl": "https://cf-st.sc-cdn.net/d/one.mp4"}, "timestampInSec": {"value": "1780000000"}},
            {"snapId": {"value": "s2"}, "snapMediaType": 0, "snapUrls": {"mediaUrl": "https://cf-st.sc-cdn.net/d/image.jpg"}},
            {"snapId": {"value": "s3"}, "snapMediaType": 1, "snapUrls": {"mediaUrl": "https://cf-st.sc-cdn.net/d/three.mp4"}, "snapTitle": {"value": "Third"}}
        ]}});
        let mut fixture = Fixture::new("snapchat", None);
        fixture.exchanges.push(get("https://www.snapchat.com/add/snapchat", &page(story.clone())));
        fixture.exchanges.push(get("https://www.snapchat.com/add/snapchat", &page(story)));
        fixture.exchanges.push(get(&format!("https://www.snapchat.com/spotlight/{ID}"), &page(json!({"videoMetadata": {"contentUrl": ""}, "spotlightFeed": {"spotlightStories": [{"story": {"snapList": []}}]}}))));
        let resolver = SnapchatResolver::new(Http::replay(fixture));
        let playlist = match resolver
            .resolve(&Url::parse("https://www.snapchat.com/add/snapchat").unwrap())
            .await
            .unwrap()
        {
            Resolution::Playlist(playlist) => playlist,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(playlist.title.as_deref(), Some("Snapchat's story"));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(playlist.entries[1].title.as_deref(), Some("Third"));
        let third = resolver
            .resolve(&playlist.entries[1].url)
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(third.id.as_deref(), Some("s3"));
        assert_eq!(third.variants[0].url.as_str(), "https://cf-st.sc-cdn.net/d/three.mp4");
        assert!(matches!(
            resolver
                .resolve(&Url::parse(&format!("https://www.snapchat.com/spotlight/{ID}")).unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
