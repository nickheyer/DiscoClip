//! TVer, Japan's catch-up service for the commercial networks. A browser session comes
//! from the platform API's `browser/create` call, giving the platform uid and token every
//! later call carries; an episode's metadata comes from `callEpisode`, its video record
//! from the statics host, and its playback from the STREAKS API, whose per-project key is
//! chosen by the month from the key list the player ships. STREAKS answers with an HLS
//! playlist, refuses addresses outside Japan with a geo error, and names DRM when the
//! sources carry it. A series lists its seasons, each a page of episodes. A short link
//! (`lp`, `corner`, `feature`) is followed to the episode or series it points at.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, SubtitleFormat, SubtitleTrack, Tag, Variant, clean_title, fetch, hls,
    navigation_headers, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::MediaKind;

pub const PLATFORM: &str = "tver";
const PLATFORM_API: &str = "https://platform-api.tver.jp";
const SERVICE_API: &str = "https://service-api.tver.jp";
const STATICS: &str = "https://statics.tver.jp";
const STREAKS_INFO: &str = "https://player.tver.jp/player/streaks_info_v2.json";
const PLAYBACK_API: &str = "https://playback.api.streaks.jp/v1/projects";
const GEO_COUNTRY: &str = "JP";
/// How many episodes a series's playlist holds at most.
const SERIES_LIMIT: usize = 400;

/// `/episodes/{id}`, `/series/{id}`, and the `/lp/`, `/corner/`, `/feature/` short links.
static RE_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^/(?:(lp|corner|feature|series|episodes?)/)+([A-Za-z0-9]+)/?$").unwrap()
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Episode {
        id: String,
    },
    Series {
        id: String,
    },
    /// A landing, corner or feature link, followed to what it points at.
    Short {
        kind: String,
        id: String,
    },
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "tver.jp" && host != "www.tver.jp" {
        return None;
    }
    let caps = RE_PATH.captures(url.path())?;
    let id = caps[2].to_string();
    Some(match &caps[1] {
        "episode" | "episodes" => Link::Episode { id },
        "series" => Link::Series { id },
        kind => Link::Short {
            kind: kind.to_string(),
            id,
        },
    })
}

/// A browser session: the uid and token the platform API hands out, and the STREAKS key
/// per project, kept for the resolver's lifetime once fetched.
#[derive(Debug, Clone, Default)]
struct Session {
    platform_uid: String,
    platform_token: String,
    /// `project id -> the six monthly keys`.
    streaks_keys: HashMap<String, Vec<String>>,
}

fn geo_error(url: &Url) -> ResolveError {
    ResolveError::unavailable(url, format!("available only in {GEO_COUNTRY}"))
}

/// The STREAKS key index for the current month in Tokyo: the month modulo six, or six.
fn key_index(now: Timestamp) -> usize {
    let month = now
        .to_zoned(TimeZone::get("Asia/Tokyo").unwrap_or(TimeZone::UTC))
        .month() as usize;
    match month % 6 {
        0 => 6,
        n => n,
    }
}

/// A STREAKS error body, read for the failure it names.
fn streaks_error(status: u16, body: &str, origin: &Url) -> ResolveError {
    let error: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let code = util::text(&error["code"]).unwrap_or_default();
    let id = util::int(&error["id"]);
    let message = util::text(&error["message"]).unwrap_or_default();
    if code == "REQUEST_FAILED" && id == Some(124) {
        return geo_error(origin);
    }
    if code == "MEDIA_NOT_FOUND" || status == 404 {
        return ResolveError::NotFound(origin.clone());
    }
    if status == 403 {
        // A refusal the player meets only outside Japan, whatever id it carries.
        return geo_error(origin);
    }
    ResolveError::unavailable(
        origin,
        if message.is_empty() {
            format!("STREAKS answered HTTP {status}")
        } else {
            message
        },
    )
}

pub struct TverResolver {
    http: Http,
    session: Mutex<Option<Session>>,
}

impl TverResolver {
    pub fn new(http: Http) -> Self {
        Self {
            http,
            session: Mutex::new(None),
        }
    }

    fn platform_headers() -> Vec<(String, String)> {
        vec![
            ("x-tver-platform-type".to_string(), "web".to_string()),
            ("origin".to_string(), "https://tver.jp".to_string()),
            ("referer".to_string(), "https://tver.jp/".to_string()),
        ]
    }

