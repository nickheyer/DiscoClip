//! Audioboom posts: the page names its audio file in its Open Graph tags, and the player
//! setup it carries, when it does, adds the clip's length and author.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Tag,
    Variant, clean_title, fetch, navigation_headers, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind};

pub const PLATFORM: &str = "audioboom";

/// `/posts/{id}` or `/boos/{id}`, with anything after the id.
static RE_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/(?:boos|posts)/(\d+)").unwrap());
static RE_PLAYER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"data-react-class="V5DetailPagePlayer"\s*data-react-props=["']([^"']+)["']"#)
        .unwrap()
});
static RE_UPLOADER_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"<div class="avatar flex-shrink-0">\s*<a href="(http[^"]+)""#).unwrap()
});

pub fn post_id(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "audioboom.com" && host != "www.audioboom.com" {
        return None;
    }
    RE_PATH.captures(url.path()).map(|caps| caps[1].to_string())
}

/// The first clip of the page's player setup, when the page carries one.
fn player_clip(html: &str) -> Option<Value> {
    let escaped = util::search(&RE_PLAYER, html)?;
    let store: Value = serde_json::from_str(&util::html_unescape(&escaped)).ok()?;
    store["clips"].as_array()?.first().cloned()
}

pub struct AudioboomResolver {
    http: Http,
}

