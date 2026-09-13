//! The Discord applications the server runs bots for: added with a bot token Discord
//! verifies, changed, removed, and each one's bot shown as it runs.

use std::convert::Infallible;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use discoclip_bot::{BotControl, BotStatus, DiscordEndpoints, http_client};
use futures::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::wrappers::errors::BroadcastStreamRecvError;
use twilight_http::error::ErrorType;

use super::AppState;
use super::auth::{Auth, parse_id};
use super::error::ApiError;
use super::proxy::{Client, ClientInfo};
use crate::applications::{
    Application, ApplicationId, Changes, CommandsState, Credentials, InstallLink, install_link,
};
use crate::audit::{Action, Actor, Target};
use crate::bots::BotEvent;
use crate::commands::{self, CommandScope, CommandSummary};
use crate::discord::BotGuild;
use crate::users::Permission;
use twilight_model::id::Id;
use url::Url;

/// An application with the state of its bot, the link that adds the bot to a guild, and
/// the redirect Discord sends browsers back to when the application signs people in,
/// which must be registered on the application's OAuth2 page at Discord.
#[derive(Debug, Serialize)]
pub struct ApplicationView {
    #[serde(flatten)]
    pub application: Application,
    pub bot: BotStatus,
    pub install_url: Url,
    pub login_callback_url: Url,
}

impl ApplicationView {
    fn new(
        state: &AppState,
        client: &ClientInfo,
        application: Application,
        bot: BotStatus,
    ) -> Result<Self, ApiError> {
        let install_url = install_link(&state.discord, &application.client_id, None).url;
        let login_callback_url = super::oauth::callback_url(state, client, "discord")?;
        Ok(Self {
            application,
            bot,
            install_url,
            login_callback_url,
        })
    }
}

async fn view(
    state: &AppState,
    client: &ClientInfo,
    application: Application,
) -> Result<ApplicationView, ApiError> {
    let bot = state
        .bots
        .status(application.id)
        .await
        .unwrap_or_else(|| BotControl::disabled().status());
    ApplicationView::new(state, client, application, bot)
}

/// What Discord says the token belongs to.
struct Verified {
    client_id: String,
    name: String,
}

/// Asks Discord whose bot token this is.
async fn verify_token(endpoints: &DiscordEndpoints, token: &str) -> Result<Verified, ApiError> {
    let http = http_client(token, endpoints);
    let application = match http.current_user_application().await {
        Ok(response) => response
            .model()
            .await
            .map_err(|e| ApiError::BadGateway(format!("Discord answered unexpectedly: {e}")))?,
        Err(error) => {
            return Err(match error.kind() {
                ErrorType::Unauthorized => {
                    ApiError::BadRequest("Discord rejected the bot token".into())
                }
                ErrorType::Response { status, .. } if status.get() == 401 => {
                    ApiError::BadRequest("Discord rejected the bot token".into())
                }
                _ => ApiError::BadGateway(format!("Discord could not be reached: {error}")),
            });
        }
    };
    Ok(Verified {
        client_id: application.id.to_string(),
        name: application.name,
    })
}

