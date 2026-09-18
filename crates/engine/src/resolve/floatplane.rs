//! Floatplane posts and creator channels, through the JSON API the site's player calls: a
//! post's video and audio attachments come with per-quality HLS playlists on the creator's
//! CDN, and a creator's home lists its posts page by page. Sauce+ runs the same platform on
//! its own domain and builds on the functions here.

use std::sync::LazyLock;
use std::time::Duration;

use async_trait::async_trait;
use regex::Regex;
use serde_json::Value;
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionCheck, SessionSupport, SubtitleFormat, SubtitleTrack, Variant, VariantKind, clean_title,
    fetch, parse_codecs, path_extension, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::Container;

pub const PLATFORM: &str = "floatplane";

/// A site running the Floatplane platform: Floatplane itself, or Sauce+, which serves the
/// same API and pages on its own domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Site {
    /// The resolver id whose cookie jar holds the session.
    pub platform: &'static str,
    /// The site's origin, without a trailing slash: where the API and the pages live.
    pub base: &'static str,
    /// The domain the site's links are on, reached bare or through `www.` or `beta.`.
    pub domain: &'static str,
    /// The cookie a logged-in session is recognised by.
    pub session_cookie: &'static str,
}

pub const FLOATPLANE: Site = Site {
    platform: PLATFORM,
    base: "https://www.floatplane.com",
    domain: "floatplane.com",
    session_cookie: "sails.sid",
};

/// Posts a creator's list serves per request.
pub const PAGE_SIZE: usize = 20;
/// Where a creator's post list stops.
const MAX_ENTRIES: usize = 500;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A post; `attachment` names one of its media when the post holds several.
    Post {
        id: String,
        attachment: Option<String>,
    },
    /// A creator's home, or one channel of the creator.
    Channel {
        creator: String,
        channel: Option<String>,
    },
}

static RE_POST: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^/post/(\w+)").unwrap());
static RE_CHANNEL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/channel/([\w-]+)/home(?:/([\w-]+))?").unwrap());

/// The link shapes of a Floatplane-platform site on `domain`.
pub fn parse_link_on(url: &Url, domain: &str) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    let bare = host
        .strip_prefix("www.")
        .or_else(|| host.strip_prefix("beta."))
        .unwrap_or(&host);
    if bare != domain {
        return None;
    }
    if let Some(caps) = RE_POST.captures(url.path()) {
        return Some(Link::Post {
            id: caps[1].to_string(),
            attachment: util::query_param(url, "attachment"),
        });
    }
    if let Some(caps) = RE_CHANNEL.captures(url.path()) {
        return Some(Link::Channel {
            creator: caps[1].to_string(),
            channel: caps.get(2).map(|m| m.as_str().to_string()),
        });
    }
    None
}

pub fn parse_link(url: &Url) -> Option<Link> {
    parse_link_on(url, FLOATPLANE.domain)
}

/// Whether the site's jar holds its session cookie.
pub fn has_session(http: &Http, site: &Site) -> bool {
    http.jar(site.platform).get(site.session_cookie).is_some()
}

fn site_headers(site: &Site) -> Vec<(String, String)> {
    vec![
        ("origin".to_string(), site.base.to_string()),
        ("referer".to_string(), format!("{}/", site.base)),
    ]
}

/// What the site's error bodies say: the names of its `errors`, and `errors[0].message`,
/// else `message`.
#[derive(Debug, Default)]
struct ApiError {
    names: Vec<String>,
    message: Option<String>,
}

impl ApiError {
    fn of(body: &[u8]) -> Self {
        let Ok(value) = serde_json::from_slice::<Value>(body) else {
            return Self::default();
        };
        let names = value["errors"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|error| error["name"].as_str())
            .map(str::to_string)
            .collect();
        let message = value["errors"][0]["message"]
            .as_str()
            .or_else(|| value["message"].as_str())
            .map(str::to_string)
            .filter(|m| !m.is_empty());
        Self { names, message }
    }

    /// The site answers every post, media and delivery request without a logged-in
    /// session with this error, beside the subscription one.
    fn not_logged_in(&self) -> bool {
        self.names.iter().any(|name| name == "notLoggedInError")
    }
}

