//! Discovery's network sites, read through the Discovery API (`disco-api`) their players
//! call: dplay and discovery+ in Europe, discovery+ in India, the US channels' watch sites
//! (Discovery, TLC, HGTV, Food Network, Travel Channel and their siblings) and the German
//! TLC, DMAX and HGTV sites. An episode is its record plus the manifests its playback
//! answer names; a discovery+ Italy or India show page is a playlist of every episode of
//! every season.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};

use async_trait::async_trait;
use jiff::Timestamp;
use regex::Regex;
use serde_json::{Value, json};
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionCheck, SessionSupport, SubtitleTrack, Variant, clean_title, dash, geo, hls,
    path_extension, status_error, util,
};
use crate::http::{BROWSER_UA, Cookie, Http, HttpError, RequestBuilder, StatusCode};
use crate::media::Container;

pub const PLATFORM: &str = "dplay";

/// The version the discovery+ web player reports in `x-disco-client`.
pub const WEB_CLIENT_VERSION: &str = "27.43.0";
/// The most episodes a show playlist lists.
const MAX_ENTRIES: usize = 500;
/// How many episodes one page of a season lists.
const PAGE_SIZE: usize = 100;
/// The header a caller's country is faked with when the API refuses a region.
const FORWARDED_FOR: &str = "x-forwarded-for";

/// How a site's player identifies itself to the Discovery API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Client {
    /// The original dplay player: a bearer token alone, minted without a device id, and
    /// playback read from `playback/videoPlaybackInfo/{id}`.
    Legacy,
    /// A discovery+ player: `x-disco-params` and `x-disco-client` headers beside a token
    /// minted with a device id, and playback requested from `playback/v3/videoPlaybackInfo`.
    Plus {
        /// The `siteLookupKey` added to `x-disco-params`, when the player adds one.
        site_lookup_key: Option<String>,
        /// The `x-disco-client` value, such as `WEB:UNKNOWN:dsc:27.43.0`.
        client: String,
    },
}

impl Client {
    /// The discovery+ web player of `product` (`dsc`, `hgtv`, `food`...).
    pub fn web(product: &str) -> Self {
        Client::Plus {
            site_lookup_key: Some(product.to_string()),
            client: format!("WEB:UNKNOWN:{product}:{WEB_CLIENT_VERSION}"),
        }
    }

    /// The `Alps:HyogaPlayer` client of the German sites, which sends the realm alone.
    pub fn hyoga() -> Self {
        Client::Plus {
            site_lookup_key: None,
            client: "Alps:HyogaPlayer:0.0.0".to_string(),
        }
    }

    /// Whether the token is minted with a device id.
    fn needs_device_id(&self) -> bool {
        matches!(self, Client::Plus { .. })
    }

    /// The `x-disco-*` headers the player sends in `realm`.
    fn headers(&self, realm: &str) -> Vec<(String, String)> {
        match self {
            Client::Legacy => Vec::new(),
            Client::Plus {
                site_lookup_key,
                client,
            } => {
                let params = match site_lookup_key {
                    Some(key) => format!("realm={realm},siteLookupKey={key}"),
                    None => format!("realm={realm}"),
                };
                vec![
                    ("x-disco-params".to_string(), params),
                    ("x-disco-client".to_string(), client.clone()),
                ]
            }
        }
    }
}

/// One site's account with the Discovery API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Site {
    /// The API host, such as `us1-prod-direct.go.discovery.com`.
    pub disco_host: String,
    /// The realm tokens are minted in, such as `go` or `dplayse`.
    pub realm: String,
    /// The country the catalogue is licensed for (ISO 3166-1 alpha-2).
    pub country: String,
    pub client: Client,
    /// The referer the media requests carry, when the site's CDN wants one.
    pub referer: Option<String>,
}

impl Site {
    pub fn new(disco_host: &str, realm: &str, country: &str, client: Client) -> Self {
        Self {
            disco_host: disco_host.to_string(),
            realm: realm.to_string(),
            country: country.to_string(),
            client,
            referer: None,
        }
    }

    pub fn with_referer(mut self, referer: &str) -> Self {
        self.referer = Some(referer.to_string());
        self
    }

    /// `https://{disco_host}/`, the root every endpoint hangs off.
    pub fn base(&self) -> Option<Url> {
        Url::parse(&format!("https://{}/", self.disco_host)).ok()
    }
}

/// The site an episode link belongs to, named after the yt-dlp class that reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Network {
    /// dplay and discovery+ in Denmark, Finland, Japan, Sweden, Norway, Spain and Italy:
    /// `domain` is the site's host without `www.`, `country` its two-letter code.
    DPlay {
        domain: String,
        country: String,
    },
    HgtvDe,
    GoDiscovery,
    TravelChannel,
    CookingChannel,
    HgtvUsa,
    FoodNetwork,
    DestinationAmerica,
    InvestigationDiscovery,
    AmHistoryChannel,
    ScienceChannel,
    DiscoveryLife,
    AnimalPlanet,
    Tlc,
    /// discoveryplus.com, `country` from the link's path (`us` when it names none).
    DiscoveryPlus {
        country: String,
    },
    DiscoveryPlusIndia,
    /// tlc.de or dmax.de: `domain` is the site's host.
    DiscoveryNetworksDe {
        domain: String,
    },
    DiscoveryPlusItaly,
}

impl Network {
    /// The API the site's player calls.
    pub fn site(&self) -> Site {
        let web_us = |host: &str, product: &str| Site::new(host, "go", "us", Client::web(product));
        match self {
            Network::DPlay { domain, country } => {
                let (disco_host, referer) = if domain.starts_with('d') {
                    (
                        format!("disco-api.{domain}"),
                        format!("https://www.{domain}/"),
                    )
                } else {
                    (
                        "eu2-prod.disco-api.com".to_string(),
                        format!("https://{domain}/"),
                    )
                };
                Site::new(
                    &disco_host,
                    &format!("dplay{country}"),
                    country,
                    Client::Legacy,
                )
                .with_referer(&referer)
            }
            Network::HgtvDe => Site::new("eu1-prod.disco-api.com", "hgtv", "de", Client::hyoga()),
            Network::GoDiscovery => web_us("us1-prod-direct.go.discovery.com", "dsc"),
            Network::TravelChannel => web_us("us1-prod-direct.watch.travelchannel.com", "trav"),
            Network::CookingChannel => web_us("us1-prod-direct.watch.cookingchanneltv.com", "cook"),
            Network::HgtvUsa => web_us("us1-prod-direct.watch.hgtv.com", "hgtv"),
            Network::FoodNetwork => web_us("us1-prod-direct.watch.foodnetwork.com", "food"),
            Network::DestinationAmerica => web_us("us1-prod-direct.destinationamerica.com", "dam"),
            Network::InvestigationDiscovery => {
                web_us("us1-prod-direct.investigationdiscovery.com", "ids")
            }
            Network::AmHistoryChannel => web_us("us1-prod-direct.ahctv.com", "ahc"),
            Network::ScienceChannel => web_us("us1-prod-direct.sciencechannel.com", "sci"),
            Network::DiscoveryLife => web_us("us1-prod-direct.discoverylife.com", "dlf"),
            Network::AnimalPlanet => web_us("us1-prod-direct.animalplanet.com", "apl"),
            Network::Tlc => web_us("us1-prod-direct.tlc.com", "tlc"),
            Network::DiscoveryPlus { country } => {
                let client = Client::Plus {
                    site_lookup_key: Some(format!("dplus_{country}")),
                    client: format!("WEB:UNKNOWN:dplus_us:{WEB_CLIENT_VERSION}"),
                };
                if matches!(country.as_str(), "br" | "ca" | "us") {
                    Site::new("us1-prod-direct.discoveryplus.com", "go", country, client)
                } else {
                    Site::new(
                        "eu1-prod-direct.discoveryplus.com",
                        "dplay",
                        country,
                        client,
                    )
                }
            }
            Network::DiscoveryPlusIndia => Site::new(
                "ap2-prod-direct.discoveryplus.in",
                "dplusindia",
                "in",
                Client::Plus {
                    site_lookup_key: None,
                    client: "WEB:UNKNOWN:dplus-india:17.0.0".to_string(),
                },
            )
            .with_referer("https://www.discoveryplus.in/"),
            Network::DiscoveryNetworksDe { domain } => Site::new(
                "eu1-prod.disco-api.com",
                &domain.replace('.', ""),
                "de",
                Client::hyoga(),
            ),
            Network::DiscoveryPlusItaly => Site::new(
                "eu1-prod-direct.discoveryplus.com",
                "dplay",
                "it",
                Client::Plus {
                    site_lookup_key: Some("dplus_it".to_string()),
                    client: format!("WEB:UNKNOWN:dplus_us:{WEB_CLIENT_VERSION}"),
                },
            ),
        }
    }
}

/// A discovery+ catalogue whose show pages list every episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Catalogue {
    Italy,
    India,
}

impl Catalogue {
    /// The API root.
    pub fn base_api(self) -> &'static str {
        match self {
            Catalogue::Italy => "https://disco-api.discoveryplus.it/",
            Catalogue::India => "https://ap2-prod-direct.discoveryplus.in/",
        }
    }

    /// The site, sent as referer and prefixed to every episode's path.
    pub fn domain(self) -> &'static str {
        match self {
            Catalogue::Italy => "https://www.discoveryplus.it/",
            Catalogue::India => "https://www.discoveryplus.in/",
        }
    }

    /// The realm tokens are minted in.
    pub fn realm(self) -> &'static str {
        match self {
            Catalogue::Italy => "dplayit",
            Catalogue::India => "dplusindia",
        }
    }

    fn client(self) -> &'static str {
        match self {
            Catalogue::Italy => "WEB:UNKNOWN:dplay-client:2.6.0",
            Catalogue::India => "WEB:UNKNOWN:dplus-india:prod",
        }
    }

    /// The route prefix show pages live under in the CMS.
    fn show_path(self) -> &'static str {
        match self {
            Catalogue::Italy => "programmi",
            Catalogue::India => "show",
        }
    }

    /// Which `included` entry of the show route carries the season picker.
    fn component_index(self) -> usize {
        match self {
            Catalogue::Italy => 1,
            Catalogue::India => 4,
        }
    }

    fn country(self) -> &'static str {
        match self {
            Catalogue::Italy => "it",
            Catalogue::India => "in",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// An episode page: the site and the `show/episode` path naming the episode.
    Video {
        network: Network,
        display_id: String,
    },
    /// An episode page on tlc.de or dmax.de, whose API id the sites' own CMS knows.
    GermanVideo {
        domain: String,
        programme: String,
        alternate_id: String,
    },
    /// A show page listing every episode.
    Show {
        catalogue: Catalogue,
        show_name: String,
    },
}

