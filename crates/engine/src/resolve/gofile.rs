//! Gofile folders and files, read the way the site's own file manager reads them: with
//! the guest account the API hands out to anyone (or the account whose token the
//! platform's jar holds), and with the website token the site's token script computes
//! from the account, the browser and the hour. The script is fetched and run in the
//! JavaScript interpreter, since its secret rotates. A folder is the files and folders
//! in it, a folder holding one file is that file, and a file is served by its storage
//! server to whoever sends the account's token as a cookie. A password-protected folder
//! is unlocked with the `password` the link carries.

use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use regex::Regex;
use serde_json::{Value, json};
use url::Url;

use super::{
    MAX_PAGE, Platform, Playlist, PlaylistEntry, Resolution, ResolveError, Resolved, Resolver,
    SessionCheck, SessionSupport, Tag, clean_title, dropbox, fetch_ok, pixeldrain, status_error,
    util,
};
use crate::http::{BROWSER_UA, Http};
use crate::js::Script;
use crate::media::MediaKind;

pub const PLATFORM: &str = "gofile";
const SITE: &str = "https://gofile.io";
const API: &str = "https://api.gofile.io";
/// The script that computes the website token every content request carries.
const TOKEN_SCRIPT: &str = "https://gofile.io/js/wt.obf.js";
/// The language the token is computed for, sent along with it.
const LANGUAGE: &str = "en-US";
/// The cookie a storage server wants with every download.
const TOKEN_COOKIE: &str = "accountToken";
/// How long the token script is kept before it is read again: its secret is rotated,
/// and a token from a retired secret is refused.
const SCRIPT_MEMORY: Duration = Duration::from_secs(30 * 60);
/// How long a guest account is used before a new one is asked for.
const GUEST_MEMORY: Duration = Duration::from_secs(6 * 60 * 60);
/// How many children a folder is read up to, a page at a time.
const PAGE_SIZE: usize = 100;
const LISTING_LIMIT: usize = 500;

/// A share code or a content uuid.
static RE_ID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[A-Za-z0-9]{6,12}|[0-9a-fA-F]{8}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{4}-[0-9a-fA-F]{12})$")
        .unwrap()
});
static RE_TOKEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[0-9a-f]{64}$").unwrap());

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// A folder's share code or a folder's or file's uuid.
    pub id: String,
    /// The password the link carries for a protected folder.
    pub password: Option<String>,
}

pub fn parse_link(url: &Url) -> Option<Link> {
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if host != "gofile.io" && host != "www.gofile.io" {
        return None;
    }
    let segments: Vec<&str> = url
        .path_segments()
        .into_iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .collect();
    let id = match segments.as_slice() {
        ["d", id] => id.to_string(),
        // The older share links: `/?c=code`.
        [] => util::query_param(url, "c")?,
        _ => return None,
    };
    if !RE_ID.is_match(&id) {
        return None;
    }
    Some(Link {
        id,
        password: util::query_param(url, "password").filter(|p| !p.is_empty()),
    })
}

/// The token script with the browser it runs in: the user agent the requests are sent
/// as and the language sent beside the token.
fn token_source(script: &str) -> String {
    format!(
        "var navigator = {{ userAgent: {}, language: {} }};\nvar window = globalThis;\n{script}",
        crate::js::literal(BROWSER_UA),
        crate::js::literal(LANGUAGE)
    )
}

struct Guest {
    token: String,
    made: Instant,
}

struct TokenScript {
    script: Script,
    fetched: Instant,
}

pub struct GofileResolver {
    http: Http,
    guest: Mutex<Option<Guest>>,
    script: Mutex<Option<TokenScript>>,
}

/// What the API answered: its own status word and its data.
struct Answer {
    status: String,
    data: Value,
    metadata: Value,
}

impl GofileResolver {
    pub fn new(http: Http) -> Self {
        Self {
            http,
            guest: Mutex::new(None),
            script: Mutex::new(None),
        }
    }

    /// The account token the jar holds, when a session was imported.
    fn jar_token(&self) -> Option<String> {
        self.http
            .jar(PLATFORM)
            .get(TOKEN_COOKIE)
            .map(|c| c.value.clone())
            .filter(|t| !t.is_empty())
    }

