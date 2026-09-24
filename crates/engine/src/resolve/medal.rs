//! Medal.tv game clips, users and categories. A clip page carries the clip in its
//! hydration data: the source MP4, the MP4 renditions by height, the HLS playlist and
//! the clip's title, length, poster and game. The Next.js pages stream that data in
//! React Server Components rows and the profile pages set it in a `hydrationData`
//! variable; both are read. Users and categories list their clips through the site's
//! content API, which answers a guest account the site hands out on request, the way
//! the web app signs itself in before it lists anything.

use std::sync::{LazyLock, Mutex};

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};
use url::Url;

use super::{
    MAX_PAGE, Page, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved,
    Resolver, SessionSupport, Tag, Variant, clean_title, fetch, hls, navigation_headers, page,
    status_error, util,
};
use crate::http::{BROWSER_UA, Http};
use crate::media::{AudioCodec, Container, MediaKind, VideoCodec};

pub const PLATFORM: &str = "medal";
const SITE: &str = "https://medal.tv";
const API: &str = "https://medal.tv/api";
/// How the web app names itself to the API.
const MEDAL_UA: &str = "Medal-Nextjs/1.0 (simplified_signup)";
/// How many clips a user or category listing is read up to: the most the API lists at
/// once.
pub const LISTING_LIMIT: usize = 50;

static RE_CLIP_ID: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9_-]{5,}$").unwrap());
static RE_SLUG: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[a-z0-9][a-z0-9_-]*$").unwrap());
/// `contentUrl720p`: a rendition keyed by its height.
static RE_RENDITION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^contentUrl(\d+)p$").unwrap());

/// A user, by the id in a `/users/{id}` link or the name in a `/u/{name}` one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserRef {
    Id(String),
    Name(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    Clip {
        id: String,
    },
    User(UserRef),
    /// A game's clips, by the game's slug.
    Category {
        slug: String,
    },
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "medal.tv" && host != "www.medal.tv" {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let clip = |id: &str| {
        RE_CLIP_ID
            .is_match(id)
            .then(|| Link::Clip { id: id.to_string() })
    };
    match segments.as_slice() {
        ["games", _, "clips" | "clip", id] => clip(id),
        ["clips" | "clip", id] => clip(id),
        [] => util::query_param(url, "contentId").and_then(|id| clip(&id)),
        ["users", id] if id.chars().all(|c| c.is_ascii_digit()) => {
            Some(Link::User(UserRef::Id(id.to_string())))
        }
        ["u", name] if !name.is_empty() => Some(Link::User(UserRef::Name(name.to_string()))),
        ["games", slug] if RE_SLUG.is_match(slug) => Some(Link::Category {
            slug: slug.to_string(),
        }),
        _ => None,
    }
}

/// The JSON rows of a React Server Components payload: `<id>:<json>` lines, stepping
/// over the `<id>:T<hex length>,<text>` rows, whose text runs for as many bytes as the
/// length says and ends without a line break.
pub fn flight_rows(flight: &str) -> Vec<&str> {
    let mut rows = Vec::new();
    let mut pos = 0;
    while pos < flight.len() {
        let Some(colon) = flight[pos..].find(':') else {
            break;
        };
        let id = &flight[pos..pos + colon];
        let after = pos + colon + 1;
        let line_end = flight[pos..].find('\n').map_or(flight.len(), |i| pos + i);
        if id.is_empty() || id.len() > 8 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            pos = line_end + 1;
            continue;
        }
        if flight.as_bytes().get(after) == Some(&b'T')
            && let Some(comma) = flight[after..].find(',')
            && let Ok(length) = usize::from_str_radix(&flight[after + 1..after + comma], 16)
        {
            let mut next = (after + comma + 1 + length).min(flight.len());
            while next < flight.len() && !flight.is_char_boundary(next) {
                next += 1;
            }
            pos = next;
            continue;
        }
        let end = flight[after..]
            .find('\n')
            .map_or(flight.len(), |i| after + i);
        rows.push(&flight[after..end]);
        pos = end + 1;
    }
    rows
}

