//! France Télévisions: a france.tv page names its video in the player options of its
//! Next.js data, and a franceinfo page in the id of its player wrapper. The player API at
//! k7.ftven.fr describes a video twice, with the DASH manifest it gives a desktop browser
//! and the HLS playlist it gives a mobile one, each signed through the hdfauth token
//! service before it plays. Live channels come the same way, and a programme page lists
//! its episodes.

use std::sync::LazyLock;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionSupport, SubtitleFormat, SubtitleTrack, Tag, clean_title, dash, fetch, hls,
    navigation_headers, status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::MediaKind;

pub const PLATFORM: &str = "francetv";
const PLAYER_API: &str = "https://k7.ftven.fr/videos/";
/// How many episodes a programme page is read up to.
const LISTING_LIMIT: usize = 100;

const UUID: &str = r"[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}";
static RE_UUID: LazyLock<Regex> = LazyLock::new(|| Regex::new(&format!("^{UUID}$")).unwrap());
/// `"options":{"id":"<uuid>"` in the page's Next.js data, escaped or not.
static RE_OPTIONS_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(&format!(
        r#"\\?"options\\?":\s*\{{\s*\\?"id\\?":\s*\\?"({UUID})"#
    ))
    .unwrap()
});
/// The ways an article page has named its video over the years.
static RE_ARTICLE_IDS: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    vec![
        Regex::new(r#"player\.load[^;]+src:\s*["']([^"'@]+)"#).unwrap(),
        Regex::new(r#"id-video=([^@"'&]+)"#).unwrap(),
        Regex::new(r#"videos\.francetv\.fr/video/([^@"'?]+)"#).unwrap(),
        Regex::new(&format!(r#"(?:data-id|<figure[^<]+\bid)=["']({UUID})"#)).unwrap(),
    ]
});
/// A link to an episode page on a programme page, with its text.
static RE_EPISODE_LINK: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)<a\b[^>]*href="((?:https://www\.france\.tv)?(/[a-z0-9-]+/[a-z0-9-]+/(?:saison-\d+/)?\d+-[a-z0-9-]+\.html))"[^>]*>(.*?)</a>"#,
    )
    .unwrap()
});

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A france.tv page: a video, or a channel's live stream.
    Page(Url),
    /// A france.tv programme page, listing its episodes.
    Listing(Url),
    /// A franceinfo page carrying the player.
    Article(Url),
    /// The player itself, by video id.
    Video { id: String },
}

fn is_uuid(text: &str) -> bool {
    RE_UUID.is_match(&text.to_ascii_lowercase())
}

/// france.tv `.html` pages and programme pages, franceinfo pages, and the player's
/// embed and legacy links.
pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let is_page = url.path().ends_with(".html");
    let video = |id: &str| {
        let id = id
            .split('@')
            .next()
            .unwrap_or(id)
            .trim()
            .to_ascii_lowercase();
        (!id.is_empty()).then_some(Link::Video { id })
    };
    match host.as_str() {
        "france.tv" | "www.france.tv" | "mobile.france.tv" => {
            if is_page && !segments.is_empty() {
                return Some(Link::Page(url.clone()));
            }
            if segments.len() == 2 && segments.iter().all(|s| !s.contains('.')) {
                return Some(Link::Listing(url.clone()));
            }
            None
        }
        "embed.francetv.fr" => util::query_param(url, "ue")
            .as_deref()
            .or(segments.first().copied())
            .filter(|id| is_uuid(id))
            .and_then(video),
        "videos.francetv.fr" => match segments.as_slice() {
            ["video", id, ..] => video(id),
            _ => None,
        },
        "sivideo.webservices.francetelevisions.fr" => {
            util::query_param(url, "id").as_deref().and_then(video)
        }
        h if h == "franceinfo.fr"
            || h.ends_with(".franceinfo.fr")
            || h == "francetvinfo.fr"
            || h.ends_with(".francetvinfo.fr") =>
        {
            (is_page || segments.contains(&"emissions")).then(|| Link::Article(url.clone()))
        }
        _ => None,
    }
}

/// The video id a france.tv page names in its player options.
pub fn options_id(html: &str) -> Option<String> {
    RE_OPTIONS_ID
        .captures(html)
        .map(|c| c[1].to_ascii_lowercase())
}

/// The video id an article page names: in the id of its player wrapper, or in the
/// older player markup.
pub fn article_video_id(html: &str) -> Option<String> {
    for tag in ["div", "button", "figure"] {
        let wrappers = util::tags_where(html, tag, &|attrs| {
            attrs
                .iter()
                .any(|(name, value)| name == "data-cy" && value == "francetv-player-wrapper")
        });
        if let Some(id) = wrappers
            .iter()
            .filter_map(|tag| util::attribute(tag, "id"))
            .find(|id| is_uuid(id))
        {
            return Some(id.to_ascii_lowercase());
        }
    }
    RE_ARTICLE_IDS
        .iter()
        .find_map(|re| util::search(re, html))
        .map(|id| id.split('@').next().unwrap_or(&id).to_ascii_lowercase())
}

/// What a page says about its video before the player API is asked: where it is, and
/// the title and picture it shows, for when the API names none.
#[derive(Debug, Clone)]
pub struct Hints {
    pub url: Url,
    pub title: Option<String>,
    pub image: Option<Url>,
}

