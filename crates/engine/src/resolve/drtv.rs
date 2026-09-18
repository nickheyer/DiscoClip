//! DR TV (dr.dk): episodes and programmes, live channels, seasons and series. Episodes
//! are read through the Massive catalogue API the site's player calls with an anonymous
//! token; live channels through the catalogue's channel items, whose custom fields carry
//! the live HLS masters the player opens; seasons and series through the catalogue's
//! page API.

use std::sync::{LazyLock, Mutex};

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, SubtitleFormat, SubtitleTrack, Variant, clean_title, fetch, geo, hls,
    navigation_headers, page, status_error, util,
};
use crate::http::{BROWSER_UA, Http};

pub const PLATFORM: &str = "drtv";

const CATALOGUE_API: &str = "https://production-cdn.dr-massive.com/api";
const ACCOUNT_API: &str = "https://production.dr-massive.com/api/account";
const TOKEN_API: &str = "https://isl.dr-massive.com/api/authorization/anonymous-sso?device=phone_android&lang=da&supportFallbackToken=true";
const GEO_COUNTRY: &str = "DK";

/// The channel item fields the site's player opens a live channel from, in its order:
/// the plain stream, the one with audio description (DR's "syn"), and the one with
/// Danish subtitles burnt in. A field valued `na` offers nothing.
const LIVE_STREAMS: &[(&str, &str)] = &[
    ("hlsURL", "StandardVideo"),
    ("hlsSynURL", "DRSyn"),
    ("hlsWithSubtitlesURL", "HardcodedSubtitle"),
];

