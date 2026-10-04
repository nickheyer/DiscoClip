//! The platforms the engine covers: what each resolver takes and returns, what its check
//! links last found when run, the links themselves, and the session its stored cookies
//! make.

use std::future::Future;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use discoclip_engine::http::{Cookie, Jar};
use discoclip_engine::resolve::Platform;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use url::Url;
use uuid::Uuid;

use super::AppState;
use super::auth::{Auth, parse_id};
use super::error::ApiError;
use crate::cookies::{SessionCheckResult, StoredSession};
use crate::fixtures::{LinkError, PlatformCoverage, RunError};
use crate::users::Permission;

/// A platform as the page shows it: what its resolver covers, what its fixtures found,
/// and what is stored about its session.
#[derive(Debug, Serialize)]
pub struct PlatformView {
    #[serde(flatten)]
    pub coverage: PlatformCoverage,
    /// How many cookies the app has saved for the platform. The cookies its requests pick
    /// up along the way are not a session and are not counted here.
    pub cookies: usize,
    /// When the platform's saved cookies were last replaced or cleared from the app.
    pub cookies_updated_at: Option<Timestamp>,
    /// What the platform said about its cookies when last asked.
    pub session_check: Option<SessionCheckResult>,
}

impl PlatformView {
    fn new(coverage: PlatformCoverage, session: Option<StoredSession>) -> Self {
        PlatformView {
            coverage,
            cookies: session.as_ref().map(|s| s.cookies).unwrap_or(0),
            cookies_updated_at: session.as_ref().and_then(|s| s.updated_at),
            session_check: session.and_then(|s| s.check),
        }
    }
}

async fn views(state: &AppState) -> Result<Vec<PlatformView>, ApiError> {
    let mut sessions = state.cookies.summaries().await?;
    Ok(state
        .fixtures
        .coverage()
        .await?
        .into_iter()
        .map(|coverage| {
            let session = sessions.remove(coverage.id);
            PlatformView::new(coverage, session)
        })
        .collect())
}

async fn view(state: &AppState, id: &str) -> Result<PlatformView, ApiError> {
    let coverage = state
        .fixtures
        .platform(id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let session = state.cookies.summaries().await?.remove(id);
    Ok(PlatformView::new(coverage, session))
}

fn platform_named(state: &AppState, id: &str) -> Result<Platform, ApiError> {
    state
        .engine
        .platforms()
        .into_iter()
        .find(|p| p.id == id)
        .ok_or(ApiError::NotFound)
}

impl From<RunError> for ApiError {
    fn from(error: RunError) -> Self {
        match error {
            RunError::Unknown(_) | RunError::UnknownLink(_) => ApiError::NotFound,
            RunError::NoFixtures(_) | RunError::Nothing => ApiError::BadRequest(error.to_string()),
            RunError::Busy(_) | RunError::AllBusy => ApiError::Conflict(error.to_string()),
            RunError::Store(inner) => ApiError::Internal(inner.to_string()),
        }
    }
}

impl From<LinkError> for ApiError {
    fn from(error: LinkError) -> Self {
        match error {
            LinkError::Duplicate { .. } => ApiError::Conflict(error.to_string()),
            LinkError::NotFound(_) => ApiError::NotFound,
            LinkError::Store(inner) => ApiError::Internal(inner.to_string()),
        }
    }
}

/// Every platform with its coverage, its fixtures' latest results and its session.
pub async fn list(
    State(state): State<AppState>,
    Auth(_): Auth,
) -> Result<Json<Vec<PlatformView>>, ApiError> {
    Ok(Json(views(&state).await?))
}

pub async fn get(
    State(state): State<AppState>,
    Auth(_): Auth,
    Path(id): Path<String>,
) -> Result<Json<PlatformView>, ApiError> {
    Ok(Json(view(&state, &id).await?))
}

/// The platforms a run was just started for.
#[derive(Debug, Serialize)]
pub struct CheckStarted {
    pub platforms: Vec<&'static str>,
}

/// Starts a run of every platform's links that is not already running.
pub async fn check_all(
    State(state): State<AppState>,
    Auth(identity): Auth,
) -> Result<(StatusCode, Json<CheckStarted>), ApiError> {
    identity.require(Permission::ManageJobs)?;
    let platforms = state.fixtures.start_all().await?;
    tracing::info!(
        by = identity.user.username,
        ?platforms,
        "platform checks started"
    );
    Ok((StatusCode::ACCEPTED, Json(CheckStarted { platforms })))
}

/// Starts a run of one platform's links and returns the platform as it stands.
pub async fn check(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<PlatformView>), ApiError> {
    identity.require(Permission::ManageJobs)?;
    state.fixtures.start(&id).await?;
    tracing::info!(
        by = identity.user.username,
        platform = id,
        "platform check started"
    );
    Ok((StatusCode::ACCEPTED, Json(view(&state, &id).await?)))
}

/// Starts a run of one of a platform's links and returns the platform as it stands.
pub async fn check_link(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path((id, link)): Path<(String, String)>,
) -> Result<(StatusCode, Json<PlatformView>), ApiError> {
    identity.require(Permission::ManageJobs)?;
    let link: Uuid = parse_id(&link)?;
    state.fixtures.start_link(&id, link).await?;
    tracing::info!(
        by = identity.user.username,
        platform = id,
        link = %link,
        "check link run started"
    );
    Ok((StatusCode::ACCEPTED, Json(view(&state, &id).await?)))
}

/// A link to check a platform with.
#[derive(Debug, Deserialize)]
pub struct LinkBody {
    pub url: String,
}

/// What changes on a link: its address, whether it is in use, or both.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct LinkChange {
    pub url: Option<String>,
    pub enabled: Option<bool>,
}

