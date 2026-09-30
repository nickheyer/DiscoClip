//! TED talks: a talk page hands its player the talk in its `__NEXT_DATA__`: an HLS
//! master playlist with every rendition and a subtitle track for every language the talk
//! is translated into, and the MP4 the player falls back to. A playlist page and a series
//! page list their talks in the same data, a series by season. The embed host serves the
//! same talks.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionSupport, SubtitleFormat, SubtitleTrack, Tag, Variant, clean_title, fetch, hls,
    navigation_headers, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "ted";
const SITE: &str = "https://www.ted.com";

static RE_HOST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:www\.|embed(?:-ssl)?\.)?ted\.com$").unwrap());
static RE_SLUG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]+$").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Talk {
        slug: String,
    },
    Playlist {
        id: u64,
    },
    /// A series, or one season of it.
    Series {
        slug: String,
        season: Option<u32>,
    },
}

/// `/talks/{slug}`, `/talks/lang/{code}/{slug}`, `/playlists/{id}/{slug}`,
/// `/series/{slug}#season_{n}`, on the site or the embed host.
pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if !RE_HOST.is_match(&host) {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let slug_of = |s: &str| RE_SLUG.is_match(s).then(|| s.to_string());
    match segments.as_slice() {
        ["talks", "lang", _, slug, ..] => slug_of(slug).map(|slug| Link::Talk { slug }),
        ["talks", slug, ..] if *slug != "lang" => slug_of(slug).map(|slug| Link::Talk { slug }),
        ["playlists", id, ..] => id.parse().ok().map(|id| Link::Playlist { id }),
        ["series", slug, ..] => slug_of(slug).map(|slug| Link::Series {
            slug,
            season: url
                .fragment()
                .and_then(|f| f.strip_prefix("season_"))
                .and_then(|n| n.parse().ok()),
        }),
        _ => None,
    }
}

/// The `__NEXT_DATA__` JSON a page renders from.
pub fn next_data(html: &str) -> Option<Value> {
    let start = html.find("id=\"__NEXT_DATA__\"")?;
    let rest = &html[start..];
    let open = rest.find('>')? + 1;
    let end = rest[open..].find("</script>")? + open;
    serde_json::from_str(&rest[open..end]).ok()
}

/// The player's data on a talk: an object on today's pages, JSON in a string on older
/// ones.
pub fn player_data(talk: &Value) -> Value {
    if talk["videoPlayerData"].is_object() {
        return talk["videoPlayerData"].clone();
    }
    talk["playerData"]
        .as_str()
        .and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(Value::Null)
}

/// A talk's link: its canonical one, else the one its slug makes.
fn talk_url(talk: &Value) -> Option<Url> {
    util::url_of(&talk["canonicalUrl"], None).or_else(|| {
        util::text(&talk["slug"]).and_then(|slug| Url::parse(&format!("{SITE}/talks/{slug}")).ok())
    })
}

fn entry_of(talk: &Value) -> Option<PlaylistEntry> {
    Some(PlaylistEntry {
        url: talk_url(talk)?,
        title: talk["title"].as_str().and_then(clean_title),
        duration: util::seconds(&talk["duration"]),
    })
}

/// The talk's poster without its resize parameters.
fn thumbnail_of(talk: &Value, player: &Value) -> Option<Url> {
    let mut url = util::url_of(&player["thumb"], None).or_else(|| {
        talk["primaryImageSet"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|i| i["aspectRatioName"].as_str() == Some("16x9"))
            .or_else(|| talk["primaryImageSet"].as_array().and_then(|a| a.first()))
            .and_then(|i| util::url_of(&i["url"], None))
    })?;
    url.set_query(None);
    Some(url)
}

/// The subtitle playlist of `language` beside the stream's master playlist:
/// `…/subtitles/{language}.m3u8` with the master's query.
fn subtitle_playlist(stream: &Url, language: &str) -> Option<Url> {
    let mut track = stream.clone();
    let path = stream.path();
    let dir = path.rsplit_once('/').map(|(dir, _)| dir)?;
    track.set_path(&format!("{dir}/subtitles/{language}.m3u8"));
    Some(track)
}

