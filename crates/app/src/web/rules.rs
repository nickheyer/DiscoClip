//! Watch rules per guild: edited by operators, and by whoever manages the guild on Discord.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use twilight_model::id::Id;

use super::AppState;
use super::auth::{Auth, Identity, parse_id};
use super::error::ApiError;
use crate::applications::ApplicationId;
use crate::rules::{Rule, RuleId, RuleInput};
use crate::users::Permission;

/// 403 unless the account may edit `guild`'s rules: through its role, or because it
/// manages the guild on Discord (from a browser session, as API tokens carry no guilds).
async fn may_edit(state: &AppState, identity: &Identity, guild: &str) -> Result<(), ApiError> {
    if identity.user.role.allows(Permission::ManageWatchRules) {
        return identity.require(Permission::ManageWatchRules);
    }
    if identity.session().is_ok() && state.guilds.can_manage(identity.user.id, guild).await? {
        return Ok(());
    }
    Err(ApiError::Forbidden(
        "editing this guild's rules needs the operator role, or managing the guild on Discord"
            .into(),
    ))
}

/// Confirms through the application's bot that `channel` is a channel of `guild`.
async fn check_channel(
    state: &AppState,
    application: ApplicationId,
    guild: &str,
    channel: &str,
) -> Result<(), ApiError> {
    let id: u64 =
        channel.parse().ok().filter(|id| *id > 0).ok_or_else(|| {
            ApiError::BadRequest(format!("channel {channel:?} is not a Discord id"))
        })?;
    let http = state
        .bots
        .client(application)
        .ok_or_else(|| ApiError::Conflict("the application's bot is not running".into()))?;
    let found = http
        .channel(Id::new(id))
        .await
        .map_err(|e| ApiError::BadRequest(format!("the bot cannot see channel {channel}: {e}")))?
        .model()
        .await
        .map_err(|e| ApiError::BadGateway(format!("Discord answered unexpectedly: {e}")))?;
    if found.guild_id.map(|g| g.to_string()).as_deref() != Some(guild) {
        return Err(ApiError::BadRequest(format!(
            "channel {channel} is not in guild {guild}"
        )));
    }
    Ok(())
}

async fn check_channels(
    state: &AppState,
    application: ApplicationId,
    guild: &str,
    input: &RuleInput,
) -> Result<(), ApiError> {
    check_channel(state, application, guild, &input.channel_id).await?;
    if let Some(post_to) = &input.post_to
        && post_to != &input.channel_id
    {
        check_channel(state, application, guild, post_to).await?;
    }
    Ok(())
}

pub async fn list_for_guild(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path((id, guild)): Path<(String, String)>,
) -> Result<Json<Vec<Rule>>, ApiError> {
    let id: ApplicationId = parse_id(&id)?;
    may_edit(&state, &identity, &guild).await?;
    state
        .applications
        .get(id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(state.rules.list_for_guild(id, &guild).await?))
}

pub async fn create(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path((id, guild)): Path<(String, String)>,
    Json(input): Json<RuleInput>,
) -> Result<(StatusCode, Json<Rule>), ApiError> {
    let id: ApplicationId = parse_id(&id)?;
    may_edit(&state, &identity, &guild).await?;
    state
        .applications
        .get(id)
        .await?
        .ok_or(ApiError::NotFound)?;
    check_channels(&state, id, &guild, &input).await?;
    let rule = state.rules.create(id, &guild, input).await?;
    tracing::info!(by = identity.user.username, rule = %rule.id, guild, channel = rule.input.channel_id, "watch rule added");
    Ok((StatusCode::CREATED, Json(rule)))
}

/// Every rule of every application, for operators.
pub async fn list_all(
    State(state): State<AppState>,
    Auth(identity): Auth,
) -> Result<Json<Vec<Rule>>, ApiError> {
    identity.require(Permission::ManageWatchRules)?;
    Ok(Json(state.rules.list_all().await?))
}

