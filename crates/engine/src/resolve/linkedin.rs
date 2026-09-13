//! LinkedIn posts with video, from the page the site serves visitors: the player's
//! sources, poster, title, author and date.

use std::sync::LazyLock;

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use scraper::Selector;
use serde_json::Value;
use url::Url;

use super::page::{Page, ld_objects_of_type};
use super::web::parse_iso_duration;
use super::{
    ClipRange, MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionCheck,
    SessionSupport, Variant, VariantKind, clean_title, timestamp_hint,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "linkedin";
const SITE: &str = "https://www.linkedin.com/";
/// The cookie a logged-in linkedin.com session carries.
const SESSION_COOKIE: &str = "li_at";

static RE_ACTIVITY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?:activity|ugcPost)[-:](\d{15,})").unwrap());

/// A post, by its activity or UGC post number, with the page that names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostRef {
    pub id: String,
    pub page: Url,
}

pub fn parse_link(url: &Url) -> Option<PostRef> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !(host == "linkedin.com" || host.ends_with(".linkedin.com")) {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let id = match segments.as_slice() {
        ["posts", slug, ..] => RE_ACTIVITY.captures(slug).map(|c| c[1].to_string())?,
        ["feed", "update", urn, ..] => RE_ACTIVITY.captures(urn).map(|c| c[1].to_string())?,
        ["embed", "feed", "update", urn, ..] => {
            RE_ACTIVITY.captures(urn).map(|c| c[1].to_string())?
        }
        ["video", "live", urn, ..] => RE_ACTIVITY.captures(urn).map(|c| c[1].to_string())?,
        _ => return None,
    };
    let mut page = url.clone();
    page.set_fragment(None);
    Some(PostRef { id, page })
}

/// One source of the page's player.
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub url: Url,
    pub mime: Option<String>,
    pub bitrate: Option<u64>,
}

/// The player sources a post page carries, with its poster.
pub fn player_sources(page: &Page) -> (Vec<Source>, Option<Url>) {
    let selector = Selector::parse("video[data-sources], [data-sources]").expect("valid");
    let mut sources = Vec::new();
    let mut poster = None;
    for element in page.document().select(&selector) {
        if poster.is_none() {
            poster = element
                .value()
                .attr("data-poster-url")
                .and_then(|p| page.url().join(p).ok());
        }
        let Some(raw) = element.value().attr("data-sources") else {
            continue;
        };
        let Ok(list) = serde_json::from_str::<Value>(raw) else {
            continue;
        };
        for item in list.as_array().into_iter().flatten() {
            let Some(url) = item["src"].as_str().and_then(|s| page.url().join(s).ok()) else {
                continue;
            };
            if sources.iter().any(|s: &Source| s.url == url) {
                continue;
            }
            sources.push(Source {
                url,
                mime: item["type"].as_str().map(String::from),
                bitrate: item["data-bitrate"]
                    .as_u64()
                    .or_else(|| item["bitrate"].as_u64()),
            });
        }
        if !sources.is_empty() {
            break;
        }
    }
    (sources, poster)
}

pub struct LinkedinResolver {
    http: Http,
}

impl LinkedinResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    fn logged_in(&self) -> bool {
        self.http.jar(PLATFORM).get(SESSION_COOKIE).is_some()
    }
}

