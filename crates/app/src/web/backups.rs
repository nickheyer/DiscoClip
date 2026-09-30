//! Backups and retention for admins: the backups on disk, one made or removed on request
//! and handed out as a file, and the retention sweeps with one run on request.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde::Serialize;

use super::AppState;
use super::auth::Auth;
use super::error::ApiError;
use super::jobs::serve_file;
use crate::audit::{Action, Target};
use crate::backup::{BackupEntry, BackupError, BackupStatus};
use crate::retention::{RetentionStatus, SweepReport};
use crate::users::Permission;
use discoclip_engine::RetentionConfig;

impl From<BackupError> for ApiError {
    fn from(error: BackupError) -> Self {
        match error {
            BackupError::Disabled => ApiError::Conflict(error.to_string()),
            BackupError::BadName(_) | BackupError::NotFound(_) => ApiError::NotFound,
            BackupError::Busy(_) => ApiError::Conflict(error.to_string()),
            BackupError::Unusable { message, .. } => {
                ApiError::Conflict(format!("This backup cannot be restored: {message}"))
            }
            BackupError::Store(_) | BackupError::Io(_) => ApiError::Internal(error.to_string()),
        }
    }
}

/// The backups on disk and how they are made.
#[derive(Debug, Serialize)]
pub struct BackupsView {
    pub enabled: bool,
    pub dir: String,
    pub interval_secs: u64,
    pub keep: u32,
    pub status: BackupStatus,
    pub backups: Vec<BackupEntry>,
}

async fn view(state: &AppState) -> Result<BackupsView, ApiError> {
    let config = state.backups.config();
    Ok(BackupsView {
        enabled: config.enabled,
        dir: config.dir.display().to_string(),
        interval_secs: config.interval_secs,
        keep: config.keep,
        status: state.backups.status(),
        backups: state.backups.list().await?,
    })
}

pub async fn list(
    State(state): State<AppState>,
    Auth(identity): Auth,
) -> Result<Json<BackupsView>, ApiError> {
    identity.require(Permission::ManageSettings)?;
    Ok(Json(view(&state).await?))
}

pub async fn run(
    State(state): State<AppState>,
    Auth(identity): Auth,
) -> Result<(StatusCode, Json<BackupEntry>), ApiError> {
    identity.require(Permission::ManageSettings)?;
    let entry = state.backups.run().await?;
    state
        .audit
        .record(
            &identity.actor(),
            Action::BackupRun,
            Target::backup(&entry.name),
            serde_json::json!({ "name": entry.name, "bytes": entry.bytes }),
        )
        .await?;
    tracing::info!(
        by = identity.user.username,
        name = entry.name,
        "backup made"
    );
    Ok((StatusCode::CREATED, Json(entry)))
}

pub async fn download(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(name): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    identity.require(Permission::ManageSettings)?;
    let path = state.backups.path_of(&name)?;
    if !tokio::fs::metadata(&path).await.is_ok_and(|m| m.is_file()) {
        return Err(ApiError::NotFound);
    }
    serve_file(&path, &name, false, &headers).await
}

pub async fn delete(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(name): Path<String>,
) -> Result<StatusCode, ApiError> {
    identity.require(Permission::ManageSettings)?;
    state.backups.delete(&name).await?;
    state
        .audit
        .record(
            &identity.actor(),
            Action::BackupDelete,
            Target::backup(&name),
            serde_json::json!({ "name": name }),
        )
        .await?;
    tracing::info!(by = identity.user.username, name, "backup deleted");
    Ok(StatusCode::NO_CONTENT)
}

pub async fn restore(
    State(state): State<AppState>,
    Auth(identity): Auth,
    Path(name): Path<String>,
) -> Result<(StatusCode, Json<crate::restore::RestoreStatus>), ApiError> {
    identity.require(Permission::ManageSettings)?;
    let settings = state.settings.load().await?;
    let status = state
        .backups
        .prepare_restore(&name, identity.actor(), settings)
        .await?;
    tracing::info!(
        by = identity.user.username,
        name,
        "database restore requested"
    );
    // The web listener drains this response before stopping. The composition root
    // owns the restore, so disconnecting the browser cannot cancel it halfway.
    state.stopping.cancel();
    Ok((StatusCode::ACCEPTED, Json(status)))
}

/// The random receipt authorizes only reading progress. It still works after the
/// restored database invalidates the browser's session; it reveals no database data.
pub async fn restore_status(
    State(state): State<AppState>,
    Path(id): Path<uuid::Uuid>,
) -> Result<Json<crate::restore::RestoreStatus>, ApiError> {
    state
        .backups
        .restores
        .status(id)
        .map(Json)
        .ok_or(ApiError::NotFound)
}

/// The retention settings and what the sweeps have done.
#[derive(Debug, Serialize)]
pub struct RetentionView {
    pub config: RetentionConfig,
    pub status: RetentionStatus,
}

