//! BuzzFeed (buzzfeed.com) posts: the videos a post embeds, read from the post data its
//! page hands Next.js (`__NEXT_DATA__`): the `video` sub-buzzes (a YouTube video by its
//! id, other players by the link the post was given) and the `embed` sub-buzzes that frame
//! a Facebook post, an Instagram reel, a TikTok, YouTube or Vimeo video. A post with one
//! video is handed to the video's host; a post with several is a playlist of them.

use async_trait::async_trait;
use serde_json::Value;
use url::Url;

use super::page::between;
use super::{
    MAX_PAGE, Page, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolver,
    SessionSupport, clean_title, fetch_ok, navigation_headers, util,
};
use crate::http::{BROWSER_UA, Http};

pub const PLATFORM: &str = "buzzfeed";
const NEXT_DATA_START: &str = r#"<script id="__NEXT_DATA__" type="application/json">"#;

/// The post id a post link names: the path after the author's segment.
pub fn post_id(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    if host != "buzzfeed.com" {
        return None;
    }
    let path = url.path();
    let rest = path.strip_prefix('/')?;
    let (_, id) = rest.split_once('/')?;
    (!id.is_empty()).then(|| id.to_string())
}

/// A video a post embeds: where it plays, and the caption the post gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Embedded {
    pub url: Url,
    pub title: Option<String>,
}

/// The post (`props.pageProps.buzz`) in the page's Next.js data; `None` when the page is
/// not a post.
pub fn post_data(next_data: &str) -> Option<Value> {
    let data: Value = serde_json::from_str(next_data).ok()?;
    let buzz = &data["props"]["pageProps"]["buzz"];
    buzz.is_object().then(|| buzz.clone())
}

/// The Next.js data a page carries.
pub fn next_data(html: &str) -> Option<&str> {
    between(html, NEXT_DATA_START, "</script>")
}

/// A Facebook link as the Facebook resolver reads it: `video.php?v=…` and
/// `video/embed?video_id=…` become the watch link of that video, `plugins/video.php?href=…`
/// the page it frames.
fn facebook_link(url: &Url) -> Option<Url> {
    let path = url.path();
    if path == "/video.php" || path.starts_with("/video/embed") {
        let id = util::query_param(url, "v").or_else(|| util::query_param(url, "video_id"))?;
        return Url::parse(&format!("https://www.facebook.com/watch/?v={id}")).ok();
    }
    if path.starts_with("/plugins/video.php") {
        let href = util::query_param(url, "href")?;
        return util::join_url(None, &href);
    }
    Some(url.clone())
}

/// The link a sub-buzz was made from: its `original_url`, else its `url`.
fn given_link(sub: &Value) -> Option<Url> {
    ["original_url", "url"]
        .iter()
        .find_map(|key| util::url_of(&sub[key], None))
}

/// Where a `video` sub-buzz plays: a YouTube video by its id, any other host by the link
/// the post was given.
fn video_link(sub: &Value) -> Option<Url> {
    if sub["source"].as_str() == Some("youtube")
        && let Some(id) = sub["source_id"].as_str().filter(|id| !id.is_empty())
    {
        return Url::parse(&format!("https://www.youtube.com/watch?v={id}")).ok();
    }
    given_link(sub)
}

/// Where an `embed` sub-buzz plays, when what it frames is a video: any Facebook post,
/// an Instagram reel or IGTV video, a TikTok video, a YouTube or Vimeo video.
fn embed_link(sub: &Value) -> Option<Url> {
    let link = given_link(sub)?;
    let host = link.host_str()?.to_ascii_lowercase();
    let host = host
        .strip_prefix("www.")
        .or_else(|| host.strip_prefix("m."))
        .unwrap_or(&host);
    let path = link.path();
    match host {
        "facebook.com" | "fb.com" | "fb.watch" => facebook_link(&link),
        "instagram.com" => {
            (path.starts_with("/reel/") || path.starts_with("/reels/") || path.starts_with("/tv/"))
                .then_some(link)
        }
        "vm.tiktok.com" => Some(link),
        "tiktok.com" => path.contains("/video/").then_some(link),
        "youtube.com" | "youtu.be" | "vimeo.com" | "player.vimeo.com" => Some(link),
        _ => None,
    }
}

/// The videos a post's sub-buzzes embed, in post order, each once.
pub fn videos_in(post: &Value) -> Vec<Embedded> {
    let mut found: Vec<Embedded> = Vec::new();
    for sub in post["sub_buzzes"].as_array().into_iter().flatten() {
        let link = match sub["form"].as_str() {
            Some("video") => video_link(sub),
            Some("embed") => embed_link(sub),
            _ => None,
        };
        let Some(link) = link else {
            continue;
        };
        if found.iter().any(|e| e.url == link) {
            continue;
        }
        found.push(Embedded {
            title: sub["header"]
                .as_str()
                .map(util::clean_html)
                .and_then(|h| clean_title(&h)),
            url: link,
        });
    }
    found
}

