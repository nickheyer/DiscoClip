//! ABC Owned Television Stations (abc7news.com, abc7ny.com, 6abc.com…): every story and
//! clip page names its content id, which the stations' content API answers with the
//! featured video's HLS playlist on Uplynk and its MP4 files.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Tag, Variant,
    clean_title, fetch, hls, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "abcotvs";
const API: &str = "https://api.abcotvs.com/v2/content";

/// The stations' sites and their call signs, which key the API.
const STATIONS: &[(&str, &str)] = &[
    ("6abc.com", "wpvi"),
    ("abc11.com", "wtvd"),
    ("abc13.com", "ktrk"),
    ("abc30.com", "kfsn"),
    ("abc7.com", "kabc"),
    ("abc7chicago.com", "wls"),
    ("abc7news.com", "kgo"),
    ("abc7ny.com", "wabc"),
];

/// `/{section…}/{display id}/{id}[/…]`: the content id is the last numeric segment.
static RE_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:/[^/?#]+)*?(?:/([^/?#]+))?/(\d+)(?:/[^?#]*)?$").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub station: &'static str,
    pub id: String,
    pub display_id: Option<String>,
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let station = STATIONS
        .iter()
        .find(|(site, _)| *site == host)
        .map(|(_, station)| *station)?;
    let caps = RE_PATH.captures(url.path())?;
    Some(Link {
        station,
        id: caps[2].to_string(),
        display_id: caps
            .get(1)
            .map(|m| m.as_str().to_string())
            .filter(|s| !s.chars().all(|c| c.is_ascii_digit())),
    })
}

/// The API request for `id` at `station`.
pub fn api_url(station: &str, id: &str) -> Url {
    util::with_query(
        &Url::parse(API).expect("valid"),
        &[
            ("id", id),
            ("key", &format!("otv.web.{station}.story")),
            ("station", station),
        ],
    )
}

/// An MP4 file of the video: the station's standard rendition is 640x360.
fn mp4_variant(url: Url, format_id: &str, standard: bool) -> Variant {
    let mut variant = Variant::file(url);
    variant.container = Some(Container::Mp4);
    variant.video = Some(VideoCodec::H264);
    variant.audio = Some(AudioCodec::Aac);
    variant.format_id = Some(format_id.to_string());
    if standard {
        variant.width = Some(640);
        variant.height = Some(360);
        variant.label = Some("360p".to_string());
    } else {
        variant.label = Some("hq".to_string());
    }
    variant
}

pub struct AbcotvsResolver {
    http: Http,
}

