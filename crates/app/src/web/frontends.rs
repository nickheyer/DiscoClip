//! Front ends as admins manage them: what each shows, who it lets in, its accounts and
//! its viewers' sessions. What the front ends serve to the public is in `front`.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use serde::Deserialize;
use uuid::Uuid;

use super::AppState;
use super::auth::{Auth, parse_id};
use super::error::ApiError;
use crate::frontends::{
    Frontend, FrontendError, FrontendId, FrontendInput, FrontendUser, Known, ViewerSession,
};
use crate::users::Permission;

impl From<FrontendError> for ApiError {
    fn from(error: FrontendError) -> Self {
        match error {
            FrontendError::NotFound(_) => ApiError::NotFound,
            FrontendError::Invalid(_)
            | FrontendError::UnknownProfile(_)
            | FrontendError::UnknownProvider(_)
            | FrontendError::Password(_) => ApiError::BadRequest(error.to_string()),
            FrontendError::DuplicateSlug(_)
            | FrontendError::DuplicateUser(..)
            | FrontendError::NoPublicUrl => ApiError::Conflict(error.to_string()),
            FrontendError::Store(_) => ApiError::Internal(error.to_string()),
        }
    }
}

/// What front ends are checked against as the server stands.
pub(super) fn known(state: &AppState) -> Known {
    Known {
        profiles: state.profiles.cache(),
        providers: state.oauth.providers().into_iter().map(|p| p.id).collect(),
        public_url: state
            .public_url
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .is_some(),
    }
}

pub async fn list(
    State(state): State<AppState>,
    Auth(identity): Auth,
) -> Result<Json<Vec<Frontend>>, ApiError> {
    identity.require(Permission::ManageSettings)?;
    Ok(Json(state.frontends.list().await?))
}

pub async fn get(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<Json<Frontend>, ApiError> {
    identity.require(Permission::ManageSettings)?;
    let id: FrontendId = parse_id(&id)?;
    Ok(Json(
        state.frontends.get(id).await?.ok_or(ApiError::NotFound)?,
    ))
}

pub async fn create(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Json(input): Json<FrontendInput>,
) -> Result<(StatusCode, Json<Frontend>), ApiError> {
    identity.require(Permission::ManageSettings)?;
    let frontend = state
        .frontends
        .create(&identity.actor(), input, &known(&state))
        .await?;
    tracing::info!(by = identity.user.username, frontend = %frontend.id, slug = frontend.input.slug, "front end created");
    Ok((StatusCode::CREATED, Json(frontend)))
}

pub async fn update(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
    Json(input): Json<FrontendInput>,
) -> Result<Json<Frontend>, ApiError> {
    identity.require(Permission::ManageSettings)?;
    let id: FrontendId = parse_id(&id)?;
    let frontend = state
        .frontends
        .update(&identity.actor(), id, input, &known(&state))
        .await?;
    tracing::info!(by = identity.user.username, frontend = %frontend.id, slug = frontend.input.slug, "front end updated");
    Ok(Json(frontend))
}

pub async fn delete(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    identity.require(Permission::ManageSettings)?;
    let id: FrontendId = parse_id(&id)?;
    state.frontends.delete(&identity.actor(), id).await?;
    tracing::info!(by = identity.user.username, frontend = %id, "front end deleted");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct SecretBody {
    /// The new shared secret; `null` removes it.
    pub secret: Option<String>,
}

pub async fn set_secret(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
    Json(body): Json<SecretBody>,
) -> Result<Json<Frontend>, ApiError> {
    identity.require(Permission::ManageSettings)?;
    let id: FrontendId = parse_id(&id)?;
    let frontend = state
        .frontends
        .set_secret(&identity.actor(), id, body.secret.as_deref())
        .await?;
    tracing::info!(by = identity.user.username, frontend = %id, has_secret = frontend.has_secret, "front end secret changed");
    Ok(Json(frontend))
}

pub async fn users(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<Json<Vec<FrontendUser>>, ApiError> {
    identity.require(Permission::ManageSettings)?;
    let id: FrontendId = parse_id(&id)?;
    state.frontends.get(id).await?.ok_or(ApiError::NotFound)?;
    Ok(Json(state.frontends.users(id).await?))
}

#[derive(Debug, Deserialize)]
pub struct NewUser {
    pub username: String,
    pub password: String,
}

pub async fn create_user(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
    Json(body): Json<NewUser>,
) -> Result<(StatusCode, Json<FrontendUser>), ApiError> {
    identity.require(Permission::ManageSettings)?;
    let id: FrontendId = parse_id(&id)?;
    let user = state
        .frontends
        .create_user(&identity.actor(), id, &body.username, &body.password)
        .await?;
    tracing::info!(by = identity.user.username, frontend = %id, username = user.username, "front end account created");
    Ok((StatusCode::CREATED, Json(user)))
}

#[derive(Debug, Deserialize)]
pub struct PasswordBody {
    pub password: String,
}

pub async fn set_user_password(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path((id, user)): Path<(String, String)>,
    Json(body): Json<PasswordBody>,
) -> Result<Json<FrontendUser>, ApiError> {
    identity.require(Permission::ManageSettings)?;
    let id: FrontendId = parse_id(&id)?;
    let user: Uuid = parse_id(&user)?;
    let user = state
        .frontends
        .set_user_password(&identity.actor(), id, user, &body.password)
        .await?;
    Ok(Json(user))
}

pub async fn delete_user(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path((id, user)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    identity.require(Permission::ManageSettings)?;
    let id: FrontendId = parse_id(&id)?;
    let user: Uuid = parse_id(&user)?;
    state
        .frontends
        .delete_user(&identity.actor(), id, user)
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn sessions(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<Json<Vec<ViewerSession>>, ApiError> {
    identity.require(Permission::ManageSettings)?;
    let id: FrontendId = parse_id(&id)?;
    state.frontends.get(id).await?.ok_or(ApiError::NotFound)?;
    Ok(Json(state.frontends.sessions(id).await?))
}

pub async fn revoke_sessions(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    identity.require(Permission::ManageSettings)?;
    let id: FrontendId = parse_id(&id)?;
    let removed = state
        .frontends
        .revoke_sessions(&identity.actor(), id, None)
        .await?;
    tracing::info!(by = identity.user.username, frontend = %id, removed, "front end sessions ended");
    Ok(StatusCode::NO_CONTENT)
}

pub async fn revoke_session(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path((id, session)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    identity.require(Permission::ManageSettings)?;
    let id: FrontendId = parse_id(&id)?;
    let session: Uuid = parse_id(&session)?;
    state
        .frontends
        .revoke_sessions(&identity.actor(), id, Some(session))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}
