//! XNXX videos, porn makers and pornstars. The pages run the XVideos player, so a video
//! page hands it the HLS playlist in the same `html5player.setVideoHLS` call, with the
//! title, thumbnails and uploader beside it and the upload date, length and description
//! in the page's JSON-LD. Porn maker, pornstar and profile pages list their uploads
//! through the same listing JSON, read here through the XVideos helpers.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use url::Url;

use super::xvideos::{resolve_listing, resolve_player_page};
use super::{Platform, Resolution, ResolveError, Resolver, SessionSupport, Tag, util};
use crate::http::Http;
use crate::media::MediaKind;

pub const PLATFORM: &str = "xnxx";
const SITE: &str = "https://www.xnxx.com";

/// `xnxx.com` with its `video.` and `www.` subdomains, and the `xnxx2.com`, `xnxx3.com`,
/// `xnxx.tv` and `xnxx.es` mirrors.
static RE_HOST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[a-z0-9-]+\.)?xnxx[23]?\.(?:com|tv|es)$").unwrap());
/// `/video-{id}/slug`, and the older `/video{digits}/slug`.
static RE_VIDEO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/video-?([0-9a-z]+)(?:/|$)").unwrap());
/// `/embedframe/{id}`.
static RE_EMBED: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/embedframe/([0-9a-z]+)/?$").unwrap());
/// `/prof-video-click/upload/{name}/{id}/slug`: how listings link their videos.
static RE_PROF_CLICK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/prof-video-click/[a-z]+/[^/]+/([0-9a-z]+)(?:/|$)").unwrap());
/// `/porn-maker/{name}`, `/pornstar/{name}` (which redirects to the porn maker page),
/// `/profiles/{name}` and `/channels/{name}`.
static RE_LISTING: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^/(?:porn-maker|pornstar|profiles|channels)/([A-Za-z0-9_.-]+)/?$").unwrap()
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Video {
        id: String,
    },
    /// A porn maker's, pornstar's or profile's uploads, by the name in its path.
    Listing {
        name: String,
    },
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !RE_HOST.is_match(&host) {
        return None;
    }
    let path = url.path();
    if let Some(caps) = RE_VIDEO
        .captures(path)
        .or_else(|| RE_EMBED.captures(path))
        .or_else(|| RE_PROF_CLICK.captures(path))
    {
        return Some(Link::Video {
            id: caps[1].to_string(),
        });
    }
    if path.starts_with("/swf/")
        && let Some(id) = util::query_param(url, "id_video")
        && id.chars().all(|c| c.is_ascii_alphanumeric())
    {
        return Some(Link::Video { id });
    }
    RE_LISTING.captures(path).map(|caps| Link::Listing {
        name: caps[1].to_string(),
    })
}

/// The page of a video: `/video-{id}/_`.
fn video_page(id: &str) -> Url {
    Url::parse(&format!("{SITE}/video-{id}/_")).expect("valid")
}

fn uploader_url(name: &str) -> Option<Url> {
    Url::parse(&format!("{SITE}/porn-maker/{name}")).ok()
}

fn entry_url(eid: &str, slug: &str) -> Url {
    Url::parse(&format!("{SITE}/video-{eid}/{slug}")).expect("valid")
}

pub struct XnxxResolver {
    http: Http,
}