/// `url` as a link `platform`'s resolver takes, spelled as the resolver will see it.
fn link_for(state: &AppState, platform: &Platform, url: &str) -> Result<String, ApiError> {
    let parsed = Url::parse(url.trim())
        .map_err(|e| ApiError::BadRequest(format!("{url:?} is not a URL: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ApiError::BadRequest(
            "a check link is an http or https URL".into(),
        ));
    }
    if !state.engine.resolvers_for(&parsed).contains(&platform.id) {
        return Err(ApiError::BadRequest(format!(
            "{} does not take {parsed}",
            platform.name
        )));
    }
    Ok(parsed.to_string())
}

/// Adds a link to check a platform with.
pub async fn add_link(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
    Json(body): Json<LinkBody>,
) -> Result<(StatusCode, Json<PlatformView>), ApiError> {
    identity.require(Permission::ManageJobs)?;
    let platform = platform_named(&state, &id)?;
    let url = link_for(&state, &platform, &body.url)?;
    let link = state.fixtures.add_link(platform.id, &url).await?;
    tracing::info!(
        by = identity.user.username,
        platform = platform.id,
        link = %link.id,
        url,
        "check link added"
    );
    Ok((StatusCode::CREATED, Json(view(&state, platform.id).await?)))
}

/// The link of `platform` called `link`, or 404.
async fn link_of(state: &AppState, platform: &Platform, link: &str) -> Result<Uuid, ApiError> {
    let id: Uuid = parse_id(link)?;
    state
        .fixtures
        .link(id)
        .await?
        .filter(|l| l.platform == platform.id)
        .map(|l| l.id)
        .ok_or(ApiError::NotFound)
}

/// Changes a link's address, or switches it on or off.
pub async fn change_link(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path((id, link)): Path<(String, String)>,
    Json(change): Json<LinkChange>,
) -> Result<Json<PlatformView>, ApiError> {
    identity.require(Permission::ManageJobs)?;
    let platform = platform_named(&state, &id)?;
    let link = link_of(&state, &platform, &link).await?;
    if change.url.is_none() && change.enabled.is_none() {
        return Err(ApiError::BadRequest("send a url, enabled, or both".into()));
    }
    if let Some(url) = &change.url {
        let url = link_for(&state, &platform, url)?;
        state.fixtures.edit_link(link, &url).await?;
        tracing::info!(
            by = identity.user.username,
            platform = platform.id,
            link = %link,
            url,
            "check link changed"
        );
    }
    if let Some(enabled) = change.enabled {
        state.fixtures.set_enabled(link, enabled).await?;
        tracing::info!(
            by = identity.user.username,
            platform = platform.id,
            link = %link,
            enabled,
            "check link switched"
        );
    }
    Ok(Json(view(&state, platform.id).await?))
}