#[async_trait]
impl Resolver for LinkedinResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "LinkedIn",
            hosts: &["linkedin.com"],
            features: &["posts", "feed updates", "embeds"],
            formats: &["mp4"],
            session: SessionSupport::Optional,
            examples: &[
                "https://www.linkedin.com/posts/the-mathworks_2_what-is-mathworks-cloud-center-activity-7151241570371948544-4Gu7",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let post = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let response = self
            .http
            .get(post.page.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8")
            .header("accept-language", "en-US,en;q=0.9")
            .send()
            .await?;
        match response.status.as_u16() {
            200..=299 => {}
            404 | 410 => return Err(ResolveError::NotFound(url.clone())),
            429 => return Err(ResolveError::RateLimited(url.clone())),
            999 => return Err(ResolveError::RateLimited(url.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    url,
                    format!("the page answered HTTP {status}"),
                ));
            }
        }
        let final_url = response.url.clone();
        if final_url.path().contains("/authwall") || final_url.path().starts_with("/login") {
            return Err(if self.logged_in() {
                ResolveError::unavailable(url, "the site sent the request to its login wall")
            } else {
                ResolveError::login_required(url, PLATFORM, "the post is behind the login wall")
            });
        }
        let html = response.text(MAX_PAGE).await?;
        let page = Page::parse(&html, &final_url);
        let (sources, poster) = player_sources(&page);
        if sources.is_empty() {
            let has_player = html.contains("data-sources") || html.contains("video-player");
            return Err(if has_player {
                ResolveError::malformed(url, "the page's player names no sources")
            } else if html.contains("authwall") {
                ResolveError::login_required(url, PLATFORM, "the post is behind the login wall")
            } else {
                ResolveError::NotFound(url.clone())
            });
        }
        let ld = page.ld_json();
        let video_object = ld_objects_of_type(&ld, "VideoObject").into_iter().next();
        let duration = video_object
            .and_then(|v| v["duration"].as_str())
            .and_then(parse_iso_duration);
        let headers = vec![("referer".to_string(), SITE.to_string())];
        let variants: Vec<Variant> = sources
            .into_iter()
            .map(|source| {
                let mut v = Variant::new(source.url, VariantKind::File);
                v.container = Some(
                    source
                        .mime
                        .as_deref()
                        .and_then(Container::from_mime)
                        .unwrap_or(Container::Mp4),
                );
                v.video = Some(VideoCodec::H264);
                v.audio = Some(AudioCodec::Aac);
                v.bitrate = source.bitrate;
                v.duration = duration;
                v.height = v.url.path().split('/').find_map(|segment| {
                    segment
                        .strip_prefix("mp4-")
                        .and_then(|rest| rest.split_once('p'))
                        .and_then(|(height, _)| height.parse().ok())
                });
                v.headers = headers.clone();
                v
            })
            .collect();
        let og_title = page.meta("og:title").and_then(|t| clean_title(&t));
        let (title, author_from_title) = match &og_title {
            Some(title) => match title.rsplit_once(" | ") {
                Some((text, author)) => (clean_title(text), clean_title(author)),
                None => (Some(title.clone()), None),
            },
            None => (None, None),
        };
        let actor_selector = Selector::parse(
            "[data-tracking-control-name='public_post_feed-actor-name'], .update-components-actor__name, .base-main-feed-card__entity-lockup a",
        )
        .expect("valid");
        let actor = page.document().select(&actor_selector).next();
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(post.id.clone());
        resolved.title = title
            .clone()
            .or_else(|| video_object.and_then(|v| v["name"].as_str()).and_then(clean_title))
            .or_else(|| page.meta("description").and_then(|d| clean_title(&d)));
        resolved.description = page
            .meta("description")
            .or_else(|| page.meta("og:description"))
            .and_then(|d| clean_title(&d));
        resolved.uploader = actor
            .and_then(|a| clean_title(&a.text().collect::<String>()))
            .or(author_from_title);
        resolved.uploader_url = actor
            .and_then(|a| a.value().attr("href"))
            .and_then(|href| final_url.join(href).ok())
            .map(|mut u| {
                u.set_query(None);
                u
            });
        resolved.uploaded_at = video_object
            .and_then(|v| v["uploadDate"].as_str().or_else(|| v["datePublished"].as_str()))
            .or_else(|| {
                ld_objects_of_type(&ld, "SocialMediaPosting")
                    .into_iter()
                    .next()
                    .and_then(|p| p["datePublished"].as_str())
            })
            .and_then(|t| t.parse::<Timestamp>().ok())
            .or_else(|| {
                super::page::between(&html, "\"datePublished\":\"", "\"")
                    .and_then(|t| t.parse::<Timestamp>().ok())
            });
        resolved.duration = duration;
        resolved.thumbnail = poster.or_else(|| page.poster());
        resolved.webpage_url = page.canonical().or(Some(final_url));
        resolved.clip = timestamp_hint(url).map(|start| ClipRange { start, end: None });
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        if !self.logged_in() {
            return Ok(SessionCheck::LoggedOut);
        }
        let me = Url::parse(&format!("{SITE}voyager/api/me")).expect("valid");
        let csrf = self
            .http
            .jar(PLATFORM)
            .get("JSESSIONID")
            .map(|c| c.value.trim_matches('"').to_string())
            .unwrap_or_default();
        let response = self
            .http
            .get(me)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "application/vnd.linkedin.normalized+json+2.1")
            .header("csrf-token", &csrf)
            .header("x-restli-protocol-version", "2.0.0")
            .send()
            .await?;
        if !response.is_success() {
            return Ok(SessionCheck::LoggedOut);
        }
        let value: Value = match response.json(MAX_PAGE).await {
            Ok(value) => value,
            Err(_) => return Ok(SessionCheck::LoggedOut),
        };
        let profile = value["included"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|i| i["firstName"].is_string())
            .cloned()
            .unwrap_or(Value::Null);
        let name = [profile["firstName"].as_str(), profile["lastName"].as_str()]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" ");
        Ok(match clean_title(&name) {
            Some(name) => SessionCheck::LoggedIn { account: name },
            None => SessionCheck::LoggedIn {
                account: profile["publicIdentifier"]
                    .as_str()
                    .map(String::from)
                    .unwrap_or_else(|| "a LinkedIn account".into()),
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

    fn get(url: &str, status: u16, body: &str, final_url: &str) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: "GET".into(),
                url: url.into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status,
                url: final_url.into(),
                headers: vec![("content-type".into(), "text/html".into())],
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    const POST_URL: &str = "https://www.linkedin.com/posts/mishalkhawaja_sendinblueviews-toronto-digitalmarketing-ugcPost-6850898786781339649-mM20";

    const PAGE: &str = r##"<html><head>
      <meta property="og:title" content="#sendinblueviews #toronto #digitalmarketing | Mishal K.">
      <meta name="description" content="Sendinblue Toronto office!! We’re located in the heart of the city.">
      <meta property="og:image" content="https://static.licdn.com/og.png">
      <link rel="canonical" href="https://www.linkedin.com/posts/mishalkhawaja_sendinblueviews-toronto-digitalmarketing-activity-6850898794805018625-uwyp">
      <script type="application/ld+json">{"@context":"https://schema.org","@type":"VideoObject","name":"Sendinblue Toronto office","uploadDate":"2021-10-04T21:05:56.213Z","duration":"PT0M31S"}</script>
      </head><body>
      <a data-tracking-control-name="public_post_feed-actor-name" href="https://ca.linkedin.com/in/mishalkhawaja?trk=public_post_feed-actor-name">Mishal K.</a>
      <video data-poster-url="https://media.licdn.com/dms/image/C4E05AQG7hCp7zIeciw/feedshare-thumbnail_720_1280/0/1633381556990?e=1&amp;v=beta&amp;t=x" data-sources='[{"src":"https://dms.licdn.com/playlist/vid/v2/C4E05AQG7hCp7zIeciw/mp4-640p-30fp-crf28/mp4-640p-30fp-crf28/0/1633381557408?e=2147483647&amp;v=beta&amp;t=a","type":"video/mp4","data-bitrate":1001934},{"src":"https://dms.licdn.com/playlist/vid/v2/C4E05AQG7hCp7zIeciw/feedshare-ambry-analyzed_servable_progressive_video/0/1633381558795?e=2147483647&amp;v=beta&amp;t=b","type":"video/mp4","data-bitrate":5216064}]'></video>
      </body></html>"##;

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link(POST_URL).map(|p| p.id),
            Some("6850898786781339649".into())
        );
        assert_eq!(
            link("https://www.linkedin.com/feed/update/urn:li:activity:7151241570371948544/").map(|p| p.id),
            Some("7151241570371948544".into())
        );
        assert_eq!(
            link("https://www.linkedin.com/embed/feed/update/urn:li:ugcPost:6850898786781339649").map(|p| p.id),
            Some("6850898786781339649".into())
        );
        assert_eq!(link("https://www.linkedin.com/in/someone/"), None);
        assert_eq!(link("https://www.linkedin.com/posts/someone_no-id-here"), None);
    }

    #[tokio::test]
    async fn posts_resolve_from_the_visitor_page() {
        let mut fixture = Fixture::new("linkedin", None);
        fixture.exchanges.push(get(POST_URL, 200, PAGE, POST_URL));
        let resolver = LinkedinResolver::new(Http::replay(fixture));
        let url = Url::parse(POST_URL).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("6850898786781339649"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("#sendinblueviews #toronto #digitalmarketing")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("Mishal K."));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://ca.linkedin.com/in/mishalkhawaja"
        );
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.duration, Some(std::time::Duration::from_secs(31)));
        assert_eq!(resolved.variants.len(), 2);
        assert_eq!(resolved.variants[0].height, Some(640));
        assert_eq!(resolved.variants[0].bitrate, Some(1001934));
        assert_eq!(resolved.variants[1].bitrate, Some(5216064));
        assert!(resolved.variants[0].url.as_str().contains("t=a"));
        assert!(
            resolved
                .thumbnail
                .as_ref()
                .unwrap()
                .as_str()
                .starts_with("https://media.licdn.com/dms/image/")
        );
        assert!(
            resolved
                .webpage_url
                .as_ref()
                .unwrap()
                .as_str()
                .contains("activity-6850898794805018625")
        );
    }

    #[tokio::test]
    async fn the_login_wall_and_missing_posts_are_told() {
        let mut fixture = Fixture::new("linkedin", None);
        fixture.exchanges.push(get(
            "https://www.linkedin.com/posts/someone_activity-7000000000000000000-abcd",
            200,
            "<html>sign in</html>",
            "https://www.linkedin.com/authwall?trk=x",
        ));
        fixture.exchanges.push(get(
            "https://www.linkedin.com/posts/someone_activity-7000000000000000001-abcd",
            404,
            "<html>gone</html>",
            "https://www.linkedin.com/posts/someone_activity-7000000000000000001-abcd",
        ));
        let resolver = LinkedinResolver::new(Http::replay(fixture));
        let walled = resolver
            .resolve(&Url::parse("https://www.linkedin.com/posts/someone_activity-7000000000000000000-abcd").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(walled, ResolveError::LoginRequired { .. }),
            "{walled}"
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.linkedin.com/posts/someone_activity-7000000000000000001-abcd").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
