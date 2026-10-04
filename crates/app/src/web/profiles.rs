//! Profiles: the policy, edited by admins, and put assigned per guild, channel or user by
//! operators and by whoever manages the guild on Discord.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use serde::{Deserialize, Serialize};

use super::AppState;
use super::auth::{Auth, Identity, parse_id};
use super::error::ApiError;
use super::rules::may_edit;
use crate::profiles::{
    Assignment, EffectivePolicy, Known, Preset, Profile, ProfileError, ProfileId, ProfileInput,
    Scope, SectionsPatch,
};
use crate::users::Permission;

impl From<ProfileError> for ApiError {
    fn from(error: ProfileError) -> Self {
        match error {
            ProfileError::NotFound(_) => ApiError::NotFound,
            ProfileError::Invalid(_)
            | ProfileError::UnknownPlatform(_)
            | ProfileError::UnknownPreset(_)
            | ProfileError::UnknownView(_)
            | ProfileError::Incomplete(_) => ApiError::BadRequest(error.to_string()),
            ProfileError::Duplicate(_)
            | ProfileError::Builtin(_)
            | ProfileError::InUse(_)
            | ProfileError::ViewDisabled(_)
            | ProfileError::NoPublicUrl
            | ProfileError::GlobalRequired => ApiError::Conflict(error.to_string()),
            ProfileError::Store(_) => ApiError::Internal(error.to_string()),
        }
    }
}

/// 403 unless the account may put profiles assigned at `scope`: the whole server needs
/// the settings permission. A guild's scopes need what editing its rules needs.
async fn may_assign(state: &AppState, identity: &Identity, scope: &Scope) -> Result<(), ApiError> {
    match scope.guild_id() {
        None => identity.require(Permission::ManageSettings),
        Some(guild) => may_edit(state, identity, guild).await,
    }
}

/// What profiles are checked against as the server stands
fn known(state: &AppState) -> Known {
    state.profiles.cache().known(
        state.frontends.cache().views(),
        state.public_url.get().is_some(),
    )
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
    let profile = state.profiles.get(id).await?.ok_or(ApiError::NotFound)?;
    Ok(Json(profile))
}

pub async fn create(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Json(input): Json<ProfileInput>,
) -> Result<(StatusCode, Json<Profile>), ApiError> {
    identity.require(Permission::ManageSettings)?;
    let profile = state
        .profiles
        .create(&identity.actor(), input, &known(&state))
        .await?;
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
    let profile = state
        .profiles
        .update(&identity.actor(), id, input, &known(&state))
        .await?;
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
    /// The guild whose scopes to list, beside the whole server's. Every guild's without.
    #[serde(default)]
    pub guild: Option<String>,
}

/// The profiles assigned: the whole server's and, for a guild, its guild, channel and
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

/// Puts a profile assigned at a scope, named as `global`, `guild:<id>`,
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

/// Takes the profile off a scope, so the parent scope's applies there again.
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