/// `/video/{show}/{episode}` on the US sites.
static RE_EPISODE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/video/([^/]+/[^/]+)").unwrap());
/// `/sendungen/{show}/{episode}` on de.hgtv.com.
static RE_HGTV_DE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/sendungen/([^/]+/[^/]+)").unwrap());
/// The dplay and discovery+ hosts of the Nordic, Spanish, Italian and Japanese sites.
static RE_DPLAY_HOST: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:www\.)?(dplay\.(dk|fi|jp|se|no)|discoveryplus\.(dk|es|fi|it|se|no))$").unwrap()
});
static RE_DPLAY_SUBDOMAIN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(es|it)\.dplay\.com$").unwrap());
/// `/{videos in the site's language}/{show}/{episode}`.
static RE_DPLAY_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/[^/]+/([^/]+/[^/]+)").unwrap());
/// `/programme|show|sendungen/{programme}/[video/]{episode}` on tlc.de and dmax.de.
static RE_GERMAN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^/(?:programme|show|sendungen)/([^/]+)/(?:video/)?([^/]+)").unwrap()
});
/// `/[{country}/]video[/sport|/olympics]/{show}/{episode}` on discoveryplus.com.
static RE_PLUS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^/(?:([a-z]{2})/)?video(?:/sport|/olympics)?/([^/]+/[^/]+)").unwrap()
});
static RE_PLUS_ITALY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/it/video(?:/sport|/olympics)?/([^/]+/[^/]+)").unwrap());
static RE_INDIA_VIDEO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/videos?/([^/]+/[^/]+)").unwrap());
static RE_INDIA_SHOW: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/show/([^/]+)/?$").unwrap());
static RE_ITALY_SHOW: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/programmi/([^/]+)/?$").unwrap());

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let path = url.path();
    let episode = |re: &Regex| re.captures(path).map(|caps| caps[1].to_string());
    let video = |network: Network, re: &Regex| {
        episode(re).map(|display_id| Link::Video {
            network,
            display_id,
        })
    };
    let is = |prefix: &str, domain: &str| host == domain || host == format!("{prefix}.{domain}");

    if host == "de.hgtv.com" {
        return video(Network::HgtvDe, &RE_HGTV_DE);
    }
    if is("watch", "hgtv.com") {
        return video(Network::HgtvUsa, &RE_EPISODE);
    }
    if is("go", "discovery.com") {
        return video(Network::GoDiscovery, &RE_EPISODE);
    }
    if is("watch", "travelchannel.com") {
        return video(Network::TravelChannel, &RE_EPISODE);
    }
    if is("watch", "cookingchanneltv.com") {
        return video(Network::CookingChannel, &RE_EPISODE);
    }
    if is("watch", "foodnetwork.com") {
        return video(Network::FoodNetwork, &RE_EPISODE);
    }
    if is("www", "destinationamerica.com") {
        return video(Network::DestinationAmerica, &RE_EPISODE);
    }
    if is("www", "investigationdiscovery.com") {
        return video(Network::InvestigationDiscovery, &RE_EPISODE);
    }
    if is("www", "ahctv.com") {
        return video(Network::AmHistoryChannel, &RE_EPISODE);
    }
    if is("www", "sciencechannel.com") {
        return video(Network::ScienceChannel, &RE_EPISODE);
    }
    if is("www", "discoverylife.com") {
        return video(Network::DiscoveryLife, &RE_EPISODE);
    }
    if is("www", "animalplanet.com") {
        return video(Network::AnimalPlanet, &RE_EPISODE);
    }
    if is("go", "tlc.com") {
        return video(Network::Tlc, &RE_EPISODE);
    }
    if is("www", "discoveryplus.com") {
        if let Some(display_id) = episode(&RE_PLUS_ITALY) {
            return Some(Link::Video {
                network: Network::DiscoveryPlusItaly,
                display_id,
            });
        }
        let caps = RE_PLUS.captures(path)?;
        let country = caps.get(1).map_or("us", |m| m.as_str());
        if country == "it" {
            return None;
        }
        return Some(Link::Video {
            network: Network::DiscoveryPlus {
                country: country.to_string(),
            },
            display_id: caps[2].to_string(),
        });
    }
    if is("www", "discoveryplus.in") {
        if let Some(display_id) = episode(&RE_INDIA_VIDEO) {
            return Some(Link::Video {
                network: Network::DiscoveryPlusIndia,
                display_id,
            });
        }
        return RE_INDIA_SHOW.captures(path).map(|caps| Link::Show {
            catalogue: Catalogue::India,
            show_name: caps[1].to_string(),
        });
    }
    if is("www", "discoveryplus.it") {
        if let Some(caps) = RE_ITALY_SHOW.captures(path) {
            return Some(Link::Show {
                catalogue: Catalogue::Italy,
                show_name: caps[1].to_string(),
            });
        }
    }
    if is("www", "tlc.de") || is("www", "dmax.de") {
        let caps = RE_GERMAN.captures(path)?;
        return Some(Link::GermanVideo {
            domain: host.strip_prefix("www.").unwrap_or(&host).to_string(),
            programme: caps[1].to_string(),
            alternate_id: caps[2].to_string(),
        });
    }
    let (domain, country) = if let Some(caps) = RE_DPLAY_HOST.captures(&host) {
        let country = caps.get(2).or_else(|| caps.get(3))?.as_str();
        (caps[1].to_string(), country.to_string())
    } else if let Some(caps) = RE_DPLAY_SUBDOMAIN.captures(&host) {
        (host.clone(), caps[1].to_string())
    } else {
        return None;
    };
    let display_id = episode(&RE_DPLAY_PATH)?;
    Some(Link::Video {
        network: Network::DPlay { domain, country },
        display_id,
    })
}

/// Every site the resolver reads, one per API host and realm.
pub fn sites() -> Vec<Site> {
    let mut sites = vec![
        Network::HgtvDe.site(),
        Network::GoDiscovery.site(),
        Network::TravelChannel.site(),
        Network::CookingChannel.site(),
        Network::HgtvUsa.site(),
        Network::FoodNetwork.site(),
        Network::DestinationAmerica.site(),
        Network::InvestigationDiscovery.site(),
        Network::AmHistoryChannel.site(),
        Network::ScienceChannel.site(),
        Network::DiscoveryLife.site(),
        Network::AnimalPlanet.site(),
        Network::Tlc.site(),
        Network::DiscoveryPlus {
            country: "us".to_string(),
        }
        .site(),
        Network::DiscoveryPlus {
            country: "gb".to_string(),
        }
        .site(),
        Network::DiscoveryPlusIndia.site(),
        Network::DiscoveryPlusItaly.site(),
        Network::DiscoveryNetworksDe {
            domain: "tlc.de".to_string(),
        }
        .site(),
        Network::DiscoveryNetworksDe {
            domain: "dmax.de".to_string(),
        }
        .site(),
    ];
    for (domain, country) in [
        ("dplay.dk", "dk"),
        ("dplay.fi", "fi"),
        ("dplay.jp", "jp"),
        ("dplay.se", "se"),
        ("dplay.no", "no"),
        ("discoveryplus.dk", "dk"),
        ("discoveryplus.es", "es"),
        ("discoveryplus.fi", "fi"),
        ("discoveryplus.it", "it"),
        ("discoveryplus.se", "se"),
        ("discoveryplus.no", "no"),
        ("es.dplay.com", "es"),
        ("it.dplay.com", "it"),
    ] {
        sites.push(
            Network::DPlay {
                domain: domain.to_string(),
                country: country.to_string(),
            }
            .site(),
        );
    }
    sites
}

/// One entry of a playback answer's `streaming`: the kind the API names (`hls`, `dash`...)
/// and the manifest or file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stream {
    pub kind: Option<String>,
    pub url: Url,
}

