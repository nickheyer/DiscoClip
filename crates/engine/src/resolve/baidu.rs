//! Baidu Video (百度视频) series pages: the episode list, through the JSON API the
//! site's app reads. Every episode links to the player of the site that hosts it.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolver,
    SessionSupport, Tag, clean_title, fetch_ok, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::MediaKind;

pub const PLATFORM: &str = "baidu";
const API: &str = "http://app.video.baidu.com";

/// A series page: `/comic/1069.htm`, `/show/11595.htm`, `/tv/123.htm`, `/movie/45.htm`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// The page's category, as the path names it.
    pub category: String,
    pub id: String,
}

static RE_SERIES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/([a-z]+)/(\d+)\.htm").unwrap());

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    if !url.host_str()?.eq_ignore_ascii_case("v.baidu.com") {
        return None;
    }
    let caps = RE_SERIES.captures(url.path())?;
    Some(Link {
        category: caps[1].to_string(),
        id: caps[2].to_string(),
    })
}

/// The `worktype` the API wants for a page category: `show` pages are TV shows and `tv`
/// pages TV plays. The other categories keep their name.
pub fn work_type(category: &str) -> String {
    let category = match category {
        "show" => "tvshow",
        "tv" => "tvplay",
        other => other,
    };
    format!("adnative{category}")
}

pub struct BaiduResolver {
    http: Http,
}

impl BaiduResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    async fn api(
        &self,
        path: &str,
        work_type: &str,
        id: &str,
        origin: &Url,
    ) -> Result<Value, ResolveError> {
        let api =
            Url::parse(&format!("{API}/{path}/?worktype={work_type}&id={id}")).expect("valid");
        let fetched = fetch_ok(&self.http, &api, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        fetched.json(origin)
    }
}