/// Replaces the sections named at a scope in the profile that is the scope's own, making
/// one when the scope shares its profile or has none. Who may post and where results go
/// need what assigning at the scope needs. Output and upload need the settings permission.
pub async fn patch_overlay(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(scope): Path<String>,
    Json(patch): Json<SectionsPatch>,
) -> Result<Json<Profile>, ApiError> {
    let scope = parse_scope(&scope)?;
    may_assign(&state, &identity, &scope).await?;
    if patch.needs_settings_permission() {
        identity.require(Permission::ManageSettings)?;
    }
    let profile = state
        .profiles
        .patch_overlay(&identity.actor(), scope.clone(), patch, &known(&state))
        .await?;
    tracing::info!(by = identity.user.username, scope = scope.key(), profile = %profile.id, "profile options set");
    Ok(Json(profile))
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

/// What the profiles assigned add up to for a link seen in a channel of a guild from a
/// user, or for the whole server alone: every value settled.
#[derive(Debug, Serialize)]
pub struct EffectiveView {
    #[serde(flatten)]
    pub effective: EffectivePolicy,
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

        // The built-in profile is there from the start and assigned for the whole server.
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
        // Every value is settled: the built-in profile names them all.
        assert_eq!(
            body["limits"],
            json!({
                "max_source_bytes": 2_u64 * 1024 * 1024 * 1024,
                "max_duration_secs": 3 * 60 * 60,
                "max_height": 1080,
                "max_capture_secs": 3 * 60 * 60
            })
        );
        assert_eq!(body["delivery"]["under_floor"], "skip");
        assert_eq!(body["upload"]["max_bytes"], "auto");
        assert_eq!(body["message"]["placement"], "reply");
        assert_eq!(body["dedupe"]["enabled"], true);
        assert_eq!(body["applied"][0]["scope"]["kind"], "global");
        assert_eq!(profiles[0]["limits"]["max_height"], 1080);
        let (status, body) = viewer.get(&format!("/api/profiles/{default_id}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["output"]["container"], "mp4");

        // Viewers may not edit.
        let input = json!({
            "name": "No fixtures",
            "description": "Keeps the fixtured platform off",
            "platforms": {"default": "inherit", "overrides": {"fixtured": false}},
            "limits": {"max_duration_secs": 120, "max_height": 720}
        });
        assert_eq!(
            viewer.post("/api/profiles", input.clone()).await.0,
            StatusCode::FORBIDDEN
        );
        let (status, body) = admin.post("/api/profiles", input.clone()).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let id = body["id"].as_str().unwrap().to_string();
        assert_eq!(body["builtin"], false);
        assert!(body["delivery"]["mode"].is_null());
        assert_eq!(body["platforms"]["overrides"]["fixtured"], false);
        assert_eq!(body["limits"]["max_duration_secs"], 120);
        assert_eq!(body["limits"]["max_height"], 720);
        assert!(body["limits"]["max_source_bytes"].is_null());
        let (status, body) = admin
            .post(
                "/api/profiles",
                json!({"name": "Zero", "limits": {"max_height": 0}}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

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

        // Put assigned for the whole server, the profile turns the platform off there.
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
        assert_eq!(body["limits"]["max_duration_secs"], 120);
        let (status, body) = admin
            .post("/api/jobs", json!({"url": "https://fixture.test/ok"}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("The assigned profile disables fixtured links."),
            "{body}"
        );
        // A link the profile allows is queued with the policy stamped on it, the
        // submitter's own limits tightening the policy's where they are tighter.
        let (status, body) = admin
            .post(
                "/api/jobs",
                json!({"url": format!("https://{}/ok", crate::web::testing::SUPPORTED_HOST), "limits": {"max_height": 480, "max_duration_secs": 600}}),
            )
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        let job_id = body["id"].as_str().unwrap().to_string();
        let (status, body) = admin.get(&format!("/api/jobs/{job_id}")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["request"]["limits"]["max_height"], 480);
        assert_eq!(body["request"]["limits"]["max_duration_secs"], 600);
        assert!(body["request"]["limits"]["max_source_bytes"].is_null());
        assert_eq!(
            body["request"]["policy"]["limits"]["max_duration_secs"],
            120
        );
        assert_eq!(body["request"]["policy"]["limits"]["max_height"], 720);
        assert_eq!(body["request"]["policy"]["delivery"]["under_floor"], "skip");
        assert_eq!(body["limits_in_force"]["max_height"], 480);
        assert_eq!(body["limits_in_force"]["max_duration_secs"], 120);
        assert_eq!(
            body["limits_in_force"]["max_source_bytes"],
            2_u64 * 1024 * 1024 * 1024
        );
        // The profile assigned cannot be removed, and the server always has one.
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

        // Guild scopes narrow the server's. The whole server needs the settings permission.
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
        // The oldest entry is the built-in profile naming every value at the first start.
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
                "profile.update",
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

    #[tokio::test]
    async fn the_builtin_is_complete_and_scopes_get_options_of_their_own() {
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
        let (_, body) = admin.get("/api/profiles").await;
        let default = body.as_array().unwrap()[0].clone();
        let default_id = default["id"].as_str().unwrap().to_string();

        // The built-in profile has to name every value.
        let (status, body) = admin
            .send(
                Method::PUT,
                &format!("/api/profiles/{default_id}"),
                Some(json!({"name": "Default", "platforms": {"default": "enabled"}})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            body["error"].as_str().unwrap().contains("leaves out"),
            "{body}"
        );
        let mut lifted = default.clone();
        for key in ["id", "builtin", "created_at", "updated_at"] {
            lifted.as_object_mut().unwrap().remove(key);
        }
        lifted["limits"]["max_duration_secs"] = json!(null);
        let (status, body) = admin
            .send(
                Method::PUT,
                &format!("/api/profiles/{default_id}"),
                Some(lifted),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (_, body) = admin.get("/api/profiles/effective").await;
        assert!(body["limits"]["max_duration_secs"].is_null());

        // Links need a view that exists and a public address.
        let (status, body) = admin
            .post(
                "/api/profiles",
                json!({"name": "Linked", "delivery": {"view": "nope"}}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        // The app learned its address from the admin's own requests, so links may be
        // chosen without naming a view.
        let (status, body) = admin
            .post(
                "/api/profiles",
                json!({"name": "Linked", "delivery": {"under_floor": "link"}}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["delivery"]["under_floor"], "link");
        assert!(body["delivery"]["view"].is_null());
        let (status, body) = admin
            .post(
                "/api/profiles",
                json!({"name": "Odd", "upload": {"max_bytes": 0}}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let (status, body) = admin
            .post(
                "/api/profiles",
                json!({"name": "Odd", "output": {"video_codec": "vp9"}}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body["error"].as_str().unwrap().starts_with("output:"));

        // A scope's options go into a profile of its own.
        let (status, body) = admin
            .send(
                Method::PUT,
                "/api/profiles/assignments/channel:5:1/overlay",
                Some(json!({"intake": {"allow_users": ["9"]}, "message": {"destination": "50"}})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["name"], "Channel 1 options");
        assert_eq!(body["intake"]["allow_users"], json!(["9"]));
        assert_eq!(body["message"]["destination"], "50");
        assert!(body["limits"].get("max_duration_secs").is_none());
        let overlay_id = body["id"].as_str().unwrap().to_string();
        let (_, body) = viewer
            .get("/api/profiles/effective?guild=5&channel=1")
            .await;
        assert_eq!(body["intake"]["allow_users"], json!(["9"]));
        assert_eq!(body["message"]["destination"], "50");
        assert_eq!(body["message"]["placement"], "reply");
        assert_eq!(body["applied"][1]["profile_id"], overlay_id);
        let (status, body) = admin
            .send(
                Method::PUT,
                "/api/profiles/assignments/channel:5:1/overlay",
                Some(json!({"message": {"destination": null}})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["id"], overlay_id);
        assert!(body["message"]["destination"].is_null());
        assert_eq!(body["intake"]["allow_users"], json!(["9"]));
        assert_eq!(
            viewer
                .send(
                    Method::PUT,
                    "/api/profiles/assignments/channel:5:1/overlay",
                    Some(json!({"intake": {"allow_users": []}}))
                )
                .await
                .0,
            StatusCode::FORBIDDEN
        );
        let (status, body) = admin
            .send(
                Method::PUT,
                "/api/profiles/assignments/channel:5:1/overlay",
                Some(json!({"message": {"destination": "x"}})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let (status, body) = admin
            .send(
                Method::PUT,
                "/api/profiles/assignments/global/overlay",
                Some(json!({"intake": {"live": false}})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
}
