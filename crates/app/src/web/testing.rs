//! A browser for tests: keeps the cookie and CSRF token between requests to the router.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Json as AxumJson;
use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::http::{Method, Request, StatusCode, header};
use discoclip_engine::store::sqlite::SqliteStore;
use serde_json::Value as Json;
use serde_json::json;
use tower::ServiceExt;

use async_trait::async_trait;
use discoclip_bot::{Clients, DiscordEndpoints, DiscordPublisher};
use discoclip_engine::download::{DownloadContext, DownloadError, Downloaded, Downloader};
use discoclip_engine::event::ProgressSender;
use discoclip_engine::media::{LocalFile, MediaInfo};
use discoclip_engine::resolve::{
    Platform, Resolution, ResolveError, Resolved, Resolver, SessionCheck, SessionSupport, Variant,
    VariantKind,
};
use discoclip_engine::transcode::{Target, TranscodeError, Transcoder};
use discoclip_engine::{Engine, EngineConfig, Http};
use futures::{SinkExt, StreamExt};
use http_body_util::BodyExt;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tokio_websockets::{CloseCode, Message, ServerBuilder};

use super::{Services, WebApp, auth};
use crate::bots::BotManager;
use crate::fixtures::{FixtureRunner, FixtureStore};
use crate::oauth::{Endpoints, Kind, Provider, Registry};
use crate::rules::RuleStore;
use crate::secrets::Keyring;
use crate::settings::{AuthConfig, OAuthClient, OidcClient, Settings, SettingsStore, WebConfig};

pub async fn app() -> WebApp {
    app_with(WebConfig::default(), Registry::default(), false).await
}

pub async fn app_with(config: WebConfig, providers: Registry, oauth_signup: bool) -> WebApp {
    app_with_discord(config, providers, oauth_signup, DiscordEndpoints::default()).await
}

/// An app whose bots and Discord login talk to `discord`, a stand-in.
pub async fn app_with_discord(
    config: WebConfig,
    providers: Registry,
    oauth_signup: bool,
    discord: DiscordEndpoints,
) -> WebApp {
    app_with_discord_db(config, providers, oauth_signup, discord)
        .await
        .0
}

/// Like [`app_with_discord`], with the database the app runs on.
pub async fn app_with_discord_db(
    config: WebConfig,
    providers: Registry,
    oauth_signup: bool,
    discord: DiscordEndpoints,
) -> (WebApp, SqliteStore) {
    let settings = Settings {
        web: config,
        auth: AuthConfig {
            oauth_signup,
            ..AuthConfig::default()
        },
        ..Settings::default()
    };
    app_with_settings(settings, providers, discord).await
}

/// The tools every test app runs on, unpacked once into a directory of their own.
static FFMPEG: tokio::sync::OnceCell<discoclip_engine::ffmpeg::Ffmpeg> =
    tokio::sync::OnceCell::const_new();

/// A cache directory each test app gets to itself, under the temporary directory.
pub fn test_dir(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("discoclip-{name}-{}", uuid::Uuid::now_v7()))
}

/// An app over `settings`, whose settings store holds `settings` as the app wrote them,
/// so a test starts from exactly what it asked for.
pub async fn app_with_settings(
    settings: Settings,
    providers: Registry,
    discord: DiscordEndpoints,
) -> (WebApp, SqliteStore) {
    // DISCOCLIP_TEST_LOG=debug shows what the bots and the gateway do during a test.
    if let Ok(filter) = std::env::var("DISCOCLIP_TEST_LOG") {
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_test_writer()
            .try_init();
    }
    let db = SqliteStore::open_in_memory().await.unwrap();
    crate::migrations::apply(&db).await.unwrap();
    let store = SettingsStore::new(db.clone());
    // Each app works under directories of its own rather than the working directory.
    let mut settings = settings;
    if settings.engine.cache_dir == EngineConfig::default().cache_dir {
        settings.engine.cache_dir = test_dir("cache");
    }
    if settings.local.dir == crate::settings::LocalConfig::default().dir {
        settings.local.dir = test_dir("local");
    }
    let mut tree = std::collections::BTreeMap::new();
    crate::config::json_leaves(&serde_json::to_value(&settings).unwrap(), "", &mut tree);
    let defaults = {
        let mut out = std::collections::BTreeMap::new();
        crate::config::json_leaves(
            &serde_json::to_value(Settings::default()).unwrap(),
            "",
            &mut out,
        );
        out
    };
    let set: std::collections::BTreeMap<String, Json> = tree
        .into_iter()
        .filter(|(key, value)| defaults.get(key) != Some(value))
        .collect();
    if !set.is_empty() {
        store
            .apply(
                &crate::audit::Actor::test(),
                crate::audit::Action::SettingsSet,
                &crate::settings::Change {
                    set,
                    reset: Vec::new(),
                },
            )
            .await
            .unwrap();
    }
    let settings = store.load().await.unwrap();
    let clients = Clients::default();
    let engine = stub_engine(db.clone(), clients.clone(), &settings);
    let handle = engine.handle();
    // The engine never runs, but it must stay alive for its queue to accept submissions.
    std::mem::forget(engine);
    let rules = RuleStore::new(db.clone());
    rules.load().await.unwrap();
    let bots = Arc::new(BotManager::new(
        handle.clone(),
        discord.clone(),
        clients,
        crate::discord::BotGuildStore::new(db.clone()),
        rules.cache(),
        CancellationToken::new(),
    ));
    let ffmpeg = FFMPEG
        .get_or_init(|| async {
            let dir = std::env::temp_dir().join("discoclip-test-ffmpeg");
            discoclip_engine::ffmpeg::Ffmpeg::provision(&dir)
                .await
                .unwrap()
        })
        .await
        .clone();
    let log = crate::telemetry::detached(&settings.log.level);
    let local = Arc::new(std::sync::RwLock::new(settings.local.clone()));
    let fixtures = Arc::new(FixtureRunner::new(
        handle.clone(),
        FixtureStore::new(db.clone()),
        Arc::new(std::sync::RwLock::new(settings.fixtures.clone())),
    ));
    let app = WebApp::new(
        &settings,
        providers,
        Services {
            store: db.clone(),
            keyring: Keyring::from_key([3; 32]),
            bots,
            rules,
            discord,
            engine: handle,
            fixtures,
            settings: store,
            log,
            ffmpeg,
            local,
            data_dir: std::path::PathBuf::from("data"),
            provisioning_file: None,
            started_at: jiff::Timestamp::now(),
        },
    )
    .await
    .unwrap();
    (app, db)
}

