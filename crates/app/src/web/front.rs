//! What a front end serves to the public: who it is and how to get in, the media it
//! shows, the files themselves, and the page for one piece of media with what link
//! unfurlers need to play it inline. Viewers hold a session cookie of the front end's
//! own. A signed token opens one file without a session, for the unfurlers.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum_extra::extract::CookieJar;
use axum_extra::extract::cookie::{Cookie, SameSite};
use std::path::PathBuf;

use discoclip_engine::job::{Job, JobId, JobStatus, Stage};
use discoclip_engine::media::{Container, LocalFile, MediaKind};
use discoclip_engine::store::{JobFilter, Order};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use url::Url;

use super::AppState;
use super::error::ApiError;
use super::jobs::{
    Artifact, DownloadQuery, has_thumbnail, locate, recording_grows, serve_file, serve_thumbnail,
};
use super::oauth::callback_url;
use super::proxy::{Client, ClientInfo, Scheme};
use super::{assets, front};
use crate::frontends::{Access, Frontend, SecretKind, Viewer};
use crate::oauth::Intent;

/// The session cookie of a front end, distinct per slug so one browser can hold several.
pub fn cookie_name(slug: &str) -> String {
    format!("dcf_{slug}")
}

const COOKIE_DAYS: i64 = 30;

/// The front end at `slug`, enabled, or 404.
fn frontend(state: &AppState, slug: &str) -> Result<Frontend, ApiError> {
    state.frontends.cache().get(slug).ok_or(ApiError::NotFound)
}

/// The viewer the cookie names, if their session is live.
async fn viewer(
    state: &AppState,
    frontend: &Frontend,
    jar: &CookieJar,
) -> Result<Option<Viewer>, ApiError> {
    let Some(cookie) = jar.get(&cookie_name(&frontend.input.slug)) else {
        return Ok(None);
    };
    Ok(state
        .frontends
        .authenticate(frontend.id, cookie.value())
        .await?)
}

/// The viewer, or 401 when the front end asks for a login and there is none.
async fn admitted(
    state: &AppState,
    frontend: &Frontend,
    jar: &CookieJar,
) -> Result<Option<Viewer>, ApiError> {
    let viewer = viewer(state, frontend, jar).await?;
    if frontend.input.access.open || viewer.is_some() {
        return Ok(viewer);
    }
    Err(ApiError::Unauthorized)
}

/// A login provider as the front end's login page offers it.
#[derive(Debug, Clone, Serialize)]
pub struct FrontProvider {
    pub id: String,
    pub name: String,
}

/// How a front end lets viewers in, as the login page needs to know it.
#[derive(Debug, Clone, Serialize)]
pub struct FrontAccess {
    pub open: bool,
    pub secret: Option<SecretKind>,
    pub accounts: bool,
    pub providers: Vec<FrontProvider>,
    pub discord_members: bool,
}

fn front_access(state: &AppState, frontend: &Frontend) -> FrontAccess {
    let access: &Access = &frontend.input.access;
    let offered = state.oauth.providers();
    FrontAccess {
        open: access.open,
        secret: access.secret_kind.filter(|_| frontend.has_secret),
        accounts: access.accounts,
        providers: access
            .providers
            .iter()
            .filter_map(|id| {
                offered.iter().find(|p| &p.id == id).map(|p| FrontProvider {
                    id: p.id.clone(),
                    name: p.name.clone(),
                })
            })
            .collect(),
        discord_members: access.discord_members,
    }
}

/// A front end as its visitors see it.
#[derive(Debug, Clone, Serialize)]
pub struct FrontInfo {
    pub slug: String,
    pub name: String,
    pub description: String,
    pub downloads: bool,
    pub access: FrontAccess,
    /// The platforms shown, by resolver id.
    pub platforms: Vec<String>,
    /// The visitor, when logged in.
    pub viewer: Option<Viewer>,
}

async fn info(
    state: &AppState,
    frontend: &Frontend,
    jar: &CookieJar,
) -> Result<FrontInfo, ApiError> {
    Ok(FrontInfo {
        slug: frontend.input.slug.clone(),
        name: frontend.input.name.clone(),
        description: frontend.input.description.clone(),
        downloads: frontend.input.downloads,
        access: front_access(state, frontend),
        platforms: state.frontends.cache().platforms_of(frontend),
        viewer: viewer(state, frontend, jar).await?,
    })
}

/// Who the front end is and how to get in. Readable before logging in.
pub async fn get(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    jar: CookieJar,
) -> Result<Json<FrontInfo>, ApiError> {
    let frontend = frontend(&state, &slug)?;
    Ok(Json(info(&state, &frontend, &jar).await?))
}

#[derive(Debug, Deserialize)]
pub struct LoginBody {
    #[serde(default)]
    pub secret: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
}

fn session_cookie(slug: &str, token: String, client: &ClientInfo) -> Cookie<'static> {
    Cookie::build((cookie_name(slug), token))
        .path("/")
        .http_only(true)
        .secure(client.scheme == Scheme::Https)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::days(COOKIE_DAYS))
        .build()
}

/// Opens a session for `viewer` and sets its cookie.
pub(super) async fn admit(
    state: &AppState,
    frontend: &Frontend,
    viewer: &Viewer,
    jar: CookieJar,
    headers: &HeaderMap,
    client: &ClientInfo,
) -> Result<CookieJar, ApiError> {
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .map(|agent| agent.chars().take(256).collect());
    let token = state
        .frontends
        .open_session(viewer, Some(client.ip), user_agent)
        .await?;
    Ok(jar.add(session_cookie(&frontend.input.slug, token, client)))
}

