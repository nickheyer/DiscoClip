//! 1News (TVNZ) article videos: the Brightcove and YouTube players an article's Fusion
//! content lists, one as a redirect to its player and several as a playlist.

use async_trait::async_trait;
use serde_json::Value;
use url::Url;

use super::page::json_after;
use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolver,
    SessionSupport, brightcove, clean_title, fetch_ok, navigation_headers,
};
use crate::http::{BROWSER_UA, Http};

pub const PLATFORM: &str = "1news";
/// The Brightcove account and player 1News articles embed.
const ACCOUNT: &str = "963482464001";
const PLAYER: &str = "0xpHIR6IB";
/// The assignment the article page renders its Fusion content into.
/// The Fusion content blob a page carries: `Fusion.globalContent=` followed by JSON, with
/// any spacing around the `=`.
fn fusion_content(html: &str) -> Option<Value> {
    let start = html.find("Fusion.globalContent")?;
    let rest = &html[start + "Fusion.globalContent".len()..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('=')?;
    json_after(rest, "")
}

/// The slug of an article link: `/<year>/<month>/<day>/<slug>/`.
pub fn parse_link(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !matches!(
        host.as_str(),
        "1news.co.nz" | "www.1news.co.nz" | "onenews.co.nz" | "www.onenews.co.nz"
    ) {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    match segments.as_slice() {
        [year, month, day, slug, ..]
            if [year, month, day]
                .iter()
                .all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())) =>
        {
            Some(slug.to_string())
        }
        _ => None,
    }
}

/// The players an article's content elements name, in order: Brightcove videos through
/// the site's player, and YouTube embeds.
pub fn players_of(content: &Value) -> Vec<PlaylistEntry> {
    let mut entries = Vec::new();
    for item in content["content_elements"].as_array().into_iter().flatten() {
        match item["subtype"].as_str() {
            Some("video") => {
                let config = &item["embed"]["config"];
                let Some(video) = config["brightcoveVideoId"]
                    .as_str()
                    .filter(|v| !v.trim().is_empty())
                else {
                    continue;
                };
                let account = config["brightcoveAccount"]
                    .as_str()
                    .filter(|a| !a.trim().is_empty())
                    .unwrap_or(ACCOUNT);
                entries.push(PlaylistEntry {
                    url: brightcove::embed_url(account, PLAYER, video),
                    title: item["headlines"]["basic"].as_str().and_then(clean_title),
                    duration: None,
                });
            }
            Some("youtube") => {
                let Some(id_or_url) = item["referent"]["id"]
                    .as_str()
                    .or_else(|| item["raw_oembed"]["_id"].as_str())
                    .filter(|s| !s.trim().is_empty())
                else {
                    continue;
                };
                let Some(url) = Url::parse(id_or_url)
                    .ok()
                    .filter(|u| matches!(u.scheme(), "http" | "https"))
                    .or_else(|| {
                        Url::parse(&format!("https://www.youtube.com/watch?v={id_or_url}")).ok()
                    })
                else {
                    continue;
                };
                entries.push(PlaylistEntry {
                    url,
                    title: item["raw_oembed"]["title"].as_str().and_then(clean_title),
                    duration: None,
                });
            }
            _ => {}
        }
    }
    entries
}

pub struct OneNewsNzResolver {
    http: Http,
}