/// GETs `path` of the site's API with `query`, as the site's platform, mapping the site's
/// refusals to the errors they mean.
pub async fn api(
    http: &Http,
    site: &Site,
    path: &str,
    query: &[(&str, &str)],
    origin: &Url,
) -> Result<Value, ResolveError> {
    let mut url = Url::parse(site.base)
        .and_then(|base| base.join(path))
        .map_err(|e| ResolveError::malformed(origin, format!("API URL: {e}")))?;
    url.query_pairs_mut().extend_pairs(query);
    let fetched = fetch(
        http,
        &url,
        site.platform,
        BROWSER_UA,
        &site_headers(site),
        MAX_PAGE,
    )
    .await?;
    match fetched.status.as_u16() {
        200..=299 => fetched.json(origin),
        401 => Err(ResolveError::login_required(
            origin,
            site.platform,
            ApiError::of(&fetched.body)
                .message
                .unwrap_or_else(|| "the session is not logged in".into()),
        )),
        403 => {
            let error = ApiError::of(&fetched.body);
            if error.not_logged_in() {
                return Err(ResolveError::login_required(
                    origin,
                    site.platform,
                    error
                        .message
                        .unwrap_or_else(|| "the session is not logged in".into()),
                ));
            }
            Err(ResolveError::unavailable(
                origin,
                error
                    .message
                    .unwrap_or_else(|| "the post needs a subscription to its creator".into()),
            ))
        }
        404 | 410 => Err(ResolveError::NotFound(origin.clone())),
        429 => Err(ResolveError::RateLimited(origin.clone())),
        status => Err(ResolveError::unavailable(
            origin,
            format!("the API answered HTTP {status}"),
        )),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentKind {
    Video,
    Audio,
}

impl AttachmentKind {
    /// The API's name for the attachment type: the `content/<type>` endpoint.
    pub fn as_str(&self) -> &'static str {
        match self {
            AttachmentKind::Video => "video",
            AttachmentKind::Audio => "audio",
        }
    }
}

/// One media of a post.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub id: String,
    pub kind: AttachmentKind,
}

/// A post as its API record describes it, before any attachment's streams are read.
#[derive(Debug, Clone)]
pub struct Post {
    pub id: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub thumbnail: Option<Url>,
    pub uploader: Option<String>,
    pub uploader_url: Option<Url>,
    pub uploaded_at: Option<jiff::Timestamp>,
    pub attachments: Vec<Attachment>,
}

fn attachments_of(list: &Value, default: AttachmentKind) -> Vec<Attachment> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| {
            let id = match item {
                Value::String(id) => id.clone(),
                other => other["id"].as_str()?.to_string(),
            };
            let kind = match item["type"].as_str() {
                Some("audio") => AttachmentKind::Audio,
                Some("video") => AttachmentKind::Video,
                _ => default,
            };
            Some(Attachment { id, kind })
        })
        .collect()
}

/// Reads post `post_id` of `site`: its record and the attachments it holds.
pub async fn post(
    http: &Http,
    site: &Site,
    post_id: &str,
    origin: &Url,
) -> Result<Post, ResolveError> {
    let data = api(
        http,
        site,
        "/api/v3/content/post",
        &[("id", post_id)],
        origin,
    )
    .await?;
    let has_media = data["metadata"]["hasVideo"].as_bool() == Some(true)
        || data["metadata"]["hasAudio"].as_bool() == Some(true);
    if !has_media {
        return Err(ResolveError::unavailable(
            origin,
            "the post has no video or audio attachment",
        ));
    }
    let mut attachments = attachments_of(&data["videoAttachments"], AttachmentKind::Video);
    attachments.extend(attachments_of(
        &data["audioAttachments"],
        AttachmentKind::Audio,
    ));
    let uploader_url = data["creator"]["urlname"]
        .as_str()
        .filter(|name| !name.is_empty())
        .and_then(|name| Url::parse(&format!("{}/channel/{name}/home", site.base)).ok());
    Ok(Post {
        id: post_id.to_string(),
        title: data["title"].as_str().and_then(clean_title),
        description: data["text"]
            .as_str()
            .map(util::clean_html)
            .filter(|text| !text.is_empty()),
        thumbnail: util::url_of(&data["thumbnail"]["path"], None),
        uploader: data["creator"]["title"].as_str().and_then(clean_title),
        uploader_url,
        uploaded_at: data["releaseDate"].as_str().and_then(util::parse_timestamp),
        attachments,
    })
}

/// The stream `kind` of a delivery variant: the CDN serves HLS media playlists for video
/// and plain files for audio.
fn variant_kind(url: &Url) -> VariantKind {
    if url.path().contains(".m3u8") {
        VariantKind::Hls
    } else {
        VariantKind::File
    }
}