    /// The browser session, created once and kept.
    async fn session(&self, origin: &Url) -> Result<Session, ResolveError> {
        if let Some(session) = self
            .session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            return Ok(session);
        }
        let create = Url::parse(&format!(
            "{PLATFORM_API}/v2/api/platform_users/browser/create"
        ))
        .expect("valid");
        let response = self
            .http
            .post(create.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .headers(&Self::platform_headers())
            .header("content-type", "application/x-www-form-urlencoded")
            .body("device_type=pc")
            .send()
            .await?;
        if let Some(error) = status_error(response.status, origin) {
            return Err(error);
        }
        let created: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, format!("session JSON: {e}")))?;
        let result = &created["result"];
        let platform_uid = util::text(&result["platform_uid"]).ok_or_else(|| {
            ResolveError::malformed(origin, "the session answer named no platform uid")
        })?;
        let platform_token = util::text(&result["platform_token"]).ok_or_else(|| {
            ResolveError::malformed(origin, "the session answer named no platform token")
        })?;
        let info = fetch(
            &self.http,
            &Url::parse(STREAKS_INFO).expect("valid"),
            PLATFORM,
            BROWSER_UA,
            &[],
            MAX_PAGE,
        )
        .await?;
        let info: Value = info.json(origin)?;
        let mut streaks_keys = HashMap::new();
        for (project, entry) in info.as_object().into_iter().flatten() {
            if let Some(keys) = entry["api_key"].as_object() {
                let mut ordered: Vec<(String, String)> = keys
                    .iter()
                    .filter_map(|(name, value)| {
                        value.as_str().map(|v| (name.clone(), v.to_string()))
                    })
                    .collect();
                ordered.sort_by(|a, b| a.0.cmp(&b.0));
                streaks_keys.insert(
                    project.clone(),
                    ordered.into_iter().map(|(_, value)| value).collect(),
                );
            }
        }
        let session = Session {
            platform_uid,
            platform_token,
            streaks_keys,
        };
        *self.session.lock().unwrap_or_else(|e| e.into_inner()) = Some(session.clone());
        Ok(session)
    }

    /// A platform API call under `/service/api`, carrying the session credentials.
    async fn platform_call(
        &self,
        session: &Session,
        path: &str,
        extra: &[(&str, &str)],
        origin: &Url,
    ) -> Result<Value, ResolveError> {
        let mut query: Vec<(&str, &str)> = vec![
            ("platform_uid", session.platform_uid.as_str()),
            ("platform_token", session.platform_token.as_str()),
        ];
        query.extend_from_slice(extra);
        let url = util::with_query(
            &Url::parse(&format!("{PLATFORM_API}/service/api/{path}")).expect("valid"),
            &query,
        );
        let response = self
            .http
            .get(url)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .headers(&Self::platform_headers())
            .send()
            .await?;
        let status = response.status;
        let text = response.text(MAX_PAGE).await?;
        let json: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        // The platform API answers an unavailable or expired item with 404 and a code
        // naming why, so the body decides before the HTTP status does.
        if let Some(code) = util::int(&json["code"]).filter(|c| *c != 0) {
            let message = util::text(&json["message"]).unwrap_or_default();
            return Err(if code == 70001 || code == 70002 {
                ResolveError::unavailable(
                    origin,
                    if message.is_empty() {
                        "the content is not available".to_string()
                    } else {
                        message
                    },
                )
            } else {
                ResolveError::NotFound(origin.clone())
            });
        }
        if let Some(error) = status_error(status, origin) {
            return Err(error);
        }
        if json.is_null() {
            return Err(ResolveError::malformed(
                origin,
                "the platform API returned no JSON",
            ));
        }
        Ok(json)
    }

    /// The STREAKS playback answer for a media reference in a project.
    async fn streaks(
        &self,
        session: &Session,
        project: &str,
        reference: &str,
        origin: &Url,
    ) -> Result<Value, ResolveError> {
        let keys = session.streaks_keys.get(project).ok_or_else(|| {
            ResolveError::malformed(origin, format!("no STREAKS key for project {project}"))
        })?;
        let index = key_index(Timestamp::now());
        let key = keys
            .get(index.saturating_sub(1))
            .or_else(|| keys.first())
            .ok_or_else(|| ResolveError::malformed(origin, "the project lists no STREAKS keys"))?;
        let url =
            Url::parse(&format!("{PLAYBACK_API}/{project}/medias/{reference}")).expect("valid");
        let response = self
            .http
            .get(url)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "application/json")
            .header("origin", "https://tver.jp")
            .header("referer", "https://tver.jp/")
            .header("x-streaks-api-key", key)
            .send()
            .await?;
        let status = response.status.as_u16();
        let body = response.text(MAX_PAGE).await?;
        if !(200..300).contains(&status) {
            return Err(streaks_error(status, &body, origin));
        }
        serde_json::from_str(&body)
            .map_err(|e| ResolveError::malformed(origin, format!("STREAKS JSON: {e}")))
    }

    async fn resolve_episode(&self, id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let session = self.session(origin).await?;
        let episode_info = self
            .platform_call(
                &session,
                &format!("v1/callEpisode/{id}"),
                &[(
                    "require_data",
                    "mylist,later[epefy106ur],good[epefy106ur],resume[epefy106ur]",
                )],
                origin,
            )
            .await?;
        let content = &episode_info["result"]["episode"]["content"];
        let version = util::text(&content["version"])
            .or_else(|| util::int(&content["version"]).map(|v| v.to_string()))
            .unwrap_or_else(|| "5".to_string());
        let statics = util::with_query(
            &Url::parse(&format!("{STATICS}/content/episode/{id}.json")).expect("valid"),
            &[("v", &version)],
        );
        let video_info = {
            let response = self
                .http
                .get(statics)
                .platform(PLATFORM)
                .user_agent(BROWSER_UA)
                .header("referer", "https://tver.jp/")
                .send()
                .await?;
            if let Some(error) = status_error(response.status, origin) {
                return Err(error);
            }
            let json: Value = response
                .json(MAX_PAGE)
                .await
                .map_err(|e| ResolveError::malformed(origin, format!("video JSON: {e}")))?;
            json
        };
        let streaks = &video_info["streaks"];
        let project = util::text(&streaks["projectID"])
            .ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
        let reference = util::text(&streaks["videoRefID"])
            .map(|r| {
                if r.starts_with("ref:") {
                    r
                } else {
                    format!("ref:{r}")
                }
            })
            .ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
        let playback = self.streaks(&session, &project, &reference, origin).await?;
        let (variants, subtitles, duration, kind) = self.streaks_media(&playback, origin).await?;

        let series =
            util::text(&content["seriesTitle"]).or_else(|| util::text(&video_info["seriesTitle"]));
        let episode = util::text(&content["title"])
            .as_deref()
            .and_then(clean_title);
        let title = match (&series, &episode) {
            (Some(series), Some(episode)) => Some(format!("{series} {episode}")),
            (Some(series), None) => clean_title(series),
            (None, episode) => episode.clone(),
        };
        let mut resolved = Resolved::of(PLATFORM, kind);
        resolved.id = Some(id.to_string());
        resolved.title = title.or_else(|| util::text(&video_info["title"]));
        resolved.description = util::text(&video_info["description"])
            .as_deref()
            .and_then(clean_title);
        resolved.uploader = util::text(&content["productionProviderName"])
            .or_else(|| util::text(&content["broadcasterName"]))
            .as_deref()
            .and_then(clean_title);
        resolved.uploaded_at = util::epoch(&video_info["viewStatus"]["startAt"]);
        resolved.duration = duration.or_else(|| util::seconds(&content["duration"]));
        resolved.thumbnail = Url::parse(&format!(
            "https://statics.tver.jp/images/content/thumbnail/episode/xlarge/{id}.jpg"
        ))
        .ok();
        resolved.webpage_url = Url::parse(&format!("https://tver.jp/episodes/{id}")).ok();
        resolved.subtitles = subtitles;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    /// The variants, subtitles, length and kind a STREAKS playback answer plays as.
    async fn streaks_media(
        &self,
        playback: &Value,
        origin: &Url,
    ) -> Result<
        (
            Vec<Variant>,
            Vec<SubtitleTrack>,
            Option<Duration>,
            MediaKind,
        ),
        ResolveError,
    > {
        let is_live = matches!(playback["type"].as_str(), Some("linear") | Some("live"));
        let mut variants = Vec::new();
        let mut subtitles = Vec::new();
        let mut drm_seen = false;
        let duration = util::seconds(&playback["duration"]);
        for source in playback["sources"].as_array().into_iter().flatten() {
            let Some(src) = util::url_of(&source["src"], None) else {
                continue;
            };
            if source["key_systems"].is_object()
                && source["key_systems"]
                    .as_object()
                    .is_some_and(|o| !o.is_empty())
            {
                drm_seen = true;
                continue;
            }
            let kind = util::text(&source["type"]).unwrap_or_default();
            if !kind.contains("mpegurl") && !src.path().ends_with(".m3u8") {
                continue;
            }
            match hls::expand(&self.http, &src, PLATFORM, BROWSER_UA, &[]).await {
                Ok(expanded) => {
                    for mut stream in expanded.variants {
                        stream.live = is_live;
                        if stream.duration.is_none() {
                            stream.duration = duration.or(expanded.duration);
                        }
                        stream.format_id = Some(match &stream.label {
                            Some(label) => format!("hls-{label}"),
                            None => "hls".to_string(),
                        });
                        if !variants.iter().any(|v: &Variant| v.url == stream.url) {
                            variants.push(stream);
                        }
                    }
                    for track in expanded.subtitles {
                        if !subtitles.iter().any(|t: &SubtitleTrack| t.url == track.url) {
                            subtitles.push(track);
                        }
                    }
                }
                Err(error) => {
                    tracing::debug!(url = %src, "TVer playlist not expanded: {error}");
                }
            }
        }
        for track in playback["tracks"].as_array().into_iter().flatten() {
            let kind = util::text(&track["kind"]).unwrap_or_default();
            if kind != "subtitles" && kind != "captions" {
                continue;
            }
            let Some(url) = util::url_of(&track["src"], None) else {
                continue;
            };
            if subtitles.iter().any(|t| t.url == url) {
                continue;
            }
            subtitles.push(SubtitleTrack {
                url,
                language: util::text(&track["srclang"])
                    .map(|l| l.to_ascii_lowercase())
                    .unwrap_or_else(|| "ja".to_string()),
                name: util::text(&track["label"]),
                format: SubtitleFormat::Vtt,
                auto: false,
                headers: Vec::new(),
            });
        }
        if variants.is_empty() {
            return Err(if drm_seen {
                ResolveError::drm(origin, "STREAKS")
            } else {
                ResolveError::unavailable(origin, "the playback answer named no streams")
            });
        }
        Ok((variants, subtitles, duration, MediaKind::Video))
    }

    async fn resolve_series(&self, id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let session = self.session(origin).await?;
        let series_info = self
            .platform_call(&session, &format!("v2/callSeries/{id}"), &[], origin)
            .await?;
        let title = util::text(&series_info["result"]["content"]["content"]["title"])
            .as_deref()
            .and_then(clean_title);
        let seasons_url =
            Url::parse(&format!("{SERVICE_API}/api/v1/callSeriesSeasons/{id}")).expect("valid");
        let seasons_response = self
            .http
            .get(seasons_url)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .headers(&Self::platform_headers())
            .send()
            .await?;
        if let Some(error) = status_error(seasons_response.status, origin) {
            return Err(error);
        }
        let seasons: Value = seasons_response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, format!("seasons JSON: {e}")))?;
        let mut entries: Vec<PlaylistEntry> = Vec::new();
        'seasons: for season in seasons["result"]["contents"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|c| c["type"].as_str() == Some("season"))
        {
            let Some(season_id) = util::text(&season["content"]["id"]) else {
                continue;
            };
            let episodes = match self
                .platform_call(
                    &session,
                    &format!("v1/callSeasonEpisodes/{season_id}"),
                    &[],
                    origin,
                )
                .await
            {
                Ok(episodes) => episodes,
                Err(error) => {
                    tracing::debug!(season_id, "TVer season not read: {error}");
                    continue;
                }
            };
            for item in episodes["result"]["contents"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|c| c["type"].as_str() == Some("episode"))
            {
                let content = &item["content"];
                let Some(episode_id) = util::text(&content["id"]) else {
                    continue;
                };
                let Some(url) = Url::parse(&format!("https://tver.jp/episodes/{episode_id}")).ok()
                else {
                    continue;
                };
                if entries.iter().any(|e| e.url == url) {
                    continue;
                }
                entries.push(PlaylistEntry {
                    url,
                    title: util::text(&content["title"])
                        .as_deref()
                        .and_then(clean_title),
                    duration: util::seconds(&content["duration"]),
                });
                if entries.len() >= SERIES_LIMIT {
                    break 'seasons;
                }
            }
        }
        if entries.is_empty() {
            return Err(ResolveError::unavailable(
                origin,
                "the series lists no episodes",
            ));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(id.to_string()),
            title,
            total: Some(entries.len()),
            entries,
        }))
    }

    /// A short link's canonical target, followed to the episode or series it names.
    async fn resolve_short(
        &self,
        kind: &str,
        id: &str,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let page = Url::parse(&format!("https://tver.jp/{kind}/{id}")).expect("valid");
        let fetched = fetch(
            &self.http,
            &page,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(fetched.status, origin) {
            return Err(error);
        }
        let html = fetched.text();
        let target =
            canonical_target(&html).ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
        match parse_link(&target) {
            Some(Link::Episode { id }) => self.resolve_episode(&id, origin).await,
            Some(Link::Series { id }) => self.resolve_series(&id, origin).await,
            _ => Err(ResolveError::NotFound(origin.clone())),
        }
    }
}