pub async fn retention(
    State(state): State<AppState>,
    Auth(identity): Auth,
) -> Result<Json<RetentionView>, ApiError> {
    identity.require(Permission::ManageSettings)?;
    Ok(Json(RetentionView {
        config: state.retention.config(),
        status: state.retention.status(),
    }))
}

pub async fn sweep(
    State(state): State<AppState>,
    Auth(identity): Auth,
) -> Result<Json<SweepReport>, ApiError> {
    identity.require(Permission::ManageSettings)?;
    let report = state.retention.sweep().await;
    tracing::info!(by = identity.user.username, "retention sweep asked for");
    Ok(Json(report))
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::Value as Json;

    use crate::web::testing::{Client, app_with_admin};

    #[tokio::test]
    async fn backup_restore_requires_admin_and_issues_a_receipt_that_survives_logout() {
        let app = app_with_admin().await;
        let mut client = Client::new(&app);
        let route = "/api/backups/discoclip-20200101T000000Z.db/restore";
        assert_eq!(
            client.post(route, Json::Null).await.0,
            StatusCode::UNAUTHORIZED
        );
        client.login("nick", "correct horse").await;
        let (status, user) = client
            .post(
                "/api/users",
                serde_json::json!({
                    "username": "viewer", "password": "viewer password", "role": "viewer"
                }),
            )
            .await;
        assert_eq!(status, StatusCode::CREATED, "{user}");
        let mut viewer = Client::new(&app);
        viewer.login("viewer", "viewer password").await;
        assert_eq!(
            viewer.post(route, Json::Null).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            client.post(route, Json::Null).await.0,
            StatusCode::NOT_FOUND
        );
        assert!(!app.state.stopping.is_cancelled());

        let (_, made) = client.post("/api/backups", Json::Null).await;
        let name = made["name"].as_str().unwrap();
        let (status, body) = client
            .post(&format!("/api/backups/{name}/restore"), Json::Null)
            .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert_eq!(body["phase"], "stopping");
        assert!(app.state.stopping.is_cancelled());
        let mut signed_out = Client::new(&app);
        let (status, receipt) = signed_out
            .get(&format!(
                "/api/backups/restore/{}",
                body["id"].as_str().unwrap()
            ))
            .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(receipt, body);
        assert_eq!(
            signed_out
                .get(&format!("/api/backups/restore/{}", uuid::Uuid::new_v4()))
                .await
                .0,
            StatusCode::NOT_FOUND
        );
        let pending = app.state.backups.restores.take_pending().unwrap();
        let staged = discoclip_engine::rusqlite::Connection::open(&pending.staged).unwrap();
        let sessions: i64 = staged
            .query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            sessions, 0,
            "old sessions must not become valid again after restore"
        );
        drop(staged);
        drop(pending);
    }

    #[tokio::test]
    async fn backups_are_made_listed_downloaded_and_removed() {
        let app = app_with_admin().await;
        let mut client = Client::new(&app);
        assert_eq!(client.get("/api/backups").await.0, StatusCode::UNAUTHORIZED);
        client.login("nick", "correct horse").await;
        let (status, body) = client.get("/api/backups").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["enabled"], true);
        assert_eq!(body["backups"].as_array().unwrap().len(), 0);
        assert_eq!(body["status"]["runs"], 0);

        let (status, made) = client.post("/api/backups", Json::Null).await;
        assert_eq!(status, StatusCode::CREATED, "{made}");
        let name = made["name"].as_str().unwrap().to_string();
        assert!(name.starts_with("discoclip-") && name.ends_with(".db"));
        assert!(made["bytes"].as_u64().unwrap() > 0);
        let (status, body) = client.get("/api/backups").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["backups"][0]["name"], name);
        assert_eq!(body["status"]["runs"], 1);
        assert_eq!(body["status"]["last_error"], Json::Null);

        let (status, headers, bytes) = client.raw(&format!("/api/backups/{name}")).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            headers["content-disposition"]
                .to_str()
                .unwrap()
                .contains(&name)
        );
        assert!(bytes.starts_with(b"SQLite format 3"));
        let (status, _) = client.delete("/api/backups/../secret.key").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = client
            .delete("/api/backups/discoclip-20200101T000000Z.db")
            .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _) = client.delete(&format!("/api/backups/{name}")).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, body) = client.get("/api/backups").await;
        assert_eq!(body["backups"].as_array().unwrap().len(), 0);

        let (status, body) = client.get("/api/audit?action=backup.run").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["entries"][0]["target"]["kind"], "backup");
        assert_eq!(body["entries"][0]["target"]["id"], name);

        // Retention: the settings and a sweep on request.
        let (status, body) = client.get("/api/retention").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["config"]["jobs_days"], 90);
        assert_eq!(body["status"]["sweeps"], 0);
        let (status, report) = client.post("/api/retention/sweep", Json::Null).await;
        assert_eq!(status, StatusCode::OK, "{report}");
        assert_eq!(report["jobs_removed"], 0);
        assert_eq!(report["error"], Json::Null);
        let (_, body) = client.get("/api/retention").await;
        assert_eq!(body["status"]["sweeps"], 1);
    }
}
