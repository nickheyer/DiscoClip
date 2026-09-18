//! BongaCams live rooms and listings. A room goes through the AMF endpoint the room page
//! calls for the room's video server, then the room's HLS playlist; a listing page (the
//! front page, a gender tab or a category) carries the rooms online in its state. The
//! site answers anything but a browser's TLS fingerprint with a bot check, so its pages
//! and endpoints are read as Chrome.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionSupport, clean_title, fetch_as_browser, hls, navigation_headers, status_error,
    util,
};
use crate::http::{BROWSER_UA, Http};

pub const PLATFORM: &str = "bongacams";

/// `bongacams.com`, `de.bongacams.net`, `bongacams2.com`…
static RE_HOST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[^/]+\.)?bongacams\d*\.(?:com|net)$").unwrap());
static RE_PATH: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/([^/?&#]+)/?$").unwrap());
/// Where a listing page keeps the rooms online: `"stateData":{…,"models":[…]}`.
static RE_STATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""stateData"\s*:\s*"#).unwrap());

/// A page on one of the site's mirrors: the front page, or one path segment that names a
/// room or a listing (a gender tab such as `female`, or a category).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub host: String,
    /// The path segment; `None` for the front page, which lists every room online.
    pub name: Option<String>,
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
    if path.is_empty() || path == "/" {
        return Some(Link { host, name: None });
    }
    let name = RE_PATH.captures(path)?[1].to_string();
    if matches!(
        name.as_str(),
        "login" | "members" | "signup" | "tools" | "api" | "static" | "cdn-cgi"
    ) {
        return None;
    }
    Some(Link {
        host,
        name: Some(name),
    })
}

/// The video server a room's data names, with the scheme the site leaves off.
fn video_server(room: &Value) -> Option<String> {
    let server = util::text(&room["localData"]["videoServerUrl"])?;
    let server = server.trim().trim_end_matches('/');
    if server.is_empty() {
        return None;
    }
    Some(if let Some(rest) = server.strip_prefix("//") {
        format!("https://{rest}")
    } else if server.contains("://") {
        server.to_string()
    } else {
        format!("https://{server}")
    })
}

/// Whether the page's tabs or categories link `/{name}`: the site answers an unknown
/// path with the front page's listing, so only a linked name is a listing of its own.
pub fn page_lists(html: &str, name: &str) -> bool {
    let needle = format!("\"url\":\"/{name}\"");
    html.replace("\\/", "/").contains(&needle)
}

/// The rooms a listing page carries in its state, with the display name of each.
pub fn listing_rooms(html: &str) -> Option<Vec<(String, Option<String>)>> {
    let start = RE_STATE.find(html)?.end();
    let rest = &html[start..];
    let end = util::balanced_js_end(rest)?;
    let state: Value = serde_json::from_str(&rest[..end]).ok()?;
    let models = state["models"].as_array()?;
    Some(
        models
            .iter()
            .filter_map(|model| {
                let username = util::text(&model["username"])?;
                let display = model["display_name"].as_str().and_then(clean_title);
                Some((username, display))
            })
            .collect(),
    )
}

pub struct BongacamsResolver {
    http: Http,
}

impl BongacamsResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// The room data the AMF endpoint returns for `getRoomData`.
    async fn room_data(&self, host: &str, room: &str, origin: &Url) -> Result<Value, ResolveError> {
        let api = Url::parse(&format!("https://{host}/tools/amf.php"))
            .map_err(|e| ResolveError::malformed(origin, e.to_string()))?;
        let response = self
            .http
            .post(api)
            .platform(PLATFORM)
            .impersonate()
            .header("x-requested-with", "XMLHttpRequest")
            .header("referer", &format!("https://{host}/{room}"))
            .form(&[
                ("method", "getRoomData"),
                ("args[]", room),
                ("args[]", "false"),
            ])
            .send()
            .await?;
        if let Some(error) = status_error(response.status, origin) {
            return Err(error);
        }
        let body = response.text(MAX_PAGE).await?;
        serde_json::from_str(&body)
            .map_err(|e| ResolveError::malformed(origin, format!("room data JSON: {e}")))
    }

    async fn resolve_room(
        &self,
        link: &Link,
        room: &str,
        data: &Value,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let server = video_server(data)
            .ok_or_else(|| ResolveError::malformed(url, "the room data names no video server"))?;
        let performer = &data["performerData"];
        let username = util::text(&performer["username"]).unwrap_or_else(|| room.to_string());
        let display_name = performer["displayName"].as_str().and_then(clean_title);
        let playlist = util::join_url(
            None,
            &format!("{server}/hls/stream_{username}/playlist.m3u8"),
        )
        .ok_or_else(|| ResolveError::malformed(url, format!("bad video server {server}")))?;
        let mut expanded = match hls::expand(&self.http, &playlist, PLATFORM, BROWSER_UA, &[]).await
        {
            Ok(expanded) => expanded,
            Err(ResolveError::NotFound(_)) => {
                return Err(ResolveError::unavailable(
                    url,
                    format!(
                        "{} is not streaming right now",
                        display_name.as_deref().unwrap_or(&username)
                    ),
                ));
            }
            Err(error) => return Err(error),
        };
        for variant in &mut expanded.variants {
            variant.live = true;
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(room.to_string());
        resolved.title = display_name.clone().or_else(|| clean_title(&username));
        resolved.uploader = display_name.or_else(|| clean_title(&username));
        resolved.uploader_url = Url::parse(&format!("https://{}/{username}", link.host)).ok();
        resolved.webpage_url = Url::parse(&format!("https://{}/{room}", link.host)).ok();
        resolved.age_limit = Some(18);
        resolved.live = true;
        resolved.variants = expanded.variants;
        Ok(Resolution::from(resolved))
    }

    /// The rooms online that the listing at `path` carries, as a playlist.
    async fn resolve_listing(&self, link: &Link, url: &Url) -> Result<Resolution, ResolveError> {
        let path = link
            .name
            .as_deref()
            .map_or(String::new(), |name| format!("{name}/"));
        let page_url = Url::parse(&format!("https://{}/{path}", link.host))
            .map_err(|e| ResolveError::malformed(url, e.to_string()))?;
        let fetched = fetch_as_browser(
            &self.http,
            &page_url,
            PLATFORM,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        let html = fetched.text();
        if link
            .name
            .as_deref()
            .is_some_and(|name| !page_lists(&html, name))
        {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let rooms = listing_rooms(&html).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let title = Page::parse(&html, url)
            .title()
            .and_then(|t| clean_title(&t));
        let entries = rooms
            .into_iter()
            .filter_map(|(username, display)| {
                Some(PlaylistEntry {
                    url: Url::parse(&format!("https://{}/{username}", link.host)).ok()?,
                    title: display.or_else(|| clean_title(&username)),
                    duration: None,
                })
            })
            .collect::<Vec<_>>();
        let total = entries.len();
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(link.name.clone().unwrap_or_else(|| "all".to_string())),
            title,
            entries,
            total: Some(total),
        }))
    }
}