pub async fn get(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<Json<Rule>, ApiError> {
    let id: RuleId = parse_id(&id)?;
    let rule = state.rules.get(id).await?.ok_or(ApiError::NotFound)?;
    may_edit(&state, &identity, &rule.guild_id).await?;
    Ok(Json(rule))
}

pub async fn update(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
    Json(input): Json<RuleInput>,
) -> Result<Json<Rule>, ApiError> {
    let id: RuleId = parse_id(&id)?;
    let current = state.rules.get(id).await?.ok_or(ApiError::NotFound)?;
    may_edit(&state, &identity, &current.guild_id).await?;
    check_channels(&state, current.application_id, &current.guild_id, &input).await?;
    let rule = state.rules.update(id, input).await?;
    tracing::info!(by = identity.user.username, rule = %rule.id, "watch rule changed");
    Ok(Json(rule))
}

pub async fn delete(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let id: RuleId = parse_id(&id)?;
    let current = state.rules.get(id).await?.ok_or(ApiError::NotFound)?;
    may_edit(&state, &identity, &current.guild_id).await?;
    state.rules.delete(id).await?;
    tracing::info!(by = identity.user.username, rule = %id, "watch rule removed");
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use axum::http::{Method, StatusCode};
    use discoclip_engine::{JobFilter, JobStore};
    use serde_json::json;

    use crate::discord::RemoteGuild;
    use crate::oauth::Registry;
    use crate::settings::WebConfig;
    use crate::users::Role;
    use crate::web::WebApp;
    use crate::web::testing::{
        Client, FakeDiscord, SUPPORTED_HOST, app_with_discord_db, message_create, wait_for,
    };

    async fn setup() -> (
        FakeDiscord,
        WebApp,
        discoclip_engine::store::sqlite::SqliteStore,
        Client,
        String,
    ) {
        let discord = FakeDiscord::start().await;
        discord.add_bot("bot-token-1", "1001", "Clipper", "secret-1");
        discord.add_channel("10", "100", "general");
        discord.add_channel("11", "100", "clips");
        discord.add_channel("20", "200", "elsewhere");
        let (app, db) = app_with_discord_db(
            WebConfig::default(),
            Registry::default(),
            false,
            discord.endpoints(),
        )
        .await;
        app.state
            .users
            .set_up("nick", "correct horse")
            .await
            .unwrap();
        let mut admin = Client::new(&app);
        admin.login("nick", "correct horse").await;
        let (status, body) = admin
            .post(
                "/api/discord/applications",
                json!({"bot_token": "bot-token-1"}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let id = body["id"].as_str().unwrap().to_string();
        (discord, app, db, admin, id)
    }

    async fn wait_connected(client: &mut Client, id: &str) {
        let path = format!("/api/discord/applications/{id}");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let (_, body) = client.get(&path).await;
            if body["bot"]["state"] == "connected" {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "bot did not connect: {body}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    #[tokio::test]
    async fn operators_and_guild_managers_edit_rules() {
        let (_discord, app, _db, mut admin, id) = setup().await;
        wait_connected(&mut admin, &id).await;
        let guild_rules = format!("/api/discord/applications/{id}/guilds/100/rules");

        let rule = json!({
            "channel_id": "10", "post_to": "11", "allow_hosts": ["reddit.com"],
            "allow_users": ["9"], "allow_roles": ["500"],
            "max_source_bytes": 1000, "max_duration_secs": 30, "max_height": 720
        });
        let (status, body) = admin.post(&guild_rules, rule.clone()).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["guild_id"], "100");
        assert_eq!(body["channel_id"], "10");
        assert_eq!(body["post_to"], "11");
        assert_eq!(body["enabled"], true);
        assert_eq!(body["max_duration_secs"], 30);
        assert_eq!(body["max_height"], 720);
        let rule_id = body["id"].as_str().unwrap().to_string();

        let (status, body) = admin.post(&guild_rules, json!({"channel_id": "20"})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body["error"].as_str().unwrap().contains("not in guild 100"));
        let (status, body) = admin.post(&guild_rules, json!({"channel_id": "999"})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let (status, body) = admin.post(&guild_rules, json!({"channel_id": "10"})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let (status, _) = admin
            .post(
                &guild_rules,
                json!({"channel_id": "11", "max_source_bytes": 0}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = admin
            .post(&guild_rules, json!({"channel_id": "11", "max_height": 0}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, _) = admin
            .post(&guild_rules, json!({"channel_id": "11", "bogus": 1}))
            .await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

        let (status, body) = admin.get(&guild_rules).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 1);
        let (status, body) = admin.get("/api/discord/rules").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body[0]["id"], rule_id);

        // Accounts: an operator, a viewer, and a viewer who manages guild 100 on Discord.
        for (name, role) in [
            ("op", Role::Operator),
            ("viewer", Role::Viewer),
            ("manager", Role::Viewer),
        ] {
            app.state
                .users
                .create(name, Some("battery staple"), role)
                .await
                .unwrap();
        }
        let manager = app.state.users.find("manager").await.unwrap().unwrap();
        app.state
            .guilds
            .replace(
                manager.id,
                &[RemoteGuild {
                    id: "100".into(),
                    name: "Clips".into(),
                    icon: None,
                    owner: true,
                    permissions: "0".into(),
                }],
            )
            .await
            .unwrap();
        let mut op = Client::new(&app);
        op.login("op", "battery staple").await;
        let mut viewer = Client::new(&app);
        viewer.login("viewer", "battery staple").await;
        let mut manager = Client::new(&app);
        manager.login("manager", "battery staple").await;

        let (status, _) = viewer.get(&guild_rules).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = viewer.get("/api/discord/rules").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = manager.get(&guild_rules).await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = manager.get("/api/discord/rules").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = manager
            .get(&format!("/api/discord/applications/{id}/guilds/200/rules"))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, body) = manager
            .send(
                Method::PUT,
                &format!("/api/discord/rules/{rule_id}"),
                Some(json!({"channel_id": "10", "allow_hosts": ["v.redd.it"]})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["allow_hosts"], json!(["v.redd.it"]));
        assert!(body["post_to"].is_null());
        let (status, body) = op.get(&format!("/api/discord/rules/{rule_id}")).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["allow_hosts"], json!(["v.redd.it"]));

        // A token needs the scope; a manager's token does not inherit the guild path.
        let (_, body) = manager.post("/api/tokens", json!({"name": "t"})).await;
        let mut token = Client::new(&app);
        token.bearer = Some(body["secret"].as_str().unwrap().to_string());
        let (status, _) = token.get(&guild_rules).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (_, body) = op
            .post(
                "/api/tokens",
                json!({"name": "t", "scopes": ["manage_watch_rules"]}),
            )
            .await;
        token.bearer = Some(body["secret"].as_str().unwrap().to_string());
        let (status, _) = token.get(&guild_rules).await;
        assert_eq!(status, StatusCode::OK);

        let (status, _) = viewer
            .delete(&format!("/api/discord/rules/{rule_id}"))
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = manager
            .delete(&format!("/api/discord/rules/{rule_id}"))
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = op.get(&format!("/api/discord/rules/{rule_id}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn rules_decide_what_the_bot_picks_up() {
        let (discord, _app, db, mut admin, id) = setup().await;
        wait_connected(&mut admin, &id).await;
        let guild_rules = format!("/api/discord/applications/{id}/guilds/100/rules");
        let (status, body) = admin
            .post(
                &guild_rules,
                json!({
                    "channel_id": "10", "post_to": "11", "allow_hosts": [SUPPORTED_HOST],
                    "allow_users": ["9"], "max_source_bytes": 5000, "max_duration_secs": 20,
                    "max_height": 480
                }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let rule_id = body["id"].as_str().unwrap().to_string();

        let link = format!("https://{SUPPORTED_HOST}/clip");
        discord.emit(message_create("10", "100", "9", &[], &link));
        let jobs = wait_for("the link to become a job", || {
            let db = db.clone();
            async move {
                let jobs = db.list(&JobFilter::default()).await.unwrap();
                (!jobs.is_empty()).then_some(jobs)
            }
        })
        .await;
        assert_eq!(jobs.len(), 1);
        let request = &jobs[0].request;
        assert_eq!(request.url.as_str(), link);
        assert_eq!(request.destination.as_deref(), Some("11"));
        assert_eq!(request.limits.max_source_bytes, Some(5000));
        assert_eq!(request.limits.max_duration_secs, Some(20));
        assert_eq!(request.limits.max_height, Some(480));
        assert!(request.origin.reference.starts_with(&id));
        assert!(request.origin.reference.contains(":100:10:900:9"));

        // Not from an allowed user, not in a watched channel, not an allowed host.
        discord.emit(message_create("10", "100", "8", &[], &link));
        discord.emit(message_create("11", "100", "9", &[], &link));
        discord.emit(message_create(
            "10",
            "100",
            "9",
            &[],
            "https://other.test/clip",
        ));
        // Then the rule is turned off, and a message that would have counted no longer does.
        let (status, _) = admin
            .send(
                Method::PUT,
                &format!("/api/discord/rules/{rule_id}"),
                Some(json!({"channel_id": "10", "enabled": false})),
            )
            .await;
        assert_eq!(status, StatusCode::OK);
        discord.emit(message_create("10", "100", "9", &[], &link));
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert_eq!(db.list(&JobFilter::default()).await.unwrap().len(), 1);
    }
}