/// Logs in with the shared secret or one of the front end's accounts. Wrong secrets
/// count against the address like wrong passwords.
pub async fn login(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Client(client): Client,
    jar: CookieJar,
    headers: HeaderMap,
    Json(body): Json<LoginBody>,
) -> Result<(CookieJar, Json<FrontInfo>), ApiError> {
    let frontend = frontend(&state, &slug)?;
    let ip_key = client.ip.to_string();
    state
        .limits
        .login_ip
        .check(&ip_key)
        .map_err(ApiError::TooManyRequests)?;
    let access = &frontend.input.access;
    let viewer = match (body.secret, body.username, body.password) {
        (Some(secret), None, None) => {
            if !(frontend.has_secret && access.secret_kind.is_some()) {
                return Err(ApiError::BadRequest(
                    "this view does not take a shared secret".into(),
                ));
            }
            if !state.frontends.verify_secret(frontend.id, &secret).await? {
                state.limits.login_ip.strike(&ip_key);
                return Err(ApiError::Unauthorized);
            }
            Viewer {
                frontend_id: frontend.id,
                subject: "secret".into(),
                display: "Guest".into(),
            }
        }
        (None, Some(username), Some(password)) => {
            if !access.accounts {
                return Err(ApiError::BadRequest(
                    "this view has no accounts of its own".into(),
                ));
            }
            let Some(user) = state
                .frontends
                .verify_user(frontend.id, &username, &password)
                .await?
            else {
                state.limits.login_ip.strike(&ip_key);
                return Err(ApiError::Unauthorized);
            };
            Viewer {
                frontend_id: frontend.id,
                subject: format!("account:{}", user.username),
                display: user.username,
            }
        }
        _ => {
            return Err(ApiError::BadRequest(
                "send either a secret, or a username and a password".into(),
            ));
        }
    };
    state.limits.login_ip.clear(&ip_key);
    let jar = admit(&state, &frontend, &viewer, jar, &headers, &client).await?;
    tracing::info!(frontend = frontend.input.slug, subject = viewer.subject, ip = %client.ip, "front end login");
    let info = info(&state, &frontend, &jar).await?;
    Ok((jar, Json(info)))
}

pub async fn logout(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    jar: CookieJar,
) -> Result<(CookieJar, StatusCode), ApiError> {
    let frontend = frontend(&state, &slug)?;
    let name = cookie_name(&frontend.input.slug);
    if let Some(cookie) = jar.get(&name) {
        state
            .frontends
            .close_session(frontend.id, cookie.value())
            .await?;
    }
    let removal = Cookie::build((name, ""))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Lax)
        .max_age(time::Duration::ZERO)
        .build();
    Ok((jar.remove(removal), StatusCode::NO_CONTENT))
}