/// The catalogue's names for its subtitle tracks, and the language codes they stand for.
const SUBTITLE_LANGS: &[(&str, &str)] = &[
    ("DanishLanguageSubtitles", "da"),
    ("ForeignLanguageSubtitles", "da_foreign"),
    ("CombinedLanguageSubtitles", "da_combined"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// An episode or programme, by the slug of its page.
    Video(String),
    /// A live channel, by its catalogue item id: the number a `/drtv/kanal/` page ends in.
    Live(String),
    /// A live channel by the slug of its old `/tv/live/` page, which the site redirects
    /// to the channel's `/drtv/kanal/` page.
    LegacyLive(String),
    Season {
        display_id: String,
        id: String,
    },
    Series {
        display_id: String,
        id: String,
    },
}

static RE_TV: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/tv/se(?:/ondemand)?/(?:[^/?#]+/)*([\da-z_-]+)").unwrap());
static RE_DRTV: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/drtv/(?:se|episode|program)/([\da-z_-]+)").unwrap());
static RE_KANAL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/drtv/kanal/(?:[\w-]+_)?(\d+)(?:[/?#]|$)").unwrap());
static RE_LIVE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/(?:tv|TV)/live/([\da-z-]+)").unwrap());
static RE_SEASON: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/drtv/saeson/([\w-]+)_(\d+)").unwrap());
static RE_SERIES: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/drtv/serie/([\w-]+)_(\d+)").unwrap());
static RE_DATA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"window\.__data\s*=").unwrap());

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let dr = host == "dr.dk";
    if !dr && host != "dr-massive.com" {
        return None;
    }
    let path = url.path();
    if let Some(caps) = RE_SEASON.captures(path) {
        return Some(Link::Season {
            display_id: caps[1].to_string(),
            id: caps[2].to_string(),
        });
    }
    if let Some(caps) = RE_SERIES.captures(path) {
        return Some(Link::Series {
            display_id: caps[1].to_string(),
            id: caps[2].to_string(),
        });
    }
    if let Some(caps) = RE_DRTV.captures(path) {
        return Some(Link::Video(caps[1].to_string()));
    }
    if let Some(caps) = RE_KANAL.captures(path) {
        return Some(Link::Live(caps[1].to_string()));
    }
    if dr {
        if let Some(caps) = RE_LIVE.captures(path) {
            return Some(Link::LegacyLive(caps[1].to_string()));
        }
        if let Some(caps) = RE_TV.captures(path) {
            return Some(Link::Video(caps[1].to_string()));
        }
    }
    None
}

/// The item the page's state carries: `cache.page.<any>.item`, or the first entry's.
pub fn page_item(data: &Value) -> Option<&Value> {
    data["cache"]["page"]
        .as_object()?
        .values()
        .find_map(|page| {
            [&page["item"], &page["entries"][0]["item"]]
                .into_iter()
                .find(|item| item.is_object())
        })
}

/// The subtitle format a track's MIME type names; a track without one is WebVTT.
pub fn subtitle_format(mime: Option<&str>) -> Option<SubtitleFormat> {
    let essence = mime
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    Some(match essence.as_str() {
        "" | "text/vtt" | "vtt" => SubtitleFormat::Vtt,
        "application/ttml+xml" | "application/xml" | "text/xml" | "ttml" | "dfxp" => {
            SubtitleFormat::Ttml
        }
        "application/x-subrip" | "text/srt" | "srt" => SubtitleFormat::Srt,
        "text/x-ssa" | "ass" | "ssa" => SubtitleFormat::Ass,
        _ => return None,
    })
}

/// Whether a catalogue flag is set: a boolean, a number, or a non-empty value.
fn flag_set(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Value::String(s) => util::boolean(&Value::String(s.clone())).unwrap_or(!s.is_empty()),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
    }
}

fn series_api(path: &str) -> Url {
    Url::parse(&format!(
        "{CATALOGUE_API}/page?device=web_browser&item_detail_expand=all&lang=da&max_list_prefetch=3&path={path}"
    ))
    .expect("valid")
}

fn geo_error(url: &Url) -> ResolveError {
    ResolveError::unavailable(url, format!("available only in {GEO_COUNTRY}"))
}

fn is_refused(status: u16) -> bool {
    matches!(status, 401 | 403)
}

/// Whether a manifest failure is the CDN refusing the request, as it does addresses
/// outside Denmark.
fn refused_reason(reason: &str) -> bool {
    reason.starts_with("HTTP 401") || reason.starts_with("HTTP 403")
}

/// Names a manifest's variants `<format_id>-<kbps>`, or by position without a bitrate,
/// and labels the ones of an assisted stream with its access service.
fn name_variants(variants: Vec<Variant>, format_id: &str, service: Option<&str>) -> Vec<Variant> {
    variants
        .into_iter()
        .enumerate()
        .map(|(index, mut variant)| {
            variant.format_id = Some(match variant.bitrate {
                Some(bitrate) => format!("{format_id}-{}", bitrate / 1000),
                None => format!("{format_id}-{}", index + 1),
            });
            if let Some(service) = service {
                variant.label = Some(match variant.label.take() {
                    Some(label) => format!("{label} ({service})"),
                    None => service.to_string(),
                });
            }
            variant
        })
        .collect()
}

pub struct DrtvResolver {
    http: Http,
    /// The anonymous catalogue token, issued once and reused.
    token: Mutex<Option<String>>,
}

impl DrtvResolver {
    pub fn new(http: Http) -> Self {
        Self {
            http,
            token: Mutex::new(None),
        }
    }

    /// The anonymous token the account API wants, issued to a random device id.
    async fn token(&self, origin: &Url) -> Result<String, ResolveError> {
        if let Some(token) = self.token.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return Ok(token);
        }
        let response = self
            .http
            .post(Url::parse(TOKEN_API).expect("valid"))
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "application/json")
            .json(&json!({
                "deviceId": util::random_uuid(),
                "scopes": ["Catalog"],
                "optout": true,
            }))
            .send()
            .await?;
        let status = response.status;
        if !status.is_success() {
            return Err(ResolveError::unavailable(
                origin,
                format!("the token service answered HTTP {status}"),
            ));
        }
        let answer: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, format!("token JSON: {e}")))?;
        let token = answer
            .as_array()
            .into_iter()
            .flatten()
            .find(|entry| entry["type"] == "UserAccount")
            .and_then(|entry| entry["value"].as_str())
            .filter(|value| !value.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                ResolveError::malformed(origin, "the token service issued no anonymous token")
            })?;
        *self.token.lock().unwrap_or_else(|e| e.into_inner()) = Some(token.clone());
        Ok(token)
    }

    /// A JSON answer, read as `origin`'s. With `geo_retry`, a refusal is tried once more
    /// from a Danish address, and a second refusal is reported as the geo block it is.
    async fn api(
        &self,
        url: &Url,
        origin: &Url,
        headers: &[(String, String)],
        geo_retry: bool,
    ) -> Result<Value, ResolveError> {
        let mut sent = vec![("accept".to_string(), "application/json".to_string())];
        sent.extend(headers.iter().cloned());
        let mut fetched = fetch(&self.http, url, PLATFORM, BROWSER_UA, &sent, MAX_PAGE).await?;
        if geo_retry && is_refused(fetched.status.as_u16()) {
            if let Some(address) = geo::random_ipv4(GEO_COUNTRY) {
                sent.push(("x-forwarded-for".to_string(), address));
                fetched = fetch(&self.http, url, PLATFORM, BROWSER_UA, &sent, MAX_PAGE).await?;
            }
            if is_refused(fetched.status.as_u16()) {
                return Err(geo_error(origin));
            }
        }
        if let Some(error) = status_error(fetched.status, origin) {
            return Err(error);
        }
        fetched.json(origin)
    }

    /// A catalogue item by its id, expanded whole.
    async fn catalogue_item(&self, id: &str, origin: &Url) -> Result<Value, ResolveError> {
        let api = Url::parse(&format!(
            "{CATALOGUE_API}/items/{id}?device=web_browser&expand=all&ff=idp,ldp,rpt&geoLocation=dk&isDeviceAbroad=false&lang=da&segments=drtv,optedout&sub=Anonymous"
        ))
        .map_err(|e| ResolveError::malformed(origin, e.to_string()))?;
        self.api(&api, origin, &[], false).await
    }

    async fn video(&self, slug: &str, url: &Url) -> Result<Resolution, ResolveError> {
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
        let data = RE_DATA
            .find(&html)
            .and_then(|found| page::leading_json(&html[found.end()..]))
            .map(|(value, _)| value);
        let mut item = data.as_ref().and_then(page_item).cloned();
        let item_id = match item.as_ref().and_then(|item| util::text(&item["id"])) {
            Some(id) => id,
            None => {
                // The page carries no item: the slug ends in the item id, which the
                // catalogue answers for directly.
                let id = slug.rsplit('_').next().unwrap_or(slug).to_string();
                item = Some(self.catalogue_item(&id, url).await?);
                id
            }
        };
        let item = item.unwrap_or(Value::Null);
        let video_id = item["customId"]
            .as_str()
            .and_then(|custom| custom.rsplit(':').next())
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| item_id.clone());

        let token = self.token(url).await?;
        let streams_url = Url::parse(&format!(
            "{ACCOUNT_API}/items/{item_id}/videos?delivery=stream&device=web_browser&ff=idp,ldp,rpt&lang=da&resolution=HD-1080&sub=Anonymous"
        ))
        .map_err(|e| ResolveError::malformed(url, e.to_string()))?;
        let streams = self
            .api(
                &streams_url,
                url,
                &[("authorization".to_string(), format!("Bearer {token}"))],
                false,
            )
            .await?;

        // Standard video streams come first, then plain ones, then the streams with
        // spoken subtitles, sign language or visual interpretation.
        let mut standard = Vec::new();
        let mut plain = Vec::new();
        let mut assisted = Vec::new();
        let mut subtitles: Vec<SubtitleTrack> = Vec::new();
        let mut duration = None;
        let mut live = false;
        for stream in streams.as_array().into_iter().flatten() {
            let Some(manifest) = stream["url"].as_str().and_then(|u| Url::parse(u).ok()) else {
                continue;
            };
            let mut format_id = stream["format"].as_str().unwrap_or("na").to_string();
            let service = stream["accessService"].as_str().unwrap_or("");
            let is_assisted = matches!(
                service,
                "SpokenSubtitles" | "SignLanguage" | "VisuallyInterpreted"
            );
            let mut subtitle_suffix = String::new();
            if is_assisted {
                format_id = format!("{format_id}-{service}");
                subtitle_suffix = format!("-{service}");
            }
            // A manifest that cannot be read only costs its own formats.
            let Ok(expanded) = hls::expand(&self.http, &manifest, PLATFORM, BROWSER_UA, &[]).await
            else {
                continue;
            };
            duration = duration.or(expanded.duration);
            live |= expanded.live;
            let named = name_variants(
                expanded.variants,
                &format_id,
                is_assisted.then_some(service),
            );
            for variant in named {
                if is_assisted {
                    assisted.push(variant);
                } else if service == "StandardVideo" {
                    standard.push(variant);
                } else {
                    plain.push(variant);
                }
            }
            let api_subtitles: Vec<(Url, &Value)> = stream["subtitles"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|track| track.is_object())
                .filter_map(|track| {
                    let link = track["link"].as_str().and_then(|l| Url::parse(l).ok())?;
                    Some((link, track))
                })
                .collect();
            if api_subtitles.is_empty() {
                for track in expanded.subtitles {
                    if !subtitles.iter().any(|known| known.url == track.url) {
                        subtitles.push(track);
                    }
                }
            }
            for (link, track) in api_subtitles {
                let name = track["language"]
                    .as_str()
                    .filter(|l| !l.is_empty())
                    .unwrap_or("da");
                let code = SUBTITLE_LANGS
                    .iter()
                    .find(|(known, _)| *known == name)
                    .map(|(_, code)| *code)
                    .unwrap_or(name);
                let Some(format) = subtitle_format(track["format"].as_str()) else {
                    continue;
                };
                if subtitles.iter().any(|known| known.url == link) {
                    continue;
                }
                subtitles.push(SubtitleTrack {
                    url: link,
                    language: format!("{code}{subtitle_suffix}"),
                    name: Some(name.to_string()),
                    format,
                    auto: false,
                    headers: Vec::new(),
                });
            }
        }
        let variants: Vec<Variant> = standard.into_iter().chain(plain).chain(assisted).collect();
        if variants.is_empty() {
            if flag_set(&item["season"]["customFields"]["IsGeoRestricted"]) {
                return Err(geo_error(url));
            }
            return Err(ResolveError::unavailable(
                url,
                "the catalogue lists no stream for this item",
            ));
        }

        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(video_id);
        resolved.title = item["title"].as_str().and_then(clean_title);
        resolved.description = item["description"].as_str().and_then(clean_title);
        resolved.thumbnail = item["images"]["wallpaper"]
            .as_str()
            .and_then(|u| Url::parse(u).ok());
        resolved.uploaded_at = item["customFields"]["BroadcastTimeDK"]
            .as_str()
            .and_then(util::parse_timestamp);
        resolved.duration = util::seconds(&item["duration"])
            .filter(|d| !d.is_zero())
            .or(duration);
        resolved.webpage_url = Some(fetched.url.clone());
        resolved.live = live;
        resolved.subtitles = subtitles;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    /// A live channel: the catalogue's channel item `id`, whose custom fields name the
    /// HLS masters of its streams. The plain stream's variants come first, then the ones
    /// with audio description and with burnt-in subtitles.
    async fn live(&self, id: &str, url: &Url) -> Result<Resolution, ResolveError> {
        let item = self.catalogue_item(id, url).await?;
        if item["type"].as_str() != Some("channel") {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let title = item["title"]
            .as_str()
            .and_then(clean_title)
            .ok_or_else(|| ResolveError::malformed(url, "the channel has no title"))?;
        let mut variants = Vec::new();
        let mut offered = false;
        let mut geo_refused = false;
        let mut failure = None;
        for (field, service) in LIVE_STREAMS {
            let Some(manifest) = item["customFields"][*field]
                .as_str()
                .filter(|value| !value.is_empty() && *value != "na")
                .and_then(|value| Url::parse(value).ok())
            else {
                continue;
            };
            offered = true;
            let assisted = (*service != "StandardVideo").then_some(*service);
            let format_id = match assisted {
                Some(service) => format!("HLS-{service}"),
                None => "HLS".to_string(),
            };
            match hls::expand(&self.http, &manifest, PLATFORM, BROWSER_UA, &[]).await {
                Ok(expanded) => {
                    for mut variant in name_variants(expanded.variants, &format_id, assisted) {
                        variant.live = true;
                        variants.push(variant);
                    }
                }
                // The channel's CDN refuses addresses outside Denmark.
                Err(ResolveError::Unavailable { reason, .. }) if refused_reason(&reason) => {
                    geo_refused = true;
                }
                Err(error) => failure = failure.or(Some(error)),
            }
        }
        if variants.is_empty() {
            if geo_refused {
                return Err(geo_error(url));
            }
            if let Some(error) = failure {
                return Err(error);
            }
            return Err(ResolveError::unavailable(
                url,
                if offered {
                    "the channel's manifests list no stream"
                } else {
                    "the channel offers no stream"
                },
            ));
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.to_string());
        resolved.title = Some(title);
        resolved.description = item["shortDescription"].as_str().and_then(clean_title);
        resolved.thumbnail = item["images"]["wallpaper"]
            .as_str()
            .and_then(|u| Url::parse(u).ok());
        resolved.webpage_url = item["path"]
            .as_str()
            .and_then(|path| Url::parse(&format!("https://www.dr.dk/drtv{path}")).ok());
        resolved.live = true;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    /// A channel's old `/tv/live/` page, which the site redirects to the channel's
    /// `/drtv/kanal/` page: the page it lands on names the channel item.
    async fn legacy_live(&self, slug: &str, url: &Url) -> Result<Resolution, ResolveError> {
        let page_url = Url::parse(&format!("https://www.dr.dk/tv/live/{slug}"))
            .map_err(|e| ResolveError::malformed(url, e.to_string()))?;
        let fetched = fetch(
            &self.http,
            &page_url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(fetched.status, url) {
            return Err(error);
        }
        match parse_link(&fetched.url) {
            Some(Link::Live(id)) => self.live(&id, url).await,
            _ => Err(ResolveError::NotFound(url.clone())),
        }
    }

    async fn season(
        &self,
        display_id: &str,
        id: &str,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let data = self
            .api(
                &series_api(&format!("/saeson/{display_id}_{id}")),
                url,
                &[],
                true,
            )
            .await?;
        let item = &data["entries"][0]["item"];
        if !item.is_object() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let entries: Vec<PlaylistEntry> = item["episodes"]["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|episode| {
                let path = episode["path"].as_str()?;
                Some(PlaylistEntry {
                    url: Url::parse(&format!("https://www.dr.dk/drtv{path}")).ok()?,
                    title: episode["title"].as_str().and_then(clean_title),
                    duration: util::seconds(&episode["duration"]).filter(|d| !d.is_zero()),
                })
            })
            .collect();
        if entries.is_empty() {
            return Err(ResolveError::unavailable(
                url,
                "the season lists no episodes",
            ));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(id.to_string()),
            title: item["title"].as_str().and_then(clean_title),
            total: Some(entries.len()),
            entries,
        }))
    }

    async fn series(
        &self,
        display_id: &str,
        id: &str,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let data = self
            .api(
                &series_api(&format!("/serie/{display_id}_{id}")),
                url,
                &[],
                true,
            )
            .await?;
        let item = &data["entries"][0]["item"];
        if !item.is_object() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let entries: Vec<PlaylistEntry> = item["show"]["seasons"]["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|season| {
                let path = season["path"].as_str()?;
                Some(PlaylistEntry {
                    url: Url::parse(&format!("https://www.dr.dk/drtv{path}")).ok()?,
                    title: season["title"].as_str().and_then(clean_title),
                    duration: None,
                })
            })
            .collect();
        if entries.is_empty() {
            return Err(ResolveError::unavailable(
                url,
                "the series lists no seasons",
            ));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(id.to_string()),
            title: item["title"].as_str().and_then(clean_title),
            total: Some(entries.len()),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for DrtvResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "DR TV",
            hosts: &["dr.dk", "dr-massive.com"],
            features: &["videos", "live", "seasons", "series"],
            formats: &["hls"],
            session: SessionSupport::None,
            examples: &[
                "https://www.dr.dk/drtv/se/frank-and-kastaniegaarden_71769",
                "https://www.dr.dk/drtv/se/spise-med-price_-pasta-selv_397445",
                "https://www.dr.dk/drtv/kanal/tva-live_192099",
                "https://www.dr.dk/drtv/saeson/frank-and-kastaniegaarden_9008",
                "https://www.dr.dk/drtv/serie/frank-and-kastaniegaarden_6954",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Video(slug) => self.video(&slug, url).await,
            Link::Live(id) => self.live(&id, url).await,
            Link::LegacyLive(slug) => self.legacy_live(&slug, url).await,
            Link::Season { display_id, id } => self.season(&display_id, &id, url).await,
            Link::Series { display_id, id } => self.series(&display_id, &id, url).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};
    use std::time::Duration;

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

    const MASTER: &str = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1280x720,CODECS=\"avc1.64001f,mp4a.40.2\"\nhttps://cdn.dr.dk/v/720.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=640x360,CODECS=\"avc1.64001e,mp4a.40.2\"\nhttps://cdn.dr.dk/v/360.m3u8\n";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10.0,\n0.ts\n#EXTINF:6.0,\n1.ts\n#EXT-X-ENDLIST\n";
    const LIVE_MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\n0.ts\n";

    fn token_exchange() -> Exchange {
        exchange(
            "POST",
            TOKEN_API,
            200,
            "application/json",
            json!([
                {"type": "Anonymous", "value": "anon-token"},
                {"type": "UserAccount", "value": "account-token"}
            ])
            .to_string(),
        )
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://www.dr.dk/tv/se/boern/ultra/klassen-ultra/klassen-darlig-taber-10"),
            Some(Link::Video("klassen-darlig-taber-10".into()))
        );
        assert_eq!(
            link("https://www.dr.dk/tv/se/historien-om-danmark/-/historien-om-danmark-stenalder"),
            Some(Link::Video("historien-om-danmark-stenalder".into()))
        );
        assert_eq!(
            link("https://www.dr.dk/tv/se/ondemand/klassen/klassen-1"),
            Some(Link::Video("klassen-1".into()))
        );
        assert_eq!(
            link("https://www.dr.dk/drtv/se/frank-and-kastaniegaarden_71769"),
            Some(Link::Video("frank-and-kastaniegaarden_71769".into()))
        );
        assert_eq!(
            link("https://www.dr.dk/drtv/se/spise-med-price_-pasta-selv_397445"),
            Some(Link::Video("spise-med-price_-pasta-selv_397445".into()))
        );
        assert_eq!(
            link("https://www.dr.dk/drtv/episode/bonderoeven_71769"),
            Some(Link::Video("bonderoeven_71769".into()))
        );
        assert_eq!(
            link("https://dr-massive.com/drtv/se/bonderoeven_71769"),
            Some(Link::Video("bonderoeven_71769".into()))
        );
        assert_eq!(
            link("https://www.dr.dk/drtv/program/jagten_220924"),
            Some(Link::Video("jagten_220924".into()))
        );
        assert_eq!(
            link("https://www.dr.dk/drtv/kanal/tva-live_192099"),
            Some(Link::Live("192099".into()))
        );
        assert_eq!(
            link("https://www.dr.dk/drtv/kanal/dr-ramasjang_20892?x=1"),
            Some(Link::Live("20892".into()))
        );
        assert_eq!(
            link("https://www.dr.dk/drtv/kanal/192099"),
            Some(Link::Live("192099".into()))
        );
        assert_eq!(
            link("https://dr-massive.com/drtv/kanal/dr1_20875"),
            Some(Link::Live("20875".into()))
        );
        assert_eq!(link("https://www.dr.dk/drtv/kanal/dr1"), None);
        assert_eq!(
            link("https://www.dr.dk/tv/live/dr1"),
            Some(Link::LegacyLive("dr1".into()))
        );
        assert_eq!(
            link("https://www.dr.dk/TV/live/dr2"),
            Some(Link::LegacyLive("dr2".into()))
        );
        assert_eq!(
            link("https://www.dr.dk/drtv/saeson/frank-and-kastaniegaarden_9008"),
            Some(Link::Season {
                display_id: "frank-and-kastaniegaarden".into(),
                id: "9008".into()
            })
        );
        assert_eq!(
            link("https://dr-massive.com/drtv/serie/frank-and-kastaniegaarden_6954"),
            Some(Link::Series {
                display_id: "frank-and-kastaniegaarden".into(),
                id: "6954".into()
            })
        );
        assert_eq!(link("https://dr-massive.com/tv/live/dr1"), None);
        assert_eq!(link("https://www.dr.dk/nyheder/indland"), None);
        assert_eq!(link("https://www.dr.dk/drtv/"), None);
        assert_eq!(link("https://example.com/drtv/se/x_1"), None);
    }

    #[test]
    fn subtitle_formats_are_named_by_mime() {
        assert_eq!(subtitle_format(None), Some(SubtitleFormat::Vtt));
        assert_eq!(subtitle_format(Some("text/vtt")), Some(SubtitleFormat::Vtt));
        assert_eq!(
            subtitle_format(Some("application/ttml+xml")),
            Some(SubtitleFormat::Ttml)
        );
        assert_eq!(subtitle_format(Some("image/png")), None);
    }

    #[tokio::test]
    async fn episodes_resolve_from_the_page_state() {
        let page_url = "https://www.dr.dk/drtv/se/frank-and-kastaniegaarden_71769";
        let state = json!({"cache": {"page": {"/se/frank-and-kastaniegaarden_71769": {"item": {
            "id": 71769,
            "customId": "urn:dr:mu:programcard:00951930010",
            "title": "Frank & Kastaniegaarden",
            "description": "Frank flytter ind.",
            "duration": 2576,
            "images": {"wallpaper": "https://images.dr.dk/frank.jpg"},
            "customFields": {"BroadcastTimeDK": "2019-01-03T21:00:00+01:00"},
            "season": {"title": "Frank & Kastaniegaarden", "show": {"title": "Frank & Kastaniegaarden"}}
        }}}}});
        let html = format!(
            "<html><head><title>DRTV</title></head><body><script>window.__data = {};</script></body></html>",
            state
        );
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture
            .exchanges
            .push(get(page_url, 200, "text/html", html));
        fixture.exchanges.push(token_exchange());
        fixture.exchanges.push(get(
            "https://production.dr-massive.com/api/account/items/71769/videos?delivery=stream&device=web_browser&ff=idp,ldp,rpt&lang=da&resolution=HD-1080&sub=Anonymous",
            200,
            "application/json",
            json!([
                {"url": "https://cdn.dr.dk/v/master.m3u8", "format": "HLS", "accessService": "StandardVideo",
                 "subtitles": [
                    {"link": "https://cdn.dr.dk/s/da.vtt", "language": "DanishLanguageSubtitles", "format": "text/vtt"},
                    {"link": "https://cdn.dr.dk/s/foreign.vtt", "language": "ForeignLanguageSubtitles", "format": "text/vtt"}
                 ]},
                {"url": "https://cdn.dr.dk/v/sign.m3u8", "format": "HLS", "accessService": "SignLanguage", "subtitles": []},
                {"url": "", "format": "HLS"}
            ])
            .to_string(),
        ));
        fixture.exchanges.push(get(
            "https://cdn.dr.dk/v/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MASTER.into(),
        ));
        fixture.exchanges.push(get(
            "https://cdn.dr.dk/v/720.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MEDIA.into(),
        ));
        fixture.exchanges.push(get(
            "https://cdn.dr.dk/v/sign.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MEDIA.into(),
        ));
        let resolver = DrtvResolver::new(Http::replay(fixture));
        let url = Url::parse(page_url).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("00951930010"));
        assert_eq!(resolved.title.as_deref(), Some("Frank & Kastaniegaarden"));
        assert_eq!(resolved.description.as_deref(), Some("Frank flytter ind."));
        assert_eq!(resolved.duration, Some(Duration::from_secs(2576)));
        assert_eq!(
            resolved.uploaded_at.unwrap().as_second(),
            1546545600,
            "BroadcastTimeDK is read as the release time"
        );
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://images.dr.dk/frank.jpg"
        );
        assert!(!resolved.live);
        assert_eq!(resolved.variants.len(), 3);
        assert_eq!(resolved.variants[0].height, Some(720));
        assert_eq!(resolved.variants[0].format_id.as_deref(), Some("HLS-3000"));
        assert_eq!(resolved.variants[1].height, Some(360));
        let sign = &resolved.variants[2];
        assert_eq!(sign.format_id.as_deref(), Some("HLS-SignLanguage-1"));
        assert_eq!(sign.label.as_deref(), Some("SignLanguage"));
        assert_eq!(resolved.subtitles.len(), 2);
        assert_eq!(resolved.subtitles[0].language, "da");
        assert_eq!(resolved.subtitles[0].format, SubtitleFormat::Vtt);
        assert_eq!(resolved.subtitles[1].language, "da_foreign");
        assert_eq!(
            resolved.subtitles[1].url.as_str(),
            "https://cdn.dr.dk/s/foreign.vtt"
        );
    }

    #[tokio::test]
    async fn episodes_without_page_state_use_the_catalogue_and_report_geo_blocks() {
        let page_url = "https://www.dr.dk/drtv/episode/bonderoeven_71769";
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            page_url,
            200,
            "text/html",
            "<html><body>shell</body></html>".into(),
        ));
        fixture.exchanges.push(get(
            "https://production-cdn.dr-massive.com/api/items/71769?device=web_browser&expand=all&ff=idp,ldp,rpt&geoLocation=dk&isDeviceAbroad=false&lang=da&segments=drtv,optedout&sub=Anonymous",
            200,
            "application/json",
            json!({"id": 71769, "title": "Bonderøven", "season": {"customFields": {"IsGeoRestricted": true}}}).to_string(),
        ));
        fixture.exchanges.push(token_exchange());
        fixture.exchanges.push(get(
            "https://production.dr-massive.com/api/account/items/71769/videos?delivery=stream&device=web_browser&ff=idp,ldp,rpt&lang=da&resolution=HD-1080&sub=Anonymous",
            200,
            "application/json",
            json!([]).to_string(),
        ));
        let resolver = DrtvResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse(page_url).unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "available only in DK"),
            "{error}"
        );

        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.dr.dk/drtv/se/gone_1",
            404,
            "text/html",
            "<html>not found</html>".into(),
        ));
        let resolver = DrtvResolver::new(Http::replay(fixture));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.dr.dk/drtv/se/gone_1").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    fn channel_item_api(id: &str) -> String {
        format!(
            "https://production-cdn.dr-massive.com/api/items/{id}?device=web_browser&expand=all&ff=idp,ldp,rpt&geoLocation=dk&isDeviceAbroad=false&lang=da&segments=drtv,optedout&sub=Anonymous"
        )
    }

    /// A channel item as the catalogue answers it: the live masters sit in custom
    /// fields, and a field valued `na` offers nothing.
    fn channel_item(id: u64, short_code: &str, title: &str, fields: Value) -> Value {
        json!({
            "id": id,
            "type": "channel",
            "customId": short_code,
            "channelShortCode": short_code,
            "title": title,
            "path": format!("/kanal/{}_{id}", title.to_ascii_lowercase().replace(' ', "-")),
            "shortDescription": format!("Klik ind og se {title} Live her på DRTV."),
            "description": "Ingen beskrivelse",
            "images": {"wallpaper": format!("https://images.dr.dk/{short_code}/wallpaper.png"),
                       "logo": format!("https://images.dr.dk/{short_code}/logo.png")},
            "customFields": fields
        })
    }

    #[tokio::test]
    async fn live_channels_resolve_from_the_catalogue_item() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            &channel_item_api("192099"),
            200,
            "application/json",
            channel_item(192099, "TVA", "TVA Live", json!({
                "hlsURL": "https://drlivedrtvahls.akamaized.net/hls/live/2113613/drlivedrtva/master.m3u8",
                "hlsAlternativeURL": "https://drlivedrtvahls.akamaized.net/hls/live/2113613-b/drlivedrtva/master.m3u8",
                "hlsSynURL": "https://drtvasynhls.akamaized.net/hls/live/2040938/drtvasyn/master.m3u8",
                "hlsWithSubtitlesURL": "na",
                "dashURL": "https://drlivedrtvadash.akamaized.net/dash/live/2113619/drlivedrtva/manifest.mpd"
            }))
            .to_string(),
        ));
        fixture.exchanges.push(get(
            "https://drlivedrtvahls.akamaized.net/hls/live/2113613/drlivedrtva/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MASTER.into(),
        ));
        fixture.exchanges.push(get(
            "https://cdn.dr.dk/v/720.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            LIVE_MEDIA.into(),
        ));
        fixture.exchanges.push(get(
            "https://drtvasynhls.akamaized.net/hls/live/2040938/drtvasyn/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            LIVE_MEDIA.into(),
        ));
        let resolver = DrtvResolver::new(Http::replay(fixture));
        let url = Url::parse("https://www.dr.dk/drtv/kanal/tva-live_192099").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("192099"));
        assert_eq!(resolved.title.as_deref(), Some("TVA Live"));
        assert_eq!(
            resolved.description.as_deref(),
            Some("Klik ind og se TVA Live Live her på DRTV.")
        );
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://images.dr.dk/TVA/wallpaper.png"
        );
        assert_eq!(
            resolved.webpage_url.as_ref().unwrap().as_str(),
            "https://www.dr.dk/drtv/kanal/tva-live_192099"
        );
        assert!(resolved.live);
        assert_eq!(resolved.variants.len(), 3);
        assert!(resolved.variants.iter().all(|v| v.live));
        assert_eq!(resolved.variants[0].format_id.as_deref(), Some("HLS-3000"));
        assert_eq!(resolved.variants[0].height, Some(720));
        assert_eq!(resolved.variants[0].label.as_deref(), Some("720p"));
        assert_eq!(resolved.variants[1].format_id.as_deref(), Some("HLS-1000"));
        let syn = &resolved.variants[2];
        assert_eq!(syn.format_id.as_deref(), Some("HLS-DRSyn-1"));
        assert_eq!(syn.label.as_deref(), Some("DRSyn"));
        assert_eq!(
            syn.url.as_str(),
            "https://drtvasynhls.akamaized.net/hls/live/2040938/drtvasyn/master.m3u8"
        );
    }

    #[tokio::test]
    async fn old_live_links_follow_the_site_to_the_channel_page() {
        let legacy = "https://www.dr.dk/tv/live/dr1";
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(Exchange {
            request: RecordedRequest {
                method: "GET".into(),
                url: legacy.into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status: 200,
                url: "https://www.dr.dk/drtv/kanal/dr1_20875".into(),
                headers: vec![("content-type".into(), "text/html".into())],
                body: RecordedBody::Text("<html><body>DRTV</body></html>".into()),
                truncated: false,
            },
        });
        fixture.exchanges.push(get(
            &channel_item_api("20875"),
            200,
            "application/json",
            channel_item(20875, "DR1", "DR1", json!({
                "hlsURL": "https://drlivedr1hls.akamaized.net/hls/live/2113625/drlivedr1/master.m3u8",
                "hlsSynURL": "na",
                "hlsWithSubtitlesURL": "na"
            }))
            .to_string(),
        ));
        fixture.exchanges.push(get(
            "https://drlivedr1hls.akamaized.net/hls/live/2113625/drlivedr1/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            MASTER.into(),
        ));
        fixture.exchanges.push(get(
            "https://cdn.dr.dk/v/720.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            LIVE_MEDIA.into(),
        ));
        let resolver = DrtvResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse(legacy).unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("20875"));
        assert_eq!(resolved.title.as_deref(), Some("DR1"));
        assert_eq!(
            resolved.webpage_url.as_ref().unwrap().as_str(),
            "https://www.dr.dk/drtv/kanal/dr1_20875"
        );
        assert!(resolved.live);
        assert_eq!(resolved.variants.len(), 2);

        // A slug the site no longer knows lands on a page that is not a channel's.
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(Exchange {
            request: RecordedRequest {
                method: "GET".into(),
                url: "https://www.dr.dk/tv/live/dr3".into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status: 200,
                url: "https://www.dr.dk/drtv/".into(),
                headers: vec![("content-type".into(), "text/html".into())],
                body: RecordedBody::Text("<html><body>DRTV</body></html>".into()),
                truncated: false,
            },
        });
        let resolver = DrtvResolver::new(Http::replay(fixture));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.dr.dk/tv/live/dr3").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn channels_refused_by_the_cdn_are_geo_blocked_and_other_items_are_not_channels() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            &channel_item_api("20876"),
            200,
            "application/json",
            channel_item(20876, "DR2", "DR2", json!({
                "hlsURL": "https://drlivedr2hls.akamaized.net/hls/live/2113623/drlivedr2/master.m3u8",
                "hlsSynURL": "https://dr2synhls.akamaized.net/hls/live/2040939/dr2syn/master.m3u8",
                "hlsWithSubtitlesURL": "na"
            }))
            .to_string(),
        ));
        fixture.exchanges.push(get(
            "https://drlivedr2hls.akamaized.net/hls/live/2113623/drlivedr2/master.m3u8",
            403,
            "text/html",
            "<HTML><HEAD><TITLE>Access Denied</TITLE></HEAD></HTML>".into(),
        ));
        fixture.exchanges.push(get(
            "https://dr2synhls.akamaized.net/hls/live/2040939/dr2syn/master.m3u8",
            403,
            "text/html",
            "<HTML><HEAD><TITLE>Access Denied</TITLE></HEAD></HTML>".into(),
        ));
        let resolver = DrtvResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://www.dr.dk/drtv/kanal/dr2_20876").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "available only in DK"),
            "{error}"
        );

        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            &channel_item_api("71769"),
            200,
            "application/json",
            json!({"id": 71769, "type": "episode", "title": "Frank & Kastaniegaarden"}).to_string(),
        ));
        fixture.exchanges.push(get(
            &channel_item_api("1"),
            200,
            "application/json",
            channel_item(1, "X", "Silent", json!({"hlsURL": "na"})).to_string(),
        ));
        let resolver = DrtvResolver::new(Http::replay(fixture));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.dr.dk/drtv/kanal/71769").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(&Url::parse("https://www.dr.dk/drtv/kanal/silent_1").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "the channel offers no stream"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn seasons_and_series_list_their_parts() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://production-cdn.dr-massive.com/api/page?device=web_browser&item_detail_expand=all&lang=da&max_list_prefetch=3&path=/saeson/frank-and-kastaniegaarden_9008",
            200,
            "application/json",
            json!({"entries": [{"item": {
                "title": "Frank & Kastaniegaarden",
                "seasonNumber": 2008,
                "episodes": {"items": [
                    {"path": "/se/frank-and-kastaniegaarden_71769", "title": "Episode 1", "duration": 2576},
                    {"path": "/se/frank-and-kastaniegaarden_71770", "title": "Episode 2"},
                    {"title": "no path"}
                ]}
            }}]})
            .to_string(),
        ));
        fixture.exchanges.push(get(
            "https://production-cdn.dr-massive.com/api/page?device=web_browser&item_detail_expand=all&lang=da&max_list_prefetch=3&path=/serie/frank-and-kastaniegaarden_6954",
            200,
            "application/json",
            json!({"entries": [{"item": {
                "title": "Frank & Kastaniegaarden",
                "show": {"seasons": {"items": [
                    {"path": "/saeson/frank-and-kastaniegaarden_9008", "title": "Season 2008"},
                    {"path": "/saeson/frank-and-kastaniegaarden_8761", "title": "Season 2009"}
                ]}}
            }}]})
            .to_string(),
        ));
        let resolver = DrtvResolver::new(Http::replay(fixture));
        let season = resolver
            .resolve(
                &Url::parse("https://www.dr.dk/drtv/saeson/frank-and-kastaniegaarden_9008")
                    .unwrap(),
            )
            .await
            .unwrap();
        let Resolution::Playlist(season) = season else {
            panic!("a season is a playlist");
        };
        assert_eq!(season.id.as_deref(), Some("9008"));
        assert_eq!(season.title.as_deref(), Some("Frank & Kastaniegaarden"));
        assert_eq!(season.entries.len(), 2);
        assert_eq!(season.total, Some(2));
        assert_eq!(
            season.entries[0].url.as_str(),
            "https://www.dr.dk/drtv/se/frank-and-kastaniegaarden_71769"
        );
        assert_eq!(season.entries[0].duration, Some(Duration::from_secs(2576)));
        assert!(resolver.matches(&season.entries[0].url));

        let series = resolver
            .resolve(
                &Url::parse("https://www.dr.dk/drtv/serie/frank-and-kastaniegaarden_6954").unwrap(),
            )
            .await
            .unwrap();
        let Resolution::Playlist(series) = series else {
            panic!("a series is a playlist");
        };
        assert_eq!(series.id.as_deref(), Some("6954"));
        assert_eq!(series.entries.len(), 2);
        assert_eq!(
            series.entries[1].url.as_str(),
            "https://www.dr.dk/drtv/saeson/frank-and-kastaniegaarden_8761"
        );
        assert_eq!(series.entries[1].title.as_deref(), Some("Season 2009"));
        assert!(resolver.matches(&series.entries[1].url));
    }
}
