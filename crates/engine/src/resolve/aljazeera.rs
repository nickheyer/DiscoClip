//! Al Jazeera (aljazeera.com, aljazeera.net and its regional sites): every article and
//! programme page is one post of the site's GraphQL API, whose video names its file on
//! the network's CDN and its Brightcove player.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use url::Url;

use super::{
    MAX_PAGE, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Tag, Variant,
    brightcove, clean_title, fetch, hls, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "aljazeera";
const DEFAULT_ACCOUNT: &str = "911432371001";
const DEFAULT_PLAYER: &str = "csvTfAlKW";

/// `/{kind}[/{programme}]/{year}/{month}/{day}/{slug}`: the kind names the post type the
/// API is asked for.
static RE_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^/(programs?|videos?|features?|news)(?:/([^/?#]+))?/(\d{4})/(\d{1,2})/(\d{1,2})/([^/?#]+)/?$")
        .unwrap()
});
static RE_HOST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[a-z0-9-]+\.)?aljazeera\.(?:com|net)$").unwrap());
/// A Brightcove player on a page, for posts whose video the API withholds.
static RE_BRIGHTCOVE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"https?://players\.brightcove\.net/(\d+)/([^/_]+)_([^/]+)/index\.html\?videoId=(\d+)"#,
    )
    .unwrap()
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub host: String,
    /// The `wp-site` the API serves the host under.
    pub site: &'static str,
    pub post_type: &'static str,
    pub name: String,
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !RE_HOST.is_match(&host) {
        return None;
    }
    let caps = RE_PATH.captures(url.path())?;
    let site = match host.as_str() {
        "balkans.aljazeera.net" => "ajb",
        "chinese.aljazeera.net" => "chinese",
        "mubasher.aljazeera.net" => "ajm",
        h if h.ends_with("aljazeera.net") => "aja",
        _ => "aje",
    };
    let kind = &caps[1];
    let programme = caps.get(2).is_some();
    let post_type = match kind {
        "program" | "programs" => "episode",
        "video" if programme => "episode",
        "video" | "videos" => "video",
        "feature" | "features" => "post",
        _ => "news",
    };
    Some(Link {
        host,
        site,
        post_type,
        name: caps[6].to_string(),
    })
}

/// The Brightcove player page a video's ids name.
pub fn brightcove_url(account: &str, player: &str, embed: &str, video_id: &str) -> Url {
    Url::parse(&format!(
        "https://players.brightcove.net/{account}/{player}_{embed}/index.html?videoId={video_id}"
    ))
    .expect("ids are plain")
}

pub struct AljazeeraResolver {
    http: Http,
}