impl OneNewsNzResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for OneNewsNzResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "1News",
            hosts: &["1news.co.nz", "onenews.co.nz"],
            features: &["article videos"],
            formats: &["hls", "dash", "mp4"],
            session: SessionSupport::None,
            examples: &[
                "https://www.1news.co.nz/2022/09/29/cows-painted-green-on-parliament-lawn-in-climate-protest/",
                "https://www.1news.co.nz/2022/09/29/raw-videos-capture-hurricane-ians-fury-as-it-slams-florida/",
                "https://www.1news.co.nz/2022/09/30/now-is-the-time-to-care-about-womens-rugby/",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let slug = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
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
        let content = fusion_content(&html).ok_or_else(|| {
            ResolveError::malformed(url, "the article page carries no Fusion content")
        })?;
        let mut entries = players_of(&content);
        match entries.len() {
            0 => Err(ResolveError::NotFound(url.clone())),
            1 => {
                let entry = entries.remove(0);
                let Some(link) = brightcove::parse_link(&entry.url) else {
                    return Err(ResolveError::Redirect(entry.url));
                };
                let mut resolved = brightcove::media(&self.http, PLATFORM, &link, url).await?;
                resolved.title = content["headlines"]["basic"]
                    .as_str()
                    .and_then(clean_title)
                    .or(entry.title)
                    .or(resolved.title);
                resolved.description = content["description"]["basic"]
                    .as_str()
                    .or(content["subheadlines"]["basic"].as_str())
                    .and_then(clean_title)
                    .or(resolved.description);
                resolved.uploader = Some("1News".to_string());
                resolved.uploader_url = Some(Url::parse("https://www.1news.co.nz/").expect("valid"));
                resolved.uploaded_at = content["publish_date"]
                    .as_str()
                    .or(content["display_date"].as_str())
                    .and_then(|t| t.parse().ok())
                    .or(resolved.uploaded_at);
                resolved.webpage_url = Some(url.clone());
                Ok(Resolution::from(resolved))
            }
            count => Ok(Resolution::Playlist(Playlist {
                resolver: PLATFORM.to_string(),
                id: Some(slug),
                title: content["headlines"]["basic"]
                    .as_str()
                    .and_then(clean_title),
                entries,
                total: Some(count),
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::VariantKind;
    use crate::http::Fixture;

    const COWS: &str =
        "https://www.1news.co.nz/2022/09/29/cows-painted-green-on-parliament-lawn-in-climate-protest/";
    const HURRICANE: &str =
        "https://www.1news.co.nz/2022/09/29/raw-videos-capture-hurricane-ians-fury-as-it-slams-florida/";
    const RUGBY: &str =
        "https://www.1news.co.nz/2022/09/30/now-is-the-time-to-care-about-womens-rugby/";

    #[test]
    fn article_links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link(COWS).as_deref(),
            Some("cows-painted-green-on-parliament-lawn-in-climate-protest")
        );
        assert_eq!(
            link("https://www.onenews.co.nz/2022/09/29/cows-painted-green/").as_deref(),
            Some("cows-painted-green")
        );
        assert_eq!(link("https://www.1news.co.nz/2022/09/29/"), None);
        assert_eq!(link("https://www.1news.co.nz/sport/"), None);
        assert_eq!(link("https://www.1news.co.nz.evil.test/2022/09/29/slug/"), None);
    }

    #[tokio::test]
    async fn articles_resolve_their_video_hand_youtube_on_or_list_several() {
        let fixture = Fixture::parse(include_str!("onenewsnz_fixture.json")).unwrap();
        let resolver = OneNewsNzResolver::new(Http::replay(fixture));
        let cows = Url::parse(COWS).unwrap();
        assert!(resolver.matches(&cows));

        let resolved = resolver.resolve(&cows).await.unwrap().media().unwrap();
        assert_eq!(resolved.resolver, PLATFORM);
        assert_eq!(resolved.id.as_deref(), Some("6312993358112"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("'Cows' painted green on Parliament lawn in climate protest")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("1News"));
        assert_eq!(resolved.webpage_url.as_ref(), Some(&cows));
        assert!(resolved.uploaded_at.is_some());
        assert!(resolved.duration.is_some());
        assert!(resolved.variants.iter().any(|v| v.kind == VariantKind::File && v.height.is_some()));
        assert!(resolved.variants.iter().any(|v| v.kind == VariantKind::Hls && v.height.is_some()));
        assert!(resolved.variants.iter().any(|v| v.kind == VariantKind::Dash && v.height.is_some()));

        let error = resolver.resolve(&Url::parse(RUGBY).unwrap()).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Redirect(to) if to.as_str() == "https://www.youtube.com/watch?v=s4wEB9neTfU"),
            "{error}"
        );

        let Resolution::Playlist(playlist) =
            resolver.resolve(&Url::parse(HURRICANE).unwrap()).await.unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(
            playlist.id.as_deref(),
            Some("raw-videos-capture-hurricane-ians-fury-as-it-slams-florida")
        );
        assert_eq!(
            playlist.title.as_deref(),
            Some("Raw videos capture Hurricane Ian's fury as it slams Florida")
        );
        assert_eq!(playlist.total, Some(2));
        assert_eq!(
            playlist
                .entries
                .iter()
                .map(|e| e.url.query().unwrap())
                .collect::<Vec<_>>(),
            vec!["videoId=6312991414112", "videoId=6312990044112"]
        );
    }

    #[test]
    fn youtube_ids_and_links_both_become_watch_links() {
        let content = serde_json::json!({"content_elements": [
            {"subtype": "youtube", "referent": {"id": "s4wEB9neTfU"}, "raw_oembed": {"title": "By id"}},
            {"subtype": "youtube", "referent": {}, "raw_oembed": {"_id": "https://youtu.be/abcdefghijk", "title": "By link"}},
            {"subtype": "video", "embed": {"config": {"brightcoveVideoId": "1", "brightcoveAccount": ""}}},
            {"subtype": "video", "embed": {"config": {}}},
            {"subtype": "image"}
        ]});
        let entries = players_of(&content);
        assert_eq!(
            entries.iter().map(|e| e.url.as_str()).collect::<Vec<_>>(),
            vec![
                "https://www.youtube.com/watch?v=s4wEB9neTfU",
                "https://youtu.be/abcdefghijk",
                "https://players.brightcove.net/963482464001/0xpHIR6IB_default/index.html?videoId=1",
            ]
        );
        assert_eq!(entries[0].title.as_deref(), Some("By id"));
    }
}
