//! API tokens, minted and revoked from a browser session by their owner, and revoked by
//! admins for anyone.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};

use super::AppState;
use super::auth::{Auth, parse_id};
use super::error::ApiError;
use crate::tokens::{ApiToken, TokenId};
use crate::users::{Permission, UserId};

pub async fn list(
    State(state): State<AppState>,
    Auth(identity): Auth,
) -> Result<Json<Vec<ApiToken>>, ApiError> {
    identity.session()?;
    Ok(Json(state.tokens.list_for(identity.user.id).await?))
}

#[derive(Debug, Deserialize)]
pub struct CreateRequest {
    pub name: String,
    /// What the token may do; each must be something the account's role allows.
    #[serde(default)]
    pub scopes: Vec<Permission>,
    /// Days until the token stops working; forever when absent.
    pub expires_in_days: Option<u32>,
}

/// The token as stored, and the secret to use as a bearer, shown this once.
#[derive(Debug, Serialize)]
pub struct Minted {
    pub token: ApiToken,
    pub secret: String,
}

pub async fn create(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Json(request): Json<CreateRequest>,
) -> Result<(StatusCode, Json<Minted>), ApiError> {
    identity.session()?;
    for scope in &request.scopes {
        if !identity.user.role.allows(*scope) {
            return Err(ApiError::BadRequest(format!(
                "the {} role does not allow {scope}, so a token cannot be given it",
                identity.user.role
            )));
        }
    }
    let expires_at = request
        .expires_in_days
        .map(|days| Timestamp::now() + SignedDuration::from_hours(24 * i64::from(days)));
    let (token, secret) = state
        .tokens
        .create(identity.user.id, &request.name, request.scopes, expires_at)
        .await?;
    tracing::info!(
        username = identity.user.username,
        name = token.name,
        "API token minted"
    );
    Ok((StatusCode::CREATED, Json(Minted { token, secret })))
}

/// Revokes one of the account's own tokens.
pub async fn revoke(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    identity.session()?;
    let id: TokenId = parse_id(&id)?;
    let token = state.tokens.get(id).await?.ok_or(ApiError::NotFound)?;
    if token.user_id != identity.user.id {
        return Err(ApiError::NotFound);
    }
    state.tokens.revoke(id).await?;
    tracing::info!(
        username = identity.user.username,
        name = token.name,
        "API token revoked"
    );
    Ok(StatusCode::NO_CONTENT)
}

/// A token with the account it belongs to.
#[derive(Debug, Serialize)]
pub struct AccountTokenView {
    pub username: String,
    #[serde(flatten)]
    pub token: ApiToken,
}

/// Every account's live tokens, newest first, for admins.
pub async fn list_all(
    State(state): State<AppState>,
    Auth(identity): Auth,
) -> Result<Json<Vec<AccountTokenView>>, ApiError> {
    identity.require(Permission::ManageUsers)?;
    let users = state.users.list().await?;
    Ok(Json(
        state
            .tokens
            .list_all()
            .await?
            .into_iter()
            .filter_map(|token| {
                let user = users.iter().find(|u| u.id == token.user_id)?;
                Some(AccountTokenView {
                    username: user.username.clone(),
                    token,
                })
            })
            .collect(),
    ))
}