pub async fn list(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Client(client): Client,
) -> Result<Json<Vec<ApplicationView>>, ApiError> {
    identity.require(Permission::ManageApplications)?;
    let statuses = state.bots.statuses().await;
    let applications = state.applications.list().await?;
    applications
        .into_iter()
        .map(|application| {
            let bot = statuses
                .get(&application.id)
                .cloned()
                .unwrap_or_else(|| BotControl::disabled().status());
            ApplicationView::new(&state, &client, application, bot)
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Json)
}

#[derive(Debug, Deserialize)]
pub struct CreateRequest {
    /// Defaults to the application's name at Discord.
    pub name: Option<String>,
    pub bot_token: String,
    /// Needed to sign people in with this application.
    pub client_secret: Option<String>,
}

pub async fn create(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Client(client): Client,
    Json(request): Json<CreateRequest>,
) -> Result<(StatusCode, Json<ApplicationView>), ApiError> {
    identity.require(Permission::ManageApplications)?;
    let verified = verify_token(&state.discord, &request.bot_token).await?;
    let name = request.name.as_deref().unwrap_or(&verified.name);
    let actor = identity.actor();
    let application = state
        .applications
        .create(
            &actor,
            name,
            &verified.client_id,
            Credentials {
                bot_token: request.bot_token.clone(),
                client_secret: request.client_secret,
            },
        )
        .await?;
    state.bots.launch(&application, &request.bot_token).await;
    tracing::info!(by = identity.user.username, application = %application.id, name = application.name, "discord application added");
    let application =
        register_commands(&state, &actor, application, &CommandScope::default()).await?;
    Ok((StatusCode::CREATED, Json(view(&state, &client, application).await?)))
}

pub async fn get(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Client(client): Client,
    Path(id): Path<String>,
) -> Result<Json<ApplicationView>, ApiError> {
    identity.require(Permission::ManageApplications)?;
    let id: ApplicationId = parse_id(&id)?;
    let application = state
        .applications
        .get(id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(view(&state, &client, application).await?))
}

#[derive(Debug, Deserialize)]
pub struct UpdateRequest {
    pub name: Option<String>,
    /// A new bot token restarts the bot with it.
    pub bot_token: Option<String>,
    /// `null` removes the secret.
    #[serde(default, deserialize_with = "deserialize_double_option")]
    pub client_secret: Option<Option<String>>,
    pub login: Option<bool>,
}

/// Tells an absent field from an explicit `null`.
fn deserialize_double_option<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Some(Option::<String>::deserialize(deserializer)?))
}

