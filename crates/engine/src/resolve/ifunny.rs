//! iFunny videos and GIF posts, from the page the site renders: the player's file, its
//! size, the poster, the creator and the time, with a picture post reported as one.

use async_trait::async_trait;
use jiff::Timestamp;
use scraper::Selector;
use serde_json::Value;
use url::Url;

use super::page::{Page, leading_json};
use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Variant,
    VariantKind, clean_title, fetch, navigation_headers,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, VideoCodec};

pub const PLATFORM: &str = "ifunny";

/// What a link names: a post of a kind, by its slug, whose id is the slug's last part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub kind: String,
    pub slug: String,
    pub id: String,
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "ifunny.co" && !host.ends_with(".ifunny.co") {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    match segments.as_slice() {
        [
            kind @ ("video" | "gif" | "picture" | "fun" | "meme"),
            slug,
            ..,
        ] => {
            let id = slug.rsplit('-').next().unwrap_or(slug);
            (id.len() >= 6 && id.chars().all(|c| c.is_ascii_alphanumeric())).then(|| Link {
                kind: kind.to_string(),
                slug: slug.to_string(),
                id: id.to_string(),
            })
        }
        _ => None,
    }
}

/// The post's record in the page's initial state: the object carrying `"id":"<id>"`.
pub fn content_in(html: &str, id: &str) -> Option<Value> {
    let state_at = html.find("window.__INITIAL_STATE__")?;
    let state = &html[state_at..];
    let needle = format!("\"id\":\"{id}\"");
    let mut from = 0;
    while let Some(found) = state[from..].find(&needle) {
        let at = from + found;
        // Walk back to the object that holds the id.
        let mut depth = 0i32;
        let mut begin = None;
        for (index, ch) in state[..at].char_indices().rev() {
            match ch {
                '}' => depth += 1,
                '{' => {
                    if depth == 0 {
                        begin = Some(index);
                        break;
                    }
                    depth -= 1;
                }
                _ => {}
            }
        }
        if let Some(begin) = begin
            && let Some((value, _)) = leading_json(&state[begin..])
            && value["url"].is_string()
        {
            return Some(value);
        }
        from = at + needle.len();
    }
    None
}

pub struct IfunnyResolver {
    http: Http,
}

