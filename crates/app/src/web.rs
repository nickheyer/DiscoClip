//! The web app: an HTTP API over the stores, behind sessions, and the browser app that
//! drives it, built from `ui/` and embedded in the binary.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use axum::Router;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::routing::{delete, get, post, put};
use discoclip_bot::DiscordEndpoints;
use discoclip_engine::ffmpeg::Ffmpeg;
use discoclip_engine::store::sqlite::SqliteStore;
use discoclip_engine::{EngineHandle, StoreError};
use jiff::Timestamp;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tower_http::trace::TraceLayer;

use discoclip_engine::http::APP_UA;
use discoclip_engine::reqwest;
use url::Url;

use crate::applications::ApplicationStore;
use crate::audit::AuditStore;
use crate::bots::BotManager;
use crate::cookies::CookieStore;
use crate::discord::{BotGuildStore, GuildStore};
use crate::fixtures::FixtureRunner;
use crate::live::Live;
use crate::local::SharedLocalConfig;
use crate::oauth::{OAuthService, OAuthStore, PendingStates, Provider, Registry};
use crate::ratelimit::RateLimiter;
use crate::rules::RuleStore;
use crate::secrets::Keyring;
use crate::sessions::{SessionStore, random_token};
use crate::settings::{OAuthClient, Settings, SettingsStore, TlsConfig, WebConfig};
use crate::telemetry::LogHandle;
use crate::tokens::TokenStore;
use crate::users::{UserError, UserStore};
use crate::web::proxy::Proxies;