#[async_trait]
impl Resolver for BongacamsResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "BongaCams",
            hosts: &["bongacams.com", "bongacams.net"],
            features: &["live", "listings"],
            formats: &["hls"],
            session: SessionSupport::None,
            examples: &["https://bongacams.com/", "https://bongacams.com/female"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let Some(name) = link.name.clone() else {
            return self.resolve_listing(&link, url).await;
        };
        // A name is a room on air when the AMF endpoint names its video server. The
        // endpoint answers success for any other name too, echoing it as a performer
        // that is not online, so the rest are listings, rooms off air, or nothing.
        let data = self.room_data(&link.host, &name, url).await?;
        let refused = data["status"].as_str() == Some("error");
        if !refused && video_server(&data).is_some() {
            return self.resolve_room(&link, &name, &data, url).await;
        }
        match self.resolve_listing(&link, url).await {
            Err(ResolveError::NotFound(_)) => {
                let performer = &data["performerData"];
                match util::text(&performer["username"]).filter(|_| !refused) {
                    Some(username) if performer["hasProfile"].as_bool() == Some(true) => {
                        let display_name = performer["displayName"].as_str().and_then(clean_title);
                        Err(ResolveError::unavailable(
                            url,
                            format!(
                                "{} is not streaming right now",
                                display_name.as_deref().unwrap_or(&username)
                            ),
                        ))
                    }
                    _ => Err(ResolveError::NotFound(url.clone())),
                }
            }
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};
    use serde_json::json;

    fn exchange(
        method: &str,
        url: &str,
        status: u16,
        content_type: &str,
        body: String,
    ) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: method.into(),
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

    const MASTER: &str = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2500000,RESOLUTION=1280x720\nhttps://ws.bcvcdn.test/hls/stream_ClaireAshton/720.m3u8\n";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2.0,\n0.ts\n";

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://de.bongacams.com/azumi-8"),
            Some(Link {
                host: "de.bongacams.com".into(),
                name: Some("azumi-8".into())
            })
        );
        assert_eq!(
            link("https://cn.bongacams.com/azumi-8"),
            Some(Link {
                host: "cn.bongacams.com".into(),
                name: Some("azumi-8".into())
            })
        );
        assert_eq!(
            link("https://de.bongacams.net/claireashton"),
            Some(Link {
                host: "de.bongacams.net".into(),
                name: Some("claireashton".into())
            })
        );
        assert_eq!(
            link("https://bongacams2.com/azumi-8?x=1"),
            Some(Link {
                host: "bongacams2.com".into(),
                name: Some("azumi-8".into())
            })
        );
        assert_eq!(
            link("https://bongacams.com/"),
            Some(Link {
                host: "bongacams.com".into(),
                name: None
            })
        );
        assert_eq!(
            link("https://bongacams.com/female/"),
            Some(Link {
                host: "bongacams.com".into(),
                name: Some("female".into())
            })
        );
        assert_eq!(link("https://bongacams.com/members/join"), None);
        assert_eq!(link("https://bongacams.com/login"), None);
        assert_eq!(link("https://bongacams.org/azumi-8"), None);
        assert_eq!(link("https://example.com/azumi-8"), None);
    }

    #[test]
    fn video_servers_get_their_scheme() {
        let server = |s: &str| video_server(&json!({"localData": {"videoServerUrl": s}}));
        assert_eq!(
            server("//live-edge74.bcvcdn.com").as_deref(),
            Some("https://live-edge74.bcvcdn.com")
        );
        assert_eq!(
            server("https://ws.bcvcdn.test/").as_deref(),
            Some("https://ws.bcvcdn.test")
        );
        assert_eq!(
            server("ws.bcvcdn.test").as_deref(),
            Some("https://ws.bcvcdn.test")
        );
        assert_eq!(server(""), None);
    }

    #[test]
    fn listings_are_read_from_the_page_state() {
        let html = r#"<html><script data-type="initialState" type="application/json">{"listing":{"thumbImgSizes":{"small":[160,120]},"stateData":{"status":"success","th_type":"live","total_count":2,"models":[{"gender":"female","username":"Taanni","display_name":"Taanni","esid":"live-edge15"},{"username":"EvaSterling","display_name":"Eva Sterling"},{"display_name":"nameless"}]},"more":true}}</script></html>"#;
        assert_eq!(
            listing_rooms(html),
            Some(vec![
                ("Taanni".into(), Some("Taanni".into())),
                ("EvaSterling".into(), Some("Eva Sterling".into())),
            ])
        );
        assert_eq!(listing_rooms("<html>no state</html>"), None);
        let tabs = r#"{"listingAvailableLiveTabs":[{"url":"\/female","liveTab":"female"}],"headerCategories":[{"url":"\/anal"}]}"#;
        assert!(page_lists(tabs, "female"));
        assert!(page_lists(tabs, "anal"));
        assert!(!page_lists(tabs, "claireashton"));
    }

    #[tokio::test]
    async fn live_rooms_resolve_to_hls() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange("POST", "https://de.bongacams.net/tools/amf.php", 200, "application/json", json!({
            "status": "success",
            "localData": {"videoServerUrl": "//ws.bcvcdn.test"},
            "performerData": {"username": "ClaireAshton", "displayName": "Claire Ashton", "loversCount": 12}
        }).to_string()));
        fixture.exchanges.push(exchange(
            "GET",
            "https://ws.bcvcdn.test/hls/stream_ClaireAshton/playlist.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MASTER.into(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://ws.bcvcdn.test/hls/stream_ClaireAshton/720.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MEDIA.into(),
        ));
        let resolver = BongacamsResolver::new(Http::replay(fixture));
        let url = Url::parse("https://de.bongacams.net/claireashton").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("claireashton"));
        assert_eq!(resolved.title.as_deref(), Some("Claire Ashton"));
        assert_eq!(resolved.uploader.as_deref(), Some("Claire Ashton"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://de.bongacams.net/ClaireAshton"
        );
        assert_eq!(resolved.age_limit, Some(18));
        assert!(resolved.live);
        assert_eq!(resolved.variants.len(), 1);
        assert!(resolved.variants[0].live);
        assert_eq!(resolved.variants[0].height, Some(720));
    }

    #[tokio::test]
    async fn listings_resolve_to_the_rooms_online() {
        let html = r#"<html><head><title>Free Live Sex Cams</title></head><script data-type="initialState" type="application/json">{"listingAvailableLiveTabs":[{"url":"\/","liveTab":"all"},{"url":"\/female","liveTab":"female"}]}</script><script data-type="initialState" type="application/json">{"stateData":{"models":[{"username":"Taanni","display_name":"Taanni"},{"username":"EvaSterling","display_name":"Eva Sterling"}]}}</script></html>"#;
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "GET",
            "https://bongacams.com/",
            200,
            "text/html",
            html.into(),
        ));
        fixture.exchanges.push(exchange(
            "POST",
            "https://bongacams.com/tools/amf.php",
            200,
            "application/json",
            json!({"status": "error", "errors": ["Room not found"]}).to_string(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://bongacams.com/female/",
            200,
            "text/html",
            html.into(),
        ));
        let resolver = BongacamsResolver::new(Http::replay(fixture));
        let front = Url::parse("https://bongacams.com/").unwrap();
        assert!(resolver.matches(&front));
        let Resolution::Playlist(playlist) = resolver.resolve(&front).await.unwrap() else {
            panic!("a listing");
        };
        assert_eq!(playlist.title.as_deref(), Some("Free Live Sex Cams"));
        assert_eq!(playlist.id.as_deref(), Some("all"));
        assert_eq!(playlist.total, Some(2));
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://bongacams.com/Taanni"
        );
        assert_eq!(playlist.entries[1].title.as_deref(), Some("Eva Sterling"));
        let tab = Url::parse("https://bongacams.com/female").unwrap();
        let Resolution::Playlist(playlist) = resolver.resolve(&tab).await.unwrap() else {
            panic!("a listing");
        };
        assert_eq!(playlist.id.as_deref(), Some("female"));
        assert_eq!(playlist.entries.len(), 2);
    }

    #[tokio::test]
    async fn offline_and_missing_rooms_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "POST",
            "https://bongacams.com/tools/amf.php",
            200,
            "application/json",
            json!({
                "localData": {"videoServerUrl": "https://ws.bcvcdn.test"},
                "performerData": {"username": "Sleeper"}
            })
            .to_string(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://ws.bcvcdn.test/hls/stream_Sleeper/playlist.m3u8",
            404,
            "text/plain",
            "".into(),
        ));
        fixture.exchanges.push(exchange(
            "POST",
            "https://bongacams.com/tools/amf.php",
            200,
            "application/json",
            json!({"status": "error", "errors": ["Room not found"]}).to_string(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://bongacams.com/nobody/",
            200,
            "text/html",
            "<html><body>No such page</body></html>".into(),
        ));
        fixture.exchanges.push(exchange(
            "POST",
            "https://bongacams.com/tools/amf.php",
            200,
            "application/json",
            json!({"status": "success", "localData": {"dataKey": "x"}, "performerData": {"username": "Dreamer", "displayName": "Dreamer", "isOnline": false, "hasProfile": true}}).to_string(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://bongacams.com/dreamer/",
            200,
            "text/html",
            "<html><body>A profile, no stream</body></html>".into(),
        ));
        fixture.exchanges.push(exchange(
            "POST",
            "https://bongacams.com/tools/amf.php",
            200,
            "application/json",
            json!({"status": "success", "localData": {"dataKey": "x"}, "performerData": {"username": "odd", "isOnline": false, "hasProfile": false}}).to_string(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://bongacams.com/odd/",
            200,
            "text/html",
            "<html><body>Nothing here</body></html>".into(),
        ));
        let resolver = BongacamsResolver::new(Http::replay(fixture));
        let resolve = |room: &str| {
            let url = Url::parse(&format!("https://bongacams.com/{room}")).unwrap();
            let resolver = &resolver;
            async move { resolver.resolve(&url).await.unwrap_err() }
        };
        let error = resolve("sleeper").await;
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "Sleeper is not streaming right now"),
            "{error}"
        );
        assert!(matches!(resolve("nobody").await, ResolveError::NotFound(_)));
        let error = resolve("dreamer").await;
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "Dreamer is not streaming right now"),
            "{error}"
        );
        assert!(matches!(resolve("odd").await, ResolveError::NotFound(_)));
    }
}
