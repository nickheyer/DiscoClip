//! The web app: an HTTP API over the stores, behind sessions, and the browser app that
//! drives it, built from `ui/` and embedded in the binary.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::{delete, get, post, put};
use discoclip_bot::DiscordEndpoints;
use discoclip_engine::StoreError;
use discoclip_engine::store::sqlite::SqliteStore;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tower_http::trace::TraceLayer;

use discoclip_engine::http::APP_UA;
use discoclip_engine::reqwest;
use url::Url;

use crate::applications::ApplicationStore;
use crate::audit::AuditStore;
use crate::bots::BotManager;
use crate::discord::{BotGuildStore, GuildStore};
use crate::oauth::{OAuthService, OAuthStore, PendingStates, Provider, Registry};
use crate::ratelimit::RateLimiter;
use crate::rules::RuleStore;
use crate::secrets::Keyring;
use crate::sessions::{SessionStore, random_token};
use crate::settings::{OAuthClient, WebConfig};
use crate::tokens::TokenStore;
use crate::users::{UserError, UserStore};

pub mod applications;
pub mod assets;
pub mod audit;
pub mod auth;
pub mod discord;
pub mod error;
pub mod oauth;
pub mod rules;
#[cfg(test)]
pub mod testing;
pub mod tokens;
pub mod users;