/// An app whose admin is `nick` with the password `correct horse`, with its database.
pub async fn app_with_admin_db() -> (WebApp, SqliteStore) {
    let (app, db) = app_with_discord_db(
        WebConfig::default(),
        Registry::default(),
        false,
        DiscordEndpoints::default(),
    )
    .await;
    app.state
        .users
        .set_up("nick", "correct horse")
        .await
        .unwrap();
    (app, db)
}

/// A host the stub engine accepts links from, so submissions can be watched.
pub const SUPPORTED_HOST: &str = "video.test";

/// The host of the fixtured platform's links.
pub const FIXTURE_HOST: &str = "fixture.test";

/// How long the fixtured platform's slow link takes to resolve.
pub const SLOW_FIXTURE: std::time::Duration = std::time::Duration::from_millis(300);

/// A platform with fixtures: `/ok` resolves, `/slow` resolves after [`SLOW_FIXTURE`], and
/// `/bad` fails the first time it is asked and resolves after that. A jar holding a `sid`
/// cookie logs it in as `tester`.
struct Fixtured {
    asked_bad: std::sync::atomic::AtomicUsize,
    http: Http,
}

impl Fixtured {
    fn media(title: &str) -> Resolution {
        let mut resolved = Resolved::new("fixtured");
        resolved.title = Some(title.into());
        resolved.variants.push(Variant::file(
            url::Url::parse("https://fixture.test/media.mp4").unwrap(),
        ));
        resolved.into()
    }
}

#[async_trait]
impl Resolver for Fixtured {
    fn id(&self) -> &'static str {
        "fixtured"
    }

    fn platform(&self) -> Platform {
        Platform {
            id: "fixtured",
            name: "Fixtured",
            hosts: &[FIXTURE_HOST],
            features: &["videos"],
            formats: &["mp4"],
            session: SessionSupport::Optional,
            examples: &[
                "https://fixture.test/ok",
                "https://fixture.test/bad",
                "https://fixture.test/slow",
            ],
        }
    }

    fn matches(&self, url: &url::Url) -> bool {
        url.host_str() == Some(FIXTURE_HOST)
    }

    async fn resolve(&self, url: &url::Url) -> Result<Resolution, ResolveError> {
        match url.path() {
            "/ok" => Ok(Self::media("A fixture")),
            "/slow" => {
                tokio::time::sleep(SLOW_FIXTURE).await;
                Ok(Self::media("A slow fixture"))
            }
            "/bad" => {
                let asked = self
                    .asked_bad
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if asked == 0 {
                    Err(ResolveError::NotFound(url.clone()))
                } else {
                    Ok(Self::media("A recovered fixture"))
                }
            }
            _ => Err(ResolveError::NotFound(url.clone())),
        }
    }

    async fn check_session(&self) -> Result<SessionCheck, ResolveError> {
        Ok(match self.http.jar("fixtured").get("sid") {
            Some(_) => SessionCheck::LoggedIn {
                account: "tester".into(),
            },
            None => SessionCheck::LoggedOut,
        })
    }
}

/// An engine that accepts links to [`SUPPORTED_HOST`] and never runs them; bots only need
/// its handle, and the jobs they submit sit in the database. It also carries the fixtured
/// platform, whose links resolve without the network.
fn stub_engine(db: SqliteStore, clients: Clients, settings: &Settings) -> Engine {
    struct Nothing;

    #[async_trait]
    impl Resolver for Nothing {
        fn id(&self) -> &'static str {
            "nothing"
        }

        fn platform(&self) -> Platform {
            Platform {
                id: "nothing",
                name: "Nothing",
                hosts: &[SUPPORTED_HOST],
                features: &[],
                formats: &[],
                session: SessionSupport::None,
                examples: &[],
            }
        }

        fn matches(&self, url: &url::Url) -> bool {
            url.host_str() == Some(SUPPORTED_HOST)
        }

        async fn resolve(&self, _: &url::Url) -> Result<Resolution, ResolveError> {
            unreachable!("the stub engine never runs a job")
        }
    }

    #[async_trait]
    impl Downloader for Nothing {
        fn handles(&self, _: VariantKind) -> bool {
            false
        }

        async fn download(
            &self,
            _: &Variant,
            _: &std::path::Path,
            _: &DownloadContext,
            _: ProgressSender,
        ) -> Result<Downloaded, DownloadError> {
            unreachable!("the stub downloader handles no variant")
        }
    }

    #[async_trait]
    impl Transcoder for Nothing {
        async fn probe(&self, _: &std::path::Path) -> Result<MediaInfo, TranscodeError> {
            unreachable!("the stub engine never runs a job")
        }

        async fn transcode(
            &self,
            _: &LocalFile,
            _: &Target,
            _: &std::path::Path,
            _: ProgressSender,
        ) -> Result<LocalFile, TranscodeError> {
            unreachable!("the stub engine never runs a job")
        }
    }

    let http = Http::new(settings.http.clone());
    Engine::builder(settings.engine.clone(), Arc::new(db), http.clone())
        .resolver(Nothing)
        .resolver(Fixtured {
            asked_bad: std::sync::atomic::AtomicUsize::new(0),
            http,
        })
        .downloader(Nothing)
        .transcoder(Nothing)
        .publisher(DiscordPublisher::new(clients))
        .publisher(crate::local::LocalPublisher::with_config(
            settings.local.dir.clone(),
            settings.local.max_bytes,
        ))
        .build()
        .unwrap()
}

/// An app whose admin is `nick` with the password `correct horse`.
pub async fn app_with_admin() -> WebApp {
    let app = app().await;
    app.state
        .users
        .set_up("nick", "correct horse")
        .await
        .unwrap();
    app
}

pub struct Client {
    pub router: Router,
    pub ip: SocketAddr,
    pub cookie: Option<String>,
    pub csrf: Option<String>,
    pub origin: Option<String>,
    /// The `Location` of the last response.
    pub location: Option<String>,
    /// Sent as a bearer token instead of the cookie.
    pub bearer: Option<String>,
    /// Extra headers on every request, as a reverse proxy would add them.
    pub headers: Vec<(String, String)>,
    /// The `Set-Cookie` header of the last response, when there was one.
    pub set_cookie: Option<String>,
}

