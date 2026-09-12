//! The server's settings, for admins: read with the defaults beside them, changed key by
//! key or several at once, reset to their defaults, and carried in and out as
//! provisioning files. Every change is stored, logged and applied to the running server
//! in that order; a change the running server cannot take is refused before anything is
//! stored.

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json_;

use super::AppState;
use super::auth::Auth;
use super::error::ApiError;
use crate::audit::Action;
use crate::config::{Format, Provisioning};
use crate::live::LiveError;
use crate::settings::{Change, SettingsError, View};
use crate::users::Permission;

impl From<SettingsError> for ApiError {
    fn from(error: SettingsError) -> Self {
        match error {
            SettingsError::Invalid { .. } | SettingsError::Conflict(_) => {
                ApiError::BadRequest(error.to_string())
            }
            SettingsError::Provisioning(inner) => ApiError::BadRequest(inner.to_string()),
            SettingsError::Store(inner) => ApiError::Internal(inner.to_string()),
        }
    }
}

impl From<LiveError> for ApiError {
    fn from(error: LiveError) -> Self {
        match error {
            LiveError::Rejected { .. } | LiveError::Log(_) => {
                ApiError::BadRequest(error.to_string())
            }
            LiveError::Engine(_) | LiveError::Ffmpeg(_) => ApiError::Internal(error.to_string()),
        }
    }
}

/// The settings with what only provisioning sets beside them.
#[derive(Debug, Serialize)]
pub struct SettingsView {
    #[serde(flatten)]
    pub view: View,
    /// Where the database lives; read from the provisioning file and the environment only.
    pub data_dir: String,
    /// The provisioning file read at startup, when one was.
    pub provisioning_file: Option<String>,
}

async fn view(state: &AppState) -> Result<Json<SettingsView>, ApiError> {
    Ok(Json(SettingsView {
        view: state.settings.view().await?,
        data_dir: state.data_dir.display().to_string(),
        provisioning_file: state
            .provisioning_file
            .as_ref()
            .map(|path| path.display().to_string()),
    }))
}

pub async fn get(
    State(state): State<AppState>,
    Auth(identity): Auth,
) -> Result<Json<SettingsView>, ApiError> {
    identity.require(Permission::ManageSettings)?;
    view(&state).await
}

/// Applies `change` to the running server, then stores it as `action`. The server takes
/// the new settings first, so what is stored is what runs: when it cannot take them, or
/// the store refuses them, the settings it ran on before are put back.
async fn change(
    state: &AppState,
    identity: &super::auth::Identity,
    action: Action,
    change: &Change,
) -> Result<Json<SettingsView>, ApiError> {
    if change.is_empty() {
        return Err(ApiError::BadRequest("nothing to set or reset".into()));
    }
    let current = state.settings.load().await?;
    let next = state.settings.preview(change).await?;
    state.live.check(&current, &next).await?;
    if let Err(error) = state.live.apply(&next).await {
        restore(state, &current).await;
        return Err(error.into());
    }
    let changed = match state
        .settings
        .apply(&identity.actor(), action, change)
        .await
    {
        Ok(changed) => changed,
        Err(error) => {
            restore(state, &current).await;
            return Err(error.into());
        }
    };
    tracing::info!(
        by = identity.user.username,
        written = ?changed.written,
        removed = ?changed.removed,
        "settings changed"
    );
    view(state).await
}

/// Puts `previous` back into the running server after a change did not go through.
async fn restore(state: &AppState, previous: &crate::settings::Settings) {
    if let Err(error) = state.live.apply(previous).await {
        tracing::error!("previous settings not restored after a failed change: {error}");
    }
}

/// Several keys set and reset together.
pub async fn patch(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Json(body): Json<Change>,
) -> Result<Json<SettingsView>, ApiError> {
    identity.require(Permission::ManageSettings)?;
    change(&state, &identity, Action::SettingsSet, &body).await
}

#[derive(Debug, Deserialize)]
pub struct SetRequest {
    pub value: Json_,
}

pub async fn set(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(key): Path<String>,
    Json(request): Json<SetRequest>,
) -> Result<Json<SettingsView>, ApiError> {
    identity.require(Permission::ManageSettings)?;
    change(
        &state,
        &identity,
        Action::SettingsSet,
        &Change::set(&key, request.value),
    )
    .await
}