#[async_trait]
impl Resolver for BaiduResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Baidu Video",
            hosts: &["v.baidu.com"],
            features: &["series", "playlists"],
            formats: &[],
            media: &[MediaKind::Video],
            tags: &[Tag::Video],
            session: SessionSupport::None,
            examples: &[
                "http://v.baidu.com/comic/1069.htm?frp=bdbrand&q=%E4%B8%AD%E5%8D%8E%E5%B0%8F%E5%BD%93%E5%AE%B6",
                "http://v.baidu.com/show/10.htm?frp=bdbrand",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let work_type = work_type(&link.category);
        let series = self.api("xqinfo", &work_type, &link.id, url).await?;
        let title = util::text(&series["title"])
            .ok_or_else(|| ResolveError::malformed(url, "the series has no title"))?;
        let episodes = self.api("xqsingle", &work_type, &link.id, url).await?;
        let mut entries = Vec::new();
        for episode in episodes["videos"].as_array().into_iter().flatten() {
            let episode_url = util::url_of(&episode["url"], Some(url)).ok_or_else(|| {
                ResolveError::malformed(url, "an episode of the series has no link")
            })?;
            entries.push(PlaylistEntry {
                url: episode_url,
                title: episode["title"].as_str().and_then(clean_title),
                duration: None,
            });
        }
        if entries.is_empty() {
            return Err(ResolveError::unavailable(
                url,
                "the series lists no episodes",
            ));
        }
        let total = util::uint(&episodes["total_num"])
            .map(|n| n as usize)
            .filter(|n| *n > entries.len());
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(link.id),
            title: clean_title(&title),
            entries,
            total,
        }))
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("http://v.baidu.com/comic/1069.htm?frp=bdbrand&q=%E4%B8%AD"),
            Some(Link {
                category: "comic".into(),
                id: "1069".into()
            })
        );
        assert_eq!(
            link("http://v.baidu.com/show/11595.htm?frp=bdbrand"),
            Some(Link {
                category: "show".into(),
                id: "11595".into()
            })
        );
        assert_eq!(
            link("https://v.baidu.com/tv/42.htm"),
            Some(Link {
                category: "tv".into(),
                id: "42".into()
            })
        );
        assert_eq!(link("http://v.baidu.com/comic/1069"), None);
        assert_eq!(link("http://v.baidu.com/Comic/1069.htm"), None);
        assert_eq!(link("http://www.baidu.com/comic/1069.htm"), None);
        assert_eq!(link("ftp://v.baidu.com/comic/1069.htm"), None);
    }

    #[test]
    fn work_types_follow_the_category() {
        assert_eq!(work_type("comic"), "adnativecomic");
        assert_eq!(work_type("show"), "adnativetvshow");
        assert_eq!(work_type("tv"), "adnativetvplay");
        assert_eq!(work_type("movie"), "adnativemovie");
    }

    #[tokio::test]
    async fn series_resolve_to_episode_playlists() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "http://app.video.baidu.com/xqinfo/?worktype=adnativetvshow&id=11595",
            200,
            "application/json",
            json!({"id": "11595", "title": "奔跑吧兄弟  第三季", "intro": "综艺&amp;节目"})
                .to_string(),
        ));
        fixture.exchanges.push(get(
            "http://app.video.baidu.com/xqsingle/?worktype=adnativetvshow&id=11595",
            200,
            "application/json",
            json!({"id": "11595", "total_num": 12, "videos": [
                {"title": "第1期", "url": "http://www.iqiyi.com/v_19rrjzi4kg.html", "episode": "1"},
                {"title": "第2期", "url": "//v.qq.com/x/cover/abc.html", "episode": "2"},
            ]})
            .to_string(),
        ));
        let resolver = BaiduResolver::new(Http::replay(fixture));
        let url = Url::parse("http://v.baidu.com/show/11595.htm?frp=bdbrand").unwrap();
        assert!(resolver.matches(&url));
        let Resolution::Playlist(playlist) = resolver.resolve(&url).await.unwrap() else {
            panic!("a series is a playlist");
        };
        assert_eq!(playlist.id.as_deref(), Some("11595"));
        assert_eq!(playlist.title.as_deref(), Some("奔跑吧兄弟 第三季"));
        assert_eq!(playlist.total, Some(12));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "http://www.iqiyi.com/v_19rrjzi4kg.html"
        );
        assert_eq!(playlist.entries[0].title.as_deref(), Some("第1期"));
        assert_eq!(
            playlist.entries[1].url.as_str(),
            "http://v.qq.com/x/cover/abc.html"
        );
    }

    #[tokio::test]
    async fn missing_and_empty_series_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "http://app.video.baidu.com/xqinfo/?worktype=adnativecomic&id=1",
            404,
            "text/html",
            "".into(),
        ));
        fixture.exchanges.push(get(
            "http://app.video.baidu.com/xqinfo/?worktype=adnativecomic&id=2",
            200,
            "application/json",
            json!({"id": "2", "title": "Empty"}).to_string(),
        ));
        fixture.exchanges.push(get(
            "http://app.video.baidu.com/xqsingle/?worktype=adnativecomic&id=2",
            200,
            "application/json",
            json!({"id": "2", "total_num": 0, "videos": []}).to_string(),
        ));
        fixture.exchanges.push(get(
            "http://app.video.baidu.com/xqinfo/?worktype=adnativecomic&id=3",
            200,
            "application/json",
            json!({"id": "3", "title": "Broken"}).to_string(),
        ));
        fixture.exchanges.push(get(
            "http://app.video.baidu.com/xqsingle/?worktype=adnativecomic&id=3",
            200,
            "application/json",
            json!({"id": "3", "videos": [{"title": "no link"}]}).to_string(),
        ));
        let resolver = BaiduResolver::new(Http::replay(fixture));
        let gone = Url::parse("http://v.baidu.com/comic/1.htm").unwrap();
        assert!(matches!(
            resolver.resolve(&gone).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let empty = Url::parse("http://v.baidu.com/comic/2.htm").unwrap();
        let error = resolver.resolve(&empty).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("no episodes")),
            "{error}"
        );
        let broken = Url::parse("http://v.baidu.com/comic/3.htm").unwrap();
        assert!(matches!(
            resolver.resolve(&broken).await.unwrap_err(),
            ResolveError::Malformed { .. }
        ));
    }
}
