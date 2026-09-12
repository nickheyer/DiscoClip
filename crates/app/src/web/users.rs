//! Accounts and roles, administered by admins; passwords, changed by their owners too.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum_extra::extract::cookie::CookieJar;
use serde::{Deserialize, Serialize};

use super::AppState;
use super::auth::{Auth, Revoked, SessionView, cleared, parse_id};
use super::error::ApiError;
use crate::sessions::SessionId;
use crate::users::{Permission, Role, User, UserId};

pub async fn list(
    State(state): State<AppState>,
    Auth(identity): Auth,
) -> Result<Json<Vec<User>>, ApiError> {
    identity.require(Permission::ManageUsers)?;
    Ok(Json(state.users.list().await?))
}

/// A role with what it allows and who holds it.
#[derive(Debug, Serialize)]
pub struct RoleView {
    pub role: Role,
    pub description: &'static str,
    pub permissions: Vec<Permission>,
    /// The accounts holding the role, oldest first.
    pub accounts: Vec<User>,
}

/// Every role with its permissions and its accounts.
pub async fn roles(
    State(state): State<AppState>,
    Auth(identity): Auth,
) -> Result<Json<Vec<RoleView>>, ApiError> {
    identity.require(Permission::ManageUsers)?;
    let users = state.users.list().await?;
    Ok(Json(
        Role::ALL
            .into_iter()
            .map(|role| RoleView {
                role,
                description: role.description(),
                permissions: role.permissions(),
                accounts: users.iter().filter(|u| u.role == role).cloned().collect(),
            })
            .collect(),
    ))
}

/// A session with the account it belongs to.
#[derive(Debug, Serialize)]
pub struct AccountSessionView {
    pub user_id: UserId,
    pub username: String,
    #[serde(flatten)]
    pub session: SessionView,
}

/// Every account's live sessions, newest first, for admins.
pub async fn list_all_sessions(
    State(state): State<AppState>,
    Auth(identity): Auth,
) -> Result<Json<Vec<AccountSessionView>>, ApiError> {
    identity.require(Permission::ManageUsers)?;
    let users = state.users.list().await?;
    let current = identity
        .session_id()
        .unwrap_or(SessionId(uuid::Uuid::nil()));
    let sessions = state.sessions.list_all().await?;
    Ok(Json(
        sessions
            .iter()
            .filter_map(|session| {
                let user = users.iter().find(|u| u.id == session.user_id)?;
                Some(AccountSessionView {
                    user_id: user.id,
                    username: user.username.clone(),
                    session: SessionView::of(session, current),
                })
            })
            .collect(),
    ))
}