pub struct TedResolver {
    http: Http,
}

impl TedResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    async fn page(&self, page_url: &Url, origin: &Url) -> Result<Value, ResolveError> {
        let fetched = fetch(
            &self.http,
            page_url,
            PLATFORM,
            BROWSER_UA,
            &navigation_headers(),
            MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(fetched.status, origin) {
            return Err(error);
        }
        next_data(&fetched.text())
            .ok_or_else(|| ResolveError::malformed(origin, "the page carries no __NEXT_DATA__"))
    }

    async fn resolve_talk(&self, slug: &str, url: &Url) -> Result<Resolution, ResolveError> {
        let page_url = Url::parse(&format!("{SITE}/talks/{slug}")).expect("valid");
        let data = self.page(&page_url, url).await?;
        let talk = &data["props"]["pageProps"]["videoData"];
        if !talk.is_object() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let player = player_data(talk);
        let duration =
            util::seconds(&talk["duration"]).or_else(|| util::seconds(&player["duration"]));
        let mut variants = Vec::new();
        let mut subtitles = Vec::new();
        let mut failure = None;
        let stream = util::url_of(&talk["hlsUrl"], None)
            .or_else(|| util::url_of(&player["resources"]["hls"]["stream"], None));
        if let Some(stream) = &stream {
            match hls::expand(&self.http, stream, PLATFORM, BROWSER_UA, &[]).await {
                Ok(expanded) => {
                    for mut variant in expanded.variants {
                        variant.format_id = Some(match &variant.label {
                            Some(label) => format!("hls-{label}"),
                            None => "hls".to_string(),
                        });
                        if variant.duration.is_none() {
                            variant.duration = duration;
                        }
                        variants.push(variant);
                    }
                    subtitles = expanded.subtitles;
                }
                Err(error) => {
                    tracing::warn!(%url, "TED stream not read: {error}");
                    failure = Some(error);
                }
            }
            // Every language the talk is translated into has a subtitle playlist beside
            // the master, whether or not the master lists it.
            for language in player["languages"].as_array().into_iter().flatten() {
                let Some(code) = util::text(&language["languageCode"]) else {
                    continue;
                };
                if subtitles
                    .iter()
                    .any(|t| t.language.eq_ignore_ascii_case(&code))
                {
                    continue;
                }
                if let Some(track) = subtitle_playlist(stream, &code) {
                    subtitles.push(SubtitleTrack {
                        url: track,
                        language: code,
                        name: language["languageName"].as_str().and_then(clean_title),
                        format: SubtitleFormat::HlsVtt,
                        auto: false,
                        headers: Vec::new(),
                    });
                }
            }
        }
        for file in player["resources"]["h264"].as_array().into_iter().flatten() {
            let Some(link) = util::url_of(&file["file"], None) else {
                continue;
            };
            let mut variant = Variant::file(link);
            variant.container = Some(Container::Mp4);
            variant.video = Some(VideoCodec::H264);
            variant.audio = Some(AudioCodec::Aac);
            variant.bitrate = util::uint(&file["bitrate"]).map(|k| k * 1000);
            let label = match util::uint(&file["bitrate"]) {
                Some(kbps) => format!("{kbps}k"),
                None => "mp4".to_string(),
            };
            variant.format_id = Some(format!("h264-{label}"));
            variant.label = Some(label);
            variant.duration = duration;
            variants.push(variant);
        }
        if let Some(audio) = util::url_of(&talk["audioDownload"], None) {
            let mut variant = Variant::file(audio);
            variant.audio_only = true;
            variant.container = Some(Container::Mp3);
            variant.audio = Some(AudioCodec::Mp3);
            variant.format_id = Some("audio".to_string());
            variant.label = Some("audio".to_string());
            variant.duration = duration;
            variants.push(variant);
        }
        if variants.is_empty() {
            let external = &player["external"];
            if external["service"]
                .as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case("youtube"))
                && let Some(code) = util::text(&external["code"])
                && let Ok(video) = Url::parse(&format!("https://www.youtube.com/watch?v={code}"))
            {
                return Err(ResolveError::Redirect(video));
            }
            if let Some(external) = util::url_of(&external["uri"], None) {
                return Err(ResolveError::Redirect(external));
            }
            return Err(failure.unwrap_or_else(|| ResolveError::NotFound(url.clone())));
        }
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = util::text(&talk["id"]);
        resolved.title = talk["title"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| player["title"].as_str().and_then(clean_title));
        resolved.description = talk["description"].as_str().and_then(clean_title);
        resolved.uploader = talk["presenterDisplayName"]
            .as_str()
            .and_then(clean_title)
            .or_else(|| player["speaker"].as_str().and_then(clean_title));
        resolved.uploader_url = talk["speakers"]["nodes"]
            .as_array()
            .and_then(|nodes| nodes.first())
            .and_then(|speaker| util::text(&speaker["slug"]))
            .and_then(|slug| Url::parse(&format!("{SITE}/speakers/{slug}")).ok());
        resolved.uploaded_at =
            util::time(&talk["publishedAt"]).or_else(|| util::epoch(&player["published"]));
        resolved.duration = duration.or_else(|| variants.iter().find_map(|v| v.duration));
        resolved.thumbnail = thumbnail_of(talk, &player);
        resolved.webpage_url = talk_url(talk).or(Some(page_url));
        resolved.subtitles = subtitles;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn resolve_playlist(&self, id: u64, url: &Url) -> Result<Resolution, ResolveError> {
        let page_url = Url::parse(&format!("{SITE}/playlists/{id}")).expect("valid");
        let data = self.page(&page_url, url).await?;
        let playlist = &data["props"]["pageProps"]["playlist"];
        if !playlist.is_object() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let entries: Vec<PlaylistEntry> = playlist["videos"]["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(entry_of)
            .collect();
        if entries.is_empty() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: util::text(&playlist["id"]).or(Some(id.to_string())),
            title: playlist["title"].as_str().and_then(clean_title),
            total: util::uint(&playlist["videos"]["totalCount"])
                .map(|n| n as usize)
                .or(Some(entries.len())),
            entries,
        }))
    }

    async fn resolve_series(
        &self,
        slug: &str,
        season: Option<u32>,
        url: &Url,
    ) -> Result<Resolution, ResolveError> {
        let page_url = Url::parse(&format!("{SITE}/series/{slug}")).expect("valid");
        let data = self.page(&page_url, url).await?;
        let props = &data["props"]["pageProps"];
        let series = &props["series"];
        if !series.is_object() {
            return Err(ResolveError::NotFound(url.clone()));
        }
        let mut seasons: Vec<&Value> = props["seasons"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| season.is_none_or(|n| util::uint(&s["seasonNumber"]) == Some(u64::from(n))))
            .collect();
        seasons.sort_by_key(|s| util::uint(&s["seasonNumber"]).unwrap_or(0));
        let mut entries = Vec::new();
        let mut total = 0usize;
        for season in &seasons {
            total += util::uint(&season["videos"]["totalCount"]).unwrap_or(0) as usize;
            entries.extend(
                season["videos"]["nodes"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(entry_of),
            );
        }
        if entries.is_empty() {
            return Err(match season {
                Some(n) => ResolveError::unavailable(url, format!("the series has no season {n}")),
                None => ResolveError::NotFound(url.clone()),
            });
        }
        let name = series["name"].as_str().and_then(clean_title);
        let series_id = util::text(&series["id"]);
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: match (series_id, season) {
                (Some(id), Some(n)) => Some(format!("{id}_{n}")),
                (id, _) => id.or(Some(slug.to_string())),
            },
            title: match (name, season) {
                (Some(name), Some(n)) => Some(format!("{name} Season {n}")),
                (name, _) => name,
            },
            total: Some(total.max(entries.len())),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for TedResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "TED",
            hosts: &["ted.com", "embed.ted.com"],
            features: &["talks", "playlists", "series", "embeds", "subtitles"],
            formats: &["hls", "mp4"],
            media: &[MediaKind::Video],
            tags: &[Tag::Video],
            session: SessionSupport::None,
            examples: &[
                "https://www.ted.com/talks/sir_ken_robinson_do_schools_kill_creativity",
                "https://embed.ted.com/talks/candace_parker_how_to_break_down_barriers_and_not_accept_limits",
                "https://www.ted.com/playlists/171/the_most_popular_talks_of_all",
                "https://www.ted.com/series/small_thing_big_idea",
                "https://www.ted.com/series/the_way_we_work#season_2",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Talk { slug } => self.resolve_talk(&slug, url).await,
            Link::Playlist { id } => self.resolve_playlist(id, url).await,
            Link::Series { slug, season } => self.resolve_series(&slug, season, url).await,
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
    use std::time::Duration;

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

    fn page(data: Value) -> String {
        format!(
            r#"<html><head><title>TED</title></head><body><script id="__NEXT_DATA__" type="application/json">{data}</script></body></html>"#
        )
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let talk = |slug: &str| Some(Link::Talk { slug: slug.into() });
        assert_eq!(
            link("https://www.ted.com/talks/candace_parker_how_to_break_down_barriers"),
            talk("candace_parker_how_to_break_down_barriers")
        );
        assert_eq!(
            link("https://www.ted.com/talks/lang/fr/sir_ken_robinson_do_schools_kill_creativity"),
            talk("sir_ken_robinson_do_schools_kill_creativity")
        );
        assert_eq!(
            link(
                "https://ted.com/talks/sir_ken_robinson_do_schools_kill_creativity/transcript?language=fr"
            ),
            talk("sir_ken_robinson_do_schools_kill_creativity")
        );
        assert_eq!(
            link("https://embed.ted.com/talks/janet_stovall_how_to_get_serious"),
            talk("janet_stovall_how_to_get_serious")
        );
        assert_eq!(
            link("https://embed-ssl.ted.com/talks/janet_stovall_how_to_get_serious"),
            talk("janet_stovall_how_to_get_serious")
        );
        assert_eq!(
            link("https://www.ted.com/playlists/171/the_most_popular_talks_of_all"),
            Some(Link::Playlist { id: 171 })
        );
        assert_eq!(
            link("https://www.ted.com/playlists/171"),
            Some(Link::Playlist { id: 171 })
        );
        assert_eq!(
            link("https://www.ted.com/series/small_thing_big_idea"),
            Some(Link::Series {
                slug: "small_thing_big_idea".into(),
                season: None
            })
        );
        assert_eq!(
            link("https://www.ted.com/series/the_way_we_work#season_2"),
            Some(Link::Series {
                slug: "the_way_we_work".into(),
                season: Some(2)
            })
        );
        assert_eq!(link("https://www.ted.com/talks"), None);
        assert_eq!(link("https://www.ted.com/speakers/candace_parker"), None);
        assert_eq!(link("https://www.ted.com/playlists/not-a-number/x"), None);
        assert_eq!(link("https://ideas.ted.com/6-ways-to-give/"), None);
        assert_eq!(link("https://example.com/talks/x"), None);
    }

    fn talk_page() -> String {
        page(json!({"props": {"pageProps": {"videoData": {
            "id": "86532", "title": "How to break down barriers and not accept limits",
            "slug": "candace_parker_how_to_break_down_barriers_and_not_accept_limits",
            "presenterDisplayName": "Candace Parker", "duration": 679,
            "canonicalUrl": "https://www.ted.com/talks/candace_parker_how_to_break_down_barriers_and_not_accept_limits",
            "publishedAt": "2022-01-14T15:39:45Z", "recordedOn": "2021-12-01",
            "description": "What can't Candace Parker do?",
            "speakers": {"nodes": [{"slug": "candace_parker", "firstname": "Candace"}]},
            "primaryImageSet": [{"url": "https://pi.tedcdn.com/r/x/CandaceParker_2021W-1350x675.jpg?u=1", "aspectRatioName": "2x1"}],
            "hlsUrl": "https://hls.ted.com/project_masters/7506/manifest.m3u8?intro_master_id=9294",
            "videoPlayerData": {
                "id": "416668", "duration": 676, "speaker": "Candace Parker", "published": 1642174785,
                "thumb": "https://pi.tedcdn.com/r/x/CandaceParker_2021W-1350x675.jpg?w=1",
                "external": {"service": "YouTube", "code": "0jNhhrgczsc"},
                "resources": {
                    "hls": {"stream": "https://hls.ted.com/project_masters/7506/manifest.m3u8?intro_master_id=9294&preview"},
                    "h264": [{"bitrate": 1200, "file": "https://py.tedcdn.com/consus/projects/x-fallback-1200k.mp4"}]
                },
                "languages": [
                    {"languageCode": "en", "languageName": "English"},
                    {"languageCode": "fr", "languageName": "French"},
                    {"languageCode": "zh-cn", "languageName": "Chinese, Simplified"}
                ]
            }
        }}}}))
    }

    const MASTER: &str = "#EXTM3U\n#EXT-X-VERSION:4\n#EXT-X-STREAM-INF:BANDWIDTH=1720927,RESOLUTION=1280x720,FRAME-RATE=23.974,CODECS=\"avc1.64001f,mp4a.40.2\",AUDIO=\"audio0\",SUBTITLES=\"subs\"\nindex-f10-v1.m3u8?intro_master_id=9294\n#EXT-X-STREAM-INF:BANDWIDTH=150689,RESOLUTION=320x180,FRAME-RATE=23.974,CODECS=\"avc1.42c00d,mp4a.40.2\",AUDIO=\"audio0\",SUBTITLES=\"subs\"\nindex-f1-v1.m3u8?intro_master_id=9294\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio0\",NAME=\"medium\",AUTOSELECT=YES,DEFAULT=YES,URI=\"index-f8-a1.m3u8?intro_master_id=9294\",LANGUAGE=\"en\"\n#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",LANGUAGE=\"en\",NAME=\"English\",AUTOSELECT=YES,DEFAULT=NO,URI=\"/project_masters/7506/subtitles/en.m3u8?intro_master_id=9294\"\n#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",LANGUAGE=\"fr\",NAME=\"French\",AUTOSELECT=YES,DEFAULT=NO,URI=\"/project_masters/7506/subtitles/fr.m3u8?intro_master_id=9294\"\n";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\n0.ts\n#EXTINF:4.0,\n1.ts\n#EXT-X-ENDLIST\n";

    #[tokio::test]
    async fn talks_resolve_with_renditions_the_mp4_and_every_language() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.ted.com/talks/candace_parker_how_to_break_down_barriers_and_not_accept_limits",
            200,
            "text/html",
            talk_page(),
        ));
        fixture.exchanges.push(get(
            "https://hls.ted.com/project_masters/7506/manifest.m3u8?intro_master_id=9294",
            200,
            "application/vnd.apple.mpegurl",
            MASTER.into(),
        ));
        fixture.exchanges.push(get(
            "https://hls.ted.com/project_masters/7506/index-f10-v1.m3u8?intro_master_id=9294",
            200,
            "application/vnd.apple.mpegurl",
            MEDIA.into(),
        ));
        let resolver = TedResolver::new(Http::replay(fixture));
        let url = Url::parse(
            "https://embed.ted.com/talks/candace_parker_how_to_break_down_barriers_and_not_accept_limits",
        )
        .unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("86532"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("How to break down barriers and not accept limits")
        );
        assert_eq!(resolved.uploader.as_deref(), Some("Candace Parker"));
        assert_eq!(
            resolved.uploader_url.as_ref().map(|u| u.as_str()),
            Some("https://www.ted.com/speakers/candace_parker")
        );
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.duration, Some(Duration::from_secs(679)));
        assert_eq!(
            resolved.thumbnail.as_ref().map(|u| u.as_str()),
            Some("https://pi.tedcdn.com/r/x/CandaceParker_2021W-1350x675.jpg")
        );
        assert_eq!(
            resolved.webpage_url.as_ref().map(|u| u.as_str()),
            Some(
                "https://www.ted.com/talks/candace_parker_how_to_break_down_barriers_and_not_accept_limits"
            )
        );
        assert_eq!(resolved.variants.len(), 3, "two renditions and the MP4");
        assert_eq!(resolved.variants[0].height, Some(720));
        assert_eq!(resolved.variants[0].format_id.as_deref(), Some("hls-720p"));
        assert_eq!(resolved.variants[0].duration, Some(Duration::from_secs(10)));
        let mp4 = &resolved.variants[2];
        assert_eq!(mp4.container, Some(Container::Mp4));
        assert_eq!(mp4.bitrate, Some(1_200_000));
        assert_eq!(mp4.format_id.as_deref(), Some("h264-1200k"));
        assert_eq!(mp4.duration, Some(Duration::from_secs(679)));
        let languages: Vec<&str> = resolved
            .subtitles
            .iter()
            .map(|t| t.language.as_str())
            .collect();
        assert_eq!(languages, ["en", "fr", "zh-cn"]);
        assert!(
            resolved
                .subtitles
                .iter()
                .all(|t| t.format == SubtitleFormat::HlsVtt)
        );
        assert_eq!(
            resolved.subtitles[2].url.as_str(),
            "https://hls.ted.com/project_masters/7506/subtitles/zh-cn.m3u8?intro_master_id=9294"
        );
        assert_eq!(
            resolved.subtitles[2].name.as_deref(),
            Some("Chinese, Simplified")
        );
    }

    #[tokio::test]
    async fn playlists_and_series_list_their_talks() {
        let talk = |id: &str, slug: &str, title: &str| {
            json!({"__typename": "Video", "id": id, "slug": slug, "title": title, "duration": 1151,
                "canonicalUrl": format!("https://www.ted.com/talks/{slug}")})
        };
        let series_page = page(json!({"props": {"pageProps": {
            "series": {"id": "3", "name": "Small Thing Big Idea", "slug": "small_thing_big_idea"},
            "seasons": [
                {"seasonNumber": 2, "videos": {"totalCount": 8, "nodes": [talk("57913", "paola_antonelli_why_pasta", "Why pasta comes in all shapes and sizes")]}},
                {"seasonNumber": 1, "videos": {"totalCount": 8, "nodes": [talk("1", "first_season_talk", "First season")]}}
            ]
        }}}));
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.ted.com/playlists/171",
            200,
            "text/html",
            page(json!({"props": {"pageProps": {"playlist": {
                "id": "171", "slug": "the_most_popular_ted_talks_of_all_time", "title": "The most popular TED Talks of all time",
                "videos": {"totalCount": 25, "nodes": [
                    talk("66", "sir_ken_robinson_do_schools_kill_creativity", "Do schools kill creativity?"),
                    talk("848", "amy_cuddy_your_body_language_may_shape_who_you_are", "Your body language may shape who you are"),
                    {"__typename": "Video", "id": "0", "title": "no link"}
                ]}
            }}}})),
        ));
        fixture.exchanges.push(get(
            "https://www.ted.com/series/small_thing_big_idea",
            200,
            "text/html",
            series_page.clone(),
        ));
        fixture.exchanges.push(get(
            "https://www.ted.com/series/small_thing_big_idea",
            200,
            "text/html",
            series_page,
        ));
        let resolver = TedResolver::new(Http::replay(fixture));
        let Resolution::Playlist(playlist) = resolver
            .resolve(
                &Url::parse("https://www.ted.com/playlists/171/the_most_popular_talks_of_all")
                    .unwrap(),
            )
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(playlist.id.as_deref(), Some("171"));
        assert_eq!(
            playlist.title.as_deref(),
            Some("The most popular TED Talks of all time")
        );
        assert_eq!(playlist.total, Some(25));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://www.ted.com/talks/sir_ken_robinson_do_schools_kill_creativity"
        );
        assert_eq!(
            playlist.entries[0].duration,
            Some(Duration::from_secs(1151))
        );
        let Resolution::Playlist(series) = resolver
            .resolve(&Url::parse("https://www.ted.com/series/small_thing_big_idea").unwrap())
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(series.id.as_deref(), Some("3"));
        assert_eq!(series.title.as_deref(), Some("Small Thing Big Idea"));
        assert_eq!(series.total, Some(16));
        assert_eq!(series.entries.len(), 2);
        assert_eq!(series.entries[0].title.as_deref(), Some("First season"));
        let Resolution::Playlist(season) = resolver
            .resolve(
                &Url::parse("https://www.ted.com/series/small_thing_big_idea#season_2").unwrap(),
            )
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(season.id.as_deref(), Some("3_2"));
        assert_eq!(
            season.title.as_deref(),
            Some("Small Thing Big Idea Season 2")
        );
        assert_eq!(season.total, Some(8));
        assert_eq!(season.entries.len(), 1);
    }

    #[tokio::test]
    async fn missing_talks_and_talks_hosted_elsewhere_say_so() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.ted.com/talks/gone",
            404,
            "text/html",
            "<html>not found</html>".into(),
        ));
        fixture.exchanges.push(get(
            "https://www.ted.com/talks/elsewhere",
            200,
            "text/html",
            page(json!({"props": {"pageProps": {"videoData": {
                "id": "1", "title": "Elsewhere", "slug": "elsewhere",
                "videoPlayerData": {"external": {"service": "YouTube", "code": "0jNhhrgczsc"}, "resources": {"hls": {"stream": ""}, "h264": []}}
            }}}})),
        ));
        fixture.exchanges.push(get(
            "https://www.ted.com/playlists/999",
            200,
            "text/html",
            page(json!({"props": {"pageProps": {"playlist": {"id": "999", "title": "Empty", "videos": {"totalCount": 0, "nodes": []}}}}})),
        ));
        let resolver = TedResolver::new(Http::replay(fixture));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.ted.com/talks/gone").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(&Url::parse("https://www.ted.com/talks/elsewhere").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Redirect(to) if to.as_str() == "https://www.youtube.com/watch?v=0jNhhrgczsc"),
            "{error}"
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.ted.com/playlists/999/empty").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    /// Every example link resolves live: talks with renditions by height and subtitles,
    /// lists with entries.
    #[tokio::test]

    async fn live_examples_resolve() {
        let resolver = TedResolver::new(Http::new(crate::http::HttpConfig::default()));
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
            let resolution = tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&url))
                .await
                .expect("resolution timed out")
                .unwrap();
            match resolution {
                Resolution::Media(resolved) => {
                    assert!(
                        resolved.variants.iter().any(|v| v.is_playable()),
                        "{link}: no playable variant"
                    );
                    assert!(
                        resolved.variants.iter().any(|v| v.height.is_some()),
                        "{link}: no rendition by height"
                    );
                    assert!(!resolved.subtitles.is_empty(), "{link}: no subtitles");
                    assert!(resolved.title.is_some(), "{link}: no title");
                    println!(
                        "{link}: {:?}, {} variants, {} subtitle languages",
                        resolved.title,
                        resolved.variants.len(),
                        resolved.subtitles.len()
                    );
                }
                Resolution::Playlist(playlist) => {
                    assert!(!playlist.entries.is_empty(), "{link}: no entries");
                    println!(
                        "{link}: {:?}, {} entries of {:?}",
                        playlist.title,
                        playlist.entries.len(),
                        playlist.total
                    );
                }
            }
        }
    }
}