impl AljazeeraResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for AljazeeraResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Al Jazeera",
            hosts: &["aljazeera.com", "aljazeera.net"],
            features: &["videos", "programmes", "articles"],
            formats: &["mp4", "hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::News],
            session: SessionSupport::None,
            examples: &[
                "https://www.aljazeera.com/video/inside-story/2026/9/10/will-foreign-workers-leave-south-africa",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let variables =
            serde_json::json!({"name": link.name, "postType": link.post_type}).to_string();
        let api = util::with_query(
            &Url::parse(&format!("https://{}/graphql", link.host)).expect("valid"),
            &[
                ("wp-site", link.site),
                ("operationName", "ArchipelagoSingleArticleQuery"),
                ("variables", &variables),
            ],
        );
        let headers = [
            ("accept".to_string(), "application/json".to_string()),
            ("wp-site".to_string(), link.site.to_string()),
            ("original-domain".to_string(), link.host.clone()),
        ];
        let fetched = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &headers, MAX_PAGE).await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        let answer = fetched.json(url)?;
        let article = &answer["data"]["article"];
        if article.is_null() {
            let reason = answer["errors"][0]["message"].as_str().unwrap_or("");
            if reason.is_empty() || reason == "no_posts_found" {
                return Err(ResolveError::NotFound(url.clone()));
            }
            return Err(ResolveError::unavailable(url, reason.to_string()));
        }
        let video = &article["video"];
        let video_id = util::text(&video["id"]);
        let account =
            util::text(&video["accountId"]).unwrap_or_else(|| DEFAULT_ACCOUNT.to_string());
        let player = util::text(&video["playerId"]).unwrap_or_else(|| DEFAULT_PLAYER.to_string());

        // The video plays through the network's Brightcove player, which the API names
        // (or the page does, when the API withholds the video): every rendition comes from
        // there, and the file the API names on the network's own CDN joins them.
        let ids = match video_id.clone() {
            Some(id) => Some((account, player, id)),
            None => {
                let page = fetch(&self.http, url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
                RE_BRIGHTCOVE.captures(&page.text()).map(|caps| {
                    (
                        caps[1].to_string(),
                        caps[2].to_string(),
                        caps[4].to_string(),
                    )
                })
            }
        };
        let source = util::url_of(&video["sourceUrl"], None);
        let mut resolved = match ids {
            Some((account, player, id)) => {
                let link = brightcove::Link::new(account, player, brightcove::Content::Video(id));
                match brightcove::media(&self.http, PLATFORM, &link, url).await {
                    Ok(resolved) => resolved,
                    Err(error) if source.is_some() => {
                        tracing::warn!(url = %url, "Al Jazeera's Brightcove player refused the video: {error}");
                        Resolved::new(PLATFORM)
                    }
                    Err(error) => return Err(error),
                }
            }
            None if source.is_some() => Resolved::new(PLATFORM),
            // Without a video of its own the post is a page like any other; the
            // generic web resolver reads whatever player it embeds.
            None => return Err(ResolveError::Unsupported(url.clone())),
        };
        if let Some(source) = source {
            if source.path().ends_with(".m3u8") {
                let expanded = hls::expand(&self.http, &source, PLATFORM, BROWSER_UA, &[]).await?;
                for variant in expanded.variants {
                    if !resolved.variants.iter().any(|v| v.url == variant.url) {
                        resolved.variants.push(variant);
                    }
                }
                for track in expanded.subtitles {
                    if !resolved.subtitles.iter().any(|t| t.url == track.url) {
                        resolved.subtitles.push(track);
                    }
                }
                if resolved.duration.is_none() {
                    resolved.duration = expanded.duration;
                }
                resolved.live |= expanded.live;
            } else if !resolved.variants.iter().any(|v| v.url == source) {
                let mut variant = Variant::file(source);
                variant.container = Some(Container::Mp4);
                variant.video = Some(VideoCodec::H264);
                variant.audio = Some(AudioCodec::Aac);
                variant.format_id = Some("source".to_string());
                variant.duration = resolved.duration;
                resolved.variants.push(variant);
            }
        }
        if resolved.variants.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        resolved.id = video_id.or_else(|| util::text(&article["id"]));
        resolved.title = video["name"]
            .as_str()
            .or(article["title"].as_str())
            .and_then(clean_title)
            .or(resolved.title);
        resolved.description = article["excerpt"]
            .as_str()
            .or(article["socialMediaSummary"].as_str())
            .map(util::clean_html)
            .and_then(|d| clean_title(&d))
            .or(resolved.description);
        resolved.thumbnail = util::url_of(&article["featuredImage"]["sourceUrl"], None)
            .or_else(|| util::url_of(&video["thumbnail"], None))
            .or(resolved.thumbnail);
        resolved.uploaded_at = util::time(&article["date"]).or(resolved.uploaded_at);
        resolved.duration = video["duration"]
            .as_str()
            .and_then(super::parse_time_stamp)
            .or(resolved.duration);
        resolved.uploader = Some("Al Jazeera".to_string());
        resolved.webpage_url = util::url_of(&article["link"], None).or_else(|| Some(url.clone()));
        Ok(Resolution::from(resolved))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};
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

    const EPISODE: &str = "https://www.aljazeera.com/video/inside-story/2026/9/10/will-foreign-workers-leave-south-africa";

    fn api(host: &str, site: &str, name: &str, post_type: &str) -> String {
        let variables = json!({"name": name, "postType": post_type}).to_string();
        util::with_query(
            &Url::parse(&format!("https://{host}/graphql")).unwrap(),
            &[
                ("wp-site", site),
                ("operationName", "ArchipelagoSingleArticleQuery"),
                ("variables", &variables),
            ],
        )
        .to_string()
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let episode = link(EPISODE).unwrap();
        assert_eq!(episode.site, "aje");
        assert_eq!(episode.post_type, "episode");
        assert_eq!(episode.name, "will-foreign-workers-leave-south-africa");
        let balkans = link("https://balkans.aljazeera.net/videos/2021/11/6/pojedini-domovi-u-sarajevu-jos-pod-vodom").unwrap();
        assert_eq!((balkans.site, balkans.post_type), ("ajb", "video"));
        let news =
            link("https://www.aljazeera.com/news/2026/9/15/can-china-play-peacemaker").unwrap();
        assert_eq!(news.post_type, "news");
        let programme =
            link("https://www.aljazeera.com/program/the-listening-post/2026/9/13/x").unwrap();
        assert_eq!(programme.post_type, "episode");
        let feature = link("https://www.aljazeera.com/features/2026/9/1/x").unwrap();
        assert_eq!(feature.post_type, "post");
        assert!(link("https://www.aljazeera.com/videos/").is_none());
        assert!(link("https://example.com/news/2026/9/15/x").is_none());
    }

    /// The player script and Playback API answer of a Brightcove video with one MP4
    /// rendition, as the network's players answer.
    fn brightcove(fixture: &mut Fixture, account: &str, player: &str, video: &str, name: &str) {
        fixture.exchanges.push(get(
            &format!("https://players.brightcove.net/{account}/{player}_default/index.min.js"),
            200,
            "application/javascript",
            r#"var x={policyKey:"BCpkADawqM1234"};"#.into(),
        ));
        fixture.exchanges.push(get(
            &format!("https://edge.api.brightcove.com/playback/v1/accounts/{account}/videos/{video}"),
            200,
            "application/json",
            json!({"id": video, "name": name, "duration": 1657000, "poster": "https://cf-images.test/poster.jpg",
                   "published_at": "2026-09-10T18:30:00.000Z",
                   "sources": [{"src": format!("https://house-fastly.brightcovecdn.test/{video}/high.mp4"), "container": "MP4", "codec": "H264", "width": 1280, "height": 720, "avg_bitrate": 2000000, "size": 4000000}],
                   "text_tracks": []}).to_string(),
        ));
    }

    #[tokio::test]
    async fn episodes_resolve_through_brightcove_with_the_networks_own_file() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            &api("www.aljazeera.com", "aje", "will-foreign-workers-leave-south-africa", "episode"),
            200,
            "application/json",
            json!({"data": {"article": {
                "id": "12345", "title": "Will foreign workers leave South Africa?", "excerpt": "<p>Inside Story asks.</p>",
                "date": "2026-09-10T18:30:00", "link": "https://www.aljazeera.com/video/inside-story/2026/9/10/will-foreign-workers-leave-south-africa",
                "featuredImage": {"sourceUrl": "https://www.aljazeera.com/wp-content/uploads/2026/09/x.jpg"},
                "video": {"id": "6404882346112", "duration": "27:37", "name": "Will foreign workers leave South Africa? I Inside story",
                          "accountId": "665003303001", "playerId": "6tKQRAx7lu",
                          "sourceUrl": "https://ajmn-aje-vod.akamaized.net/media/v1/pmp4/static/clear/665003303001/x/main.mp4"}
            }}}).to_string(),
        ));
        brightcove(
            &mut fixture,
            "665003303001",
            "6tKQRAx7lu",
            "6404882346112",
            "Inside Story",
        );
        let resolver = AljazeeraResolver::new(Http::replay(fixture));
        let url = Url::parse(EPISODE).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.resolver, PLATFORM);
        assert_eq!(resolved.id.as_deref(), Some("6404882346112"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Will foreign workers leave South Africa? I Inside story")
        );
        assert_eq!(resolved.description.as_deref(), Some("Inside Story asks."));
        assert_eq!(resolved.duration, Some(Duration::from_secs(27 * 60 + 37)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1789065000)
        );
        assert_eq!(resolved.uploader.as_deref(), Some("Al Jazeera"));
        assert_eq!(
            resolved.variants.len(),
            2,
            "the player's rendition and the network's file"
        );
        assert_eq!(resolved.variants[0].height, Some(720));
        assert_eq!(
            resolved.variants[1].url.as_str(),
            "https://ajmn-aje-vod.akamaized.net/media/v1/pmp4/static/clear/665003303001/x/main.mp4"
        );
        assert_eq!(resolved.variants[1].format_id.as_deref(), Some("source"));
    }

    #[tokio::test]
    async fn recorded_episode_resolves_with_every_rendition() {
        let fixture = Fixture::parse(include_str!("aljazeera_fixture.json")).unwrap();
        let resolver = AljazeeraResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse(EPISODE).unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.resolver, PLATFORM);
        assert_eq!(
            resolved.title.as_deref(),
            Some("Will foreign workers leave South Africa? I Inside story")
        );
        assert!(resolved.variants.len() > 10, "{}", resolved.variants.len());
        assert!(
            resolved
                .variants
                .iter()
                .any(|v| v.kind == super::super::VariantKind::File)
        );
        assert!(
            resolved
                .variants
                .iter()
                .any(|v| v.kind == super::super::VariantKind::Hls && v.height.is_some())
        );
        assert!(resolved.variants.iter().any(|v| v.height == Some(1080)));
        assert!(
            resolved
                .variants
                .iter()
                .any(|v| v.url.host_str() == Some("ajmn-aje-vod.akamaized.net")),
            "the network's own file is among the renditions"
        );
        assert!(matches!(
            resolver
                .resolve(
                    &Url::parse("https://www.aljazeera.com/news/2026/9/10/no-such-post-here")
                        .unwrap()
                )
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn videos_without_a_file_resolve_through_brightcove_and_missing_posts_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            &api("balkans.aljazeera.net", "ajb", "djokovic-usao-u-finale", "video"),
            200,
            "application/json",
            json!({"data": {"article": {"id": "1", "title": "Djokovic", "video": {"id": "6285347919001", "accountId": "911432371001", "playerId": "csvTfAlKW"}}}}).to_string(),
        ));
        brightcove(
            &mut fixture,
            "911432371001",
            "csvTfAlKW",
            "6285347919001",
            "Djokovic u finalu",
        );
        fixture.exchanges.push(get(
            &api("www.aljazeera.com", "aje", "no-such-post", "news"),
            200,
            "application/json",
            json!({"errors": [{"message": "no_posts_found"}], "data": {"article": null}})
                .to_string(),
        ));
        fixture.exchanges.push(get(
            &api("www.aljazeera.com", "aje", "player-on-page", "news"),
            200,
            "application/json",
            json!({"data": {"article": {"id": "2", "title": "On the page", "video": null}}})
                .to_string(),
        ));
        fixture.exchanges.push(get(
            "https://www.aljazeera.com/news/2026/9/15/player-on-page",
            200,
            "text/html",
            r#"<html><iframe src="https://players.brightcove.net/665003303001/6tKQRAx7lu_default/index.html?videoId=6404882346112"></iframe></html>"#.into(),
        ));
        brightcove(
            &mut fixture,
            "665003303001",
            "6tKQRAx7lu",
            "6404882346112",
            "From the page",
        );
        fixture.exchanges.push(get(
            &api("www.aljazeera.com", "aje", "text-only", "news"),
            200,
            "application/json",
            json!({"data": {"article": {"id": "3", "title": "Words", "video": null}}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://www.aljazeera.com/news/2026/9/15/text-only",
            200,
            "text/html",
            "<html><p>words</p></html>".into(),
        ));
        let resolver = AljazeeraResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(
                &Url::parse(
                    "https://balkans.aljazeera.net/videos/2021/11/6/djokovic-usao-u-finale",
                )
                .unwrap(),
            )
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.resolver, PLATFORM);
        assert_eq!(resolved.title.as_deref(), Some("Djokovic"));
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].height, Some(720));
        assert!(matches!(
            resolver
                .resolve(
                    &Url::parse("https://www.aljazeera.com/news/2026/9/15/no-such-post").unwrap()
                )
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let resolved = resolver
            .resolve(
                &Url::parse("https://www.aljazeera.com/news/2026/9/15/player-on-page").unwrap(),
            )
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("2"));
        assert_eq!(resolved.title.as_deref(), Some("On the page"));
        assert_eq!(resolved.variants[0].url.path(), "/6404882346112/high.mp4");
        let text_only = Url::parse("https://www.aljazeera.com/news/2026/9/15/text-only").unwrap();
        assert!(matches!(
            resolver.resolve(&text_only).await.unwrap_err(),
            ResolveError::Unsupported(u) if u == text_only
        ));
    }
}