impl Client {
    pub fn new(app: &WebApp) -> Self {
        Self {
            router: app.router(),
            ip: "10.0.0.1:5000".parse().unwrap(),
            cookie: None,
            csrf: None,
            origin: None,
            location: None,
            bearer: None,
            headers: Vec::new(),
            set_cookie: None,
        }
    }

    pub async fn send(
        &mut self,
        method: Method,
        path: &str,
        body: Option<Json>,
    ) -> (StatusCode, Json) {
        let mut request = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "localhost:8080");
        if let Some(cookie) = &self.cookie {
            request = request.header(header::COOKIE, format!("{}={cookie}", auth::COOKIE));
        }
        if let Some(csrf) = &self.csrf {
            request = request.header(auth::CSRF_HEADER, csrf);
        }
        if let Some(origin) = &self.origin {
            request = request.header(header::ORIGIN, origin);
        }
        if let Some(bearer) = &self.bearer {
            request = request.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        for (name, value) in &self.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let body = match body {
            Some(json) => {
                request = request.header(header::CONTENT_TYPE, "application/json");
                Body::from(json.to_string())
            }
            None => Body::empty(),
        };
        let mut request = request.body(body).unwrap();
        request.extensions_mut().insert(ConnectInfo(self.ip));
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        self.location = response
            .headers()
            .get(header::LOCATION)
            .map(|v| v.to_str().unwrap().to_string());
        self.set_cookie = response
            .headers()
            .get(header::SET_COOKIE)
            .map(|v| v.to_str().unwrap().to_string());
        if let Some(set) = response.headers().get(header::SET_COOKIE) {
            let set = set.to_str().unwrap();
            let value = set.split(';').next().unwrap().split_once('=').unwrap().1;
            assert!(set.contains("HttpOnly"), "{set}");
            assert!(set.contains("SameSite=Lax"), "{set}");
            assert!(set.contains("Path=/"), "{set}");
            self.cookie = if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            };
        }
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json = if bytes.is_empty() {
            Json::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or(Json::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        if let Some(csrf) = json.get("csrf_token").and_then(Json::as_str) {
            self.csrf = Some(csrf.to_string());
        }
        (status, json)
    }

    pub async fn get(&mut self, path: &str) -> (StatusCode, Json) {
        self.send(Method::GET, path, None).await
    }

    /// A GET whose answer is read as bytes, with its headers, for files.
    pub async fn raw(&mut self, path: &str) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let mut request = Request::builder()
            .method(Method::GET)
            .uri(path)
            .header(header::HOST, "localhost:8080");
        if let Some(cookie) = &self.cookie {
            request = request.header(header::COOKIE, format!("{}={cookie}", auth::COOKIE));
        }
        if let Some(bearer) = &self.bearer {
            request = request.header(header::AUTHORIZATION, format!("Bearer {bearer}"));
        }
        for (name, value) in &self.headers {
            request = request.header(name.as_str(), value.as_str());
        }
        let mut request = request.body(Body::empty()).unwrap();
        request.extensions_mut().insert(ConnectInfo(self.ip));
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, bytes.to_vec())
    }

    /// Opens a server-sent event stream and returns what arrives within a moment.
    pub async fn stream(&mut self, path: &str) -> String {
        let mut request = Request::builder()
            .method(Method::GET)
            .uri(path)
            .header(header::HOST, "localhost:8080");
        if let Some(cookie) = &self.cookie {
            request = request.header(header::COOKIE, format!("{}={cookie}", auth::COOKIE));
        }
        let mut request = request.body(Body::empty()).unwrap();
        request.extensions_mut().insert(ConnectInfo(self.ip));
        let response = self.router.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/event-stream"
        );
        let mut body = response.into_body();
        let mut text = String::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
        loop {
            let frame = tokio::time::timeout_at(deadline, body.frame()).await;
            match frame {
                Ok(Some(Ok(frame))) => {
                    if let Some(data) = frame.data_ref() {
                        text.push_str(&String::from_utf8_lossy(data));
                    }
                }
                _ => return text,
            }
        }
    }

    pub async fn post(&mut self, path: &str, body: Json) -> (StatusCode, Json) {
        self.send(Method::POST, path, Some(body)).await
    }

    pub async fn delete(&mut self, path: &str) -> (StatusCode, Json) {
        self.send(Method::DELETE, path, None).await
    }

    pub async fn login(&mut self, username: &str, password: &str) -> (StatusCode, Json) {
        self.post(
            "/api/login",
            json!({"username": username, "password": password}),
        )
        .await
    }
}

/// A stand-in for an OAuth provider: issues codes the test hands out, trades them for
/// tokens, serves profiles, and remembers what was revoked.
pub struct FakeProvider {
    pub base: url::Url,
    pub state: Arc<Mutex<FakeState>>,
}

#[derive(Default)]
pub struct FakeState {
    base: String,
    /// Codes waiting to be exchanged, with the subject each stands for.
    pub codes: HashMap<String, String>,
    pub verifiers: Vec<String>,
    /// Access tokens issued, with their subjects.
    pub access: HashMap<String, String>,
    pub refresh: HashMap<String, String>,
    /// Profiles by subject, in the shape the provider kind uses.
    pub profiles: HashMap<String, Json>,
    pub revoked: Vec<String>,
    /// What `/discord/users/@me/guilds` lists.
    pub guilds: Json,
    pub refreshes: u32,
    pub issued: u32,
    pub expires_in: i64,
    pub fail_token: bool,
}

pub const CLIENT_ID: &str = "cid";
pub const CLIENT_SECRET: &str = "csecret";

