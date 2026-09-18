//! CPAC (cpac.ca), Canada's parliamentary channel: every episode page names its HLS
//! stream in its player's data attributes, beside the episode's title, length and air
//! time; the older `episode?id=` links redirect there.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport,
    clean_title, fetch, hls, navigation_headers, parse_time_stamp, status_error, util,
};
use crate::http::{BROWSER_UA, Http};

pub const PLATFORM: &str = "cpac";

/// The episode id every CPAC link carries: `?id={uuid}`.
static RE_ID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}$").unwrap());
/// `/episode`, `/l-episode`, or `/{category}/episode/{slug}`.
static RE_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^/(?:l-)?episode/?$|^/[^/]+/(?:episode|l-episode|episode-fr)/[^/]+/?$").unwrap()
});
/// The site's content services, which its app reads.
const EPISODE_SERVICE: &str = "https://www.cpac.ca/api/1/services/episode-info.json";
const PROGRAM_SERVICE: &str = "https://www.cpac.ca/api/1/services/item-list-program.json";
const SITE_KEY: &str = "cpacca";
/// The player's attributes.
static RE_PLAYER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(<[^>]+\bdata-videourl=[^>]*>)").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// An episode, by its id.
    Episode,
    /// A programme, whose latest episodes the site lists.
    Program,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub kind: Kind,
    pub id: String,
    pub french: bool,
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "cpac.ca" && host != "www.cpac.ca" {
        return None;
    }
    let path = url.path().trim_end_matches('/');
    if matches!(path, "/program" | "/emission") {
        let id = util::query_param(url, "id")?;
        if id.is_empty() || !id.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        return Some(Link {
            kind: Kind::Program,
            id,
            french: path == "/emission",
        });
    }
    if !RE_PATH.is_match(url.path()) {
        return None;
    }
    let id = util::query_param(url, "id")?.to_ascii_lowercase();
    if !RE_ID.is_match(&id) {
        return None;
    }
    Some(Link {
        kind: Kind::Episode,
        id,
        french: path.starts_with("/l-episode")
            || path.contains("/l-episode/")
            || path.contains("/episode-fr/"),
    })
}

/// A field the services give in both languages: `{name}_{lang}_t` or `_s`.
fn localized(details: &Value, name: &str, french: bool) -> Option<String> {
    let (first, second) = if french { ("fr", "en") } else { ("en", "fr") };
    [first, second]
        .iter()
        .flat_map(|lang| [format!("{name}_{lang}_t"), format!("{name}_{lang}_s")])
        .find_map(|key| util::text(&details[&key]).filter(|v| !v.trim().is_empty()))
}

pub struct CpacResolver {
    http: Http,
}

