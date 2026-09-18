//! Profiles: which platforms are on where, edited by admins, and put in force per guild,
//! channel or user by operators and by whoever manages the guild on Discord.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use super::AppState;
use super::auth::{Auth, Identity, parse_id};
use super::error::ApiError;
use super::rules::may_edit;
use crate::profiles::{
    Assignment, EffectiveProfile, Preset, Profile, ProfileError, ProfileId, ProfileInput, Scope,
};
use crate::users::Permission;

impl From<ProfileError> for ApiError {
    fn from(error: ProfileError) -> Self {
        match error {
            ProfileError::NotFound(_) => ApiError::NotFound,
            ProfileError::Invalid(_)
            | ProfileError::UnknownPlatform(_)
            | ProfileError::UnknownPreset(_) => ApiError::BadRequest(error.to_string()),
            ProfileError::Duplicate(_)
            | ProfileError::Builtin(_)
            | ProfileError::InUse(_)
            | ProfileError::GlobalRequired => ApiError::Conflict(error.to_string()),
            ProfileError::Store(_) => ApiError::Internal(error.to_string()),
        }
    }
}

/// 403 unless the account may put profiles in force at `scope`: the whole server needs
/// the settings permission; a guild's scopes need what editing its rules needs.
async fn may_assign(state: &AppState, identity: &Identity, scope: &Scope) -> Result<(), ApiError> {
    match scope.guild_id() {
        None => identity.require(Permission::ManageSettings),
        Some(guild) => may_edit(state, identity, guild).await,
    }
}

/// The presets a profile can choose, each with the platforms in it.
pub async fn presets(State(state): State<AppState>, Auth(_): Auth) -> Json<Vec<Preset>> {
    Json(state.profiles.cache().presets().list().to_vec())
}

/// Every profile, for anyone logged in: guild managers pick from them.
pub async fn list(
    State(state): State<AppState>,
    Auth(_): Auth,
) -> Result<Json<Vec<Profile>>, ApiError> {
    Ok(Json(state.profiles.list().await?))
}

pub async fn get(
    State(state): State<AppState>,
    Auth(_): Auth,
    Path(id): Path<String>,
) -> Result<Json<Profile>, ApiError> {
    let id: ProfileId = parse_id(&id)?;
    Ok(Json(
        state.profiles.get(id).await?.ok_or(ApiError::NotFound)?,
    ))
}

pub async fn create(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Json(input): Json<ProfileInput>,
) -> Result<(StatusCode, Json<Profile>), ApiError> {
    identity.require(Permission::ManageSettings)?;
    let profile = state.profiles.create(&identity.actor(), input).await?;
    tracing::info!(by = identity.user.username, profile = %profile.id, name = profile.input.name, "profile created");
    Ok((StatusCode::CREATED, Json(profile)))
}

pub async fn update(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
    Json(input): Json<ProfileInput>,
) -> Result<Json<Profile>, ApiError> {
    identity.require(Permission::ManageSettings)?;
    let id: ProfileId = parse_id(&id)?;
    let profile = state.profiles.update(&identity.actor(), id, input).await?;
    tracing::info!(by = identity.user.username, profile = %profile.id, name = profile.input.name, "profile updated");
    Ok(Json(profile))
}