impl FakeProvider {
    pub async fn start() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = url::Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let state = Arc::new(Mutex::new(FakeState {
            base: base.to_string(),
            expires_in: 3600,
            guilds: json!([]),
            ..FakeState::default()
        }));
        let router = Router::new()
            .route(
                "/.well-known/openid-configuration",
                axum::routing::get(fake_discovery),
            )
            .route("/token", axum::routing::post(fake_token))
            .route("/userinfo", axum::routing::get(fake_userinfo))
            .route("/discord/users/@me", axum::routing::get(fake_userinfo))
            .route("/github/user", axum::routing::get(fake_userinfo))
            .route("/discord/users/@me/guilds", axum::routing::get(fake_guilds))
            .route("/revoke", axum::routing::post(fake_revoke))
            .route(
                "/github/applications/{client_id}/token",
                axum::routing::delete(fake_github_revoke),
            )
            .with_state(state.clone());
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        Self { base, state }
    }

    pub fn client(&self) -> OAuthClient {
        OAuthClient {
            client_id: CLIENT_ID.into(),
            client_secret: CLIENT_SECRET.into(),
        }
    }

    pub fn endpoints(&self, kind: Kind) -> Endpoints {
        let at = |path: &str| self.base.join(path).unwrap();
        Endpoints {
            authorize: at("authorize"),
            token: at("token"),
            userinfo: at(match kind {
                Kind::Discord => "discord/users/@me",
                Kind::GitHub => "github/user",
                Kind::Oidc => "userinfo",
            }),
            revoke: Some(at(match kind {
                Kind::GitHub => "github/applications",
                _ => "revoke",
            })),
            basic_auth: kind == Kind::Oidc,
        }
    }

    /// A provider of `kind` with fixed endpoints here.
    pub fn provider(&self, id: &str, kind: Kind) -> Provider {
        Provider::fixed(id, kind, &self.client(), self.endpoints(kind))
    }

    /// The generic OpenID Connect provider, discovering its endpoints here.
    pub fn oidc(&self) -> Provider {
        Provider::oidc(&OidcClient {
            name: "Fake SSO".into(),
            issuer: self.base.clone(),
            client_id: CLIENT_ID.into(),
            client_secret: CLIENT_SECRET.into(),
            scopes: vec!["openid".into(), "profile".into()],
        })
    }

    /// Registers a code the browser will bring back, for `subject` with `profile`.
    pub fn grant(&self, code: &str, subject: &str, profile: Json) {
        let mut state = self.state.lock().unwrap();
        state.codes.insert(code.into(), subject.into());
        state.profiles.insert(subject.into(), profile);
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().unwrap()
    }
}

type Fake = axum::extract::State<Arc<Mutex<FakeState>>>;

async fn fake_discovery(axum::extract::State(state): Fake) -> AxumJson<Json> {
    let base = state.lock().unwrap().base.clone();
    AxumJson(json!({
        "issuer": base,
        "authorization_endpoint": format!("{base}authorize"),
        "token_endpoint": format!("{base}token"),
        "userinfo_endpoint": format!("{base}userinfo"),
        "revocation_endpoint": format!("{base}revoke"),
        "token_endpoint_auth_methods_supported": ["client_secret_basic", "client_secret_post"],
    }))
}

fn client_ok(headers: &axum::http::HeaderMap, form: &HashMap<String, String>) -> bool {
    if let Some(auth) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        let expected = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            format!("{CLIENT_ID}:{CLIENT_SECRET}"),
        );
        return auth == format!("Basic {expected}");
    }
    form.get("client_id").map(String::as_str) == Some(CLIENT_ID)
        && form.get("client_secret").map(String::as_str) == Some(CLIENT_SECRET)
}

fn issue(state: &mut FakeState, subject: &str) -> Json {
    state.issued += 1;
    let access = format!("access-{}", state.issued);
    let refresh = format!("refresh-{}", state.issued);
    state.access.insert(access.clone(), subject.into());
    state.refresh.insert(refresh.clone(), subject.into());
    json!({
        "access_token": access,
        "refresh_token": refresh,
        "token_type": "Bearer",
        "expires_in": state.expires_in,
        "scope": "openid profile",
    })
}

async fn fake_token(
    axum::extract::State(state): Fake,
    headers: axum::http::HeaderMap,
    axum::Form(form): axum::Form<HashMap<String, String>>,
) -> (StatusCode, AxumJson<Json>) {
    let mut state = state.lock().unwrap();
    if !client_ok(&headers, &form) {
        return (
            StatusCode::UNAUTHORIZED,
            AxumJson(json!({"error": "invalid_client"})),
        );
    }
    if state.fail_token {
        return (
            StatusCode::BAD_REQUEST,
            AxumJson(json!({"error": "invalid_grant"})),
        );
    }
    match form.get("grant_type").map(String::as_str) {
        Some("authorization_code") => {
            let Some(subject) = form.get("code").and_then(|code| state.codes.remove(code)) else {
                return (
                    StatusCode::BAD_REQUEST,
                    AxumJson(json!({"error": "invalid_grant"})),
                );
            };
            let verifier = form.get("code_verifier").cloned().unwrap_or_default();
            state.verifiers.push(verifier);
            (StatusCode::OK, AxumJson(issue(&mut state, &subject)))
        }
        Some("refresh_token") => {
            let Some(subject) = form
                .get("refresh_token")
                .and_then(|token| state.refresh.get(token).cloned())
            else {
                return (
                    StatusCode::BAD_REQUEST,
                    AxumJson(json!({"error": "invalid_grant"})),
                );
            };
            state.refreshes += 1;
            (StatusCode::OK, AxumJson(issue(&mut state, &subject)))
        }
        _ => (
            StatusCode::BAD_REQUEST,
            AxumJson(json!({"error": "unsupported_grant_type"})),
        ),
    }
}

async fn fake_userinfo(
    axum::extract::State(state): Fake,
    headers: axum::http::HeaderMap,
) -> (StatusCode, AxumJson<Json>) {
    let state = state.lock().unwrap();
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    match state
        .access
        .get(token)
        .and_then(|subject| state.profiles.get(subject))
    {
        Some(profile) => (StatusCode::OK, AxumJson(profile.clone())),
        None => (
            StatusCode::UNAUTHORIZED,
            AxumJson(json!({"error": "invalid_token"})),
        ),
    }
}

async fn fake_guilds(
    axum::extract::State(state): Fake,
    headers: axum::http::HeaderMap,
) -> (StatusCode, AxumJson<Json>) {
    let state = state.lock().unwrap();
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if state.access.contains_key(token) {
        (StatusCode::OK, AxumJson(state.guilds.clone()))
    } else {
        (
            StatusCode::UNAUTHORIZED,
            AxumJson(json!({"error": "invalid_token"})),
        )
    }
}

async fn fake_revoke(
    axum::extract::State(state): Fake,
    headers: axum::http::HeaderMap,
    axum::Form(form): axum::Form<HashMap<String, String>>,
) -> StatusCode {
    let mut state = state.lock().unwrap();
    if !client_ok(&headers, &form) {
        return StatusCode::UNAUTHORIZED;
    }
    state
        .revoked
        .push(form.get("token").cloned().unwrap_or_default());
    StatusCode::OK
}

