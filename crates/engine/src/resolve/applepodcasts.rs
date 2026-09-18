//! Apple Podcasts episodes: the episode page carries the episode's stream link in its
//! serialized server data; when a page comes without it, the catalogue API answers
//! with the token the page's script bundle carries.

use std::sync::LazyLock;

use async_trait::async_trait;
use base64::Engine as _;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Resolution, ResolveError, Resolved, Resolver, SessionSupport, Tag,
    Variant, clean_title, fetch, navigation_headers, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind};

pub const PLATFORM: &str = "applepodcasts";
const SITE: &str = "https://podcasts.apple.com";
const CATALOG_API: &str = "https://amp-api.podcasts.apple.com/v1/catalog/";
/// The key id of the token the site's script bundle carries.
const JWT_KEY_ID: &str = "C4J7GBP74H";

/// `/{country}/podcast/{name}/id{show}?i={episode}` and its shorter forms.
static RE_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(?:([a-z]{2})/)?podcast(?:/[^/?#]+){1,2}/?$").unwrap());
static RE_SCRIPT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"<script [^>]*\bsrc="(/assets/index~[0-9a-f]+\.js)""#).unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub country: String,
    pub episode: String,
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    if !url.host_str()?.eq_ignore_ascii_case("podcasts.apple.com") {
        return None;
    }
    let caps = RE_PATH.captures(url.path())?;
    let episode = util::query_param(url, "i")
        .filter(|i| !i.is_empty() && i.chars().all(|c| c.is_ascii_digit()))?;
    Some(Link {
        country: caps.get(1).map_or("us", |m| m.as_str()).to_string(),
        episode,
    })
}

/// The episode model the page's server data carries: the share button's lockup.
pub fn page_model(html: &str) -> Option<Value> {
    let json = util::element_by_id(html, "serialized-server-data")?;
    let data: Value = serde_json::from_str(json.trim()).ok()?;
    let items = data["data"][0]["data"]["headerButtonItems"].as_array()?;
    items
        .iter()
        .find(|item| {
            item["$kind"].as_str() == Some("share")
                && item["modelType"].as_str() == Some("EpisodeLockup")
        })
        .map(|item| item["model"].clone())
}

/// The header of the token the bundle carries, which the token is found by.
fn token_header() -> String {
    let header = format!(r#"{{"typ":"JWT","alg":"ES256","kid":"{JWT_KEY_ID}"}}"#);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(header.as_bytes())
}

/// The token in a script bundle: the header followed by two more segments.
pub fn token_in(script: &str) -> Option<String> {
    let header = regex::escape(&token_header());
    let re = Regex::new(&format!(r#"["']({header}(?:\.[\w-]+){{2}})["']"#)).ok()?;
    util::search(&re, script)
}

/// When a token stops being accepted: the `exp` claim of its payload.
pub fn token_expiry(token: &str) -> Option<i64> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims["exp"].as_i64()
}

/// Whether a token has expired, or expires within two minutes.
pub fn token_expired(token: &str, now: i64) -> bool {
    token_expiry(token).is_some_and(|exp| exp - now < 120)
}

fn audio_variant(url: Url) -> Variant {
    let mut variant = Variant::file(url);
    variant.audio_only = true;
    let extension = super::path_extension(&variant.url).unwrap_or_default();
    variant.container = Some(match extension.as_str() {
        "" => Container::Mp3,
        "mp4" => Container::M4a,
        ext => Container::from_extension(ext).unwrap_or_else(|| Container::Other(ext.to_string())),
    });
    variant.audio = Some(match extension.as_str() {
        "m4a" | "mp4" | "aac" => AudioCodec::Aac,
        _ => AudioCodec::Mp3,
    });
    variant.format_id = Some("audio".to_string());
    variant
}

pub struct ApplePodcastsResolver {
    http: Http,
}

impl ApplePodcastsResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// The episode as the catalogue API describes it, read with the bundle's token.
    async fn via_api(&self, html: &str, link: &Link, url: &Url) -> Result<Resolved, ResolveError> {
        let script_path = util::search(&RE_SCRIPT, html)
            .ok_or_else(|| ResolveError::malformed(url, "the page names no script bundle"))?;
        let script_url = Url::parse(&format!("{SITE}{script_path}"))
            .map_err(|e| ResolveError::malformed(url, e.to_string()))?;
        let script = fetch(&self.http, &script_url, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        if let Some(error) = status_error(script.status, url) {
            return Err(error);
        }
        let token = token_in(&script.text())
            .ok_or_else(|| ResolveError::malformed(url, "the script bundle carries no token"))?;
        if token_expired(&token, jiff::Timestamp::now().as_second()) {
            return Err(ResolveError::unavailable(
                url,
                "the site's script bundle carries a token that has already expired",
            ));
        }
        let api = util::with_query(
            &Url::parse(&format!(
                "{CATALOG_API}{}/podcast-episodes/{}",
                link.country, link.episode
            ))
            .expect("valid"),
            &[
                ("extend", "fullDescription"),
                ("include", "podcast"),
                ("l", "en-US"),
            ],
        );
        let response = self
            .http
            .get(api)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "application/json")
            .header("authorization", &format!("Bearer {token}"))
            .header("origin", SITE)
            .send()
            .await?;
        if let Some(error) = status_error(response.status, url) {
            return Err(error);
        }
        let answer: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(url, format!("catalogue JSON: {e}")))?;
        let episode = &answer["data"][0];
        let attributes = &episode["attributes"];
        let mut asset = util::url_of(&attributes["assetUrl"], None)
            .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        if asset.scheme() == "http" {
            asset
                .set_scheme("https")
                .map_err(|()| ResolveError::malformed(url, "the asset link has no host"))?;
        }
        let mut resolved = Resolved::of(PLATFORM, MediaKind::Audio);
        resolved.title = attributes["name"].as_str().and_then(clean_title);
        resolved.description = attributes["fullDescription"]
            .as_str()
            .or(attributes["description"]["standard"].as_str())
            .map(util::clean_html)
            .and_then(|d| clean_title(&d));
        resolved.uploaded_at = util::time(&attributes["releaseDateTime"]);
        resolved.duration = util::millis(&attributes["durationInMilliseconds"]);
        resolved.uploader = episode["relationships"]["podcast"]["data"][0]["attributes"]["name"]
            .as_str()
            .and_then(clean_title);
        resolved.thumbnail = attributes["artwork"]["url"].as_str().and_then(|template| {
            let width = util::uint(&attributes["artwork"]["width"]).unwrap_or(600);
            let height = util::uint(&attributes["artwork"]["height"]).unwrap_or(600);
            Url::parse(
                &template
                    .replace("{w}", &width.to_string())
                    .replace("{h}", &height.to_string())
                    .replace("{f}", "jpg"),
            )
            .ok()
        });
        resolved.variants = vec![audio_variant(util::clean_podcast_url(asset))];
        Ok(resolved)
    }
}

#[async_trait]
impl Resolver for ApplePodcastsResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Apple Podcasts",
            hosts: &["podcasts.apple.com"],
            features: &["audio", "podcasts"],
            formats: &["mp3", "m4a"],
            media: &[MediaKind::Audio],
            tags: &[Tag::Podcasts],
            session: SessionSupport::None,
            examples: &[
                "https://podcasts.apple.com/us/podcast/urbana-podcast-724-by-david-penn/id1531349107?i=1000748574256",
                "https://podcasts.apple.com/podcast/id1531349107?i=1000748574256",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        // The page is read as linked; the site answers HTTP 500 for some episodes and
        // still carries the script bundle the catalogue API is read with.
        let page_url = url.clone();
        let fetched = fetch(
            &self.http,
            &page_url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        if matches!(fetched.status.as_u16(), 404 | 410) {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let html = fetched.text();
        let (thumbnail, canonical) = {
            let page = Page::parse(&html, &page_url);
            (
                page.meta("og:image")
                    .and_then(|t| util::join_url(Some(&page_url), &t)),
                page.canonical(),
            )
        };
        let mut resolved = match page_model(&html) {
            Some(model) => {
                let stream = util::url_of(&model["playAction"]["episodeOffer"]["streamUrl"], None)
                    .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
                let mut resolved = Resolved::of(PLATFORM, MediaKind::Audio);
                resolved.title = model["title"].as_str().and_then(clean_title);
                resolved.description = model["summary"]
                    .as_str()
                    .map(util::clean_html)
                    .and_then(|d| clean_title(&d));
                resolved.uploaded_at = util::time(&model["releaseDate"]);
                resolved.duration = util::seconds(&model["duration"]);
                resolved.uploader = model["showTitle"].as_str().and_then(clean_title);
                resolved.variants = vec![audio_variant(util::clean_podcast_url(stream))];
                resolved
            }
            None => {
                if !fetched.status.is_success() && fetched.status.as_u16() != 500 {
                    return Err(status_error(fetched.status, url).expect("not a success"));
                }
                self.via_api(&html, &link, url).await?
            }
        };
        resolved.id = Some(link.episode.clone());
        resolved.thumbnail = resolved.thumbnail.take().or(thumbnail);
        resolved.webpage_url = canonical.or_else(|| Some(url.clone()));
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

    const EPISODE: &str = "https://podcasts.apple.com/us/podcast/urbana-podcast-724-by-david-penn/id1531349107?i=1000748574256";

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link(EPISODE),
            Some(Link {
                country: "us".into(),
                episode: "1000748574256".into()
            })
        );
        assert_eq!(
            link("https://podcasts.apple.com/podcast/207-whitney-webb-returns?i=1000482637777"),
            Some(Link {
                country: "us".into(),
                episode: "1000482637777".into()
            })
        );
        assert_eq!(
            link("https://podcasts.apple.com/gb/podcast/id1135137367?i=1000482637777")
                .map(|l| l.country),
            Some("gb".into())
        );
        assert_eq!(
            link("https://podcasts.apple.com/us/podcast/urbana-radio-show/id1531349107"),
            None
        );
        assert_eq!(link("https://music.apple.com/us/album/x/1?i=2"), None);
    }

    #[test]
    fn tokens_are_found_by_their_header() {
        let header = token_header();
        assert_eq!(
            header,
            "eyJ0eXAiOiJKV1QiLCJhbGciOiJFUzI1NiIsImtpZCI6IkM0SjdHQlA3NEgifQ"
        );
        let script =
            format!(r#"var x=1;const t="{header}.eyJpc3MiOiJBTVAifQ.sig_part-1";fetch(t)"#);
        assert_eq!(
            token_in(&script).as_deref(),
            Some(&*format!("{header}.eyJpc3MiOiJBTVAifQ.sig_part-1"))
        );
        assert_eq!(token_in("nothing here"), None);
        let expiring = format!("{header}.eyJpc3MiOiAiQU1QIiwgImV4cCI6IDQxMDI0NDQ4MDB9.sig");
        assert_eq!(token_expiry(&expiring), Some(4102444800));
        assert!(!token_expired(&expiring, 1_800_000_000));
        assert!(token_expired(&expiring, 4102444800 - 60));
        assert!(
            !token_expired(&format!("{header}.eyJpc3MiOiJBTVAifQ.sig"), 1_800_000_000),
            "a token without expiry is taken as valid"
        );
        assert_eq!(
            util::clean_podcast_url(Url::parse("https://dts.podtrac.com/redirect.m4a/www.music-zone.es/PodcastDP/UrbanaRS724.m4a").unwrap()).as_str(),
            "https://www.music-zone.es/PodcastDP/UrbanaRS724.m4a"
        );
        assert_eq!(
            util::clean_podcast_url(
                Url::parse(
                    "https://chtbl.com/track/ABC/pdst.fm/e/traffic.megaphone.fm/x.mp3?updated=1"
                )
                .unwrap()
            )
            .as_str(),
            "https://traffic.megaphone.fm/x.mp3?updated=1"
        );
    }

    fn server_data_page(model: &Value) -> String {
        let data = json!({"data": [{"data": {"headerButtonItems": [
            {"$kind": "play", "modelType": "EpisodeLockup"},
            {"$kind": "share", "modelType": "EpisodeLockup", "model": model}
        ]}}]});
        format!(
            r#"<html><head><meta property="og:image" content="https://is1-ssl.mzstatic.com/image/thumb/x/600x600bb.jpg"><link rel="canonical" href="{EPISODE}"></head><body><script id="serialized-server-data" type="application/json">{data}</script></body></html>"#
        )
    }

    #[tokio::test]
    async fn episodes_resolve_from_the_page_data() {
        let model = json!({"title": "URBANA PODCAST 724 BY DAVID PENN", "showTitle": "Urbana Radio Show", "releaseDate": "2026-02-06T18:00:01Z", "duration": 3602,
            "summary": "<p>Urbana Radio Show By David Penn Chapter #724</p>",
            "playAction": {"episodeOffer": {"streamUrl": "https://dts.podtrac.com/redirect.m4a/www.music-zone.es/PodcastDP/UrbanaRS724.m4a"}}});
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture
            .exchanges
            .push(get(EPISODE, 200, "text/html", server_data_page(&model)));
        let resolver = ApplePodcastsResolver::new(Http::replay(fixture));
        let url = Url::parse(EPISODE).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("1000748574256"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("URBANA PODCAST 724 BY DAVID PENN")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("Urbana Radio Show"));
        assert_eq!(
            resolved.description.as_deref(),
            Some("Urbana Radio Show By David Penn Chapter #724")
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(3602)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1770400801)
        );
        assert_eq!(resolved.webpage_url.as_ref().unwrap().as_str(), EPISODE);
        assert_eq!(resolved.variants.len(), 1);
        let audio = &resolved.variants[0];
        assert!(audio.audio_only);
        assert_eq!(resolved.media, MediaKind::Audio);
        assert_eq!(audio.container, Some(Container::M4a));
        assert_eq!(audio.audio, Some(AudioCodec::Aac));
        assert_eq!(
            audio.url.as_str(),
            "https://www.music-zone.es/PodcastDP/UrbanaRS724.m4a"
        );
    }

    #[tokio::test]
    async fn pages_without_data_go_through_the_catalogue_api() {
        let header = token_header();
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://podcasts.apple.com/podcast/id1135137367?i=1000482637777",
            500,
            "text/html",
            r#"<html><head><script src="/assets/index~8209afa97b.js"></script></head><body>error</body></html>"#.into(),
        ));
        fixture.exchanges.push(get(
            "https://podcasts.apple.com/assets/index~8209afa97b.js",
            200,
            "text/javascript",
            format!(
                r#"const token="{header}.eyJpc3MiOiAiQU1QIiwgImV4cCI6IDQxMDI0NDQ4MDB9.abc-def";"#
            ),
        ));
        fixture.exchanges.push(get(
            "https://amp-api.podcasts.apple.com/v1/catalog/us/podcast-episodes/1000482637777?extend=fullDescription&include=podcast&l=en-US",
            200,
            "application/json",
            json!({"data": [{"id": "1000482637777", "attributes": {
                "name": "207 - Whitney Webb Returns", "fullDescription": "Whitney is back.", "assetUrl": "http://traffic.libsyn.com/x/207.mp3?dest-id=1",
                "releaseDateTime": "2020-07-08T00:00:00Z", "durationInMilliseconds": 5322000,
                "artwork": {"url": "https://is1-ssl.mzstatic.com/image/thumb/x/{w}x{h}bb.{f}", "width": 3000, "height": 3000}
            }, "relationships": {"podcast": {"data": [{"attributes": {"name": "The Tim Dillon Show"}}]}}}]}).to_string(),
        ));
        let resolver = ApplePodcastsResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(
                &Url::parse("https://podcasts.apple.com/podcast/id1135137367?i=1000482637777")
                    .unwrap(),
            )
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(
            resolved.title.as_deref(),
            Some("207 - Whitney Webb Returns")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("The Tim Dillon Show"));
        assert_eq!(resolved.duration, Some(Duration::from_millis(5322000)));
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://is1-ssl.mzstatic.com/image/thumb/x/3000x3000bb.jpg"
        );
        assert_eq!(
            resolved.variants[0].url.as_str(),
            "https://traffic.libsyn.com/x/207.mp3?dest-id=1"
        );
        assert_eq!(resolved.variants[0].audio, Some(AudioCodec::Mp3));
    }
}