pub struct BuzzfeedResolver {
    http: Http,
}

impl BuzzfeedResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for BuzzfeedResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "BuzzFeed",
            hosts: &["buzzfeed.com"],
            features: &["posts", "playlists"],
            formats: &["mp4", "hls"],
            session: SessionSupport::None,
            examples: &[
                "http://www.buzzfeed.com/abagg/this-angry-ram-destroys-a-punching-bag-like-a-boss?utm_term=4ldqpia",
                "http://www.buzzfeed.com/craigsilverman/the-most-adorable-crash-landing-ever#.eq7pX0BAmK",
                "https://www.buzzfeed.com/emlyntravis/the-30-best-k-pop-music-videos-of-the-year-5bhzmv5641",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        post_id(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let id = post_id(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
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
        let next_data = next_data(&html)
            .ok_or_else(|| ResolveError::malformed(url, "no __NEXT_DATA__ on the page"))?;
        let post = post_data(next_data).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let mut videos = videos_in(&post);
        videos.truncate(500);
        match videos.len() {
            0 => Err(ResolveError::NotFound(url.clone())),
            1 => Err(ResolveError::Redirect(videos.remove(0).url)),
            _ => {
                let page = Page::parse(&html, &fetched.url);
                let title = page
                    .meta("og:title")
                    .and_then(|t| clean_title(&t))
                    .or_else(|| post["title"].as_str().and_then(clean_title));
                let total = videos.len();
                Ok(Resolution::Playlist(Playlist {
                    resolver: PLATFORM.to_string(),
                    id: Some(id),
                    title,
                    entries: videos
                        .into_iter()
                        .map(|video| PlaylistEntry {
                            url: video.url,
                            title: video.title,
                            duration: None,
                        })
                        .collect(),
                    total: Some(total),
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::Fixture;
    use serde_json::json;

    const RAM: &str = "http://www.buzzfeed.com/abagg/this-angry-ram-destroys-a-punching-bag-like-a-boss?utm_term=4ldqpia";
    const GOSLING: &str =
        "http://www.buzzfeed.com/craigsilverman/the-most-adorable-crash-landing-ever#.eq7pX0BAmK";
    const MANY: &str = "https://www.buzzfeed.com/emlyntravis/the-30-best-k-pop-music-videos-of-the-year-5bhzmv5641";

    fn resolver() -> BuzzfeedResolver {
        let fixture = Fixture::parse(include_str!("buzzfeed_fixture.json")).unwrap();
        BuzzfeedResolver::new(Http::replay(fixture))
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn links_are_read() {
        let id = |s: &str| post_id(&url(s));
        assert_eq!(
            id(RAM),
            Some("this-angry-ram-destroys-a-punching-bag-like-a-boss".into())
        );
        assert_eq!(
            id("http://www.buzzfeed.com/sheridanwatson/look-at-this-cute-dog-omg?utm_term=4ldqpia"),
            Some("look-at-this-cute-dog-omg".into())
        );
        assert_eq!(
            id(GOSLING),
            Some("the-most-adorable-crash-landing-ever".into())
        );
        assert_eq!(id("https://buzzfeed.com/a/b/c"), Some("b/c".into()));
        assert_eq!(id("https://www.buzzfeed.com/abagg"), None);
        assert_eq!(id("https://www.buzzfeednews.com/abagg/x"), None);
    }

    #[test]
    fn sub_buzzes_name_their_videos() {
        let post = json!({"sub_buzzes": [
            {"form": "video", "source": "youtube", "source_id": "aVCR29aE_OQ", "original_url": "http://youtube.com/watch?v=aVCR29aE_OQ", "url": "https://www.youtube.com/v/aVCR29aE_OQ", "header": "Rambro got out <i>all</i> his stress."},
            {"form": "video", "source": "vimeo", "source_id": "76979871", "original_url": "https://vimeo.com/76979871", "header": ""},
            {"form": "video", "source": "youtube", "source_id": "aVCR29aE_OQ", "header": "again"},
            {"form": "embed", "source": "facebook", "source_type": "facebook", "original_url": "http://www.facebook.com/video.php?v=971793786185728", "url": "https://www.facebook.com/video.php?v=971793786185728", "header": "So some kind Canadians set up crash mats."},
            {"form": "embed", "source": "facebook", "original_url": "https://www.facebook.com/video/embed?video_id=123"},
            {"form": "embed", "source": "facebook", "original_url": "https://www.facebook.com/plugins/video.php?href=https%3A%2F%2Fwww.facebook.com%2Fcalgary%2Fvideos%2F456%2F&show_text=0"},
            {"form": "embed", "source": "facebook", "original_url": "https://www.facebook.com/calgary/posts/789"},
            {"form": "embed", "source": "instagram", "original_url": "https://instagram.com/p/CFYJwAyAsBY/", "url": "https://www.instagram.com/p/CFYJwAyAsBY/embed/"},
            {"form": "embed", "source": "instagram", "original_url": "https://www.instagram.com/reel/C1a2b3c4d5e/"},
            {"form": "embed", "source": "tiktok", "original_url": "https://www.tiktok.com/@user/video/7300000000000000000"},
            {"form": "embed", "source": "tiktok", "original_url": "https://www.tiktok.com/@user"},
            {"form": "embed", "source": "youtube", "original_url": "https://youtu.be/mVmBL8B-In0"},
            {"form": "embed", "source": "anonymous-comments-google-form", "embed_domain": "https://embed.contagiousmedia.com"},
            {"form": "embed", "source": "twitter", "original_url": "https://twitter.com/BuzzFeed/status/1"},
            {"form": "image", "url": "https://img.buzzfeed.com/x.jpg"},
            {"form": "bfp", "format_name": "quiz"}
        ]});
        let videos = videos_in(&post);
        let links: Vec<&str> = videos.iter().map(|v| v.url.as_str()).collect();
        assert_eq!(
            links,
            vec![
                "https://www.youtube.com/watch?v=aVCR29aE_OQ",
                "https://vimeo.com/76979871",
                "https://www.facebook.com/watch/?v=971793786185728",
                "https://www.facebook.com/watch/?v=123",
                "https://www.facebook.com/calgary/videos/456/",
                "https://www.facebook.com/calgary/posts/789",
                "https://www.instagram.com/reel/C1a2b3c4d5e/",
                "https://www.tiktok.com/@user/video/7300000000000000000",
                "https://youtu.be/mVmBL8B-In0",
            ]
        );
        assert_eq!(
            videos[0].title.as_deref(),
            Some("Rambro got out all his stress.")
        );
        assert_eq!(videos[1].title, None);
        assert_eq!(
            videos[2].title.as_deref(),
            Some("So some kind Canadians set up crash mats.")
        );
        assert!(videos_in(&json!({"sub_buzzes": []})).is_empty());
        assert!(videos_in(&json!({"title": "no sub-buzzes"})).is_empty());
    }

    #[test]
    fn pages_carry_their_post() {
        let html = r#"<html><body><script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{"buzz":{"id":"3518553","title":"Ram","sub_buzzes":[]}}}}</script></body></html>"#;
        let post = post_data(next_data(html).unwrap()).unwrap();
        assert_eq!(post["id"].as_str(), Some("3518553"));
        let tag_page = r#"<script id="__NEXT_DATA__" type="application/json">{"props":{"pageProps":{"feed":[]}}}</script>"#;
        assert_eq!(post_data(next_data(tag_page).unwrap()), None);
        assert_eq!(next_data("<html>no data</html>"), None);
    }

    #[tokio::test]
    async fn a_post_with_one_video_hands_it_on() {
        let resolver = resolver();
        assert!(resolver.matches(&url(RAM)));
        let error = resolver.resolve(&url(RAM)).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Redirect(target) if target.as_str() == "https://www.youtube.com/watch?v=aVCR29aE_OQ"),
            "{error}"
        );
        let error = resolver.resolve(&url(GOSLING)).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Redirect(target) if target.as_str() == "https://www.facebook.com/watch/?v=971793786185728"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_post_with_several_videos_is_a_playlist() {
        let resolver = resolver();
        let Resolution::Playlist(playlist) = resolver.resolve(&url(MANY)).await.unwrap() else {
            panic!("expected a playlist");
        };
        assert_eq!(
            playlist.id.as_deref(),
            Some("the-30-best-k-pop-music-videos-of-the-year-5bhzmv5641")
        );
        assert_eq!(
            playlist.title.as_deref(),
            Some(
                "30 K-Pop Music Videos Released This Year That Prove You Need To Start Listening To K-Pop, Like, Yesterday"
            )
        );
        assert_eq!(playlist.entries.len(), 30);
        assert_eq!(playlist.total, Some(30));
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://www.youtube.com/watch?v=4gX_p1VkgA4"
        );
        assert_eq!(
            playlist.entries[0].title.as_deref(),
            Some("30. \"Adios\" by Everglow")
        );
        assert_eq!(
            playlist.entries[29].url.as_str(),
            "https://www.youtube.com/watch?v=YBnGBb1wg98"
        );
        assert_eq!(
            playlist.entries[29].title.as_deref(),
            Some("1. \"Zimzalabim\" by Red Velvet")
        );
        assert!(
            playlist
                .entries
                .iter()
                .all(|e| e.url.host_str() == Some("www.youtube.com"))
        );
    }
}
