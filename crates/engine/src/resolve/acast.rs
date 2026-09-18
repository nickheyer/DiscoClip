//! Acast podcasts: an episode page, an embed or a `play.acast.com` link names its show
//! and episode, which the feeder API answers with the audio file; a show page lists every
//! episode.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, Tag, Variant, clean_title, fetch, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind};

pub const PLATFORM: &str = "acast";
const API: &str = "https://feeder.acast.com/api/v1/shows/";

/// `/{show}/episodes/{episode}`, `/{show}/{episode}` or `/{show}` on acast.com and its
/// `www`, `shows` and `embed` hosts; the same under `/s/` on play.acast.com.
static RE_PATH: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/([^/?#]+)(?:/(?:episodes/)?([^/?#]+))?/?$").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Episode { show: String, episode: String },
    Show { show: String },
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let path = match host.as_str() {
        "acast.com" | "www.acast.com" | "shows.acast.com" | "embed.acast.com" => url.path(),
        "play.acast.com" => url.path().strip_prefix("/s")?,
        _ => return None,
    };
    let caps = RE_PATH.captures(path)?;
    let show = caps[1].to_string();
    if matches!(
        show.as_str(),
        "episodes" | "s" | "api" | "static" | "about" | "blog"
    ) {
        return None;
    }
    Some(match caps.get(2) {
        Some(episode) if episode.as_str() != "episodes" => Link::Episode {
            show,
            episode: episode.as_str().to_string(),
        },
        _ => Link::Show { show },
    })
}

/// The audio container the feed's content type names.
fn container_of(content_type: Option<&str>) -> Container {
    match content_type.unwrap_or("").to_ascii_lowercase().as_str() {
        "audio/mp4" | "audio/x-m4a" | "audio/aac" => Container::M4a,
        "audio/ogg" => Container::Ogg,
        _ => Container::Mp3,
    }
}

/// The codec the feed's content type names.
fn codec_of(content_type: Option<&str>) -> AudioCodec {
    match content_type.unwrap_or("").to_ascii_lowercase().as_str() {
        "audio/mp4" | "audio/x-m4a" | "audio/aac" => AudioCodec::Aac,
        "audio/ogg" => AudioCodec::Vorbis,
        _ => AudioCodec::Mp3,
    }
}

/// An episode entry's page on shows.acast.com.
fn episode_page(show: &str, episode: &Value) -> Option<Url> {
    util::url_of(&episode["link"], None).or_else(|| {
        let slug = util::text(&episode["episodeUrl"]).or_else(|| util::text(&episode["id"]))?;
        Url::parse(&format!("https://shows.acast.com/{show}/episodes/{slug}")).ok()
    })
}

pub struct AcastResolver {
    http: Http,
}

impl AcastResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    async fn api(
        &self,
        path: &str,
        query: &[(&str, &str)],
        origin: &Url,
    ) -> Result<Value, ResolveError> {
        let api = util::with_query(&Url::parse(&format!("{API}{path}")).expect("valid"), query);
        let accept = [("accept".to_string(), "application/json".to_string())];
        let fetched = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &accept, MAX_PAGE).await?;
        if let Some(error) = status_error(fetched.status, origin) {
            return Err(error);
        }
        fetched.json(origin)
    }

    async fn resolve_episode(
        &self,
        show: &str,
        episode: &str,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let entry = self
            .api(
                &format!("{show}/episodes/{episode}"),
                &[("showInfo", "true")],
                url,
            )
            .await?;
        let audio = util::url_of(&entry["url"], None)
            .map(util::clean_podcast_url)
            .ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let content_type = entry["contentType"].as_str();
        let mut variant = Variant::file(audio);
        variant.audio_only = true;
        variant.container = Some(container_of(content_type));
        variant.audio = Some(codec_of(content_type));
        variant.size = util::uint(&entry["contentLength"]);
        variant.format_id = Some("audio".to_string());
        let mut resolved = Resolved::of(PLATFORM, MediaKind::Audio);
        resolved.id = util::text(&entry["id"]).or_else(|| Some(episode.to_string()));
        resolved.title = entry["title"].as_str().and_then(clean_title);
        resolved.description = entry["description"]
            .as_str()
            .or(entry["summary"].as_str())
            .map(util::clean_html)
            .and_then(|d| clean_title(&d));
        resolved.thumbnail = util::url_of(&entry["image"], None)
            .or_else(|| util::url_of(&entry["show"]["image"], None));
        resolved.uploaded_at = util::time(&entry["publishDate"]);
        resolved.duration = util::seconds(&entry["duration"]);
        resolved.uploader = entry["show"]["title"]
            .as_str()
            .or(entry["show"]["author"].as_str())
            .and_then(clean_title);
        resolved.uploader_url = Url::parse(&format!("https://shows.acast.com/{show}")).ok();
        resolved.webpage_url = episode_page(show, &entry).or_else(|| Some(url.clone()));
        resolved.variants = vec![variant];
        Ok(Resolution::from(resolved))
    }

    async fn resolve_show(&self, show: &str, url: &Url) -> Result<Resolution, ResolveError> {
        let entry = self.api(show, &[], url).await?;
        let episodes = entry["episodes"].as_array().cloned().unwrap_or_default();
        let entries: Vec<PlaylistEntry> = episodes
            .iter()
            .filter_map(|episode| {
                Some(PlaylistEntry {
                    url: episode_page(show, episode)?,
                    title: episode["title"].as_str().and_then(clean_title),
                    duration: util::seconds(&episode["duration"]),
                })
            })
            .collect();
        if entries.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let total = entries.len();
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: util::text(&entry["id"]).or_else(|| Some(show.to_string())),
            title: entry["title"].as_str().and_then(clean_title),
            entries,
            total: Some(total),
        }))
    }
}