pub mod applications;
pub mod assets;
pub mod audit;
pub mod auth;
pub mod channels;
pub mod discord;
pub mod error;
pub mod health;
pub mod jobs;
pub mod logs;
pub mod metrics;
pub mod oauth;
pub mod platforms;
pub mod proxy;
pub mod rules;
pub mod settings;
#[cfg(test)]
pub mod testing;
pub mod tls;
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
    #[error(transparent)]
    Tls(#[from] tls::TlsError),
    #[error(transparent)]
    Settings(#[from] crate::settings::SettingsError),
    #[error(transparent)]
    Live(#[from] crate::live::LiveError),
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
    /// `web.public_url`, as it stands.
    pub public_url: Arc<RwLock<Option<Url>>>,
    pub applications: ApplicationStore,
    pub bots: Arc<BotManager>,
    pub bot_guilds: BotGuildStore,
    pub rules: RuleStore,
    pub discord: DiscordEndpoints,
    pub audit: AuditStore,
    /// `web.trusted_proxies`: whose forwarding headers are believed, as it stands.
    pub proxies: Arc<RwLock<Proxies>>,
    /// The job engine: what the jobs pages list, stream and act on.
    pub engine: EngineHandle,
    /// Runs the platforms' fixtures and keeps what they found.
    pub fixtures: Arc<FixtureRunner>,
    /// The platforms' cookie jars, sealed in the database.
    pub cookies: CookieStore,
    pub settings: SettingsStore,
    /// Where settings changes are applied.
    pub live: Arc<Live>,
    /// Where the database lives; provisioned, never a setting.
    pub data_dir: PathBuf,
    /// The provisioning file read at startup, when one was.
    pub provisioning_file: Option<PathBuf>,
    /// The database itself, for the health and metrics pages.
    pub db: SqliteStore,
    pub started_at: Timestamp,
    /// Reads the process and the machine for the metrics page.
    pub sampler: Arc<metrics::Sampler>,
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
    pub engine: EngineHandle,
    pub fixtures: Arc<FixtureRunner>,
    pub settings: SettingsStore,
    pub log: LogHandle,
    pub ffmpeg: Ffmpeg,
    /// The `local` settings as the local publisher reads them.
    pub local: SharedLocalConfig,
    pub data_dir: PathBuf,
    pub provisioning_file: Option<PathBuf>,
    /// When the server started.
    pub started_at: Timestamp,
}

pub struct WebApp {
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
    /// `providers` are the login providers offered at first, joined by Discord when an
    /// application is marked for login; the `auth` settings replace them whenever they
    /// change.
    pub async fn new(
        settings: &Settings,
        providers: Registry,
        services: Services,
    ) -> Result<Self, WebError> {
        let Services {
            store,
            keyring,
            bots,
            rules,
            discord,
            engine,
            fixtures,
            settings: settings_store,
            log,
            ffmpeg,
            local,
            data_dir,
            provisioning_file,
            started_at,
        } = services;
        let oauth = Arc::new(OAuthService {
            registry: std::sync::RwLock::new(providers),
            store: OAuthStore::new(store.clone(), keyring.clone()),
            http: oauth_http()?,
            states: PendingStates::default(),
            signup: AtomicBool::new(settings.auth.oauth_signup),
        });
        let public_url = Arc::new(RwLock::new(settings.web.public_url.clone()));
        let proxies = Arc::new(RwLock::new(Proxies::new(
            settings.web.trusted_proxies.clone(),
        )));
        let live = Arc::new(Live {
            log,
            engine: engine.clone(),
            ffmpeg,
            local,
            fixtures: fixtures.config.clone(),
            oauth: oauth.clone(),
            public_url: public_url.clone(),
            proxies: proxies.clone(),
            web: watch::Sender::new(settings.web.clone()),
        });
        let state = AppState {
            users: UserStore::new(store.clone()),
            sessions: SessionStore::new(store.clone()),
            tokens: TokenStore::new(store.clone()),
            guilds: GuildStore::new(store.clone()),
            limits: Arc::new(Limits::default()),
            setup_token: Arc::new(Mutex::new(None)),
            oauth,
            public_url,
            applications: ApplicationStore::new(store.clone(), keyring.clone()),
            cookies: CookieStore::new(store.clone(), keyring),
            bots,
            bot_guilds: BotGuildStore::new(store.clone()),
            rules,
            discord,
            audit: AuditStore::new(store.clone()),
            db: store,
            started_at,
            sampler: Arc::new(metrics::Sampler::new()),
            proxies,
            engine,
            fixtures,
            settings: settings_store,
            live,
            data_dir,
            provisioning_file,
        };
        state.refresh_discord_login().await?;
        Ok(Self { state })
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

    /// The API under `/api`, and the browser app everywhere else. Every request first
    /// has its client worked out through the trusted proxies.
    pub fn router(&self) -> Router {
        Router::new()
            .nest("/api", api(self.state.clone()))
            .fallback(assets::serve)
            .layer(from_fn_with_state(self.state.clone(), proxy::resolve))
            .layer(TraceLayer::new_for_http())
    }

    /// The `web` settings as they stand.
    pub fn config(&self) -> WebConfig {
        self.state.live.web_config()
    }

    /// Opens the socket `web.bind` names.
    pub async fn bind(&self) -> Result<TcpListener, WebError> {
        bind(self.config().bind).await
    }

    /// Binds and serves until `shutdown`.
    pub async fn serve(self, shutdown: CancellationToken) -> Result<(), WebError> {
        let listener = self.bind().await?;
        self.run(listener, shutdown).await
    }

    /// Serves on `listener` until `shutdown`: over TLS when `web.tls` names a certificate,
    /// plain HTTP otherwise. When `web.bind` changes, the new address is listened on before
    /// the old one is given up, so an address that cannot be taken after all leaves the app
    /// where it was; when `web.tls` changes, the same address is listened on again with the
    /// new files. Open connections get a moment to finish at each change and at shutdown.
    pub async fn run(
        self,
        listener: TcpListener,
        shutdown: CancellationToken,
    ) -> Result<(), WebError> {
        let mut web = self.state.live.web.subscribe();
        let mut listener = Some(listener);
        let mut config = web.borrow_and_update().clone();
        loop {
            let socket = match listener.take() {
                Some(socket) => socket,
                None => rebind(config.bind, &shutdown).await?,
            };
            let addr = socket.local_addr()?;
            let scheme = if config.tls.is_some() {
                "https"
            } else {
                "http"
            };
            tracing::info!(%addr, scheme, "web app listening");
            if let Some(token) = self.arm_setup().await? {
                tracing::warn!(
                    "no accounts yet: open {scheme}://{addr}/setup and enter setup token {token}"
                );
            }
            let stop = shutdown.child_token();
            let server = tokio::spawn(serve_on(
                socket,
                config.tls.clone(),
                self.router(),
                stop.clone(),
            ));
            let next = loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break None,
                    changed = web.changed() => {
                        if changed.is_err() {
                            break None;
                        }
                        let next = web.borrow().clone();
                        if next.bind == config.bind && next.tls == config.tls {
                            continue;
                        }
                        if next.bind == config.bind {
                            break Some((next, None));
                        }
                        match bind(next.bind).await {
                            Ok(fresh) => break Some((next, Some(fresh))),
                            Err(error) => {
                                tracing::error!(
                                    "web app stays on {addr}: the new address cannot be listened on: {error}"
                                );
                            }
                        }
                    }
                }
            };
            stop.cancel();
            server
                .await
                .map_err(|e| WebError::Io(std::io::Error::other(e)))??;
            let Some((next, fresh)) = next else {
                return Ok(());
            };
            tracing::info!("web app listener closed; the new address or certificate takes over");
            config = next;
            listener = fresh;
        }
    }
}

/// Listens on `addr` again once the listener there was closed, asking a few times while
/// the socket is released, unless `shutdown` comes first.
async fn rebind(addr: SocketAddr, shutdown: &CancellationToken) -> Result<TcpListener, WebError> {
    const ATTEMPTS: u32 = 50;
    let mut attempt = 0;
    loop {
        attempt += 1;
        match bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(error) if attempt < ATTEMPTS && !shutdown.is_cancelled() => {
                tracing::warn!(attempt, "{error}; asking again");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn bind(addr: SocketAddr) -> Result<TcpListener, WebError> {
    TcpListener::bind(addr)
        .await
        .map_err(|source| WebError::Bind { addr, source })
}

/// Serves `router` on `listener` until `stop`, over TLS when `tls` names a certificate.
async fn serve_on(
    listener: TcpListener,
    tls: Option<TlsConfig>,
    router: Router,
    stop: CancellationToken,
) -> Result<(), WebError> {
    match tls {
        Some(config) => {
            let certificate = tls::Reloading::load(&config)?;
            let server = certificate.server_config()?;
            tokio::spawn(certificate.watch(stop.clone()));
            tls::serve(listener, server, router, stop).await?;
        }
        None => {
            axum::serve(
                listener,
                router.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .with_graceful_shutdown(stop.cancelled_owned())
            .await?;
        }
    }
    Ok(())
}

fn api(state: AppState) -> Router {
    Router::new()
        .route("/setup", get(auth::setup_status).post(auth::setup))
        .route("/login", post(auth::login))
        .route("/logout", post(auth::logout))
        .route("/session", get(auth::current_session))
        .route("/sessions", get(auth::list_sessions))
        .route("/sessions/all", get(users::list_all_sessions))
        .route("/sessions/others", delete(auth::revoke_other_sessions))
        .route("/sessions/{id}", delete(auth::revoke_session))
        .route("/roles", get(users::roles))
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
        .route(
            "/users/{id}/sessions/{session}",
            delete(users::revoke_session),
        )
        .route("/tokens", get(tokens::list).post(tokens::create))
        .route("/tokens/all", get(tokens::list_all))
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
        .route("/settings", get(settings::get).patch(settings::patch))
        .route("/settings/export", get(settings::export))
        .route("/settings/import", post(settings::import))
        .route(
            "/settings/{key}",
            put(settings::set).delete(settings::reset),
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
        .route(
            "/discord/applications/{id}/guilds/{guild}/channels",
            get(channels::list_channels),
        )
        .route(
            "/discord/applications/{id}/guilds/{guild}/roles",
            get(channels::list_roles),
        )
        .route(
            "/discord/applications/{id}/guilds/{guild}/members",
            get(channels::search_members),
        )
        .route(
            "/discord/applications/{id}/guilds/{guild}/members/{user}",
            get(channels::get_member),
        )
        .route("/discord/rules", get(rules::list_all))
        .route(
            "/discord/rules/{id}",
            get(rules::get).put(rules::update).delete(rules::delete),
        )
        .route("/discord/guilds", get(discord::list_guilds))
        .route("/discord/guilds/refresh", post(discord::refresh_guilds))
        .route("/audit", get(audit::list))
        .route("/platforms", get(platforms::list))
        .route("/platforms/check", post(platforms::check_all))
        .route("/platforms/{id}", get(platforms::get))
        .route("/platforms/{id}/check", post(platforms::check))
        .route(
            "/platforms/{id}/cookies",
            put(platforms::import_cookies).delete(platforms::clear_cookies),
        )
        .route(
            "/platforms/{id}/session/check",
            post(platforms::check_session),
        )
        .route("/health", get(health::get))
        .route("/metrics", get(metrics::get))
        .route("/logs", get(logs::list))
        .route("/logs/events", get(logs::events))
        .route("/jobs", get(jobs::list).post(jobs::submit))
        .route("/jobs/stats", get(jobs::stats))
        .route("/jobs/events", get(jobs::events))
        .route("/jobs/bulk", post(jobs::bulk))
        .route("/jobs/{id}", get(jobs::get).delete(jobs::delete))
        .route("/jobs/{id}/children", get(jobs::children))
        .route("/jobs/{id}/retry", post(jobs::retry))
        .route("/jobs/{id}/cancel", post(jobs::cancel))
        .route("/jobs/{id}/download", get(jobs::download))
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

    use super::testing::{Client, app, app_with, app_with_admin};
    use crate::oauth::Registry;
    use crate::settings::{TlsConfig, WebConfig};
    use crate::users::Role;

    fn behind_proxies(nets: &[&str]) -> WebConfig {
        WebConfig {
            trusted_proxies: nets.iter().map(|n| n.parse().unwrap()).collect(),
            ..WebConfig::default()
        }
    }

    #[tokio::test]
    async fn forwarding_headers_count_only_from_trusted_proxies() {
        let app = app_with(behind_proxies(&["10.0.0.0/8"]), Registry::default(), false).await;
        app.state
            .users
            .set_up("nick", "correct horse")
            .await
            .unwrap();

        // The test client's peer is 10.0.0.1, a trusted proxy: the browser is what it
        // forwards, and a forwarded https scheme makes the cookie Secure.
        let mut browser = Client::new(&app);
        browser.headers = vec![
            ("x-forwarded-for".into(), "203.0.113.7, 10.0.0.2".into()),
            ("x-forwarded-proto".into(), "https".into()),
            ("x-forwarded-host".into(), "clips.example.com".into()),
        ];
        let (status, body) = browser.login("nick", "correct horse").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["session"]["ip"], "203.0.113.7");
        assert!(browser.set_cookie.as_deref().unwrap().contains("Secure"));

        // The origin check compares against the forwarded host, not the backend's.
        browser.origin = Some("https://clips.example.com".into());
        let (status, body) = browser.get("/api/session").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, _) = browser.post("/api/logout", Json::Null).await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        // A peer outside the trusted networks keeps its own address and scheme.
        let mut direct = Client::new(&app);
        direct.ip = "192.0.2.5:4000".parse().unwrap();
        direct.headers = vec![
            ("x-forwarded-for".into(), "203.0.113.7".into()),
            ("x-forwarded-proto".into(), "https".into()),
        ];
        let (status, body) = direct.login("nick", "correct horse").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["session"]["ip"], "192.0.2.5");
        assert!(!direct.set_cookie.as_deref().unwrap().contains("Secure"));
    }

    #[tokio::test]
    async fn wrong_passwords_are_counted_against_the_forwarded_address() {
        let app = app_with(behind_proxies(&["10.0.0.1"]), Registry::default(), false).await;
        app.state
            .users
            .set_up("nick", "correct horse")
            .await
            .unwrap();
        let mut proxied = Client::new(&app);
        for i in 0..20 {
            proxied.headers = vec![("x-forwarded-for".into(), format!("203.0.113.{i}"))];
            let (status, _) = proxied.login(&format!("user{i}"), "x").await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
        // Twenty failures from twenty different browsers lock none of them out.
        proxied.headers = vec![("x-forwarded-for".into(), "203.0.113.99".into())];
        let (status, _) = proxied.login("nick", "correct horse").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn login_callbacks_follow_the_forwarded_scheme_and_host() {
        let fake = super::testing::FakeProvider::start().await;
        let app = app_with(
            behind_proxies(&["10.0.0.0/8"]),
            Registry::new(vec![fake.oidc()]),
            true,
        )
        .await;
        let mut browser = Client::new(&app);
        browser.headers = vec![
            ("x-forwarded-proto".into(), "https".into()),
            ("x-forwarded-host".into(), "clips.example.com".into()),
        ];
        let (status, _) = browser.get("/api/auth/oidc/start").await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        let location = url::Url::parse(&browser.location.clone().unwrap()).unwrap();
        let query: std::collections::HashMap<String, String> =
            location.query_pairs().into_owned().collect();
        assert_eq!(
            query["redirect_uri"],
            "https://clips.example.com/api/auth/oidc/callback"
        );
    }

    fn write_certificate(dir: &std::path::Path) -> TlsConfig {
        let key = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let config = TlsConfig {
            cert: dir.join("cert.pem"),
            key: dir.join("key.pem"),
        };
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(&config.cert, key.cert.pem()).unwrap();
        std::fs::write(&config.key, key.key_pair.serialize_pem()).unwrap();
        config
    }

    #[tokio::test]
    async fn https_is_served_from_pem_files_and_reloaded_when_they_change() {
        let dir = std::env::temp_dir().join(format!("discoclip-tls-{}", uuid::Uuid::now_v7()));
        let tls = write_certificate(&dir);
        let config = WebConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            tls: Some(tls.clone()),
            ..WebConfig::default()
        };
        let app = app_with(config, Registry::default(), false).await;
        let listener = app.bind().await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = tokio_util::sync::CancellationToken::new();
        let server = tokio::spawn(app.run(listener, shutdown.clone()));

        let client = discoclip_engine::reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();
        let response = client
            .get(format!("https://{addr}/api/setup"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.headers().get("strict-transport-security").unwrap(),
            "max-age=31536000"
        );
        assert_eq!(
            response.json::<Json>().await.unwrap(),
            json!({"needed": true})
        );
        // Plain HTTP on the same port is refused.
        assert!(
            client
                .get(format!("http://{addr}/api/setup"))
                .send()
                .await
                .is_err()
        );

        // A renewed certificate is picked up from the files.
        let reloading = super::tls::Reloading::load(&tls).unwrap();
        assert!(!reloading.reload_if_changed().unwrap());
        std::thread::sleep(std::time::Duration::from_millis(20));
        let renewed = rcgen::generate_simple_self_signed(vec!["renewed.test".into()]).unwrap();
        std::fs::write(&tls.cert, renewed.cert.pem()).unwrap();
        std::fs::write(&tls.key, renewed.key_pair.serialize_pem()).unwrap();
        assert!(reloading.reload_if_changed().unwrap());
        assert!(!reloading.reload_if_changed().unwrap());
        std::fs::write(&tls.key, "not a key").unwrap();
        assert!(reloading.reload_if_changed().is_err());

        shutdown.cancel();
        server.await.unwrap().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn bad_certificate_files_are_refused_at_startup() {
        let dir = std::env::temp_dir().join(format!("discoclip-tls-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let cert = dir.join("cert.pem");
        let key = dir.join("key.pem");
        std::fs::write(&cert, "").unwrap();
        std::fs::write(&key, "").unwrap();
        let config = TlsConfig {
            cert: cert.clone(),
            key: key.clone(),
        };
        assert!(matches!(
            super::tls::Reloading::load(&config),
            Err(super::tls::TlsError::Read { .. } | super::tls::TlsError::NoCertificate { .. })
        ));
        let config = TlsConfig {
            cert: dir.join("missing.pem"),
            key,
        };
        assert!(matches!(
            super::tls::Reloading::load(&config),
            Err(super::tls::TlsError::Read { .. })
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