impl XnxxResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for XnxxResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "XNXX",
            hosts: &["xnxx.com", "xnxx2.com", "xnxx3.com", "xnxx.tv", "xnxx.es"],
            features: &["videos", "embeds", "porn makers", "pornstars", "profiles"],
            formats: &["hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::Nsfw, Tag::Video],
            session: SessionSupport::None,
            on_by_default: true,
            examples: &[
                "https://www.xnxx.com/video-55awb78/skyrim_test_video",
                "https://www.xnxx.com/embedframe/55awb78",
                "https://www.xnxx.es/video-55awb78/skyrim_test_video",
                "https://www.xnxx.com/porn-maker/glurp",
                "https://www.xnxx.com/pornstar/mia-khalifa",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Video { id } => {
                let mut resolved =
                    resolve_player_page(&self.http, PLATFORM, &video_page(&id), url, &uploader_url)
                        .await?;
                if resolved.id.is_none() {
                    resolved.id = Some(id);
                }
                Ok(Resolution::from(resolved))
            }
            Link::Listing { name } => {
                resolve_listing(&self.http, PLATFORM, SITE, &name, url, &entry_url).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };
    use serde_json::json;

    fn get(url: &str, status: u16, content_type: &str, body: &str) -> Exchange {
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
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    const VIDEO_HTML: &str = r#"<html><head><title>Skyrim Test Video - XNXX.COM</title>
<meta property="og:title" content="Skyrim Test Video" />
<meta property="og:url" content="https://www.xnxx.com/video-55awb78/skyrim_test_video" />
<meta property="og:duration" content="469" />
<meta property="og:image" content="https://thumb-cdn77.xnxx-cdn.com/606b175b/0/xn_12_t.jpg" />
<script type="application/ld+json">{"@context":"https://schema.org","@type":"VideoObject","name":"Skyrim Test Video","description":"Skyrim Test Video","uploadDate":"2014-08-01T06:38:18+00:00","duration":"PT00H07M49S"}</script>
</head><body><script>
html5player.setVideoTitle('Skyrim Test Video');
html5player.setEncodedIdVideo('55awb78');
html5player.setThumbUrl('https://thumb-cdn77.xnxx-cdn.com/606b175b/0/xv_12_t.jpg');
html5player.setVideoUrlLow('https://mp4-cdn77.xnxx-cdn.com/606b175b/0/video_240p.mp4?secure=a,1');
html5player.setVideoUrlHigh('https://mp4-cdn77.xnxx-cdn.com/606b175b/0/video_360p.mp4?secure=b,1');
html5player.setVideoHLS('https://hls-cdn77.xnxx-cdn.com/c,1/606b175b/0/hls.m3u8');
html5player.setThumbUrl169('https://thumb-cdn77.xnxx-cdn.com/606b175b/0/xv_10_t.jpg');
html5player.setUploaderName('glurp');
</script></body></html>"#;

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let video = |id: &str| Some(Link::Video { id: id.into() });
        let listing = |name: &str| Some(Link::Listing { name: name.into() });
        assert_eq!(
            link("http://www.xnxx.com/video-55awb78/skyrim_test_video"),
            video("55awb78")
        );
        assert_eq!(link("http://www.xnxx.com/video-55awb78/"), video("55awb78"));
        assert_eq!(
            link("http://video.xnxx.com/video1135332/lida_naked_funny_actress_5_"),
            video("1135332")
        );
        assert_eq!(
            link("http://www.xnxx3.com/video-55awb78/"),
            video("55awb78")
        );
        assert_eq!(
            link("https://www.xnxx.es/video-55awb78/x"),
            video("55awb78")
        );
        assert_eq!(
            link("https://www.xnxx.tv/video-55awb78/x"),
            video("55awb78")
        );
        assert_eq!(
            link("https://www.xnxx.com/embedframe/55awb78"),
            video("55awb78")
        );
        assert_eq!(
            link("https://www.xnxx.com/prof-video-click/upload/glurp/icbectkd155/giant"),
            video("icbectkd155")
        );
        assert_eq!(
            link("https://www.xnxx.com/porn-maker/glurp"),
            listing("glurp")
        );
        assert_eq!(
            link("https://www.xnxx.com/pornstar/mia-khalifa/"),
            listing("mia-khalifa")
        );
        assert_eq!(
            link("https://www.xnxx.com/profiles/glurp"),
            listing("glurp")
        );
        assert_eq!(link("https://www.xnxx.com/search/skyrim"), None);
        assert_eq!(link("https://www.xnxx.com/tags"), None);
        assert_eq!(link("https://www.xnxx.com/"), None);
        assert_eq!(link("https://www.xvideos.com/video.keecekh888c/x"), None);
    }

    #[tokio::test]
    async fn videos_resolve_through_the_shared_player() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.xnxx.com/video-55awb78/_",
            200,
            "text/html",
            VIDEO_HTML,
        ));
        fixture.exchanges.push(get(
            "https://hls-cdn77.xnxx-cdn.com/c,1/606b175b/0/hls.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:PROGRAM-ID=1,BANDWIDTH=763904,RESOLUTION=640x480,NAME=\"480p\"\nhls-480p-4023d.m3u8\n",
        ));
        fixture.exchanges.push(get(
            "https://hls-cdn77.xnxx-cdn.com/c,1/606b175b/0/hls-480p-4023d.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n0.ts\n#EXT-X-ENDLIST\n",
        ));
        fixture.exchanges.push(get(
            "https://www.xnxx.com/video-zzzzzzz/_",
            404,
            "text/html",
            "<html>Not found</html>",
        ));
        let resolver = XnxxResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.xnxx.com/video-55awb78/skyrim_test_video").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        crate::resolve::assert_one_family(&resolved.variants);
        assert_eq!(resolved.resolver, PLATFORM);
        assert_eq!(resolved.id.as_deref(), Some("55awb78"));
        assert_eq!(resolved.title.as_deref(), Some("Skyrim Test Video"));
        assert_eq!(resolved.description, None, "the same text as the title");
        assert_eq!(resolved.uploader.as_deref(), Some("glurp"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://www.xnxx.com/porn-maker/glurp"
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(469)));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.age_limit, Some(18));
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].height, Some(480));
        assert_eq!(resolved.variants[0].format_id.as_deref(), Some("hls-480p"));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.xnxx.com/video-zzzzzzz/x").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn porn_makers_list_their_uploads_with_xnxx_links() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.xnxx.com/profiles/glurp/videos/new/0",
            200,
            "application/json",
            &json!({"nb_videos": 2, "nb_per_page": 50, "current_page": 0, "result": true, "videos": [
                {"id": 1, "eid": "rxmo3c8", "u": "/prof-video-click/upload/glurp/icbectkd155/giant_hunter_and_ravaged", "tf": "Giant hunter and ravaged", "d": "5 min", "p": "glurp", "pn": "Glurp"},
                {"id": 2, "eid": "xc2gl5c", "u": "/prof-video-click/upload/glurp/kopolmo4102/skyrim_caitlyn", "tf": "Skyrim Caitlyn", "d": "58 sec", "p": "glurp", "pn": "Glurp"}
            ]}).to_string(),
        ));
        let resolver = XnxxResolver::new(Http::replay(fixture));
        let Resolution::Playlist(playlist) = resolver
            .resolve(&Url::parse("https://www.xnxx.com/porn-maker/glurp").unwrap())
            .await
            .unwrap()
        else {
            panic!("a porn maker page is a playlist");
        };
        assert_eq!(playlist.resolver, PLATFORM);
        assert_eq!(playlist.title.as_deref(), Some("Glurp"));
        assert_eq!(playlist.total, Some(2));
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://www.xnxx.com/video-rxmo3c8/giant_hunter_and_ravaged"
        );
        assert_eq!(playlist.entries[0].duration, Some(Duration::from_secs(300)));
        assert_eq!(playlist.entries[1].duration, Some(Duration::from_secs(58)));
    }

    /// Every example link resolves live.
    #[ignore = "reaches the live site: cargo test -- --ignored"]
    #[tokio::test]

    async fn live_examples_resolve() {
        let resolver = XnxxResolver::new(Http::new(crate::http::HttpConfig::default()));
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
            assert!(resolver.matches(&url), "{link}: not matched");
            let resolution = tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&url))
                .await
                .expect("resolution timed out")
                .unwrap_or_else(|e| panic!("{link}: {e}"));
            match resolution {
                Resolution::Media(resolved) => {
                    assert!(
                        resolved.variants.iter().any(|v| v.is_playable()),
                        "{link}: no variants"
                    );
                    println!(
                        "{link}: {:?} ({} variants)",
                        resolved.title,
                        resolved.variants.len()
                    );
                }
                Resolution::Playlist(playlist) => {
                    assert!(!playlist.entries.is_empty(), "{link}: no entries");
                    println!(
                        "{link}: {:?} ({} of {:?} entries)",
                        playlist.title,
                        playlist.entries.len(),
                        playlist.total
                    );
                }
            }
        }
    }
}
