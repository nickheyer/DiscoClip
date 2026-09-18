//! Business Insider (businessinsider.com, businessinsider.nl) articles: the JW Player
//! media the article's player names, read from JW Player's delivery API for every
//! rendition, with the article's own title, summary, author and date. Video articles
//! carry the id on the player element (`data-media-id`), in the page state
//! (`"jwplayer":{"assetID":…}`), in the structured data's player script link
//! (`content.jwplatform.com/players/{id}-…`), on older player elements, and as a bare
//! `id: "…"` value in the player's setup.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use url::Url;

use super::page::Page;
use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolver, SessionSupport, Tag, clean_title,
    fetch_ok, jwplayer, navigation_headers, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::MediaKind;

pub const PLATFORM: &str = "businessinsider";

/// `<div … data-media-id="cjGDb0X9" …>`: the player element.
static RE_MEDIA_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"data-media-id=["']([a-zA-Z0-9]{8})["']"#).unwrap());
/// `"jwplayer":{"assetID":"cjGDb0X9"}`: the article's page state.
static RE_ASSET_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#""jwplayer"\s*:\s*\{\s*"assetID"\s*:\s*"([a-zA-Z0-9]{8})""#).unwrap()
});
/// `content.jwplatform.com/players/cjGDb0X9-mxPVKKUe.js` and `cdn.jwplayer.com/players/…`:
/// the player script the structured data embeds.
static RE_PLAYER_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?:jwplatform\.com|jwplayer\.com)/players/([a-zA-Z0-9]{8})[-.]").unwrap()
});
/// `id="jwplayer_cjGDb0X9"`: the player element of older article markup.
static RE_JWPLAYER_ELEMENT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"id=["']jwplayer_([a-zA-Z0-9]{8})["']"#).unwrap());
/// `.setup({ id: "cjGDb0X9", … })`: the media id in a player's inline setup.
static RE_SETUP_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\.setup\(\s*\{[^}]*?\bid["']?\s*:\s*["']([a-zA-Z0-9]{8})["']"#).unwrap()
});

/// The article slug an article link names: its last path segment.
pub fn article_id(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !["businessinsider.com", "businessinsider.nl"]
        .iter()
        .any(|site| host == *site || host.ends_with(&format!(".{site}")))
    {
        return None;
    }
    url.path_segments()?
        .rfind(|s| !s.is_empty())
        .map(str::to_string)
}

/// The JW Player media id an article page names: on its player element, in its page
/// state, in its structured data's player script, on an older player element, or as the
/// bare `id` of a player setup.
pub fn jwplayer_id(html: &str) -> Option<String> {
    util::search_any(
        &[
            &RE_MEDIA_ID,
            &RE_ASSET_ID,
            &RE_PLAYER_LINK,
            &RE_JWPLAYER_ELEMENT,
            &RE_SETUP_ID,
        ],
        html,
    )
}

/// What an article page says about itself: its Open Graph title, summary and image, its
/// author and publication time, and its canonical link.
struct ArticleMeta {
    title: Option<String>,
    description: Option<String>,
    author: Option<String>,
    published_at: Option<jiff::Timestamp>,
    image: Option<Url>,
    canonical: Option<Url>,
}

impl ArticleMeta {
    fn of(html: &str, url: &Url) -> Self {
        let page = Page::parse(html, url);
        Self {
            title: page.meta("og:title").and_then(|t| clean_title(&t)),
            description: page.meta("og:description").and_then(|t| clean_title(&t)),
            author: page
                .meta("author")
                .or_else(|| page.meta("article:author"))
                .and_then(|t| clean_title(&t)),
            published_at: page
                .meta("article:published_time")
                .and_then(|t| t.parse().ok()),
            image: page.meta("og:image").and_then(|t| Url::parse(&t).ok()),
            canonical: page.canonical(),
        }
    }
}

pub struct BusinessinsiderResolver {
    http: Http,
}

