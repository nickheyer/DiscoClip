//! The Discord guilds an account belongs to and can manage, from its Discord grant.

use axum::Json;
use axum::extract::State;

use super::AppState;
use super::auth::Auth;
use super::error::ApiError;
use crate::discord::{Guild, fetch_guilds};
use crate::users::UserId;

/// Fetches `user`'s guilds with a fresh Discord token and stores them.
pub async fn sync_guilds(state: &AppState, user: UserId) -> Result<Vec<Guild>, ApiError> {
    let (_, tokens, _) = state.oauth.tokens(user, "discord").await?;
    let provider = state.oauth.provider("discord")?;
    let url = provider
        .discord_guilds_url(&state.oauth.http)
        .await?
        .ok_or_else(|| {
            ApiError::Internal("the discord provider is not of the Discord kind".into())
        })?;
    let guilds = fetch_guilds(&state.oauth.http, &url, &tokens.access_token).await?;
    Ok(state.guilds.replace(user, &guilds).await?)
}

/// The guilds as last fetched; manageable ones first.
pub async fn list_guilds(
    State(state): State<AppState>,
    Auth(identity): Auth,
) -> Result<Json<Vec<Guild>>, ApiError> {
    Ok(Json(state.guilds.list_for(identity.user.id).await?))
}

/// Fetches the guilds again.
pub async fn refresh_guilds(
    State(state): State<AppState>,
    Auth(identity): Auth,
) -> Result<Json<Vec<Guild>>, ApiError> {
    Ok(Json(sync_guilds(&state, identity.user.id).await?))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use axum::http::{Method, StatusCode};
    use serde_json::json;
    use url::Url;

    use crate::oauth::{Kind, Registry};
    use crate::settings::WebConfig;
    use crate::web::testing::{Client, FakeProvider, app_with};

    #[tokio::test]
    async fn guilds_are_stored_at_login_and_refreshed_on_request() {
        let fake = FakeProvider::start().await;
        let registry = Registry::new(vec![fake.provider("discord", Kind::Discord), fake.oidc()]);
        let app = app_with(WebConfig::default(), registry, true).await;
        fake.lock().guilds = json!([
            {"id": "100", "name": "Clips", "icon": "abc", "owner": true, "permissions": "0"},
            {"id": "200", "name": "Friends", "icon": null, "owner": false, "permissions": "32"},
            {"id": "300", "name": "Lurking", "owner": false, "permissions": "1024"}
        ]);
        fake.grant(
            "d",
            "1234",
            json!({"id": "1234", "username": "octo", "global_name": "Octo"}),
        );

        let mut browser = Client::new(&app);
        browser.get("/api/auth/discord/start").await;
        let location = Url::parse(&browser.location.clone().unwrap()).unwrap();
        let query: HashMap<String, String> = location.query_pairs().into_owned().collect();
        assert_eq!(query["scope"], "identify guilds guilds.members.read");
        browser
            .get(&format!(
                "/api/auth/discord/callback?code=d&state={}",
                query["state"]
            ))
            .await;
        assert_eq!(browser.location.as_deref(), Some("/"));
        browser.get("/api/session").await;

        let (status, body) = browser.get("/api/discord/guilds").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let guilds = body.as_array().unwrap();
        assert_eq!(guilds.len(), 3);
        assert_eq!(guilds[0]["name"], "Clips");
        assert_eq!(guilds[0]["manageable"], true);
        assert_eq!(guilds[0]["icon"], "abc");
        assert_eq!(guilds[1]["name"], "Friends");
        assert_eq!(guilds[1]["manageable"], true);
        assert_eq!(guilds[2]["name"], "Lurking");
        assert_eq!(guilds[2]["manageable"], false);

        fake.lock().guilds = json!([
            {"id": "100", "name": "Clips renamed", "owner": false, "permissions": "0"}
        ]);
        let (status, body) = browser
            .send(Method::POST, "/api/discord/guilds/refresh", None)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body.as_array().unwrap().len(), 1);
        assert_eq!(body[0]["name"], "Clips renamed");
        assert_eq!(body[0]["manageable"], false);
        assert_eq!(
            browser
                .get("/api/discord/guilds")
                .await
                .1
                .as_array()
                .unwrap()
                .len(),
            1
        );

        // Refreshing the identity refreshes the guilds too.
        fake.lock().guilds = json!([]);
        let (status, _) = browser
            .send(Method::POST, "/api/auth/identities/discord/refresh", None)
            .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            browser
                .get("/api/discord/guilds")
                .await
                .1
                .as_array()
                .unwrap()
                .is_empty()
        );

        // An account without a Discord identity has no guilds and nothing to refresh.
        fake.grant(
            "o",
            "sub-1",
            json!({"sub": "sub-1", "preferred_username": "other"}),
        );
        let mut other = Client::new(&app);
        other.get("/api/auth/oidc/start").await;
        let location = Url::parse(&other.location.clone().unwrap()).unwrap();
        let query: HashMap<String, String> = location.query_pairs().into_owned().collect();
        other
            .get(&format!(
                "/api/auth/oidc/callback?code=o&state={}",
                query["state"]
            ))
            .await;
        other.get("/api/session").await;
        let (status, body) = other.get("/api/discord/guilds").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!([]));
        let (status, _) = other
            .send(Method::POST, "/api/discord/guilds/refresh", None)
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
