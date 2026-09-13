//! Pinterest video pins and idea pins, through the resource API the site's own pages
//! call, with `pin.it` short links unwrapped.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};
use url::Url;

use super::{
    ClipRange, MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionSupport, Variant, VariantKind, clean_title, fetch, timestamp_hint,
};
use crate::http::cookies::parse_http_date;
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "pinterest";
const SITE: &str = "https://www.pinterest.com/";
const PIN_RESOURCE: &str = "https://www.pinterest.com/resource/PinResource/get/";

static RE_PIN_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{6,}$").unwrap());

/// What a link names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Pin {
        id: String,
        /// The page of an idea pin the link picks out.
        page: Option<usize>,
    },
    Short(Url),
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
    if host == "pin.it" {
        return (!segments.is_empty()).then(|| Link::Short(url.clone()));
    }
    let pinterest = host == "pinterest.com"
        || host.ends_with(".pinterest.com")
        || host.starts_with("pinterest.")
        || host.contains(".pinterest.");
    if !pinterest {
        return None;
    }
    match segments.as_slice() {
        ["pin", id, ..] if RE_PIN_ID.is_match(id) => Some(Link::Pin {
            id: id.to_string(),
            page: url
                .fragment()
                .and_then(|f| f.strip_prefix("page-"))
                .and_then(|n| n.parse().ok())
                .filter(|n: &usize| *n >= 1),
        }),
        _ => None,
    }
}

pub struct PinterestResolver {
    http: Http,
}

impl PinterestResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// The pin's record through the resource API.
    async fn pin(&self, id: &str, origin: &Url) -> Result<Value, ResolveError> {
        let mut url = Url::parse(PIN_RESOURCE).expect("valid");
        let data = json!({"options": {"field_set_key": "unauth_react_main_pin", "id": id}, "context": {}});
        url.query_pairs_mut()
            .append_pair("source_url", &format!("/pin/{id}/"))
            .append_pair("data", &data.to_string());
        let headers = [
            ("x-pinterest-pws-handler".to_string(), "www/pin/[id].js".to_string()),
            ("x-requested-with".to_string(), "XMLHttpRequest".to_string()),
            ("accept".to_string(), "application/json, text/javascript, */*, q=0.01".to_string()),
            ("referer".to_string(), format!("{SITE}pin/{id}/")),
        ];
        let fetched = fetch(&self.http, &url, PLATFORM, BROWSER_UA, &headers, MAX_PAGE).await?;
        if fetched.status.as_u16() == 429 {
            return Err(ResolveError::RateLimited(origin.clone()));
        }
        let answer = fetched.json(origin)?;
        let response = &answer["resource_response"];
        if let Some(error) = response.get("error").filter(|e| e.is_object()) {
            let code = error["http_status"]
                .as_u64()
                .unwrap_or(fetched.status.as_u16() as u64);
            let message = error["message"]
                .as_str()
                .unwrap_or("the API answered with an error");
            return Err(match code {
                404 => ResolveError::NotFound(origin.clone()),
                429 => ResolveError::RateLimited(origin.clone()),
                _ => ResolveError::unavailable(origin, format!("{message} (HTTP {code})")),
            });
        }
        if !fetched.status.is_success() {
            return Err(ResolveError::unavailable(
                origin,
                format!("the API answered HTTP {}", fetched.status),
            ));
        }
        let data = &response["data"];
        if !data.is_object() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        Ok(data.clone())
    }
}