impl CpacResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// One of the site's content services, by `id`.
    async fn service(&self, service: &str, id: &str, origin: &Url) -> Result<Value, ResolveError> {
        let api = util::with_query(
            &Url::parse(service).expect("valid"),
            &[("crafterSite", SITE_KEY), ("id", id)],
        );
        let accept = [("accept".to_string(), "application/json".to_string())];
        let fetched = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &accept, MAX_PAGE).await?;
        if let Some(error) = status_error(fetched.status, origin) {
            return Err(error);
        }
        fetched.json(origin)
    }

    /// An episode as the episode service describes it, in the link's language: the
    /// stream, titles, description, poster and air time.
    async fn resolve_episode_from_service(
        &self,
        link: &Link,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let answer = self.service(EPISODE_SERVICE, &link.id, url).await?;
        let details = &answer["component"]["details"];
        let stream = util::url_of(&details["videoUrl"], None)
            .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let expanded = hls::expand(&self.http, &stream, PLATFORM, BROWSER_UA, &[]).await?;
        if expanded.variants.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = util::text(&details["episodeId"]).or_else(|| Some(link.id.clone()));
        resolved.title = localized(details, "title", link.french).and_then(|t| clean_title(&t));
        resolved.description = localized(details, "description", link.french)
            .map(|d| util::clean_html(&d))
            .and_then(|d| clean_title(&d));
        resolved.thumbnail =
            localized(details, "image", link.french).and_then(|i| util::join_url(Some(url), &i));
        resolved.duration = util::text(&details["videoDuration"])
            .and_then(|span| parse_time_stamp(&span))
            .or(expanded.duration);
        resolved.uploaded_at =
            util::text(&details["liveDateTime"]).and_then(|t| util::parse_timestamp(&t));
        resolved.uploader = Some("CPAC".to_string());
        resolved.live = details["type"]
            .as_str()
            .is_some_and(|t| t.eq_ignore_ascii_case("live"))
            || expanded.live;
        resolved.webpage_url = localized(details, "url", link.french)
            .and_then(|path| util::join_url(Some(url), &path))
            .or_else(|| Some(url.clone()));
        resolved.subtitles = expanded.subtitles;
        let wanted = if link.french { "fr" } else { "en" };
        let mut variants = expanded.variants;
        variants.sort_by_key(|v| match v.language.as_deref() {
            Some(language) if language.starts_with(wanted) => 0,
            None => 1,
            Some(_) => 2,
        });
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    /// A programme's latest episodes, as the programme service lists them.
    async fn resolve_program(&self, link: &Link, url: &Url) -> Result<Resolution, ResolveError> {
        let answer = self.service(PROGRAM_SERVICE, &link.id, url).await?;
        let entries: Vec<super::PlaylistEntry> = answer["item"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| {
                let path = localized(item, "url", link.french)?;
                let episode = util::join_url(Some(url), &path)?;
                Some(super::PlaylistEntry {
                    url: episode,
                    title: localized(item, "title", link.french).and_then(|t| clean_title(&t)),
                    duration: util::text(&item["videoDuration"])
                        .and_then(|span| parse_time_stamp(&span)),
                })
            })
            .collect();
        if entries.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        Ok(Resolution::Playlist(super::Playlist {
            resolver: PLATFORM.into(),
            id: Some(format!("program{}", link.id)),
            title: localized(&answer, "title", link.french)
                .and_then(|t| clean_title(&t))
                .or_else(|| {
                    entries
                        .first()
                        .and_then(|_| localized(&answer["item"][0], "category", link.french))
                }),
            total: Some(entries.len()),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for CpacResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "CPAC",
            hosts: &["cpac.ca"],
            features: &["videos", "live", "programmes"],
            formats: &["hls"],
            session: SessionSupport::None,
            examples: &[
                "https://www.cpac.ca/episode?id=fc7edcae-4660-47e1-ba61-5b7f29a9db0f",
                "https://www.cpac.ca/headline-politics/episode/news-conference-to-celebrate-national-kindness-week--february-15-2022?id=fc7edcae-4660-47e1-ba61-5b7f29a9db0f",
                "https://www.cpac.ca/program?id=6",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        if link.kind == Kind::Program {
            return self.resolve_program(&link, url).await;
        }
        // The episode service describes the episode in both languages; the page
        // carries the same player when the service has no record of it.
        match self.resolve_episode_from_service(&link, url).await {
            Ok(resolution) => return Ok(resolution),
            Err(ResolveError::NotFound(_)) | Err(ResolveError::Malformed { .. }) => {}
            Err(error) => return Err(error),
        }
        let fetched = fetch(
            &self.http,
            url,
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
        let player = util::search(&RE_PLAYER, &html)
            .map(|tag| util::extract_attributes(&tag))
            .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let attribute = |name: &str| {
            player
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| util::html_unescape(value))
                .filter(|value| !value.trim().is_empty())
        };
        let stream = attribute("data-videourl")
            .and_then(|link| util::join_url(Some(&fetched.url), &link))
            .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let expanded = hls::expand(&self.http, &stream, PLATFORM, BROWSER_UA, &[]).await?;
        if expanded.variants.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let (title, description, thumbnail) = {
            let page = Page::parse(&html, &fetched.url);
            (
                page.meta("og:title")
                    .and_then(|t| clean_title(&t))
                    .or_else(|| page.title()),
                page.meta("og:description").and_then(|d| clean_title(&d)),
                page.meta("og:image")
                    .and_then(|t| util::join_url(Some(&fetched.url), &t)),
            )
        };
        let live = attribute("data-type").is_some_and(|kind| kind.eq_ignore_ascii_case("live"));
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(attribute("data-episodeid").unwrap_or(link.id));
        resolved.title = title;
        resolved.description = description;
        resolved.thumbnail = thumbnail;
        resolved.duration = attribute("data-videoduration")
            .and_then(|span| parse_time_stamp(&span))
            .or(expanded.duration);
        resolved.uploaded_at =
            attribute("data-livedatetime").and_then(|t| util::parse_timestamp(&t));
        resolved.uploader = Some("CPAC".to_string());
        resolved.live = live || expanded.live;
        resolved.webpage_url = Some(fetched.url.clone());
        resolved.subtitles = expanded.subtitles;
        // The renditions in the link's language come first, then those without one.
        let wanted = if link.french { "fr" } else { "en" };
        let mut variants = expanded.variants;
        variants.sort_by_key(|v| match v.language.as_deref() {
            Some(language) if language.starts_with(wanted) => 0,
            None => 1,
            Some(_) => 2,
        });
        resolved.variants = variants;
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

    const ID: &str = "fc7edcae-4660-47e1-ba61-5b7f29a9db0f";

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://www.cpac.ca/episode?id=fc7edcae-4660-47e1-ba61-5b7f29a9db0f"),
            Some(Link {
                kind: Kind::Episode,
                id: ID.into(),
                french: false
            })
        );
        assert_eq!(
            link("https://www.cpac.ca/l-episode?id=FC7EDCAE-4660-47E1-BA61-5B7F29A9DB0F"),
            Some(Link {
                kind: Kind::Episode,
                id: ID.into(),
                french: true
            })
        );
        assert_eq!(
            link(
                "https://www.cpac.ca/headline-politics/episode/news-conference?id=fc7edcae-4660-47e1-ba61-5b7f29a9db0f"
            ),
            Some(Link {
                kind: Kind::Episode,
                id: ID.into(),
                french: false
            })
        );
        assert_eq!(link("https://www.cpac.ca/episode?id=123"), None);
        assert_eq!(
            link("https://www.cpac.ca/program?id=6"),
            Some(Link {
                kind: Kind::Program,
                id: "6".into(),
                french: false
            })
        );
        assert_eq!(
            link("https://www.cpac.ca/emission?id=6"),
            Some(Link {
                kind: Kind::Program,
                id: "6".into(),
                french: true
            })
        );
        assert_eq!(
            link("https://example.com/episode?id=fc7edcae-4660-47e1-ba61-5b7f29a9db0f"),
            None
        );
    }

    #[tokio::test]
    async fn episodes_resolve_from_the_player_attributes() {
        let page = format!(concat!(
            r#"<html><head><meta property="og:title" content="News Conference to Celebrate National Kindness Week – February 15, 2022">"#,
            r#"<meta property="og:description" content="Speakers take part."><meta property="og:image" content="https://images.cpac.ca/episode/thumbnail/2022/02/{id}/kindness.jpg"></head>"#,
            r#"<body><div id="page-video" data-episodeid="{id}" data-type="video" data-videourl="https://cpac-vod.cdn.vustreams.com/cpac/vod/{id}/{id}_nodrm.ism/.m3u8" data-videoduration="00:53:36" data-livedatetime="2022-02-15T05:00:00.000Z"></div></body></html>"#
        ), id = ID).replace("{id}", ID);
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            &format!(
                "https://www.cpac.ca/api/1/services/episode-info.json?crafterSite=cpacca&id={ID}"
            ),
            404,
            "text/html",
            "<html>Page not found</html>".into(),
        ));
        fixture.exchanges.push(get(
            &format!("https://www.cpac.ca/episode?id={ID}"),
            200,
            "text/html",
            page,
        ));
        fixture.exchanges.push(get(
            &format!("https://cpac-vod.cdn.vustreams.com/cpac/vod/{ID}/{ID}_nodrm.ism/.m3u8"),
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2000000,RESOLUTION=1280x720\n720.m3u8\n".into(),
        ));
        fixture.exchanges.push(get(
            &format!("https://cpac-vod.cdn.vustreams.com/cpac/vod/{ID}/{ID}_nodrm.ism/720.m3u8"),
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\n0.ts\n#EXT-X-ENDLIST\n".into(),
        ));
        fixture.exchanges.push(get(
            "https://www.cpac.ca/episode?id=00000000-0000-0000-0000-000000000000",
            200,
            "text/html",
            "<html><body>No such episode</body></html>".into(),
        ));
        let resolver = CpacResolver::new(Http::replay(fixture));
        let url = Url::parse(&format!("https://www.cpac.ca/episode?id={ID}")).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some(ID));
        assert_eq!(
            resolved.title.as_deref(),
            Some("News Conference to Celebrate National Kindness Week – February 15, 2022")
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(53 * 60 + 36)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1644901200)
        );
        assert!(!resolved.live);
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].height, Some(720));
        assert!(matches!(
            resolver
                .resolve(
                    &Url::parse(
                        "https://www.cpac.ca/episode?id=00000000-0000-0000-0000-000000000000"
                    )
                    .unwrap()
                )
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