/// The variants a delivery answer offers, on the CDN origin it names.
pub fn variants_of(
    stream: &Value,
    kind: AttachmentKind,
    site: &Site,
    origin: &Url,
) -> Result<Vec<Variant>, ResolveError> {
    let group = &stream["groups"][0];
    let cdn = group["origins"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|o| util::url_of(&o["url"], None))
        .ok_or_else(|| ResolveError::malformed(origin, "the delivery info names no CDN origin"))?;
    let mut variants = Vec::new();
    for variant in group["variants"].as_array().into_iter().flatten() {
        let Some(path) = variant["url"].as_str().filter(|p| !p.is_empty()) else {
            continue;
        };
        let Some(url) = util::join_url(Some(&cdn), path) else {
            continue;
        };
        let meta = &variant["meta"];
        let mut v = Variant::new(url.clone(), variant_kind(&url));
        if v.kind == VariantKind::File {
            v.container = path_extension(&url).and_then(|e| Container::from_extension(&e));
        }
        v.format_id = variant["name"].as_str().map(str::to_string);
        v.label = variant["label"].as_str().map(str::to_string);
        v.width = util::u32_of(&meta["video"]["width"]);
        v.height = util::u32_of(&meta["video"]["height"]);
        v.fps = util::float(&meta["video"]["fps"]).filter(|f| *f > 0.0);
        let video_codec = meta["video"]["codec"].as_str().filter(|c| !c.is_empty());
        let audio_codec = meta["audio"]["codec"].as_str().filter(|c| !c.is_empty());
        let codecs: Vec<&str> = video_codec.into_iter().chain(audio_codec).collect();
        if !codecs.is_empty() {
            let joined = codecs.join(",");
            let (video, audio) = parse_codecs(Some(&joined));
            v.video = video;
            v.audio = audio;
            v.codecs = Some(joined);
        }
        let video_rate = util::uint(&meta["video"]["bitrate"]["average"]);
        let audio_rate = util::uint(&meta["audio"]["bitrate"]["average"]);
        if video_rate.is_some() || audio_rate.is_some() {
            v.bitrate = Some(video_rate.unwrap_or(0) + audio_rate.unwrap_or(0));
        }
        v.audio_only = kind == AttachmentKind::Audio;
        v.headers = site_headers(site);
        variants.push(v);
    }
    Ok(variants)
}

/// The caption format a text track's link ends in; the site's player is an HTML5
/// `<track>`, which plays WebVTT, so tracks without an extension are WebVTT.
fn subtitle_format(url: &Url) -> Option<SubtitleFormat> {
    match path_extension(url).as_deref() {
        None | Some("vtt") | Some("webvtt") => Some(SubtitleFormat::Vtt),
        Some("srt") => Some(SubtitleFormat::Srt),
        Some("ttml") | Some("dfxp") | Some("xml") => Some(SubtitleFormat::Ttml),
        Some("ass") | Some("ssa") => Some(SubtitleFormat::Ass),
        Some(_) => None,
    }
}

/// The subtitle tracks a media's metadata lists; generated ones are automatic captions.
pub fn subtitles_of(metadata: &Value) -> Vec<SubtitleTrack> {
    metadata["textTracks"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|track| {
            let url = util::url_of(&track["src"], None)?;
            let format = subtitle_format(&url)?;
            Some(SubtitleTrack {
                url,
                language: track["language"]
                    .as_str()
                    .filter(|l| !l.is_empty())
                    .unwrap_or("en")
                    .to_string(),
                name: track["label"].as_str().map(str::to_string),
                format,
                auto: track["generated"].as_bool() == Some(true),
                headers: Vec::new(),
            })
        })
        .collect()
}

/// The media record of an attachment: its title, duration, thumbnail and text tracks.
/// The site answers nothing for some media, which leaves the post's own details.
pub async fn attachment_metadata(
    http: &Http,
    site: &Site,
    attachment: &Attachment,
    origin: &Url,
) -> Value {
    api(
        http,
        site,
        &format!("/api/v3/content/{}", attachment.kind.as_str()),
        &[("id", &attachment.id)],
        origin,
    )
    .await
    .unwrap_or(Value::Null)
}

/// Reads one attachment of `post`: its delivery streams and its media record.
pub async fn attachment(
    http: &Http,
    site: &Site,
    post: &Post,
    attachment: &Attachment,
    origin: &Url,
) -> Result<Resolved, ResolveError> {
    let stream = api(
        http,
        site,
        "/api/v3/delivery/info",
        &[("scenario", "onDemand"), ("entityId", &attachment.id)],
        origin,
    )
    .await?;
    let metadata = attachment_metadata(http, site, attachment, origin).await;
    let variants = variants_of(&stream, attachment.kind, site, origin)?;
    if variants.is_empty() {
        return Err(ResolveError::unavailable(
            origin,
            "the delivery info offers no stream",
        ));
    }
    let mut resolved = Resolved::new(site.platform);
    resolved.id = Some(attachment.id.clone());
    resolved.title = metadata["title"]
        .as_str()
        .and_then(clean_title)
        .or_else(|| post.title.clone());
    resolved.description = post.description.clone();
    resolved.uploader = post.uploader.clone();
    resolved.uploader_url = post.uploader_url.clone();
    resolved.uploaded_at = post.uploaded_at;
    resolved.duration = util::uint(&metadata["duration"])
        .filter(|d| *d > 0)
        .map(Duration::from_secs);
    resolved.thumbnail =
        util::url_of(&metadata["thumbnail"]["path"], None).or_else(|| post.thumbnail.clone());
    resolved.webpage_url = Url::parse(&format!("{}/post/{}", site.base, post.id)).ok();
    resolved.subtitles = subtitles_of(&metadata);
    resolved.variants = variants;
    Ok(resolved)
}