/// `<link rel="canonical" href="…">` or a `&link=` target: the TVer URL a short link
/// resolves to.
static RE_CANONICAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?:rel="canonical"\s+href|&link)="?(https?://tver\.jp/[^"&]+)"#).unwrap()
});

pub fn canonical_target(html: &str) -> Option<Url> {
    RE_CANONICAL
        .captures(html)
        .and_then(|caps| Url::parse(&caps[1]).ok())
}

#[async_trait]
impl Resolver for TverResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "TVer",
            hosts: &["tver.jp"],
            features: &["episodes", "series", "landing links", "subtitles"],
            formats: &["hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::Video],
            session: SessionSupport::None,
            // TVer restricts every episode's playback to Japan, so a series, whose
            // listing resolves from anywhere, stands as the fixture the scheduled smoke
            // tests resolve.
            examples: &["https://tver.jp/series/srtxft431v"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Episode { id } => self.resolve_episode(&id, url).await,
            Link::Series { id } => self.resolve_series(&id, url).await,
            Link::Short { kind, id } => self.resolve_short(&kind, &id, url).await,
        }
    }
}

#[cfg(test)]
mod tests {
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

    fn post(url: &str, status: u16, body: &str) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: "POST".into(),
                url: url.into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status,
                url: url.into(),
                headers: vec![("content-type".into(), "application/json".into())],
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    const UID: &str = "7dab5eda19d241c49e1e9b727dec2f6ced5f";
    const TOK: &str = "clildxoefb26p1s93ns830xyw5erwgzdkbyl3leq";