/// The object `accept` holds for among the JSON a page hydrates itself with: the React
/// Server Components rows that mention `needle`, then a `hydrationData` variable.
pub fn page_object(html: &str, needle: &str, accept: &dyn Fn(&Value) -> bool) -> Option<Value> {
    if let Some(flight) = page::next_flight_data(html) {
        for row in flight_rows(&flight) {
            if !row.contains(needle) {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(row)
                && let Some(found) = util::find_object(&value, accept)
            {
                return Some(found.clone());
            }
        }
    }
    let hydration = page::json_after(html, "var hydrationData=")?;
    util::find_object(&hydration, accept).cloned()
}

/// The clip with `id` on its page.
pub fn clip_on_page(html: &str, id: &str) -> Option<Value> {
    let needle = format!("\"contentId\":\"{id}\"");
    page_object(html, &needle, &|object| {
        object["contentId"].as_str() == Some(id) && object.get("contentUrl").is_some()
    })
}

/// The category with `slug` on its page.
pub fn category_on_page(html: &str, slug: &str) -> Option<Value> {
    let needle = format!("\"slug\":\"{slug}\"");
    page_object(html, &needle, &|object| {
        object["slug"].as_str() == Some(slug) && object["categoryId"].is_string()
    })
}

/// A link the site withholds from guests.
fn is_withheld(url: &Url) -> bool {
    url.as_str().contains("privacy-protected-guest")
}

fn mp4_variant(url: Url, format_id: &str, label: &str) -> Variant {
    let mut variant = Variant::file(url);
    variant.container = Some(Container::Mp4);
    variant.video = Some(VideoCodec::H264);
    variant.audio = Some(AudioCodec::Aac);
    variant.format_id = Some(format_id.to_string());
    variant.label = Some(label.to_string());
    variant
}

/// The MP4 files a clip names: its source, and the renditions by height that exist (a
/// rendition the site marks `missing` is the source under another name).
pub fn clip_files(clip: &Value) -> Vec<Variant> {
    let mut variants = Vec::new();
    if let Some(source) = util::url_of(&clip["contentUrl"], None).filter(|u| !is_withheld(u)) {
        let mut variant = mp4_variant(source, "source", "source");
        variant.width = util::u32_of(&clip["sourceWidth"]).filter(|w| *w > 0);
        variant.height = util::u32_of(&clip["sourceHeight"]).filter(|h| *h > 0);
        if let Some(height) = variant.height {
            variant.label = Some(format!("{height}p source"));
        }
        variants.push(variant);
    }
    for (key, value) in clip.as_object().into_iter().flatten() {
        let Some(height) = util::search(&RE_RENDITION, key).and_then(|h| h.parse::<u32>().ok())
        else {
            continue;
        };
        let Some(url) = util::url_of(value, None).filter(|u| !is_withheld(u)) else {
            continue;
        };
        if url.query_pairs().any(|(k, _)| k == "missing") {
            continue;
        }
        let mut variant = mp4_variant(url, &format!("mp4-{height}p"), &format!("{height}p"));
        variant.height = Some(height);
        variants.push(variant);
    }
    variants.sort_by_key(|v| std::cmp::Reverse(v.height));
    variants
}

fn entry_of(item: &Value) -> Option<PlaylistEntry> {
    let id = util::text(&item["contentId"])?;
    let url = util::url_of(&item["contentShareUrl"], None)
        .or_else(|| Url::parse(&format!("{SITE}/clips/{id}")).ok())?;
    Some(PlaylistEntry {
        url,
        title: util::text(&item["contentTitle"]).and_then(|t| clean_title(&t)),
        duration: util::seconds(&item["videoLengthSeconds"]).filter(|d| !d.is_zero()),
    })
}

pub struct MedalResolver {
    http: Http,
    /// The guest account's `X-Authentication` value, once the site handed one out.
    guest: Mutex<Option<String>>,
}

impl MedalResolver {
    pub fn new(http: Http) -> Self {
        Self {
            http,
            guest: Mutex::new(None),
        }
    }

    /// The `X-Authentication` value of a guest account: the one already handed out, or a
    /// new one the site creates for a `guest` sign-up, as the web app does on first load.
    async fn guest_auth(&self, origin: &Url) -> Result<String, ResolveError> {
        if let Some(auth) = self.guest.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return Ok(auth);
        }
        let response = self
            .http
            .post(Url::parse(&format!("{API}/users")).expect("valid"))
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "application/json")
            .header("medal-user-agent", MEDAL_UA)
            .json(&json!({
                "userName": "guest",
                "email": "guest",
                "password": util::random_uuid(),
            }))
            .send()
            .await?;
        if response.status.as_u16() == 429 {
            return Err(ResolveError::RateLimited(origin.clone()));
        }
        if !response.status.is_success() {
            return Err(ResolveError::unavailable(
                origin,
                format!(
                    "the site refused a guest account with HTTP {}",
                    response.status
                ),
            ));
        }
        let answer: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, format!("guest account: {e}")))?;
        let user_id = util::text(&answer["auth"]["userId"])
            .or_else(|| util::text(&answer["user"]["userId"]))
            .filter(|id| !id.is_empty());
        let key = util::text(&answer["auth"]["key"]).filter(|key| !key.is_empty());
        let (Some(user_id), Some(key)) = (user_id, key) else {
            return Err(ResolveError::malformed(
                origin,
                "the guest account came without credentials",
            ));
        };
        let auth = format!("{user_id},{key}");
        *self.guest.lock().unwrap_or_else(|e| e.into_inner()) = Some(auth.clone());
        Ok(auth)
    }

    /// A JSON answer of the content API, asked as the guest account.
    async fn api(&self, path: &str, origin: &Url) -> Result<Value, ResolveError> {
        let auth = self.guest_auth(origin).await?;
        let url = Url::parse(&format!("{API}/{path}"))
            .map_err(|e| ResolveError::malformed(origin, e.to_string()))?;
        let response = self
            .http
            .get(url)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "application/json")
            .header("medal-user-agent", MEDAL_UA)
            .header("x-authentication", &auth)
            .send()
            .await?;
        let status = response.status;
        let text = response.text(MAX_PAGE).await?;
        let answer: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let message = || {
            util::text(&answer["errorMessage"])
                .unwrap_or_else(|| format!("the API answered HTTP {status}"))
        };
        let details = || {
            let mut text = message();
            for info in answer["additionalInfo"].as_array().into_iter().flatten() {
                if let Some(info) = info.as_str() {
                    text.push_str(": ");
                    text.push_str(info);
                }
            }
            text
        };
        match status.as_u16() {
            200..=299 => Ok(answer),
            404 => Err(ResolveError::NotFound(origin.clone())),
            400 if details().to_ascii_lowercase().contains("invalid id") => {
                Err(ResolveError::NotFound(origin.clone()))
            }
            400 => Err(ResolveError::unavailable(origin, details())),
            401 => {
                *self.guest.lock().unwrap_or_else(|e| e.into_inner()) = None;
                Err(ResolveError::unavailable(origin, message()))
            }
            429 => Err(ResolveError::RateLimited(origin.clone())),
            _ => Err(ResolveError::unavailable(origin, message())),
        }
    }

    /// The clips a listing lists, newest or most viewed first as the site sorts them.
    async fn listing(
        &self,
        filter: &str,
        sort: &str,
        origin: &Url,
    ) -> Result<Vec<PlaylistEntry>, ResolveError> {
        let answer = self
            .api(
                &format!(
                    "content?newPagination=true&{filter}&limit={LISTING_LIMIT}&sortDirection=DESC&sortBy={sort}"
                ),
                origin,
            )
            .await?;
        Ok(answer["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(entry_of)
            .collect())
    }

    async fn resolve_clip(&self, id: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let page_url = Url::parse(&format!("{SITE}/clips/{id}")).expect("valid");
        let fetched = fetch(
            &self.http,
            &page_url,
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
        let clip = clip_on_page(&html, id)
            .ok_or_else(|| ResolveError::malformed(origin, "the page hydrates no clip"))?;
        let mut variants = clip_files(&clip);
        let mut subtitles = Vec::new();
        if let Some(playlist) =
            util::url_of(&clip["contentUrlHls"], None).filter(|u| !is_withheld(u))
        {
            let expanded = hls::expand(&self.http, &playlist, PLATFORM, BROWSER_UA, &[])
                .await
                .map_err(|error| error.at(origin))?;
            for mut variant in expanded.variants {
                variant.format_id = Some(match &variant.label {
                    Some(label) => format!("hls-{label}"),
                    None => "hls".to_string(),
                });
                variants.push(variant);
            }
            subtitles = expanded.subtitles;
        }
        if variants.is_empty() {
            variants.push(self.social_video(id, origin).await?);
        }
        let page = Page::parse(&html, &fetched.url);
        let poster = &clip["poster"];
        let mut resolved = Resolved::new(PLATFORM);
        resolved.id = Some(id.to_string());
        resolved.title = util::text(&clip["contentTitle"])
            .and_then(|t| clean_title(&t))
            .or_else(|| page.title());
        resolved.description =
            util::text(&clip["contentDescription"]).and_then(|d| clean_title(&d));
        resolved.uploader = util::text(&poster["displayName"])
            .or_else(|| util::text(&poster["userName"]))
            .and_then(|n| clean_title(&n));
        resolved.uploader_url = util::text(&poster["userName"])
            .and_then(|name| Url::parse(&format!("{SITE}/u/{name}")).ok())
            .or_else(|| {
                util::text(&poster["userId"])
                    .and_then(|id| Url::parse(&format!("{SITE}/users/{id}")).ok())
            });
        resolved.uploaded_at =
            util::epoch(&clip["publishedAt"]).or_else(|| util::epoch(&clip["created"]));
        resolved.duration = util::seconds(&clip["videoLengthSeconds"]).filter(|d| !d.is_zero());
        resolved.thumbnail = util::url_of(&clip["thumbnailUrl"], None).or_else(|| page.poster());
        resolved.webpage_url =
            util::url_of(&clip["contentShareUrl"], None).or_else(|| Some(fetched.url.clone()));
        resolved.subtitles = subtitles;
        resolved.variants = variants;
        Ok(Resolution::from(resolved))
    }

    /// The file behind the social video link, which the site serves for clips whose
    /// other links it withholds from guests: the redirect the endpoint answers with.
    async fn social_video(&self, id: &str, origin: &Url) -> Result<Variant, ResolveError> {
        let endpoint = Url::parse(&format!("{API}/content/{id}/socialVideoUrl")).expect("valid");
        let response = self
            .http
            .get(endpoint)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .follow_redirects(false)
            .send()
            .await?;
        let target = response
            .header("location")
            .and_then(|l| util::join_url(Some(&response.url), l));
        match (response.status.as_u16(), target) {
            (300..=399, Some(url)) => Ok(mp4_variant(url, "social", "social")),
            (404, _) => Err(ResolveError::NotFound(origin.clone())),
            (429, _) => Err(ResolveError::RateLimited(origin.clone())),
            (code, _) => Err(ResolveError::unavailable(
                origin,
                format!(
                    "the clip's links are withheld from guests and the social video endpoint answered HTTP {code}"
                ),
            )),
        }
    }

    async fn resolve_user(&self, user: &UserRef, origin: &Url) -> Result<Resolution, ResolveError> {
        let profile = match user {
            UserRef::Id(id) => self.api(&format!("users/{id}"), origin).await?,
            UserRef::Name(name) => {
                let found = self
                    .api(
                        &format!("users?username={}", util::url_encode(name)),
                        origin,
                    )
                    .await?;
                let users = found.as_array().cloned().unwrap_or_default();
                users
                    .iter()
                    .find(|u| {
                        u["userName"]
                            .as_str()
                            .is_some_and(|n| n.eq_ignore_ascii_case(name))
                    })
                    .cloned()
                    .ok_or_else(|| ResolveError::NotFound(origin.clone()))?
            }
        };
        let id = util::text(&profile["userId"])
            .ok_or_else(|| ResolveError::malformed(origin, "the profile has no user id"))?;
        let entries = self
            .listing(&format!("userId={id}"), "publishedAt", origin)
            .await?;
        if entries.is_empty() {
            return Err(ResolveError::unavailable(
                origin,
                "the user has published no clips",
            ));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(id),
            title: util::text(&profile["displayName"])
                .or_else(|| util::text(&profile["userName"]))
                .and_then(|n| clean_title(&n)),
            total: util::uint(&profile["submissions"])
                .map(|n| n as usize)
                .filter(|n| *n >= entries.len())
                .or(Some(entries.len())),
            entries,
        }))
    }

    async fn resolve_category(&self, slug: &str, origin: &Url) -> Result<Resolution, ResolveError> {
        let page_url = Url::parse(&format!("{SITE}/games/{slug}")).expect("valid");
        let fetched = fetch(
            &self.http,
            &page_url,
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
        let category = category_on_page(&html, slug)
            .ok_or_else(|| ResolveError::malformed(origin, "the page hydrates no category"))?;
        let id = util::text(&category["categoryId"])
            .ok_or_else(|| ResolveError::malformed(origin, "the category has no id"))?;
        let entries = self
            .listing(&format!("categoryId={id}"), "views", origin)
            .await?;
        if entries.is_empty() {
            return Err(ResolveError::unavailable(
                origin,
                "the game has no published clips",
            ));
        }
        Ok(Resolution::Playlist(Playlist {
            resolver: PLATFORM.to_string(),
            id: Some(slug.to_string()),
            title: util::text(&category["categoryName"])
                .or_else(|| util::text(&category["alternativeName"]))
                .and_then(|n| clean_title(&n)),
            total: util::uint(&category["publishedClipCount"])
                .map(|n| n as usize)
                .filter(|n| *n >= entries.len())
                .or(Some(entries.len())),
            entries,
        }))
    }
}