#[derive(Debug, thiserror::Error)]
pub enum WebError {
    #[error("could not listen on {addr}: {source}")]
    Bind {
        addr: SocketAddr,
        source: std::io::Error,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    User(#[from] UserError),
    #[error("http client: {0}")]
    Http(#[from] discoclip_engine::reqwest::Error),
    #[error(transparent)]
    Application(#[from] crate::applications::ApplicationError),
}

/// How many failed attempts a key gets before it has to wait.
pub struct Limits {
    /// Wrong passwords per username.
    pub login_user: RateLimiter,
    /// Wrong passwords and unknown API tokens per client address.
    pub login_ip: RateLimiter,
    /// Wrong setup tokens per client address.
    pub setup: RateLimiter,
}

impl Default for Limits {
    fn default() -> Self {
        let quarter_hour = Duration::from_secs(15 * 60);
        Self {
            login_user: RateLimiter::new(5, quarter_hour),
            login_ip: RateLimiter::new(20, quarter_hour),
            setup: RateLimiter::new(5, quarter_hour),
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub users: UserStore,
    pub sessions: SessionStore,
    pub tokens: TokenStore,
    pub guilds: GuildStore,
    pub limits: Arc<Limits>,
    /// Set while no account exists; the setup page must present it.
    pub setup_token: Arc<Mutex<Option<String>>>,
    pub oauth: Arc<OAuthService>,
    /// `web.public_url`.
    pub public_url: Option<Url>,
    pub applications: ApplicationStore,
    pub bots: Arc<BotManager>,
    pub bot_guilds: BotGuildStore,
    pub rules: RuleStore,
    pub discord: DiscordEndpoints,
    pub audit: AuditStore,
}

impl AppState {
    /// Offers Discord login through the application marked for it, or withdraws it.
    pub async fn refresh_discord_login(&self) -> Result<(), WebError> {
        let provider =
            self.applications
                .login_application()
                .await?
                .and_then(|(application, credentials)| {
                    credentials.client_secret.map(|secret| {
                        Provider::discord(
                            &OAuthClient {
                                client_id: application.client_id,
                                client_secret: secret.into(),
                            },
                            &self.discord,
                        )
                    })
                });
        self.oauth
            .registry
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .set_discord(provider);
        Ok(())
    }
}

/// What the web app is built over.
pub struct Services {
    pub store: SqliteStore,
    pub keyring: Keyring,
    /// Runs the applications' bots.
    pub bots: Arc<BotManager>,
    pub rules: RuleStore,
    pub discord: DiscordEndpoints,
}

pub struct WebApp {
    config: WebConfig,
    state: AppState,
}

/// The client the login providers are reached with: token exchanges, discovery documents
/// and user info, none of which should hang a login.
fn oauth_http() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .user_agent(APP_UA)
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
}

impl WebApp {
    /// `providers` are the login providers the settings name, joined by Discord when an
    /// application is marked for login; `oauth_signup` lets an unknown provider identity
    /// create its own viewer account.
    pub async fn new(
        config: WebConfig,
        providers: Registry,
        oauth_signup: bool,
        services: Services,
    ) -> Result<Self, WebError> {
        let Services {
            store,
            keyring,
            bots,
            rules,
            discord,
        } = services;
        let oauth = OAuthService {
            registry: std::sync::RwLock::new(providers),
            store: OAuthStore::new(store.clone(), keyring.clone()),
            http: oauth_http()?,
            states: PendingStates::default(),
            signup: oauth_signup,
        };
        let state = AppState {
            users: UserStore::new(store.clone()),
            sessions: SessionStore::new(store.clone()),
            tokens: TokenStore::new(store.clone()),
            guilds: GuildStore::new(store.clone()),
            limits: Arc::new(Limits::default()),
            setup_token: Arc::new(Mutex::new(None)),
            oauth: Arc::new(oauth),
            public_url: config.public_url.clone(),
            applications: ApplicationStore::new(store.clone(), keyring),
            bots,
            bot_guilds: BotGuildStore::new(store.clone()),
            rules,
            discord,
            audit: AuditStore::new(store),
        };
        state.refresh_discord_login().await?;
        Ok(Self { state, config })
    }

    /// While no account exists, arms the setup page with a fresh token and returns it.
    pub async fn arm_setup(&self) -> Result<Option<String>, WebError> {
        if self.state.users.is_set_up().await? {
            return Ok(None);
        }
        let token = random_token();
        *self
            .state
            .setup_token
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(token.clone());
        Ok(Some(token))
    }

    /// The API under `/api`, and the browser app everywhere else.
    pub fn router(&self) -> Router {
        Router::new()
            .nest("/api", api(self.state.clone()))
            .fallback(assets::serve)
            .layer(TraceLayer::new_for_http())
    }

    pub async fn serve(self, shutdown: CancellationToken) -> Result<(), WebError> {
        let addr = self.config.bind;
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|source| WebError::Bind { addr, source })?;
        let addr = listener.local_addr()?;
        tracing::info!(%addr, "web app listening");
        if let Some(token) = self.arm_setup().await? {
            tracing::warn!(
                "no accounts yet: open http://{addr}/setup and enter setup token {token}"
            );
        }
        axum::serve(
            listener,
            self.router()
                .into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(shutdown.cancelled_owned())
        .await?;
        Ok(())
    }
}

fn api(state: AppState) -> Router {
    Router::new()
        .route("/setup", get(auth::setup_status).post(auth::setup))
        .route("/login", post(auth::login))
        .route("/logout", post(auth::logout))
        .route("/session", get(auth::current_session))
        .route("/sessions", get(auth::list_sessions))
        .route("/sessions/others", delete(auth::revoke_other_sessions))
        .route("/sessions/{id}", delete(auth::revoke_session))
        .route("/users", get(users::list).post(users::create))
        .route(
            "/users/{id}",
            get(users::get).patch(users::update).delete(users::delete),
        )
        .route("/users/{id}/password", put(users::set_password))
        .route(
            "/users/{id}/sessions",
            get(users::list_sessions).delete(users::revoke_sessions),
        )
        .route("/tokens", get(tokens::list).post(tokens::create))
        .route("/tokens/{id}", delete(tokens::revoke))
        .route("/users/{id}/tokens", get(tokens::list_for_user))
        .route(
            "/users/{id}/tokens/{token}",
            delete(tokens::revoke_for_user),
        )
        .route("/auth/providers", get(oauth::providers))
        .route("/auth/identities", get(oauth::identities))
        .route("/auth/identities/{provider}", delete(oauth::unlink))
        .route(
            "/auth/identities/{provider}/refresh",
            post(oauth::refresh_identity),
        )
        .route(
            "/discord/applications",
            get(applications::list).post(applications::create),
        )
        .route(
            "/discord/applications/{id}",
            get(applications::get)
                .patch(applications::update)
                .delete(applications::delete),
        )
        .route(
            "/discord/applications/{id}/install",
            get(applications::install),
        )
        .route(
            "/discord/applications/{id}/commands",
            get(applications::get_commands).put(applications::set_commands),
        )
        .route(
            "/discord/applications/{id}/commands/register",
            post(applications::register),
        )
        .route(
            "/discord/applications/{id}/bot/start",
            post(applications::start_bot),
        )
        .route(
            "/discord/applications/{id}/bot/stop",
            post(applications::stop_bot),
        )
        .route(
            "/discord/applications/{id}/bot/restart",
            post(applications::restart_bot),
        )
        .route("/discord/bots/events", get(applications::bot_events))
        .route(
            "/discord/applications/{id}/guilds",
            get(applications::guilds),
        )
        .route(
            "/discord/applications/{id}/guilds/{guild}/rules",
            get(rules::list_for_guild).post(rules::create),
        )
        .route("/discord/rules", get(rules::list_all))
        .route(
            "/discord/rules/{id}",
            get(rules::get).put(rules::update).delete(rules::delete),
        )
        .route("/discord/guilds", get(discord::list_guilds))
        .route("/discord/guilds/refresh", post(discord::refresh_guilds))
        .route("/audit", get(audit::list))
        .route("/auth/{provider}/start", get(oauth::start))
        .route("/auth/{provider}/callback", get(oauth::callback))
        .fallback(api_not_found)
        .layer(from_fn(auth::csrf_guard))
        .layer(from_fn_with_state(state.clone(), auth::identify))
        .with_state(state)
}

/// An `/api` path nothing answers: the API's own 404, never the browser app's page.
async fn api_not_found() -> error::ApiError {
    error::ApiError::NotFound
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::{Value as Json, json};

    use super::testing::{Client, app, app_with_admin};
    use crate::users::Role;

    #[tokio::test]
    async fn first_run_setup_needs_the_token_and_happens_once() {
        let app = app().await;
        let token = app.arm_setup().await.unwrap().unwrap();
        let mut client = Client::new(&app);
        let (status, body) = client.get("/api/setup").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"needed": true}));

        let (status, _) = client
            .post(
                "/api/setup",
                json!({"username": "nick", "password": "correct horse", "token": "wrong"}),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(client.cookie.is_none());

        let (status, body) = client
            .post(
                "/api/setup",
                json!({"username": "nick", "password": "short", "token": token}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        let (status, body) = client
            .post(
                "/api/setup",
                json!({"username": "nick", "password": "correct horse", "token": token}),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["user"]["username"], "nick");
        assert_eq!(body["user"]["role"], "admin");
        assert!(client.cookie.is_some());
        assert!(client.csrf.is_some());

        let (_, body) = client.get("/api/setup").await;
        assert_eq!(body, json!({"needed": false}));
        let (status, _) = client
            .post(
                "/api/setup",
                json!({"username": "other", "password": "correct horse", "token": token}),
            )
            .await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, body) = client.get("/api/session").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["user"]["username"], "nick");
        assert!(app.arm_setup().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn wrong_setup_tokens_are_rate_limited() {
        let app = app().await;
        app.arm_setup().await.unwrap();
        let mut client = Client::new(&app);
        for _ in 0..5 {
            let (status, _) = client
                .post(
                    "/api/setup",
                    json!({"username": "nick", "password": "correct horse", "token": "wrong"}),
                )
                .await;
            assert_eq!(status, StatusCode::FORBIDDEN);
        }
        let (status, _) = client
            .post(
                "/api/setup",
                json!({"username": "nick", "password": "correct horse", "token": "wrong"}),
            )
            .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn login_and_logout() {
        let app = app_with_admin().await;
        let mut client = Client::new(&app);
        let (status, _) = client.get("/api/session").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);

        let (status, body) = client.login("nick", "wrong horse").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"], "not logged in");
        let (status, _) = client.login("nobody", "correct horse").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(client.cookie.is_none());

        let (status, body) = client.login("NICK", "correct horse").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["user"]["username"], "nick");
        assert!(body["user"].get("password_hash").is_none());
        assert_eq!(body["session"]["current"], true);
        assert_eq!(body["session"]["ip"], "10.0.0.1");
        let cookie = client.cookie.clone().unwrap();
        assert_eq!(cookie.len(), 43);

        let (status, body) = client.get("/api/session").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["user"]["username"], "nick");

        let (status, _) = client.post("/api/logout", Json::Null).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(client.cookie.is_none());
        client.cookie = Some(cookie);
        let (status, _) = client.get("/api/session").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_passwords_are_rate_limited_per_user_and_address() {
        let app = app_with_admin().await;
        app.state
            .users
            .create("other", Some("battery staple"), Role::Viewer)
            .await
            .unwrap();
        let mut client = Client::new(&app);
        for _ in 0..5 {
            let (status, _) = client.login("nick", "wrong horse").await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
        let (status, body) = client.login("nick", "correct horse").await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
        // Another account from the same address still gets its own tries...
        let (status, _) = client.login("other", "battery staple").await;
        assert_eq!(status, StatusCode::OK);
        // ...and another address gets its own tries at the first account.
        let mut elsewhere = Client::new(&app);
        elsewhere.ip = "10.0.0.2:1".parse().unwrap();
        let (status, _) = elsewhere.login("nick", "correct horse").await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);

        // Twenty failures from one address lock the address for every account.
        let mut hammer = Client::new(&app);
        hammer.ip = "10.0.0.3:1".parse().unwrap();
        for i in 0..20 {
            let (status, _) = hammer.login(&format!("user{i}"), "x").await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
        let (status, _) = hammer.login("other", "battery staple").await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn state_changes_need_the_csrf_token_and_a_matching_origin() {
        let app = app_with_admin().await;
        let mut client = Client::new(&app);
        client.login("nick", "correct horse").await;
        let csrf = client.csrf.take().unwrap();

        let (status, body) = client.post("/api/logout", Json::Null).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"], "missing or wrong CSRF token");
        client.csrf = Some("wrong".into());
        let (status, _) = client.post("/api/logout", Json::Null).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        client.csrf = Some(csrf);
        client.origin = Some("http://evil.example".into());
        let (status, body) = client.post("/api/logout", Json::Null).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["error"], "request origin does not match this host");
        client.origin = Some("null".into());
        let (status, _) = client.post("/api/logout", Json::Null).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        client.origin = Some("http://LOCALHOST:8080".into());
        let (status, _) = client.get("/api/session").await;
        assert_eq!(status, StatusCode::OK);
        let (status, _) = client.post("/api/logout", Json::Null).await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        // Login itself is guarded by origin alone, having no session yet.
        let mut fresh = Client::new(&app);
        fresh.origin = Some("http://evil.example".into());
        let (status, _) = fresh.login("nick", "correct horse").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn sessions_are_listed_and_revoked() {
        let app = app_with_admin().await;
        app.state
            .users
            .create("other", Some("battery staple"), Role::Viewer)
            .await
            .unwrap();
        let mut phone = Client::new(&app);
        phone.login("nick", "correct horse").await;
        let mut laptop = Client::new(&app);
        laptop.login("nick", "correct horse").await;
        let mut other = Client::new(&app);
        other.login("other", "battery staple").await;

        let (status, body) = laptop.get("/api/sessions").await;
        assert_eq!(status, StatusCode::OK);
        let sessions = body.as_array().unwrap();
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0]["current"], true);
        assert_eq!(sessions[1]["current"], false);
        let phone_id = sessions[1]["id"].as_str().unwrap().to_string();

        let (status, _) = other.delete(&format!("/api/sessions/{phone_id}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = laptop.delete("/api/sessions/not-a-session").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = laptop.delete(&format!("/api/sessions/{phone_id}")).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _) = phone.get("/api/session").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _) = laptop.delete(&format!("/api/sessions/{phone_id}")).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let mut tablet = Client::new(&app);
        tablet.login("nick", "correct horse").await;
        let (status, body) = laptop.delete("/api/sessions/others").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"revoked": 1}));
        let (status, _) = tablet.get("/api/session").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, body) = laptop.get("/api/sessions").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 1);

        let own = body[0]["id"].as_str().unwrap().to_string();
        let (status, _) = laptop.delete(&format!("/api/sessions/{own}")).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(laptop.cookie.is_none());
        let (status, _) = other.get("/api/session").await;
        assert_eq!(status, StatusCode::OK);
    }
}