pub async fn update(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Client(client): Client,
    Path(id): Path<String>,
    Json(request): Json<UpdateRequest>,
) -> Result<Json<ApplicationView>, ApiError> {
    identity.require(Permission::ManageApplications)?;
    let id: ApplicationId = parse_id(&id)?;
    let current = state
        .applications
        .get(id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if let Some(token) = &request.bot_token {
        let verified = verify_token(&state.discord, token).await?;
        if verified.client_id != current.client_id {
            return Err(ApiError::BadRequest(format!(
                "that token belongs to Discord application {}, not {}",
                verified.client_id, current.client_id
            )));
        }
    }
    let application = state
        .applications
        .update(
            &identity.actor(),
            id,
            Changes {
                name: request.name,
                bot_token: request.bot_token.clone(),
                client_secret: request.client_secret,
                login: request.login,
            },
        )
        .await?;
    if let Some(token) = &request.bot_token {
        state.bots.launch(&application, token).await;
    }
    state.refresh_discord_login().await?;
    tracing::info!(by = identity.user.username, application = %application.id, "discord application changed");
    Ok(Json(view(&state, &client, application).await?))
}

#[derive(Debug, Deserialize)]
pub struct InstallQuery {
    /// A guild to preselect.
    pub guild: Option<String>,
}

/// The link that adds the application's bot to a guild, with what it asks for.
pub async fn install(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
    Query(query): Query<InstallQuery>,
) -> Result<Json<InstallLink>, ApiError> {
    identity.require(Permission::ManageApplications)?;
    let id: ApplicationId = parse_id(&id)?;
    let application = state
        .applications
        .get(id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(install_link(
        &state.discord,
        &application.client_id,
        query.guild.as_deref(),
    )))
}

/// The guilds the application's bot is in, and the ones it was removed from.
pub async fn guilds(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<Json<Vec<BotGuild>>, ApiError> {
    identity.require(Permission::ManageApplications)?;
    let id: ApplicationId = parse_id(&id)?;
    state
        .applications
        .get(id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(state.bot_guilds.list_for(id).await?))
}

/// Makes Discord's registrations match the application's command scope, given what
/// `previous` had registered, and records the outcome on the application and in the audit
/// log as asked for by `actor`.
async fn register_commands(
    state: &AppState,
    actor: &Actor,
    application: Application,
    previous: &CommandScope,
) -> Result<Application, ApiError> {
    let http = state
        .bots
        .client(application.id)
        .ok_or_else(|| ApiError::Conflict("the application's bot is not running".into()))?;
    let discord_id = application
        .client_id
        .parse()
        .ok()
        .and_then(Id::new_checked)
        .ok_or_else(|| {
            ApiError::Internal(format!(
                "application {} has a bad client id",
                application.id
            ))
        })?;
    let outcome = commands::apply(&http, discord_id, previous, &application.commands.scope)
        .await
        .map_err(|e| e.to_string());
    match &outcome {
        Ok(()) => {
            tracing::info!(application = %application.id, mode = application.commands.scope.mode.as_str(), "slash commands registered")
        }
        Err(error) => {
            tracing::warn!(application = %application.id, "slash commands not registered: {error}")
        }
    }
    Ok(state
        .applications
        .record_commands(actor, application.id, outcome)
        .await?)
}

/// The command scope, how the last registration went, and the commands themselves.
#[derive(Debug, Serialize)]
pub struct CommandsView {
    #[serde(flatten)]
    pub state: CommandsState,
    pub commands: Vec<CommandSummary>,
}

fn commands_view(application: Application) -> CommandsView {
    CommandsView {
        state: application.commands,
        commands: commands::summaries(),
    }
}

pub async fn get_commands(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<Json<CommandsView>, ApiError> {
    identity.require(Permission::ManageApplications)?;
    let id: ApplicationId = parse_id(&id)?;
    let application = state
        .applications
        .get(id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(commands_view(application)))
}

/// Sets where the commands are registered and registers them there at once. A failed
/// registration keeps the new scope and reports the error; `register_commands` retries.
pub async fn set_commands(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
    Json(scope): Json<CommandScope>,
) -> Result<Json<CommandsView>, ApiError> {
    identity.require(Permission::ManageApplications)?;
    let id: ApplicationId = parse_id(&id)?;
    let previous = state
        .applications
        .get(id)
        .await?
        .ok_or(ApiError::NotFound)?
        .commands
        .scope;
    let actor = identity.actor();
    let application = state.applications.set_commands(&actor, id, scope).await?;
    let application = register_commands(&state, &actor, application, &previous).await?;
    tracing::info!(by = identity.user.username, application = %id, "command scope changed");
    if let Some(error) = &application.commands.error {
        return Err(ApiError::BadGateway(format!(
            "the scope was saved, but Discord refused the registration: {error}"
        )));
    }
    Ok(Json(commands_view(application)))
}

/// Registers the commands again where the scope says.
pub async fn register(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<Json<CommandsView>, ApiError> {
    identity.require(Permission::ManageApplications)?;
    let id: ApplicationId = parse_id(&id)?;
    let application = state
        .applications
        .get(id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let scope = application.commands.scope.clone();
    let application = register_commands(&state, &identity.actor(), application, &scope).await?;
    if let Some(error) = &application.commands.error {
        return Err(ApiError::BadGateway(format!(
            "Discord refused the registration: {error}"
        )));
    }
    Ok(Json(commands_view(application)))
}

async fn bot_application(state: &AppState, id: &str) -> Result<Application, ApiError> {
    let id: ApplicationId = parse_id(id)?;
    state.applications.get(id).await?.ok_or(ApiError::NotFound)
}

/// Logs a bot action that went through, by `identity`.
async fn audit_bot(
    state: &AppState,
    identity: &super::auth::Identity,
    action: Action,
    application: &Application,
) -> Result<(), ApiError> {
    state
        .audit
        .record(
            &identity.actor(),
            action,
            Target::application(application.id, &application.name),
            json!({ "enabled": application.enabled }),
        )
        .await?;
    Ok(())
}

/// Starts the application's bot and keeps it meant to run.
pub async fn start_bot(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Client(client): Client,
    Path(id): Path<String>,
) -> Result<Json<ApplicationView>, ApiError> {
    identity.require(Permission::ManageBots)?;
    let application = bot_application(&state, &id).await?;
    let application = state.applications.set_enabled(application.id, true).await?;
    state.bots.start(application.id).await?;
    audit_bot(&state, &identity, Action::BotStart, &application).await?;
    tracing::info!(by = identity.user.username, application = %application.id, "bot started");
    Ok(Json(view(&state, &client, application).await?))
}

/// Stops the application's bot until it is started again, across restarts too.
pub async fn stop_bot(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Client(client): Client,
    Path(id): Path<String>,
) -> Result<Json<ApplicationView>, ApiError> {
    identity.require(Permission::ManageBots)?;
    let application = bot_application(&state, &id).await?;
    let application = state
        .applications
        .set_enabled(application.id, false)
        .await?;
    state.bots.stop(application.id).await?;
    audit_bot(&state, &identity, Action::BotStop, &application).await?;
    tracing::info!(by = identity.user.username, application = %application.id, "bot stopped");
    Ok(Json(view(&state, &client, application).await?))
}

/// Stops and starts the application's bot, and keeps it meant to run.
pub async fn restart_bot(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Client(client): Client,
    Path(id): Path<String>,
) -> Result<Json<ApplicationView>, ApiError> {
    identity.require(Permission::ManageBots)?;
    let application = bot_application(&state, &id).await?;
    let application = state.applications.set_enabled(application.id, true).await?;
    state.bots.restart(application.id).await?;
    audit_bot(&state, &identity, Action::BotRestart, &application).await?;
    tracing::info!(by = identity.user.username, application = %application.id, "bot restarted");
    Ok(Json(view(&state, &client, application).await?))
}

/// Every bot's status now, then each change as it happens, as `bot` events; an
/// application's removal is the last event about its bot.
pub(super) async fn bot_stream(
    state: AppState,
) -> impl Stream<Item = Result<Event, Infallible>> + Send + 'static {
    let live = BroadcastStream::new(state.bots.subscribe());
    let snapshot = state.bots.snapshot().await;
    let event = |bot: BotEvent| {
        Event::default()
            .event("bot")
            .json_data(bot)
            .expect("bot events serialize")
    };
    let first = futures::stream::iter(snapshot.into_iter().map(move |bot| Ok(event(bot))));
    let rest = live.filter_map(move |item| async move {
        match item {
            Ok(bot) => Some(Ok(event(bot))),
            // A slow reader missed some changes; the next one brings it up to date.
            Err(BroadcastStreamRecvError::Lagged(_)) => None,
        }
    });
    first.chain(rest)
}

/// [`bot_stream`] as server-sent events.
pub async fn bot_events(
    State(state): State<AppState>,
    Auth(_): Auth,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    Sse::new(bot_stream(state).await).keep_alive(KeepAlive::default())
}

pub async fn delete(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    identity.require(Permission::ManageApplications)?;
    let id: ApplicationId = parse_id(&id)?;
    state.applications.delete(&identity.actor(), id).await?;
    state.bots.retire(id).await;
    state.refresh_discord_login().await?;
    tracing::info!(by = identity.user.username, application = %id, "discord application removed");
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use axum::http::{Method, StatusCode};
    use serde_json::{Value as Json, json};
    use url::Url;

    use crate::oauth::Registry;
    use crate::settings::WebConfig;
    use crate::web::WebApp;
    use crate::web::testing::{
        Client, FakeDiscord, app_with_discord, guild_create, guild_delete, wait_for,
    };

    async fn setup() -> (FakeDiscord, WebApp, Client) {
        let discord = FakeDiscord::start().await;
        discord.add_bot("bot-token-1", "1001", "Clipper", "secret-1");
        let app = app_with_discord(
            WebConfig::default(),
            Registry::default(),
            true,
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
        (discord, app, admin)
    }

    async fn wait_connected(client: &mut Client, id: &str, user: &str) -> Json {
        let path = format!("/api/discord/applications/{id}");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let (status, body) = client.get(&path).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            if body["bot"]["state"] == "connected" && body["bot"]["user"] == user {
                return body;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "timed out waiting for the bot of {id} to connect as {user}: {body}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }

    #[tokio::test]
    async fn applications_are_verified_at_discord_and_run_a_bot() {
        let (discord, app, mut admin) = setup().await;
        let (status, body) = admin
            .post("/api/discord/applications", json!({"bot_token": "nope"}))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "Discord rejected the bot token");

        let (status, body) = admin
            .post(
                "/api/discord/applications",
                json!({"bot_token": "bot-token-1", "client_secret": "secret-1"}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["client_id"], "1001");
        assert_eq!(body["name"], "Clipper");
        assert_eq!(body["login"], false);
        assert_eq!(body["has_client_secret"], true);
        assert!(body.get("bot_token").is_none());
        assert!(body.get("client_secret").is_none());
        assert_eq!(body["bot"]["state"], "starting");
        let id = body["id"].as_str().unwrap().to_string();

        let body = wait_connected(&mut admin, &id, "Clipper").await;
        assert!(body["bot"]["since"].is_string());
        assert_eq!(discord.lock().identified, vec!["bot-token-1"]);
        let commands = discord.lock().commands.get("1001").cloned().unwrap();
        assert_eq!(commands.as_array().unwrap().len(), 2);

        let (status, body) = admin
            .post(
                "/api/discord/applications",
                json!({"bot_token": "bot-token-1"}),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let (status, body) = admin.get("/api/discord/applications").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 1);
        assert_eq!(body[0]["bot"]["state"], "connected");

        app.state
            .users
            .create("op", Some("battery staple"), crate::users::Role::Operator)
            .await
            .unwrap();
        let mut operator = Client::new(&app);
        operator.login("op", "battery staple").await;
        let (status, _) = operator.get("/api/discord/applications").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _) = operator
            .post(
                "/api/discord/applications",
                json!({"bot_token": "bot-token-1"}),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn updates_rename_retoken_and_offer_discord_login() {
        let (discord, _app, mut admin) = setup().await;
        let (_, body) = admin
            .post(
                "/api/discord/applications",
                json!({"bot_token": "bot-token-1"}),
            )
            .await;
        let id = body["id"].as_str().unwrap().to_string();
        wait_connected(&mut admin, &id, "Clipper").await;
        let path = format!("/api/discord/applications/{id}");

        let (status, body) = admin
            .send(Method::PATCH, &path, Some(json!({"name": "Renamed"})))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["name"], "Renamed");

        discord.add_bot("bot-token-other", "2002", "Other", "secret-2");
        let (status, body) = admin
            .send(
                Method::PATCH,
                &path,
                Some(json!({"bot_token": "bot-token-other"})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body["error"].as_str().unwrap().contains("2002"));

        discord.add_bot("bot-token-1b", "1001", "Clipper", "secret-1");
        let (status, body) = admin
            .send(
                Method::PATCH,
                &path,
                Some(json!({"bot_token": "bot-token-1b"})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        wait_for("the bot to identify with the new token", || async {
            discord
                .lock()
                .identified
                .contains(&"bot-token-1b".to_string())
                .then_some(())
        })
        .await;
        wait_connected(&mut admin, &id, "Clipper").await;

        // Login needs the client secret.
        let (status, body) = admin
            .send(Method::PATCH, &path, Some(json!({"login": true})))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let (_, body) = admin.get("/api/auth/providers").await;
        assert_eq!(body, json!([]));
        let (status, body) = admin
            .send(
                Method::PATCH,
                &path,
                Some(json!({"client_secret": "secret-1", "login": true})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["login"], true);
        let (_, body) = admin.get("/api/auth/providers").await;
        assert_eq!(body, json!([{"id": "discord", "name": "Discord"}]));

        // Removing the secret withdraws the login.
        let (status, body) = admin
            .send(Method::PATCH, &path, Some(json!({"client_secret": null})))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["login"], false);
        assert_eq!(body["has_client_secret"], false);
        let (_, body) = admin.get("/api/auth/providers").await;
        assert_eq!(body, json!([]));
    }

    #[tokio::test]
    async fn deleting_an_application_stops_its_bot() {
        let (discord, app, mut admin) = setup().await;
        let (_, body) = admin
            .post(
                "/api/discord/applications",
                json!({"bot_token": "bot-token-1", "client_secret": "secret-1"}),
            )
            .await;
        let id = body["id"].as_str().unwrap().to_string();
        wait_connected(&mut admin, &id, "Clipper").await;
        admin
            .send(
                Method::PATCH,
                &format!("/api/discord/applications/{id}"),
                Some(json!({"login": true})),
            )
            .await;
        assert_eq!(
            admin
                .get("/api/auth/providers")
                .await
                .1
                .as_array()
                .unwrap()
                .len(),
            1
        );

        let (status, _) = admin
            .delete(&format!("/api/discord/applications/{id}"))
            .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = admin.get(&format!("/api/discord/applications/{id}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(app.state.bots.statuses().await.is_empty());
        assert_eq!(admin.get("/api/auth/providers").await.1, json!([]));
        assert_eq!(discord.lock().identified.len(), 1);
        let (status, _) = admin
            .delete(&format!("/api/discord/applications/{id}"))
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn install_links_and_guild_joins_and_leaves_are_tracked() {
        let (discord, app, mut admin) = setup().await;
        discord.set_ready_guilds("bot-token-1", &["100", "300"]);
        let (_, body) = admin
            .post(
                "/api/discord/applications",
                json!({"bot_token": "bot-token-1"}),
            )
            .await;
        let id = body["id"].as_str().unwrap().to_string();
        let install = Url::parse(body["install_url"].as_str().unwrap()).unwrap();
        assert_eq!(install.path(), "/oauth2/authorize");
        let query: HashMap<String, String> = install.query_pairs().into_owned().collect();
        assert_eq!(query["client_id"], "1001");
        assert_eq!(query["scope"], "bot applications.commands");
        let (status, body) = admin
            .get(&format!("/api/discord/applications/{id}/install?guild=42"))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["url"].as_str().unwrap().contains("guild_id=42"));
        assert_eq!(body["scopes"], json!(["bot", "applications.commands"]));
        assert!(
            body["permissions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|p| p == "SEND_MESSAGES")
        );

        // A guild the ledger remembers as present, but which login does not list, was
        // left while the bot was away.
        app.state
            .bot_guilds
            .joined(
                id.parse().unwrap(),
                crate::discord::JoinedGuild {
                    guild_id: "200".into(),
                    name: "Gone".into(),
                    icon: None,
                    member_count: None,
                },
            )
            .await
            .unwrap();
        wait_connected(&mut admin, &id, "Clipper").await;
        let path = format!("/api/discord/applications/{id}/guilds");
        let ledger = app.state.bot_guilds.clone();
        let app_id: crate::applications::ApplicationId = id.parse().unwrap();
        let ledger_shows = |what: &'static str, check: fn(&[crate::discord::BotGuild]) -> bool| {
            let ledger = ledger.clone();
            async move {
                wait_for(what, || {
                    let ledger = ledger.clone();
                    async move { check(&ledger.list_for(app_id).await.unwrap()).then_some(()) }
                })
                .await
            }
        };
        ledger_shows("the stale guild to be marked left", |guilds| {
            guilds.iter().any(|g| g.guild_id == "200" && !g.present)
        })
        .await;

        // Discord then describes each guild the bot is in, and one it is added to.
        discord.emit(guild_create("100", "Clips", 12));
        discord.emit(guild_create("300", "Lurk", 3));
        discord.emit(guild_create("400", "Brand new", 1));
        ledger_shows("three guilds to be present", |guilds| {
            guilds.iter().filter(|g| g.present).count() == 3
        })
        .await;
        let (status, body) = admin.get(&path).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let clips = body
            .as_array()
            .unwrap()
            .iter()
            .find(|g| g["guild_id"] == "100")
            .unwrap();
        assert_eq!(clips["name"], "Clips");
        assert_eq!(clips["icon"], crate::web::testing::ICON_HASH);
        assert_eq!(clips["member_count"], 12);
        assert!(clips["left_at"].is_null());

        // Removal marks the guild left; an outage does not.
        discord.emit(guild_delete("400", false));
        discord.emit(guild_delete("100", true));
        ledger_shows("the removed guild to be marked left", |guilds| {
            guilds.iter().any(|g| g.guild_id == "400" && !g.present)
        })
        .await;
        let (_, body) = admin.get(&path).await;
        let by_id = |gid: &str| {
            body.as_array()
                .unwrap()
                .iter()
                .find(|g| g["guild_id"] == gid)
                .cloned()
                .unwrap()
        };
        assert_eq!(by_id("100")["present"], true);
        assert_eq!(by_id("400")["present"], false);
        assert!(by_id("400")["left_at"].is_string());

        let (status, _) = admin
            .get("/api/discord/applications/00000000-0000-0000-0000-000000000000/guilds")
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn commands_register_globally_by_default_and_per_guild_on_request() {
        let (discord, _app, mut admin) = setup().await;
        let (status, body) = admin
            .post(
                "/api/discord/applications",
                json!({"bot_token": "bot-token-1"}),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let id = body["id"].as_str().unwrap().to_string();
        assert_eq!(body["commands"]["mode"], "global");
        assert!(body["commands"]["registered_at"].is_string());
        assert!(body["commands"]["error"].is_null());
        assert_eq!(discord.lock().commands["1001"].as_array().unwrap().len(), 2);

        let path = format!("/api/discord/applications/{id}/commands");
        let (status, body) = admin.get(&path).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["mode"], "global");
        assert_eq!(body["guilds"], json!([]));
        let names: Vec<&str> = body["commands"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["clip", "status"]);

        // Per guild: the global set is cleared and each guild gets the commands.
        let (status, body) = admin
            .send(
                Method::PUT,
                &path,
                Some(json!({"mode": "guilds", "guilds": ["100", "200"]})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["mode"], "guilds");
        {
            let fake = discord.lock();
            assert_eq!(fake.commands["1001"], json!([]));
            assert_eq!(fake.commands["1001:100"].as_array().unwrap().len(), 2);
            assert_eq!(fake.commands["1001:200"].as_array().unwrap().len(), 2);
        }
        // Dropping a guild clears it there.
        let (status, body) = admin
            .send(
                Method::PUT,
                &path,
                Some(json!({"mode": "guilds", "guilds": ["100"]})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(discord.lock().commands["1001:200"], json!([]));
        assert_eq!(
            discord.lock().commands["1001:100"]
                .as_array()
                .unwrap()
                .len(),
            2
        );

        // A guild the bot cannot reach: the scope is kept, the error shown.
        let (status, body) = admin
            .send(
                Method::PUT,
                &path,
                Some(json!({"mode": "guilds", "guilds": ["100", crate::web::testing::FORBIDDEN_GUILD]})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        let (_, body) = admin.get(&path).await;
        assert_eq!(body["guilds"], json!(["100", "403"]));
        assert!(body["error"].as_str().unwrap().contains("Missing Access"));
        let (status, _) = admin
            .send(Method::POST, &format!("{path}/register"), None)
            .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        let (status, _) = admin
            .send(
                Method::PUT,
                &path,
                Some(json!({"mode": "guilds", "guilds": ["x"]})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        // Off clears everything; registering again is idempotent.
        let (status, body) = admin
            .send(Method::PUT, &path, Some(json!({"mode": "off"})))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["error"].is_null());
        assert_eq!(discord.lock().commands["1001:100"], json!([]));
        let (status, body) = admin
            .send(Method::POST, &format!("{path}/register"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["mode"], "off");
        let (status, body) = admin
            .send(Method::PUT, &path, Some(json!({"mode": "global"})))
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(discord.lock().commands["1001"].as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn bots_stop_start_and_restart_from_the_app_and_report_live() {
        let (discord, app, mut admin) = setup().await;
        let (_, body) = admin
            .post(
                "/api/discord/applications",
                json!({"bot_token": "bot-token-1"}),
            )
            .await;
        let id = body["id"].as_str().unwrap().to_string();
        assert_eq!(body["enabled"], true);
        wait_connected(&mut admin, &id, "Clipper").await;
        let mut events = app.state.bots.subscribe();

        let bot = format!("/api/discord/applications/{id}/bot");
        let (status, body) = admin.send(Method::POST, &format!("{bot}/stop"), None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["bot"]["state"], "stopped");
        assert_eq!(body["enabled"], false);
        let stopped = events.recv().await.unwrap();
        assert_eq!(stopped.application.to_string(), id);
        assert_eq!(serde_json::to_value(&stopped).unwrap()["state"], "stopped");
        assert_eq!(discord.lock().identified.len(), 1);

        // A stopped bot stays stopped when the server starts again.
        let (application, credentials) = app
            .state
            .applications
            .credentials(id.parse().unwrap())
            .await
            .unwrap();
        assert!(!application.enabled);
        app.state
            .bots
            .launch(&application, &credentials.bot_token)
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
        assert_eq!(discord.lock().identified.len(), 1);
        let (_, body) = admin.get(&format!("/api/discord/applications/{id}")).await;
        assert_eq!(body["bot"]["state"], "stopped");

        let (status, body) = admin
            .send(Method::POST, &format!("{bot}/start"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["enabled"], true);
        assert_eq!(body["bot"]["state"], "starting");
        wait_connected(&mut admin, &id, "Clipper").await;
        assert_eq!(discord.lock().identified.len(), 2);

        let (status, body) = admin
            .send(Method::POST, &format!("{bot}/restart"), None)
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        wait_connected(&mut admin, &id, "Clipper").await;
        assert_eq!(discord.lock().identified.len(), 3);

        // The stream: a snapshot first, then what changes.
        let stream = admin.stream("/api/discord/bots/events").await;
        assert!(stream.contains("event: bot"));
        assert!(stream.contains("\"state\":\"connected\""));
        assert!(stream.contains(&format!("\"application\":\"{id}\"")));

        app.state
            .users
            .create("viewer", Some("battery staple"), crate::users::Role::Viewer)
            .await
            .unwrap();
        let mut viewer = Client::new(&app);
        viewer.login("viewer", "battery staple").await;
        let (status, _) = viewer
            .send(Method::POST, &format!("{bot}/stop"), None)
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(
            viewer
                .stream("/api/discord/bots/events")
                .await
                .contains("event: bot")
        );
        let (status, _) = admin
            .send(
                Method::POST,
                "/api/discord/applications/00000000-0000-0000-0000-000000000000/bot/stop",
                None,
            )
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn discord_login_goes_through_the_application_marked_for_it() {
        let (discord, app, mut admin) = setup().await;
        let (_, body) = admin
            .post(
                "/api/discord/applications",
                json!({"bot_token": "bot-token-1", "client_secret": "secret-1"}),
            )
            .await;
        let id = body["id"].as_str().unwrap().to_string();
        admin
            .send(
                Method::PATCH,
                &format!("/api/discord/applications/{id}"),
                Some(json!({"login": true})),
            )
            .await;
        discord.grant(
            "code-1",
            "4242",
            json!({"id": "4242", "username": "octo", "global_name": "Octo"}),
        );
        discord.lock().guilds = json!([
            {"id": "100", "name": "Clips", "owner": true, "permissions": "0"}
        ]);

        let mut browser = Client::new(&app);
        let (status, _) = browser.get("/api/auth/discord/start").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let location = Url::parse(&browser.location.clone().unwrap()).unwrap();
        assert_eq!(location.host_str(), Some("127.0.0.1"));
        assert_eq!(location.path(), "/oauth2/authorize");
        let query: HashMap<String, String> = location.query_pairs().into_owned().collect();
        assert_eq!(query["client_id"], "1001");
        browser
            .get(&format!(
                "/api/auth/discord/callback?code=code-1&state={}",
                query["state"]
            ))
            .await;
        assert_eq!(browser.location.as_deref(), Some("/"));
        browser.get("/api/session").await;
        let (_, body) = browser.get("/api/session").await;
        assert_eq!(body["user"]["username"], "octo");
        let (_, body) = browser.get("/api/auth/identities").await;
        assert_eq!(body[0]["provider"], "discord");
        assert_eq!(body[0]["subject"], "4242");
        let (_, body) = browser.get("/api/discord/guilds").await;
        assert_eq!(body[0]["name"], "Clips");
        let (status, body) = browser.delete("/api/auth/identities/discord").await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
    }
}