impl BusinessinsiderResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for BusinessinsiderResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Business Insider",
            hosts: &["businessinsider.com", "businessinsider.nl"],
            features: &["videos"],
            formats: &["mp4", "hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::News],
            session: SessionSupport::None,
            examples: &[
                "https://www.businessinsider.com/how-much-radiation-youre-exposed-to-in-everyday-life-2016-6",
                "https://www.businessinsider.com/how-wildlife-poaching-works-according-to-an-undercover-investigator-2026-8",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        article_id(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        article_id(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let fetched = fetch_ok(
            &self.http,
            url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        let html = fetched.text();
        let id = jwplayer_id(&html).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let meta = ArticleMeta::of(&html, &fetched.url);
        let mut resolved = jwplayer::media(&self.http, PLATFORM, &id, url).await?;
        if meta.title.is_some() {
            resolved.title = meta.title;
        }
        if meta.description.is_some() {
            resolved.description = meta.description;
        }
        resolved.uploader = meta.author.or_else(|| Some("Business Insider".to_string()));
        resolved.uploaded_at = meta.published_at.or(resolved.uploaded_at);
        if resolved.thumbnail.is_none() {
            resolved.thumbnail = meta.image;
        }
        resolved.webpage_url = Some(meta.canonical.unwrap_or_else(|| url.clone()));
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use super::super::VariantKind;
    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};

    const RADIATION: &str = "https://www.businessinsider.com/how-much-radiation-youre-exposed-to-in-everyday-life-2016-6";
    const POACHING: &str = "https://www.businessinsider.com/how-wildlife-poaching-works-according-to-an-undercover-investigator-2026-8";

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

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn links_are_read() {
        let id = |s: &str| article_id(&url(s));
        assert_eq!(
            id(RADIATION),
            Some("how-much-radiation-youre-exposed-to-in-everyday-life-2016-6".into())
        );
        assert_eq!(
            id(POACHING),
            Some(
                "how-wildlife-poaching-works-according-to-an-undercover-investigator-2026-8".into()
            )
        );
        assert_eq!(
            id("http://www.businessinsider.com/excel-index-match-vlookup-video-how-to-2015-2?IR=T"),
            Some("excel-index-match-vlookup-video-how-to-2015-2".into())
        );
        assert_eq!(
            id("https://markets.businessinsider.com/news/stocks/some-story-2026-9/"),
            Some("some-story-2026-9".into())
        );
        assert_eq!(id("https://www.businessinsider.com/"), None);
        assert_eq!(
            id("https://www.businessinsider.nl/artikel-2017-7/"),
            Some("artikel-2017-7".into())
        );
        assert_eq!(id("https://www.businessinsider.de/politik/artikel-1"), None);
        assert_eq!(id("https://businessinsider.com.evil.test/x-2020-1"), None);
    }

    #[test]
    fn player_ids_are_found_in_every_markup() {
        assert_eq!(
            jwplayer_id(r#"<div id="player-container-mxPVKKUe" data-media-id="cjGDb0X9" data-title="Bananas"></div>"#).as_deref(),
            Some("cjGDb0X9")
        );
        assert_eq!(
            jwplayer_id(
                r#"{"meta":{"_id":"5824c655aee45882a647ea6d","jwplayer":{"assetID":"Md8g7uqw"}}}"#
            )
            .as_deref(),
            Some("Md8g7uqw")
        );
        assert_eq!(
            jwplayer_id(r#"{"@type":"VideoObject","embedUrl":"&lt;script src=&#39;https://content.jwplatform.com/players/Yptt4ycg-mxPVKKUe.js&#39;&gt;&lt;/script&gt;"}"#).as_deref(),
            Some("Yptt4ycg")
        );
        assert_eq!(
            jwplayer_id(
                r#"<script src="https://cdn.jwplayer.com/players/Xm3xYlth-mxPVKKUe.js"></script>"#
            )
            .as_deref(),
            Some("Xm3xYlth")
        );
        assert_eq!(
            jwplayer_id(r#"<div id="jwplayer_5zJwd4FK"></div>"#).as_deref(),
            Some("5zJwd4FK")
        );
        assert_eq!(
            jwplayer_id(r#"jwplayer("player").setup({ id: "5zJwd4FK", autostart: false })"#)
                .as_deref(),
            Some("5zJwd4FK")
        );
        assert_eq!(
            jwplayer_id(r#"{"id":"0000002f","links":{"self":"https://i.insider.com/563baea3"}}"#),
            None
        );
        assert_eq!(jwplayer_id("<p>no player</p>"), None);
    }

    #[tokio::test]
    async fn articles_resolve_their_players_media_with_the_articles_metadata() {
        let fixture = Fixture::parse(include_str!("businessinsider_fixture.json")).unwrap();
        let resolver = BusinessinsiderResolver::new(Http::replay(fixture));
        for (link, id) in [(RADIATION, "cjGDb0X9"), (POACHING, "Md8g7uqw")] {
            assert!(resolver.matches(&url(link)));
            let resolved = resolver.resolve(&url(link)).await.unwrap().media().unwrap();
            assert_eq!(resolved.resolver, PLATFORM, "{link}");
            assert_eq!(resolved.id.as_deref(), Some(id), "{link}");
            assert!(resolved.title.is_some(), "{link}");
            assert!(resolved.uploader.is_some(), "{link}");
            assert!(resolved.uploaded_at.is_some(), "{link}");
            assert!(resolved.duration.is_some(), "{link}");
            assert!(resolved.webpage_url.is_some(), "{link}");
            assert!(
                resolved
                    .variants
                    .iter()
                    .any(|v| v.kind == VariantKind::File),
                "{link}"
            );
            assert!(
                resolved.variants.iter().any(|v| v.kind == VariantKind::Hls),
                "{link}"
            );
            assert!(
                resolved
                    .variants
                    .iter()
                    .all(|v| v.kind != VariantKind::Hls || v.height.is_some() || v.audio_only),
                "{link}: every HLS video stream names its size"
            );
            assert!(
                resolved.variants.iter().any(|v| v.height == Some(1080)),
                "{link}"
            );
        }
    }

    #[tokio::test]
    async fn pages_without_a_player_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.businessinsider.com/no-video-here-2020-1",
            200,
            "text/html",
            r#"<html><body><p>text only</p><script>{"id":"0000002f","rid":8040323822}</script></body></html>"#.into(),
        ));
        fixture.exchanges.push(get(
            "https://www.businessinsider.com/gone-2020-1",
            404,
            "text/html",
            "<html>404</html>".into(),
        ));
        let resolver = BusinessinsiderResolver::new(Http::replay(fixture));
        assert!(matches!(
            resolver
                .resolve(&url("https://www.businessinsider.com/no-video-here-2020-1"))
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        assert!(matches!(
            resolver
                .resolve(&url("https://www.businessinsider.com/gone-2020-1"))
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