    /// The account the requests are made as: the jar's, or a guest account the API
    /// hands out, kept for a while.
    async fn account_token(&self, origin: &Url) -> Result<String, ResolveError> {
        if let Some(token) = self.jar_token() {
            return Ok(token);
        }
        if let Some(guest) = self
            .guest
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            && guest.made.elapsed() < GUEST_MEMORY
        {
            return Ok(guest.token.clone());
        }
        let api = Url::parse(&format!("{API}/accounts")).expect("valid");
        let response = self
            .http
            .post(api.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "application/json")
            .json(&json!({}))
            .send()
            .await?;
        if let Some(error) = status_error(response.status, origin) {
            return Err(error);
        }
        let answer: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, format!("guest account JSON: {e}")))?;
        if answer["status"].as_str() != Some("ok") {
            return Err(ResolveError::unavailable(
                origin,
                format!(
                    "Gofile hands out no guest account ({})",
                    answer["status"].as_str().unwrap_or("no status")
                ),
            ));
        }
        let token = util::text(&answer["data"]["token"])
            .ok_or_else(|| ResolveError::malformed(origin, "the guest account has no token"))?;
        *self.guest.lock().unwrap_or_else(|e| e.into_inner()) = Some(Guest {
            token: token.clone(),
            made: Instant::now(),
        });
        Ok(token)
    }

    /// Forgets the guest account, so the next request asks for a new one.
    fn drop_guest(&self) {
        *self.guest.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// The site's token script, read again once it has aged.
    async fn token_script(&self, origin: &Url) -> Result<Script, ResolveError> {
        if let Some(kept) = self
            .script
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            && kept.fetched.elapsed() < SCRIPT_MEMORY
        {
            return Ok(kept.script.clone());
        }
        let script_url = Url::parse(TOKEN_SCRIPT).expect("valid");
        let fetched = fetch_ok(&self.http, &script_url, PLATFORM, BROWSER_UA, &[], MAX_PAGE)
            .await
            .map_err(|e| e.at(origin))?;
        let text = fetched.text();
        if !text.contains("generateWT") {
            return Err(ResolveError::malformed(
                origin,
                "the token script does not define generateWT",
            ));
        }
        let script = Script::new(token_source(&text));
        *self.script.lock().unwrap_or_else(|e| e.into_inner()) = Some(TokenScript {
            script: script.clone(),
            fetched: Instant::now(),
        });
        Ok(script)
    }

    /// The website token for `token`, as the site's script computes it now.
    async fn website_token(&self, token: &str, origin: &Url) -> Result<String, ResolveError> {
        let script = self.token_script(origin).await?;
        let value = script
            .call("generateWT", &[token.to_string()])
            .await
            .map_err(|e| ResolveError::malformed(origin, format!("token script: {e}")))?;
        if !RE_TOKEN.is_match(&value) {
            return Err(ResolveError::malformed(
                origin,
                format!("the token script answered {value:?}"),
            ));
        }
        Ok(value)
    }

    /// One page of a content's children, read as `token`.
    async fn contents_page(
        &self,
        id: &str,
        password: Option<&str>,
        page: usize,
        token: &str,
        origin: &Url,
    ) -> Result<Answer, ResolveError> {
        let website_token = self.website_token(token, origin).await?;
        let page_text = page.to_string();
        let size_text = PAGE_SIZE.to_string();
        let mut query: Vec<(&str, &str)> = vec![
            ("page", page_text.as_str()),
            ("pageSize", size_text.as_str()),
            ("sortField", "name"),
            ("sortDirection", "1"),
        ];
        let hashed = password.map(|p| util::sha256_hex(p.as_bytes()));
        if let Some(hashed) = &hashed {
            query.push(("password", hashed.as_str()));
        }
        let api = util::with_query(
            &Url::parse(&format!("{API}/contents/{id}")).expect("valid"),
            &query,
        );
        let response = self
            .http
            .get(api)
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "application/json")
            .header("accept-language", LANGUAGE)
            .header("authorization", &format!("Bearer {token}"))
            .header("x-website-token", &website_token)
            .header("x-bl", LANGUAGE)
            .send()
            .await?;
        if let Some(error) = status_error(response.status, origin) {
            return Err(error);
        }
        let mut answer: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(origin, format!("contents JSON: {e}")))?;
        Ok(Answer {
            status: answer["status"].as_str().unwrap_or("").to_string(),
            data: answer["data"].take(),
            metadata: answer["metadata"].take(),
        })
    }

    /// A content with its children, every page up to the listing limit. A guest
    /// account the API no longer takes is replaced once.
    async fn contents(
        &self,
        id: &str,
        password: Option<&str>,
        origin: &Url,
    ) -> Result<(Value, usize), ResolveError> {
        let mut token = self.account_token(origin).await?;
        let mut first = self.contents_page(id, password, 1, &token, origin).await?;
        if first.status != "ok" && self.jar_token().is_none() && first.status != "error-notFound" {
            self.drop_guest();
            token = self.account_token(origin).await?;
            first = self.contents_page(id, password, 1, &token, origin).await?;
        }
        match first.status.as_str() {
            "ok" => {}
            "error-notFound" => return Err(ResolveError::NotFound(origin.clone())),
            "error-notPremium" => {
                return Err(ResolveError::unavailable(
                    origin,
                    "Gofile refused the website token",
                ));
            }
            "error-rateLimit" => return Err(ResolveError::RateLimited(origin.clone())),
            other => {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("Gofile answered {other}"),
                ));
            }
        }
        let mut data = first.data;
        let total = util::uint(&first.metadata["totalCount"])
            .map(|n| n as usize)
            .or_else(|| util::uint(&data["childrenCount"]).map(|n| n as usize))
            .unwrap_or_else(|| children_of(&data).len());
        let mut page = 1;
        while first.metadata["hasNextPage"].as_bool() == Some(true)
            && children_of(&data).len() < LISTING_LIMIT.min(total)
        {
            page += 1;
            let next = self
                .contents_page(id, password, page, &token, origin)
                .await?;
            if next.status != "ok" {
                return Err(ResolveError::unavailable(
                    origin,
                    format!("Gofile answered {} for page {page}", next.status),
                ));
            }
            let more = children_of(&next.data);
            if more.is_empty() {
                break;
            }
            if let Some(children) = data["children"].as_object_mut() {
                for child in more {
                    if let Some(child_id) = child["id"].as_str() {
                        children.insert(child_id.to_string(), child);
                    }
                }
            }
            if next.metadata["hasNextPage"].as_bool() != Some(true) {
                break;
            }
        }
        data["accountToken"] = Value::String(token);
        Ok((data, total))
    }

    /// A file as media of its kind, served with the account's cookie.
    fn media_of(&self, file: &Value, token: &str, origin: &Url) -> Result<Resolved, ResolveError> {
        let link = util::url_of(&file["link"], None)
            .ok_or_else(|| ResolveError::unavailable(origin, "the file has no download link"))?;
        let name = util::text(&file["name"]).unwrap_or_default();
        let mime = util::text(&file["mimetype"]).unwrap_or_default();
        let (kind, container) = dropbox::classify(&name, &mime);
        let mut variant = pixeldrain::file_variant(
            link,
            kind,
            container,
            util::uint(&file["size"]).filter(|s| *s > 0),
        );
        variant
            .headers
            .push(("cookie".to_string(), format!("{TOKEN_COOKIE}={token}")));
        let mut resolved = Resolved::of(PLATFORM, kind);
        resolved.id = util::text(&file["id"]);
        resolved.title = pixeldrain::title_of(&name);
        resolved.uploaded_at = util::epoch(&file["createTime"]);
        if matches!(kind, MediaKind::Video | MediaKind::Image) {
            resolved.thumbnail = util::url_of(&file["thumbnail"], None);
        }
        resolved.webpage_url = resolved
            .id
            .as_deref()
            .and_then(|id| Url::parse(&format!("{SITE}/d/{id}")).ok());
        resolved.variants = vec![variant];
        Ok(resolved)
    }
}