#[async_trait]
impl Resolver for MedalResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Medal.tv",
            hosts: &["medal.tv"],
            features: &["clips", "embeds", "users", "categories"],
            formats: &["mp4", "hls"],
            media: &[MediaKind::Video],
            tags: &[Tag::Video],
            session: SessionSupport::None,
            examples: &[
                "https://medal.tv/games/valorant/clips/jTBFnLKdLy15K",
                "https://medal.tv/clips/2um24TWdty0NA",
                "https://medal.tv/games/valorant/clip/jTBFnLKdLy15K",
                "https://medal.tv/u/aciel",
                "https://medal.tv/users/19335460",
                "https://medal.tv/games/valorant",
            ],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        match parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))? {
            Link::Clip { id } => self.resolve_clip(&id, url).await,
            Link::User(user) => self.resolve_user(&user, url).await,
            Link::Category { slug } => self.resolve_category(&slug, url).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::http::transport::{
        Exchange, Fixture, RecordedBody, RecordedRequest, RecordedResponse,
    };

    fn exchange(method: &str, url: &str, status: u16, content_type: &str, body: &str) -> Exchange {
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
                body: RecordedBody::Text(body.into()),
                truncated: false,
            },
        }
    }

    fn clip(id: &str, title: &str, with_hls: bool) -> Value {
        let mut clip = json!({
            "contentId": id, "contentTitle": title, "contentDescription": "",
            "contentShareUrl": format!("https://medal.tv/games/valorant/clips/{id}"),
            "contentUrl": format!("https://cdn.medal.tv/mediap/{id}.mp4?auth=exp=1"),
            "contentUrl1080p": format!("https://cdn.medal.tv/mediap/{id}.mp4?auth=exp=1&t=1080p&c=2&missing"),
            "contentUrl720p": format!("https://cdn.medal.tv/mediap/{id}.mp4?auth=exp=1&t=720p&c=2"),
            "contentUrl360p": format!("https://cdn.medal.tv/mediap/{id}.mp4?auth=exp=1&t=360p&c=2"),
            "created": 1651628243000i64, "publishedAt": 1651628816000i64,
            "videoLengthSeconds": 13.0, "sourceWidth": 1920, "sourceHeight": 1080,
            "thumbnailUrl": format!("https://cdn.medal.tv/ugcp/content-thumbnail/{id}.jpg"),
            "poster": {"displayName": "aciel", "userId": "19335460", "userName": "aciel"},
            "category": {"categoryId": "fW3AZxHf_c", "categoryName": "Valorant", "slug": "valorant"},
            "views": 75726
        });
        if with_hls {
            clip["contentUrlHls"] =
                json!(format!("https://medal.tv/api/hls/{id}/master.m3u8?bebit=x"));
        }
        clip
    }

    /// A clip page as the Next.js app streams it: the clip inside a server components
    /// row, after a text row (the JSON-LD, which also mentions the clip) that runs for
    /// its byte length and ends without a line break.
    fn clip_page(clip: &Value) -> String {
        let ld = json!({"@type": "VideoObject", "name": "Mornu's clutch", "@id": format!("https://medal.tv/games/valorant/clips/{}", clip["contentId"].as_str().unwrap()), "description": "with a contentId \"contentId\":\"x\" inside"}).to_string();
        let row = json!(["$", "$L46", null, {"clip": clip, "profileColor": "hsl(134 69% 5%)"}]);
        let escaped = serde_json::to_string(&format!("5c:T{:x},{ld}5e:{row}\n", ld.len())).unwrap();
        format!(
            r#"<html><head><title>Mornu's clutch | Medal</title></head><body><script>self.__next_f.push([1,"0:[\"$\",\"$L1\",null,{{}}]\n"])</script><script>self.__next_f.push([1,{escaped}])</script></body></html>"#
        )
    }

    #[test]
    fn links_are_read() {
        let link = |s: &str| parse_link(&Url::parse(s).unwrap());
        let clip = |id: &str| Some(Link::Clip { id: id.into() });
        assert_eq!(
            link("https://medal.tv/games/valorant/clips/jTBFnLKdLy15K"),
            clip("jTBFnLKdLy15K")
        );
        assert_eq!(
            link("https://medal.tv/games/cod-cold-war/clips/2um24TWdty0NA?invite=cr-x"),
            clip("2um24TWdty0NA")
        );
        assert_eq!(
            link("https://medal.tv/games/valorant/clip/jTBFnLKdLy15K"),
            clip("jTBFnLKdLy15K")
        );
        assert_eq!(
            link("https://medal.tv/clips/2um24TWdty0NA"),
            clip("2um24TWdty0NA")
        );
        assert_eq!(
            link("https://medal.tv/clip/2um24TWdty0NA"),
            clip("2um24TWdty0NA")
        );
        assert_eq!(
            link("https://medal.tv/?contentId=jTBFnLKdLy15K"),
            clip("jTBFnLKdLy15K")
        );
        assert_eq!(
            link("https://medal.tv/users/19335460"),
            Some(Link::User(UserRef::Id("19335460".into())))
        );
        assert_eq!(
            link("https://medal.tv/u/aciel"),
            Some(Link::User(UserRef::Name("aciel".into())))
        );
        assert_eq!(
            link("https://medal.tv/games/valorant"),
            Some(Link::Category {
                slug: "valorant".into()
            })
        );
        assert_eq!(link("https://medal.tv/"), None);
        assert_eq!(link("https://medal.tv/games"), None);
        assert_eq!(link("https://medal.tv/games/valorant/clips"), None);
        assert_eq!(link("https://medal.tv/login"), None);
        assert_eq!(link("https://example.com/clips/2um24TWdty0NA"), None);
    }

    #[test]
    fn clips_are_found_in_server_rows_and_hydration_variables() {
        let page = clip_page(&clip("jTBFnLKdLy15K", "Mornu's clutch", true));
        let found = clip_on_page(&page, "jTBFnLKdLy15K").unwrap();
        assert_eq!(found["contentTitle"], "Mornu's clutch");
        assert_eq!(clip_on_page(&page, "other"), None);
        let vite = format!(
            r#"<html><script>var hydrationData={};</script></html>"#,
            json!({"clips": {"nxYcrMeewonJLGFnf": clip("nxYcrMeewonJLGFnf", "clip dump", false)}, "profiles": {}})
        );
        let found = clip_on_page(&vite, "nxYcrMeewonJLGFnf").unwrap();
        assert_eq!(found["contentTitle"], "clip dump");
        let files = clip_files(&found);
        assert_eq!(
            files.len(),
            3,
            "the source and the two renditions that exist"
        );
        assert_eq!(files[0].height, Some(1080));
        assert_eq!(files[0].format_id.as_deref(), Some("source"));
        assert_eq!(files[1].height, Some(720));
        assert_eq!(files[2].label.as_deref(), Some("360p"));
        let category_page = r#"<html><script>self.__next_f.push([1,"46:[\"$\",\"$L46\",null,{\"category\":{\"categoryId\":\"fW3AZxHf_c\",\"categoryName\":\"Valorant\",\"igdbGame\":{\"slug\":\"valorant\"},\"publishedClipCount\":44543743,\"slug\":\"valorant\"}}]\n"])</script></html>"#;
        let category = category_on_page(category_page, "valorant").unwrap();
        assert_eq!(category["categoryId"], "fW3AZxHf_c");
        assert_eq!(category_on_page(category_page, "cs2"), None);
    }

    #[tokio::test]
    async fn clips_resolve_with_files_and_hls_renditions() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "GET",
            "https://medal.tv/clips/jTBFnLKdLy15K",
            200,
            "text/html",
            &clip_page(&clip("jTBFnLKdLy15K", "Mornu's clutch", true)),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://medal.tv/api/hls/jTBFnLKdLy15K/master.m3u8?bebit=x",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=9972000,RESOLUTION=1920x1080,FRAME-RATE=60,CODECS=\"avc1.64002a,mp4a.40.2\"\n1080.m3u8\n",
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://medal.tv/api/hls/jTBFnLKdLy15K/1080.m3u8",
            200,
            "application/vnd.apple.mpegurl",
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXTINF:4.0,\n0.ts\n#EXT-X-ENDLIST\n",
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://medal.tv/clips/zzzzzzzzzzzzz",
            404,
            "text/html",
            "<html>404</html>",
        ));
        let resolver = MedalResolver::new(Http::replay(fixture));
        let url = Url::parse("https://medal.tv/games/valorant/clips/jTBFnLKdLy15K").unwrap();
        assert!(resolver.matches(&url));
        let resolved = resolver.resolve(&url).await.unwrap().media().unwrap();
        assert_eq!(resolved.id.as_deref(), Some("jTBFnLKdLy15K"));
        assert_eq!(resolved.title.as_deref(), Some("Mornu's clutch"));
        assert_eq!(resolved.uploader.as_deref(), Some("aciel"));
        assert_eq!(
            resolved.uploader_url.as_ref().unwrap().as_str(),
            "https://medal.tv/u/aciel"
        );
        assert_eq!(resolved.duration, Some(Duration::from_secs(13)));
        assert!(resolved.uploaded_at.is_some());
        assert!(resolved.thumbnail.is_some());
        assert_eq!(
            resolved.webpage_url.as_ref().unwrap().as_str(),
            "https://medal.tv/games/valorant/clips/jTBFnLKdLy15K"
        );
        assert_eq!(
            resolved.variants.len(),
            4,
            "source, 720p, 360p and the HLS rendition"
        );
        assert_eq!(resolved.variants[0].format_id.as_deref(), Some("source"));
        assert_eq!(resolved.variants[0].width, Some(1920));
        let hls = resolved
            .variants
            .iter()
            .find(|v| v.format_id.as_deref() == Some("hls-1080p60"))
            .unwrap();
        assert_eq!(hls.fps, Some(60.0));
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://medal.tv/clips/zzzzzzzzzzzzz").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn withheld_clips_come_through_the_social_video_link() {
        let mut withheld = clip("2WRj40tpY_EU9", "1v5 clutch", false);
        for key in ["contentUrl", "contentUrl720p", "contentUrl360p"] {
            withheld[key] = json!("https://cdn.medal.tv/video/privacy-protected-guest.mp4");
        }
        withheld["contentUrl1080p"] = json!("");
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "GET",
            "https://medal.tv/clips/2WRj40tpY_EU9",
            200,
            "text/html",
            &clip_page(&withheld),
        ));
        fixture.exchanges.push(Exchange {
            request: RecordedRequest {
                method: "GET".into(),
                url: "https://medal.tv/api/content/2WRj40tpY_EU9/socialVideoUrl".into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status: 307,
                url: "https://medal.tv/api/content/2WRj40tpY_EU9/socialVideoUrl".into(),
                headers: vec![(
                    "location".into(),
                    "https://cdn.medal.tv/mediap/social/2WRj40tpY_EU9.mp4?auth=exp=1".into(),
                )],
                body: RecordedBody::Empty,
                truncated: false,
            },
        });
        let resolver = MedalResolver::new(Http::replay(fixture));
        let resolved = resolver
            .resolve(&Url::parse("https://medal.tv/games/valorant/clips/2WRj40tpY_EU9").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(resolved.variants.len(), 1);
        assert_eq!(
            resolved.variants[0].url.as_str(),
            "https://cdn.medal.tv/mediap/social/2WRj40tpY_EU9.mp4?auth=exp=1"
        );
        assert_eq!(resolved.variants[0].format_id.as_deref(), Some("social"));
    }

    #[tokio::test]
    async fn users_and_categories_list_clips_as_a_guest() {
        let items = |ids: &[&str]| {
            json!({"items": ids.iter().map(|id| clip(id, &format!("clip {id}"), false)).collect::<Vec<_>>(),
                "meta": {"next": "https://medal.tv/api/content?offset=2"}})
        };
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "POST",
            "https://medal.tv/api/users",
            200,
            "application/json",
            &json!({"user": {"userId": "849039250", "guest": true}, "auth": {"userId": "849039250", "key": "mdl_guestkey"}}).to_string(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://medal.tv/api/users?username=aciel",
            200,
            "application/json",
            &json!([{"userId": "19335460", "userName": "aciel", "displayName": "aciel", "submissions": 2328}]).to_string(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://medal.tv/api/content?newPagination=true&userId=19335460&limit=50&sortDirection=DESC&sortBy=publishedAt",
            200,
            "application/json",
            &items(&["nxYcrMeewonJLGFnf", "nxpu1etTezlGKytfs"]).to_string(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://medal.tv/games/valorant",
            200,
            "text/html",
            r#"<html><script>self.__next_f.push([1,"46:[\"$\",\"$L46\",null,{\"category\":{\"categoryId\":\"fW3AZxHf_c\",\"categoryName\":\"Valorant\",\"publishedClipCount\":44543743,\"slug\":\"valorant\"}}]\n"])</script></html>"#,
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://medal.tv/api/content?newPagination=true&categoryId=fW3AZxHf_c&limit=50&sortDirection=DESC&sortBy=views",
            200,
            "application/json",
            &items(&["30EZiwQoPtffH"]).to_string(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://medal.tv/api/users/1",
            400,
            "application/json",
            r#"{"httpStatusCode":400,"errorId":21,"errorMessage":"Some input data was in an unexpected format","additionalInfo":["Invalid Id given"]}"#,
        ));
        let resolver = MedalResolver::new(Http::replay(fixture));
        let Resolution::Playlist(user) = resolver
            .resolve(&Url::parse("https://medal.tv/u/aciel").unwrap())
            .await
            .unwrap()
        else {
            panic!("a user is a playlist");
        };
        assert_eq!(user.id.as_deref(), Some("19335460"));
        assert_eq!(user.title.as_deref(), Some("aciel"));
        assert_eq!(user.total, Some(2328));
        assert_eq!(user.entries.len(), 2);
        assert_eq!(
            user.entries[0].url.as_str(),
            "https://medal.tv/games/valorant/clips/nxYcrMeewonJLGFnf"
        );
        assert_eq!(user.entries[0].duration, Some(Duration::from_secs(13)));
        let Resolution::Playlist(category) = resolver
            .resolve(&Url::parse("https://medal.tv/games/valorant").unwrap())
            .await
            .unwrap()
        else {
            panic!("a category is a playlist");
        };
        assert_eq!(category.title.as_deref(), Some("Valorant"));
        assert_eq!(category.total, Some(44543743));
        assert_eq!(category.entries.len(), 1);
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://medal.tv/users/1").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
    }

    /// Every example link resolves live.
    #[tokio::test]
    #[ignore = "requires live Medal access"]
    async fn live_examples_resolve() {
        let resolver = MedalResolver::new(Http::new(crate::http::HttpConfig::default()));
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
            assert!(resolver.matches(&url), "{link}: not matched");
            let resolution = tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&url))
                .await
                .expect("resolution timed out")
                .unwrap_or_else(|e| panic!("{link}: {e}"));
            match resolution {
                Resolution::Media(resolved) => {
                    assert!(
                        resolved.variants.iter().any(|v| v.is_playable()),
                        "{link}: no variants"
                    );
                    println!(
                        "{link}: {:?} ({} variants)",
                        resolved.title,
                        resolved.variants.len()
                    );
                }
                Resolution::Playlist(playlist) => {
                    assert!(!playlist.entries.is_empty(), "{link}: no entries");
                    println!(
                        "{link}: {:?} ({} of {:?} entries)",
                        playlist.title,
                        playlist.entries.len(),
                        playlist.total
                    );
                }
            }
        }
    }
}