/// The variants a `video_list` lists: HLS playlists first, then MP4 files by name,
/// without repeats of one playlist under several names.
pub fn variants_of(video_list: &Value) -> Vec<Variant> {
    let headers = vec![("referer".to_string(), SITE.to_string())];
    let mut variants: Vec<Variant> = Vec::new();
    let mut entries: Vec<(&String, &Value)> = video_list
        .as_object()
        .into_iter()
        .flat_map(|m| m.iter())
        .collect();
    let is_playlist = |name: &str, entry: &Value| {
        name.contains("HLS") || entry["url"].as_str().is_some_and(|u| u.contains(".m3u8"))
    };
    entries.sort_by(|a, b| {
        is_playlist(b.0, b.1)
            .cmp(&is_playlist(a.0, a.1))
            .then_with(|| a.0.cmp(b.0))
    });
    for (name, entry) in entries {
        let Some(url) = entry["url"].as_str().and_then(|u| Url::parse(u).ok()) else {
            continue;
        };
        if variants.iter().any(|v| v.url == url) {
            continue;
        }
        let playlist = url.path().ends_with(".m3u8") || name.contains("HLS");
        let mut v = Variant::new(
            url,
            if playlist {
                VariantKind::Hls
            } else {
                VariantKind::File
            },
        );
        if !playlist {
            v.container = Some(Container::Mp4);
            v.video = Some(VideoCodec::H264);
            v.audio = Some(AudioCodec::Aac);
        }
        v.width = entry["width"].as_u64().map(|w| w as u32);
        v.height = entry["height"].as_u64().map(|h| h as u32);
        v.duration = entry["duration"]
            .as_u64()
            .filter(|d| *d > 0)
            .map(Duration::from_millis);
        v.format_id = Some(name.clone());
        v.label = name.strip_prefix("V_").map(|l| l.replace('_', " "));
        v.headers = headers.clone();
        variants.push(v);
    }
    variants
}

/// The video lists of an idea pin's pages, in order.
fn story_videos(pin: &Value) -> Vec<&Value> {
    pin["story_pin_data"]["pages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|page| {
            page["blocks"]
                .as_array()
                .into_iter()
                .flatten()
                .find_map(|block| {
                    let list = &block["video"]["video_list"];
                    list.is_object().then_some(list)
                })
        })
        .collect()
}

fn thumbnail_of(pin: &Value, video_list: Option<&Value>) -> Option<Url> {
    video_list
        .and_then(|list| {
            list.as_object()
                .and_then(|m| m.values().find_map(|v| v["thumbnail"].as_str()))
        })
        .or_else(|| pin["images"]["orig"]["url"].as_str())
        .and_then(|u| Url::parse(u).ok())
}