async fn fake_github_revoke(
    axum::extract::State(state): Fake,
    axum::extract::Path(client_id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    AxumJson(body): AxumJson<Json>,
) -> StatusCode {
    let mut state = state.lock().unwrap();
    if client_id != CLIENT_ID || !client_ok(&headers, &HashMap::new()) {
        return StatusCode::UNAUTHORIZED;
    }
    state.revoked.push(
        body["access_token"]
            .as_str()
            .unwrap_or_default()
            .to_string(),
    );
    StatusCode::NO_CONTENT
}

/// A stand-in for Discord: the REST API twilight calls, under `/api/v10`, and a gateway
/// that greets, identifies, acknowledges heartbeats, and relays events tests emit.
pub struct FakeDiscord {
    /// `host:port` of the REST stand-in.
    pub host: String,
    /// `ws://host:port` of the gateway stand-in.
    pub gateway_url: String,
    pub state: Arc<Mutex<FakeDiscordState>>,
}

/// A channel Discord knows, as its REST API lists it.
#[derive(Debug, Clone)]
pub struct FakeChannel {
    pub guild: String,
    pub name: String,
    /// Discord's channel type number: 0 text, 2 voice, 4 category, 5 announcement, 11
    /// thread, 15 forum.
    pub kind: u8,
    pub parent: Option<String>,
    pub position: i32,
}

impl FakeChannel {
    fn json(&self, id: &str) -> Json {
        let mut channel = json!({
            "id": id, "type": self.kind, "guild_id": self.guild, "name": self.name,
            "position": self.position,
        });
        if let Some(parent) = &self.parent {
            channel["parent_id"] = json!(parent);
        }
        channel
    }
}

#[derive(Debug, Clone)]
pub struct FakeBot {
    pub application_id: String,
    pub name: String,
    pub client_secret: String,
    /// Guild ids listed at login.
    pub guilds: Vec<String>,
}

/// A hash of the shape Discord uses for icons.
pub const ICON_HASH: &str = "0123456789abcdef0123456789abcdef";

/// A `GUILD_CREATE` for an available guild, as the gateway sends it.
pub fn guild_create(id: &str, name: &str, members: u64) -> Json {
    json!({
        "op": 0, "t": "GUILD_CREATE",
        "d": {
            "id": id, "name": name, "icon": ICON_HASH, "member_count": members,
            "owner_id": "1", "afk_channel_id": null, "afk_timeout": 300, "application_id": null,
            "banner": null, "default_message_notifications": 0, "description": null,
            "discovery_splash": null, "emojis": [], "explicit_content_filter": 0, "features": [],
            "large": false, "mfa_level": 0, "nsfw_level": 0, "preferred_locale": "en-US",
            "premium_progress_bar_enabled": false, "public_updates_channel_id": null, "roles": [],
            "rules_channel_id": null, "splash": null, "stage_instances": [], "stickers": [],
            "system_channel_flags": 0, "system_channel_id": null, "vanity_url_code": null,
            "verification_level": 0
        }
    })
}

/// A `MESSAGE_CREATE` from a guild member with `roles`.
pub fn message_create(
    channel: &str,
    guild: &str,
    author: &str,
    roles: &[&str],
    content: &str,
) -> Json {
    json!({
        "op": 0, "t": "MESSAGE_CREATE",
        "d": {
            "id": "900", "channel_id": channel, "guild_id": guild,
            "author": {"id": author, "username": "someone", "discriminator": "0", "avatar": null},
            "content": content, "timestamp": "2026-01-01T00:00:00.000000+00:00",
            "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
            "attachments": [], "embeds": [], "pinned": false, "type": 0, "edited_timestamp": null,
            "member": {
                "roles": roles, "joined_at": null, "deaf": false, "mute": false, "flags": 0,
                "nick": null, "communication_disabled_until": null
            }
        }
    })
}

/// A `GUILD_DELETE`: a removal, or an outage when `unavailable`.
pub fn guild_delete(id: &str, unavailable: bool) -> Json {
    let mut d = json!({"id": id});
    if unavailable {
        d["unavailable"] = json!(true);
    }
    json!({"op": 0, "t": "GUILD_DELETE", "d": d})
}

pub struct FakeDiscordState {
    gateway_url: String,
    /// Bot tokens Discord knows, with the application each belongs to.
    pub bots: HashMap<String, FakeBot>,
    /// Tokens seen identifying on the gateway, in order.
    pub identified: Vec<String>,
    /// Commands set, by application id for global ones and `application:guild` per guild.
    pub commands: HashMap<String, Json>,
    /// Channels Discord knows, by id.
    pub channels: HashMap<String, FakeChannel>,
    /// Roles by guild, in the order added.
    pub roles: HashMap<String, Vec<Json>>,
    /// Members by guild, in the order added.
    pub members: HashMap<String, Vec<Json>>,
    /// OAuth codes waiting to be exchanged, with the user subject each stands for.
    pub codes: HashMap<String, String>,
    /// User access tokens issued, with their subjects.
    pub access: HashMap<String, String>,
    pub refresh: HashMap<String, String>,
    /// User profiles by subject.
    pub profiles: HashMap<String, Json>,
    pub guilds: Json,
    pub revoked: Vec<String>,
    pub issued: u32,
    events: broadcast::Sender<Json>,
}

impl FakeDiscord {
    pub async fn start() -> Self {
        let rest = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gateway = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = rest.local_addr().unwrap().to_string();
        let gateway_url = format!("ws://{}", gateway.local_addr().unwrap());
        let (events, _) = broadcast::channel(64);
        let state = Arc::new(Mutex::new(FakeDiscordState {
            gateway_url: gateway_url.clone(),
            bots: HashMap::new(),
            identified: Vec::new(),
            commands: HashMap::new(),
            channels: HashMap::new(),
            roles: HashMap::new(),
            members: HashMap::new(),
            codes: HashMap::new(),
            access: HashMap::new(),
            refresh: HashMap::new(),
            profiles: HashMap::new(),
            guilds: json!([]),
            revoked: Vec::new(),
            issued: 0,
            events,
        }));
        let router = Router::new()
            .route(
                "/api/v10/applications/@me",
                axum::routing::get(discord_current_application),
            )
            .route(
                "/api/v10/gateway/bot",
                axum::routing::get(discord_gateway_bot),
            )
            .route(
                "/api/v10/applications/{id}/commands",
                axum::routing::put(discord_set_commands),
            )
            .route(
                "/api/v10/applications/{id}/guilds/{guild}/commands",
                axum::routing::put(discord_set_guild_commands),
            )
            .route(
                "/api/v10/channels/{id}",
                axum::routing::get(discord_channel),
            )
            .route(
                "/api/v10/guilds/{id}/channels",
                axum::routing::get(discord_guild_channels),
            )
            .route(
                "/api/v10/guilds/{id}/roles",
                axum::routing::get(discord_guild_roles),
            )
            .route(
                "/api/v10/guilds/{id}/members/search",
                axum::routing::get(discord_guild_members_search),
            )
            .route(
                "/api/v10/guilds/{id}/members/{user}",
                axum::routing::get(discord_guild_member),
            )
            .route("/api/v10/users/@me", axum::routing::get(discord_me))
            .route(
                "/api/v10/users/@me/guilds",
                axum::routing::get(discord_my_guilds),
            )
            .route("/api/v10/oauth2/token", axum::routing::post(discord_token))
            .route(
                "/api/v10/oauth2/token/revoke",
                axum::routing::post(discord_revoke),
            )
            .fallback(discord_unknown)
            .with_state(state.clone());
        tokio::spawn(async move {
            axum::serve(rest, router).await.unwrap();
        });
        let gateway_state = state.clone();
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = gateway.accept().await else {
                    return;
                };
                tokio::spawn(gateway_connection(stream, gateway_state.clone()));
            }
        });
        Self {
            host,
            gateway_url,
            state,
        }
    }

    pub fn endpoints(&self) -> DiscordEndpoints {
        DiscordEndpoints {
            http_proxy: Some(self.host.clone()),
            gateway_url: Some(self.gateway_url.clone()),
        }
    }

    /// Makes Discord accept `token` as the bot of `application_id`.
    pub fn add_bot(&self, token: &str, application_id: &str, name: &str, client_secret: &str) {
        self.lock().bots.insert(
            token.into(),
            FakeBot {
                application_id: application_id.into(),
                name: name.into(),
                client_secret: client_secret.into(),
                guilds: Vec::new(),
            },
        );
    }

    /// A text channel Discord knows about.
    pub fn add_channel(&self, id: &str, guild: &str, name: &str) {
        self.add_channel_of(id, guild, name, 0, None, 0);
    }

    /// A channel of Discord type `kind` under `parent`, at `position` among its siblings.
    pub fn add_channel_of(
        &self,
        id: &str,
        guild: &str,
        name: &str,
        kind: u8,
        parent: Option<&str>,
        position: i32,
    ) {
        self.lock().channels.insert(
            id.into(),
            FakeChannel {
                guild: guild.into(),
                name: name.into(),
                kind,
                parent: parent.map(str::to_string),
                position,
            },
        );
    }

    /// A role of `guild`.
    pub fn add_role(
        &self,
        guild: &str,
        id: &str,
        name: &str,
        color: u32,
        position: i64,
        managed: bool,
    ) {
        self.lock()
            .roles
            .entry(guild.into())
            .or_default()
            .push(json!({
                "id": id, "name": name, "color": color,
                "colors": {"primary_color": color, "secondary_color": null, "tertiary_color": null},
                "hoist": false, "managed": managed, "mentionable": false, "permissions": "0",
                "position": position, "flags": 0,
            }));
    }

    /// A member of `guild`, with an account avatar when it has a nickname, and a guild
    /// avatar besides, which the app is not to confuse with the account's.
    pub fn add_member(
        &self,
        guild: &str,
        id: &str,
        username: &str,
        global_name: Option<&str>,
        nick: Option<&str>,
        bot: bool,
    ) {
        let avatar = nick.map(|_| "0123456789abcdef0123456789abcdef");
        let guild_avatar = nick.map(|_| "fedcba9876543210fedcba9876543210");
        self.lock()
            .members
            .entry(guild.into())
            .or_default()
            .push(json!({
                "user": {
                    "id": id, "username": username, "discriminator": "0", "avatar": avatar,
                    "global_name": global_name, "bot": bot,
                },
                "nick": nick, "roles": [], "joined_at": "2026-01-01T00:00:00.000000+00:00",
                "deaf": false, "mute": false, "flags": 0, "pending": false,
                "avatar": guild_avatar,
            }));
    }

    /// The guilds `token`'s bot is in at its next login.
    pub fn set_ready_guilds(&self, token: &str, guilds: &[&str]) {
        self.lock().bots.get_mut(token).unwrap().guilds =
            guilds.iter().map(|g| g.to_string()).collect();
    }

    /// Registers an OAuth code a browser will bring back, for a user.
    pub fn grant(&self, code: &str, subject: &str, profile: Json) {
        let mut state = self.lock();
        state.codes.insert(code.into(), subject.into());
        state.profiles.insert(subject.into(), profile);
    }

    /// Sends a gateway event to every connected bot.
    pub fn emit(&self, event: Json) {
        let _ = self.lock().events.send(event);
    }

    pub fn lock(&self) -> std::sync::MutexGuard<'_, FakeDiscordState> {
        self.state.lock().unwrap()
    }
}