/// Removes a link.
pub async fn remove_link(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path((id, link)): Path<(String, String)>,
) -> Result<Json<PlatformView>, ApiError> {
    identity.require(Permission::ManageJobs)?;
    let platform = platform_named(&state, &id)?;
    let link = link_of(&state, &platform, &link).await?;
    state.fixtures.remove_link(link).await?;
    tracing::info!(
        by = identity.user.username,
        platform = platform.id,
        link = %link,
        "check link removed"
    );
    Ok(Json(view(&state, platform.id).await?))
}

/// How cookies are given: a Netscape `cookies.txt`, or one `Cookie` header's worth.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CookieFormat {
    Netscape,
    Header,
}

impl CookieFormat {
    fn as_str(self) -> &'static str {
        match self {
            CookieFormat::Netscape => "netscape",
            CookieFormat::Header => "header",
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct CookiesImport {
    pub format: CookieFormat,
    pub text: String,
    /// The domain cookies given as a header belong to. The platform's first host otherwise.
    #[serde(default)]
    pub domain: Option<String>,
}

/// The platform after its cookies changed, and why it could not be asked about the
/// session they make, when it could not.
#[derive(Debug, Serialize)]
pub struct SessionOutcome {
    #[serde(flatten)]
    pub platform: PlatformView,
    pub check_error: Option<String>,
}

fn parse_cookies(platform: &Platform, body: &CookiesImport) -> Result<Jar, ApiError> {
    let jar = match body.format {
        CookieFormat::Netscape => Jar::import_netscape(&body.text)
            .map_err(|e| ApiError::BadRequest(format!("cookies.txt: {e}")))?,
        CookieFormat::Header => {
            let domain = body
                .domain
                .as_deref()
                .map(str::trim)
                .filter(|d| !d.is_empty())
                .map(str::to_string)
                .or_else(|| {
                    platform
                        .hosts
                        .iter()
                        .find(|h| **h != "*")
                        .map(|h| h.to_string())
                })
                .ok_or_else(|| {
                    ApiError::BadRequest("a domain is needed for cookies given as a header".into())
                })?;
            let mut jar = Jar::new();
            for pair in body.text.split(';') {
                let pair = pair.trim();
                if pair.is_empty() {
                    continue;
                }
                let (name, value) = pair
                    .split_once('=')
                    .ok_or_else(|| ApiError::BadRequest(format!("{pair:?} is not name=value")))?;
                if name.trim().is_empty() {
                    return Err(ApiError::BadRequest(format!("{pair:?} has no name")));
                }
                jar.insert(Cookie::new(name.trim(), value.trim(), &domain));
            }
            jar
        }
    };
    if jar.is_empty() {
        return Err(ApiError::BadRequest("no cookies found in the text".into()));
    }
    Ok(jar)
}

/// Runs `check` under the configured outgoing request timeout, so a platform that never
/// answers cannot hold the request open.
async fn within_timeout<T>(
    state: &AppState,
    id: &str,
    check: impl Future<Output = T>,
) -> Result<T, ApiError> {
    let secs = state.engine.http().config().request_timeout_secs.max(1);
    tokio::time::timeout(Duration::from_secs(secs), check)
        .await
        .map_err(|_| {
            ApiError::BadGateway(format!(
                "{id} did not answer about its session within {secs}s"
            ))
        })
}

/// Asks the platform what its stored cookies are worth, and keeps the answer.
async fn ask_platform(state: &AppState, id: &str) -> Result<SessionCheckResult, ApiError> {
    let session = within_timeout(state, id, state.engine.check_session(id))
        .await?
        .map_err(|e| {
            ApiError::BadGateway(format!("{id} could not be asked about its session: {e}"))
        })?;
    Ok(state.cookies.record_check(id, &session.check).await?)
}

/// Replaces a platform's cookies, then asks the platform what session they make.
pub async fn import_cookies(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
    Json(body): Json<CookiesImport>,
) -> Result<Json<SessionOutcome>, ApiError> {
    identity.require(Permission::ManageSettings)?;
    let platform = platform_named(&state, &id)?;
    let jar = parse_cookies(&platform, &body)?;
    state.engine.http().set_jar(platform.id, jar.clone());
    state
        .cookies
        .replace(&identity.actor(), platform.id, &jar, body.format.as_str())
        .await?;
    tracing::info!(
        by = identity.user.username,
        platform = platform.id,
        cookies = jar.len(),
        "session cookies replaced"
    );
    let check_error = ask_platform(&state, platform.id)
        .await
        .err()
        .map(|e| e.message());
    Ok(Json(SessionOutcome {
        platform: view(&state, platform.id).await?,
        check_error,
    }))
}

/// Removes a platform's cookies.
pub async fn clear_cookies(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<Json<PlatformView>, ApiError> {
    identity.require(Permission::ManageSettings)?;
    let platform = platform_named(&state, &id)?;
    state.engine.http().clear_jar(platform.id);
    state.cookies.clear(&identity.actor(), platform.id).await?;
    tracing::info!(
        by = identity.user.username,
        platform = platform.id,
        "session cookies cleared"
    );
    Ok(Json(view(&state, platform.id).await?))
}

/// Asks the platform what its stored cookies are worth now.
pub async fn check_session(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<Json<PlatformView>, ApiError> {
    identity.require(Permission::ManageSettings)?;
    let platform = platform_named(&state, &id)?;
    ask_platform(&state, platform.id).await?;
    Ok(Json(view(&state, platform.id).await?))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use axum::http::{Method, StatusCode};
    use discoclip_engine::http::{Cookie, Jar};
    use serde_json::{Value as Json, json};

    use super::within_timeout;
    use crate::users::Role;
    use crate::web::testing::{Client, FIXTURE_HOST, app_with_admin, app_with_admin_db, wait_for};

    #[tokio::test]
    async fn session_cookies_are_imported_checked_and_cleared() {
        let app = app_with_admin().await;
        app.state
            .users
            .create("op", Some("battery staple"), Role::Operator)
            .await
            .unwrap();
        let mut admin = Client::new(&app);
        admin.login("nick", "correct horse").await;
        let mut op = Client::new(&app);
        op.login("op", "battery staple").await;

        let (_, body) = admin.get("/api/platforms/fixtured").await;
        assert_eq!(body["cookies"], 0);
        assert!(body["cookies_updated_at"].is_null());
        assert!(body["session_check"].is_null());

        // Cookies the platform's own requests pick up along the way are no saved session:
        // the count and the date it carries come from what the app stored.
        app.state.engine.http().set_jar(
            "fixtured",
            Jar::from_cookies(vec![Cookie::new("visited", "1", FIXTURE_HOST)]),
        );
        let (_, body) = admin.get("/api/platforms/fixtured").await;
        assert_eq!(body["cookies"], 0, "{body}");
        assert!(body["cookies_updated_at"].is_null(), "{body}");

        // Operators may not. Admins import a cookies.txt and the platform is asked.
        let netscape = json!({
            "format": "netscape",
            "text": "# Netscape HTTP Cookie File\n.fixture.test\tTRUE\t/\tTRUE\t2147483647\tsid\tsecret-value\n"
        });
        assert_eq!(
            op.send(
                Method::PUT,
                "/api/platforms/fixtured/cookies",
                Some(netscape.clone())
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        let (status, body) = admin
            .send(
                Method::PUT,
                "/api/platforms/fixtured/cookies",
                Some(netscape),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["cookies"], 1);
        assert!(!body["cookies_updated_at"].is_null());
        assert_eq!(body["session_check"]["state"], "logged_in");
        assert_eq!(body["session_check"]["account"], "tester");
        assert!(body["check_error"].is_null());
        assert_eq!(app.state.engine.http().jar("fixtured").len(), 1);
        assert!(!body.to_string().contains("secret-value"));

        // A header's worth of cookies replaces the jar, on the platform's own domain.
        let (status, body) = admin
            .send(
                Method::PUT,
                "/api/platforms/fixtured/cookies",
                Some(json!({"format": "header", "text": "a=1; b=2"})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["cookies"], 2);
        assert_eq!(body["session_check"]["state"], "logged_out");
        let jar = app.state.engine.http().jar("fixtured");
        assert_eq!(jar.get("a").unwrap().domain, "fixture.test");
        assert!(jar.get("sid").is_none());

        // What is refused: bad text, a header for a platform without a host of its own.
        for (body, message) in [
            (
                json!({"format": "netscape", "text": "not\ttabs"}),
                "cookies.txt",
            ),
            (json!({"format": "header", "text": "novalue"}), "name=value"),
            (json!({"format": "header", "text": "  ;  "}), "no cookies"),
            (json!({"format": "netscape", "text": ""}), "no cookies"),
        ] {
            let (status, answer) = admin
                .send(Method::PUT, "/api/platforms/fixtured/cookies", Some(body))
                .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{answer}");
            assert!(
                answer["error"].as_str().unwrap().contains(message),
                "{answer}"
            );
        }
        assert_eq!(
            admin
                .send(
                    Method::PUT,
                    "/api/platforms/unknown/cookies",
                    Some(json!({"format": "header", "text": "a=1"}))
                )
                .await
                .0,
            StatusCode::NOT_FOUND
        );

        // The check runs on request. Clearing empties the jar and the platform is logged out.
        let (status, body) = admin
            .post("/api/platforms/fixtured/session/check", Json::Null)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["session_check"]["state"], "logged_out");
        assert_eq!(
            op.post("/api/platforms/fixtured/session/check", Json::Null)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            admin
                .post("/api/platforms/nothing/session/check", Json::Null)
                .await
                .1["session_check"]["state"],
            "unsupported"
        );
        let (status, body) = admin.delete("/api/platforms/fixtured/cookies").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["cookies"], 0);
        assert!(app.state.engine.http().jar("fixtured").is_empty());
        assert_eq!(
            op.delete("/api/platforms/fixtured/cookies").await.0,
            StatusCode::FORBIDDEN
        );

        // The audit log has the imports and the clearing, with counts and never a cookie.
        let (status, body) = admin
            .get("/api/audit?target_kind=platform&target_id=fixtured")
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let entries = body["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0]["action"], "session.clear");
        assert_eq!(entries[0]["details"]["cookies"], 2);
        assert_eq!(entries[1]["action"], "session.import");
        assert_eq!(entries[1]["details"]["format"], "header");
        assert_eq!(entries[1]["details"]["previous_cookies"], 1);
        assert_eq!(entries[2]["details"]["format"], "netscape");
        assert!(!body.to_string().contains("secret-value"));
        let (_, body) = admin.get("/api/audit?action=session.import").await;
        assert_eq!(body["entries"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_session_check_cannot_outlast_the_configured_timeout() {
        let app = app_with_admin().await;
        let mut config = app.state.engine.http().config();
        config.request_timeout_secs = 1;
        app.state.engine.http().configure(config);

        // A platform that never answers is given up on, and one that answers is not cut short.
        let began = Instant::now();
        let error = within_timeout(&app.state, "fixtured", std::future::pending::<()>())
            .await
            .unwrap_err();
        assert!(began.elapsed() < Duration::from_secs(10));
        let message = error.message();
        assert!(message.contains("did not answer"), "{message}");
        assert!(message.contains("1s"), "{message}");
        assert_eq!(
            within_timeout(&app.state, "fixtured", async { 7 })
                .await
                .unwrap(),
            7
        );
    }

    #[tokio::test]
    async fn platforms_list_their_coverage_and_run_their_links() {
        let (app, db) = app_with_admin_db().await;
        app.state
            .users
            .create("viewer", Some("battery staple"), Role::Viewer)
            .await
            .unwrap();
        let mut admin = Client::new(&app);
        admin.login("nick", "correct horse").await;
        let mut viewer = Client::new(&app);
        viewer.login("viewer", "battery staple").await;

        let (status, body) = viewer.get("/api/platforms").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let platforms = body.as_array().unwrap();
        assert_eq!(platforms.len(), 2);
        let fixtured = platforms.iter().find(|p| p["id"] == "fixtured").unwrap();
        assert_eq!(fixtured["name"], "Fixtured");
        assert_eq!(fixtured["hosts"], json!(["fixture.test"]));
        assert_eq!(fixtured["features"], json!(["videos"]));
        assert_eq!(fixtured["formats"], json!(["mp4"]));
        assert_eq!(fixtured["session"], "optional");
        assert_eq!(fixtured["cookies"], 0);
        assert_eq!(fixtured["running"], false);
        assert_eq!(fixtured["health"], "unknown");
        assert!(fixtured["last_run_at"].is_null());
        assert!(fixtured["last_pass_at"].is_null());
        assert!(fixtured["last_job_at"].is_null());
        assert_eq!(fixtured["passed"], 0);
        assert_eq!(fixtured["login_required"], 0);
        let fixtures = fixtured["fixtures"].as_array().unwrap();
        assert_eq!(
            fixtures.len(),
            4,
            "the links the platform ships with are seeded"
        );
        assert!(fixtures.iter().all(|f| f["status"] == "never"));
        assert!(fixtures.iter().all(|f| f["origin"] == "builtin"));
        assert!(fixtures.iter().all(|f| f["enabled"] == true));
        assert!(fixtures.iter().all(|f| f["id"].is_string()));
        let nothing = platforms.iter().find(|p| p["id"] == "nothing").unwrap();
        assert_eq!(nothing["fixtures"], json!([]));
        assert_eq!(nothing["health"], "unknown");
        let by_url = |body: &Json, path: &str| {
            body["fixtures"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["url"] == format!("https://fixture.test{path}"))
                .unwrap()
                .clone()
        };
        let link_id =
            |body: &Json, path: &str| by_url(body, path)["id"].as_str().unwrap().to_string();

        // Viewers look. Running and editing take managing jobs.
        assert_eq!(
            viewer
                .post("/api/platforms/fixtured/check", Json::Null)
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            viewer.post("/api/platforms/check", Json::Null).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            viewer
                .post(
                    "/api/platforms/fixtured/fixtures",
                    json!({"url": "https://fixture.test/more"})
                )
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            admin
                .post("/api/platforms/unknown/check", Json::Null)
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            admin.get("/api/platforms/unknown").await.0,
            StatusCode::NOT_FOUND
        );
        let (status, body) = admin.post("/api/platforms/nothing/check", Json::Null).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        // A run starts in the background and stops at the first link that resolves.
        let (status, body) = admin
            .post("/api/platforms/fixtured/check", Json::Null)
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let settled = |admin: &Client, want: &'static str| {
            let cookie = admin.cookie.clone();
            let app = &app;
            async move {
                wait_for(want, || {
                    let mut client = Client::new(app);
                    client.cookie = cookie.clone();
                    async move {
                        let (_, body) = client.get("/api/platforms/fixtured").await;
                        (body["running"] == false && !body["last_run_at"].is_null()).then_some(body)
                    }
                })
                .await
            }
        };
        let done = settled(&admin, "the first run to finish").await;
        assert_eq!(done["health"], "working", "{done}");
        assert_eq!(done["passed"], 1);
        assert_eq!(done["failed"], 0);
        assert_eq!(done["login_required"], 0);
        assert_eq!(done["last_pass_at"], done["last_run_at"]);
        let ok = by_url(&done, "/ok");
        assert_eq!(ok["status"], "pass");
        assert_eq!(ok["title"], "A fixture");
        assert!(ok["error"].is_null());
        assert_eq!(ok["last_pass_at"], ok["run_at"]);
        assert!(ok["duration_ms"].is_number());
        for path in ["/bad", "/slow", "/login"] {
            assert_eq!(
                by_url(&done, path)["status"],
                "never",
                "{path} was not needed"
            );
        }

        // With the resolving link switched off, the run moves on to the next: the broken
        // link fails, the slow one resolves, and the broken one is switched off quietly.
        let ok_id = link_id(&done, "/ok");
        let (status, body) = admin
            .send(
                Method::PATCH,
                &format!("/api/platforms/fixtured/fixtures/{ok_id}"),
                Some(json!({"enabled": false})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(by_url(&body, "/ok")["enabled"], false);
        assert_eq!(body["passed"], 0, "a link switched off does not count");
        assert_eq!(body["health"], "unknown");
        let (status, body) = admin
            .post("/api/platforms/fixtured/check", Json::Null)
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        // The slow link takes a while, and a second start meanwhile is refused.
        let (status, body) = admin
            .post("/api/platforms/fixtured/check", Json::Null)
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let (status, body) = admin.post("/api/platforms/check", Json::Null).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let done =
            wait_for("the second run to finish", || {
                let mut client = Client::new(&app);
                client.cookie = admin.cookie.clone();
                async move {
                    let (_, body) = client.get("/api/platforms/fixtured").await;
                    (body["running"] == false
                        && body["fixtures"].as_array().unwrap().iter().any(|f| {
                            f["url"] == "https://fixture.test/slow" && f["status"] == "pass"
                        }))
                    .then_some(body)
                }
            })
            .await;
        assert_eq!(done["health"], "working", "{done}");
        let bad = by_url(&done, "/bad");
        assert_eq!(bad["status"], "fail");
        assert_eq!(
            bad["enabled"], false,
            "it failed while another link resolved"
        );
        // The message says what went wrong and never repeats the link the row already shows.
        assert_eq!(bad["error"], "No video found");
        assert_eq!(bad["disabled_reason"], "No video found");
        assert!(bad["last_pass_at"].is_null());
        let slow = by_url(&done, "/slow");
        assert_eq!(slow["status"], "pass");
        assert!(slow["duration_ms"].as_u64().unwrap() >= 300);
        assert_eq!(by_url(&done, "/login")["status"], "never");
        assert_eq!(done["passed"], 1);
        assert_eq!(done["failed"], 0, "the link switched off does not count");

        // One link runs on its own, and switches nothing off. A link that wants a login is
        // told apart from a broken one.
        let login_id = link_id(&done, "/login");
        let (status, body) = admin
            .post(
                &format!("/api/platforms/fixtured/fixtures/{login_id}/check"),
                Json::Null,
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let done = wait_for("the login link to run", || {
            let mut client = Client::new(&app);
            client.cookie = admin.cookie.clone();
            async move {
                let (_, body) = client.get("/api/platforms/fixtured").await;
                (body["running"] == false
                    && body["fixtures"].as_array().unwrap().iter().any(|f| {
                        f["url"] == "https://fixture.test/login" && f["status"] != "never"
                    }))
                .then_some(body)
            }
        })
        .await;
        let login = by_url(&done, "/login");
        assert_eq!(login["status"], "login_required");
        assert_eq!(login["enabled"], true);
        assert_eq!(
            login["error"],
            "Needs a logged-in fixtured session: members' links are read with the sid cookie",
            "{login}"
        );
        assert_eq!(done["login_required"], 1);
        assert_eq!(done["health"], "working", "the slow link still resolves");
        assert_eq!(
            admin
                .post(
                    &format!(
                        "/api/platforms/fixtured/fixtures/{}/check",
                        uuid::Uuid::now_v7()
                    ),
                    Json::Null
                )
                .await
                .0,
            StatusCode::NOT_FOUND
        );

        // With a session stored, the link resolves on its next run.
        let (status, body) = admin
            .send(
                Method::PUT,
                "/api/platforms/fixtured/cookies",
                Some(json!({
                    "format": "netscape",
                    "text": "# Netscape HTTP Cookie File\n.fixture.test\tTRUE\t/\tTRUE\t2147483647\tsid\tsecret-value\n"
                })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = admin
            .post(
                &format!("/api/platforms/fixtured/fixtures/{login_id}/check"),
                Json::Null,
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let done =
            wait_for("the login link to resolve", || {
                let mut client = Client::new(&app);
                client.cookie = admin.cookie.clone();
                async move {
                    let (_, body) = client.get("/api/platforms/fixtured").await;
                    (body["running"] == false
                        && body["fixtures"].as_array().unwrap().iter().any(|f| {
                            f["url"] == "https://fixture.test/login" && f["status"] == "pass"
                        }))
                    .then_some(body)
                }
            })
            .await;
        assert_eq!(by_url(&done, "/login")["title"], "A members-only fixture");
        assert_eq!(done["login_required"], 0);
        assert_eq!(done["passed"], 2);

        // A link switched back on by hand loses the reason it went off, and resolves once
        // the platform recovers.
        let bad_id = link_id(&done, "/bad");
        let (status, body) = admin
            .send(
                Method::PATCH,
                &format!("/api/platforms/fixtured/fixtures/{bad_id}"),
                Some(json!({"enabled": true})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let bad = by_url(&body, "/bad");
        assert_eq!(bad["enabled"], true);
        assert!(bad["disabled_reason"].is_null());
        assert_eq!(body["failed"], 1, "its last result still stands");
        assert_eq!(body["health"], "working");

        // Links are added by hand, must be ones the platform takes, and may not repeat.
        for (body, code) in [
            (json!({"url": "not a url"}), StatusCode::BAD_REQUEST),
            (
                json!({"url": "ftp://fixture.test/x"}),
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({"url": "https://elsewhere.test/clip"}),
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({"url": "https://fixture.test/ok"}),
                StatusCode::CONFLICT,
            ),
        ] {
            let (status, answer) = admin.post("/api/platforms/fixtured/fixtures", body).await;
            assert_eq!(status, code, "{answer}");
        }
        let (status, body) = admin
            .post(
                "/api/platforms/fixtured/fixtures",
                json!({"url": "https://fixture.test/mine"}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let mine = by_url(&body, "/mine");
        assert_eq!(mine["origin"], "custom");
        assert_eq!(mine["status"], "never");
        let mine_id = mine["id"].as_str().unwrap().to_string();
        assert_eq!(
            admin
                .post(
                    "/api/platforms/nothing/fixtures",
                    json!({"url": "https://fixture.test/mine"})
                )
                .await
                .0,
            StatusCode::BAD_REQUEST,
            "a link of another platform"
        );

        // Edited to a new address, a link starts over.
        let (status, body) = admin
            .send(
                Method::PATCH,
                &format!("/api/platforms/fixtured/fixtures/{mine_id}"),
                Some(json!({"url": "https://fixture.test/ok"})),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let (status, body) = admin
            .send(
                Method::PATCH,
                &format!("/api/platforms/fixtured/fixtures/{mine_id}"),
                Some(json!({})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let (status, body) = admin
            .send(
                Method::PATCH,
                &format!("/api/platforms/fixtured/fixtures/{mine_id}"),
                Some(json!({"url": "https://fixture.test/moved"})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(by_url(&body, "/moved")["id"], mine_id);
        assert_eq!(body["fixtures"].as_array().unwrap().len(), 5);

        // Removed, a link is gone; a shipped link stays gone across a reseed.
        let (status, body) = admin
            .delete(&format!("/api/platforms/fixtured/fixtures/{mine_id}"))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["fixtures"].as_array().unwrap().len(), 4);
        let (status, body) = admin
            .delete(&format!("/api/platforms/fixtured/fixtures/{bad_id}"))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["fixtures"].as_array().unwrap().len(), 3);
        assert_eq!(
            admin
                .delete(&format!("/api/platforms/fixtured/fixtures/{bad_id}"))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            admin
                .delete(&format!("/api/platforms/nothing/fixtures/{login_id}"))
                .await
                .0,
            StatusCode::NOT_FOUND,
            "a link of another platform"
        );

        // A job that finishes on the platform is proof it works, and its link is kept to
        // check the platform with from then on.
        let mut job = discoclip_engine::Job::new(discoclip_engine::Request::new(
            discoclip_engine::Origin {
                source: discoclip_engine::SourceId::new("local"),
                reference: "nick".into(),
                url: None,
                guild: None,
                channel: None,
            },
            url::Url::parse("https://fixture.test/fresh").unwrap(),
        ));
        job.artifacts.resolved = Some(discoclip_engine::Resolved::new("fixtured"));
        job.status = discoclip_engine::JobStatus::Done;
        job.finished_at = Some(jiff::Timestamp::now());
        use discoclip_engine::JobStore;
        db.insert(&job).await.unwrap();
        assert!(
            app.state
                .fixtures
                .learn("fixtured", "https://fixture.test/fresh")
                .await
                .unwrap()
        );
        let (status, body) = admin.get("/api/platforms/fixtured").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(!body["last_job_at"].is_null(), "{body}");
        let fresh = by_url(&body, "/fresh");
        assert_eq!(fresh["origin"], "job");
        assert_eq!(fresh["enabled"], true);
        let (status, body) = admin.get("/api/platforms").await;
        assert_eq!(status, StatusCode::OK);
        let fixtured = body
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "fixtured")
            .unwrap()
            .clone();
        assert_eq!(fixtured["health"], "working");
        assert_eq!(fixtured["fixtures"].as_array().unwrap().len(), 4);
    }
}
