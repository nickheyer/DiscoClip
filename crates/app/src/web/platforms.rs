//! The platforms the engine covers: what each resolver takes and returns, what its
//! fixture links last found when run, and the session its stored cookies make.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use discoclip_engine::http::{Cookie, Jar};
use discoclip_engine::resolve::Platform;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};

use super::AppState;
use super::auth::Auth;
use super::error::ApiError;
use crate::cookies::SessionCheckResult;
use crate::fixtures::{PlatformCoverage, RunError};
use crate::users::Permission;

/// A platform as the page shows it: what its resolver covers, what its fixtures found,
/// and what is stored about its session.
#[derive(Debug, Serialize)]
pub struct PlatformView {
    #[serde(flatten)]
    pub coverage: PlatformCoverage,
    /// When the platform's cookies were last replaced or cleared from the app.
    pub cookies_updated_at: Option<Timestamp>,
    /// What the platform said about its cookies when last asked.
    pub session_check: Option<SessionCheckResult>,
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
            PlatformView {
                coverage,
                cookies_updated_at: session.as_ref().and_then(|s| s.updated_at),
                session_check: session.and_then(|s| s.check),
            }
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
    Ok(PlatformView {
        coverage,
        cookies_updated_at: session.as_ref().and_then(|s| s.updated_at),
        session_check: session.and_then(|s| s.check),
    })
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
            RunError::Unknown(_) => ApiError::NotFound,
            RunError::NoFixtures(_) | RunError::Nothing => ApiError::BadRequest(error.to_string()),
            RunError::Busy(_) | RunError::AllBusy => ApiError::Conflict(error.to_string()),
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

/// Starts a run of every platform's fixtures that is not already running.
pub async fn check_all(
    State(state): State<AppState>,
    Auth(identity): Auth,
) -> Result<(StatusCode, Json<CheckStarted>), ApiError> {
    identity.require(Permission::ManageJobs)?;
    let platforms = state.fixtures.start_all()?;
    tracing::info!(
        by = identity.user.username,
        ?platforms,
        "fixture run started"
    );
    Ok((StatusCode::ACCEPTED, Json(CheckStarted { platforms })))
}

/// Starts a run of one platform's fixtures and returns the platform as it stands.
pub async fn check(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<PlatformView>), ApiError> {
    identity.require(Permission::ManageJobs)?;
    state.fixtures.start(&id)?;
    tracing::info!(
        by = identity.user.username,
        platform = id,
        "fixture run started"
    );
    Ok((StatusCode::ACCEPTED, Json(view(&state, &id).await?)))
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
    /// The domain cookies given as a header belong to; the platform's first host otherwise.
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

/// Asks the platform what its stored cookies are worth, and keeps the answer.
async fn ask_platform(state: &AppState, id: &str) -> Result<SessionCheckResult, ApiError> {
    let session = state.engine.check_session(id).await.map_err(|e| {
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
    use axum::http::{Method, StatusCode};
    use serde_json::{Value as Json, json};

    use crate::users::Role;
    use crate::web::testing::{Client, app_with_admin, wait_for};

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

        // Operators may not; admins import a cookies.txt and the platform is asked.
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

        // The check runs on request; clearing empties the jar and the platform is logged out.
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
    async fn platforms_list_their_coverage_and_run_their_fixtures() {
        let app = app_with_admin().await;
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
        assert!(fixtured["last_run_at"].is_null());
        assert!(fixtured["last_pass_at"].is_null());
        assert_eq!(fixtured["passed"], 0);
        let fixtures = fixtured["fixtures"].as_array().unwrap();
        assert_eq!(fixtures.len(), 3);
        assert!(fixtures.iter().all(|f| f["status"] == "never"));
        let nothing = platforms.iter().find(|p| p["id"] == "nothing").unwrap();
        assert_eq!(nothing["fixtures"], json!([]));

        // Viewers look; running takes managing jobs.
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

        // A run starts in the background; a second start meanwhile is refused.
        let (status, body) = admin
            .post("/api/platforms/fixtured/check", Json::Null)
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert_eq!(body["running"], true);
        let (status, body) = admin
            .post("/api/platforms/fixtured/check", Json::Null)
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let (status, body) = admin.post("/api/platforms/check", Json::Null).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let done = wait_for("the fixtures to finish", || {
            let mut client = Client::new(&app);
            client.cookie = admin.cookie.clone();
            async move {
                let (_, body) = client.get("/api/platforms/fixtured").await;
                (body["running"] == false && !body["last_run_at"].is_null()).then_some(body)
            }
        })
        .await;
        assert_eq!(done["passed"], 2);
        assert_eq!(done["failed"], 1);
        assert!(done["last_pass_at"].is_null(), "{done}");
        assert!(!done["last_fail_at"].is_null());
        let by_url = |body: &Json, path: &str| {
            body["fixtures"]
                .as_array()
                .unwrap()
                .iter()
                .find(|f| f["url"] == format!("https://fixture.test{path}"))
                .unwrap()
                .clone()
        };
        let ok = by_url(&done, "/ok");
        assert_eq!(ok["status"], "pass");
        assert_eq!(ok["title"], "A fixture");
        assert!(ok["error"].is_null());
        assert!(!ok["run_at"].is_null());
        assert_eq!(ok["last_pass_at"], ok["run_at"]);
        assert!(ok["duration_ms"].is_number());
        let bad = by_url(&done, "/bad");
        assert_eq!(bad["status"], "fail");
        assert!(bad["error"].as_str().unwrap().contains("no video found"));
        assert!(bad["last_pass_at"].is_null());
        let slow = by_url(&done, "/slow");
        assert_eq!(slow["status"], "pass");
        assert!(slow["duration_ms"].as_u64().unwrap() >= 300);

        // The failing link recovers, every fixture passes, and the platform's pass date is set.
        let (status, body) = admin.post("/api/platforms/check", Json::Null).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert_eq!(body["platforms"], json!(["fixtured"]));
        let done = wait_for("the second run to finish", || {
            let mut client = Client::new(&app);
            client.cookie = admin.cookie.clone();
            async move {
                let (_, body) = client.get("/api/platforms/fixtured").await;
                (body["running"] == false && body["passed"] == 3).then_some(body)
            }
        })
        .await;
        assert_eq!(done["failed"], 0);
        assert_eq!(done["last_pass_at"], done["last_run_at"]);
        assert!(done["last_fail_at"].as_str().unwrap() < done["last_pass_at"].as_str().unwrap());
        let bad = by_url(&done, "/bad");
        assert_eq!(bad["status"], "pass");
        assert_eq!(bad["title"], "A recovered fixture");
        assert!(bad["error"].is_null());
        let (status, body) = admin.get("/api/platforms").await;
        assert_eq!(status, StatusCode::OK);
        let fixtured = body
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "fixtured")
            .unwrap()
            .clone();
        assert_eq!(fixtured["last_pass_at"], done["last_pass_at"]);
    }
}