/// The link of one attachment of a post.
pub fn attachment_link(site: &Site, post_id: &str, attachment_id: &str) -> Option<Url> {
    Url::parse(&format!(
        "{}/post/{post_id}?attachment={attachment_id}",
        site.base
    ))
    .ok()
}

/// Resolves post `post_id`: its one attachment as media, one named attachment, or a
/// playlist of its attachments when it holds several.
pub async fn resolve_post(
    http: &Http,
    site: &Site,
    post_id: &str,
    attachment_id: Option<&str>,
    origin: &Url,
) -> Result<Resolution, ResolveError> {
    let post = post(http, site, post_id, origin).await?;
    if let Some(wanted) = attachment_id {
        let found = post
            .attachments
            .iter()
            .find(|a| a.id == wanted)
            .ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
        return Ok(Resolution::from(
            attachment(http, site, &post, found, origin).await?,
        ));
    }
    match post.attachments.as_slice() {
        [] => Err(ResolveError::unavailable(
            origin,
            "the post has no video or audio attachment",
        )),
        [only] => Ok(Resolution::from(
            attachment(http, site, &post, only, origin).await?,
        )),
        many => {
            let mut entries = Vec::with_capacity(many.len());
            for item in many {
                let Some(url) = attachment_link(site, post_id, &item.id) else {
                    continue;
                };
                let metadata = attachment_metadata(http, site, item, origin).await;
                entries.push(PlaylistEntry {
                    url,
                    title: metadata["title"]
                        .as_str()
                        .and_then(clean_title)
                        .or_else(|| post.title.clone()),
                    duration: util::uint(&metadata["duration"])
                        .filter(|d| *d > 0)
                        .map(Duration::from_secs),
                });
            }
            Ok(Resolution::Playlist(Playlist {
                resolver: site.platform.to_string(),
                id: Some(post_id.to_string()),
                title: post.title.clone(),
                entries,
                total: None,
            }))
        }
    }
}

/// Lists the posts of `creator`, or of its channel `channel`, page by page.
pub async fn channel(
    http: &Http,
    site: &Site,
    creator: &str,
    channel: Option<&str>,
    origin: &Url,
) -> Result<Playlist, ResolveError> {
    let creators = api(
        http,
        site,
        "/api/v3/creator/named",
        &[("creatorURL[0]", creator)],
        origin,
    )
    .await?;
    let creator_data = creators
        .as_array()
        .and_then(|list| list.first())
        .filter(|c| c.is_object())
        .ok_or_else(|| ResolveError::NotFound(origin.clone()))?;
    let creator_id = creator_data["id"]
        .as_str()
        .ok_or_else(|| ResolveError::malformed(origin, "the creator record has no id"))?;
    let channel_data = channel.and_then(|wanted| {
        creator_data["channels"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|c| c["urlname"].as_str() == Some(wanted))
    });
    let channel_id = channel_data.and_then(|c| c["id"].as_str());
    let display_id = match channel {
        Some(name) => format!("{creator}/{name}"),
        None => creator.to_string(),
    };
    let mut entries = Vec::new();
    let limit = PAGE_SIZE.to_string();
    for page in 0.. {
        let after = (page * PAGE_SIZE).to_string();
        let mut query = vec![
            ("id", creator_id),
            ("limit", limit.as_str()),
            ("fetchAfter", after.as_str()),
        ];
        if let Some(id) = channel_id {
            query.push(("channel", id));
        }
        let posts = api(http, site, "/api/v3/content/creator", &query, origin).await?;
        let page_posts = posts.as_array().cloned().unwrap_or_default();
        for post in &page_posts {
            let Some(id) = post["id"].as_str() else {
                continue;
            };
            let Ok(url) = Url::parse(&format!("{}/post/{id}", site.base)) else {
                continue;
            };
            entries.push(PlaylistEntry {
                url,
                title: post["title"].as_str().and_then(clean_title),
                duration: None,
            });
        }
        if page_posts.len() < PAGE_SIZE || entries.len() >= MAX_ENTRIES {
            break;
        }
    }
    let title = channel_data
        .and_then(|c| c["title"].as_str())
        .or_else(|| creator_data["title"].as_str())
        .and_then(clean_title);
    Ok(Playlist {
        resolver: site.platform.to_string(),
        id: Some(display_id),
        title,
        entries,
        total: None,
    })
}