pub async fn delete(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    identity.require(Permission::ManageSettings)?;
    let id: ProfileId = parse_id(&id)?;
    state.profiles.delete(&identity.actor(), id).await?;
    tracing::info!(by = identity.user.username, profile = %id, "profile deleted");
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct AssignmentsQuery {
    /// The guild whose scopes to list, beside the whole server's; every guild's without.
    #[serde(default)]
    pub guild: Option<String>,
}

/// The profiles in force: the whole server's and, for a guild, its guild, channel and
/// user scopes. Listing every guild's needs the rules permission.
pub async fn assignments(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Query(query): Query<AssignmentsQuery>,
) -> Result<Json<Vec<Assignment>>, ApiError> {
    match &query.guild {
        Some(guild) => may_edit(&state, &identity, guild).await?,
        None => identity.require(Permission::ManageWatchRules)?,
    }
    Ok(Json(
        state.profiles.assignments(query.guild.as_deref()).await?,
    ))
}

#[derive(Debug, Deserialize)]
pub struct AssignBody {
    pub profile_id: ProfileId,
}

fn parse_scope(key: &str) -> Result<Scope, ApiError> {
    key.parse().map_err(ApiError::BadRequest)
}

/// Puts a profile in force at a scope, named as `global`, `guild:<id>`,
/// `channel:<guild>:<id>` or `user:<guild>:<id>`.
pub async fn assign(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(scope): Path<String>,
    Json(body): Json<AssignBody>,
) -> Result<Json<Assignment>, ApiError> {
    let scope = parse_scope(&scope)?;
    may_assign(&state, &identity, &scope).await?;
    let assignment = state
        .profiles
        .assign(&identity.actor(), scope, body.profile_id)
        .await?;
    tracing::info!(by = identity.user.username, scope = assignment.scope.key(), profile = %assignment.profile_id, "profile assigned");
    Ok(Json(assignment))
}

/// Takes the profile off a scope, so the wider scope's applies there again.
pub async fn unassign(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(scope): Path<String>,
) -> Result<StatusCode, ApiError> {
    let scope = parse_scope(&scope)?;
    may_assign(&state, &identity, &scope).await?;
    state
        .profiles
        .unassign(&identity.actor(), scope.clone())
        .await?;
    tracing::info!(
        by = identity.user.username,
        scope = scope.key(),
        "profile unassigned"
    );
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
pub struct EffectiveQuery {
    #[serde(default)]
    pub guild: Option<String>,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
}

/// What the profiles in force add up to for a link seen in a channel of a guild from a
/// user, or for the whole server alone.
#[derive(Debug, Serialize)]
pub struct EffectiveView {
    #[serde(flatten)]
    pub effective: EffectiveProfile,
    /// The resolver ids turned off, as the engine is told.
    pub disabled: Vec<String>,
}

pub async fn effective(
    State(state): State<AppState>,
    Auth(_): Auth,
    Query(query): Query<EffectiveQuery>,
) -> Result<Json<EffectiveView>, ApiError> {
    if query.guild.is_none() && (query.channel.is_some() || query.user.is_some()) {
        return Err(ApiError::BadRequest(
            "channel and user scopes need a guild".into(),
        ));
    }
    let effective = state.profiles.effective(
        query.guild.as_deref(),
        query.channel.as_deref(),
        query.user.as_deref(),
    );
    let disabled = effective.disabled();
    Ok(Json(EffectiveView {
        effective,
        disabled,
    }))
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use serde_json::json;

    use crate::users::Role;
    use crate::web::testing::{Client, app_with_admin};

    #[tokio::test]
    async fn profiles_are_edited_by_admins_and_read_by_anyone() {
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

        // The built-in profile is there from the start and in force for the whole server.
        let (status, body) = viewer.get("/api/profiles").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let profiles = body.as_array().unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0]["name"], "Default");
        assert_eq!(profiles[0]["builtin"], true);
        assert_eq!(profiles[0]["platforms"]["default"], "enabled");
        let default_id = profiles[0]["id"].as_str().unwrap().to_string();
        let (status, body) = viewer.get("/api/profiles/effective").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["platforms"]["fixtured"], true);
        assert_eq!(body["platforms"]["nothing"], true);
        assert_eq!(body["disabled"], json!([]));
        assert_eq!(body["applied"][0]["scope"]["kind"], "global");

        // Viewers may not edit.
        let input = json!({
            "name": "No fixtures",
            "description": "Keeps the fixtured platform off",
            "platforms": {"default": "inherit", "overrides": {"fixtured": false}}
        });
        assert_eq!(
            viewer.post("/api/profiles", input.clone()).await.0,
            StatusCode::FORBIDDEN
        );
        let (status, body) = admin.post("/api/profiles", input.clone()).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let id = body["id"].as_str().unwrap().to_string();
        assert_eq!(body["builtin"], false);
        assert_eq!(body["platforms"]["overrides"]["fixtured"], false);

        // Names are unique, platforms must exist, and the built-in one stays.
        assert_eq!(
            admin.post("/api/profiles", input.clone()).await.0,
            StatusCode::CONFLICT
        );
        let (status, body) = admin
            .post(
                "/api/profiles",
                json!({"name": "Odd", "platforms": {"overrides": {"myspace": true}}}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(
            admin.delete(&format!("/api/profiles/{default_id}")).await.0,
            StatusCode::CONFLICT
        );

        // Put in force for the whole server, the profile turns the platform off there.
        let (status, body) = admin
            .send(
                Method::PUT,
                "/api/profiles/assignments/global",
                Some(json!({"profile_id": id})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["scope"]["kind"], "global");
        let (_, body) = viewer.get("/api/profiles/effective").await;
        assert_eq!(body["platforms"]["fixtured"], false);
        assert_eq!(body["disabled"], json!(["fixtured"]));
        let (status, body) = admin
            .post("/api/jobs", json!({"url": "https://fixture.test/ok"}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("fixtured links are turned off"),
            "{body}"
        );
        // The profile in force cannot be removed, and the server always has one.
        assert_eq!(
            admin.delete(&format!("/api/profiles/{id}")).await.0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            admin
                .send(Method::DELETE, "/api/profiles/assignments/global", None)
                .await
                .0,
            StatusCode::CONFLICT
        );

        // Guild scopes narrow the server's; the whole server needs the settings permission.
        let (status, body) = admin
            .send(
                Method::PUT,
                "/api/profiles/assignments/channel:5:1",
                Some(json!({"profile_id": default_id})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = admin
            .get("/api/profiles/effective?guild=5&channel=1&user=9")
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["platforms"]["fixtured"], true);
        assert_eq!(body["applied"].as_array().unwrap().len(), 2);
        let (_, body) = admin.get("/api/profiles/effective?guild=5&channel=2").await;
        assert_eq!(body["platforms"]["fixtured"], false);
        assert_eq!(
            admin.get("/api/profiles/effective?channel=1").await.0,
            StatusCode::BAD_REQUEST
        );
        let (status, body) = admin.get("/api/profiles/assignments?guild=5").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body.as_array().unwrap().len(), 2);
        assert_eq!(
            viewer.get("/api/profiles/assignments").await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            viewer
                .send(
                    Method::PUT,
                    "/api/profiles/assignments/guild:5",
                    Some(json!({"profile_id": default_id}))
                )
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            admin
                .send(
                    Method::PUT,
                    "/api/profiles/assignments/guild:nope",
                    Some(json!({"profile_id": default_id}))
                )
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            admin
                .send(
                    Method::DELETE,
                    "/api/profiles/assignments/channel:5:1",
                    None
                )
                .await
                .0,
            StatusCode::NO_CONTENT
        );
        let (_, body) = admin.get("/api/profiles/assignments?guild=5").await;
        assert_eq!(body.as_array().unwrap().len(), 1);

        // Back to the default, the link is taken again, and the log has every step.
        admin
            .send(
                Method::PUT,
                "/api/profiles/assignments/global",
                Some(json!({"profile_id": default_id})),
            )
            .await;
        let (status, body) = admin
            .send(
                Method::PUT,
                &format!("/api/profiles/{id}"),
                Some(json!({"name": "No fixtures", "platforms": {"default": "disabled"}})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["platforms"]["default"], "disabled");
        assert_eq!(
            admin.delete(&format!("/api/profiles/{id}")).await.0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            admin.get(&format!("/api/profiles/{id}")).await.0,
            StatusCode::NOT_FOUND
        );
        let (status, body) = admin
            .post("/api/jobs", json!({"url": "https://fixture.test/ok"}))
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let (status, body) = admin.get("/api/audit?target_kind=profile").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let actions: Vec<&str> = body["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["action"].as_str().unwrap())
            .collect();
        assert_eq!(
            actions,
            vec![
                "profile.delete",
                "profile.update",
                "profile.assign",
                "profile.unassign",
                "profile.assign",
                "profile.assign",
                "profile.create",
            ]
        );
        let assign = body["entries"]
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["details"]["scope"]["kind"] == "channel")
            .unwrap();
        assert_eq!(assign["details"]["scope"]["channel_id"], "1");
        assert_eq!(assign["target"]["kind"], "profile");
    }
}