type Discord = axum::extract::State<Arc<Mutex<FakeDiscordState>>>;

fn bot_token(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bot ")
        .map(str::to_string)
}

fn bearer(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::to_string)
}

fn unauthorized() -> (StatusCode, AxumJson<Json>) {
    (
        StatusCode::UNAUTHORIZED,
        AxumJson(json!({"message": "401: Unauthorized", "code": 0})),
    )
}

fn bot_user(bot: &FakeBot) -> Json {
    json!({
        "id": bot.application_id,
        "username": bot.name,
        "discriminator": "0",
        "avatar": null,
        "bot": true,
        "mfa_enabled": false,
    })
}

async fn discord_unknown(uri: axum::http::Uri) -> (StatusCode, AxumJson<Json>) {
    eprintln!("fake discord: no route for {uri}");
    (
        StatusCode::NOT_FOUND,
        AxumJson(json!({"message": format!("404: Not Found ({uri})"), "code": 0})),
    )
}

async fn discord_current_application(
    axum::extract::State(state): Discord,
    headers: axum::http::HeaderMap,
) -> (StatusCode, AxumJson<Json>) {
    let state = state.lock().unwrap();
    let Some(bot) = bot_token(&headers).and_then(|t| state.bots.get(&t)) else {
        return unauthorized();
    };
    (
        StatusCode::OK,
        AxumJson(json!({
            "id": bot.application_id,
            "name": bot.name,
            "icon": null,
            "description": "",
            "bot_public": true,
            "bot_require_code_grant": false,
            "verify_key": "",
            "flags": 0,
            "team": null,
            "rpc_origins": [],
        })),
    )
}