pub async fn reset(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(key): Path<String>,
) -> Result<Json<SettingsView>, ApiError> {
    identity.require(Permission::ManageSettings)?;
    change(
        &state,
        &identity,
        Action::SettingsReset,
        &Change::reset(&key),
    )
    .await
}

#[derive(Debug, Deserialize)]
pub struct ImportRequest {
    pub format: Format,
    pub text: String,
}

/// Writes every key of a provisioning file as an app change.
pub async fn import(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Json(request): Json<ImportRequest>,
) -> Result<Json<SettingsView>, ApiError> {
    identity.require(Permission::ManageSettings)?;
    let provisioning = Provisioning::from_text(&request.text, request.format)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let body = state.settings.import_change(&provisioning).await?;
    change(&state, &identity, Action::SettingsImport, &body).await
}

#[derive(Debug, Deserialize)]
pub struct ExportQuery {
    #[serde(default = "toml")]
    pub format: Format,
}

fn toml() -> Format {
    Format::Toml
}

/// The stored values as a provisioning file.
pub async fn export(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Query(query): Query<ExportQuery>,
) -> Result<Response, ApiError> {
    identity.require(Permission::ManageSettings)?;
    let text = state.settings.export(query.format).await?;
    let (content_type, extension) = match query.format {
        Format::Toml => ("application/toml; charset=utf-8", "toml"),
        Format::Yaml => ("application/yaml; charset=utf-8", "yaml"),
        Format::Json => ("application/json; charset=utf-8", "json"),
    };
    let disposition = format!("attachment; filename=\"discoclip.{extension}\"");
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, HeaderValue::from_static(content_type)),
            (
                header::CONTENT_DISPOSITION,
                HeaderValue::from_str(&disposition)
                    .map_err(|e| ApiError::Internal(format!("content disposition: {e}")))?,
            ),
        ],
        text,
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use axum::http::{Method, StatusCode};
    use serde_json::{Value as Json, json};
    use url::Url;

    use crate::oauth::Registry;
    use crate::settings::{Settings, WebConfig};
    use crate::users::Role;
    use crate::web::testing::{Client, app_with_admin, app_with_settings, test_dir};

    async fn admin(app: &crate::web::WebApp) -> Client {
        let mut client = Client::new(app);
        client.login("nick", "correct horse").await;
        client
    }

    #[tokio::test]
    async fn settings_are_read_with_defaults_sources_and_withheld_secrets() {
        let mut settings = Settings::default();
        settings.engine.workers = 5;
        settings.auth.github = Some(crate::settings::OAuthClient {
            client_id: "gh-id".into(),
            client_secret: "gh-secret".into(),
        });
        let (app, _) = app_with_settings(
            settings,
            Registry::default(),
            discoclip_bot::DiscordEndpoints::default(),
        )
        .await;
        app.state
            .users
            .set_up("nick", "correct horse")
            .await
            .unwrap();
        let mut admin = admin(&app).await;
        let (status, body) = admin.get("/api/settings").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["settings"]["engine"]["workers"], 5);
        assert_eq!(body["defaults"]["engine"]["workers"], 2);
        assert_eq!(body["settings"]["auth"]["github"]["client_id"], "gh-id");
        assert_eq!(
            body["settings"]["auth"]["github"]["client_secret"],
            Json::Null
        );
        assert_eq!(body["secrets"], json!(["auth.github.client_secret"]));
        assert!(!body.to_string().contains("gh-secret"));
        assert_eq!(body["data_dir"], "data");
        assert!(body["provisioning_file"].is_null());
        let entries = body["entries"].as_array().unwrap();
        assert!(
            entries
                .iter()
                .any(|e| e["key"] == "engine.workers" && e["source"] == "app")
        );

        app.state
            .users
            .create("op", Some("battery staple"), Role::Operator)
            .await
            .unwrap();
        let mut operator = Client::new(&app);
        operator.login("op", "battery staple").await;
        let (status, body) = operator.get("/api/settings").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            body["error"],
            "the operator role does not allow managing settings"
        );
        let (status, _) = operator
            .send(
                Method::PUT,
                "/api/settings/engine.workers",
                Some(json!({"value": 1})),
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        // A token needs the scope.
        let (_, body) = admin
            .post(
                "/api/tokens",
                json!({"name": "narrow", "scopes": ["manage_jobs"]}),
            )
            .await;
        let mut script = Client::new(&app);
        script.bearer = Some(body["secret"].as_str().unwrap().to_string());
        assert_eq!(script.get("/api/settings").await.0, StatusCode::FORBIDDEN);
        let (_, body) = admin
            .post(
                "/api/tokens",
                json!({"name": "wide", "scopes": ["manage_settings"]}),
            )
            .await;
        script.bearer = Some(body["secret"].as_str().unwrap().to_string());
        assert_eq!(script.get("/api/settings").await.0, StatusCode::OK);
    }

    #[tokio::test]
    async fn changes_are_stored_logged_and_applied_to_the_running_server() {
        let app = app_with_admin().await;
        let mut admin = admin(&app).await;
        let cache = test_dir("cache-moved");
        let local = test_dir("local-moved");
        let (status, body) = admin
            .send(
                Method::PATCH,
                "/api/settings",
                Some(json!({
                    "set": {
                        "engine.workers": 3,
                        "engine.cache_dir": cache.display().to_string(),
                        "engine.limits.max_height": 720,
                        "local.dir": local.display().to_string(),
                        "local.max_bytes": 4096,
                        "fixtures.interval_secs": 3600,
                        "http.user_agent": "Tester/1.0",
                        "http.connect_timeout_secs": 3,
                        "http.rate_limits.hosts": {"youtube.com": {"per_second": 1.0, "burst": 2}},
                        "web.public_url": "https://clips.example.com",
                        "web.trusted_proxies": ["10.0.0.0/8"],
                        "auth.oauth_signup": true,
                        "auth.github": {"client_id": "id", "client_secret": "secret"},
                        "log.level": "info,discoclip=debug"
                    }
                })),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["settings"]["engine"]["workers"], 3);
        assert_eq!(
            body["settings"]["auth"]["github"]["client_secret"],
            Json::Null
        );
        assert_eq!(body["secrets"], json!(["auth.github.client_secret"]));

        // The engine, the HTTP client, local publishing, proxies, the public URL and the
        // login providers follow.
        let engine = app.state.engine.config();
        assert_eq!(engine.workers, 3);
        assert_eq!(engine.limits.max_height, 720);
        assert_eq!(engine.cache_dir, cache);
        assert!(cache.join("jobs").is_dir());
        assert_eq!(app.state.engine.utilisation().workers, 3);
        assert_eq!(app.state.live.ffmpeg.cache_dir(), cache);
        let http = app.state.engine.http().config();
        assert_eq!(http.user_agent, "Tester/1.0");
        assert_eq!(http.connect_timeout_secs, 3);
        assert_eq!(http.rate_limits.hosts["youtube.com"].burst, 2);
        assert_eq!(app.state.live.local.read().unwrap().max_bytes, 4096);
        assert_eq!(app.state.live.fixtures.read().unwrap().interval_secs, 3600);
        assert_eq!(app.state.fixtures.config().interval_secs, 3600);
        assert_eq!(
            app.state
                .public_url
                .read()
                .unwrap()
                .as_ref()
                .unwrap()
                .as_str(),
            "https://clips.example.com/"
        );
        assert!(
            app.state
                .proxies
                .read()
                .unwrap()
                .is_trusted("10.1.2.3".parse().unwrap())
        );
        assert!(app.state.oauth.signup());
        let (_, providers) = admin.get("/api/auth/providers").await;
        assert_eq!(providers, json!([{"id": "github", "name": "GitHub"}]));

        // Logged, with the secret withheld.
        let (status, body) = admin.get("/api/audit?action=settings.set").await;
        assert_eq!(status, StatusCode::OK);
        let entries = body["entries"].as_array().unwrap();
        let github = entries
            .iter()
            .find(|e| e["target"]["id"] == "auth.github")
            .expect("the github change is logged");
        assert_eq!(github["details"]["value"]["client_secret"], "[redacted]");
        assert_eq!(github["actor"]["username"], "nick");
        assert!(
            entries
                .iter()
                .any(|e| e["target"]["id"] == "engine.workers")
        );

        // One key at a time, and back to the default.
        let (status, body) = admin
            .send(
                Method::PUT,
                "/api/settings/engine.workers",
                Some(json!({"value": 4})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(app.state.engine.config().workers, 4);
        let (status, body) = admin.delete("/api/settings/engine.workers").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["settings"]["engine"]["workers"], 2);
        assert_eq!(app.state.engine.config().workers, 2);
        assert!(
            !body["entries"]
                .as_array()
                .unwrap()
                .iter()
                .any(|e| e["key"] == "engine.workers")
        );
        // Turning a section off.
        let (status, body) = admin
            .send(
                Method::PUT,
                "/api/settings/auth.github",
                Some(json!({"value": null})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["settings"]["auth"]["github"].is_null());
        assert_eq!(body["secrets"], json!([]));
        assert_eq!(admin.get("/api/auth/providers").await.1, json!([]));
    }

    #[tokio::test]
    async fn bad_changes_are_refused_before_anything_is_stored() {
        let app = app_with_admin().await;
        let mut admin = admin(&app).await;
        for (body, message) in [
            (json!({"set": {"engine.workers": 0}}), "at least 1"),
            (json!({"set": {"engine.bogus": 1}}), "unknown field"),
            (json!({"set": {"engine.workers": "many"}}), "invalid type"),
            (
                json!({"set": {"log.level": "not a [filter"}}),
                "tracing filter",
            ),
            (
                json!({"set": {"http.rate_limits.hosts.a.b": 1}}),
                "set as a whole",
            ),
            (json!({"set": {"": 1}}), "dotted settings path"),
            (json!({}), "nothing to set"),
            (json!({"bogus": 1}), "unknown field"),
            (
                json!({"set": {"web.bind": "not an address"}}),
                "invalid socket address",
            ),
            (
                json!({"set": {"web.tls": {"cert": "/nowhere/cert.pem", "key": "/nowhere/key.pem"}}}),
                "could not read",
            ),
            (
                json!({"set": {"engine.cache_dir": "/proc/nowhere/cache"}}),
                "cannot create",
            ),
        ] {
            let (status, answer) = admin
                .send(Method::PATCH, "/api/settings", Some(body.clone()))
                .await;
            assert!(
                status == StatusCode::BAD_REQUEST || status == StatusCode::UNPROCESSABLE_ENTITY,
                "{body}: {status} {answer}"
            );
            let text = answer.to_string();
            assert!(text.contains(message), "{body}: {answer}");
        }
        // Only the directories the test app was given are stored.
        let (_, body) = admin.get("/api/settings").await;
        let stored: Vec<&str> = body["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["key"].as_str().unwrap())
            .collect();
        assert_eq!(stored, vec!["engine.cache_dir", "local.dir"]);
        assert_eq!(app.state.engine.config().workers, 2);

        // An address already in use is refused before it is stored.
        let taken = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = taken.local_addr().unwrap();
        let (status, body) = admin
            .send(
                Method::PUT,
                "/api/settings/web.bind",
                Some(json!({"value": addr.to_string()})),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(body["error"].as_str().unwrap().contains("cannot listen"));
        assert_eq!(app.config().bind, WebConfig::default().bind);
    }

    #[tokio::test]
    async fn settings_import_and_export_as_provisioning_files() {
        let app = app_with_admin().await;
        let mut admin = admin(&app).await;
        let (status, body) = admin
            .post(
                "/api/settings/import",
                json!({
                    "format": "toml",
                    "text": "[engine]\nworkers = 7\n[log]\nlevel = \"warn\"\n[http.proxies.hosts]\n\"a.test\" = \"socks5://p:1\"\n"
                }),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["settings"]["engine"]["workers"], 7);
        assert_eq!(body["settings"]["log"]["level"], "warn");
        assert_eq!(
            body["settings"]["http"]["proxies"]["hosts"]["a.test"],
            "socks5://p:1"
        );
        assert_eq!(app.state.engine.config().workers, 7);
        let (status, body) = admin
            .post(
                "/api/settings/import",
                json!({"format": "toml", "text": "[engine\n"}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let (status, _) = admin
            .post(
                "/api/settings/import",
                json!({"format": "toml", "text": "[engine]\nbogus = 1\n"}),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let (status, body) = admin.get("/api/audit?action=settings.import").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["entries"].as_array().unwrap().len() >= 3);

        for (format, needle) in [
            ("toml", "workers = 7"),
            ("yaml", "workers: 7"),
            ("json", "\"workers\": 7"),
        ] {
            let (status, headers, text) = admin
                .raw(&format!("/api/settings/export?format={format}"))
                .await;
            assert_eq!(status, StatusCode::OK);
            let text = String::from_utf8(text).unwrap();
            assert!(text.contains(needle), "{format}: {text}");
            assert!(text.contains("a.test"), "{format}: {text}");
            assert!(
                headers["content-disposition"]
                    .to_str()
                    .unwrap()
                    .contains(&format!("discoclip.{format}"))
            );
        }
        let (status, _, _) = admin.raw("/api/settings/export?format=ini").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // A viewer is refused.
        app.state
            .users
            .create("viewer", Some("battery staple"), Role::Viewer)
            .await
            .unwrap();
        let mut viewer = Client::new(&app);
        viewer.login("viewer", "battery staple").await;
        assert_eq!(
            viewer.raw("/api/settings/export").await.0,
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn the_listener_moves_when_the_address_or_certificate_changes() {
        let mut settings = Settings::default();
        settings.web.bind = "127.0.0.1:0".parse().unwrap();
        let (app, _) = app_with_settings(
            settings,
            Registry::default(),
            discoclip_bot::DiscordEndpoints::default(),
        )
        .await;
        app.state
            .users
            .set_up("nick", "correct horse")
            .await
            .unwrap();
        let state = app.state.clone();
        let listener = app.bind().await.unwrap();
        let first = listener.local_addr().unwrap();
        let shutdown = tokio_util::sync::CancellationToken::new();
        let server = tokio::spawn(app.run(listener, shutdown.clone()));
        let http = discoclip_engine::reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();
        let response = http
            .get(format!("http://{first}/api/setup"))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);

        // Pick a free port, then ask the app to move there.
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let second = probe.local_addr().unwrap();
        drop(probe);
        let mut sets: BTreeMap<String, Json> = BTreeMap::new();
        sets.insert("web.bind".into(), json!(second.to_string()));
        let current = state.settings.load().await.unwrap();
        let change = crate::settings::Change {
            set: sets,
            reset: Vec::new(),
        };
        let next = state.settings.preview(&change).await.unwrap();
        state.live.check(&current, &next).await.unwrap();
        let changed = state
            .settings
            .apply(
                &crate::audit::Actor::test(),
                crate::audit::Action::SettingsSet,
                &change,
            )
            .await
            .unwrap();
        state.live.apply(&changed.settings).await.unwrap();
        let moved = crate::web::testing::wait_for("the app to listen on the new address", || {
            let http = http.clone();
            async move {
                http.get(format!("http://{second}/api/setup"))
                    .send()
                    .await
                    .ok()
                    .filter(|r| r.status() == 200)
            }
        })
        .await;
        assert_eq!(
            moved.json::<Json>().await.unwrap(),
            json!({"needed": false})
        );
        assert!(
            http.get(format!("http://{first}/api/setup"))
                .send()
                .await
                .is_err()
        );

        // Then over TLS, on the same port.
        let dir = test_dir("tls");
        std::fs::create_dir_all(&dir).unwrap();
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        std::fs::write(dir.join("cert.pem"), cert.cert.pem()).unwrap();
        std::fs::write(dir.join("key.pem"), cert.key_pair.serialize_pem()).unwrap();
        let change = crate::settings::Change::set(
            "web.tls",
            json!({"cert": dir.join("cert.pem"), "key": dir.join("key.pem")}),
        );
        let changed = state
            .settings
            .apply(
                &crate::audit::Actor::test(),
                crate::audit::Action::SettingsSet,
                &change,
            )
            .await
            .unwrap();
        state.live.apply(&changed.settings).await.unwrap();
        let secured = crate::web::testing::wait_for("the app to speak https", || {
            let http = http.clone();
            async move {
                http.get(format!("https://{second}/api/setup"))
                    .send()
                    .await
                    .ok()
                    .filter(|r| r.status() == 200)
            }
        })
        .await;
        assert_eq!(
            secured.headers().get("strict-transport-security").unwrap(),
            "max-age=31536000"
        );
        shutdown.cancel();
        server.await.unwrap().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let _ = Url::parse("https://x/");
    }
}