/// Ends one session of an account, for admins; the cookie is cleared when it is their own.
pub async fn revoke_session(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path((id, session)): Path<(String, String)>,
    jar: CookieJar,
) -> Result<(CookieJar, StatusCode), ApiError> {
    identity.require(Permission::ManageUsers)?;
    let id: UserId = parse_id(&id)?;
    let session_id: SessionId = parse_id(&session)?;
    let found = state
        .sessions
        .get(session_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if found.user_id != id {
        return Err(ApiError::NotFound);
    }
    state.sessions.revoke(session_id).await?;
    tracing::info!(by = identity.user.username, %id, session = %session_id, "session ended");
    let jar = if identity.session_id() == Some(session_id) {
        cleared(jar)
    } else {
        jar
    };
    Ok((jar, StatusCode::NO_CONTENT))
}

#[derive(Debug, Deserialize)]
pub struct CreateRequest {
    pub username: String,
    /// Without one, the account can only log in another way.
    pub password: Option<String>,
    pub role: Role,
}

pub async fn create(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Json(request): Json<CreateRequest>,
) -> Result<(StatusCode, Json<User>), ApiError> {
    identity.require(Permission::ManageUsers)?;
    let user = state
        .users
        .create(&request.username, request.password.as_deref(), request.role)
        .await?;
    tracing::info!(by = identity.user.username, username = user.username, role = %user.role, "account created");
    Ok((StatusCode::CREATED, Json(user)))
}

/// Any account for admins; one's own for everyone.
pub async fn get(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<Json<User>, ApiError> {
    let id: UserId = parse_id(&id)?;
    if id != identity.user.id {
        identity.require(Permission::ManageUsers)?;
    }
    Ok(Json(state.users.get(id).await?.ok_or(ApiError::NotFound)?))
}

#[derive(Debug, Deserialize)]
pub struct UpdateRequest {
    pub role: Role,
}

pub async fn update(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
    Json(request): Json<UpdateRequest>,
) -> Result<Json<User>, ApiError> {
    identity.require(Permission::ManageUsers)?;
    let id: UserId = parse_id(&id)?;
    let user = state.users.set_role(id, request.role).await?;
    tracing::info!(by = identity.user.username, username = user.username, role = %user.role, "role changed");
    Ok(Json(user))
}

/// Removes an account and ends its sessions. Admins delete others, never themselves.
pub async fn delete(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    identity.require(Permission::ManageUsers)?;
    let id: UserId = parse_id(&id)?;
    if id == identity.user.id {
        return Err(ApiError::Conflict(
            "log in as another admin to delete this account".into(),
        ));
    }
    let grants = state.oauth.store.all_tokens_for(id).await?;
    state.users.delete(id).await?;
    state.oauth.revoke_grants(&grants).await;
    tracing::info!(by = identity.user.username, %id, "account deleted");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct PasswordRequest {
    pub password: String,
    /// Required when changing one's own password.
    #[serde(default)]
    pub current_password: Option<String>,
}

/// Sets a password. An account changing its own must give the current one, and keeps only
/// the session asking; an admin resetting another's ends all of that account's sessions.
pub async fn set_password(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
    jar: CookieJar,
    Json(request): Json<PasswordRequest>,
) -> Result<(CookieJar, StatusCode), ApiError> {
    let id: UserId = parse_id(&id)?;
    if id == identity.user.id {
        let key = identity.user.username.to_ascii_lowercase();
        state
            .limits
            .login_user
            .check(&key)
            .map_err(ApiError::TooManyRequests)?;
        let current = request
            .current_password
            .as_deref()
            .ok_or_else(|| ApiError::BadRequest("current_password is required".into()))?;
        if state
            .users
            .verify_login(&identity.user.username, current)
            .await?
            .is_none()
        {
            state.limits.login_user.strike(&key);
            return Err(ApiError::Forbidden("the current password is wrong".into()));
        }
        state.limits.login_user.clear(&key);
        state.users.set_password(id, &request.password).await?;
        state
            .sessions
            .revoke_all_for(id, identity.session_id())
            .await?;
        tracing::info!(username = identity.user.username, "password changed");
        return Ok((jar, StatusCode::NO_CONTENT));
    }
    identity.require(Permission::ManageUsers)?;
    let user = state.users.set_password(id, &request.password).await?;
    state.sessions.revoke_all_for(id, None).await?;
    tracing::info!(
        by = identity.user.username,
        username = user.username,
        "password reset"
    );
    Ok((jar, StatusCode::NO_CONTENT))
}

/// An account's live sessions, for admins.
pub async fn list_sessions(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<Json<Vec<SessionView>>, ApiError> {
    identity.require(Permission::ManageUsers)?;
    let id: UserId = parse_id(&id)?;
    state.users.get(id).await?.ok_or(ApiError::NotFound)?;
    let sessions = state.sessions.list_for(id).await?;
    let current = identity
        .session_id()
        .unwrap_or(SessionId(uuid::Uuid::nil()));
    Ok(Json(
        sessions
            .iter()
            .map(|session| SessionView::of(session, current))
            .collect(),
    ))
}

/// Ends every session of an account, for admins; their own included when it is theirs.
pub async fn revoke_sessions(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
    jar: CookieJar,
) -> Result<(CookieJar, Json<Revoked>), ApiError> {
    identity.require(Permission::ManageUsers)?;
    let id: UserId = parse_id(&id)?;
    state.users.get(id).await?.ok_or(ApiError::NotFound)?;
    let revoked = state.sessions.revoke_all_for(id, None).await?;
    tracing::info!(by = identity.user.username, %id, revoked, "sessions ended");
    let jar = if id == identity.user.id {
        cleared(jar)
    } else {
        jar
    };
    Ok((jar, Json(Revoked { revoked })))
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use serde_json::{Value as Json, json};

    use crate::web::testing::{Client, app_with_admin};

    #[tokio::test]
    async fn admins_manage_accounts_and_others_do_not() {
        let app = app_with_admin().await;
        let mut admin = Client::new(&app);
        admin.login("nick", "correct horse").await;

        let (status, body) = admin
            .post(
                "/api/users",
                json!({"username": "viewer", "password": "battery staple", "role": "viewer"}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["role"], "viewer");
        let viewer_id = body["id"].as_str().unwrap().to_string();
        let (status, body) = admin
            .post(
                "/api/users",
                json!({"username": "bot-only", "role": "operator"}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["has_password"], false);
        let (status, _) = admin
            .post(
                "/api/users",
                json!({"username": "viewer", "role": "viewer"}),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _) = admin
            .post(
                "/api/users",
                json!({"username": "bad name", "role": "viewer"}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = admin
            .post("/api/users", json!({"username": "x", "role": "root"}))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        let (status, body) = admin.get("/api/users").await;
        assert_eq!(status, StatusCode::OK);
        let names: Vec<&str> = body
            .as_array()
            .unwrap()
            .iter()
            .map(|u| u["username"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["nick", "viewer", "bot-only"]);

        let mut viewer = Client::new(&app);
        viewer.login("viewer", "battery staple").await;
        let (status, body) = viewer.get("/api/users").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body["error"],
            "the viewer role does not allow managing accounts"
        );
        let (status, _) = viewer
            .post("/api/users", json!({"username": "x", "role": "admin"}))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, body) = viewer.get(&format!("/api/users/{viewer_id}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["username"], "viewer");
        let admin_id = admin.get("/api/session").await.1["user"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let (status, _) = viewer.get(&format!("/api/users/{admin_id}")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = viewer
            .send(
                Method::PATCH,
                &format!("/api/users/{viewer_id}"),
                Some(json!({"role": "admin"})),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (status, body) = admin
            .send(
                Method::PATCH,
                &format!("/api/users/{viewer_id}"),
                Some(json!({"role": "operator"})),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["role"], "operator");
        let (status, _) = admin
            .send(
                Method::PATCH,
                &format!("/api/users/{admin_id}"),
                Some(json!({"role": "viewer"})),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _) = admin.delete(&format!("/api/users/{admin_id}")).await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _) = admin.get("/api/users/not-an-id").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = admin
            .get("/api/users/00000000-0000-0000-0000-000000000000")
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, _) = admin.delete(&format!("/api/users/{viewer_id}")).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = viewer.get("/api/session").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = admin.delete(&format!("/api/users/{viewer_id}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn passwords_change_by_owner_with_the_current_one_or_by_admin() {
        let app = app_with_admin().await;
        let mut admin = Client::new(&app);
        admin.login("nick", "correct horse").await;
        let (_, body) = admin
            .post(
                "/api/users",
                json!({"username": "viewer", "password": "battery staple", "role": "viewer"}),
            )
            .await;
        let viewer_id = body["id"].as_str().unwrap().to_string();
        let mut viewer = Client::new(&app);
        viewer.login("viewer", "battery staple").await;
        let mut viewer_phone = Client::new(&app);
        viewer_phone.login("viewer", "battery staple").await;

        let path = format!("/api/users/{viewer_id}/password");
        let (status, body) = viewer
            .send(
                Method::PUT,
                &path,
                Some(json!({"password": "new password"})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let (status, body) = viewer
            .send(
                Method::PUT,
                &path,
                Some(json!({"password": "new password", "current_password": "wrong"})),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        let (status, body) = viewer
            .send(
                Method::PUT,
                &path,
                Some(json!({"password": "short", "current_password": "battery staple"})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let (status, _) = viewer
            .send(
                Method::PUT,
                &path,
                Some(json!({"password": "new password", "current_password": "battery staple"})),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = viewer.get("/api/session").await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = viewer_phone.get("/api/session").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = viewer_phone.login("viewer", "new password").await;
        assert_eq!(status, StatusCode::OK);

        let admin_id = admin.get("/api/session").await.1["user"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let (status, _) = viewer
            .send(
                Method::PUT,
                &format!("/api/users/{admin_id}/password"),
                Some(json!({"password": "hijacked password"})),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (status, _) = admin
            .send(
                Method::PUT,
                &path,
                Some(json!({"password": "reset by admin"})),
            )
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = viewer.get("/api/session").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = viewer_phone.get("/api/session").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = viewer.login("viewer", "reset by admin").await;
        assert_eq!(status, StatusCode::OK);

        // Wrong current passwords count like wrong logins.
        for _ in 0..5 {
            viewer
                .send(
                    Method::PUT,
                    &path,
                    Some(json!({"password": "new password", "current_password": "wrong"})),
                )
                .await;
        }
        let (status, _) = viewer
            .send(
                Method::PUT,
                &path,
                Some(json!({"password": "new password", "current_password": "reset by admin"})),
            )
            .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

        let (status, _) = viewer_phone.login("viewer", "reset by admin").await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn roles_are_listed_with_their_permissions_and_accounts() {
        let app = app_with_admin().await;
        let mut admin = Client::new(&app);
        admin.login("nick", "correct horse").await;
        admin
            .post(
                "/api/users",
                json!({"username": "viewer", "password": "battery staple", "role": "viewer"}),
            )
            .await;
        let (status, body) = admin.get("/api/roles").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let roles = body.as_array().unwrap();
        assert_eq!(roles.len(), 3);
        assert_eq!(roles[0]["role"], "admin");
        assert_eq!(roles[0]["permissions"].as_array().unwrap().len(), 8);
        assert_eq!(roles[0]["accounts"][0]["username"], "nick");
        assert_eq!(roles[1]["role"], "operator");
        assert_eq!(
            roles[1]["permissions"],
            json!(["manage_watch_rules", "manage_bots", "manage_jobs"])
        );
        assert_eq!(roles[1]["accounts"], json!([]));
        assert_eq!(roles[2]["role"], "viewer");
        assert_eq!(roles[2]["permissions"], json!([]));
        assert_eq!(roles[2]["accounts"][0]["username"], "viewer");
        assert!(
            roles[2]["description"]
                .as_str()
                .unwrap()
                .contains("Read only")
        );
        let mut viewer = Client::new(&app);
        viewer.login("viewer", "battery staple").await;
        assert_eq!(viewer.get("/api/roles").await.0, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn admins_list_every_session_and_end_one_of_anyones() {
        let app = app_with_admin().await;
        let mut admin = Client::new(&app);
        admin.login("nick", "correct horse").await;
        let (_, body) = admin
            .post(
                "/api/users",
                json!({"username": "viewer", "password": "battery staple", "role": "viewer"}),
            )
            .await;
        let viewer_id = body["id"].as_str().unwrap().to_string();
        let mut viewer = Client::new(&app);
        viewer.login("viewer", "battery staple").await;
        let mut viewer_phone = Client::new(&app);
        viewer_phone.headers = vec![("user-agent".into(), "Phone/1.0 (Android)".into())];
        viewer_phone.login("viewer", "battery staple").await;

        let (status, body) = admin.get("/api/sessions/all").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let sessions = body.as_array().unwrap();
        assert_eq!(sessions.len(), 3);
        assert_eq!(sessions[0]["username"], "viewer");
        assert_eq!(sessions[0]["user_agent"], "Phone/1.0 (Android)");
        assert_eq!(sessions[0]["current"], false);
        assert!(
            sessions
                .iter()
                .any(|s| s["username"] == "nick" && s["current"] == true)
        );
        let phone_id = sessions[0]["id"].as_str().unwrap().to_string();
        let own_id = sessions.iter().find(|s| s["current"] == true).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(
            viewer.get("/api/sessions/all").await.0,
            StatusCode::FORBIDDEN
        );

        // One session of another account; the wrong account or id is not found.
        let (status, _) = admin
            .delete(&format!("/api/users/{viewer_id}/sessions/{own_id}"))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = admin
            .delete(&format!("/api/users/{viewer_id}/sessions/not-a-session"))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = viewer
            .delete(&format!("/api/users/{viewer_id}/sessions/{phone_id}"))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = admin
            .delete(&format!("/api/users/{viewer_id}/sessions/{phone_id}"))
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert_eq!(
            viewer_phone.get("/api/session").await.0,
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(viewer.get("/api/session").await.0, StatusCode::OK);
        assert_eq!(
            admin
                .get("/api/sessions/all")
                .await
                .1
                .as_array()
                .unwrap()
                .len(),
            2
        );

        // Admins ending their own current session are logged out.
        let admin_id = admin.get("/api/session").await.1["user"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let (status, _) = admin
            .delete(&format!("/api/users/{admin_id}/sessions/{own_id}"))
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(admin.cookie.is_none());
        assert_eq!(admin.get("/api/session").await.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn admins_see_and_end_anyones_sessions() {
        let app = app_with_admin().await;
        let mut admin = Client::new(&app);
        admin.login("nick", "correct horse").await;
        let (_, body) = admin
            .post(
                "/api/users",
                json!({"username": "viewer", "password": "battery staple", "role": "viewer"}),
            )
            .await;
        let viewer_id = body["id"].as_str().unwrap().to_string();
        let mut viewer = Client::new(&app);
        viewer.login("viewer", "battery staple").await;
        let mut viewer_phone = Client::new(&app);
        viewer_phone.login("viewer", "battery staple").await;

        let (status, _) = viewer
            .get(&format!("/api/users/{viewer_id}/sessions"))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, body) = admin.get(&format!("/api/users/{viewer_id}/sessions")).await;
        assert_eq!(status, StatusCode::OK);
        let sessions = body.as_array().unwrap();
        assert_eq!(sessions.len(), 2);
        assert!(sessions.iter().all(|s| s["current"] == false));
        let (status, _) = admin
            .get("/api/users/00000000-0000-0000-0000-000000000000/sessions")
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (status, body) = admin
            .delete(&format!("/api/users/{viewer_id}/sessions"))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"revoked": 2}));
        let (status, _) = viewer.get("/api/session").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = viewer_phone.get("/api/session").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let admin_id = admin.get("/api/session").await.1["user"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let (status, body) = admin
            .delete(&format!("/api/users/{admin_id}/sessions"))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"revoked": 1}));
        assert!(admin.cookie.is_none());
        let (status, body) = admin.get("/api/session").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert_eq!(
            body,
            Json::Null
                .as_object()
                .map_or(body.clone(), |_| body.clone())
        );
    }
}