async fn discord_gateway_bot(
    axum::extract::State(state): Discord,
    headers: axum::http::HeaderMap,
) -> (StatusCode, AxumJson<Json>) {
    let state = state.lock().unwrap();
    if bot_token(&headers).is_none_or(|t| !state.bots.contains_key(&t)) {
        return unauthorized();
    }
    (
        StatusCode::OK,
        AxumJson(json!({
            "url": state.gateway_url,
            "shards": 1,
            "session_start_limit": {"total": 1000, "remaining": 999, "reset_after": 0, "max_concurrency": 1},
        })),
    )
}

/// A guild the bot has no access to; registering commands there fails as at Discord.
pub const FORBIDDEN_GUILD: &str = "403";

async fn discord_set_guild_commands(
    axum::extract::State(state): Discord,
    axum::extract::Path((id, guild)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
    AxumJson(body): AxumJson<Json>,
) -> (StatusCode, AxumJson<Json>) {
    let mut state = state.lock().unwrap();
    if bot_token(&headers).is_none_or(|t| !state.bots.contains_key(&t)) {
        return unauthorized();
    }
    if guild == FORBIDDEN_GUILD {
        return (
            StatusCode::FORBIDDEN,
            AxumJson(json!({"message": "Missing Access", "code": 50001})),
        );
    }
    state.commands.insert(format!("{id}:{guild}"), body);
    (StatusCode::OK, AxumJson(json!([])))
}

async fn discord_set_commands(
    axum::extract::State(state): Discord,
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
    AxumJson(body): AxumJson<Json>,
) -> (StatusCode, AxumJson<Json>) {
    let mut state = state.lock().unwrap();
    if bot_token(&headers).is_none_or(|t| !state.bots.contains_key(&t)) {
        return unauthorized();
    }
    state.commands.insert(id, body);
    (StatusCode::OK, AxumJson(json!([])))
}

async fn discord_channel(
    axum::extract::State(state): Discord,
    axum::extract::Path(id): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> (StatusCode, AxumJson<Json>) {
    let state = state.lock().unwrap();
    if bot_token(&headers).is_none_or(|t| !state.bots.contains_key(&t)) {
        return unauthorized();
    }
    match state.channels.get(&id) {
        Some(channel) => (StatusCode::OK, AxumJson(channel.json(&id))),
        None => (
            StatusCode::NOT_FOUND,
            AxumJson(json!({"message": "Unknown Channel", "code": 10003})),
        ),
    }
}

/// Whether the bot behind `headers` is in `guild`: the guild has something in it.
fn guild_known(state: &FakeDiscordState, guild: &str) -> bool {
    state.channels.values().any(|c| c.guild == guild)
        || state.roles.contains_key(guild)
        || state.members.contains_key(guild)
}

fn unknown_guild() -> (StatusCode, AxumJson<Json>) {
    (
        StatusCode::NOT_FOUND,
        AxumJson(json!({"message": "Unknown Guild", "code": 10004})),
    )
}

async fn discord_guild_channels(
    axum::extract::State(state): Discord,
    axum::extract::Path(guild): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> (StatusCode, AxumJson<Json>) {
    let state = state.lock().unwrap();
    if bot_token(&headers).is_none_or(|t| !state.bots.contains_key(&t)) {
        return unauthorized();
    }
    if !guild_known(&state, &guild) {
        return unknown_guild();
    }
    let channels: Vec<Json> = state
        .channels
        .iter()
        .filter(|(_, c)| c.guild == guild)
        .map(|(id, c)| c.json(id))
        .collect();
    (StatusCode::OK, AxumJson(Json::Array(channels)))
}

async fn discord_guild_roles(
    axum::extract::State(state): Discord,
    axum::extract::Path(guild): axum::extract::Path<String>,
    headers: axum::http::HeaderMap,
) -> (StatusCode, AxumJson<Json>) {
    let state = state.lock().unwrap();
    if bot_token(&headers).is_none_or(|t| !state.bots.contains_key(&t)) {
        return unauthorized();
    }
    if !guild_known(&state, &guild) {
        return unknown_guild();
    }
    (
        StatusCode::OK,
        AxumJson(Json::Array(
            state.roles.get(&guild).cloned().unwrap_or_default(),
        )),
    )
}

async fn discord_guild_members_search(
    axum::extract::State(state): Discord,
    axum::extract::Path(guild): axum::extract::Path<String>,
    axum::extract::Query(query): axum::extract::Query<HashMap<String, String>>,
    headers: axum::http::HeaderMap,
) -> (StatusCode, AxumJson<Json>) {
    let state = state.lock().unwrap();
    if bot_token(&headers).is_none_or(|t| !state.bots.contains_key(&t)) {
        return unauthorized();
    }
    if !guild_known(&state, &guild) {
        return unknown_guild();
    }
    let needle = query
        .get("query")
        .cloned()
        .unwrap_or_default()
        .to_lowercase();
    let limit: usize = query.get("limit").and_then(|l| l.parse().ok()).unwrap_or(1);
    let found: Vec<Json> = state
        .members
        .get(&guild)
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .filter(|m| {
            [
                m["user"]["username"].as_str(),
                m["user"]["global_name"].as_str(),
                m["nick"].as_str(),
            ]
            .into_iter()
            .flatten()
            .any(|name| name.to_lowercase().starts_with(&needle))
        })
        .take(limit)
        .collect();
    (StatusCode::OK, AxumJson(Json::Array(found)))
}

async fn discord_guild_member(
    axum::extract::State(state): Discord,
    axum::extract::Path((guild, user)): axum::extract::Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> (StatusCode, AxumJson<Json>) {
    let state = state.lock().unwrap();
    if bot_token(&headers).is_none_or(|t| !state.bots.contains_key(&t)) {
        return unauthorized();
    }
    if !guild_known(&state, &guild) {
        return unknown_guild();
    }
    let found = state
        .members
        .get(&guild)
        .and_then(|members| members.iter().find(|m| m["user"]["id"] == user.as_str()))
        .cloned();
    match found {
        Some(member) => (StatusCode::OK, AxumJson(member)),
        None => (
            StatusCode::NOT_FOUND,
            AxumJson(json!({"message": "Unknown Member", "code": 10007})),
        ),
    }
}

async fn discord_me(
    axum::extract::State(state): Discord,
    headers: axum::http::HeaderMap,
) -> (StatusCode, AxumJson<Json>) {
    let state = state.lock().unwrap();
    if let Some(bot) = bot_token(&headers).and_then(|t| state.bots.get(&t)) {
        return (StatusCode::OK, AxumJson(bot_user(bot)));
    }
    match bearer(&headers)
        .and_then(|t| state.access.get(&t))
        .and_then(|subject| state.profiles.get(subject))
    {
        Some(profile) => (StatusCode::OK, AxumJson(profile.clone())),
        None => unauthorized(),
    }
}

async fn discord_my_guilds(
    axum::extract::State(state): Discord,
    headers: axum::http::HeaderMap,
) -> (StatusCode, AxumJson<Json>) {
    let state = state.lock().unwrap();
    match bearer(&headers).filter(|t| state.access.contains_key(t)) {
        Some(_) => (StatusCode::OK, AxumJson(state.guilds.clone())),
        None => unauthorized(),
    }
}

async fn discord_token(
    axum::extract::State(state): Discord,
    axum::Form(form): axum::Form<HashMap<String, String>>,
) -> (StatusCode, AxumJson<Json>) {
    let mut state = state.lock().unwrap();
    let client_ok = state.bots.values().any(|bot| {
        form.get("client_id") == Some(&bot.application_id)
            && form.get("client_secret") == Some(&bot.client_secret)
    });
    if !client_ok {
        return (
            StatusCode::UNAUTHORIZED,
            AxumJson(json!({"error": "invalid_client"})),
        );
    }
    let subject = match form.get("grant_type").map(String::as_str) {
        Some("authorization_code") => form.get("code").and_then(|code| state.codes.remove(code)),
        Some("refresh_token") => form
            .get("refresh_token")
            .and_then(|token| state.refresh.get(token).cloned()),
        _ => None,
    };
    let Some(subject) = subject else {
        return (
            StatusCode::BAD_REQUEST,
            AxumJson(json!({"error": "invalid_grant"})),
        );
    };
    state.issued += 1;
    let access = format!("discord-access-{}", state.issued);
    let refresh = format!("discord-refresh-{}", state.issued);
    state.access.insert(access.clone(), subject.clone());
    state.refresh.insert(refresh.clone(), subject);
    (
        StatusCode::OK,
        AxumJson(json!({
            "access_token": access,
            "refresh_token": refresh,
            "token_type": "Bearer",
            "expires_in": 604800,
            "scope": "identify guilds guilds.members.read",
        })),
    )
}

async fn discord_revoke(
    axum::extract::State(state): Discord,
    axum::Form(form): axum::Form<HashMap<String, String>>,
) -> StatusCode {
    let mut state = state.lock().unwrap();
    state
        .revoked
        .push(form.get("token").cloned().unwrap_or_default());
    StatusCode::OK
}

async fn gateway_connection(stream: tokio::net::TcpStream, state: Arc<Mutex<FakeDiscordState>>) {
    let Ok((_, mut ws)) = ServerBuilder::new().accept(stream).await else {
        return;
    };
    let mut events = state.lock().unwrap().events.subscribe();
    let mut seq: u64 = 0;
    let hello = json!({"op": 10, "d": {"heartbeat_interval": 45000}});
    if ws.send(Message::text(hello.to_string())).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            incoming = ws.next() => {
                let Some(Ok(message)) = incoming else { return };
                if message.is_close() {
                    // The reply to a close is queued; sending it needs a flush.
                    let _ = ws.close().await;
                    return;
                }
                let Some(text) = message.as_text() else { continue };
                let payload: Json = serde_json::from_str(text).unwrap_or(Json::Null);
                match payload["op"].as_u64() {
                    Some(2) => {
                        // twilight identifies with the token as the REST API takes it.
                        let token = payload["d"]["token"]
                            .as_str()
                            .unwrap_or("")
                            .trim_start_matches("Bot ")
                            .to_string();
                        let bot = {
                            let mut state = state.lock().unwrap();
                            state.identified.push(token.clone());
                            state.bots.get(&token).cloned()
                        };
                        let Some(bot) = bot else {
                            let _ = ws
                                .send(Message::close(
                                    Some(CloseCode::try_from(4004).unwrap()),
                                    "Authentication failed.",
                                ))
                                .await;
                            return;
                        };
                        seq += 1;
                        let gateway_url = state.lock().unwrap().gateway_url.clone();
                        let ready = json!({
                            "op": 0, "t": "READY", "s": seq,
                            "d": {
                                "v": 10,
                                "user": bot_user(&bot),
                                "guilds": bot.guilds.iter().map(|id| json!({"id": id, "unavailable": true})).collect::<Vec<_>>(),
                                "session_id": "session",
                                "resume_gateway_url": gateway_url,
                                "shard": [0, 1],
                                "application": {"id": bot.application_id, "flags": 0},
                            }
                        });
                        if ws.send(Message::text(ready.to_string())).await.is_err() {
                            return;
                        }
                    }
                    Some(1) => {
                        let ack = json!({"op": 11}).to_string();
                        if ws.send(Message::text(ack)).await.is_err() {
                            return;
                        }
                    }
                    Some(6) => {
                        // Resuming is not kept up; the shard identifies again.
                        let invalid = json!({"op": 9, "d": false}).to_string();
                        if ws.send(Message::text(invalid)).await.is_err() {
                            return;
                        }
                    }
                    _ => {}
                }
            }
            event = events.recv() => {
                let Ok(mut event) = event else { return };
                seq += 1;
                event["s"] = json!(seq);
                if ws.send(Message::text(event.to_string())).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// Polls `check` until it returns Some, or panics after ten seconds.
pub async fn wait_for<T, F, Fut>(what: &str, mut check: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if let Some(value) = check().await {
            return value;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "timed out waiting for {what}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}