/// Whom the site's session cookie logs in as, by the account endpoint the site's own
/// pages call.
pub async fn check_session(http: &Http, site: &Site) -> Result<SessionCheck, ResolveError> {
    if !has_session(http, site) {
        return Ok(SessionCheck::LoggedOut);
    }
    let origin = Url::parse(site.base).map_err(|e| ResolveError::Malformed {
        url: Url::parse("https://www.floatplane.com/").expect("valid"),
        detail: e.to_string(),
    })?;
    match api(http, site, "/api/v3/user/self", &[], &origin).await {
        Ok(user) => Ok(match user["username"].as_str().filter(|u| !u.is_empty()) {
            Some(name) => SessionCheck::LoggedIn {
                account: name.to_string(),
            },
            None => SessionCheck::LoggedOut,
        }),
        Err(ResolveError::LoginRequired { .. })
        | Err(ResolveError::Unavailable { .. })
        | Err(ResolveError::NotFound(_)) => Ok(SessionCheck::LoggedOut),
        Err(error) => Err(error),
    }
}

/// Resolves a link of `site` after [`parse_link_on`] read it.
pub async fn resolve_link(
    http: &Http,
    site: &Site,
    link: Link,
    origin: &Url,
) -> Result<Resolution, ResolveError> {
    match link {
        Link::Post { id, attachment } => {
            if !has_session(http, site) {
                return Err(ResolveError::login_required(
                    origin,
                    site.platform,
                    format!(
                        "posts are read with the {} session cookie",
                        site.session_cookie
                    ),
                ));
            }
            resolve_post(http, site, &id, attachment.as_deref(), origin).await
        }
        Link::Channel {
            creator,
            channel: name,
        } => Ok(Resolution::Playlist(
            channel(http, site, &creator, name.as_deref(), origin).await?,
        )),
    }
}

pub struct FloatplaneResolver {
    http: Http,
}

impl FloatplaneResolver {
    pub fn new(http: Http) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Resolver for FloatplaneResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Floatplane",
            hosts: &["floatplane.com"],
            features: &["videos", "audio", "posts", "channels"],
            formats: &["hls", "mp4", "aac"],
            session: SessionSupport::Required,
            examples: &[
                "https://www.floatplane.com/post/957jPKiAOV",
                "https://www.floatplane.com/channel/linustechtips/home/ltxexpo",
                "https://www.floatplane.com/channel/ShankMods/home",
                "https://beta.floatplane.com/channel/linustechtips/home",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        resolve_link(&self.http, &FLOATPLANE, link, url).await
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        check_session(&self.http, &FLOATPLANE).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{Cookie, Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse};
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

    fn logged_in(fixture: Fixture) -> Http {
        let http = Http::replay(fixture);
        http.with_jar(PLATFORM, |jar| {
            jar.insert(Cookie::new("sails.sid", "s%3Aabc", "www.floatplane.com"));
        });
        http
    }

    const MEDIA_PLAYLIST: &str = "#EXTM3U\n#EXT-X-TARGETDURATION:6\n#EXTINF:6.0,\n0.ts\n#EXTINF:4.0,\n1.ts\n#EXT-X-ENDLIST\n";

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        assert_eq!(
            link("https://www.floatplane.com/post/2Yf3UedF7C"),
            Some(Link::Post {
                id: "2Yf3UedF7C".into(),
                attachment: None
            })
        );
        assert_eq!(
            link("https://beta.floatplane.com/post/d870PEFXS1"),
            Some(Link::Post {
                id: "d870PEFXS1".into(),
                attachment: None
            })
        );
        assert_eq!(
            link("https://floatplane.com/post/65B5PNoBtf?attachment=ISPJjexylS"),
            Some(Link::Post {
                id: "65B5PNoBtf".into(),
                attachment: Some("ISPJjexylS".into())
            })
        );
        assert_eq!(
            link("https://www.floatplane.com/channel/linustechtips/home/ltxexpo"),
            Some(Link::Channel {
                creator: "linustechtips".into(),
                channel: Some("ltxexpo".into())
            })
        );
        assert_eq!(
            link("https://www.floatplane.com/channel/ShankMods/home"),
            Some(Link::Channel {
                creator: "ShankMods".into(),
                channel: None
            })
        );
        assert_eq!(
            link("https://beta.floatplane.com/channel/linustechtips/home"),
            Some(Link::Channel {
                creator: "linustechtips".into(),
                channel: None
            })
        );
        assert_eq!(link("https://www.floatplane.com/channel/ShankMods"), None);
        assert_eq!(link("https://www.floatplane.com/browse"), None);
        assert_eq!(link("https://www.sauceplus.com/post/YbBwIa2A5g"), None);
        assert_eq!(
            parse_link_on(
                &Url::parse("https://www.sauceplus.com/post/YbBwIa2A5g").unwrap(),
                "sauceplus.com"
            ),
            Some(Link::Post {
                id: "YbBwIa2A5g".into(),
                attachment: None
            })
        );
    }