#[async_trait]
impl Resolver for PinterestResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Pinterest",
            hosts: &["pinterest.com", "pin.it"],
            features: &["video pins", "idea pins", "short links"],
            formats: &["mp4", "hls"],
            session: SessionSupport::None,
            examples: &["https://www.pinterest.com/pin/4855512095534420/"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let (id, page) = match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Pin { id, page } => (id, page),
            Link::Short(short) => {
                let target = self
                    .http
                    .unwrap_redirects(&short, Some(PLATFORM), BROWSER_UA)
                    .await?;
                match parse_link(&target) {
                    Some(Link::Pin { id, page }) => (id, page),
                    _ => return Err(ResolveError::NotFound(url.clone())),
                }
            }
        };
        let pin = self.pin(&id, url).await?;
        let pinner = &pin["pinner"];
        let username = pinner["username"].as_str().unwrap_or("");
        let mut base = Resolved::new(PLATFORM);
        base.id = Some(id.clone());
        base.title = ["title", "grid_title", "closeup_unified_description", "description"]
            .iter()
            .find_map(|k| pin[k].as_str().and_then(clean_title))
            .or_else(|| (!username.is_empty()).then(|| format!("Pin by {username}")));
        base.description = pin["closeup_unified_description"]
            .as_str()
            .or_else(|| pin["description"].as_str())
            .and_then(clean_title);
        base.uploader = pinner["full_name"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| clean_title(username));
        base.uploader_url = (!username.is_empty())
            .then(|| Url::parse(&format!("{SITE}{username}/")).ok())
            .flatten();
        base.uploaded_at = pin["created_at"].as_str().and_then(parse_http_date);
        base.webpage_url = Url::parse(&format!("{SITE}pin/{id}/")).ok();
        base.clip = timestamp_hint(url).map(|start| ClipRange { start, end: None });

        let own = &pin["videos"]["video_list"];
        if own.is_object() && page.is_none() {
            let variants = variants_of(own);
            if !variants.is_empty() {
                let mut resolved = base;
                resolved.duration = variants.iter().find_map(|v| v.duration);
                resolved.thumbnail = thumbnail_of(&pin, Some(own));
                resolved.variants = variants;
                return Ok(Resolution::from(resolved));
            }
        }
        let pages = story_videos(&pin);
        if pages.is_empty() {
            return Err(if pin["images"].is_object() {
                ResolveError::unavailable(url, "the pin is an image, not a video")
            } else {
                ResolveError::NotFound(url.clone())
            });
        }
        if pages.len() > 1 && page.is_none() {
            let entries = pages
                .iter()
                .enumerate()
                .filter_map(|(index, list)| {
                    let mut entry = base.webpage_url.clone()?;
                    entry.set_fragment(Some(&format!("page-{}", index + 1)));
                    Some(PlaylistEntry {
                        url: entry,
                        title: base.title.as_ref().map(|t| format!("{t} ({})", index + 1)),
                        duration: variants_of(list).iter().find_map(|v| v.duration),
                    })
                })
                .collect::<Vec<_>>();
            return Ok(Resolution::Playlist(Playlist {
                resolver: PLATFORM.into(),
                id: base.id,
                title: base.title,
                total: Some(entries.len()),
                entries,
            }));
        }
        let index = page.unwrap_or(1);
        let list = pages
            .get(index - 1)
            .copied()
            .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let variants = variants_of(list);
        if variants.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let mut resolved = base;
        if pages.len() > 1 {
            resolved.id = Some(format!("{id}-{index}"));
        }
        resolved.duration = variants.iter().find_map(|v| v.duration);
        resolved.thumbnail = thumbnail_of(&pin, Some(list));
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

    fn get(url: &str, status: u16, body: &str, headers: &[(&str, &str)]) -> Exchange {
        let mut all: Vec<(String, String)> = vec![("content-type".into(), "application/json".into())];
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

    fn answer(data: Value) -> String {
        json!({"resource_response": {"status": "success", "code": 0, "message": "ok", "data": data}}).to_string()
    }

    fn video_pin() -> Value {
        json!({
            "id": "4855512095534420", "title": "Cute Cat Aesthetic 🐾", "description": "This cute cat video will instantly make your day better", "created_at": "Fri, 13 Mar 2026 17:57:09 +0000",
            "pinner": {"username": "caitlin2235", "full_name": "Caitlin Leavitt"}, "images": {"orig": {"url": "https://i.pinimg.com/originals/8e/07/e8.jpg"}},
            "videos": {"video_list": {
                "V_720P": {"url": "https://v1.pinimg.com/videos/iht/expMp4/bb/e6/07/bbe6_720w.mp4", "width": 576, "height": 1024, "duration": 24300, "thumbnail": "https://i.pinimg.com/videos/thumbnails/bbe6.jpg"},
                "V_HLSV4": {"url": "https://v1.pinimg.com/videos/iht/hls/bb/e6/07/bbe6.m3u8", "width": 576, "height": 1024, "duration": 24300},
                "V_HLSV3_MOBILE": {"url": "https://v1.pinimg.com/videos/iht/hls/bb/e6/07/bbe6.m3u8", "width": 576, "height": 1024, "duration": 24300}
            }},
            "story_pin_data": null
        })
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://www.pinterest.com/pin/4855512095534420/"),
            Some(Link::Pin {
                id: "4855512095534420".into(),
                page: None
            })
        );
        assert_eq!(
            link("https://pinterest.co.uk/pin/4855512095534420/#page-2"),
            Some(Link::Pin {
                id: "4855512095534420".into(),
                page: Some(2)
            })
        );
        assert!(matches!(link("https://pin.it/1abc"), Some(Link::Short(_))));
        assert_eq!(link("https://www.pinterest.com/caitlin2235/"), None);
        assert_eq!(link("https://www.pinterest.com/pin/abc/"), None);
    }

    #[tokio::test]
    async fn video_pins_resolve_through_the_resource_api() {
        let mut fixture = Fixture::new("pinterest", None);
        fixture.exchanges.push(get(PIN_RESOURCE, 200, &answer(video_pin()), &[]));
        let resolver = PinterestResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.pinterest.com/pin/4855512095534420/").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("4855512095534420"));
        assert_eq!(resolved.title.as_deref(), Some("Cute Cat Aesthetic 🐾"));
        assert_eq!(resolved.uploader.as_deref(), Some("Caitlin Leavitt"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://www.pinterest.com/caitlin2235/"
        );
        assert_eq!(
            resolved.uploaded_at.unwrap(),
            "2026-03-13T17:57:09Z".parse::<jiff::Timestamp>().unwrap()
        );
        assert_eq!(resolved.duration, Some(Duration::from_millis(24300)));
        // The MP4 and one HLS playlist; the second name of the same playlist is dropped.
        assert_eq!(resolved.variants.len(), 2);
        assert_eq!(resolved.variants[0].kind, VariantKind::Hls);
        assert_eq!(resolved.variants[1].kind, VariantKind::File);
        assert_eq!(resolved.variants[1].height, Some(1024));
        assert_eq!(resolved.variants[1].label.as_deref(), Some("720P"));
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://i.pinimg.com/videos/thumbnails/bbe6.jpg"
        );
    }

    #[tokio::test]
    async fn idea_pins_short_links_and_missing_pins() {
        let story = json!({
            "id": "1089589703629338745", "title": "Two pages", "pinner": {"username": "someone"}, "images": {"orig": {"url": "https://i.pinimg.com/originals/a.jpg"}},
            "videos": null,
            "story_pin_data": {"pages": [
                {"blocks": [{"block_type": 3, "video": {"video_list": {"V_HLSV3_MOBILE": {"width": 720, "height": 1280, "duration": 10000, "url": "https://v1.pinimg.com/videos/iht/hls/9a/one.m3u8", "thumbnail": "https://i.pinimg.com/videos/thumbnails/one.jpg"}}}}]},
                {"blocks": [{"block_type": 1, "text": "just text"}]},
                {"blocks": [{"block_type": 3, "video": {"video_list": {"V_EXP7": {"width": 720, "height": 1280, "duration": 12000, "url": "https://v1.pinimg.com/videos/iht/expMp4/two.mp4"}}}}]}
            ]}
        });
        let mut fixture = Fixture::new("pinterest", None);
        fixture.exchanges.push(get("https://pin.it/1abc", 308, "", &[("location", "https://api.pinterest.com/url_shortener/1abc/redirect/")]));
        fixture.exchanges.push(get("https://api.pinterest.com/url_shortener/1abc/redirect/", 302, "", &[("location", "https://www.pinterest.com/pin/1089589703629338745/?invite_code=x")]));
        fixture.exchanges.push(get("https://www.pinterest.com/pin/1089589703629338745/?invite_code=x", 200, "<html></html>", &[("content-type", "text/html")]));
        fixture.exchanges.push(get(PIN_RESOURCE, 200, &answer(story.clone()), &[]));
        fixture.exchanges.push(get(PIN_RESOURCE, 200, &answer(story), &[]));
        fixture.exchanges.push(get(
            PIN_RESOURCE,
            404,
            &json!({"resource_response": {"error": {"status": "failure", "http_status": 404, "code": 50, "message": "Pin not found."}}}).to_string(),
            &[],
        ));
        let resolver = PinterestResolver::new(Http::replay(fixture));
        let playlist = match resolver
            .resolve(&Url::parse("https://pin.it/1abc").unwrap())
            .await
            .unwrap()
        {
            Resolution::Playlist(playlist) => playlist,
            other => panic!("expected a playlist, got {other:?}"),
        };
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[1].url.as_str(),
            "https://www.pinterest.com/pin/1089589703629338745/#page-2"
        );
        assert_eq!(playlist.entries[1].duration, Some(Duration::from_millis(12000)));
        let second = resolver
            .resolve(&playlist.entries[1].url)
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(second.id.as_deref(), Some("1089589703629338745-2"));
        assert_eq!(second.variants[0].kind, VariantKind::File);
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.pinterest.com/pin/1/").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