/// An account's tokens, for admins.
pub async fn list_for_user(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<Json<Vec<ApiToken>>, ApiError> {
    identity.require(Permission::ManageUsers)?;
    let id: UserId = parse_id(&id)?;
    state.users.get(id).await?.ok_or(ApiError::NotFound)?;
    Ok(Json(state.tokens.list_for(id).await?))
}

/// Revokes any account's token, for admins.
pub async fn revoke_for_user(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path((id, token)): Path<(String, String)>,
) -> Result<StatusCode, ApiError> {
    identity.require(Permission::ManageUsers)?;
    let id: UserId = parse_id(&id)?;
    let token_id: TokenId = parse_id(&token)?;
    let token = state
        .tokens
        .get(token_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if token.user_id != id {
        return Err(ApiError::NotFound);
    }
    state.tokens.revoke(token_id).await?;
    tracing::info!(by = identity.user.username, %id, name = token.name, "API token revoked");
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use serde_json::{Value as Json, json};

    use crate::web::testing::{Client, app_with_admin};

    #[tokio::test]
    async fn tokens_act_for_their_account_within_their_scopes() {
        let app = app_with_admin().await;
        let mut browser = Client::new(&app);
        browser.login("nick", "correct horse").await;

        let (status, body) = browser
            .post(
                "/api/tokens",
                json!({"name": "ci", "scopes": ["manage_users"]}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let secret = body["secret"].as_str().unwrap().to_string();
        assert!(secret.starts_with("dc_"));
        assert_eq!(body["token"]["name"], "ci");
        assert_eq!(body["token"]["scopes"], json!(["manage_users"]));
        assert_eq!(body["token"]["prefix"], &secret[..11]);
        assert!(body["token"]["expires_at"].is_null());
        let (status, body) = browser
            .post(
                "/api/tokens",
                json!({"name": "read only", "expires_in_days": 7}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let narrow = body["secret"].as_str().unwrap().to_string();
        assert!(body["token"]["expires_at"].is_string());

        // A script with the wide token: no cookie, no CSRF, no origin needed.
        let mut script = Client::new(&app);
        script.bearer = Some(secret.clone());
        script.origin = Some("http://elsewhere.example".into());
        let (status, body) = script.get("/api/session").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["user"]["username"], "nick");
        assert!(body["csrf_token"].is_null());
        assert!(body["session"].is_null());
        assert_eq!(body["token"]["name"], "ci");
        assert!(body["token"]["last_used_at"].is_string());
        script.origin = None;
        let (status, body) = script
            .post(
                "/api/users",
                json!({"username": "made-by-token", "role": "viewer"}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");

        // The narrow token reads but does not manage.
        let mut narrow_script = Client::new(&app);
        narrow_script.bearer = Some(narrow);
        let (status, _) = narrow_script.get("/api/session").await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = narrow_script
            .post("/api/users", json!({"username": "x", "role": "viewer"}))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body["error"],
            "this API token was not given managing accounts"
        );

        // Tokens do not manage sessions or tokens.
        let (status, body) = script.get("/api/tokens").await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        let (status, _) = script
            .post(
                "/api/tokens",
                json!({"name": "more", "scopes": ["manage_users"]}),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = script.get("/api/sessions").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = script.post("/api/logout", Json::Null).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // Listing and revoking.
        let (status, body) = browser.get("/api/tokens").await;
        assert_eq!(status, StatusCode::OK);
        let listed = body.as_array().unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|t| t.get("secret").is_none()));
        let wide_id = listed.iter().find(|t| t["name"] == "ci").unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let (status, _) = browser.delete(&format!("/api/tokens/{wide_id}")).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = browser.delete(&format!("/api/tokens/{wide_id}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, body) = script.get("/api/session").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        assert_eq!(
            browser.get("/api/tokens").await.1.as_array().unwrap().len(),
            1
        );
    }

    #[tokio::test]
    async fn scopes_stay_within_the_role_and_admins_revoke_anyones() {
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

        let (status, body) = viewer
            .post(
                "/api/tokens",
                json!({"name": "too much", "scopes": ["manage_users"]}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let (status, _) = viewer
            .post("/api/tokens", json!({"name": "", "scopes": []}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = viewer
            .post(
                "/api/tokens",
                json!({"name": "bogus scope", "scopes": ["fly"]}),
            )
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        let (status, body) = viewer.post("/api/tokens", json!({"name": "mine"})).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let token_id = body["token"]["id"].as_str().unwrap().to_string();
        let secret = body["secret"].as_str().unwrap().to_string();

        // Another account's token is invisible to a non-admin, visible to admins.
        let (status, _) = admin.delete(&format!("/api/tokens/{token_id}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, body) = admin.get("/api/tokens/all").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let all = body.as_array().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0]["username"], "viewer");
        assert_eq!(all[0]["name"], "mine");
        assert!(all[0].get("secret").is_none());
        assert_eq!(viewer.get("/api/tokens/all").await.0, StatusCode::FORBIDDEN);
        let (status, _) = viewer.get(&format!("/api/users/{viewer_id}/tokens")).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, body) = admin.get(&format!("/api/users/{viewer_id}/tokens")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body[0]["name"], "mine");
        let (status, _) = admin
            .delete(&format!("/api/users/{viewer_id}/tokens/{token_id}"))
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let mut script = Client::new(&app);
        script.bearer = Some(secret);
        let (status, _) = script.get("/api/session").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        // A deleted account takes its tokens with it.
        let (_, body) = viewer.post("/api/tokens", json!({"name": "again"})).await;
        script.bearer = Some(body["secret"].as_str().unwrap().to_string());
        assert_eq!(script.get("/api/session").await.0, StatusCode::OK);
        admin.delete(&format!("/api/users/{viewer_id}")).await;
        assert_eq!(script.get("/api/session").await.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn unknown_bearer_tokens_are_refused_and_rate_limited() {
        let app = app_with_admin().await;
        let mut script = Client::new(&app);
        script.bearer = Some("dc_not-a-token".into());
        for _ in 0..20 {
            let (status, body) = script.get("/api/setup").await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
        }
        let (status, _) = script.get("/api/setup").await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        // The address is locked for passwords too.
        script.bearer = None;
        let (status, _) = script
            .send(
                Method::POST,
                "/api/login",
                Some(json!({"username": "nick", "password": "correct horse"})),
            )
            .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    }
}