/// A content's children, in the order the API lists them.
fn children_of(data: &Value) -> Vec<Value> {
    match &data["children"] {
        Value::Object(map) => map.values().cloned().collect(),
        Value::Array(list) => list.clone(),
        _ => Vec::new(),
    }
}

/// Why a content cannot be read, when its gate says.
fn gate_error(data: &Value, password_given: bool, origin: &Url) -> ResolveError {
    if data["password"].as_bool() == Some(true) {
        return if password_given {
            ResolveError::unavailable(origin, "the folder refused the password")
        } else {
            ResolveError::unavailable(origin, "Password required. Add ?password= to the link.")
        };
    }
    if data["public"].as_bool() == Some(false) {
        return ResolveError::unavailable(origin, "the folder is not public");
    }
    if util::uint(&data["expire"]).is_some() {
        return ResolveError::unavailable(origin, "the folder has expired");
    }
    ResolveError::unavailable(origin, "the folder cannot be accessed")
}

#[async_trait]
impl Resolver for GofileResolver {
    fn id(&self) -> &'static str {
        PLATFORM
    }

    fn platform(&self) -> Platform {
        Platform {
            id: PLATFORM,
            name: "Gofile",
            hosts: &["gofile.io"],
            features: &[
                "folders",
                "files",
                "password-protected folders",
                "audio",
                "images",
                "any file",
            ],
            formats: &[
                "mp4", "mov", "webm", "mkv", "mp3", "m4a", "flac", "jpg", "png", "zip", "7z",
            ],
            media: &[
                MediaKind::Video,
                MediaKind::Audio,
                MediaKind::Image,
                MediaKind::File,
            ],
            tags: &[Tag::Files],
            session: SessionSupport::Optional,
            on_by_default: true,
            examples: &["https://gofile.io/d/b4Ds9u"],
        }
    }

    fn matches(&self, url: &Url) -> bool {
        parse_link(url).is_some()
    }

    async fn resolve(&self, url: &Url) -> Result<Resolution, ResolveError> {
        let link = parse_link(url).ok_or_else(|| ResolveError::NotFound(url.clone()))?;
        let (data, total) = self
            .contents(&link.id, link.password.as_deref(), url)
            .await?;
        let token = data["accountToken"].as_str().unwrap_or("").to_string();
        if data["canAccess"].as_bool() == Some(false) {
            return Err(gate_error(&data, link.password.is_some(), url));
        }
        match data["type"].as_str() {
            Some("file") => Ok(Resolution::from(self.media_of(&data, &token, url)?)),
            Some("folder") => {
                let children = children_of(&data);
                let files: Vec<&Value> = children
                    .iter()
                    .filter(|c| c["type"].as_str() == Some("file"))
                    .collect();
                let folders: Vec<&Value> = children
                    .iter()
                    .filter(|c| c["type"].as_str() == Some("folder"))
                    .collect();
                if files.len() == 1 && folders.is_empty() && total <= 1 {
                    return Ok(Resolution::from(self.media_of(files[0], &token, url)?));
                }
                let entries: Vec<PlaylistEntry> = children
                    .iter()
                    .filter_map(|child| {
                        let is_file = child["type"].as_str() == Some("file");
                        let id = if is_file {
                            util::text(&child["id"])?
                        } else {
                            util::text(&child["code"]).or_else(|| util::text(&child["id"]))?
                        };
                        Some(PlaylistEntry {
                            url: Url::parse(&format!("{SITE}/d/{id}")).ok()?,
                            title: util::text(&child["name"]).and_then(|n| clean_title(&n)),
                            duration: None,
                        })
                    })
                    .collect();
                if entries.is_empty() {
                    return Err(ResolveError::unavailable(url, "the folder is empty"));
                }
                Ok(Resolution::Playlist(Playlist {
                    resolver: PLATFORM.to_string(),
                    id: util::text(&data["code"]).or_else(|| util::text(&data["id"])),
                    title: util::text(&data["name"]).and_then(|n| clean_title(&n)),
                    total: Some(total.max(entries.len())),
                    entries,
                }))
            }
            other => Err(ResolveError::malformed(
                url,
                format!("the content is a {}", other.unwrap_or("typeless thing")),
            )),
        }
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        let Some(token) = self.jar_token() else {
            return Ok(SessionCheck::LoggedOut);
        };
        let api = Url::parse(&format!("{API}/accounts/website")).expect("valid");
        let response = self
            .http
            .get(api.clone())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "application/json")
            .header("authorization", &format!("Bearer {token}"))
            .send()
            .await?;
        if !response.status.is_success() {
            return Ok(SessionCheck::LoggedOut);
        }
        let answer: Value = response
            .json(MAX_PAGE)
            .await
            .map_err(|e| ResolveError::malformed(&api, format!("account JSON: {e}")))?;
        if answer["status"].as_str() != Some("ok") {
            return Ok(SessionCheck::LoggedOut);
        }
        let account = &answer["data"];
        if account["tier"].as_str() == Some("guest") {
            return Ok(SessionCheck::LoggedOut);
        }
        Ok(
            match util::text(&account["email"]).or_else(|| util::text(&account["id"])) {
                Some(name) => SessionCheck::LoggedIn { account: name },
                None => SessionCheck::LoggedOut,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::transport::Fixture;
    use crate::http::{Cookie, Exchange, RecordedBody, RecordedRequest, RecordedResponse};
    use crate::media::Container;

    fn exchange(method: &str, url: &str, content_type: &str, body: String) -> Exchange {
        Exchange {
            request: RecordedRequest {
                method: method.into(),
                url: url.into(),
                headers: Vec::new(),
                body: None,
            },
            response: RecordedResponse {
                status: 200,
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
        assert_eq!(
            link("https://gofile.io/d/b4Ds9u"),
            Some(Link {
                id: "b4Ds9u".into(),
                password: None
            })
        );
        assert_eq!(
            link("https://www.gofile.io/d/158d8c26-7935-4929-8b22-242a07154539?password=hunter2"),
            Some(Link {
                id: "158d8c26-7935-4929-8b22-242a07154539".into(),
                password: Some("hunter2".into())
            })
        );
        assert_eq!(
            link("https://gofile.io/?c=b4Ds9u"),
            Some(Link {
                id: "b4Ds9u".into(),
                password: None
            })
        );
        assert_eq!(link("https://gofile.io/"), None);
        assert_eq!(link("https://gofile.io/d/"), None);
        assert_eq!(
            link("https://gofile.io/d/65pGBWhc"),
            Some(Link {
                id: "65pGBWhc".into(),
                password: None
            })
        );
        assert_eq!(link("https://gofile.io/d/ab"), None);
        assert_eq!(link("https://gofile.io/d/not-a-code-or-uuid"), None);
        assert_eq!(link("https://gofile.io/api"), None);
        assert_eq!(link("https://gofile.io/myfiles"), None);
        assert_eq!(link("https://example.com/d/b4Ds9u"), None);
    }

    /// The token script as the site served it on 2026-09-24, with the guest account, the
    /// single-file folder `b4Ds9u` and the folder of videos `65pGBWhc` as they were that
    /// day.
    fn recorded() -> Fixture {
        Fixture::parse(include_str!("gofile_fixture.json")).unwrap()
    }

    #[tokio::test]
    async fn the_token_script_runs_in_the_interpreter() {
        let fixture = recorded();
        let script_text = fixture
            .exchanges
            .iter()
            .find(|e| e.request.url == TOKEN_SCRIPT)
            .map(|e| String::from_utf8_lossy(&e.response.body.to_bytes()).into_owned())
            .unwrap();
        let script = Script::new(token_source(&script_text));
        let token = script
            .call(
                "generateWT",
                &["4X8KJcTyx7mkesJM5TDjfc72ow7uefRJ".to_string()],
            )
            .await
            .unwrap();
        assert!(RE_TOKEN.is_match(&token), "{token}");
        let other = script
            .call(
                "generateWT",
                &["zJH6UEQBkhXFglvxGBRQMMNkKI5fPtXq".to_string()],
            )
            .await
            .unwrap();
        assert_ne!(token, other, "the token binds the account");
    }

    #[tokio::test]
    async fn folders_and_files_resolve_with_the_guest_account() {
        let resolver = GofileResolver::new(Http::replay(recorded()));
        let single = resolver
            .resolve(&Url::parse("https://gofile.io/d/b4Ds9u").unwrap())
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(single.media, MediaKind::File);
        assert_eq!(single.title.as_deref(), Some("AI-onnx"));
        assert_eq!(single.variants.len(), 1);
        assert_eq!(
            single.variants[0].container,
            Some(Container::Other("zip".into()))
        );
        assert!(single.variants[0].size.unwrap() > 1_000_000);
        assert!(
            single.variants[0]
                .url
                .as_str()
                .contains("gofile.io/download/web/")
        );
        assert_eq!(
            single.variants[0].headers[0].0, "cookie",
            "the storage server wants the account's cookie"
        );
        assert!(single.variants[0].headers[0].1.starts_with("accountToken="));
        assert!(single.uploaded_at.is_some());

        let Resolution::Playlist(folder) = resolver
            .resolve(&Url::parse("https://gofile.io/d/65pGBWhc").unwrap())
            .await
            .unwrap()
        else {
            panic!("a folder of many files is a playlist");
        };
        assert_eq!(folder.id.as_deref(), Some("65pGBWhc"));
        assert!(folder.entries.len() > 10);
        assert_eq!(folder.total, Some(folder.entries.len()));
        assert!(
            folder
                .entries
                .iter()
                .all(|e| e.url.as_str().starts_with("https://gofile.io/d/"))
        );
        let video = resolver
            .resolve(&folder.entries[0].url)
            .await
            .unwrap()
            .media()
            .unwrap();
        assert_eq!(video.media, MediaKind::Video);
        assert!(video.thumbnail.is_some());
        assert!(video.variants[0].size.is_some());
    }

    #[tokio::test]
    async fn gates_and_missing_folders_say_why() {
        let mut fixture = recorded();
        fixture
            .exchanges
            .retain(|e| !e.request.url.contains("/contents/"));
        fixture.exchanges.push(exchange(
            "GET",
            "https://api.gofile.io/contents/l0cked?page=1&pageSize=100&sortField=name&sortDirection=1",
            "application/json",
            json!({"status": "ok", "data": {"canAccess": false, "id": "aaaa", "type": "folder", "name": "l0cked", "password": true, "public": true}}).to_string(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://api.gofile.io/contents/wr0ng1?page=1&pageSize=100&sortField=name&sortDirection=1&password=2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
            "application/json",
            json!({"status": "ok", "data": {"canAccess": false, "id": "bbbb", "type": "folder", "name": "wr0ng1", "password": true, "passwordStatus": "passwordWrong", "public": true}}).to_string(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://api.gofile.io/contents/g0ne00?page=1&pageSize=100&sortField=name&sortDirection=1",
            "application/json",
            json!({"status": "error-notFound", "data": {}}).to_string(),
        ));
        fixture.exchanges.push(exchange(
            "GET",
            "https://api.gofile.io/contents/pr1vat?page=1&pageSize=100&sortField=name&sortDirection=1",
            "application/json",
            json!({"status": "ok", "data": {"canAccess": false, "id": "cccc", "type": "folder", "name": "pr1vat", "public": false}}).to_string(),
        ));
        let resolver = GofileResolver::new(Http::replay(fixture));
        let error = resolver
            .resolve(&Url::parse("https://gofile.io/d/l0cked").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("Password required")),
            "{error}"
        );
        let error = resolver
            .resolve(&Url::parse("https://gofile.io/d/wr0ng1?password=hello").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("refused the password")),
            "{error}"
        );
        assert!(matches!(
            resolver
                .resolve(&Url::parse("https://gofile.io/d/g0ne00").unwrap())
                .await
                .unwrap_err(),
            ResolveError::NotFound(_)
        ));
        let error = resolver
            .resolve(&Url::parse("https://gofile.io/d/pr1vat").unwrap())
            .await
            .unwrap_err();
        assert!(
            matches!(&error, ResolveError::Unavailable { reason, .. } if reason.contains("not public")),
            "{error}"
        );
    }

    #[tokio::test]
    async fn sessions_are_checked_through_the_account_endpoint() {
        let mut fixture = Fixture::new(PLATFORM, None);
        fixture.exchanges.push(exchange(
            "GET",
            "https://api.gofile.io/accounts/website",
            "application/json",
            json!({"status": "ok", "data": {"id": "x", "email": "nick@example.com", "tier": "premium"}}).to_string(),
        ));
        let http = Http::replay(fixture);
        let resolver = GofileResolver::new(http.clone());
        assert_eq!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedOut
        );
        http.with_jar(PLATFORM, |jar| {
            jar.insert(Cookie::new(TOKEN_COOKIE, "s3cret", "gofile.io"));
        });
        assert_eq!(
            resolver.check_session().await.unwrap(),
            SessionCheck::LoggedIn {
                account: "nick@example.com".into()
            }
        );
    }

    /// The clip the run uploads: a tenth of a second of black, 16 by 16, H.264 in MP4.
    const CLIP: &[u8] = include_bytes!("gofile_clip.mp4");
    const UPLOAD: &str = "https://upload.gofile.io/uploadfile";

    /// Uploads the clip as `name`: into `account`'s folder, or as a new guest into a new
    /// folder. What the upload API answers: the file's id and its folder's code and id,
    /// and the guest token when it made the guest.
    async fn upload(http: &Http, name: &str, account: Option<(&str, &str)>) -> Value {
        let boundary = "discoclip-gofile-live-test";
        let mut body = Vec::new();
        if let Some((token, folder)) = account {
            for (field, value) in [("token", token), ("folderId", folder)] {
                body.extend_from_slice(
                    format!(
                        "--{boundary}\r\nContent-Disposition: form-data; name=\"{field}\"\r\n\r\n{value}\r\n"
                    )
                    .as_bytes(),
                );
            }
        }
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{name}\"\r\nContent-Type: video/mp4\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(CLIP);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let response = http
            .post(Url::parse(UPLOAD).unwrap())
            .platform(PLATFORM)
            .user_agent(BROWSER_UA)
            .header("accept", "application/json")
            .header(
                "content-type",
                &format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body)
            .send()
            .await
            .unwrap();
        assert!(
            response.status.is_success(),
            "upload: HTTP {}",
            response.status
        );
        let answer: Value = response.json(MAX_PAGE).await.unwrap();
        assert_eq!(answer["status"].as_str(), Some("ok"), "upload: {answer}");
        answer["data"].clone()
    }

    /// The example link resolves live as a file. Gofile forgets a folder no one downloads
    /// from for ten days, so the run uploads two clips of its own as a guest and resolves
    /// their folder as a playlist of two entries, and the first entry as a video by its
    /// uuid, with another guest account.
    #[ignore = "reaches the live site: cargo test -- --ignored"]
    #[tokio::test]
    async fn live_examples_and_a_fresh_upload_resolve() {
        let http = Http::new(crate::http::HttpConfig::default());
        let resolver = GofileResolver::new(http.clone());
        for link in resolver.platform().examples {
            let url = Url::parse(link).unwrap();
            let resolved = tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&url))
                .await
                .expect("resolution timed out")
                .unwrap()
                .media()
                .unwrap();
            assert!(
                resolved.variants.iter().any(|v| v.is_playable()),
                "{link}: no variants"
            );
            println!("{link}: {:?} {:?}", resolved.media, resolved.title);
        }

        let first =
            tokio::time::timeout(Duration::from_secs(60), upload(&http, "clip-1.mp4", None))
                .await
                .expect("the first upload timed out");
        let token = first["guestToken"].as_str().expect("a guest token");
        let folder_id = first["parentFolder"].as_str().expect("a folder id");
        let code = first["parentFolderCode"].as_str().expect("a folder code");
        let second = tokio::time::timeout(
            Duration::from_secs(60),
            upload(&http, "clip-2.mp4", Some((token, folder_id))),
        )
        .await
        .expect("the second upload timed out");
        assert_eq!(second["parentFolderCode"].as_str(), Some(code));
        let ids = [
            first["id"].as_str().expect("a file id"),
            second["id"].as_str().expect("a file id"),
        ];

        let folder_url = Url::parse(&format!("{SITE}/d/{code}")).unwrap();
        let Resolution::Playlist(folder) =
            tokio::time::timeout(Duration::from_secs(60), resolver.resolve(&folder_url))
                .await
                .expect("resolution timed out")
                .unwrap()
        else {
            panic!("{folder_url}: a folder of two files is a playlist");
        };
        assert_eq!(folder.id.as_deref(), Some(code));
        assert_eq!(folder.entries.len(), 2, "{folder_url}");
        assert_eq!(folder.total, Some(2));
        for entry in &folder.entries {
            assert!(
                ids.iter()
                    .any(|id| entry.url.as_str() == format!("{SITE}/d/{id}")),
                "{}: not an uploaded file",
                entry.url
            );
        }
        let video = tokio::time::timeout(
            Duration::from_secs(60),
            resolver.resolve(&folder.entries[0].url),
        )
        .await
        .expect("resolution timed out")
        .unwrap()
        .media()
        .unwrap();
        assert_eq!(video.media, MediaKind::Video);
        assert_eq!(video.variants.len(), 1);
        assert!(video.variants[0].is_playable());
        assert_eq!(video.variants[0].size, Some(CLIP.len() as u64));
        assert_eq!(video.variants[0].container, Some(Container::Mp4));
        println!(
            "{folder_url}: {} entries, the first {:?} {:?}",
            folder.entries.len(),
            video.title,
            video.media
        );
    }
}