impl AbcotvsResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for AbcotvsResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "ABC Owned TV Stations",
            hosts: &[
                "abc7news.com",
                "abc7ny.com",
                "abc7chicago.com",
                "abc7.com",
                "abc11.com",
                "abc13.com",
                "abc30.com",
                "6abc.com",
            ],
            features: &["videos", "articles"],
            formats: &["hls", "mp4"],
            media: &[MediaKind::Video],
            tags: &[Tag::News],
            session: SessionSupport::None,
            examples: &[
                "https://abc7news.com/post/11k-worth-equipment-stolen-richmond-little-leagues-storage-container/19831881/",
                "https://abc7news.com/injured-san-jose-climber-crawls-to-safety-descending-mount-shasta/19834888/",
                "http://abc7news.com/entertainment/east-bay-museum-celebrates-vintage-synthesizers/472581/",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let api = api_url(link.station, &link.id);
        let accept = [("accept".to_string(), "application/json".to_string())];
        let fetched = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &accept, MAX_PAGE).await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        let answer = fetched.json(url)?;
        let data = &answer["data"];
        if data.is_null() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let video = match &data["featuredMedia"]["video"] {
            Value::Object(_) => &data["featuredMedia"]["video"],
            _ => data,
        };
        let mut resolved = Resolved::new(PLATFORM);
        let mut failure = None;
        if let Some(playlist) = util::url_of(&video["m3u8"], None) {
            // The playlist link carries ad parameters the player fills in; without them
            // Uplynk serves the plain programme.
            let mut plain = playlist.clone();
            plain.set_query(None);
            match hls::expand(&self.http, &plain, PLATFORM, BROWSER_UA, &[]).await {
                Ok(expanded) => {
                    resolved.variants.extend(expanded.variants);
                    resolved.subtitles.extend(expanded.subtitles);
                    resolved.duration = expanded.duration;
                    resolved.live |= expanded.live;
                }
                Err(error) => failure = Some(error),
            }
        }
        if let Some(mp4) = util::url_of(&video["hqMp4"], None) {
            resolved.variants.push(mp4_variant(mp4, "hq-mp4", false));
        }
        if let Some(mp4) = util::url_of(&video["mp4"], None) {
            resolved.variants.push(mp4_variant(mp4, "mp4", true));
        }
        if resolved.variants.is_empty() {
            return Err(failure.unwrap_or_else(|| ResolveError::NotFound(url.clone())));
        }
        resolved.id = util::text(&video["id"])
            .or_else(|| util::text(&video["publishedKey"]))
            .or_else(|| Some(link.id.clone()));
        resolved.title = video["title"]
            .as_str()
            .or(video["linkText"].as_str())
            .and_then(clean_title)
            .or_else(|| link.display_id.as_deref().and_then(clean_title));
        resolved.description = video["description"]
            .as_str()
            .or(video["caption"].as_str())
            .or(video["meta"]["description"].as_str())
            .and_then(clean_title);
        resolved.thumbnail = util::url_of(&video["image"]["source"], None)
            .or_else(|| util::url_of(&video["image"]["dynamicSource"], None));
        resolved.uploaded_at = util::epoch(&video["date"]);
        resolved.duration = util::seconds(&video["length"]).or(resolved.duration);
        resolved.uploader = Some(link.station.to_ascii_uppercase());
        resolved.webpage_url = util::url_of(&video["link"]["canonical"], None)
            .or_else(|| util::url_of(&data["link"]["canonical"], None))
            .or_else(|| Some(url.clone()));
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};
    use crate::resolve::VariantKind;
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

    const STORY: &str =
        "http://abc7news.com/entertainment/east-bay-museum-celebrates-vintage-synthesizers/472581/";

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link(STORY),
            Some(Link {
                station: "kgo",
                id: "472581".into(),
                display_id: Some("east-bay-museum-celebrates-vintage-synthesizers".into())
            })
        );
        assert_eq!(
            link("http://abc7news.com/472581"),
            Some(Link {
                station: "kgo",
                id: "472581".into(),
                display_id: None
            })
        );
        assert_eq!(
            link(
                "https://6abc.com/man-75-killed-after-being-struck-by-vehicle-in-chester/5725182/"
            ),
            Some(Link {
                station: "wpvi",
                id: "5725182".into(),
                display_id: Some("man-75-killed-after-being-struck-by-vehicle-in-chester".into())
            })
        );
        assert_eq!(
            link("https://www.abc7chicago.com/post/some-story/19831881/").map(|l| l.station),
            Some("wls")
        );
        assert_eq!(link("https://abc7news.com/"), None);
        assert_eq!(link("https://abc7news.com/weather/"), None);
        assert_eq!(link("https://abc.com/some-story/19831881/"), None);
        assert_eq!(
            api_url("kgo", "472581").as_str(),
            "https://api.abcotvs.com/v2/content?id=472581&key=otv.web.kgo.story&station=kgo"
        );
    }

    #[tokio::test]
    async fn stories_resolve_to_their_featured_video() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://api.abcotvs.com/v2/content?id=472581&key=otv.web.kgo.story&station=kgo",
            200,
            "application/json",
            json!({"data": {"id": 472581, "type": "post", "title": "Story", "link": {"canonical": "https://abc7news.com/east-bay-emeryville-museum-synthesizer/472581/"},
                "featuredMedia": {"video": {
                    "id": 472548, "title": "East Bay museum celebrates synthesized music", "linkText": "East Bay museum", "length": 8092, "date": 1421118520,
                    "description": "A new East Bay museum dedicated to vintage synthesizers.",
                    "m3u8": "https://content.uplynk.com/ext/4413/museum.m3u8?ad._v=2&ad.preroll=1",
                    "mp4": "https://dig.abclocal.go.com/kgo/video/2015/01/12/museum_500.mp4",
                    "image": {"source": "https://cdn.abcotvs.com/dip/images/472589.jpg"}
                }}}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://content.uplynk.com/ext/4413/museum.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1500000,RESOLUTION=1280x720\nhttps://content.uplynk.test/x/f.m3u8\n".into(),
        ));
        fixture.exchanges.push(get(
            "https://content.uplynk.test/x/f.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n0.ts\n#EXT-X-ENDLIST\n".into(),
        ));
        let resolver = AbcotvsResolver::new(Http::replay(fixture));
        let url = Url::parse(STORY).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("472548"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("East Bay museum celebrates synthesized music")
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(8092)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1421118520)
        );
        assert_eq!(resolved.uploader.as_deref(), Some("KGO"));
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://cdn.abcotvs.com/dip/images/472589.jpg"
        );
        assert_eq!(
            resolved.webpage_url.as_ref().unwrap().as_str(),
            "https://abc7news.com/east-bay-emeryville-museum-synthesizer/472581/"
        );
        assert_eq!(resolved.variants.len(), 2);
        assert_eq!(resolved.variants[0].kind, VariantKind::Hls);
        assert_eq!(resolved.variants[0].height, Some(720));
        assert_eq!(resolved.variants[1].kind, VariantKind::File);
        assert_eq!(resolved.variants[1].height, Some(360));
    }

    #[tokio::test]
    async fn clips_are_the_content_itself_and_missing_ids_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://api.abcotvs.com/v2/content?id=19834888&key=otv.web.kgo.story&station=kgo",
            200,
            "application/json",
            json!({"data": {"id": "19834888", "type": "videoClip", "linkText": "Injured climber crawls to safety", "length": "33", "date": "1789483078",
                "caption": "A San Jose climber crawled his way to a cabin.",
                "mp4": "https://vcl.abcotv.net/video/kgo/shasta.mp4", "hqMp4": "https://hq.vcl.abcotv.net/kgo/shasta.mp4",
                "image": {"dynamicSource": "https://cdn.abcotvs.com/dip/images/19834975.jpg"}}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://api.abcotvs.com/v2/content?id=1&key=otv.web.wpvi.story&station=wpvi",
            200,
            "application/json",
            json!({"data": null, "error": "not found"}).to_string(),
        ));
        let resolver = AbcotvsResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://abc7news.com/injured-climber/19834888/").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(
            resolved.title.as_deref(),
            Some("Injured climber crawls to safety")
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(33)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1789483078)
        );
        assert_eq!(resolved.variants.len(), 2);
        assert_eq!(resolved.variants[0].format_id.as_deref(), Some("hq-mp4"));
        assert_eq!(resolved.variants[1].width, Some(640));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://6abc.com/x/1/").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