impl IfunnyResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for IfunnyResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "iFunny",
            hosts: &["ifunny.co"],
            features: &["videos", "gif posts"],
            formats: &["mp4"],
            session: SessionSupport::None,
            examples: &["https://ifunny.co/video/veclHKeeD"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let page_url =
            Url::parse(&format!("https://ifunny.co/{}/{}", link.kind, link.slug)).expect("valid");
        let fetched = fetch(
            &self.http,
            &page_url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        match fetched.status.as_u16() {
            200..=299 => {}
            404 | 410 => return Err(ResolveError::NotFound(url.clone())),
            429 => return Err(ResolveError::RateLimited(url.clone())),
            status => {
                return Err(ResolveError::unavailable(
                    url,
                    format!("the page answered HTTP {status}"),
                ));
            }
        }
        let html = fetched.text();
        let page = Page::parse(&html, &fetched.url);
        let content = content_in(&html, &link.id);
        let video_url = page
            .meta("og:video:url")
            .or_else(|| page.meta("og:video:secure_url"))
            .or_else(|| page.meta("og:video"))
            .or_else(|| {
                let selector = Selector::parse("video[data-src]").expect("valid");
                page.document()
                    .select(&selector)
                    .next()
                    .and_then(|v| v.value().attr("data-src"))
                    .map(String::from)
            })
            .or_else(|| {
                content
                    .as_ref()
                    .and_then(|c| c["url"].as_str())
                    .filter(|u| u.contains("/videos/"))
                    .map(String::from)
            })
            .and_then(|u| Url::parse(&u).ok());
        let Some(video_url) = video_url else {
            return Err(
                if link.kind == "picture"
                    || content
                        .as_ref()
                        .is_some_and(|c| c["type"].as_str() == Some("pic"))
                {
                    ResolveError::unavailable(url, "the post is a picture")
                } else {
                    ResolveError::NotFound(url.clone())
                },
            );
        };
        let mut v = Variant::new(video_url, VariantKind::File);
        v.container = Some(Container::Mp4);
        v.video = Some(VideoCodec::H264);
        v.audio = Some(AudioCodec::Aac);
        v.width = page
            .meta("og:video:width")
            .and_then(|w| w.parse().ok())
            .or_else(|| {
                content
                    .as_ref()
                    .and_then(|c| c["size"]["w"].as_u64())
                    .map(|w| w as u32)
            });
        v.height = page
            .meta("og:video:height")
            .and_then(|h| h.parse().ok())
            .or_else(|| {
                content
                    .as_ref()
                    .and_then(|c| c["size"]["h"].as_u64())
                    .map(|h| h as u32)
            });
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(link.id.clone());
        resolved.title = page
            .meta("og:title")
            .map(|t| t.trim_end_matches(" - iFunny").to_string())
            .and_then(|t| clean_title(&t))
            .or_else(|| {
                content
                    .as_ref()
                    .and_then(|c| c["title"].as_str())
                    .and_then(clean_title)
            });
        resolved.description = content
            .as_ref()
            .and_then(|c| c["tags"].as_array())
            .map(|tags| {
                tags.iter()
                    .filter_map(|t| t.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .and_then(|t| clean_title(&t));
        resolved.uploader = content
            .as_ref()
            .and_then(|c| c["creator"]["nick"].as_str())
            .and_then(clean_title);
        resolved.uploader_url = content
            .as_ref()
            .and_then(|c| c["creator"]["profileUrl"].as_str())
            .and_then(|p| fetched.url.join(p).ok());
        resolved.uploaded_at = content
            .as_ref()
            .and_then(|c| c["published"].as_i64().or(c["created"].as_i64()))
            .and_then(|t| Timestamp::from_second(t).ok());
        resolved.thumbnail = page.meta("og:image").and_then(|u| Url::parse(&u).ok());
        resolved.webpage_url = Some(fetched.url.clone());
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

    const PAGE: &str = r#"<html><head>
    <meta property="og:title" content="Video memes veclHKeeD by Crumbob_Breadpants - iFunny" />
    <meta property="og:image" content="https://imageproxy.getfn.io/resize:640x/images/0e0c_3.jpg" />
    <meta property="og:video:url" content="https://img.getfn.io/videos/0e0c_1.mp4" />
    <meta property="og:video:height" content="1138" />
    <meta property="og:video:width" content="640" />
    </head><body>
    <video data-poster="https://img.getfn.io/images/0e0c_3.jpg" data-src="https://img.getfn.io/videos/0e0c_1.mp4" loop="loop"></video>
    <script>window.__INITIAL_STATE__={"channels":[{"name":"WTF","path":"wtf"}],"content":{"items":[{"bottomText":"","comments":0,"created":1789167588,"description":"","id":"veclHKeeD","fixedTitle":"Video memes veclHKeeD by Crumbob_Breadpants","link":"https://ifunny.co/video/veclHKeeD","published":1789167588,"smiles":15,"thumb":{"m":"x"},"title":"cat, vs, bobcat, kitty, cats","url":"https://img.getfn.io/videos/0e0c_1.mp4","canonical":"https://ifunny.co/video/veclHKeeD","creator":{"avatar":{"bgColor":"a49a8e"},"nick":"Crumbob_Breadpants","profileUrl":"/user/Crumbob_Breadpants"},"source":null,"size":{"w":640,"h":1138},"tags":["cat","vs","bobcat","kitty","cats"],"type":"video","risk":1}]}};</script>
    </body></html>"#;

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://ifunny.co/video/veclHKeeD?s=cl"),
            Some(Link {
                kind: "video".into(),
                slug: "veclHKeeD".into(),
                id: "veclHKeeD".into()
            })
        );
        assert_eq!(
            link("https://ifunny.co/video/me-after-she-only-gave-me-simple-hug-iJwU1NmeD")
                .unwrap()
                .id,
            "iJwU1NmeD"
        );
        assert_eq!(
            link("https://ifunny.co/picture/abcdef12").unwrap().kind,
            "picture"
        );
        assert_eq!(link("https://ifunny.co/user/someone"), None);
        assert_eq!(link("https://ifunny.co/"), None);
    }

    #[tokio::test]
    async fn video_posts_resolve_with_their_creator() {
        let mut fixture = Fixture::new("ifunny", None);
        fixture
            .exchanges
            .push(get("https://ifunny.co/video/veclHKeeD", 200, PAGE));
        fixture
            .exchanges
            .push(get("https://ifunny.co/video/gone12345", 404, ""));
        let resolver = IfunnyResolver::new(Http::replay(fixture));
        let url = Url::parse("https://ifunny.co/video/veclHKeeD").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(
            resolved.title.as_deref(),
            Some("Video memes veclHKeeD by Crumbob_Breadpants")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("Crumbob_Breadpants"));
        assert_eq!(
            resolved.uploader_url.unwrap().as_str(),
            "https://ifunny.co/user/Crumbob_Breadpants"
        );
        assert_eq!(resolved.uploaded_at.unwrap().as_second(), 1789167588);
        assert_eq!(
            resolved.description.as_deref(),
            Some("cat, vs, bobcat, kitty, cats")
        );
        assert_eq!(resolved.variants.len(), 1);
        let v = &resolved.variants[0];
        assert_eq!(v.url.as_str(), "https://img.getfn.io/videos/0e0c_1.mp4");
        assert_eq!((v.width, v.height), (Some(640), Some(1138)));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://ifunny.co/video/gone12345").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn pictures_are_reported_as_such() {
        let picture = r#"<html><head><meta property="og:title" content="Picture memes abcdef12 - iFunny" /><meta property="og:image" content="https://img.getfn.io/images/p.jpg" /></head><body><script>window.__INITIAL_STATE__={"content":{"items":[{"id":"abcdef12","url":"https://img.getfn.io/images/p.jpg","type":"pic"}]}};</script></body></html>"#;
        let mut fixture = Fixture::new("ifunny", None);
        fixture
            .exchanges
            .push(get("https://ifunny.co/picture/abcdef12", 200, picture));
        let resolver = IfunnyResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://ifunny.co/picture/abcdef12").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("picture")),
            "{error}"
        );
    }
}