/// Sends the browser to a login provider on the front end's behalf.
pub async fn start(
    State(state): State<AppState>,
    Path((slug, provider)): Path<(String, String)>,
    Client(client): Client,
) -> Result<Redirect, ApiError> {
    let frontend = frontend(&state, &slug)?;
    if !frontend.input.access.providers.contains(&provider) {
        return Err(ApiError::BadRequest(format!(
            "{} does not log viewers in through {provider}",
            frontend.input.name
        )));
    }
    let redirect_uri = callback_url(&state, &client, &provider)?;
    let location = state
        .oauth
        .begin(
            &provider,
            Intent::Frontend(frontend.input.slug.clone()),
            None,
            redirect_uri,
        )
        .await?
        .ok_or_else(|| ApiError::TooManyRequests(std::time::Duration::from_secs(60)))?;
    Ok(Redirect::to(location.as_str()))
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub q: Option<String>,
    #[serde(default)]
    pub media: Option<MediaKind>,
    #[serde(default)]
    pub resolver: Option<String>,
    /// Jobs finished before this moment: the cursor for the next page.
    #[serde(default)]
    pub before: Option<Timestamp>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// A piece of media as a front end shows it.
#[derive(Debug, Clone, Serialize)]
pub struct FrontJob {
    pub id: JobId,
    pub title: Option<String>,
    pub media: MediaKind,
    pub resolver: String,
    /// The resolver's display name
    pub platform: String,
    pub uploader: Option<String>,
    pub uploader_url: Option<Url>,
    pub webpage_url: Option<Url>,
    /// Shows the still that stands for the media. Carries the token that opens it without
    /// a session, as the media link does.
    pub thumbnail: Option<String>,
    pub duration_secs: Option<f64>,
    /// A recorded stream.
    pub live: bool,
    /// The media is the recording of a capture under way, playing while it grows.
    pub recording: bool,
    pub size: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// The output's media type.
    pub content_type: String,
    pub published_at: Timestamp,
    /// Plays or shows the media. Carries the token that opens it without a session.
    pub media_url: String,
    /// Hands the file out, when the front end allows downloads.
    pub download_url: Option<String>,
}

/// The output from the moment it is made and being posted on, else the growing recording of a capture
fn playable(job: &Job) -> Option<(&LocalFile, bool)> {
    match &job.status {
        JobStatus::Done
        | JobStatus::Running {
            stage: Stage::Publish | Stage::Archive,
        } => job.artifacts.output.as_ref().map(|file| (file, false)),
        JobStatus::Running { .. } => job.artifacts.recording.as_ref().map(|file| (file, true)),
        _ => None,
    }
}

fn front_job(state: &AppState, frontend: &Frontend, job: &Job) -> Option<FrontJob> {
    let resolved = job.artifacts.resolved.as_ref()?;
    let (file, recording) = playable(job)?;
    let token = state.frontends.sign_media(frontend, job.id.0);
    let base = format!("/api/f/{}/jobs/{}", frontend.input.slug, job.id);
    let info = file.info.as_ref();
    let picture = info.and_then(|i| i.video.as_ref());
    let container = info.map(|i| i.container.clone()).unwrap_or_else(|| {
        let ext = file
            .path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("bin");
        Container::from_extension(ext).unwrap_or_else(|| Container::Other(ext.to_string()))
    });
    let size = if recording {
        std::fs::metadata(&file.path).map(|m| m.len()).unwrap_or(0)
    } else {
        file.size
    };
    Some(FrontJob {
        id: job.id,
        title: resolved.title.clone(),
        media: job.media(),
        resolver: resolved.resolver.clone(),
        platform: platform_name(state, &resolved.resolver),
        uploader: resolved.uploader.clone(),
        uploader_url: resolved.uploader_url.clone(),
        webpage_url: resolved.webpage_url.clone(),
        thumbnail: has_thumbnail(job).then(|| format!("{base}/thumbnail?t={token}")),
        duration_secs: if recording {
            None
        } else {
            info.and_then(|i| i.duration)
                .or(resolved.duration)
                .map(|d| d.as_secs_f64())
        },
        live: resolved.live || recording,
        recording,
        size,
        width: picture.map(|p| p.width),
        height: picture.map(|p| p.height),
        content_type: container.mime().to_string(),
        published_at: if recording {
            job.started_at.unwrap_or(job.created_at)
        } else {
            job.finished_at.unwrap_or(job.updated_at)
        },
        media_url: format!("{base}/media?t={token}"),
        download_url: frontend.input.downloads.then(|| format!("{base}/download")),
    })
}

/// The display name of the platform behind the resolver id, else the id itself
fn platform_name(state: &AppState, resolver: &str) -> String {
    state
        .engine
        .platforms()
        .into_iter()
        .find(|p| p.id == resolver)
        .map(|p| p.name.to_string())
        .unwrap_or_else(|| resolver.to_string())
}

/// Whether the job has a playable file, lies in the front end's scope and sits on a shown platform
fn shown(state: &AppState, frontend: &Frontend, job: &Job) -> bool {
    playable(job).is_some()
        && state.frontends.cache().shows(
            frontend,
            job.resolver(),
            job.request.origin.guild.as_deref(),
            job.request.origin.channel.as_deref(),
        )
}

/// Where the media a front end plays for `job` is: the output, or the recording of a
/// capture under way, and whether that recording is still growing.
async fn media_file(job: &Job) -> Result<(PathBuf, String, bool), ApiError> {
    let (_, recording) = playable(job).ok_or(ApiError::NotFound)?;
    if recording {
        let query = DownloadQuery {
            artifact: Artifact::Recording,
            index: 0,
            inline: true,
        };
        let (path, filename) = locate(job, &query).await?;
        return Ok((path, filename, recording_grows(job)));
    }
    let (path, filename) = locate(job, &DownloadQuery::output()).await?;
    Ok((path, filename, false))
}

#[derive(Debug, Serialize)]
pub struct FrontPage {
    pub jobs: Vec<FrontJob>,
    /// The `before` for the next page, when there may be more.
    pub next: Option<Timestamp>,
}

const PAGE_LIMIT: usize = 48;

/// The media the front end shows, newest first.
pub async fn list(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Query(query): Query<ListQuery>,
    jar: CookieJar,
) -> Result<Json<FrontPage>, ApiError> {
    let frontend = frontend(&state, &slug)?;
    admitted(&state, &frontend, &jar).await?;
    let platforms = state.frontends.cache().platforms_of(&frontend);
    if platforms.is_empty() {
        return Ok(Json(FrontPage {
            jobs: Vec::new(),
            next: None,
        }));
    }
    let resolvers = match &query.resolver {
        Some(resolver) if platforms.iter().any(|p| p == resolver) => vec![resolver.clone()],
        Some(_) => {
            return Ok(Json(FrontPage {
                jobs: Vec::new(),
                next: None,
            }));
        }
        None => platforms,
    };
    let limit = query.limit.unwrap_or(PAGE_LIMIT).clamp(1, PAGE_LIMIT);
    let filter = JobFilter {
        source: None,
        status: None,
        before: query.before,
        after: None,
        limit: Some(limit + 1),
        offset: None,
        q: query.q,
        resolver: None,
        resolvers,
        parent: None,
        top_level: false,
        guilds: frontend.input.scope.guilds.clone(),
        channels: frontend.input.scope.channels.clone(),
        media: query.media,
        with_output: true,
        order: Order::Newest,
    };
    let mut jobs = state.engine.list(&filter).await?;
    let next = if jobs.len() > limit {
        jobs.truncate(limit);
        jobs.last().map(|j| j.created_at)
    } else {
        None
    };
    let jobs = jobs
        .iter()
        .filter_map(|job| front_job(&state, &frontend, job))
        .collect();
    Ok(Json(FrontPage { jobs, next }))
}

/// One piece of media the front end shows.
pub async fn get_job(
    State(state): State<AppState>,
    Path((slug, id)): Path<(String, String)>,
    jar: CookieJar,
) -> Result<Json<FrontJob>, ApiError> {
    let frontend = frontend(&state, &slug)?;
    admitted(&state, &frontend, &jar).await?;
    let id: JobId = super::auth::parse_id(&id)?;
    let job = state.engine.get(id).await?.ok_or(ApiError::NotFound)?;
    if !shown(&state, &frontend, &job) {
        return Err(ApiError::NotFound);
    }
    front_job(&state, &frontend, &job)
        .map(Json)
        .ok_or(ApiError::NotFound)
}

#[derive(Debug, Deserialize)]
pub struct MediaQuery {
    /// A signed token that opens the media without a session.
    #[serde(default)]
    pub t: Option<String>,
}

/// The media itself, to play or show inline: for a viewer, or for anyone holding a
/// signed token, as link unfurlers do.
pub async fn media(
    State(state): State<AppState>,
    Path((slug, id)): Path<(String, String)>,
    Query(query): Query<MediaQuery>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let frontend = frontend(&state, &slug)?;
    let id: JobId = super::auth::parse_id(&id)?;
    let ticketed = query
        .t
        .as_deref()
        .is_some_and(|t| state.frontends.verify_media(frontend.id, id.0, t));
    if !ticketed {
        admitted(&state, &frontend, &jar).await?;
    }
    let job = state.engine.get(id).await?.ok_or(ApiError::NotFound)?;
    if !shown(&state, &frontend, &job) {
        return Err(ApiError::NotFound);
    }
    let (path, filename, growing) = media_file(&job).await?;
    serve_file(&path, &filename, true, &headers, growing).await
}

/// The still that stands for the media: for a viewer, or for anyone holding a signed
/// token, as link unfurlers do.
pub async fn thumbnail(
    State(state): State<AppState>,
    Path((slug, id)): Path<(String, String)>,
    Query(query): Query<MediaQuery>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let frontend = frontend(&state, &slug)?;
    let id: JobId = super::auth::parse_id(&id)?;
    let ticketed = query
        .t
        .as_deref()
        .is_some_and(|t| state.frontends.verify_media(frontend.id, id.0, t));
    if !ticketed {
        admitted(&state, &frontend, &jar).await?;
    }
    let job = state.engine.get(id).await?.ok_or(ApiError::NotFound)?;
    if !shown(&state, &frontend, &job) {
        return Err(ApiError::NotFound);
    }
    let file = state
        .engine
        .thumbnail(id)
        .await?
        .ok_or(ApiError::NotFound)?;
    serve_thumbnail(&file.path, &headers).await
}

/// The file to keep, when the front end allows downloads.
pub async fn download(
    State(state): State<AppState>,
    Path((slug, id)): Path<(String, String)>,
    jar: CookieJar,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let frontend = frontend(&state, &slug)?;
    admitted(&state, &frontend, &jar).await?;
    if !frontend.input.downloads {
        return Err(ApiError::Forbidden(
            "this view does not hand its media out".into(),
        ));
    }
    let id: JobId = super::auth::parse_id(&id)?;
    let job = state.engine.get(id).await?.ok_or(ApiError::NotFound)?;
    if !shown(&state, &frontend, &job) {
        return Err(ApiError::NotFound);
    }
    let (path, filename, growing) = media_file(&job).await?;
    serve_file(&path, &filename, false, &headers, growing).await
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

fn meta(property: &str, content: &str) -> String {
    format!(
        "<meta property=\"{}\" content=\"{}\">\n",
        escape(property),
        escape(content)
    )
}

fn meta_name(name: &str, content: &str) -> String {
    format!(
        "<meta name=\"{}\" content=\"{}\">\n",
        escape(name),
        escape(content)
    )
}

/// Discord plays an external video inline up to about this size and drops the whole embed above it
pub const INLINE_VIDEO_LIMIT: u64 = 80 * 1024 * 1024;

/// The coral of the brand mark, drawn by Discord as the bar beside a preview
const BRAND_COLOR: &str = "#f2542d";

/// The title the page and its oEmbed document carry, the media kind on the view when the source gave none
fn page_title(frontend: &Frontend, job: &FrontJob) -> String {
    job.title
        .clone()
        .unwrap_or_else(|| format!("{} on {}", job.media, frontend.input.name))
}

/// What link unfurlers show as the provider and author lines of a media page's preview
#[derive(Debug, Serialize)]
pub struct OEmbed {
    pub version: &'static str,
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub title: String,
    pub author_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_url: Option<Url>,
    pub provider_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_url: Option<Url>,
}

/// The view as the provider and the uploader as the author, linked to their page or else the source
pub fn oembed_document(base: Option<&Url>, frontend: &Frontend, job: &FrontJob) -> OEmbed {
    let (author_name, author_url) = match &job.uploader {
        Some(uploader) => (
            uploader.clone(),
            job.uploader_url.clone().or_else(|| job.webpage_url.clone()),
        ),
        None => (job.platform.clone(), job.webpage_url.clone()),
    };
    OEmbed {
        version: "1.0",
        kind: "link",
        title: page_title(frontend, job),
        author_name,
        author_url,
        provider_name: frontend.input.name.clone(),
        provider_url: base.map(|base| {
            base.join(&format!("f/{}", frontend.input.slug))
                .expect("a view path joins")
        }),
    }
}

/// The head a front end's media page carries: what Discord and other unfurlers read to
/// show the title and thumbnail and to play a video or audio inline from a plain link.
/// The media and thumbnail links are signed so the unfurler needs no session, and every
/// link is absolute under the app's public address.
pub fn media_head(base: &Url, frontend: &Frontend, job: &FrontJob) -> String {
    let page = base
        .join(&format!("f/{}/j/{}", frontend.input.slug, job.id))
        .expect("a page path joins");
    let media = base
        .join(job.media_url.trim_start_matches('/'))
        .expect("a media path joins");
    let image = job
        .thumbnail
        .as_deref()
        .map(|path| {
            base.join(path.trim_start_matches('/'))
                .expect("a thumbnail path joins")
        })
        .unwrap_or_else(|| base.join("icons/icon-512.png").expect("an icon path joins"));
    let oembed = base
        .join(&format!(
            "api/f/{}/jobs/{}/oembed",
            frontend.input.slug, job.id
        ))
        .expect("an oembed path joins");
    let title = page_title(frontend, job);
    let description = match job.duration_secs {
        Some(secs) => format!("{} · {}", job.platform, clock(secs)),
        None if job.live => format!("{} · live", job.platform),
        None => job.platform.clone(),
    };
    let mut head = String::new();
    head.push_str(&format!("<title>{}</title>\n", escape(&title)));
    head.push_str(&meta("og:site_name", &frontend.input.name));
    head.push_str(&meta("og:title", &title));
    head.push_str(&meta("og:url", page.as_str()));
    head.push_str(&meta("og:description", &description));
    head.push_str(&meta_name("description", &description));
    head.push_str(&meta("theme-color", BRAND_COLOR));
    head.push_str(&format!(
        "<link rel=\"alternate\" type=\"application/json+oembed\" href=\"{}\" title=\"{}\">\n",
        escape(oembed.as_str()),
        escape(&title)
    ));
    match job.media {
        MediaKind::Video if job.size <= INLINE_VIDEO_LIMIT => {
            head.push_str(&meta("og:type", "video.other"));
            head.push_str(&meta("og:video", media.as_str()));
            head.push_str(&meta("og:video:url", media.as_str()));
            head.push_str(&meta("og:video:secure_url", media.as_str()));
            head.push_str(&meta("og:video:type", &job.content_type));
            if let (Some(w), Some(h)) = (job.width, job.height) {
                head.push_str(&meta("og:video:width", &w.to_string()));
                head.push_str(&meta("og:video:height", &h.to_string()));
            }
            head.push_str(&meta("og:image", image.as_str()));
            head.push_str(&meta_name("twitter:image", image.as_str()));
            head.push_str(&meta_name("twitter:card", "player"));
            head.push_str(&meta_name("twitter:title", &title));
            head.push_str(&meta_name("twitter:player", media.as_str()));
            head.push_str(&meta_name("twitter:player:stream", media.as_str()));
            head.push_str(&meta_name(
                "twitter:player:stream:content_type",
                &job.content_type,
            ));
            if let (Some(w), Some(h)) = (job.width, job.height) {
                head.push_str(&meta_name("twitter:player:width", &w.to_string()));
                head.push_str(&meta_name("twitter:player:height", &h.to_string()));
            }
        }
        MediaKind::Video => {
            head.push_str(&meta("og:type", "website"));
            head.push_str(&meta("og:image", image.as_str()));
            head.push_str(&meta_name("twitter:image", image.as_str()));
            head.push_str(&meta_name("twitter:card", "summary_large_image"));
            head.push_str(&meta_name("twitter:title", &title));
        }
        MediaKind::Audio => {
            head.push_str(&meta("og:type", "music.song"));
            head.push_str(&meta("og:audio", media.as_str()));
            head.push_str(&meta("og:audio:url", media.as_str()));
            head.push_str(&meta("og:audio:secure_url", media.as_str()));
            head.push_str(&meta("og:audio:type", &job.content_type));
            head.push_str(&meta("og:image", image.as_str()));
            head.push_str(&meta_name("twitter:image", image.as_str()));
            head.push_str(&meta_name("twitter:card", "summary_large_image"));
        }
        MediaKind::Image => {
            head.push_str(&meta("og:type", "website"));
            head.push_str(&meta("og:image", media.as_str()));
            head.push_str(&meta("og:image:url", media.as_str()));
            head.push_str(&meta("og:image:secure_url", media.as_str()));
            head.push_str(&meta("og:image:type", &job.content_type));
            if let (Some(w), Some(h)) = (job.width, job.height) {
                head.push_str(&meta("og:image:width", &w.to_string()));
                head.push_str(&meta("og:image:height", &h.to_string()));
            }
            head.push_str(&meta_name("twitter:card", "summary_large_image"));
            head.push_str(&meta_name("twitter:image", media.as_str()));
        }
        MediaKind::File => {
            head.push_str(&meta("og:type", "website"));
            head.push_str(&meta("og:image", image.as_str()));
            head.push_str(&meta_name("twitter:card", "summary"));
        }
    }
    head
}

fn clock(secs: f64) -> String {
    let total = secs.round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// The page for one piece of media: the app's shell with the unfurl metadata in its
/// head. Anyone with the link gets the metadata, so the link can be unfurled wherever it
/// is posted. The page itself asks for a login as the front end does.
pub async fn page(
    State(state): State<AppState>,
    Path((slug, id)): Path<(String, String)>,
    Client(client): Client,
) -> Response {
    let Some(frontend) = state.frontends.cache().get(&slug) else {
        return assets::page_with_head("");
    };
    let Ok(id) = id.parse::<JobId>() else {
        return assets::page_with_head("");
    };
    let job = match state.engine.get(id).await {
        Ok(Some(job)) if shown(&state, &frontend, &job) => job,
        _ => return assets::page_with_head(""),
    };
    let Some(front) = front_job(&state, &frontend, &job) else {
        return assets::page_with_head("");
    };
    let base = state
        .public_url
        .get()
        .or_else(|| client.origin().and_then(|o| Url::parse(&o).ok()));

    let Some(base) = base else {
        return assets::page_with_head("");
    };
    assets::page_with_head(&front::media_head(&base, &frontend, &front))
}

/// The oEmbed document of a media page, open to anyone as the page's head is
pub async fn oembed(
    State(state): State<AppState>,
    Path((slug, id)): Path<(String, String)>,
    Client(client): Client,
) -> Result<Json<OEmbed>, ApiError> {
    let frontend = frontend(&state, &slug)?;
    let id: JobId = super::auth::parse_id(&id)?;
    let job = state.engine.get(id).await?.ok_or(ApiError::NotFound)?;
    if !shown(&state, &frontend, &job) {
        return Err(ApiError::NotFound);
    }
    let front = front_job(&state, &frontend, &job).ok_or(ApiError::NotFound)?;
    let base = state
        .public_url
        .get()
        .or_else(|| client.origin().and_then(|o| Url::parse(&o).ok()));
    Ok(Json(oembed_document(base.as_ref(), &frontend, &front)))
}

impl IntoResponse for FrontInfo {
    fn into_response(self) -> Response {
        Json(self).into_response()
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use discoclip_engine::job::{JobStatus, Origin, Request, SourceId, Stage};
    use discoclip_engine::media::{LocalFile, MediaKind};
    use discoclip_engine::store::sqlite::SqliteStore;
    use discoclip_engine::{Job, JobStore};
    use serde_json::json;
    use url::Url;

    use crate::users::Role;
    use crate::web::WebApp;
    use crate::web::testing::{Client, SUPPORTED_HOST, app_with_admin_db};

    /// A finished job with an MP4 output, seen in `guild`/`channel` on Discord or from
    /// the web app when both are `None`.
    async fn seed(
        db: &SqliteStore,
        dir: &std::path::Path,
        name: &str,
        guild: Option<&str>,
        channel: Option<&str>,
        resolver: &str,
    ) -> Job {
        let output = dir.join(format!("{name}.mp4"));
        std::fs::write(&output, b"0123456789").unwrap();
        let origin = match guild {
            Some(guild) => Origin {
                source: SourceId::new("discord"),
                reference: format!("app:{guild}:{}:1:9", channel.unwrap_or("0")),
                url: None,
                guild: Some(guild.to_string()),
                channel: channel.map(String::from),
            },
            None => Origin {
                source: SourceId::new("local"),
                reference: "nick".into(),
                url: None,
                guild: None,
                channel: None,
            },
        };
        let mut job = Job::new(Request::new(
            origin,
            Url::parse(&format!("https://{SUPPORTED_HOST}/{name}")).unwrap(),
        ));
        let mut resolved = discoclip_engine::Resolved::new(resolver);
        resolved.title = Some(format!("Clip {name}"));
        resolved.uploader = Some("someone".into());
        resolved.uploader_url =
            Some(Url::parse(&format!("https://{SUPPORTED_HOST}/someone")).unwrap());
        resolved.webpage_url =
            Some(Url::parse(&format!("https://{SUPPORTED_HOST}/watch/{name}")).unwrap());
        resolved.thumbnail = Some(Url::parse("https://thumbs.test/t.jpg").unwrap());
        job.artifacts.resolved = Some(resolved);
        job.artifacts.output = Some(LocalFile {
            path: output,
            size: 10,
            info: None,
        });
        job.status = JobStatus::Done;
        job.finished_at = Some(jiff::Timestamp::now());
        db.insert(&job).await.unwrap();
        job
    }

    /// A visitor of the front end at `slug`: sends the front end's cookie, never the
    /// admin one.
    fn visitor(app: &WebApp) -> Client {
        let mut client = Client::new(app);
        client.origin = Some("http://localhost:8080".into());
        client
    }

    /// Keeps the front end cookie the last response set, as a browser would.
    fn keep_cookie(client: &mut Client, slug: &str) {
        let set = client.set_cookie.clone().expect("a cookie was set");
        let pair = set.split(';').next().unwrap().to_string();
        assert!(pair.starts_with(&format!("dcf_{slug}=")), "{set}");
        client.cookie = None;
        client.headers.retain(|(name, _)| name != "cookie");
        client.headers.push(("cookie".into(), pair));
    }

    #[tokio::test]
    async fn front_ends_show_their_scope_to_the_viewers_they_let_in() {
        let (app, db) = app_with_admin_db().await;
        let dir = std::env::temp_dir().join(format!("discoclip-front-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let in_guild = seed(&db, &dir, "a", Some("5"), Some("1"), "fixtured").await;
        let other_guild = seed(&db, &dir, "b", Some("6"), Some("2"), "fixtured").await;
        let hidden_platform = seed(&db, &dir, "c", Some("5"), Some("1"), "nothing").await;
        let local = seed(&db, &dir, "d", None, None, "fixtured").await;
        let mut admin = Client::new(&app);
        admin.login("nick", "correct horse").await;

        // A profile without the `nothing` platform, and an open front end over guild 5.
        let (status, profile) = admin
            .post(
                "/api/profiles",
                json!({"name": "Fixtured only", "platforms": {"overrides": {"nothing": false}}}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{profile}");
        let (status, body) = admin
            .post(
                "/api/frontends",
                json!({
                    "name": "Guild Five", "slug": "five", "profile_id": profile["id"],
                    "scope": {"guilds": ["5"]}, "access": {"open": true}, "downloads": false
                }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let id = body["id"].as_str().unwrap().to_string();
        assert_eq!(body["has_secret"], false);
        assert_eq!(body["links"]["enabled"], false);

        let mut guest = visitor(&app);
        let (status, info) = guest.get("/api/f/five").await;
        assert_eq!(status, StatusCode::OK, "{info}");
        assert_eq!(info["name"], "Guild Five");
        assert_eq!(info["access"]["open"], true);
        assert_eq!(info["platforms"], json!(["fixtured"]));
        assert!(info["viewer"].is_null());
        assert_eq!(guest.get("/api/f/nope").await.0, StatusCode::NOT_FOUND);

        let (status, page) = guest.get("/api/f/five/jobs").await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let jobs = page["jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), 1, "{page}");
        assert_eq!(jobs[0]["id"], in_guild.id.to_string());
        assert_eq!(jobs[0]["title"], "Clip a");
        assert_eq!(jobs[0]["media"], "video");
        assert_eq!(jobs[0]["content_type"], "video/mp4");
        assert!(jobs[0]["download_url"].is_null());
        let media_url = jobs[0]["media_url"].as_str().unwrap().to_string();
        assert!(media_url.starts_with(&format!("/api/f/five/jobs/{}/media?t=", in_guild.id)));
        for gone in [&other_guild, &hidden_platform, &local] {
            assert_eq!(
                guest.get(&format!("/api/f/five/jobs/{}", gone.id)).await.0,
                StatusCode::NOT_FOUND
            );
        }
        let (status, body) = guest.get(&media_url).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body, json!("0123456789"));
        // A token opens only its own job on its own front end.
        let token = media_url.split_once("?t=").unwrap().1.to_string();
        let mut stranger = Client::new(&app);
        assert_eq!(
            stranger
                .get(&format!(
                    "/api/f/five/jobs/{}/media?t={token}",
                    other_guild.id
                ))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            guest
                .get(&format!("/api/f/five/jobs/{}/download", in_guild.id))
                .await
                .0,
            StatusCode::FORBIDDEN
        );

        // The page carries what unfurlers read.
        let (status, html) = stranger.get(&format!("/f/five/j/{}", in_guild.id)).await;
        assert_eq!(status, StatusCode::OK);
        let html = html.as_str().unwrap();
        assert!(
            html.contains(r#"<meta property="og:type" content="video.other">"#),
            "{html}"
        );
        assert!(html.contains(r#"<meta property="og:video:type" content="video/mp4">"#));
        assert!(html.contains(&format!(
            r#"<meta property="og:video" content="http://localhost:8080/api/f/five/jobs/{}/media?t="#,
            in_guild.id
        )), "{html}");
        // The poster is the app's own still of the output, signed like the media, so
        // Discord shows it and nothing depends on the platform's picture staying up.
        let thumbnail_url = jobs[0]["thumbnail"].as_str().unwrap().to_string();
        assert!(
            thumbnail_url.starts_with(&format!("/api/f/five/jobs/{}/thumbnail?t=", in_guild.id)),
            "{thumbnail_url}"
        );
        assert!(
            html.contains(&format!(
                r#"<meta property="og:image" content="http://localhost:8080/api/f/five/jobs/{}/thumbnail?t="#,
                in_guild.id
            )),
            "{html}"
        );
        assert!(html.contains(r#"<meta name="twitter:card" content="player">"#));
        assert!(
            html.contains(&format!(
                r#"<meta name="twitter:player" content="http://localhost:8080/api/f/five/jobs/{}/media?t="#,
                in_guild.id
            )),
            "{html}"
        );

        assert!(html.contains("<title>Clip a</title>"));
        let (status, _, bytes) = stranger.raw(&thumbnail_url).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(bytes, b"0123456789".to_vec());

        let (status, html) = stranger.get(&format!("/f/five/j/{}", other_guild.id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!html.as_str().unwrap().contains("og:video"));

        // Closed with a PIN: nothing until the PIN is given, then everything.
        let (status, body) = admin
            .send(
                Method::PUT,
                &format!("/api/frontends/{id}"),
                Some(json!({
                    "name": "Guild Five", "slug": "five", "profile_id": profile["id"],
                    "scope": {"guilds": ["5"]},
                    "access": {"open": false, "secret_kind": "pin", "accounts": true},
                    "downloads": true
                })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = admin
            .send(
                Method::PUT,
                &format!("/api/frontends/{id}/secret"),
                Some(json!({"secret": "2468"})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["has_secret"], true);
        let mut viewer = visitor(&app);
        assert_eq!(
            viewer.get("/api/f/five/jobs").await.0,
            StatusCode::UNAUTHORIZED
        );
        let (status, info) = viewer.get("/api/f/five").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(info["access"]["secret"], "pin");
        assert_eq!(info["access"]["accounts"], true);
        assert_eq!(
            viewer
                .post("/api/f/five/login", json!({"secret": "0000"}))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        let (status, info) = viewer
            .post("/api/f/five/login", json!({"secret": "2468"}))
            .await;
        assert_eq!(status, StatusCode::OK, "{info}");
        assert_eq!(info["viewer"]["subject"], "secret");
        keep_cookie(&mut viewer, "five");
        let (status, page) = viewer.get("/api/f/five/jobs").await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["jobs"].as_array().unwrap().len(), 1);
        assert!(page["jobs"][0]["download_url"].is_string());
        let (status, body) = viewer
            .get(&format!("/api/f/five/jobs/{}/download", in_guild.id))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // The signed link still works without a session, for unfurlers.
        assert_eq!(stranger.get(&media_url).await.0, StatusCode::OK);
        assert_eq!(
            stranger
                .get(&format!("/api/f/five/jobs/{}/media", in_guild.id))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            stranger
                .get(&format!("/api/f/five/jobs/{}/thumbnail", in_guild.id))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(stranger.raw(&thumbnail_url).await.0, StatusCode::OK);

        let (status, sessions) = admin.get(&format!("/api/frontends/{id}/sessions")).await;
        assert_eq!(status, StatusCode::OK, "{sessions}");
        assert_eq!(sessions.as_array().unwrap().len(), 1);
        assert_eq!(
            viewer.post("/api/f/five/logout", json!({})).await.0,
            StatusCode::NO_CONTENT
        );
        viewer.headers.clear();
        assert_eq!(
            viewer.get("/api/f/five/jobs").await.0,
            StatusCode::UNAUTHORIZED
        );

        // An account of the front end's own.
        let (status, user) = admin
            .post(
                &format!("/api/frontends/{id}/users"),
                json!({"username": "alice", "password": "correct horse"}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{user}");
        let mut alice = visitor(&app);
        let (status, info) = alice
            .post(
                "/api/f/five/login",
                json!({"username": "alice", "password": "correct horse"}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{info}");
        assert_eq!(info["viewer"]["display"], "alice");
        keep_cookie(&mut alice, "five");
        assert_eq!(alice.get("/api/f/five/jobs").await.0, StatusCode::OK);
        assert_eq!(
            admin
                .delete(&format!(
                    "/api/frontends/{id}/users/{}",
                    user["id"].as_str().unwrap()
                ))
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            alice.get("/api/f/five/jobs").await.0,
            StatusCode::UNAUTHORIZED
        );

        // Links are built on the app's public address. None is set, but the admin's own
        // requests taught the server where it is reached, so links can be posted. A set
        // web.public_url wins over what was learned, and clearing it falls back to it.
        assert_eq!(
            app.state.public_url.get().unwrap().as_str(),
            "http://localhost:8080/"
        );
        let (status, body) = admin
            .send(
                Method::PUT,
                &format!("/api/frontends/{id}"),
                Some(json!({
                    "name": "Guild Five", "slug": "five", "profile_id": profile["id"],
                    "scope": {"guilds": ["5"]}, "access": {"open": true},
                    "links": {"enabled": true}
                })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, html) = stranger.get(&format!("/f/five/j/{}", in_guild.id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.as_str()
                .unwrap()
                .contains(r#"<meta property="og:url" content="http://localhost:8080/f/five/j/"#),
            "{html}"
        );
        let (status, body) = admin
            .send(
                Method::PATCH,
                "/api/settings",
                Some(json!({"set": {"web.public_url": "https://clips.example"}})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["public_url"], "https://clips.example/");
        assert_eq!(body["public_url_source"], "configured");
        let (status, html) = stranger.get(&format!("/f/five/j/{}", in_guild.id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.as_str()
                .unwrap()
                .contains(r#"<meta property="og:url" content="https://clips.example/f/five/j/"#)
        );
        let (status, body) = admin
            .send(
                Method::PATCH,
                "/api/settings",
                Some(json!({"reset": ["web.public_url"]})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["public_url"], "http://localhost:8080/");
        assert_eq!(body["public_url_source"], "learned");

        // Deleting the front end closes its pages.
        assert_eq!(
            admin.delete(&format!("/api/frontends/{id}")).await.0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(guest.get("/api/f/five").await.0, StatusCode::NOT_FOUND);

        // Viewers of the admin app may not manage front ends.
        app.state
            .users
            .create("viewer", Some("battery staple"), Role::Viewer)
            .await
            .unwrap();
        let mut plain = Client::new(&app);
        plain.login("viewer", "battery staple").await;
        assert_eq!(plain.get("/api/frontends").await.0, StatusCode::FORBIDDEN);
        let (status, body) = admin.get("/api/audit?target_kind=frontend").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let actions: Vec<&str> = body["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["action"].as_str().unwrap())
            .collect();
        assert_eq!(actions[0], "frontend.delete");
        assert!(actions.contains(&"frontend.user.create"));
        assert!(actions.contains(&"frontend.secret.set"));
        assert!(actions.contains(&"frontend.create"));
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// A seeded guild 5 job with the still the transcode made, left at the given status
    async fn seed_at(
        db: &SqliteStore,
        dir: &std::path::Path,
        name: &str,
        status: JobStatus,
    ) -> Job {
        let mut job = seed(db, dir, name, Some("5"), Some("1"), "fixtured").await;
        let poster = dir.join(format!("{name}-poster.jpg"));
        std::fs::write(&poster, b"poster").unwrap();
        job.artifacts.thumbnail = Some(LocalFile {
            path: poster,
            size: 6,
            info: None,
        });
        job.status = status;
        job.finished_at = None;
        db.update(&job).await.unwrap();
        job
    }

    /// Opens an open front end over guild 5 as the admin
    async fn open_guild_five(app: &WebApp) {
        let mut admin = Client::new(app);
        admin.login("nick", "correct horse").await;
        let (status, profile) = admin.post("/api/profiles", json!({"name": "Any"})).await;
        assert_eq!(status, StatusCode::CREATED, "{profile}");
        let (status, body) = admin
            .post(
                "/api/frontends",
                json!({
                    "name": "Guild Five", "slug": "five", "profile_id": profile["id"],
                    "scope": {"guilds": ["5"]}, "access": {"open": true}, "downloads": false
                }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }

    /// The bot posts the page mid publish and Discord reads it at once, before the job is done
    #[tokio::test]
    async fn the_page_unfurls_while_the_job_is_still_being_published() {
        let (app, db) = app_with_admin_db().await;
        let dir = std::env::temp_dir().join(format!("discoclip-front-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let publishing = seed_at(
            &db,
            &dir,
            "p",
            JobStatus::Running {
                stage: Stage::Publish,
            },
        )
        .await;
        let archiving = seed_at(
            &db,
            &dir,
            "r",
            JobStatus::Running {
                stage: Stage::Archive,
            },
        )
        .await;
        let transcoding = seed_at(
            &db,
            &dir,
            "t",
            JobStatus::Running {
                stage: Stage::Transcode,
            },
        )
        .await;
        open_guild_five(&app).await;

        let mut guest = visitor(&app);
        for job in [&publishing, &archiving] {
            let (status, html) = guest.get(&format!("/f/five/j/{}", job.id)).await;
            assert_eq!(status, StatusCode::OK);
            let html = html.as_str().unwrap();
            let video = format!(
                r#"<meta property="og:video" content="http://localhost:8080/api/f/five/jobs/{}/media?t="#,
                job.id
            );
            assert!(html.contains(&video), "{html}");
            assert!(html.contains(&format!(
                r#"<meta property="og:image" content="http://localhost:8080/api/f/five/jobs/{}/thumbnail?t="#,
                job.id
            )), "{html}");
            // The crawler follows the signed links with no session of its own
            let rest = &html[html.find(&video).unwrap() + video.len()..];
            let token = &rest[..rest.find('"').unwrap()];
            let mut crawler = Client::new(&app);
            let (status, body) = crawler
                .get(&format!("/api/f/five/jobs/{}/media?t={token}", job.id))
                .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body, json!("0123456789"));
            let (status, _, bytes) = crawler
                .raw(&format!("/api/f/five/jobs/{}/thumbnail?t={token}", job.id))
                .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(bytes, b"poster".to_vec());
        }
        let (status, html) = guest.get(&format!("/f/five/j/{}", transcoding.id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!html.as_str().unwrap().contains("og:"), "{html}");

        let (status, page) = guest.get("/api/f/five/jobs").await;
        assert_eq!(status, StatusCode::OK, "{page}");
        let listed: Vec<&str> = page["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|j| j["id"].as_str().unwrap())
            .collect();
        assert_eq!(listed.len(), 2, "{page}");
        assert!(listed.contains(&publishing.id.to_string().as_str()));
        assert!(listed.contains(&archiving.id.to_string().as_str()));
        assert_eq!(
            guest
                .get(&format!("/api/f/five/jobs/{}", transcoding.id))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Discord drops the whole embed of a video past its proxy's limit, so it unfurls as a card
    #[tokio::test]
    async fn a_video_past_the_inline_limit_unfurls_as_a_card() {
        let (app, db) = app_with_admin_db().await;
        let dir = std::env::temp_dir().join(format!("discoclip-front-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut big = seed(&db, &dir, "big", Some("5"), Some("1"), "fixtured").await;
        big.artifacts.output.as_mut().unwrap().size = super::INLINE_VIDEO_LIMIT + 1;
        db.update(&big).await.unwrap();
        let mut fits = seed(&db, &dir, "fits", Some("5"), Some("1"), "fixtured").await;
        fits.artifacts.output.as_mut().unwrap().size = super::INLINE_VIDEO_LIMIT;
        db.update(&fits).await.unwrap();
        open_guild_five(&app).await;

        let mut guest = visitor(&app);
        let (status, html) = guest.get(&format!("/f/five/j/{}", big.id)).await;
        assert_eq!(status, StatusCode::OK);
        let html = html.as_str().unwrap();
        assert!(!html.contains("og:video"), "{html}");
        assert!(!html.contains("twitter:player"), "{html}");
        assert!(
            html.contains(r#"<meta property="og:type" content="website">"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<meta name="twitter:card" content="summary_large_image">"#),
            "{html}"
        );
        assert!(html.contains("<title>Clip big</title>"), "{html}");
        assert!(
            html.contains(r#"<meta property="og:description" content="Fixtured">"#),
            "{html}"
        );
        assert!(html.contains(&format!(
            r#"<meta property="og:image" content="http://localhost:8080/api/f/five/jobs/{}/thumbnail?t="#,
            big.id
        )), "{html}");

        let (status, html) = guest.get(&format!("/f/five/j/{}", fits.id)).await;
        assert_eq!(status, StatusCode::OK);
        let html = html.as_str().unwrap();
        assert!(
            html.contains(r#"<meta property="og:type" content="video.other">"#),
            "{html}"
        );
        assert!(
            html.contains(r#"<meta name="twitter:card" content="player">"#),
            "{html}"
        );

        // The page's own player is told where the media is either way
        let (status, page) = guest.get("/api/f/five/jobs").await;
        assert_eq!(status, StatusCode::OK, "{page}");
        for job in page["jobs"].as_array().unwrap() {
            assert_eq!(job["media"], "video");
            let (status, body) = guest.get(job["media_url"].as_str().unwrap()).await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// Discord lays the provider and author lines out from the oEmbed document the page links to
    #[tokio::test]
    async fn the_page_names_its_view_and_uploader_through_oembed() {
        let (app, db) = app_with_admin_db().await;
        let dir = std::env::temp_dir().join(format!("discoclip-front-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let video = seed(&db, &dir, "v", Some("5"), Some("1"), "fixtured").await;
        let mut sound = seed(&db, &dir, "s", Some("5"), Some("1"), "fixtured").await;
        let resolved = sound.artifacts.resolved.as_mut().unwrap();
        resolved.media = MediaKind::Audio;
        resolved.uploader = None;
        db.update(&sound).await.unwrap();
        let elsewhere = seed(&db, &dir, "e", Some("6"), Some("2"), "fixtured").await;
        open_guild_five(&app).await;

        let mut guest = visitor(&app);
        let (status, html) = guest.get(&format!("/f/five/j/{}", video.id)).await;
        assert_eq!(status, StatusCode::OK);
        let html = html.as_str().unwrap();
        assert!(html.contains(&format!(
            r#"<link rel="alternate" type="application/json+oembed" href="http://localhost:8080/api/f/five/jobs/{}/oembed" title="Clip v">"#,
            video.id
        )), "{html}");
        assert!(
            html.contains(r##"<meta property="theme-color" content="#f2542d">"##),
            "{html}"
        );
        assert!(
            html.contains(r#"<meta property="og:description" content="Fixtured">"#),
            "{html}"
        );
        let (status, doc) = guest
            .get(&format!("/api/f/five/jobs/{}/oembed", video.id))
            .await;
        assert_eq!(status, StatusCode::OK, "{doc}");
        assert_eq!(
            doc,
            json!({
                "version": "1.0", "type": "link", "title": "Clip v",
                "author_name": "someone", "author_url": "https://video.test/someone",
                "provider_name": "Guild Five", "provider_url": "http://localhost:8080/f/five"
            })
        );
        let (status, job) = guest.get(&format!("/api/f/five/jobs/{}", video.id)).await;
        assert_eq!(status, StatusCode::OK, "{job}");
        assert_eq!(job["platform"], "Fixtured");
        assert_eq!(job["uploader_url"], "https://video.test/someone");

        // Sound without cover art shows the app's own mark, and the platform stands as the author
        let (status, html) = guest.get(&format!("/f/five/j/{}", sound.id)).await;
        assert_eq!(status, StatusCode::OK);
        let html = html.as_str().unwrap();
        assert!(
            html.contains(r#"<meta property="og:type" content="music.song">"#),
            "{html}"
        );
        assert!(
            html.contains(
                r#"<meta property="og:image" content="http://localhost:8080/icons/icon-512.png">"#
            ),
            "{html}"
        );
        let (status, doc) = guest
            .get(&format!("/api/f/five/jobs/{}/oembed", sound.id))
            .await;
        assert_eq!(status, StatusCode::OK, "{doc}");
        assert_eq!(doc["author_name"], "Fixtured");
        assert_eq!(doc["author_url"], "https://video.test/watch/s");
        let (status, headers, bytes) = guest.raw("/icons/icon-512.png").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["content-type"], "image/png");
        assert!(bytes.starts_with(b"\x89PNG"));

        assert_eq!(
            guest
                .get(&format!("/api/f/five/jobs/{}/oembed", elsewhere.id))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            guest.get("/api/f/nope/jobs/x/oembed").await.0,
            StatusCode::NOT_FOUND
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