impl Hints {
    fn of(html: &str, url: &Url) -> Self {
        let page = Page::parse(html, url);
        Self {
            url: url.clone(),
            title: page
                .meta("og:title")
                .and_then(|t| clean_title(&t))
                .or_else(|| page.title()),
            image: page
                .meta("og:image")
                .and_then(|i| util::join_url(Some(url), &i)),
        }
    }
}

/// The `domain` the player API is told a video plays on.
fn domain_of(url: &Url) -> String {
    match url.host_str().map(str::to_ascii_lowercase) {
        Some(host) if host.contains("franceinfo") || host.contains("francetvinfo") => {
            "www.franceinfo.fr".to_string()
        }
        _ => "www.france.tv".to_string(),
    }
}

/// The subtitle language the API names, and what it stands for.
fn subtitle_language(code: &str) -> (String, Option<String>) {
    match code {
        "qsm" => ("fr".into(), Some("sourds et malentendants".into())),
        "qad" | "qtz" => ("fr".into(), Some("audiodescription".into())),
        other => (other.to_string(), None),
    }
}

fn subtitle_format(url: &Url) -> SubtitleFormat {
    match super::path_extension(url).as_deref() {
        Some("srt") => SubtitleFormat::Srt,
        Some("ttml") | Some("xml") | Some("dfxp") => SubtitleFormat::Ttml,
        _ => SubtitleFormat::Vtt,
    }
}

/// What the player API's error codes mean.
fn api_error(answer: &Value, origin: &Url) -> ResolveError {
    let message = answer["message"]
        .as_str()
        .map(util::clean_html)
        .and_then(|m| clean_title(&m));
    match util::int(&answer["code"]) {
        Some(2009) => ResolveError::unavailable(origin, "available only in France"),
        Some(2015) | Some(2017) | Some(2019) => ResolveError::drm(origin, "the player's"),
        Some(2012) => ResolveError::NotFound(origin.clone()),
        Some(code) => ResolveError::unavailable(
            origin,
            match message {
                Some(message) => format!("the player answered {code}: {message}"),
                None => format!("the player answered {code}"),
            },
        ),
        None => ResolveError::unavailable(
            origin,
            message.unwrap_or_else(|| "the player answered without a video".to_string()),
        ),
    }
}

pub struct FrancetvResolver {
    http: Http,
}