    /// The session-creating exchanges, shared by every resolution.
    fn session_exchanges(fixture: &mut Fixture) {
        fixture.exchanges.push(post(
            "https://platform-api.tver.jp/v2/api/platform_users/browser/create",
            200,
            &json!({"code": 0, "result": {"platform_uid": UID, "platform_token": TOK}}).to_string(),
        ));
        fixture.exchanges.push(get(
            STREAKS_INFO,
            200,
            "application/json",
            &json!({
                "tver-ntv": {"api_key": {"key01": "k1", "key02": "k2", "key03": "k3", "key04": "k4", "key05": "k5", "key06": "k6"}}
            })
            .to_string(),
        ));
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://tver.jp/episodes/epc1hdugbk"),
            Some(Link::Episode {
                id: "epc1hdugbk".into()
            })
        );
        assert_eq!(
            link("https://tver.jp/series/srtxft431v/"),
            Some(Link::Series {
                id: "srtxft431v".into()
            })
        );
        assert_eq!(
            link("https://tver.jp/lp/f0033031"),
            Some(Link::Short {
                kind: "lp".into(),
                id: "f0033031".into()
            })
        );
        assert_eq!(
            link("https://tver.jp/corner/f0103888"),
            Some(Link::Short {
                kind: "corner".into(),
                id: "f0103888".into()
            })
        );
        assert!(matches!(
            link("https://tver.jp/feature/fxyz"),
            Some(Link::Short { .. })
        ));
        assert_eq!(link("https://tver.jp/"), None);
        assert_eq!(link("https://tver.jp/mypage"), None);
        assert_eq!(link("https://example.com/episodes/x"), None);
    }

    #[test]
    fn key_index_follows_the_month() {
        let at = |s: &str| key_index(s.parse::<Timestamp>().unwrap());
        // Tokyo is nine hours ahead: this UTC instant is January there.
        assert_eq!(at("2026-01-15T00:00:00Z"), 1);
        assert_eq!(at("2026-06-15T00:00:00Z"), 6);
        assert_eq!(at("2026-07-15T00:00:00Z"), 1);
        assert_eq!(at("2026-12-15T00:00:00Z"), 6);
    }

    #[test]
    fn short_link_targets_are_read() {
        assert_eq!(
            canonical_target(
                r#"<link rel="canonical" href="https://tver.jp/lp/series/sr1rgswqb0"/>"#
            )
            .unwrap()
            .as_str(),
            "https://tver.jp/lp/series/sr1rgswqb0"
        );
        assert_eq!(
            canonical_target(r#"data-x="&link=https://tver.jp/episodes/epxyz&more"#)
                .unwrap()
                .as_str(),
            "https://tver.jp/episodes/epxyz"
        );
        assert!(canonical_target("<html>nothing</html>").is_none());
    }

    #[tokio::test]
    async fn episodes_resolve_through_the_platform_and_streaks_apis() {
        let mut fixture = Fixture::new(PLATFORM, None);
        session_exchanges(&mut fixture);
        fixture.exchanges.push(get(
            "https://platform-api.tver.jp/service/api/v1/callEpisode/epc1hdugbk?platform_uid=7dab5eda19d241c49e1e9b727dec2f6ced5f&platform_token=clildxoefb26p1s93ns830xyw5erwgzdkbyl3leq&require_data=mylist%2Clater%5Bepefy106ur%5D%2Cgood%5Bepefy106ur%5D%2Cresume%5Bepefy106ur%5D",
            200,
            "application/json",
            &json!({"code": 0, "result": {"episode": {"content": {"id": "epc1hdugbk", "version": 16,
                "title": "#2 壮烈！車大騎馬戦", "seriesTitle": "神回だけ見せます！", "duration": 1158,
                "productionProviderName": "日テレ", "broadcasterName": "日テレ"}}}})
            .to_string(),
        ));
        fixture.exchanges.push(get(
            "https://statics.tver.jp/content/episode/epc1hdugbk.json?v=16",
            200,
            "application/json",
            &json!({"id": "epc1hdugbk", "version": 16, "title": "#2 壮烈！車大騎馬戦",
                "description": "幻の神回。", "no": 2, "seriesID": "sru35hwdd2", "seasonID": "ss2lcn4af6",
                "viewStatus": {"startAt": 1651453200, "endAt": 2556111540u64},
                "streaks": {"videoRefID": "baeebeac-a2a6-4dbf-9eb3-c40d59b40068", "mediaID": "47da", "projectID": "tver-ntv"}})
            .to_string(),
        ));
        let key = format!("key0{}", key_index(Timestamp::now()));
        assert!(["key01", "key02", "key03", "key04", "key05", "key06"].contains(&key.as_str()));
        fixture.exchanges.push(get(
            "https://playback.api.streaks.jp/v1/projects/tver-ntv/medias/ref:baeebeac-a2a6-4dbf-9eb3-c40d59b40068",
            200,
            "application/json",
            &json!({"id": "streaks123", "type": "file", "duration": 1158.024,
                "sources": [{"src": "https://streaks.jp/master.m3u8", "type": "application/x-mpegurl"}],
                "tracks": [{"kind": "subtitles", "src": "https://streaks.jp/ja.vtt", "srclang": "JA", "label": "日本語"}]})
            .to_string(),
        ));
        fixture.exchanges.push(get(
            "https://streaks.jp/master.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1280x720,CODECS=\"avc1.4d401f,mp4a.40.2\"\n720.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=1000000,RESOLUTION=640x360,CODECS=\"avc1.4d401e,mp4a.40.2\"\n360.m3u8\n",
        ));
        for name in ["720.m3u8", "360.m3u8"] {
            fixture.exchanges.push(get(
                &format!("https://streaks.jp/{name}"),
                200,
                "application/vnd.apple.mpegurl",
                "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\n0.ts\n#EXT-X-ENDLIST\n",
            ));
        }
        let resolver = TverResolver::new(Http::replay(fixture));
        let url = Url::parse("https://tver.jp/episodes/epc1hdugbk").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("epc1hdugbk"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("神回だけ見せます！ #2 壮烈！車大騎馬戦")
        );
        assert_eq!(resolved.description.as_deref(), Some("幻の神回。"));
        assert_eq!(resolved.uploader.as_deref(), Some("日テレ"));
        assert_eq!(resolved.duration, Some(Duration::from_secs_f64(1158.024)));
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.media, MediaKind::Video);
        assert_eq!(resolved.variants.len(), 2);
        assert_eq!(resolved.variants[0].height, Some(720));
        assert_eq!(resolved.variants[1].height, Some(360));
        assert_eq!(resolved.subtitles.len(), 1);
        assert_eq!(resolved.subtitles[0].language, "ja");
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://statics.tver.jp/images/content/thumbnail/episode/xlarge/epc1hdugbk.jpg"
        );
    }

    #[tokio::test]
    async fn a_geo_gated_episode_is_reported() {
        let mut fixture = Fixture::new(PLATFORM, None);
        session_exchanges(&mut fixture);
        fixture.exchanges.push(get(
            "https://platform-api.tver.jp/service/api/v1/callEpisode/epc1hdugbk?platform_uid=7dab5eda19d241c49e1e9b727dec2f6ced5f&platform_token=clildxoefb26p1s93ns830xyw5erwgzdkbyl3leq&require_data=mylist%2Clater%5Bepefy106ur%5D%2Cgood%5Bepefy106ur%5D%2Cresume%5Bepefy106ur%5D",
            200,
            "application/json",
            &json!({"code": 0, "result": {"episode": {"content": {"id": "epc1hdugbk", "version": 16, "title": "x", "seriesTitle": "y"}}}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://statics.tver.jp/content/episode/epc1hdugbk.json?v=16",
            200,
            "application/json",
            &json!({"id": "epc1hdugbk", "streaks": {"videoRefID": "ref:abc", "projectID": "tver-ntv"}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://playback.api.streaks.jp/v1/projects/tver-ntv/medias/ref:abc",
            403,
            "application/json",
            &json!({"id": 124, "code": "REQUEST_FAILED", "status": 403, "message": "この動画の視聴は許可されていません。"}).to_string(),
        ));
        let resolver = TverResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://tver.jp/episodes/epc1hdugbk").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "available only in JP"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn a_missing_episode_says_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        session_exchanges(&mut fixture);
        fixture.exchanges.push(get(
            "https://platform-api.tver.jp/service/api/v1/callEpisode/epzzzzzzzz?platform_uid=7dab5eda19d241c49e1e9b727dec2f6ced5f&platform_token=clildxoefb26p1s93ns830xyw5erwgzdkbyl3leq&require_data=mylist%2Clater%5Bepefy106ur%5D%2Cgood%5Bepefy106ur%5D%2Cresume%5Bepefy106ur%5D",
            404,
            "application/json",
            &json!({"code": 70001, "message": "このコンテンツは現在表示できません"}).to_string(),
        ));
        let resolver = TverResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://tver.jp/episodes/epzzzzzzzz").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("表示")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn series_list_their_seasons_episodes() {
        let mut fixture = Fixture::new(PLATFORM, None);
        session_exchanges(&mut fixture);
        fixture.exchanges.push(get(
            "https://platform-api.tver.jp/service/api/v2/callSeries/srtxft431v?platform_uid=7dab5eda19d241c49e1e9b727dec2f6ced5f&platform_token=clildxoefb26p1s93ns830xyw5erwgzdkbyl3leq",
            200,
            "application/json",
            &json!({"code": 0, "result": {"content": {"content": {"id": "srtxft431v", "title": "名探偵コナン"}}}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://service-api.tver.jp/api/v1/callSeriesSeasons/srtxft431v",
            200,
            "application/json",
            &json!({"code": 0, "result": {"contents": [
                {"type": "season", "content": {"id": "s0000178", "title": "本編"}},
                {"type": "season", "content": {"id": "ss7e63qkqx", "title": "特番"}}
            ]}})
            .to_string(),
        ));
        fixture.exchanges.push(get(
            "https://platform-api.tver.jp/service/api/v1/callSeasonEpisodes/s0000178?platform_uid=7dab5eda19d241c49e1e9b727dec2f6ced5f&platform_token=clildxoefb26p1s93ns830xyw5erwgzdkbyl3leq",
            200,
            "application/json",
            &json!({"code": 0, "result": {"contents": [
                {"type": "episode", "content": {"id": "epfv7xtphh", "title": "#1213", "duration": 1469}},
                {"type": "episode", "content": {"id": "ep8b33bi4s", "title": "R170", "duration": 1440}}
            ]}})
            .to_string(),
        ));
        fixture.exchanges.push(get(
            "https://platform-api.tver.jp/service/api/v1/callSeasonEpisodes/ss7e63qkqx?platform_uid=7dab5eda19d241c49e1e9b727dec2f6ced5f&platform_token=clildxoefb26p1s93ns830xyw5erwgzdkbyl3leq",
            200,
            "application/json",
            &json!({"code": 0, "result": {"contents": [
                {"type": "episode", "content": {"id": "epspecial1", "title": "特番1", "duration": 3600}}
            ]}})
            .to_string(),
        ));
        let resolver = TverResolver::new(Http::replay(fixture));
        let Resolution::Playlist(series) = resolver
            .resolve(&Url::parse("https://tver.jp/series/srtxft431v").unwrap())
            .await
            .unwrap()
        else {
            panic!("a series is a playlist");
        };
        assert_eq!(series.title.as_deref(), Some("名探偵コナン"));
        assert_eq!(series.id.as_deref(), Some("srtxft431v"));
        assert_eq!(series.entries.len(), 3);
        assert_eq!(series.total, Some(3));
        assert_eq!(
            series.entries[0].url.as_str(),
            "https://tver.jp/episodes/epfv7xtphh"
        );
        assert_eq!(series.entries[0].title.as_deref(), Some("#1213"));
        assert_eq!(series.entries[0].duration, Some(Duration::from_secs(1469)));
        assert_eq!(
            series.entries[2].url.as_str(),
            "https://tver.jp/episodes/epspecial1"
        );
    }

    #[tokio::test]
    async fn short_links_follow_their_canonical_target() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://tver.jp/lp/f0033031",
            200,
            "text/html",
            r#"<html><head><link rel="canonical" href="https://tver.jp/series/sr1rgswqb0"/></head></html>"#,
        ));
        session_exchanges(&mut fixture);
        fixture.exchanges.push(get(
            "https://platform-api.tver.jp/service/api/v2/callSeries/sr1rgswqb0?platform_uid=7dab5eda19d241c49e1e9b727dec2f6ced5f&platform_token=clildxoefb26p1s93ns830xyw5erwgzdkbyl3leq",
            200,
            "application/json",
            &json!({"code": 0, "result": {"content": {"content": {"id": "sr1rgswqb0", "title": "特集シリーズ"}}}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://service-api.tver.jp/api/v1/callSeriesSeasons/sr1rgswqb0",
            200,
            "application/json",
            &json!({"code": 0, "result": {"contents": [{"type": "season", "content": {"id": "se1"}}]}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://platform-api.tver.jp/service/api/v1/callSeasonEpisodes/se1?platform_uid=7dab5eda19d241c49e1e9b727dec2f6ced5f&platform_token=clildxoefb26p1s93ns830xyw5erwgzdkbyl3leq",
            200,
            "application/json",
            &json!({"code": 0, "result": {"contents": [{"type": "episode", "content": {"id": "ep1", "title": "第1話"}}]}}).to_string(),
        ));
        let resolver = TverResolver::new(Http::replay(fixture));
        let Resolution::Playlist(series) = resolver
            .resolve(&Url::parse("https://tver.jp/lp/f0033031").unwrap())
            .await
            .unwrap()
        else {
            panic!("the landing link points at a series");
        };
        assert_eq!(series.title.as_deref(), Some("特集シリーズ"));
        assert_eq!(series.entries.len(), 1);
    }

    /// Every example resolves live: an episode to a playable HLS stream, or the geo gate
    /// from outside Japan, and a series to its episodes.
    #[ignore = "reaches the live site: cargo test -- --ignored"]
    #[tokio::test]

    async fn live_examples_resolve() {
        let resolver = TverResolver::new(Http::new(crate::http::HttpConfig::default()));
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
            assert!(resolver.matches(&url), "{link}");
            match tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&url)).await {
                Ok(Ok(Resolution::Media(resolved))) => {
                    let playable = resolved.variants.iter().filter(|v| v.is_playable()).count();
                    assert!(playable > 0, "{link}: no playable variant");
                    println!(
                        "{link}: {:?} with {playable} variants, title {:?}",
                        resolved.media, resolved.title
                    );
                }
                Ok(Ok(Resolution::Playlist(playlist))) => {
                    assert!(!playlist.entries.is_empty(), "{link}: no entries");
                    println!(
                        "{link}: playlist of {} entries, title {:?}",
                        playlist.entries.len(),
                        playlist.title
                    );
                }
                Ok(Err(ResolveError::Unavailable { reason, .. }))
                    if reason == "available only in JP" =>
                {
                    println!("{link}: geo gate reported");
                }
                Ok(Err(error)) => panic!("{link}: {error}"),
                Err(_) => panic!("{link}: resolution timed out"),
            }
        }
        // An episode resolves to a playable stream in Japan and reports the geo gate
        // elsewhere: either outcome proves the episode path runs end to end.
        let episode = Url::parse("https://tver.jp/episodes/epc1hdugbk").unwrap();
        match tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&episode)).await {
            Ok(Ok(Resolution::Media(resolved))) => {
                let playable = resolved.variants.iter().filter(|v| v.is_playable()).count();
                assert!(playable > 0, "{episode}: no playable variant");
                println!(
                    "{episode}: {:?} with {playable} variants, title {:?}",
                    resolved.media, resolved.title
                );
            }
            Ok(Ok(Resolution::Playlist(_))) => panic!("{episode}: an episode is not a playlist"),
            Ok(Err(ResolveError::Unavailable { reason, .. }))
                if reason == "available only in JP" =>
            {
                println!("{episode}: geo gate reported");
            }
            Ok(Err(error)) => panic!("{episode}: {error}"),
            Err(_) => panic!("{episode}: resolution timed out"),
        }
    }
}