impl AudioboomResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for AudioboomResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Audioboom",
            hosts: &["audioboom.com"],
            features: &["audio", "podcasts"],
            formats: &["mp3"],
            media: &[MediaKind::Audio],
            tags: &[Tag::Podcasts],
            session: SessionSupport::None,
            examples: &[
                "https://audioboom.com/posts/7398103-asim-chaudhry",
                "https://audioboom.com/posts/8128496.mp3",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        post_id(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let id = post_id(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let page_url = Url::parse(&format!("https://audioboom.com/posts/{id}")).expect("valid");
        let fetched = fetch(
            &self.http,
            &page_url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        let html = fetched.text();
        let clip = player_clip(&html).unwrap_or(Value::Null);
        let page = Page::parse(&html, &page_url);
        let audio = util::url_of(&clip["clipURLPriorToLoading"], None)
            .or_else(|| {
                page.meta("og:audio")
                    .and_then(|a| util::join_url(Some(&page_url), &a))
            })
            .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let mut variant = Variant::file(audio);
        variant.audio_only = true;
        variant.container = Some(Container::Mp3);
        variant.audio = Some(AudioCodec::Mp3);
        variant.format_id = Some("audio".into());
        let mut resolved = Resolved::of(PLATFORM, MediaKind::Audio);
        resolved.id = Some(id);
        resolved.title = clip["title"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| page.meta("og:audio:title").and_then(|t| clean_title(&t)))
            .or_else(|| page.meta("og:title").and_then(|t| clean_title(&t)))
            .or_else(|| page.meta("audio_title").and_then(|t| clean_title(&t)))
            .or_else(|| page.title());
        resolved.description = clip["description"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| {
                clip["formattedDescription"]
                    .as_str()
                    .map(util::clean_html)
                    .and_then(|d| clean_title(&d))
            })
            .or_else(|| page.meta("og:description").and_then(|d| clean_title(&d)));
        resolved.duration = util::seconds(&clip["duration"]).or_else(|| {
            page.meta("weibo:audio:duration")
                .and_then(|d| d.trim().parse::<f64>().ok())
                .filter(|d| *d > 0.0)
                .map(std::time::Duration::from_secs_f64)
        });
        resolved.uploader = clip["author"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| page.meta("og:audio:artist").and_then(|a| clean_title(&a)))
            .or_else(|| {
                page.meta("twitter:audio:artist_name")
                    .and_then(|a| clean_title(&a))
            })
            .or_else(|| page.meta("audio_artist").and_then(|a| clean_title(&a)));
        resolved.uploader_url = util::url_of(&clip["author_url"], None)
            .or_else(|| util::search(&RE_UPLOADER_LINK, &html).and_then(|u| Url::parse(&u).ok()));
        resolved.thumbnail = page
            .meta("og:image")
            .and_then(|t| util::join_url(Some(&page_url), &t));
        resolved.uploaded_at = page
            .meta("article:published_time")
            .and_then(|t| util::parse_timestamp(&t));
        resolved.webpage_url = Some(page_url);
        resolved.variants = vec![variant];
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};

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

    #[test]
    fn links_are_read() {
        let id = |s: &str| post_id(&Url::parse(s).unwrap());
        assert_eq!(
            id("https://audioboom.com/posts/7398103-asim-chaudhry"),
            Some("7398103".into())
        );
        assert_eq!(
            id("https://audioboom.com/posts/8128496.mp3"),
            Some("8128496".into())
        );
        assert_eq!(
            id("https://audioboom.com/boos/4279833-3-09-2016?t=0"),
            Some("4279833".into())
        );
        assert_eq!(id("https://audioboom.com/channels/5017447"), None);
        assert_eq!(id("https://example.com/posts/7398103"), None);
    }

    #[tokio::test]
    async fn posts_resolve_from_the_page_tags_or_the_player() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://audioboom.com/posts/7398103",
            200,
            "text/html",
            concat!(
                r#"<html><head><meta property="og:title" content="Asim Chaudhry"><meta property="og:audio:title" content="Asim Chaudhry - Ep 1">"#,
                r#"<meta property="og:audio" content="https://audioboom.com/posts/7398103.mp3?modified=1&source=fb"><meta property="og:audio:artist" content="Unofficial Partridge">"#,
                r#"<meta property="og:description" content="Asim talks."><meta property="og:image" content="https://images.audioboom.com/1.jpg"><meta name="weibo:audio:duration" content="1352.73">"#,
                r#"<meta property="article:published_time" content="2019-08-21T14:00:00Z"></head><body><div class="avatar flex-shrink-0"><a href="https://audioboom.com/channels/5017447">x</a></div></body></html>"#
            ).into(),
        ));
        fixture.exchanges.push(get(
            "https://audioboom.com/posts/4279833",
            200,
            "text/html",
            r#"<html><body><div data-react-class="V5DetailPagePlayer" data-react-props="{&quot;clips&quot;:[{&quot;clipURLPriorToLoading&quot;:&quot;https://audioboom.com/posts/4279833.mp3&quot;,&quot;title&quot;:&quot;3/09/2016 Czaban Hour 3&quot;,&quot;description&quot;:&quot;Guest.&quot;,&quot;duration&quot;:2245.72,&quot;author&quot;:&quot;Steve Czaban&quot;,&quot;author_url&quot;:&quot;https://audioboom.com/channel/steveczaban&quot;}]}"></div></body></html>"#.into(),
        ));
        fixture.exchanges.push(get(
            "https://audioboom.com/posts/1",
            200,
            "text/html",
            "<html><head><title>Gone</title></head></html>".into(),
        ));
        let resolver = AudioboomResolver::new(Http::replay(fixture));
        let url = Url::parse("https://audioboom.com/posts/7398103-asim-chaudhry").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("7398103"));
        assert_eq!(resolved.title.as_deref(), Some("Asim Chaudhry - Ep 1"));
        assert_eq!(resolved.uploader.as_deref(), Some("Unofficial Partridge"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://audioboom.com/channels/5017447"
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(1352.73)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1566396000)
        );
        assert_eq!(resolved.variants.len(), 1);
        assert!(resolved.variants[0].audio_only);
        assert_eq!(resolved.media, MediaKind::Audio);
        assert_eq!(resolved.variants[0].container, Some(Container::Mp3));
        assert_eq!(
            resolved.variants[0].url.as_str(),
            "https://audioboom.com/posts/7398103.mp3?modified=1&source=fb"
        );
        let player = resolver
            .resolve(&Url::parse("https://audioboom.com/boos/4279833-3-09-2016?t=0").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(player.title.as_deref(), Some("3/09/2016 Czaban Hour 3"));
        assert_eq!(player.uploader.as_deref(), Some("Steve Czaban"));
        assert_eq!(player.duration, Some(Duration::from_secs_f64(2245.72)));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://audioboom.com/posts/1").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