impl FrancetvResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }

    /// The player API's description of `id` for one kind of browser: the answer with its
    /// `video` and `meta`, or the error the API names.
    async fn player(
        &self,
        id: &str,
        device: &str,
        browser: &str,
        domain: &str,
        origin: &Url,
    ) -> Result<Value, ResolveError> {
        let mut api = Url::parse(&format!("{PLAYER_API}{id}")).expect("valid");
        api.query_pairs_mut()
            .append_pair("device_type", device)
            .append_pair("browser", browser)
            .append_pair("domain", domain);
        let fetched = fetch(&self.http, &api, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        let answer: Option<Value> = serde_json::from_slice(&fetched.body).ok();
        match (fetched.status.as_u16(), answer) {
            (200..=299, Some(answer)) if answer["video"].is_object() => Ok(answer),
            (200..=299, Some(answer)) | (404 | 422, Some(answer)) => {
                Err(api_error(&answer, origin))
            }
            (429, _) => Err(ResolveError::RateLimited(origin.clone())),
            (status, _) => Err(ResolveError::unavailable(
                origin,
                format!("the player answered HTTP {status}"),
            )),
        }
    }

    /// `media` signed through the token service the API names, so the CDN serves it.
    async fn sign(&self, video: &Value, media: &Url, origin: &Url) -> Result<Url, ResolveError> {
        let Some(service) = util::url_of(&video["token"]["akamai"], None)
            .or_else(|| util::url_of(&video["token"], None))
        else {
            return Ok(media.clone());
        };
        let mut request = service.clone();
        request.set_query(None);
        request
            .query_pairs_mut()
            .append_pair("format", "json")
            .append_pair("url", media.as_str());
        let fetched = fetch(&self.http, &request, PLATFORM, BROWSER_UA, &[], MAX_PAGE).await?;
        if let Some(error) = status_error(fetched.status, origin) {
            return Err(error);
        }
        let answer = fetched.json(origin)?;
        util::url_of(&answer["url"], None)
            .ok_or_else(|| ResolveError::malformed(origin, "the token service named no URL"))
    }

    /// Whether the CDN withholds `media` for this region.
    async fn is_geo_blocked(&self, media: &Url) -> bool {
        match self
            .http
            .head(media.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .send()
            .await
        {
            Ok(response) => response.header("x-errortype") == Some("geo"),
            Err(_) => false,
        }
    }

    async fn resolve_video(
        &self,
        id: &str,
        domain: &str,
        hints: Option<Hints>,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let mut answers = Vec::new();
        let mut refusals = Vec::new();
        for (device, browser) in [("desktop", "chrome"), ("mobile", "safari")] {
            match self.player(id, device, browser, domain, origin).await {
                Ok(answer) => answers.push(answer),
                Err(error) => refusals.push(error),
            }
        }
        if answers.is_empty() {
            return Err(refusals.remove(0));
        }
        let meta = answers
            .iter()
            .map(|a| &a["meta"])
            .find(|m| m.is_object())
            .cloned()
            .unwrap_or(Value::Null);
        let mut variants = Vec::new();
        let mut subtitles = Vec::new();
        let mut live = false;
        let mut duration = None;
        let mut media_url = None;
        let mut failure = None;
        for answer in &answers {
            let video = &answer["video"];
            let Some(media) = util::url_of(&video["url"], None) else {
                continue;
            };
            live |= video["is_live"].as_bool().unwrap_or(false);
            duration = duration.or_else(|| util::seconds(&video["duration"]));
            media_url.get_or_insert(media.clone());
            let signed = self.sign(video, &media, origin).await?;
            let token: Vec<(String, String)> = signed
                .query_pairs()
                .filter(|(k, _)| k == "hdnea")
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            let format = video["format"].as_str().unwrap_or("").to_ascii_lowercase();
            let extension = super::path_extension(&signed).unwrap_or_default();
            let expanded = if format == "dash" || extension == "mpd" {
                dash::expand(&self.http, &signed, PLATFORM, BROWSER_UA, &[])
                    .await
                    .map(|e| ("dash", e.variants, e.subtitles))
            } else {
                hls::expand(&self.http, &signed, PLATFORM, BROWSER_UA, &[])
                    .await
                    .map(|e| ("hls", e.variants, e.subtitles))
            };
            match expanded {
                Ok((name, streams, tracks)) => {
                    for mut variant in streams {
                        variant.format_id = Some(match &variant.label {
                            Some(label) => format!("{name}-{label}"),
                            None => name.to_string(),
                        });
                        variant.query = token.clone();
                        variant.live |= live;
                        variants.push(variant);
                    }
                    for track in tracks {
                        if !subtitles.iter().any(|t: &SubtitleTrack| t.url == track.url) {
                            subtitles.push(track);
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(%origin, "France TV {format} manifest not read: {error}");
                    failure = Some(error);
                }
            }
        }
        if variants.is_empty() {
            if let Some(media) = &media_url
                && self.is_geo_blocked(media).await
            {
                return Err(ResolveError::unavailable(
                    origin,
                    "available only in France",
                ));
            }
            return Err(failure.unwrap_or_else(|| ResolveError::NotFound(origin.clone())));
        }
        for caption in meta["subtitles"].as_array().into_iter().flatten() {
            let Some(track_url) = util::url_of(&caption["url"], None) else {
                continue;
            };
            if subtitles.iter().any(|t| t.url == track_url) {
                continue;
            }
            let (language, name) =
                subtitle_language(caption["lang"].as_str().unwrap_or("fr").trim());
            subtitles.push(SubtitleTrack {
                format: subtitle_format(&track_url),
                url: track_url,
                language,
                name,
                auto: false,
                headers: Vec::new(),
            });
        }
        let title = match (
            meta["title"].as_str().and_then(clean_title),
            meta["additional_title"].as_str().and_then(clean_title),
        ) {
            (Some(title), Some(more)) => Some(format!("{title} - {more}")),
            (Some(title), None) => Some(title),
            (None, more) => more,
        };
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.to_string());
        resolved.title = title.or_else(|| hints.as_ref().and_then(|h| h.title.clone()));
        resolved.description = meta["description"].as_str().and_then(clean_title);
        resolved.uploaded_at = util::time(&meta["broadcasted_at"]);
        resolved.duration = duration.or_else(|| variants.iter().find_map(|v| v.duration));
        resolved.thumbnail = util::url_of(&meta["image_url"], None)
            .or_else(|| hints.as_ref().and_then(|h| h.image.clone()));
        resolved.webpage_url = Some(
            hints
                .as_ref()
                .map(|h| h.url.clone())
                .unwrap_or_else(|| origin.clone()),
        );
        resolved.live = live;
        resolved.subtitles = subtitles;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    async fn html(&self, page_url: &Url, origin: &Url) -> Result<(Url, String), ResolveError> {
        let mut headers = navigation_headers();
        headers.retain(|(name, _)| name != "accept-language");
        headers.push(("accept-language".into(), "fr-FR,fr;q=0.9,en;q=0.5".into()));
        let fetched = fetch(
            &self.http, page_url, PLATFORM, BROWSER_UA, &headers, MAX_PAGE,
        )
        .await?;
        if let Some(error) = status_error(fetched.status, origin) {
            return Err(error);
        }
        let html = fetched.text();
        Ok((fetched.url, html))
    }

    async fn resolve_page(&self, page_url: &Url, origin: &Url) -> Result<Resolution, ResolveError> {
        let (final_url, html) = self.html(page_url, origin).await?;
        let id = options_id(&html).ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
        let hints = Hints::of(&html, &final_url);
        self.resolve_video(&id, &domain_of(page_url), Some(hints), origin)
            .await
    }

    async fn resolve_article(
        &self,
        page_url: &Url,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let (final_url, html) = self.html(page_url, origin).await?;
        // A page without the player carries some other player, or none: the next
        // resolver looks.
        let id =
            article_video_id(&html).ok_or_else(|| ResolveError::Unsupported(origin.clone()))?;
        let hints = Hints::of(&html, &final_url);
        self.resolve_video(&id, &domain_of(page_url), Some(hints), origin)
            .await
    }

    async fn resolve_listing(
        &self,
        page_url: &Url,
        origin: &Url,
    ) -> Result<Resolution, ResolveError> {
        let (final_url, html) = self.html(page_url, origin).await?;
        let prefix = format!("{}/", final_url.path().trim_end_matches('/'));
        let mut entries: Vec<PlaylistEntry> = Vec::new();
        for caps in RE_EPISODE_LINK.captures_iter(&html) {
            if !caps[2].starts_with(&prefix) {
                continue;
            }
            let Some(link) = util::join_url(Some(&final_url), &caps[1]) else {
                continue;
            };
            if entries.iter().any(|e| e.url == link) {
                continue;
            }
            entries.push(PlaylistEntry {
                url: link,
                title: clean_title(&util::clean_html(&caps[3].replace("><", "> <"))),
                duration: None,
            });
            if entries.len() >= LISTING_LIMIT {
                break;
            }
        }
        if entries.is_empty() {
            return Err(ResolveError::NotFound(origin.clone()));
        }
        let page = Page::parse(&html, &final_url);
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(final_url.path().trim_matches('/').to_string()),
            title: page
                .meta("og:title")
                .and_then(|t| clean_title(&t))
                .or_else(|| page.title()),
            total: Some(entries.len()),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for FrancetvResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "France Télévisions",
            hosts: &[
                "france.tv",
                "franceinfo.fr",
                "francetvinfo.fr",
                "embed.francetv.fr",
                "videos.francetv.fr",
            ],
            features: &[
                "videos",
                "live",
                "programmes",
                "news articles",
                "embeds",
                "subtitles",
            ],
            formats: &["hls", "dash"],
            media: &[MediaKind::Video],
            tags: &[Tag::News, Tag::Video, Tag::Live],
            session: SessionSupport::None,
            examples: &[
                "https://www.france.tv/france-2/direct.html",
                "https://www.france.tv/france-2/journal-20h00/",
                "https://www.franceinfo.fr/monde/usa/presidentielle/donald-trump/etats-unis-un-risque-d-embrasement-apres-la-mort-d-un-manifestant_7764542.html",
                "https://embed.francetv.fr/?ue=f920fcc2-fa20-11f0-ac98-57a09c50f7ce",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Page(page) => self.resolve_page(&page, url).await,
            Link::Listing(page) => self.resolve_listing(&page, url).await,
            Link::Article(page) => self.resolve_article(&page, url).await,
            Link::Video { id } => self.resolve_video(&id, "www.france.tv", None, url).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };
    use crate::resolve::{Variant, VariantKind};
    use serde_json::json;
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

    const ID: &str = "1c4166fb-a4a0-4313-8b51-fc94a3fe9361";

    fn player_answer(format: &str, url: &str, live: bool) -> String {
        json!({
            "video": {"token": {"akamai": "https://hdfauth.ftven.fr/esi/TA?format=json"}, "duration": if live { Value::Null } else { json!(2997) },
                "format": format, "is_live": live, "drm": false, "url": url},
            "meta": {"id": ID, "title": "Les abeilles, sentinelles de la planète", "additional_title": if live { json!("N'oubliez pas les paroles") } else { Value::Null },
                "broadcasted_at": "2026-08-06T19:59:14+02:00",
                "image_url": "https://assets.webservices.francetelevisions.fr/v1/assets/images/6e/cf/5a/7b845bc2.jpg",
                "subtitles": [{"url": "https://assets.webservices.francetelevisions.fr/v1/assets/captions/e5/59/42/f597.vtt", "lang": "qsm"}],
                "description": "Le travail de pollinisation des abeilles est essentiel."}
        })
        .to_string()
    }

    const MPD: &str = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT49M57S" profiles="urn:mpeg:dash:profile:isoff-on-demand:2011">
  <Period>
    <AdaptationSet mimeType="video/mp4" contentType="video">
      <Representation id="video=1500000" bandwidth="1500000" width="1280" height="720" codecs="avc1.64001f"/>
      <Representation id="video=4000000" bandwidth="4000000" width="1920" height="1080" codecs="avc1.640028"/>
    </AdaptationSet>
    <AdaptationSet mimeType="audio/mp4" contentType="audio" lang="fr">
      <Representation id="audio=96000" bandwidth="96000" codecs="mp4a.40.2"/>
    </AdaptationSet>
  </Period>
</MPD>"#;
    const MASTER: &str = "#EXTM3U\n#EXT-X-VERSION:5\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"audio\",LANGUAGE=\"fr\",NAME=\"Audio\",DEFAULT=YES,AUTOSELECT=YES,URI=\"audio.m3u8\"\n#EXT-X-STREAM-INF:BANDWIDTH=1208000,CODECS=\"mp4a.40.2,avc1.4D401F\",RESOLUTION=1024x576,FRAME-RATE=25,AUDIO=\"audio\"\nvideo-576.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2322000,CODECS=\"mp4a.40.2,avc1.64001F\",RESOLUTION=1280x720,FRAME-RATE=25,AUDIO=\"audio\"\nvideo-720.m3u8\n";
    const MEDIA: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\n0.ts\n#EXTINF:4.0,\n1.ts\n#EXT-X-ENDLIST\n";
    const LIVE_MEDIA: &str =
        "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\n0.ts\n#EXTINF:6.0,\n1.ts\n";

    fn signed(url: &str) -> String {
        json!({"url": format!("{url}?hdnea=exp=1790271531~acl=%2f*~hmac=abc")}).to_string()
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let url = |s: &str| Url::parse(s).unwrap();
        assert_eq!(
            link("https://www.france.tv/france-2/direct.html"),
            Some(Link::Page(url(
                "https://www.france.tv/france-2/direct.html"
            )))
        );
        assert_eq!(
            link(
                "https://mobile.france.tv/france-5/c-dans-l-air/137347-emission-du-vendredi-12-mai-2017.html"
            ),
            Some(Link::Page(url(
                "https://mobile.france.tv/france-5/c-dans-l-air/137347-emission-du-vendredi-12-mai-2017.html"
            )))
        );
        assert_eq!(
            link("https://www.france.tv/france-2/journal-20h00/"),
            Some(Link::Listing(url(
                "https://www.france.tv/france-2/journal-20h00/"
            )))
        );
        assert_eq!(link("https://www.france.tv/"), None);
        assert_eq!(link("https://www.france.tv/france-2/"), None);
        assert_eq!(
            link(
                "https://www.franceinfo.fr/replay-jt/france-2/20-heures/prives-d-eau-potable_8208452.html"
            ),
            Some(Link::Article(url(
                "https://www.franceinfo.fr/replay-jt/france-2/20-heures/prives-d-eau-potable_8208452.html"
            )))
        );
        assert_eq!(
            link(
                "http://www.francetvinfo.fr/economie/entreprises/les-entreprises-familiales_933271.html"
            ),
            Some(Link::Article(url(
                "http://www.francetvinfo.fr/economie/entreprises/les-entreprises-familiales_933271.html"
            )))
        );
        assert_eq!(
            link("http://france3-regions.francetvinfo.fr/limousin/emissions/jt-1213-limousin"),
            Some(Link::Article(url(
                "http://france3-regions.francetvinfo.fr/limousin/emissions/jt-1213-limousin"
            )))
        );
        assert_eq!(link("https://www.franceinfo.fr/"), None);
        assert_eq!(link("https://www.franceinfo.fr/culture/"), None);
        assert_eq!(
            link("https://embed.francetv.fr/?ue=1C4166FB-a4a0-4313-8b51-fc94a3fe9361"),
            Some(Link::Video { id: ID.into() })
        );
        assert_eq!(
            link("https://embed.francetv.fr/1c4166fb-a4a0-4313-8b51-fc94a3fe9361"),
            Some(Link::Video { id: ID.into() })
        );
        assert_eq!(link("https://embed.francetv.fr/?ue=not-an-id"), None);
        assert_eq!(
            link("https://videos.francetv.fr/video/NI_1004933@Zouzous"),
            Some(Link::Video {
                id: "ni_1004933".into()
            })
        );
        assert_eq!(
            link(
                "https://sivideo.webservices.francetelevisions.fr/?id=162311093&idcatalogue=Jeunesse"
            ),
            Some(Link::Video {
                id: "162311093".into()
            })
        );
        assert_eq!(link("https://example.com/france-2/direct.html"), None);
    }

    #[test]
    fn video_ids_are_found_in_pages_and_articles() {
        let page = r#"<script>self.__next_f.push([1,"[\"$\",\"$L44\",null,{\"options\":{\"id\":\"1c4166fb-a4a0-4313-8b51-fc94a3fe9361\",\"initOptions\":{}}]"])</script>"#;
        assert_eq!(options_id(page).as_deref(), Some(ID));
        assert_eq!(options_id("<html>nothing</html>"), None);
        let article = "<div\n  id=\"5d751fe8-01be-4148-804a-f393fe4871ea\"\n  class=\"francetv-player-wrapper  francetv-player-wrapper--portrait\"\n  data-cy=\"francetv-player-wrapper\"\n  data-autoload=\"true\">";
        assert_eq!(
            article_video_id(article).as_deref(),
            Some("5d751fe8-01be-4148-804a-f393fe4871ea")
        );
        assert_eq!(
            article_video_id(
                r#"<figure id="7d204c9e-a2d3-11eb-9e4c-000d3a23d482" class="player">"#
            )
            .as_deref(),
            Some("7d204c9e-a2d3-11eb-9e4c-000d3a23d482")
        );
        assert_eq!(
            article_video_id(r#"<a href="//videos.francetv.fr/video/NI_657393@Regions">"#)
                .as_deref(),
            Some("ni_657393")
        );
        assert_eq!(article_video_id("<p>no player</p>"), None);
    }

    #[tokio::test]
    async fn pages_resolve_their_dash_and_hls_streams_signed() {
        let page_url = "https://www.france.tv/documentaires/documentaires-animaliers/5947971-les-abeilles-sentinelles-de-la-planete.html";
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            page_url,
            200,
            "text/html",
            format!(
                r#"<html><head><meta property="og:title" content="Les abeilles - Documentaire en replay"><meta property="og:image" content="https://medias.france.tv/x/abeilles.jpg"></head><body><script>self.__next_f.push([1,"[\"$\",\"$L44\",null,{{\"options\":{{\"id\":\"{ID}\",\"initOptions\":{{}}}}}}]"])</script></body></html>"#
            ),
        ));
        fixture.exchanges.push(get(
            &format!("{PLAYER_API}{ID}?device_type=desktop&browser=chrome&domain=www.france.tv"),
            200,
            "application/json",
            player_answer(
                "dash",
                "https://cloudreplay.ftven.fr/ftv/6/76/x.ism/manifest.mpd",
                false,
            ),
        ));
        fixture.exchanges.push(get(
            &format!("{PLAYER_API}{ID}?device_type=mobile&browser=safari&domain=www.france.tv"),
            200,
            "application/json",
            player_answer(
                "hls",
                "https://cloudreplay.ftven.fr/ftv/6/76/x.ism/master.m3u8",
                false,
            ),
        ));
        fixture.exchanges.push(get(
            "https://hdfauth.ftven.fr/esi/TA?format=json&url=https%3A%2F%2Fcloudreplay.ftven.fr%2Fftv%2F6%2F76%2Fx.ism%2Fmanifest.mpd",
            200,
            "application/json",
            signed("https://cloudreplay.ftven.fr/tok/ftv/6/76/x.ism/manifest.mpd"),
        ));
        fixture.exchanges.push(get(
            "https://cloudreplay.ftven.fr/tok/ftv/6/76/x.ism/manifest.mpd?hdnea=exp=1790271531~acl=%2f*~hmac=abc",
            200,
            "application/dash+xml",
            MPD.into(),
        ));
        fixture.exchanges.push(get(
            "https://hdfauth.ftven.fr/esi/TA?format=json&url=https%3A%2F%2Fcloudreplay.ftven.fr%2Fftv%2F6%2F76%2Fx.ism%2Fmaster.m3u8",
            200,
            "application/json",
            signed("https://cloudreplay.ftven.fr/tok/ftv/6/76/x.ism/master.m3u8"),
        ));
        fixture.exchanges.push(get(
            "https://cloudreplay.ftven.fr/tok/ftv/6/76/x.ism/master.m3u8?hdnea=exp=1790271531~acl=%2f*~hmac=abc",
            200,
            "application/x-mpegURL",
            MASTER.into(),
        ));
        fixture.exchanges.push(get(
            "https://cloudreplay.ftven.fr/tok/ftv/6/76/x.ism/video-720.m3u8",
            200,
            "application/x-mpegURL",
            MEDIA.into(),
        ));
        let resolver = FrancetvResolver::new(Http::replay(fixture));
        let url = Url::parse(page_url).unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some(ID));
        assert_eq!(
            resolved.title.as_deref(),
            Some("Les abeilles, sentinelles de la planète")
        );
        assert_eq!(
            resolved.description.as_deref(),
            Some("Le travail de pollinisation des abeilles est essentiel.")
        );
        assert!(resolved.uploaded_at.is_some());
        assert_eq!(resolved.duration, Some(Duration::from_secs(2997)));
        assert!(!resolved.live);
        assert_eq!(
            resolved.thumbnail.as_ref().map(|u| u.as_str()),
            Some(
                "https://assets.webservices.francetelevisions.fr/v1/assets/images/6e/cf/5a/7b845bc2.jpg"
            )
        );
        assert_eq!(
            resolved.webpage_url.as_ref().map(|u| u.as_str()),
            Some(page_url)
        );
        let dash: Vec<&Variant> = resolved
            .variants
            .iter()
            .filter(|v| v.kind == VariantKind::Dash)
            .collect();
        let hls: Vec<&Variant> = resolved
            .variants
            .iter()
            .filter(|v| v.kind == VariantKind::Hls)
            .collect();
        assert_eq!(dash.len(), 2);
        assert_eq!(hls.len(), 2);
        assert_eq!(dash[0].format_id.as_deref(), Some("dash-720p"));
        assert_eq!(hls[1].height, Some(720));
        assert_eq!(hls[1].format_id.as_deref(), Some("hls-720p"));
        assert_eq!(hls[1].duration, Some(Duration::from_secs(10)));
        assert!(resolved.variants.iter().all(|v| v.query
            == [(
                "hdnea".to_string(),
                "exp=1790271531~acl=/*~hmac=abc".to_string()
            )]));
        assert_eq!(resolved.subtitles.len(), 1);
        assert_eq!(resolved.subtitles[0].language, "fr");
        assert_eq!(
            resolved.subtitles[0].name.as_deref(),
            Some("sourds et malentendants")
        );
        assert_eq!(resolved.subtitles[0].format, SubtitleFormat::Vtt);
    }

    #[tokio::test]
    async fn live_channels_and_articles_resolve_and_pages_without_the_player_are_handed_on() {
        let live_id = "006194ea-117d-4bcf-94a9-153d999c59ae";
        let article_id = "5d751fe8-01be-4148-804a-f393fe4871ea";
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.france.tv/france-2/direct.html",
            200,
            "text/html",
            format!(
                r#"<html><head><meta property="og:image" content="https://medias.france.tv/x/france2.jpg"></head><body><script>self.__next_f.push([1,"{{\"options\":{{\"id\":\"{live_id}\"}}}}"])</script></body></html>"#
            ),
        ));
        fixture.exchanges.push(get(
            &format!("{PLAYER_API}{live_id}?device_type=desktop&browser=chrome&domain=www.france.tv"),
            422,
            "application/json",
            json!({"code": 2017, "message": "Cette vidéo n'est pas disponible depuis le site web mobile"}).to_string(),
        ));
        fixture.exchanges.push(get(
            &format!("{PLAYER_API}{live_id}?device_type=mobile&browser=safari&domain=www.france.tv"),
            200,
            "application/json",
            json!({"video": {"token": {"dai": "https://api.ssai.ftven.fr/v1/session/x/index.m3u8", "akamai": "https://hdfauth.ftven.fr/esi/TA?format=json"},
                    "duration": null, "format": "hls", "is_live": true, "drm": false, "url": "https://live-ssai-p.ftven.fr/dai/v1/master/x/index.m3u8"},
                "meta": {"id": live_id, "title": "France 2 en direct", "additional_title": "N'oubliez pas les paroles", "image_url": null, "subtitles": []}}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://hdfauth.ftven.fr/esi/TA?format=json&url=https%3A%2F%2Flive-ssai-p.ftven.fr%2Fdai%2Fv1%2Fmaster%2Fx%2Findex.m3u8",
            200,
            "application/json",
            signed("https://live-ssai-p.ftven.fr/tok/dai/v1/master/x/index.m3u8"),
        ));
        fixture.exchanges.push(get(
            "https://live-ssai-p.ftven.fr/tok/dai/v1/master/x/index.m3u8?hdnea=exp=1790271531~acl=%2f*~hmac=abc",
            200,
            "application/x-mpegURL",
            MASTER.into(),
        ));
        fixture.exchanges.push(get(
            "https://live-ssai-p.ftven.fr/tok/dai/v1/master/x/video-720.m3u8",
            200,
            "application/x-mpegURL",
            LIVE_MEDIA.into(),
        ));
        fixture.exchanges.push(get(
            "https://www.franceinfo.fr/replay-jt/france-2/20-heures/prives-d-eau-potable_8208452.html",
            200,
            "text/html",
            format!(
                "<html><head><meta property=\"og:title\" content=\"Privés d'eau potable depuis un an\"></head><body><div\n  id=\"{article_id}\"\n  class=\"francetv-player-wrapper\"\n  data-cy=\"francetv-player-wrapper\"></div></body></html>"
            ),
        ));
        fixture.exchanges.push(get(
            &format!("{PLAYER_API}{article_id}?device_type=desktop&browser=chrome&domain=www.franceinfo.fr"),
            200,
            "application/json",
            json!({"video": {"token": {"akamai": "https://hdfauth.ftven.fr/esi/TA?format=json"}, "duration": 139, "format": "dash", "is_live": false, "drm": false,
                    "url": "https://cloudingest.ftven.fr/ftv/4/d3/y.ism/manifest.mpd"},
                "meta": {"id": article_id, "title": "Privés d'eau potable depuis un an", "additional_title": null, "broadcasted_at": "2026-09-24T18:53:23+02:00", "image_url": null, "subtitles": []}}).to_string(),
        ));
        fixture.exchanges.push(get(
            &format!("{PLAYER_API}{article_id}?device_type=mobile&browser=safari&domain=www.franceinfo.fr"),
            422,
            "application/json",
            json!({"code": 2017, "message": "indisponible"}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://hdfauth.ftven.fr/esi/TA?format=json&url=https%3A%2F%2Fcloudingest.ftven.fr%2Fftv%2F4%2Fd3%2Fy.ism%2Fmanifest.mpd",
            200,
            "application/json",
            signed("https://cloudingest.ftven.fr/tok/ftv/4/d3/y.ism/manifest.mpd"),
        ));
        fixture.exchanges.push(get(
            "https://cloudingest.ftven.fr/tok/ftv/4/d3/y.ism/manifest.mpd?hdnea=exp=1790271531~acl=%2f*~hmac=abc",
            200,
            "application/dash+xml",
            MPD.into(),
        ));
        fixture.exchanges.push(get(
            "https://www.franceinfo.fr/culture/livres/un-roman_8207663.html",
            200,
            "text/html",
            r#"<html><body><p>Un article</p><iframe src="https://www.dailymotion.com/embed/video/x4iiko0"></iframe></body></html>"#.into(),
        ));
        let resolver = FrancetvResolver::new(Http::replay(fixture));
        let live = resolver
            .resolve(&Url::parse("https://www.france.tv/france-2/direct.html").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert!(live.live);
        assert_eq!(
            live.title.as_deref(),
            Some("France 2 en direct - N'oubliez pas les paroles")
        );
        assert_eq!(live.variants.len(), 2);
        assert!(
            live.variants
                .iter()
                .all(|v| v.live && v.kind == VariantKind::Hls)
        );
        assert_eq!(
            live.thumbnail.as_ref().map(|u| u.as_str()),
            Some("https://medias.france.tv/x/france2.jpg")
        );
        let article = resolver
            .resolve(
                &Url::parse(
                    "https://www.franceinfo.fr/replay-jt/france-2/20-heures/prives-d-eau-potable_8208452.html",
                )
                .unwrap(),
            )
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(article.id.as_deref(), Some(article_id));
        assert_eq!(
            article.title.as_deref(),
            Some("Privés d'eau potable depuis un an")
        );
        assert_eq!(article.duration, Some(Duration::from_secs(139)));
        assert_eq!(article.variants.len(), 2);
        assert!(article.variants.iter().all(|v| v.kind == VariantKind::Dash));
        assert_eq!(
            article.webpage_url.as_ref().map(|u| u.as_str()),
            Some(
                "https://www.franceinfo.fr/replay-jt/france-2/20-heures/prives-d-eau-potable_8208452.html"
            )
        );
        assert!(matches!(
            resolver
                .resolve(
                    &Url::parse("https://www.franceinfo.fr/culture/livres/un-roman_8207663.html")
                        .unwrap()
                )
                .await
                .unwrap_err(),
            ResolveError::Unsupported(_)
        ));
    }

    #[tokio::test]
    async fn programme_pages_list_their_episodes() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.france.tv/france-2/journal-20h00/",
            200,
            "text/html",
            concat!(
                r#"<html><head><meta property="og:title" content="Journal 20h00 - Replay et vidéos en streaming"></head><body>"#,
                r#"<a data-card-link="true" class="x" href="/france-2/journal-20h00/8763609-edition-du-mardi-8-septembre-2026.html"><div><span>Journal 20h00</span><div><span>Édition du mardi 8 septembre 2026</span></div></div></a>"#,
                r#"<a href="https://www.france.tv/france-2/journal-20h00/8763942-edition-du-lundi-7-septembre-2026.html">Édition du lundi 7 septembre 2026</a>"#,
                r#"<a href="/france-2/journal-20h00/8763609-edition-du-mardi-8-septembre-2026.html">again</a>"#,
                r#"<a href="/france-2/journal-13h00/8763000-edition-du-mardi-8-septembre-2026.html">another programme</a>"#,
                r#"</body></html>"#
            )
            .into(),
        ));
        let resolver = FrancetvResolver::new(Http::replay(fixture));
        let Resolution::Playlist(playlist) = resolver
            .resolve(&Url::parse("https://www.france.tv/france-2/journal-20h00/").unwrap())
            .await
            .unwrap()
        else {
            panic!("a playlist");
        };
        assert_eq!(playlist.id.as_deref(), Some("france-2/journal-20h00"));
        assert_eq!(
            playlist.title.as_deref(),
            Some("Journal 20h00 - Replay et vidéos en streaming")
        );
        assert_eq!(playlist.total, Some(2));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://www.france.tv/france-2/journal-20h00/8763609-edition-du-mardi-8-septembre-2026.html"
        );
        assert_eq!(
            playlist.entries[0].title.as_deref(),
            Some("Journal 20h00 Édition du mardi 8 septembre 2026")
        );
        assert_eq!(
            playlist.entries[1].title.as_deref(),
            Some("Édition du lundi 7 septembre 2026")
        );
    }

    #[tokio::test]
    async fn the_player_api_s_refusals_are_reported() {
        let refusal = |id: &str, code: u32, message: &str| {
            let body = json!({"code": code, "message": message, "id": id}).to_string();
            [
                get(
                    &format!(
                        "{PLAYER_API}{id}?device_type=desktop&browser=chrome&domain=www.france.tv"
                    ),
                    422,
                    "application/json",
                    body.clone(),
                ),
                get(
                    &format!(
                        "{PLAYER_API}{id}?device_type=mobile&browser=safari&domain=www.france.tv"
                    ),
                    422,
                    "application/json",
                    body,
                ),
            ]
        };
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.extend(refusal(
            "a9050959-eedd-4b4a-9b0d-de6eeaa73e44",
            2009,
            "Ce contenu n'est pas disponible dans votre zone",
        ));
        fixture.exchanges.extend(refusal(
            "b448bfe4-9fe7-11ee-97d8-2ba3426fa3df",
            2012,
            "Cette vidéo n'est pas disponible.",
        ));
        fixture.exchanges.extend(refusal(
            "c5bda21d-2c6f-4470-8849-3d8327adb2ba",
            2015,
            "L'accès à cette vidéo est impossible.",
        ));
        let resolver = FrancetvResolver::new(Http::replay(fixture));
        let embed = |id: &str| Url::parse(&format!("https://embed.francetv.fr/?ue={id}")).unwrap();
        let error = resolver
            .resolve(&embed("a9050959-eedd-4b4a-9b0d-de6eeaa73e44"))
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason == "available only in France"),
            "{error}"
        );
        assert!(matches!(
            resolver
                .resolve(&embed("b448bfe4-9fe7-11ee-97d8-2ba3426fa3df"))
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        assert!(matches!(
            resolver
                .resolve(&embed("c5bda21d-2c6f-4470-8849-3d8327adb2ba"))
                .await
                .unwrap_err(),
            ResolveError::Drm { .. }
        ));
    }

    /// Every example link resolves live: the live channel and the videos with signed
    /// streams, the programme page with episodes.
    #[tokio::test]

    async fn live_examples_resolve() {
        let resolver = FrancetvResolver::new(Http::new(crate::http::HttpConfig::default()));
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
                    assert!(resolved.title.is_some(), "{link}: no title");
                    println!(
                        "{link}: {:?}, live={}, {} variants, {} subtitles",
                        resolved.title,
                        resolved.live,
                        resolved.variants.len(),
                        resolved.subtitles.len()
                    );
                }
                Resolution::Playlist(playlist) => {
                    assert!(!playlist.entries.is_empty(), "{link}: no entries");
                    println!(
                        "{link}: {:?}, {} entries",
                        playlist.title,
                        playlist.entries.len()
                    );
                }
            }
        }
    }
}