    fn post_record(id: &str, videos: Vec<Value>, audios: Vec<Value>) -> Value {
        json!({
            "id": id, "title": "8K Yule Log Fireplace", "text": "<p>Crackling &amp; cosy</p>",
            "creator": {"id": "59f94c0bdd241b70349eb72b", "urlname": "linustechtips", "title": "LinusTechTips"},
            "channel": {"id": "63fe42c309e691e4e36de93d", "urlname": "main", "title": "Linus Tech Tips"},
            "releaseDate": "2019-12-06T18:30:00.000Z", "likes": 10, "dislikes": 1, "comments": 3,
            "thumbnail": {"path": "https://pbs.floatplane.com/blogPost_thumbnails/x/post.jpeg"},
            "metadata": {"hasVideo": !videos.is_empty(), "hasAudio": !audios.is_empty()},
            "videoAttachments": videos, "audioAttachments": audios
        })
    }

    fn delivery(cdn: &str, variants: Vec<Value>) -> Value {
        json!({"groups": [{"origins": [{"url": cdn}], "variants": variants}]})
    }

    #[tokio::test]
    async fn single_video_posts_resolve_to_hls_qualities() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.floatplane.com/api/v3/content/post?id=2Yf3UedF7C",
            200,
            "application/json",
            post_record(
                "2Yf3UedF7C",
                vec![json!({"id": "yuleLogLTT", "type": "video"})],
                vec![],
            )
            .to_string(),
        ));
        fixture.exchanges.push(get(
            "https://www.floatplane.com/api/v3/delivery/info?scenario=onDemand&entityId=yuleLogLTT",
            200,
            "application/json",
            delivery("https://cdn-vod-drm2.floatplane.com", vec![
                json!({"name": "1080p", "label": "1080p", "url": "/Videos/yuleLogLTT/1080.mp4/chunk.m3u8?token=t",
                       "meta": {"video": {"codec": "avc1.64002a", "width": 1920, "height": 1080, "fps": 30, "bitrate": {"average": 4000000}},
                                "audio": {"codec": "mp4a.40.2", "bitrate": {"average": 128000}, "channelCount": 2}}}),
                json!({"name": "360p", "label": "360p", "url": "/Videos/yuleLogLTT/360.mp4/chunk.m3u8?token=t",
                       "meta": {"video": {"codec": "avc1.64001e", "width": 640, "height": 360, "fps": 30, "bitrate": {"average": 800000}},
                                "audio": {"codec": "mp4a.40.2", "bitrate": {"average": 96000}}}}),
                json!({"name": "broken", "url": ""}),
            ]).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://www.floatplane.com/api/v3/content/video?id=yuleLogLTT",
            200,
            "application/json",
            json!({"id": "yuleLogLTT", "title": "8K Yule Log Fireplace with Crackling Fire Sounds - 10 Hours", "duration": 36035,
                   "thumbnail": {"path": "https://pbs.floatplane.com/video_thumbnails/yuleLogLTT/a.jpeg"},
                   "textTracks": [{"src": "https://pbs.floatplane.com/captions/yuleLogLTT/en.vtt", "language": "en", "generated": true},
                                  {"src": "https://pbs.floatplane.com/captions/yuleLogLTT/de.srt", "language": "de"}]})
            .to_string(),
        ));
        let resolver = FloatplaneResolver::new(logged_in(fixture));
        let url = Url::parse("https://www.floatplane.com/post/2Yf3UedF7C").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("yuleLogLTT"));
        assert_eq!(
            resolved.title.as_deref(),
            Some("8K Yule Log Fireplace with Crackling Fire Sounds - 10 Hours")
        );
        assert_eq!(resolved.description.as_deref(), Some("Crackling & cosy"));
        assert_eq!(resolved.uploader.as_deref(), Some("LinusTechTips"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://www.floatplane.com/channel/linustechtips/home"
        );
        assert_eq!(resolved.uploaded_at.unwrap().as_second(), 1575657000);
        assert_eq!(resolved.duration, Some(Duration::from_secs(36035)));
        assert_eq!(
            resolved.thumbnail.as_ref().unwrap().as_str(),
            "https://pbs.floatplane.com/video_thumbnails/yuleLogLTT/a.jpeg"
        );
        assert_eq!(resolved.variants.len(), 2);
        let best = &resolved.variants[0];
        assert_eq!(best.kind, VariantKind::Hls);
        assert_eq!(
            best.url.as_str(),
            "https://cdn-vod-drm2.floatplane.com/Videos/yuleLogLTT/1080.mp4/chunk.m3u8?token=t"
        );
        assert_eq!(best.height, Some(1080));
        assert_eq!(best.fps, Some(30.0));
        assert_eq!(best.bitrate, Some(4128000));
        assert_eq!(best.format_id.as_deref(), Some("1080p"));
        assert_eq!(best.codecs.as_deref(), Some("avc1.64002a,mp4a.40.2"));
        assert_eq!(best.video, Some(crate::media::VideoCodec::H264));
        assert!(!best.audio_only);
        assert!(
            best.headers
                .iter()
                .any(|(k, v)| k == "origin" && v == "https://www.floatplane.com")
        );
        assert_eq!(resolved.subtitles.len(), 2);
        assert!(resolved.subtitles[0].auto);
        assert_eq!(resolved.subtitles[0].format, SubtitleFormat::Vtt);
        assert_eq!(resolved.subtitles[1].language, "de");
        assert_eq!(resolved.subtitles[1].format, SubtitleFormat::Srt);
        assert!(!resolved.subtitles[1].auto);
    }

    #[tokio::test]
    async fn posts_with_several_attachments_list_them_and_each_resolves() {
        let record = post_record(
            "65B5PNoBtf",
            vec![json!({"id": "ISPJjexylS", "type": "video"})],
            vec![json!({"id": "qKfxu6fEpu", "type": "audio"})],
        );
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.floatplane.com/api/v3/content/post?id=65B5PNoBtf",
            200,
            "application/json",
            record.to_string(),
        ));
        fixture.exchanges.push(get(
            "https://www.floatplane.com/api/v3/content/video?id=ISPJjexylS",
            200,
            "application/json",
            json!({"title": "The $50 electronic drum kit. .mov", "duration": 622}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://www.floatplane.com/api/v3/content/audio?id=qKfxu6fEpu",
            200,
            "application/json",
            json!({"title": "Roland TD-7 Demo.m4a", "duration": 114}).to_string(),
        ));
        let resolver = FloatplaneResolver::new(logged_in(fixture));
        let url = Url::parse("https://www.floatplane.com/post/65B5PNoBtf").unwrap();
        let Resolution::Playlist(playlist) = resolver.resolve(&url).await.unwrap() else {
            panic!("expected a playlist");
        };
        assert_eq!(playlist.id.as_deref(), Some("65B5PNoBtf"));
        assert_eq!(playlist.title.as_deref(), Some("8K Yule Log Fireplace"));
        assert_eq!(playlist.entries.len(), 2);
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://www.floatplane.com/post/65B5PNoBtf?attachment=ISPJjexylS"
        );
        assert_eq!(
            playlist.entries[0].title.as_deref(),
            Some("The $50 electronic drum kit. .mov")
        );
        assert_eq!(playlist.entries[1].duration, Some(Duration::from_secs(114)));
        assert!(resolver.matches(&playlist.entries[1].url));

        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.floatplane.com/api/v3/content/post?id=65B5PNoBtf",
            200,
            "application/json",
            record.to_string(),
        ));
        fixture.exchanges.push(get(
            "https://www.floatplane.com/api/v3/delivery/info?scenario=onDemand&entityId=qKfxu6fEpu",
            200,
            "application/json",
            delivery("https://cdn-vod-drm2.floatplane.com", vec![
                json!({"name": "original", "label": "Original", "url": "/Audio/qKfxu6fEpu/original.m4a?token=t",
                       "meta": {"audio": {"codec": "mp4a.40.2", "bitrate": {"average": 192000}, "channelCount": 2}}}),
            ]).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://www.floatplane.com/api/v3/content/audio?id=qKfxu6fEpu",
            500,
            "text/plain",
            "boom".into(),
        ));
        let resolver = FloatplaneResolver::new(logged_in(fixture));
        let resolved = resolver
            .resolve(&playlist.entries[1].url)
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.id.as_deref(), Some("qKfxu6fEpu"));
        assert_eq!(resolved.title.as_deref(), Some("8K Yule Log Fireplace"));
        assert_eq!(resolved.variants.len(), 1);
        let audio = &resolved.variants[0];
        assert_eq!(audio.kind, VariantKind::File);
        assert!(audio.audio_only);
        assert_eq!(audio.bitrate, Some(192000));
        assert_eq!(audio.audio, Some(crate::media::AudioCodec::Aac));
        assert_eq!(
            audio.url.as_str(),
            "https://cdn-vod-drm2.floatplane.com/Audio/qKfxu6fEpu/original.m4a?token=t"
        );
    }

    #[tokio::test]
    async fn channels_list_posts_page_by_page() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.floatplane.com/api/v3/creator/named?creatorURL%5B0%5D=linustechtips",
            200,
            "application/json",
            json!([{"id": "59f94c0bdd241b70349eb72b", "title": "LinusTechTips", "about": "Tech",
                    "channels": [{"id": "63fe42c309e691e4e36de93d", "urlname": "main", "title": "Linus Tech Tips"},
                                 {"id": "64135f82fc76ab7f9fbdc876", "urlname": "ltxexpo", "title": "LTX Expo", "about": "Expo"}]}])
            .to_string(),
        ));
        let first_page: Vec<Value> = (0..PAGE_SIZE)
            .map(|i| json!({"id": format!("post{i}"), "title": format!("Post {i}"), "releaseDate": "2023-01-01T00:00:00.000Z"}))
            .collect();
        fixture.exchanges.push(get(
            "https://www.floatplane.com/api/v3/content/creator?id=59f94c0bdd241b70349eb72b&limit=20&fetchAfter=0&channel=64135f82fc76ab7f9fbdc876",
            200,
            "application/json",
            Value::Array(first_page).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://www.floatplane.com/api/v3/content/creator?id=59f94c0bdd241b70349eb72b&limit=20&fetchAfter=20&channel=64135f82fc76ab7f9fbdc876",
            200,
            "application/json",
            json!([{"id": "last", "title": "Last one"}]).to_string(),
        ));
        let resolver = FloatplaneResolver::new(Http::replay(fixture));
        let url =
            Url::parse("https://www.floatplane.com/channel/linustechtips/home/ltxexpo").unwrap();
        let Resolution::Playlist(playlist) = resolver.resolve(&url).await.unwrap() else {
            panic!("expected a playlist");
        };
        assert_eq!(playlist.id.as_deref(), Some("linustechtips/ltxexpo"));
        assert_eq!(playlist.title.as_deref(), Some("LTX Expo"));
        assert_eq!(playlist.entries.len(), PAGE_SIZE + 1);
        assert_eq!(
            playlist.entries[0].url.as_str(),
            "https://www.floatplane.com/post/post0"
        );
        assert_eq!(
            playlist.entries[PAGE_SIZE].title.as_deref(),
            Some("Last one")
        );
        assert!(resolver.matches(&playlist.entries[0].url));
    }

    #[tokio::test]
    async fn locked_and_missing_posts_say_so() {
        let resolver = FloatplaneResolver::new(Http::replay(Fixture::new(PLATFORM, None)));
        let url = Url::parse("https://www.floatplane.com/post/2Yf3UedF7C").unwrap();
        assert!(matches!(
            resolver.resolve(&url).await.unwrap_err(),
            ResolveError::LoginRequired {
                platform: "floatplane",
                ..
            }
        ));

        // A stored cookie whose session has expired is refused the way a missing one is.
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.floatplane.com/api/v3/content/post?id=NfPch1ksHq",
            403,
            "application/json",
            json!({"id": "kdvh-lafg-ppjt", "errors": [
                {"id": "pnny-gqqm-hvgd", "name": "notLoggedInError", "message": "You must be logged-in to access this resource."},
                {"id": "devf-4cag-0q37", "name": "notSubscribedError", "message": "You do not have the necessary subscription to access this item."}
            ], "message": "You must be logged-in to access this resource."}).to_string(),
        ));
        let resolver = FloatplaneResolver::new(logged_in(fixture));
        let error = resolver
            .resolve(&Url::parse("https://www.floatplane.com/post/NfPch1ksHq").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::LoginRequired { platform: "floatplane", reason, .. } if reason.contains("logged-in")),
            "{error}"
        );

        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.floatplane.com/api/v3/content/post?id=65B5PNoBtf",
            403,
            "application/json",
            json!({"errors": [{"id": "x", "name": "notSubscribedError", "message": "You must be subscribed to The Trash Network."}]}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://www.floatplane.com/api/v3/content/post?id=gone",
            404,
            "application/json",
            json!({"errors": [{"message": "not found"}]}).to_string(),
        ));
        fixture.exchanges.push(get(
            "https://www.floatplane.com/api/v3/content/post?id=text",
            200,
            "application/json",
            json!({"id": "text", "title": "Just words", "metadata": {"hasVideo": false, "hasAudio": false}}).to_string(),
        ));
        let resolver = FloatplaneResolver::new(logged_in(fixture));
        let error = resolver
            .resolve(&Url::parse("https://www.floatplane.com/post/65B5PNoBtf").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("The Trash Network")),
            "{error}"
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://www.floatplane.com/post/gone").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(&Url::parse("https://www.floatplane.com/post/text").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("no video or audio")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn sessions_are_named_by_the_account_endpoint() {
        let resolver = FloatplaneResolver::new(Http::replay(Fixture::new(PLATFORM, None)));
        assert!(matches!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedOut
        ));

        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.floatplane.com/api/v3/user/self",
            200,
            "application/json",
            json!({"id": "abc", "username": "floatie"}).to_string(),
        ));
        let resolver = FloatplaneResolver::new(logged_in(fixture));
        assert!(matches!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedIn { account } if account == "floatie"
        ));

        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(get(
            "https://www.floatplane.com/api/v3/user/self",
            401,
            "application/json",
            json!({"errors": [{"message": "not logged in"}]}).to_string(),
        ));
        let resolver = FloatplaneResolver::new(logged_in(fixture));
        assert!(matches!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedOut
        ));
    }

    #[test]
    fn hls_media_playlists_are_recognised_by_path() {
        assert_eq!(
            variant_kind(
                &Url::parse("https://cdn.test/Videos/a/1080.mp4/chunk.m3u8?token=t").unwrap()
            ),
            VariantKind::Hls
        );
        assert_eq!(
            variant_kind(&Url::parse("https://cdn.test/Audio/a/original.m4a?token=t").unwrap()),
            VariantKind::File
        );
        let _ = MEDIA_PLAYLIST;
    }
}