#[async_trait]
impl Resolver for AcastResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Acast",
            hosts: &[
                "acast.com",
                "shows.acast.com",
                "play.acast.com",
                "embed.acast.com",
            ],
            features: &["audio", "podcasts", "shows"],
            formats: &["mp3", "m4a"],
            media: &[MediaKind::Audio],
            tags: &[Tag::Podcasts],
            session: SessionSupport::None,
            examples: &[
                "https://shows.acast.com/sparpodcast/episodes/1-mordet-pa-sargonia-dankha-forsvinnandet",
                "https://play.acast.com/s/sparpodcast/6a2fbf52685069f99fec1577",
                "https://shows.acast.com/sparpodcast",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Episode { show, episode } => self.resolve_episode(&show, &episode, url).await,
            Link::Show { show } => self.resolve_show(&show, url).await,
        }
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

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let episode = |show: &str, episode: &str| {
            Some(Link::Episode {
                show: show.into(),
                episode: episode.into(),
            })
        };
        assert_eq!(
            link(
                "https://shows.acast.com/sparpodcast/episodes/2.raggarmordet-rosterurdetforflutna"
            ),
            episode("sparpodcast", "2.raggarmordet-rosterurdetforflutna")
        );
        assert_eq!(
            link("http://embed.acast.com/adambuxton/ep.12-adam-joeschristmaspodcast2015"),
            episode("adambuxton", "ep.12-adam-joeschristmaspodcast2015")
        );
        assert_eq!(
            link("https://play.acast.com/s/rattegangspodden/s04e09styckmordetihelenelund-del2-2"),
            episode("rattegangspodden", "s04e09styckmordetihelenelund-del2-2")
        );
        assert_eq!(
            link("https://www.acast.com/sparpodcast/2.raggarmordet-rosterurdetforflutna"),
            episode("sparpodcast", "2.raggarmordet-rosterurdetforflutna")
        );
        assert_eq!(
            link("https://play.acast.com/s/sparpodcast/2a92b283-1a75-4ad8-8396-499c641de0d9"),
            episode("sparpodcast", "2a92b283-1a75-4ad8-8396-499c641de0d9")
        );
        assert_eq!(
            link("https://www.acast.com/todayinfocus"),
            Some(Link::Show {
                show: "todayinfocus".into()
            })
        );
        assert_eq!(
            link("http://play.acast.com/s/ft-banking-weekly"),
            Some(Link::Show {
                show: "ft-banking-weekly".into()
            })
        );
        assert_eq!(
            link("https://shows.acast.com/sparpodcast/episodes"),
            Some(Link::Show {
                show: "sparpodcast".into()
            })
        );
        assert_eq!(link("https://play.acast.com/sparpodcast"), None);
        assert_eq!(link("https://www.acast.com/"), None);
        assert_eq!(link("https://example.com/sparpodcast/x"), None);
    }

    #[tokio::test]
    async fn episodes_resolve_to_their_audio() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://feeder.acast.com/api/v1/shows/sparpodcast/episodes/1-mordet?showInfo=true",
            200,
            "application/json",
            json!({"id": "6a2fbf52685069f99fec1577", "url": "https://sphinx.acast.com/p/acast/s/sparpodcast/e/6a2f/media.mp3",
                   "contentLength": 45210226, "contentType": "audio/mpeg", "link": "https://shows.acast.com/sparpodcast/episodes/1-mordet",
                   "title": "1. Mordet på Sargonia Dankha", "description": "<p>Appelationsdomstolen i Genua.</p>", "image": "https://assets.pippa.io/shows/6/1.jpeg",
                   "duration": 2825, "publishDate": "2026-06-23T00:00:00.000Z", "season": 22, "episode": 1, "episodeUrl": "1-mordet",
                   "show": {"title": "Spår", "author": "Acast", "image": "https://assets.pippa.io/shows/6/show.jpg"}}).to_string(),
        ));
        let resolver = AcastResolver::new(Http::replay(fixture));
        let url = Url::parse("https://shows.acast.com/sparpodcast/episodes/1-mordet").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("6a2fbf52685069f99fec1577"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("1. Mordet på Sargonia Dankha")
        );
        assert_eq!(
            resolved.description.as_deref(),
            Some("Appelationsdomstolen i Genua.")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("Spår"));
        assert_eq!(resolved.duration, Some(Duration::from_secs(2825)));
        assert_eq!(
            resolved.uploaded_at.map(|t| t.as_second()),
            Some(1782172800)
        );
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://assets.pippa.io/shows/6/1.jpeg"
        );
        assert_eq!(resolved.variants.len(), 1);
        let audio = &resolved.variants[0];
        assert!(audio.audio_only);
        assert_eq!(resolved.media, MediaKind::Audio);
        assert_eq!(audio.container, Some(Container::Mp3));
        assert_eq!(audio.size, Some(45210226));
        assert_eq!(audio.audio, Some(AudioCodec::Mp3));
        assert_eq!(
            audio.url.as_str(),
            "https://sphinx.acast.com/p/acast/s/sparpodcast/e/6a2f/media.mp3"
        );
    }

    #[tokio::test]
    async fn shows_list_their_episodes() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://feeder.acast.com/api/v1/shows/sparpodcast",
            200,
            "application/json",
            json!({"id": "d826bce9", "title": "Spår", "episodes": [
                {"id": "a", "title": "1. Mordet", "duration": 2825, "link": "https://shows.acast.com/sparpodcast/episodes/1-mordet", "episodeUrl": "1-mordet"},
                {"id": "b", "title": "2. Raggarmordet", "duration": 3000, "episodeUrl": "2-raggarmordet"},
                {"title": "no page"}
            ]}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://feeder.acast.com/api/v1/shows/nothing",
            404,
            "application/json",
            json!({"message": "Not found"}).to_string(),
        ));
        let resolver = AcastResolver::new(Http::replay(fixture));
        let Resolution::Playlist(playlist) = resolver
            .resolve(&Url::parse("https://play.acast.com/s/sparpodcast").unwrap())
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(playlist.title.as_deref(), Some("Spår"));
        assert_eq!(playlist.id.as_deref(), Some("d826bce9"));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[1].url.as_str(),
            "https://shows.acast.com/sparpodcast/episodes/2-raggarmordet"
        );
        assert_eq!(
            playlist.entries[0].duration,
            Some(Duration::from_secs(2825))
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.acast.com/nothing").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }
}