/// The streams a playback answer's `streaming` names: a list of `{type, url}` from the
/// v3 endpoint, a map of kind to `{url}` from the older one.
pub fn streams_of(streaming: &Value) -> Vec<Stream> {
    let link = |value: &Value| value.as_str().and_then(|u| Url::parse(u).ok());
    match streaming {
        Value::Object(map) => map
            .iter()
            .filter_map(|(kind, entry)| {
                Some(Stream {
                    kind: Some(kind.clone()),
                    url: link(&entry["url"])?,
                })
            })
            .collect(),
        Value::Array(list) => list
            .iter()
            .filter_map(|entry| {
                Some(Stream {
                    kind: util::text(&entry["type"]),
                    url: link(&entry["url"])?,
                })
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Where a bearer token came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenSource {
    /// The `st` cookie of the platform's jar: the user's own session.
    Cookie,
    /// An anonymous token minted by an earlier call.
    Cached,
    /// An anonymous token minted by this call.
    Minted,
}

struct Token {
    bearer: String,
    source: TokenSource,
}

/// Why an API call did not answer, beyond what the caller can pass on unchanged.
enum Failure {
    /// The API refused the caller's region.
    Geoblocked,
    /// The API rejected a token remembered from an earlier call.
    StaleToken,
    /// The API answered 404: the record is gone, or hidden from the caller's region, which
    /// the Indian catalogue reports the same way.
    Missing,
    Error(ResolveError),
}

impl From<ResolveError> for Failure {
    fn from(error: ResolveError) -> Self {
        Failure::Error(error)
    }
}

impl From<HttpError> for Failure {
    fn from(error: HttpError) -> Self {
        Failure::Error(ResolveError::Http(error))
    }
}

/// `path` under the API root `base`.
fn endpoint(base: &Url, path: &str, origin: &Url) -> Result<Url, ResolveError> {
    base.join(path)
        .map_err(|e| ResolveError::malformed(origin, format!("bad API path {path}: {e}")))
}

/// The Discovery API, called on behalf of one platform: requests go through that
/// platform's cookie jar, what is read is attributed to it, and the anonymous tokens
/// minted for it are remembered per API host and realm, as yt-dlp keeps them.
#[derive(Clone)]
pub struct DiscoApi {
    http: Http,
    platform: &'static str,
    tokens: Arc<Mutex<HashMap<(String, String), String>>>,
}

impl DiscoApi {
    pub fn new(http: Http, platform: &'static str) -> Self {
        Self {
            http,
            platform,
            tokens: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The user's `st` session cookie for the API at `base`, when the jar holds a live one.
    fn session_cookie(&self, base: &Url) -> Option<Cookie> {
        let now = Timestamp::now();
        self.http
            .jar(self.platform)
            .cookies()
            .iter()
            .find(|cookie| cookie.name == "st" && !cookie.is_expired(now) && cookie.matches(base))
            .cloned()
    }

    /// Forgets the anonymous token minted for `realm` at `base`, and the `st` cookie the
    /// token endpoint set it as, so the next call mints afresh; a session the jar held
    /// before (a login) stays.
    fn forget_token(&self, base: &Url, realm: &str) {
        let forgotten = self
            .tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&(base.to_string(), realm.to_string()));
        if let Some(token) = forgotten {
            self.drop_anonymous_cookie(base, &token);
        }
    }

    /// Removes the `st` cookie for `base` when it carries `token`, the anonymous token
    /// this resolver minted: the token endpoint hands the cookie's token back instead of
    /// minting a new one while the cookie is sent.
    fn drop_anonymous_cookie(&self, base: &Url, token: &str) {
        let Some(cookie) = self
            .session_cookie(base)
            .filter(|cookie| cookie.value == token)
        else {
            return;
        };
        self.http.with_jar(self.platform, |jar| {
            jar.remove(&cookie.name, &cookie.domain);
        });
    }

    /// Sends `request` as the platform and reads the JSON it answers; a refusal becomes
    /// the failure the API's error record explains.
    async fn call(
        &self,
        request: RequestBuilder,
        origin: &Url,
        token: TokenSource,
    ) -> Result<Value, Failure> {
        let response = request
            .platform(self.platform)
            .user_agent(BROWSER_UA)
            .send()
            .await?;
        let status = response.status;
        let body = response.bytes(MAX_PAGE).await?;
        if !status.is_success() {
            return Err(self.failure(status, &body, origin, token));
        }
        serde_json::from_slice(&body)
            .map_err(|e| Failure::Error(ResolveError::malformed(origin, format!("API JSON: {e}"))))
    }

    /// What a refused call means: the API's `errors[0]` names a geo block, a missing
    /// subscription or a bad token, else its `detail` says why.
    fn failure(
        &self,
        status: StatusCode,
        body: &[u8],
        origin: &Url,
        token: TokenSource,
    ) -> Failure {
        let error = serde_json::from_slice::<Value>(body)
            .map(|answer| answer["errors"][0].clone())
            .unwrap_or(Value::Null);
        // A `not.found` whose detail says the video "was filtered by validator" is the
        // catalogue withholding a video it has: reason 9 is the caller's region, as the
        // Indian catalogue answers a caller outside India; reason 1 is a video whose
        // availability window has closed, which is gone for everyone.
        let validator_reason = error["code"]
            .as_str()
            .filter(|code| *code == "not.found")
            .and(error["detail"].as_str())
            .filter(|detail| detail.contains("filtered by validator"))
            .and_then(|detail| detail.rsplit("reasonCode=").next())
            .and_then(|code| code.trim().parse::<u32>().ok());
        match validator_reason {
            Some(9) => return Failure::Geoblocked,
            Some(_) => return Failure::Missing,
            None => {}
        }
        match error["code"].as_str() {
            Some("access.denied.geoblocked") => return Failure::Geoblocked,
            Some("access.denied.missingpackage") => {
                return Failure::Error(ResolveError::login_required(
                    origin,
                    self.platform,
                    "the video is only available to subscribers",
                ));
            }
            Some("invalid.token") => {
                return match token {
                    TokenSource::Cached => Failure::StaleToken,
                    TokenSource::Cookie => Failure::Error(ResolveError::login_required(
                        origin,
                        self.platform,
                        "the stored session token was rejected; log in to the site again",
                    )),
                    TokenSource::Minted => Failure::Error(ResolveError::login_required(
                        origin,
                        self.platform,
                        "the video is only available to registered users",
                    )),
                };
            }
            _ => {}
        }
        if matches!(status.as_u16(), 404 | 410) {
            return Failure::Missing;
        }
        if let Some(detail) = error["detail"]
            .as_str()
            .map(str::trim)
            .filter(|detail| !detail.is_empty())
        {
            return Failure::Error(ResolveError::unavailable(origin, detail));
        }
        Failure::Error(
            status_error(status, origin)
                .unwrap_or_else(|| ResolveError::unavailable(origin, format!("HTTP {status}"))),
        )
    }

    /// The error a failure of a call for `country`'s catalogue comes down to.
    fn settle(&self, failure: Failure, origin: &Url, country: &str) -> ResolveError {
        match failure {
            Failure::Error(error) => error,
            Failure::Geoblocked => ResolveError::unavailable(
                origin,
                format!("available only in {}", country.to_ascii_uppercase()),
            ),
            Failure::StaleToken => ResolveError::login_required(
                origin,
                self.platform,
                "the API keeps rejecting the session token",
            ),
            Failure::Missing => ResolveError::NotFound(origin.clone()),
        }
    }

    /// The bearer token for `realm` at the API rooted at `base`: the user's `st` cookie
    /// when the jar holds one for that host, else the anonymous token remembered for the
    /// host and realm, else one minted at `token`. A token minted while faking the
    /// caller's country as `forwarded_for` is always fresh.
    async fn token(
        &self,
        base: &Url,
        realm: &str,
        needs_device_id: bool,
        origin: &Url,
        forwarded_for: Option<&str>,
    ) -> Result<Token, Failure> {
        let key = (base.to_string(), realm.to_string());
        let remembered = self
            .tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
            .cloned();
        if let Some(cookie) = self.session_cookie(base) {
            // The anonymous token the API set as the `st` cookie is the one being
            // retried with a faked address; only a session the jar held before (a
            // login) is kept through that retry. The cookie goes before the fresh mint,
            // or the endpoint hands the same token back.
            let minted_here = remembered.as_deref() == Some(cookie.value.as_str());
            if forwarded_for.is_none() || !minted_here {
                return Ok(Token {
                    bearer: format!("Bearer {}", cookie.value),
                    source: TokenSource::Cookie,
                });
            }
            self.drop_anonymous_cookie(base, &cookie.value);
        } else if forwarded_for.is_none()
            && let Some(token) = remembered
        {
            return Ok(Token {
                bearer: format!("Bearer {token}"),
                source: TokenSource::Cached,
            });
        }
        let device_id = uuid::Uuid::new_v4().simple().to_string();
        let mut query = vec![("realm", realm)];
        if needs_device_id {
            query.push(("deviceId", device_id.as_str()));
        }
        let url = util::with_query(&endpoint(base, "token", origin)?, &query);
        let mut request = self.http.get(url);
        if let Some(address) = forwarded_for {
            request = request.header(FORWARDED_FOR, address);
        }
        let answer = self.call(request, origin, TokenSource::Minted).await?;
        let token = answer["data"]["attributes"]["token"]
            .as_str()
            .map(str::trim)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| ResolveError::malformed(origin, "the token answer names no token"))?
            .to_string();
        // Remembered whether or not the endpoint set it as the `st` cookie too: for calls
        // without a cookie, and to tell the anonymous cookie from a login when a region
        // refusal is retried.
        self.tokens
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, token.clone());
        Ok(Token {
            bearer: format!("Bearer {token}"),
            source: TokenSource::Minted,
        })
    }

    /// `Bearer …` for `realm` at the API rooted at `base`, as the requests of resolvers
    /// built on this one send it: the user's `st` cookie when the platform's jar holds one
    /// for that host, else an anonymous token minted at `token` (with a fresh device id
    /// when `needs_device_id`) and remembered for later calls.
    pub async fn auth(
        &self,
        base: &Url,
        realm: &str,
        needs_device_id: bool,
        origin: &Url,
    ) -> Result<String, ResolveError> {
        match self.token(base, realm, needs_device_id, origin, None).await {
            Ok(token) => Ok(token.bearer),
            Err(failure) => Err(self.settle(failure, origin, "")),
        }
    }

    /// The episode `display_id` (its `show/episode` path, or its numeric id) of `site`,
    /// with every stream its playback answer names, as yt-dlp's `_get_disco_api_info`
    /// reads it. A region refusal, or a 404 (which the Indian catalogue answers callers
    /// outside India with), is retried once with a faked caller address in the site's
    /// country; a remembered token the API rejects is minted afresh once.
    pub async fn video(
        &self,
        url: &Url,
        display_id: &str,
        site: &Site,
    ) -> Result<Resolved, ResolveError> {
        let mut forwarded_for: Option<String> = None;
        let mut token_renewed = false;
        loop {
            match self
                .video_once(url, display_id, site, forwarded_for.as_deref())
                .await
            {
                Ok(resolved) => return Ok(resolved),
                Err(failure @ (Failure::Geoblocked | Failure::Missing))
                    if forwarded_for.is_none() =>
                {
                    match geo::random_ipv4(&site.country) {
                        Some(address) => forwarded_for = Some(address),
                        None => return Err(self.settle(failure, url, &site.country)),
                    }
                }
                Err(Failure::StaleToken) if !token_renewed => {
                    token_renewed = true;
                    if let Some(base) = site.base() {
                        self.forget_token(&base, &site.realm);
                    }
                }
                Err(failure) => return Err(self.settle(failure, url, &site.country)),
            }
        }
    }

    async fn video_once(
        &self,
        url: &Url,
        display_id: &str,
        site: &Site,
        forwarded_for: Option<&str>,
    ) -> Result<Resolved, Failure> {
        let base = site.base().ok_or_else(|| {
            ResolveError::malformed(url, format!("bad API host {}", site.disco_host))
        })?;
        let token = self
            .token(
                &base,
                &site.realm,
                site.client.needs_device_id(),
                url,
                forwarded_for,
            )
            .await?;
        let mut headers = vec![("referer".to_string(), url.to_string())];
        headers.extend(site.client.headers(&site.realm));
        headers.push(("authorization".to_string(), token.bearer.clone()));
        if let Some(address) = forwarded_for {
            headers.push((FORWARDED_FOR.to_string(), address.to_string()));
        }

        let record_url = util::with_query(
            &endpoint(&base, &format!("content/videos/{display_id}"), url)?,
            &[
                ("fields[channel]", "name"),
                ("fields[image]", "height,src,width"),
                ("fields[show]", "name"),
                ("fields[tag]", "name"),
                (
                    "fields[video]",
                    "description,episodeNumber,name,publishStart,seasonNumber,videoDuration",
                ),
                ("include", "images,primaryChannel,show,tags"),
            ],
        );
        let answer = self
            .call(
                self.http.get(record_url).headers(&headers),
                url,
                token.source,
            )
            .await?;
        let record = &answer["data"];
        let video_id = util::text(&record["id"])
            .ok_or_else(|| ResolveError::malformed(url, "the video record has no id"))?;
        let info = &record["attributes"];
        let title = info["name"]
            .as_str()
            .and_then(clean_title)
            .ok_or_else(|| ResolveError::malformed(url, "the video record has no name"))?;

        let playback = match &site.client {
            Client::Legacy => {
                let playback_url = endpoint(
                    &base,
                    &format!("playback/videoPlaybackInfo/{video_id}"),
                    url,
                )?;
                self.call(
                    self.http.get(playback_url).headers(&headers),
                    url,
                    token.source,
                )
                .await?
            }
            Client::Plus { .. } => {
                let playback_url = endpoint(&base, "playback/v3/videoPlaybackInfo", url)?;
                let body = json!({
                    "deviceInfo": {"adBlocker": false, "drmSupported": false},
                    "videoId": video_id,
                    "wisteriaProperties": {},
                });
                self.call(
                    self.http.post(playback_url).headers(&headers).json(&body),
                    url,
                    token.source,
                )
                .await?
            }
        };
        let streams = streams_of(&playback["data"]["attributes"]["streaming"]);
        if streams.is_empty() {
            return Err(
                ResolveError::malformed(url, "the playback answer names no streams").into(),
            );
        }

        let mut manifest_headers = Vec::new();
        if let Some(referer) = &site.referer {
            manifest_headers.push(("referer".to_string(), referer.clone()));
        }
        if let Some(address) = forwarded_for {
            manifest_headers.push((FORWARDED_FOR.to_string(), address.to_string()));
        }
        let mut variants = Vec::new();
        let mut subtitles = Vec::new();
        let mut manifest_duration = None;
        let mut live = false;
        let mut last_error = None;
        for stream in streams {
            let ext = path_extension(&stream.url).unwrap_or_default();
            if stream.kind.as_deref() == Some("dash") || ext == "mpd" {
                match dash::expand(
                    &self.http,
                    &stream.url,
                    self.platform,
                    BROWSER_UA,
                    &manifest_headers,
                )
                .await
                {
                    Ok(expanded) => {
                        variants.extend(expanded.variants);
                        subtitles.extend(expanded.subtitles);
                        manifest_duration = manifest_duration.or(expanded.duration);
                        live |= expanded.live;
                    }
                    Err(error) => last_error = Some(error),
                }
            } else if stream.kind.as_deref() == Some("hls") || ext == "m3u8" {
                match hls::expand(
                    &self.http,
                    &stream.url,
                    self.platform,
                    BROWSER_UA,
                    &manifest_headers,
                )
                .await
                {
                    Ok(expanded) => {
                        variants.extend(expanded.variants);
                        subtitles.extend(expanded.subtitles);
                        manifest_duration = manifest_duration.or(expanded.duration);
                        live |= expanded.live;
                    }
                    Err(error) => last_error = Some(error),
                }
            } else {
                let mut variant = Variant::file(stream.url);
                variant.container = Container::from_extension(&ext);
                variant.headers = manifest_headers.clone();
                variant.format_id = stream.kind;
                variants.push(variant);
            }
        }
        // The faked caller address is for the API and its manifests; the media is fetched
        // as the user is.
        strip_forwarded_for(&mut variants, &mut subtitles);
        if variants.is_empty() {
            return Err(last_error
                .unwrap_or_else(|| {
                    ResolveError::malformed(url, "no stream of the playback answer could be read")
                })
                .into());
        }

        let mut uploader = None;
        let mut thumbnail: Option<(u64, Url)> = None;
        for entry in answer["included"].as_array().into_iter().flatten() {
            let attributes = &entry["attributes"];
            match entry["type"].as_str() {
                Some("channel") => uploader = attributes["name"].as_str().and_then(clean_title),
                Some("image") => {
                    if let Some(src) = attributes["src"].as_str().and_then(|s| Url::parse(s).ok()) {
                        let area = util::uint(&attributes["width"]).unwrap_or(0)
                            * util::uint(&attributes["height"]).unwrap_or(0);
                        if thumbnail.as_ref().is_none_or(|(best, _)| area > *best) {
                            thumbnail = Some((area, src));
                        }
                    }
                }
                _ => {}
            }
        }

        let mut resolved = Resolved::new(self.platform);
        resolved.id = Some(video_id);
        resolved.title = Some(title);
        resolved.description = info["description"]
            .as_str()
            .map(str::trim)
            .filter(|description| !description.is_empty())
            .map(String::from);
        resolved.duration = util::millis(&info["videoDuration"])
            .filter(|duration| !duration.is_zero())
            .or(manifest_duration);
        resolved.uploaded_at = util::time(&info["publishStart"]);
        resolved.uploader = uploader;
        resolved.thumbnail = thumbnail.map(|(_, src)| src);
        resolved.webpage_url = Some(url.clone());
        resolved.live = live;
        resolved.subtitles = subtitles;
        resolved.variants = variants;
        if let Some(system) = resolved.drm() {
            return Err(ResolveError::drm(url, system).into());
        }
        Ok(resolved)
    }

    /// The Discovery API id of an episode on tlc.de or dmax.de, looked up in the sites'
    /// own CMS (`de-api.loma-cms.com`): the last seven characters of the record's `uid`.
    /// `None` when the CMS does not know it, in which case the page path names the
    /// episode to the API.
    pub async fn german_video_id(
        &self,
        domain: &str,
        programme: &str,
        alternate_id: &str,
    ) -> Option<String> {
        let environment = domain.split('.').next().unwrap_or(domain);
        let url = Url::parse(&format!(
            "https://de-api.loma-cms.com/feloma/videos/{alternate_id}/"
        ))
        .ok()?;
        let url = util::with_query(
            &url,
            &[
                ("environment", environment),
                ("v", "2"),
                ("filter[show.slug]", programme),
            ],
        );
        let response = self
            .http
            .get(url)
            .platform(self.platform)
            .user_agent(BROWSER_UA)
            .send()
            .await
            .ok()?;
        if !response.status.is_success() {
            return None;
        }
        let record: Value = response.json(MAX_PAGE).await.ok()?;
        let uid: Vec<char> = record["uid"].as_str()?.chars().collect();
        let tail: String = uid[uid.len().saturating_sub(7)..].iter().collect();
        (!tail.is_empty()).then_some(tail)
    }

    /// Every episode of every season of the show at `show_name` in `catalogue`, as links
    /// the catalogue's episode resolver reads, oldest season first and episodes in order.
    /// A region refusal or a 404 is retried once with a faked caller address in the
    /// catalogue's country, as [`Self::video`] does.
    pub async fn show(
        &self,
        url: &Url,
        catalogue: Catalogue,
        show_name: &str,
    ) -> Result<Playlist, ResolveError> {
        let mut forwarded_for: Option<String> = None;
        let mut token_renewed = false;
        loop {
            match self
                .show_once(url, catalogue, show_name, forwarded_for.as_deref())
                .await
            {
                Ok(playlist) => return Ok(playlist),
                Err(failure @ (Failure::Geoblocked | Failure::Missing))
                    if forwarded_for.is_none() =>
                {
                    match geo::random_ipv4(catalogue.country()) {
                        Some(address) => forwarded_for = Some(address),
                        None => return Err(self.settle(failure, url, catalogue.country())),
                    }
                }
                Err(Failure::StaleToken) if !token_renewed => {
                    token_renewed = true;
                    if let Ok(base) = Url::parse(catalogue.base_api()) {
                        self.forget_token(&base, catalogue.realm());
                    }
                }
                Err(failure) => return Err(self.settle(failure, url, catalogue.country())),
            }
        }
    }

    async fn show_once(
        &self,
        url: &Url,
        catalogue: Catalogue,
        show_name: &str,
        forwarded_for: Option<&str>,
    ) -> Result<Playlist, Failure> {
        let base = Url::parse(catalogue.base_api())
            .map_err(|e| ResolveError::malformed(url, format!("bad API root: {e}")))?;
        let token = self
            .token(&base, catalogue.realm(), true, url, forwarded_for)
            .await?;
        let mut headers = vec![
            ("x-disco-client".to_string(), catalogue.client().to_string()),
            (
                "x-disco-params".to_string(),
                format!("realm={}", catalogue.realm()),
            ),
            ("referer".to_string(), catalogue.domain().to_string()),
            ("authorization".to_string(), token.bearer.clone()),
        ];
        if let Some(address) = forwarded_for {
            headers.push((FORWARDED_FOR.to_string(), address.to_string()));
        }
        let route = util::with_query(
            &endpoint(
                &base,
                &format!("cms/routes/{}/{show_name}", catalogue.show_path()),
                url,
            )?,
            &[("include", "default")],
        );
        let page = self
            .call(self.http.get(route).headers(&headers), url, token.source)
            .await?;
        let included: Vec<&Value> = page["included"].as_array().into_iter().flatten().collect();
        let picker = |entry: &Value| -> bool {
            let component = &entry["attributes"]["component"];
            component["mandatoryParams"].is_string() && component["filters"].is_array()
        };
        let component = included
            .get(catalogue.component_index())
            .filter(|entry| picker(entry))
            .or_else(|| included.iter().find(|entry| picker(entry)))
            .map(|entry| &entry["attributes"]["component"])
            .ok_or_else(|| ResolveError::malformed(url, "the show page has no season picker"))?;
        let show_id = component["mandatoryParams"]
            .as_str()
            .and_then(|params| params.rsplit('=').next())
            .filter(|id| !id.is_empty())
            .ok_or_else(|| ResolveError::malformed(url, "the season picker names no show id"))?;
        let title = included
            .iter()
            .find(|entry| entry["type"] == "show")
            .and_then(|entry| entry["attributes"]["name"].as_str())
            .and_then(clean_title);
        let seasons: Vec<String> = component["filters"][0]["options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|option| util::text(&option["id"]))
            .collect();

        let mut entries = Vec::new();
        'seasons: for season in seasons {
            let mut total_pages = 1;
            let mut page_number = 1;
            while page_number <= total_pages {
                let listing = util::with_query(
                    &endpoint(&base, "content/videos", url)?,
                    &[
                        ("sort", "episodeNumber"),
                        ("filter[seasonNumber]", season.as_str()),
                        ("filter[show.id]", show_id),
                        ("page[size]", &PAGE_SIZE.to_string()),
                        ("page[number]", &page_number.to_string()),
                    ],
                );
                let season_page = self
                    .call(self.http.get(listing).headers(&headers), url, token.source)
                    .await?;
                if page_number == 1 {
                    total_pages = util::uint(&season_page["meta"]["totalPages"])
                        .filter(|pages| *pages > 0)
                        .unwrap_or(1);
                }
                for episode in season_page["data"].as_array().into_iter().flatten() {
                    let attributes = &episode["attributes"];
                    let Some(path) = attributes["path"].as_str().filter(|path| !path.is_empty())
                    else {
                        continue;
                    };
                    let Ok(entry_url) = Url::parse(&format!("{}videos/{path}", catalogue.domain()))
                    else {
                        continue;
                    };
                    entries.push(PlaylistEntry {
                        url: entry_url,
                        title: attributes["name"].as_str().and_then(clean_title),
                        duration: util::millis(&attributes["videoDuration"])
                            .filter(|duration| !duration.is_zero()),
                    });
                    if entries.len() >= MAX_ENTRIES {
                        break 'seasons;
                    }
                }
                page_number += 1;
            }
        }
        if entries.is_empty() {
            return Err(ResolveError::unavailable(url, "the show lists no episodes").into());
        }
        Ok(Playlist {
            resolver: self.platform.to_string(),
            id: Some(show_name.to_string()),
            title,
            entries,
            total: None,
        })
    }

    /// Whom the platform's `st` cookie logs in at `site`, asked of `users/me`: the
    /// account's user name (or id) when the cookie is a signed-in session, `None` when
    /// there is no cookie for the site or it is anonymous or rejected.
    pub async fn account(&self, site: &Site) -> Result<Option<String>, ResolveError> {
        let Some(base) = site.base() else {
            return Ok(None);
        };
        let Some(cookie) = self.session_cookie(&base) else {
            return Ok(None);
        };
        let mut headers = site.client.headers(&site.realm);
        headers.push((
            "authorization".to_string(),
            format!("Bearer {}", cookie.value),
        ));
        let me = endpoint(&base, "users/me", &base)?;
        let response = self
            .http
            .get(me)
            .platform(self.platform)
            .user_agent(BROWSER_UA)
            .headers(&headers)
            .send()
            .await?;
        if matches!(response.status.as_u16(), 401 | 403) {
            return Ok(None);
        }
        if let Some(error) = status_error(response.status, &base) {
            return Err(error);
        }
        let user: Value = response.json(MAX_PAGE).await?;
        let attributes = &user["data"]["attributes"];
        if attributes["anonymous"].as_bool() == Some(true) {
            return Ok(None);
        }
        let username = util::text(&attributes["username"]);
        if username.is_some() {
            return Ok(username);
        }
        if attributes["anonymous"].as_bool() == Some(false) {
            return Ok(util::text(&user["data"]["id"]));
        }
        Ok(None)
    }
}

fn strip_forwarded_for(variants: &mut [Variant], subtitles: &mut [SubtitleTrack]) {
    for variant in variants {
        variant.headers.retain(|(name, _)| name != FORWARDED_FOR);
    }
    for track in subtitles {
        track.headers.retain(|(name, _)| name != FORWARDED_FOR);
    }
}

pub struct DplayResolver {
    api: DiscoApi,
}

impl DplayResolver {
    pub fn new(http: Http) -> Self {
        Self {
            api: DiscoApi::new(http, PLATFORM),
        }
    }
}

#[async_trait]
impl Resolver for DplayResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Discovery networks",
            hosts: &[
                "discoveryplus.dk",
                "discoveryplus.es",
                "discoveryplus.fi",
                "discoveryplus.it",
                "discoveryplus.se",
                "discoveryplus.no",
                "discoveryplus.com",
                "discoveryplus.in",
                "discovery.com",
                "tlc.com",
                "tlc.de",
                "dmax.de",
                "hgtv.com",
                "foodnetwork.com",
                "cookingchanneltv.com",
                "travelchannel.com",
                "destinationamerica.com",
                "investigationdiscovery.com",
                "ahctv.com",
                "sciencechannel.com",
                "discoverylife.com",
                "animalplanet.com",
            ],
            features: &["videos", "shows"],
            formats: &["hls", "dash", "mp4"],
            session: SessionSupport::Optional,
            examples: &[
                "https://www.ahctv.com/video/blood-and-fury-americas-civil-war-ahc/battle-of-bull-run",
                "https://www.animalplanet.com/video/i-was-prey-animal-planet/20-foot-terror",
                "https://watch.cookingchanneltv.com/video/bobbys-triple-threat-food-network-atve-us/titans-vs-marcus-samuelsson",
                "https://www.destinationamerica.com/video/our-america-with-lisa-ling-own/3-am-girls",
                "https://www.discoverylife.com/video/er-files-discovery-life-atve-us/sweet-charity",
                "https://dmax.de/sendungen/die-hausmeister/festgekrallt",
                "https://tlc.de/sendungen/ghost-adventures/der-poltergeist-im-kostumladen",
                "https://www.discoveryplus.in/videos/how-do-they-do-it/fugu-and-more?seasonId=8&type=EPISODE",
                "https://www.discoveryplus.in/show/how-do-they-do-it",
                "https://watch.foodnetwork.com/video/halloween-wars-food-network-atve-us/back-from-the-dead-all-stars",
                "https://go.discovery.com/video/in-the-eye-of-the-storm-discovery-atve-us/trapped-in-a-twister",
                "https://de.hgtv.com/sendungen/mein-kleinstadt-traumhaus/vom-landleben-ins-loft",
                "https://watch.hgtv.com/video/flip-or-flop-the-final-flip-hgtv-atve-us/flip-or-flop-the-final-flip",
                "https://www.investigationdiscovery.com/video/deadly-influence-the-social-media-murders-investigation-discovery-atve-us/rip-bianca",
                "https://www.sciencechannel.com/video/spaces-deepest-secrets-science-atve-us/mystery-of-the-dead-planets",
                "https://go.tlc.com/video/er-caught-on-camera-tlc-atve-us/a-swifties-survivor-era",
                "https://watch.travelchannel.com/video/paranormal-caught-on-camera-travel-channel/williamsburg-skinwalker",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Video {
                network,
                display_id,
            } => Ok(Resolution::from(
                self.api.video(url, &display_id, &network.site()).await?,
            )),
            Link::GermanVideo {
                domain,
                programme,
                alternate_id,
            } => {
                let video_id = self
                    .api
                    .german_video_id(&domain, &programme, &alternate_id)
                    .await
                    .unwrap_or_else(|| format!("{programme}/{alternate_id}"));
                let site = Network::DiscoveryNetworksDe { domain }.site();
                Ok(Resolution::from(
                    self.api.video(url, &video_id, &site).await?,
                ))
            }
            Link::Show {
                catalogue,
                show_name,
            } => Ok(Resolution::Playlist(
                self.api.show(url, catalogue, &show_name).await?,
            )),
        }
    }

    /// Every `st` cookie in the jar is tried at the site whose API it is sent to.
    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        let jar = self.api.http.jar(PLATFORM);
        let now = Timestamp::now();
        let mut asked = HashSet::new();
        for cookie in jar
            .cookies()
            .iter()
            .filter(|cookie| cookie.name == "st" && !cookie.is_expired(now))
        {
            for site in sites() {
                let Some(base) = site.base() else {
                    continue;
                };
                if !cookie.matches(&base) || !asked.insert(site.disco_host.clone()) {
                    continue;
                }
                if let Some(account) = self.api.account(&site).await? {
                    return Ok(SessionCheck::LoggedIn { account });
                }
            }
        }
        Ok(SessionCheck::LoggedOut)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{ReplayTransport, Transport, TransportRequest, TransportResponse};
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};
    use crate::resolve::VariantKind;
    use serde_json::json;
    use std::time::Duration;

    /// A request as a resolver sent it, for asserting on headers.
    struct Sent {
        url: String,
        headers: Vec<(String, String)>,
    }

    impl Sent {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.as_str())
        }
    }

    /// Replays a fixture and keeps every request it was asked for.
    struct Watched {
        replay: ReplayTransport,
        sent: Mutex<Vec<Sent>>,
    }

    #[async_trait]
    impl Transport for Watched {
        async fn send(&self, request: TransportRequest) -> Result<TransportResponse, HttpError> {
            self.sent.lock().unwrap().push(Sent {
                url: request.url.to_string(),
                headers: request
                    .headers
                    .iter()
                    .map(|(name, value)| {
                        (
                            name.as_str().to_string(),
                            value.to_str().unwrap_or("").to_string(),
                        )
                    })
                    .collect(),
            });
            self.replay.send(request).await
        }

        fn name(&self) -> &'static str {
            "watched"
        }
    }

    fn watched(fixture: Fixture) -> (Http, Arc<Watched>) {
        let transport = Arc::new(Watched {
            replay: ReplayTransport::new(fixture),
            sent: Mutex::new(Vec::new()),
        });
        (
            Http::with_transport(transport.clone(), Http::test_config()),
            transport,
        )
    }

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

    fn get(url: &str, status: u16, content_type: &str, body: String) -> Exchange {
        exchange("GET", url, status, content_type, body)
    }

    fn post(url: &str, status: u16, content_type: &str, body: String) -> Exchange {
        exchange("POST", url, status, content_type, body)
    }

    fn token(base: &str, realm: &str, value: &str) -> Exchange {
        get(
            &format!("{base}token?realm={realm}"),
            200,
            "application/json",
            json!({"data": {"attributes": {"token": value}}}).to_string(),
        )
    }

    fn record(id: &str, name: &str) -> String {
        json!({
            "data": {
                "id": id,
                "type": "video",
                "attributes": {
                    "name": format!("  {name} "),
                    "description": " An episode. ",
                    "videoDuration": 2649856,
                    "publishStart": "2013-04-08T20:42:00Z",
                    "seasonNumber": 1,
                    "episodeNumber": 1,
                },
            },
            "included": [
                {"type": "channel", "attributes": {"name": "Kanal 5"}},
                {"type": "image", "attributes": {"src": "https://images.test/small.jpg", "width": 320, "height": 180}},
                {"type": "image", "attributes": {"src": "https://images.test/large.jpg", "width": 1920, "height": 1080}},
                {"type": "show", "attributes": {"name": "A show"}},
                {"type": "tag", "attributes": {"name": "history"}},
            ],
        })
        .to_string()
    }

    const MASTER: &str = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1280x720,CODECS=\"avc1.64001f,mp4a.40.2\"\n720.m3u8\n";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n0.ts\n#EXTINF:5.5,\n1.ts\n#EXT-X-ENDLIST\n";
    const MPD: &str = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT44M9.856S" profiles="urn:mpeg:dash:profile:isoff-on-demand:2011">
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <Representation id="v1080" bandwidth="4000000" width="1920" height="1080" codecs="avc1.640028"><BaseURL>v1080.mp4</BaseURL></Representation>
    </AdaptationSet>
    <AdaptationSet mimeType="audio/mp4" contentType="audio" lang="sv">
      <Representation id="a128" bandwidth="128000" codecs="mp4a.40.2"><BaseURL>a128.mp4</BaseURL></Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    fn hls_exchanges(fixture: &mut Fixture) {
        fixture.exchanges.push(get(
            "https://cdn.test/v/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MASTER.into(),
        ));
        fixture.exchanges.push(get(
            "https://cdn.test/v/720.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MEDIA.into(),
        ));
    }

    fn link(url: &str) -> Option<Link> {
        parse_link(&Url::parse(url).unwrap())
    }

    fn video(network: Network, display_id: &str) -> Option<Link> {
        Some(Link::Video {
            network,
            display_id: display_id.into(),
        })
    }

    #[test]
    fn links_are_read() {
        assert_eq!(
            link(
                "https://www.ahctv.com/video/blood-and-fury-americas-civil-war-ahc/battle-of-bull-run"
            ),
            video(
                Network::AmHistoryChannel,
                "blood-and-fury-americas-civil-war-ahc/battle-of-bull-run"
            )
        );
        assert_eq!(
            link("https://www.ahctv.com/video/modern-sniper-ahc/army"),
            video(Network::AmHistoryChannel, "modern-sniper-ahc/army")
        );
        assert_eq!(
            link(
                "https://www.animalplanet.com/video/north-woods-law-animal-planet/squirrel-showdown"
            ),
            video(
                Network::AnimalPlanet,
                "north-woods-law-animal-planet/squirrel-showdown"
            )
        );
        assert_eq!(
            link(
                "https://watch.cookingchanneltv.com/video/carnival-eats-cooking-channel/the-postman-always-brings-rice-2348634"
            ),
            video(
                Network::CookingChannel,
                "carnival-eats-cooking-channel/the-postman-always-brings-rice-2348634"
            )
        );
        assert_eq!(
            link(
                "https://www.destinationamerica.com/video/alaska-monsters-destination-america-atve-us/central-alaskas-bigfoot"
            ),
            video(
                Network::DestinationAmerica,
                "alaska-monsters-destination-america-atve-us/central-alaskas-bigfoot"
            )
        );
        assert_eq!(
            link(
                "https://www.discoverylife.com/video/surviving-death-discovery-life-atve-us/bodily-trauma"
            ),
            video(
                Network::DiscoveryLife,
                "surviving-death-discovery-life-atve-us/bodily-trauma"
            )
        );
        assert_eq!(
            link(
                "https://watch.foodnetwork.com/video/kids-baking-championship-food-network/float-like-a-butterfly"
            ),
            video(
                Network::FoodNetwork,
                "kids-baking-championship-food-network/float-like-a-butterfly"
            )
        );
        assert_eq!(
            link("https://discovery.com/video/dirty-jobs-discovery-atve-us/rodbuster-galvanizer"),
            video(
                Network::GoDiscovery,
                "dirty-jobs-discovery-atve-us/rodbuster-galvanizer"
            )
        );
        assert_eq!(
            link(
                "https://go.discovery.com/video/in-the-eye-of-the-storm-discovery-atve-us/trapped-in-a-twister"
            ),
            video(
                Network::GoDiscovery,
                "in-the-eye-of-the-storm-discovery-atve-us/trapped-in-a-twister"
            )
        );
        assert_eq!(
            link("https://de.hgtv.com/sendungen/mein-kleinstadt-traumhaus/vom-landleben-ins-loft"),
            video(
                Network::HgtvDe,
                "mein-kleinstadt-traumhaus/vom-landleben-ins-loft"
            )
        );
        assert_eq!(
            link("https://watch.hgtv.com/video/home-inspector-joe-hgtv-atve-us/this-mold-house"),
            video(
                Network::HgtvUsa,
                "home-inspector-joe-hgtv-atve-us/this-mold-house"
            )
        );
        assert_eq!(
            link(
                "https://www.investigationdiscovery.com/video/unmasked-investigation-discovery/the-killer-clown"
            ),
            video(
                Network::InvestigationDiscovery,
                "unmasked-investigation-discovery/the-killer-clown"
            )
        );
        assert_eq!(
            link(
                "https://www.sciencechannel.com/video/strangest-things-science-atve-us/nazi-mystery-machine"
            ),
            video(
                Network::ScienceChannel,
                "strangest-things-science-atve-us/nazi-mystery-machine"
            )
        );
        assert_eq!(
            link("https://go.tlc.com/video/my-600-lb-life-tlc/melissas-story-part-1"),
            video(Network::Tlc, "my-600-lb-life-tlc/melissas-story-part-1")
        );
        assert_eq!(
            link(
                "https://watch.travelchannel.com/video/ghost-adventures-travel-channel/ghost-train-of-ely"
            ),
            video(
                Network::TravelChannel,
                "ghost-adventures-travel-channel/ghost-train-of-ely"
            )
        );

        let dplay = |domain: &str, country: &str| Network::DPlay {
            domain: domain.into(),
            country: country.into(),
        };
        assert_eq!(
            link(
                "https://www.dplay.se/videos/nugammalt-77-handelser-som-format-sverige/nugammalt-77-handelser-som-format-sverige-101"
            ),
            video(
                dplay("dplay.se", "se"),
                "nugammalt-77-handelser-som-format-sverige/nugammalt-77-handelser-som-format-sverige-101"
            )
        );
        assert_eq!(
            link(
                "http://www.dplay.dk/videoer/ted-bundy-mind-of-a-monster/ted-bundy-mind-of-a-monster"
            ),
            video(
                dplay("dplay.dk", "dk"),
                "ted-bundy-mind-of-a-monster/ted-bundy-mind-of-a-monster"
            )
        );
        assert_eq!(
            link("https://www.dplay.no/videoer/i-kongens-klr/sesong-1-episode-7"),
            video(dplay("dplay.no", "no"), "i-kongens-klr/sesong-1-episode-7")
        );
        assert_eq!(
            link(
                "http://it.dplay.com/nove/biografie-imbarazzanti/luigi-di-maio-la-psicosi-di-stanislawskij/"
            ),
            video(
                dplay("it.dplay.com", "it"),
                "biografie-imbarazzanti/luigi-di-maio-la-psicosi-di-stanislawskij"
            )
        );
        assert_eq!(
            link("https://es.dplay.com/dmax/la-fiebre-del-oro/temporada-8-episodio-1/"),
            video(
                dplay("es.dplay.com", "es"),
                "la-fiebre-del-oro/temporada-8-episodio-1"
            )
        );
        assert_eq!(
            link("https://www.dplay.fi/videot/shifting-gears-with-aaron-kaufman/episode-16"),
            video(
                dplay("dplay.fi", "fi"),
                "shifting-gears-with-aaron-kaufman/episode-16"
            )
        );
        assert_eq!(
            link("https://www.dplay.jp/video/gold-rush/24086"),
            video(dplay("dplay.jp", "jp"), "gold-rush/24086")
        );
        assert_eq!(
            link(
                "https://www.discoveryplus.se/videos/nugammalt-77-handelser-som-format-sverige/nugammalt-77-handelser-som-format-sverige-101"
            ),
            video(
                dplay("discoveryplus.se", "se"),
                "nugammalt-77-handelser-som-format-sverige/nugammalt-77-handelser-som-format-sverige-101"
            )
        );
        assert_eq!(
            link(
                "https://www.discoveryplus.dk/videoer/ted-bundy-mind-of-a-monster/ted-bundy-mind-of-a-monster"
            ),
            video(
                dplay("discoveryplus.dk", "dk"),
                "ted-bundy-mind-of-a-monster/ted-bundy-mind-of-a-monster"
            )
        );
        assert_eq!(
            link("https://www.discoveryplus.no/videoer/i-kongens-klr/sesong-1-episode-7"),
            video(
                dplay("discoveryplus.no", "no"),
                "i-kongens-klr/sesong-1-episode-7"
            )
        );
        assert_eq!(
            link(
                "https://www.discoveryplus.it/videos/biografie-imbarazzanti/luigi-di-maio-la-psicosi-di-stanislawskij"
            ),
            video(
                dplay("discoveryplus.it", "it"),
                "biografie-imbarazzanti/luigi-di-maio-la-psicosi-di-stanislawskij"
            )
        );
        assert_eq!(
            link("https://www.discoveryplus.es/videos/la-fiebre-del-oro/temporada-8-episodio-1"),
            video(
                dplay("discoveryplus.es", "es"),
                "la-fiebre-del-oro/temporada-8-episodio-1"
            )
        );
        assert_eq!(
            link(
                "https://www.discoveryplus.fi/videot/shifting-gears-with-aaron-kaufman/episode-16"
            ),
            video(
                dplay("discoveryplus.fi", "fi"),
                "shifting-gears-with-aaron-kaufman/episode-16"
            )
        );

        let german = |domain: &str, programme: &str, alternate_id: &str| {
            Some(Link::GermanVideo {
                domain: domain.into(),
                programme: programme.into(),
                alternate_id: alternate_id.into(),
            })
        };
        assert_eq!(
            link("https://dmax.de/sendungen/goldrausch-in-australien/german-gold"),
            german("dmax.de", "goldrausch-in-australien", "german-gold")
        );
        assert_eq!(
            link(
                "https://www.tlc.de/programme/breaking-amish/video/die-welt-da-drauen/DCB331270001100"
            ),
            german("tlc.de", "breaking-amish", "die-welt-da-drauen")
        );
        assert_eq!(
            link(
                "https://www.dmax.de/programme/dmax-highlights/video/tuning-star-sidney-hoffmann-exklusiv-bei-dmax/191023082312316"
            ),
            german(
                "dmax.de",
                "dmax-highlights",
                "tuning-star-sidney-hoffmann-exklusiv-bei-dmax"
            )
        );
        assert_eq!(
            link("https://tlc.de/sendungen/breaking-amish/die-welt-da-drauen/"),
            german("tlc.de", "breaking-amish", "die-welt-da-drauen")
        );
        assert_eq!(
            link("https://tlc.de/sendungen/evil-gesichter-des-boesen/das-geheimnis-meines-bruders"),
            german(
                "tlc.de",
                "evil-gesichter-des-boesen",
                "das-geheimnis-meines-bruders"
            )
        );

        let plus = |country: &str| Network::DiscoveryPlus {
            country: country.into(),
        };
        assert_eq!(
            link(
                "https://www.discoveryplus.com/video/property-brothers-forever-home/food-and-family"
            ),
            video(plus("us"), "property-brothers-forever-home/food-and-family")
        );
        assert_eq!(
            link("https://discoveryplus.com/ca/video/bering-sea-gold-discovery-ca/goldslingers"),
            video(plus("ca"), "bering-sea-gold-discovery-ca/goldslingers")
        );
        assert_eq!(
            link(
                "https://www.discoveryplus.com/gb/video/sport/eurosport-1-british-eurosport-1-british-sport/6-hours-of-spa-review"
            ),
            video(
                plus("gb"),
                "eurosport-1-british-eurosport-1-british-sport/6-hours-of-spa-review"
            )
        );
        assert_eq!(
            link(
                "https://www.discoveryplus.com/gb/video/olympics/dplus-sport-dplus-sport-sport/rugby-sevens-australia-samoa"
            ),
            video(
                plus("gb"),
                "dplus-sport-dplus-sport-sport/rugby-sevens-australia-samoa"
            )
        );
        assert_eq!(
            link(
                "https://www.discoveryplus.com/it/video/i-signori-della-neve/stagione-2-episodio-1-i-preparativi"
            ),
            video(
                Network::DiscoveryPlusItaly,
                "i-signori-della-neve/stagione-2-episodio-1-i-preparativi"
            )
        );
        assert_eq!(
            link("https://www.discoveryplus.com/it/video/super-benny/trailer"),
            video(Network::DiscoveryPlusItaly, "super-benny/trailer")
        );
        assert_eq!(
            link(
                "https://www.discoveryplus.com/it/video/olympics/dplus-sport-dplus-sport-sport/water-polo-greece-italy"
            ),
            video(
                Network::DiscoveryPlusItaly,
                "dplus-sport-dplus-sport-sport/water-polo-greece-italy"
            )
        );
        assert_eq!(
            link(
                "https://www.discoveryplus.com/it/video/sport/dplus-sport-dplus-sport-sport/lisa-vittozzi-allinferno-e-ritorno"
            ),
            video(
                Network::DiscoveryPlusItaly,
                "dplus-sport-dplus-sport-sport/lisa-vittozzi-allinferno-e-ritorno"
            )
        );
        assert_eq!(
            link(
                "https://www.discoveryplus.in/videos/how-do-they-do-it/fugu-and-more?seasonId=8&type=EPISODE"
            ),
            video(
                Network::DiscoveryPlusIndia,
                "how-do-they-do-it/fugu-and-more"
            )
        );
        assert_eq!(
            link("https://www.discoveryplus.in/video/how-do-they-do-it/fugu-and-more"),
            video(
                Network::DiscoveryPlusIndia,
                "how-do-they-do-it/fugu-and-more"
            )
        );
        assert_eq!(
            link("https://www.discoveryplus.in/show/how-do-they-do-it"),
            Some(Link::Show {
                catalogue: Catalogue::India,
                show_name: "how-do-they-do-it".into()
            })
        );
        assert_eq!(
            link("https://www.discoveryplus.it/programmi/deal-with-it-stai-al-gioco/?ref=1"),
            Some(Link::Show {
                catalogue: Catalogue::Italy,
                show_name: "deal-with-it-stai-al-gioco".into()
            })
        );

        assert_eq!(
            link("https://www.discoveryplus.com/it/show/super-benny"),
            None
        );
        assert_eq!(
            link("https://www.discoveryplus.com/video/only-one-segment"),
            None
        );
        assert_eq!(link("https://www.discoveryplus.in/show/"), None);
        // The dplay pattern takes any word before `{show}/{episode}`, so a programmi path
        // deeper than a show's is a video on the Italian dplay site, as it is for yt-dlp.
        assert_eq!(
            link("https://www.discoveryplus.it/programmi/a/b/c"),
            video(
                Network::DPlay {
                    domain: "discoveryplus.it".into(),
                    country: "it".into()
                },
                "a/b"
            )
        );
        assert_eq!(link("https://www.dplay.de/videos/a/b"), None);
        assert_eq!(link("https://media.discovery.com/video/a/b"), None);
        assert_eq!(link("https://tlc.de/news/a"), None);
        assert_eq!(link("ftp://go.tlc.com/video/a/b"), None);
    }

    #[test]
    fn sites_are_derived_from_the_link() {
        let se = Network::DPlay {
            domain: "dplay.se".into(),
            country: "se".into(),
        }
        .site();
        assert_eq!(se.disco_host, "disco-api.dplay.se");
        assert_eq!(se.realm, "dplayse");
        assert_eq!(se.client, Client::Legacy);
        assert_eq!(se.referer.as_deref(), Some("https://www.dplay.se/"));
        let it = Network::DPlay {
            domain: "it.dplay.com".into(),
            country: "it".into(),
        }
        .site();
        assert_eq!(it.disco_host, "eu2-prod.disco-api.com");
        assert_eq!(it.realm, "dplayit");
        assert_eq!(it.referer.as_deref(), Some("https://it.dplay.com/"));
        let gb = Network::DiscoveryPlus {
            country: "gb".into(),
        }
        .site();
        assert_eq!(gb.disco_host, "eu1-prod-direct.discoveryplus.com");
        assert_eq!(gb.realm, "dplay");
        assert_eq!(
            gb.client.headers("dplay"),
            vec![
                (
                    "x-disco-params".to_string(),
                    "realm=dplay,siteLookupKey=dplus_gb".to_string()
                ),
                (
                    "x-disco-client".to_string(),
                    "WEB:UNKNOWN:dplus_us:27.43.0".to_string()
                ),
            ]
        );
        let ca = Network::DiscoveryPlus {
            country: "ca".into(),
        }
        .site();
        assert_eq!(ca.disco_host, "us1-prod-direct.discoveryplus.com");
        assert_eq!(ca.realm, "go");
        let tlc = Network::DiscoveryNetworksDe {
            domain: "tlc.de".into(),
        }
        .site();
        assert_eq!(tlc.realm, "tlcde");
        assert_eq!(
            tlc.client.headers("tlcde"),
            vec![
                ("x-disco-params".to_string(), "realm=tlcde".to_string()),
                (
                    "x-disco-client".to_string(),
                    "Alps:HyogaPlayer:0.0.0".to_string()
                ),
            ]
        );
        assert_eq!(
            Network::GoDiscovery.site().client.headers("go"),
            vec![
                (
                    "x-disco-params".to_string(),
                    "realm=go,siteLookupKey=dsc".to_string()
                ),
                (
                    "x-disco-client".to_string(),
                    "WEB:UNKNOWN:dsc:27.43.0".to_string()
                ),
            ]
        );
        assert_eq!(sites().len(), 32);
        assert!(sites().iter().all(|site| site.base().is_some()));
    }

    #[test]
    fn playback_streams_are_read_in_both_shapes() {
        let legacy = json!({"hls": {"url": "https://cdn.test/v/master.m3u8"}, "dash": {"url": ""}});
        let streams = streams_of(&legacy);
        assert_eq!(streams.len(), 1);
        assert_eq!(streams[0].kind.as_deref(), Some("hls"));
        let v3 = json!([{"type": "dash", "url": "https://cdn.test/v/manifest.mpd"}, {"url": "https://cdn.test/v/file.mp4"}, {"type": "hls"}]);
        let streams = streams_of(&v3);
        assert_eq!(streams.len(), 2);
        assert_eq!(streams[0].kind.as_deref(), Some("dash"));
        assert_eq!(streams[1].kind, None);
        assert!(streams_of(&Value::Null).is_empty());
    }

    #[tokio::test]
    async fn dplay_videos_resolve_through_the_legacy_api() {
        let base = "https://disco-api.dplay.se/";
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(token(base, "dplayse", "anon-token"));
        fixture.exchanges.push(get(
            &format!("{base}content/videos/nugammalt-77-handelser-som-format-sverige/nugammalt-77-handelser-som-format-sverige-101"),
            200,
            "application/json",
            record("13628", "Svensken lär sig  njuta av livet"),
        ));
        fixture.exchanges.push(get(
            &format!("{base}playback/videoPlaybackInfo/13628"),
            200,
            "application/json",
            json!({"data": {"attributes": {"streaming": {
                "hls": {"url": "https://cdn.test/v/master.m3u8"},
                "dash": {"url": "https://cdn.test/v/manifest.mpd"},
                "mp4": {"url": "https://cdn.test/v/file.mp4"},
            }}}})
            .to_string(),
        ));
        hls_exchanges(&mut fixture);
        fixture.exchanges.push(get(
            "https://cdn.test/v/manifest.mpd",
            200,
            "application/dash+xml",
            MPD.into(),
        ));
        let resolver = DplayResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.dplay.se/videos/nugammalt-77-handelser-som-format-sverige/nugammalt-77-handelser-som-format-sverige-101").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.resolver, PLATFORM);
        assert_eq!(resolved.id.as_deref(), Some("13628"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Svensken lär sig njuta av livet")
        );
        assert_eq!(resolved.description.as_deref(), Some("An episode."));
        assert_eq!(resolved.uploader.as_deref(), Some("Kanal 5"));
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(2649.856)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1365453720)
        );
        assert_eq!(
            resolved.thumbnail.as_ref().map(|u| u.as_str()),
            Some("https://images.test/large.jpg")
        );
        assert_eq!(resolved.webpage_url.as_ref(), Some(&url));
        assert!(!resolved.live);
        let hls: Vec<&Variant> = resolved
            .variants
            .iter()
            .filter(|v| v.kind == VariantKind::Hls)
            .collect();
        assert_eq!(hls.len(), 1);
        assert_eq!(hls[0].height, Some(720));
        assert_eq!(hls[0].url.as_str(), "https://cdn.test/v/720.m3u8");
        assert_eq!(
            hls[0].headers,
            vec![("referer".to_string(), "https://www.dplay.se/".to_string())]
        );
        assert!(
            resolved
                .variants
                .iter()
                .any(|v| v.kind == VariantKind::Dash && v.height == Some(1080))
        );
        let file = resolved
            .variants
            .iter()
            .find(|v| v.kind == VariantKind::File)
            .unwrap();
        assert_eq!(file.url.as_str(), "https://cdn.test/v/file.mp4");
        assert_eq!(file.container, Some(Container::Mp4));
        assert_eq!(file.format_id.as_deref(), Some("mp4"));
        assert_eq!(file.headers[0].0, "referer");
        assert!(
            resolved
                .variants
                .iter()
                .all(|v| v.headers.iter().all(|(name, _)| name != FORWARDED_FOR))
        );
    }

    #[tokio::test]
    async fn network_sites_resolve_through_the_v3_playback_endpoint() {
        let base = "https://us1-prod-direct.go.discovery.com/";
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(token(base, "go", "anon-token"));
        fixture.exchanges.push(get(
            &format!("{base}content/videos/in-the-eye-of-the-storm-discovery-atve-us/trapped-in-a-twister"),
            200,
            "application/json",
            json!({
                "data": {"id": 5352642, "attributes": {"name": "Trapped in a Twister", "videoDuration": 2490237, "publishStart": "2024-07-15T02:00:00Z"}},
                "included": [{"type": "channel", "attributes": {"name": "Discovery"}}],
            })
            .to_string(),
        ));
        fixture.exchanges.push(post(
            &format!("{base}playback/v3/videoPlaybackInfo"),
            200,
            "application/json",
            json!({"data": {"attributes": {"streaming": [{"type": "hls", "url": "https://cdn.test/v/master.m3u8"}]}}}).to_string(),
        ));
        hls_exchanges(&mut fixture);
        let resolver = DplayResolver::new(Http::replay(fixture));
        let url = Url::parse("https://go.discovery.com/video/in-the-eye-of-the-storm-discovery-atve-us/trapped-in-a-twister").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("5352642"));
        assert_eq!(resolved.title.as_deref(), Some("Trapped in a Twister"));
        assert_eq!(resolved.uploader.as_deref(), Some("Discovery"));
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(2490.237)));
        assert_eq!(resolved.thumbnail, None);
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(resolved.variants[0].kind, VariantKind::Hls);
        assert!(resolved.variants[0].headers.is_empty());
    }

    #[tokio::test]
    async fn german_sites_look_the_api_id_up_in_their_cms() {
        let base = "https://eu1-prod.disco-api.com/";
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://de-api.loma-cms.com/feloma/videos/german-gold/?environment=dmax&v=2&filter[show.slug]=goldrausch-in-australien",
            200,
            "application/json",
            json!({"uid": "video-4756322", "taxonomies": [{"category": "genre", "title": "Gold"}]}).to_string(),
        ));
        fixture.exchanges.push(token(base, "dmaxde", "anon-token"));
        fixture.exchanges.push(get(
            &format!("{base}content/videos/4756322"),
            200,
            "application/json",
            record("4756322", "German Gold"),
        ));
        fixture.exchanges.push(post(
            &format!("{base}playback/v3/videoPlaybackInfo"),
            200,
            "application/json",
            json!({"data": {"attributes": {"streaming": [{"type": "hls", "url": "https://cdn.test/v/master.m3u8"}]}}}).to_string(),
        ));
        hls_exchanges(&mut fixture);
        let resolver = DplayResolver::new(Http::replay(fixture));
        let url =
            Url::parse("https://dmax.de/sendungen/goldrausch-in-australien/german-gold").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("4756322"));
        assert_eq!(resolved.title.as_deref(), Some("German Gold"));
        assert_eq!(resolved.variants.len(), 1);

        // Without a CMS record the page path names the episode.
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://de-api.loma-cms.com/feloma/videos/der-poltergeist-im-kostumladen/",
            404,
            "application/json",
            json!({"detail": "Not found."}).to_string(),
        ));
        fixture.exchanges.push(token(base, "tlcde", "anon-token"));
        fixture.exchanges.push(get(
            &format!("{base}content/videos/ghost-adventures/der-poltergeist-im-kostumladen"),
            200,
            "application/json",
            record("4550602", "Der Poltergeist im Kostümladen"),
        ));
        fixture.exchanges.push(post(
            &format!("{base}playback/v3/videoPlaybackInfo"),
            200,
            "application/json",
            json!({"data": {"attributes": {"streaming": [{"type": "hls", "url": "https://cdn.test/v/master.m3u8"}]}}}).to_string(),
        ));
        hls_exchanges(&mut fixture);
        let resolver = DplayResolver::new(Http::replay(fixture));
        let url =
            Url::parse("https://tlc.de/sendungen/ghost-adventures/der-poltergeist-im-kostumladen")
                .unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("4550602"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Der Poltergeist im Kostümladen")
        );
    }

    fn episodes(names: &[&str]) -> Vec<Value> {
        names
            .iter()
            .map(|name| {
                json!({"id": format!("id-{name}"), "attributes": {"path": format!("how-do-they-do-it/{name}"), "name": name, "videoDuration": 1319320}})
            })
            .collect()
    }

    #[tokio::test]
    async fn shows_list_every_episode_of_every_season() {
        let base = "https://ap2-prod-direct.discoveryplus.in/";
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture
            .exchanges
            .push(token(base, "dplusindia", "anon-token"));
        fixture.exchanges.push(get(
            &format!("{base}cms/routes/show/how-do-they-do-it?include=default"),
            200,
            "application/json",
            json!({
                "data": {"type": "route"},
                "included": [
                    {"type": "page", "attributes": {}},
                    {"type": "show", "id": "1234", "attributes": {"name": "How Do They Do It?"}},
                    {"type": "pageItem", "attributes": {}},
                    {"type": "collection", "attributes": {"component": {"id": "hero"}}},
                    {"type": "collection", "attributes": {"component": {
                        "id": "tabbed-content",
                        "mandatoryParams": "pf[show.id]=1234",
                        "filters": [{"id": "seasonNumber", "options": [{"id": "1"}, {"id": 2}]}],
                    }}},
                ],
            })
            .to_string(),
        ));
        fixture.exchanges.push(get(
            &format!("{base}content/videos?sort=episodeNumber&filter[seasonNumber]=1&filter[show.id]=1234&page[size]=100&page[number]=1"),
            200,
            "application/json",
            json!({"meta": {"totalPages": 2}, "data": episodes(&["fugu-and-more", "glass-eyes"])}).to_string(),
        ));
        fixture.exchanges.push(get(
            &format!("{base}content/videos?sort=episodeNumber&filter[seasonNumber]=1&filter[show.id]=1234&page[size]=100&page[number]=2"),
            200,
            "application/json",
            json!({"meta": {"totalPages": 2}, "data": episodes(&["pencils"])}).to_string(),
        ));
        fixture.exchanges.push(get(
            &format!("{base}content/videos?sort=episodeNumber&filter[seasonNumber]=2&filter[show.id]=1234&page[size]=100&page[number]=1"),
            200,
            "application/json",
            json!({"meta": {"totalPages": 1}, "data": episodes(&["submarines"])}).to_string(),
        ));
        let resolver = DplayResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.discoveryplus.in/show/how-do-they-do-it").unwrap();
        assert!(resolver.matches(&url));
        let Resolution::Playlist(playlist) = resolver.resolve(&url).await.unwrap() else {
            panic!("a show is a playlist");
        };
        assert_eq!(playlist.resolver, PLATFORM);
        assert_eq!(playlist.id.as_deref(), Some("how-do-they-do-it"));
        assert_eq!(playlist.title.as_deref(), Some("How Do They Do It?"));
        assert_eq!(playlist.total, None);
        let urls: Vec<&str> = playlist.entries.iter().map(|e| e.url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://www.discoveryplus.in/videos/how-do-they-do-it/fugu-and-more",
                "https://www.discoveryplus.in/videos/how-do-they-do-it/glass-eyes",
                "https://www.discoveryplus.in/videos/how-do-they-do-it/pencils",
                "https://www.discoveryplus.in/videos/how-do-they-do-it/submarines",
            ]
        );
        assert_eq!(playlist.entries[0].title.as_deref(), Some("fugu-and-more"));
        assert_eq!(
            playlist.entries[0].duration,
            Some(Duration::from_secs_f64(1319.32))
        );
        assert!(playlist.entries.iter().all(|e| resolver.matches(&e.url)));
    }

    #[tokio::test]
    async fn italian_shows_stop_at_five_hundred_episodes() {
        let base = "https://disco-api.discoveryplus.it/";
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(token(base, "dplayit", "anon-token"));
        fixture.exchanges.push(get(
            &format!("{base}cms/routes/programmi/deal-with-it-stai-al-gioco?include=default"),
            200,
            "application/json",
            json!({
                "included": [
                    {"type": "page", "attributes": {}},
                    {"type": "collection", "attributes": {"component": {"id": "hero"}}},
                    {"type": "collection", "attributes": {"component": {
                        "mandatoryParams": "pf[show.id]=99",
                        "filters": [{"options": [{"id": "1"}, {"id": "2"}]}],
                    }}},
                ],
            })
            .to_string(),
        ));
        for page in 1..=6 {
            let names: Vec<String> = (1..=100).map(|n| format!("p{page}-e{n}")).collect();
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            fixture.exchanges.push(get(
                &format!("{base}content/videos?sort=episodeNumber&filter[seasonNumber]=1&filter[show.id]=99&page[size]=100&page[number]={page}"),
                200,
                "application/json",
                json!({"meta": {"totalPages": 6}, "data": episodes(&names)}).to_string(),
            ));
        }
        let resolver = DplayResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.discoveryplus.it/programmi/deal-with-it-stai-al-gioco")
            .unwrap();
        let Resolution::Playlist(playlist) = resolver.resolve(&url).await.unwrap() else {
            panic!("a show is a playlist");
        };
        assert_eq!(playlist.entries.len(), 500);
        assert_eq!(playlist.title, None);
        assert_eq!(
            playlist.entries[499].url.as_str(),
            "https://www.discoveryplus.it/videos/how-do-they-do-it/p5-e100"
        );
        assert!(matches!(
            parse_link(&playlist.entries[0].url),
            Some(Link::Video {
                network: Network::DPlay { .. },
                ..
            })
        ));
    }

    fn error_body(code: &str, detail: &str) -> String {
        json!({"errors": [{"status": "403", "code": code, "detail": detail}]}).to_string()
    }

    #[tokio::test]
    async fn region_refusals_are_retried_with_a_faked_address() {
        let base = "https://disco-api.dplay.dk/";
        let content =
            format!("{base}content/videos/ted-bundy-mind-of-a-monster/ted-bundy-mind-of-a-monster");
        let url = Url::parse(
            "http://www.dplay.dk/videoer/ted-bundy-mind-of-a-monster/ted-bundy-mind-of-a-monster",
        )
        .unwrap();

        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(token(base, "dplaydk", "anon-token"));
        fixture.exchanges.push(get(
            &content,
            400,
            "application/json",
            error_body("access.denied.geoblocked", "Geoblocked"),
        ));
        fixture.exchanges.push(get(
            &content,
            200,
            "application/json",
            record("104465", "Ted Bundy: Mind Of A Monster"),
        ));
        fixture.exchanges.push(get(
            &format!("{base}playback/videoPlaybackInfo/104465"),
            200,
            "application/json",
            json!({"data": {"attributes": {"streaming": {"hls": {"url": "https://cdn.test/v/master.m3u8"}}}}}).to_string(),
        ));
        hls_exchanges(&mut fixture);
        let resolver = DplayResolver::new(Http::replay(fixture));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("104465"));
        assert!(
            resolved
                .variants
                .iter()
                .all(|v| v.headers.iter().all(|(name, _)| name != FORWARDED_FOR))
        );

        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(token(base, "dplaydk", "anon-token"));
        fixture.exchanges.push(get(
            &content,
            400,
            "application/json",
            error_body("access.denied.geoblocked", "Geoblocked"),
        ));
        fixture.exchanges.push(get(
            &content,
            400,
            "application/json",
            error_body("access.denied.geoblocked", "Geoblocked"),
        ));
        let resolver = DplayResolver::new(Http::replay(fixture));
        let error = resolver.resolve(&url).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "available only in DK"),
            "{error}"
        );
    }

    /// The Indian catalogue answers callers outside India with a 404 whose detail says
    /// the video "was filtered by validator"; the token the first call minted was set as
    /// the `st` cookie, and the retry with a faked address must mint a fresh one rather
    /// than send that cookie again.
    #[tokio::test]
    async fn validator_filtered_videos_are_retried_with_a_fresh_token_and_faked_address() {
        let base = "https://ap2-prod-direct.discoveryplus.in/";
        let content = format!("{base}content/videos/how-do-they-do-it/fugu-and-more");
        let url = Url::parse(
            "https://www.discoveryplus.in/videos/how-do-they-do-it/fugu-and-more?seasonId=8&type=EPISODE",
        )
        .unwrap();
        let filtered = json!({"errors": [{"status": "404", "code": "not.found", "detail": "video with id fugu-and-more was filtered by validator, reasonCode=9"}]}).to_string();

        let mut fixture = Fixture::new(PLATFORM, None);
        let mut first = exchange(
            "GET",
            &format!("{base}token?realm=dplusindia"),
            200,
            "application/json",
            json!({"data": {"attributes": {"token": "outside-india"}}}).to_string(),
        );
        first.response.headers.push((
            "set-cookie".to_string(),
            "st=outside-india; Path=/; Domain=.discoveryplus.in; Secure".to_string(),
        ));
        fixture.exchanges.push(first);
        fixture
            .exchanges
            .push(get(&content, 404, "application/json", filtered.clone()));
        fixture
            .exchanges
            .push(token(base, "dplusindia", "inside-india"));
        fixture.exchanges.push(get(
            &content,
            200,
            "application/json",
            record("27104", "Fugu and More"),
        ));
        fixture.exchanges.push(post(
            &format!("{base}playback/v3/videoPlaybackInfo"),
            200,
            "application/json",
            json!({"data": {"attributes": {"streaming": [{"type": "hls", "url": "https://cdn.test/v/master.m3u8"}]}}}).to_string(),
        ));
        hls_exchanges(&mut fixture);
        let (http, transport) = watched(fixture);
        let resolver = DplayResolver::new(http);
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("27104"));
        assert_eq!(resolved.title.as_deref(), Some("Fugu and More"));
        let sent = transport.sent.lock().unwrap();
        let tokens: Vec<&Sent> = sent
            .iter()
            .filter(|request| request.url.contains("/token?"))
            .collect();
        assert_eq!(tokens.len(), 2, "the retry mints a fresh token");
        assert!(
            tokens[0].header(FORWARDED_FOR).is_none(),
            "the first token is minted as the caller"
        );
        assert!(
            tokens[1].header(FORWARDED_FOR).is_some(),
            "the second token is minted behind the faked address"
        );
        assert!(
            !tokens[1]
                .header("cookie")
                .unwrap_or("")
                .contains("outside-india"),
            "the refused token's cookie is dropped before the fresh mint"
        );
        let retried = sent
            .iter()
            .filter(|request| request.url.starts_with(&content))
            .nth(1)
            .expect("the video record is asked for twice");
        assert_eq!(retried.header("authorization"), Some("Bearer inside-india"));
        assert!(retried.header(FORWARDED_FOR).is_some());
        assert!(
            !retried
                .header("cookie")
                .unwrap_or("")
                .contains("outside-india"),
            "the cookie of the refused token is not sent again"
        );
    }

    /// A video the catalogue does not have is a 404 whichever address asks for it.
    #[tokio::test]
    async fn missing_videos_are_not_found_after_one_faked_address_retry() {
        let base = "https://ap2-prod-direct.discoveryplus.in/";
        let content = format!("{base}content/videos/how-do-they-do-it/does-not-exist");
        let url =
            Url::parse("https://www.discoveryplus.in/videos/how-do-they-do-it/does-not-exist")
                .unwrap();
        let missing = json!({"errors": [{"status": "404", "code": "not.found", "detail": "video with id does-not-exist could not be found"}]}).to_string();
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture
            .exchanges
            .push(token(base, "dplusindia", "outside-india"));
        fixture
            .exchanges
            .push(get(&content, 404, "application/json", missing.clone()));
        fixture
            .exchanges
            .push(token(base, "dplusindia", "inside-india"));
        fixture
            .exchanges
            .push(get(&content, 404, "application/json", missing));
        let (http, transport) = watched(fixture);
        let resolver = DplayResolver::new(http);
        let error = resolver.resolve(&url).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::NotFound(at) if at == &url),
            "{error}"
        );
        let sent = transport.sent.lock().unwrap();
        assert_eq!(
            sent.iter()
                .filter(|request| request.url.starts_with(&content))
                .count(),
            2,
            "asked once as the caller and once behind the faked address"
        );
    }

    #[tokio::test]
    async fn show_region_refusals_are_retried_with_a_faked_address() {
        let base = "https://ap2-prod-direct.discoveryplus.in/";
        let route = format!("{base}cms/routes/show/how-do-they-do-it?include=default");
        let filtered = json!({"errors": [{"status": "404", "code": "not.found", "detail": "No matching page found for route /show/how-do-they-do-it"}]}).to_string();
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture
            .exchanges
            .push(token(base, "dplusindia", "outside-india"));
        fixture
            .exchanges
            .push(get(&route, 404, "application/json", filtered));
        fixture
            .exchanges
            .push(token(base, "dplusindia", "inside-india"));
        fixture.exchanges.push(get(
            &route,
            200,
            "application/json",
            json!({
                "data": {"type": "route"},
                "included": [
                    {"type": "show", "id": "1234", "attributes": {"name": "How Do They Do It?"}},
                    {"type": "collection", "attributes": {"component": {
                        "id": "tabbed-content",
                        "mandatoryParams": "pf[show.id]=1234",
                        "filters": [{"id": "seasonNumber", "options": [{"id": "1"}]}],
                    }}},
                ],
            })
            .to_string(),
        ));
        fixture.exchanges.push(get(
            &format!("{base}content/videos?sort=episodeNumber&filter[seasonNumber]=1&filter[show.id]=1234&page[size]=100&page[number]=1"),
            200,
            "application/json",
            json!({"meta": {"totalPages": 1}, "data": episodes(&["fugu-and-more"])}).to_string(),
        ));
        let (http, transport) = watched(fixture);
        let resolver = DplayResolver::new(http);
        let url = Url::parse("https://www.discoveryplus.in/show/how-do-they-do-it").unwrap();
        let Resolution::Playlist(playlist) = resolver.resolve(&url).await.unwrap() else {
            panic!("a show is a playlist");
        };
        assert_eq!(playlist.entries.len(), 1);
        let sent = transport.sent.lock().unwrap();
        let listing = sent
            .iter()
            .find(|request| request.url.contains("content/videos?"))
            .expect("the season is listed");
        assert!(listing.header(FORWARDED_FOR).is_some());
        assert_eq!(listing.header("authorization"), Some("Bearer inside-india"));
    }

    #[tokio::test]
    async fn refusals_say_why() {
        let base = "https://us1-prod-direct.tlc.com/";
        let content = format!("{base}content/videos/my-600-lb-life-tlc/melissas-story-part-1");
        let playback = format!("{base}playback/v3/videoPlaybackInfo");
        let url = Url::parse("https://go.tlc.com/video/my-600-lb-life-tlc/melissas-story-part-1")
            .unwrap();

        // A subscription the caller lacks.
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(token(base, "go", "anon-token"));
        fixture.exchanges.push(get(
            &content,
            200,
            "application/json",
            record("2206540", "Melissas Story (Part 1)"),
        ));
        fixture.exchanges.push(post(
            &playback,
            403,
            "application/json",
            error_body("access.denied.missingpackage", "Missing package"),
        ));
        let resolver = DplayResolver::new(Http::replay(fixture));
        let error = resolver.resolve(&url).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::LoginRequired { platform, reason, .. } if *platform == PLATFORM && reason.contains("subscribers")),
            "{error}"
        );

        // Nothing at the path.
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(token(base, "go", "anon-token"));
        fixture.exchanges.push(get(
            &content,
            404,
            "application/json",
            json!({"errors": [{"status": "404", "code": "not.found", "detail": "Not Found"}]})
                .to_string(),
        ));
        let resolver = DplayResolver::new(Http::replay(fixture));
        assert!(matches!(
            resolver.resolve(&url).await.unwrap_err(),
            ResolveError::NotFound(_)
        ));

        // A refusal the API explains.
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(token(base, "go", "anon-token"));
        fixture.exchanges.push(get(
            &content,
            200,
            "application/json",
            record("2206540", "Melissas Story (Part 1)"),
        ));
        fixture.exchanges.push(post(
            &playback,
            403,
            "application/json",
            error_body(
                "access.denied.expired",
                "This content is no longer available",
            ),
        ));
        let resolver = DplayResolver::new(Http::replay(fixture));
        let error = resolver.resolve(&url).await.unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "This content is no longer available"),
            "{error}"
        );

        // No stream that can be read.
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(token(base, "go", "anon-token"));
        fixture.exchanges.push(get(
            &content,
            200,
            "application/json",
            record("2206540", "Melissas Story (Part 1)"),
        ));
        fixture.exchanges.push(post(&playback, 200, "application/json", json!({"data": {"attributes": {"streaming": [{"type": "hls", "url": "https://cdn.test/v/master.m3u8"}]}}}).to_string()));
        fixture.exchanges.push(get(
            "https://cdn.test/v/master.m3u8",
            403,
            "text/plain",
            "denied".into(),
        ));
        let resolver = DplayResolver::new(Http::replay(fixture));
        assert!(matches!(
            resolver.resolve(&url).await.unwrap_err(),
            ResolveError::Unavailable { .. }
        ));
    }

    #[tokio::test]
    async fn remembered_tokens_are_renewed_when_rejected() {
        let base = "https://us1-prod-direct.watch.hgtv.com/";
        let playback = format!("{base}playback/v3/videoPlaybackInfo");
        let first = format!(
            "{base}content/videos/flip-or-flop-the-final-flip-hgtv-atve-us/flip-or-flop-the-final-flip"
        );
        let second =
            format!("{base}content/videos/home-inspector-joe-hgtv-atve-us/this-mold-house");
        let streaming = json!({"data": {"attributes": {"streaming": [{"type": "hls", "url": "https://cdn.test/v/master.m3u8"}]}}}).to_string();
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(token(base, "go", "first-token"));
        fixture.exchanges.push(get(
            &first,
            200,
            "application/json",
            record("5025585", "Flip or Flop: The Final Flip"),
        ));
        fixture
            .exchanges
            .push(post(&playback, 200, "application/json", streaming.clone()));
        hls_exchanges(&mut fixture);
        fixture.exchanges.push(get(
            &second,
            403,
            "application/json",
            error_body("invalid.token", "Token expired"),
        ));
        fixture.exchanges.push(token(base, "go", "second-token"));
        fixture.exchanges.push(get(
            &second,
            200,
            "application/json",
            record("4289736", "This Mold House"),
        ));
        fixture
            .exchanges
            .push(post(&playback, 200, "application/json", streaming));
        hls_exchanges(&mut fixture);
        let resolver = DplayResolver::new(Http::replay(fixture));
        let first_url = Url::parse("https://watch.hgtv.com/video/flip-or-flop-the-final-flip-hgtv-atve-us/flip-or-flop-the-final-flip").unwrap();
        let resolved = resolver.resolve(&first_url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("5025585"));
        let second_url = Url::parse(
            "https://watch.hgtv.com/video/home-inspector-joe-hgtv-atve-us/this-mold-house",
        )
        .unwrap();
        let resolved = resolver
            .resolve(&second_url)
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("4289736"));

        // A token the API rejects when it was minted this very call is a login demand.
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(token(base, "go", "fresh-token"));
        fixture.exchanges.push(get(
            &second,
            403,
            "application/json",
            error_body("invalid.token", "Token expired"),
        ));
        let resolver = DplayResolver::new(Http::replay(fixture));
        assert!(matches!(
            resolver.resolve(&second_url).await.unwrap_err(),
            ResolveError::LoginRequired { .. }
        ));
    }

    #[tokio::test]
    async fn stored_sessions_are_used_and_checked() {
        let base = "https://us1-prod-direct.watch.foodnetwork.com/";
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            &format!("{base}content/videos/guys-grocery-games-food-network/wild-in-the-aisles"),
            200,
            "application/json",
            record("2152549", "Wild in the Aisles"),
        ));
        fixture.exchanges.push(post(
            &format!("{base}playback/v3/videoPlaybackInfo"),
            200,
            "application/json",
            json!({"data": {"attributes": {"streaming": [{"type": "hls", "url": "https://cdn.test/v/master.m3u8"}]}}}).to_string(),
        ));
        hls_exchanges(&mut fixture);
        fixture.exchanges.push(get(
            &format!("{base}users/me"),
            200,
            "application/json",
            json!({"data": {"id": "u1", "attributes": {"anonymous": false, "username": "nick@example.com"}}}).to_string(),
        ));
        let http = Http::replay(fixture);
        http.with_jar(PLATFORM, |jar| {
            jar.insert(Cookie::new("st", "session-token", ".foodnetwork.com"))
        });
        let resolver = DplayResolver::new(http);
        let url = Url::parse("https://watch.foodnetwork.com/video/guys-grocery-games-food-network/wild-in-the-aisles").unwrap();
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("2152549"));
        assert_eq!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedIn {
                account: "nick@example.com".into()
            }
        );

        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            &format!("{base}users/me"),
            200,
            "application/json",
            json!({"data": {"id": "u2", "attributes": {"anonymous": true}}}).to_string(),
        ));
        let http = Http::replay(fixture);
        http.with_jar(PLATFORM, |jar| {
            jar.insert(Cookie::new("st", "anonymous-token", ".foodnetwork.com"))
        });
        let resolver = DplayResolver::new(http);
        assert_eq!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedOut
        );

        let resolver = DplayResolver::new(Http::replay(Fixture::new(PLATFORM, None)));
        assert_eq!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedOut
        );
    }

    #[test]
    fn examples_are_matched() {
        let resolver = DplayResolver::new(Http::replay(Fixture::new(PLATFORM, None)));
        for example in resolver.platform().examples {
            assert!(resolver.matches(&Url::parse(example).unwrap()), "{example}");
        }
    }
}
