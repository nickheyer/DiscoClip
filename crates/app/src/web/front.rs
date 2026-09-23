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
use discoclip_engine::job::{Job, JobId, JobStatus};
use discoclip_engine::media::{Container, MediaKind};
use discoclip_engine::store::{JobFilter, Order};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use url::Url;

use super::AppState;
use super::error::ApiError;
use super::jobs::{DownloadQuery, locate, serve_file};
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
    pub uploader: Option<String>,
    pub webpage_url: Option<Url>,
    pub thumbnail: Option<Url>,
    pub duration_secs: Option<f64>,
    /// A recorded stream.
    pub live: bool,
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

fn front_job(state: &AppState, frontend: &Frontend, job: &Job) -> Option<FrontJob> {
    let resolved = job.artifacts.resolved.as_ref()?;
    let output = job.artifacts.output.as_ref()?;
    let token = state.frontends.sign_media(frontend, job.id.0);
    let base = format!("/api/f/{}/jobs/{}", frontend.input.slug, job.id);
    let info = output.info.as_ref();
    let picture = info.and_then(|i| i.video.as_ref());
    let container = info.map(|i| i.container.clone()).unwrap_or_else(|| {
        let ext = output
            .path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("bin");
        Container::from_extension(ext).unwrap_or_else(|| Container::Other(ext.to_string()))
    });
    Some(FrontJob {
        id: job.id,
        title: resolved.title.clone(),
        media: job.media(),
        resolver: resolved.resolver.clone(),
        uploader: resolved.uploader.clone(),
        webpage_url: resolved.webpage_url.clone(),
        thumbnail: resolved.thumbnail.clone(),
        duration_secs: info
            .and_then(|i| i.duration)
            .or(resolved.duration)
            .map(|d| d.as_secs_f64()),
        live: resolved.live,
        size: output.size,
        width: picture.map(|p| p.width),
        height: picture.map(|p| p.height),
        content_type: container.mime().to_string(),
        published_at: job.finished_at.unwrap_or(job.updated_at),
        media_url: format!("{base}/media?t={token}"),
        download_url: frontend.input.downloads.then(|| format!("{base}/download")),
    })
}

/// Whether `frontend` shows `job`: finished with an output, in scope, on a shown platform.
fn shown(state: &AppState, frontend: &Frontend, job: &Job) -> bool {
    job.status == JobStatus::Done
        && job.artifacts.output.is_some()
        && state.frontends.cache().shows(
            frontend,
            job.resolver(),
            job.request.origin.guild.as_deref(),
            job.request.origin.channel.as_deref(),
        )
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
    let (path, filename) = locate(&job, &DownloadQuery::output()).await?;
    serve_file(&path, &filename, true, &headers).await
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
    let (path, filename) = locate(&job, &DownloadQuery::output()).await?;
    serve_file(&path, &filename, false, &headers).await
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

/// The head a front end's media page carries: what Discord and other unfurlers read to
/// show the title and thumbnail and to play a video or audio inline from a plain link.
/// The media link is signed so the unfurler needs no session.
pub fn media_head(base: &Url, frontend: &Frontend, job: &FrontJob) -> String {
    let page = base
        .join(&format!("f/{}/j/{}", frontend.input.slug, job.id))
        .expect("a page path joins");
    let media = base
        .join(job.media_url.trim_start_matches('/'))
        .expect("a media path joins");
    let title = job
        .title
        .clone()
        .unwrap_or_else(|| format!("{} on {}", job.media, frontend.input.name));
    let description = match (&job.uploader, job.duration_secs) {
        (Some(uploader), Some(secs)) => format!("{uploader} · {}", clock(secs)),
        (Some(uploader), None) => uploader.clone(),
        (None, Some(secs)) => clock(secs),
        (None, None) => frontend.input.description.clone(),
    };
    let mut head = String::new();
    head.push_str(&format!("<title>{}</title>\n", escape(&title)));
    head.push_str(&meta("og:site_name", &frontend.input.name));
    head.push_str(&meta("og:title", &title));
    head.push_str(&meta("og:url", page.as_str()));
    head.push_str(&meta("og:description", &description));
    head.push_str(&meta_name("description", &description));
    match job.media {
        MediaKind::Video => {
            head.push_str(&meta("og:type", "video.other"));
            head.push_str(&meta("og:video", media.as_str()));
            head.push_str(&meta("og:video:url", media.as_str()));
            head.push_str(&meta("og:video:secure_url", media.as_str()));
            head.push_str(&meta("og:video:type", &job.content_type));
            if let (Some(w), Some(h)) = (job.width, job.height) {
                head.push_str(&meta("og:video:width", &w.to_string()));
                head.push_str(&meta("og:video:height", &h.to_string()));
            }
            if let Some(thumbnail) = &job.thumbnail {
                head.push_str(&meta("og:image", thumbnail.as_str()));
                head.push_str(&meta_name("twitter:image", thumbnail.as_str()));
            }
            head.push_str(&meta_name("twitter:card", "player"));
            head.push_str(&meta_name("twitter:title", &title));
            head.push_str(&meta_name("twitter:player", page.as_str()));
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
        MediaKind::Audio => {
            head.push_str(&meta("og:type", "music.song"));
            head.push_str(&meta("og:audio", media.as_str()));
            head.push_str(&meta("og:audio:url", media.as_str()));
            head.push_str(&meta("og:audio:secure_url", media.as_str()));
            head.push_str(&meta("og:audio:type", &job.content_type));
            if let Some(thumbnail) = &job.thumbnail {
                head.push_str(&meta("og:image", thumbnail.as_str()));
            }
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
            if let Some(thumbnail) = &job.thumbnail {
                head.push_str(&meta("og:image", thumbnail.as_str()));
            }
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
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .or_else(|| client.origin().and_then(|o| Url::parse(&o).ok()));
    let Some(base) = base else {
        return assets::page_with_head("");
    };
    assets::page_with_head(&front::media_head(&base, &frontend, &front))
}

impl IntoResponse for FrontInfo {
    fn into_response(self) -> Response {
        Json(self).into_response()
    }
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use discoclip_engine::job::{JobStatus, Origin, Request, SourceId};
    use discoclip_engine::media::LocalFile;
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
        assert!(html.contains(r#"<meta property="og:image" content="https://thumbs.test/t.jpg">"#));
        assert!(html.contains(r#"<meta name="twitter:card" content="player">"#));
        assert!(html.contains("<title>Clip a</title>"));
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

        // Links need a public URL, and the URL stays while links are posted.
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
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let (status, body) = admin
            .send(
                Method::PATCH,
                "/api/settings",
                Some(json!({"set": {"web.public_url": "https://clips.example"}})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
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
        let (status, body) = admin
            .send(
                Method::PATCH,
                "/api/settings",
                Some(json!({"reset": ["web.public_url"]})),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let (status, html) = stranger.get(&format!("/f/five/j/{}", in_guild.id)).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            html.as_str()
                .unwrap()
                .contains(r#"<meta property="og:url" content="https://clips.example/f/five/j/"#)
        );
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
}
