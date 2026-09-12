//! The server's health: each part it runs on checked, and summed up.

use std::path::Path;

use axum::Json;
use axum::extract::State;
use discoclip_bot::supervisor::BotState;
use discoclip_engine::StoreError;
use discoclip_engine::store::sqlite::SqliteStore;
use jiff::Timestamp;
use serde::Serialize;

use super::AppState;
use super::auth::Auth;
use super::error::ApiError;

/// How a part is doing; the worst of them is the server's status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ok,
    /// Working, with something to look at.
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub label: &'static str,
    pub status: Status,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Health {
    pub status: Status,
    pub version: &'static str,
    pub started_at: Timestamp,
    pub uptime_secs: u64,
    pub at: Timestamp,
    pub checks: Vec<Check>,
}

pub async fn get(State(state): State<AppState>, Auth(_): Auth) -> Result<Json<Health>, ApiError> {
    Ok(Json(read_health(&state).await))
}

pub async fn read_health(state: &AppState) -> Health {
    let now = Timestamp::now();
    let local_dir = state
        .live
        .local
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .dir
        .clone();
    let checks = vec![
        database(&state.db).await,
        engine(state),
        directory("cache", "Cache directory", &state.engine.cache_dir()).await,
        directory("local", "Local publishing directory", &local_dir).await,
        ffmpeg(state).await,
        bots(state).await,
        fixtures(state).await,
    ];
    let status = checks.iter().map(|c| c.status).max().unwrap_or(Status::Ok);
    Health {
        status,
        version: env!("CARGO_PKG_VERSION"),
        started_at: state.started_at,
        uptime_secs: uptime_secs(state.started_at, now),
        at: now,
        checks,
    }
}

/// Whole seconds from `started_at` to `now`.
pub fn uptime_secs(started_at: Timestamp, now: Timestamp) -> u64 {
    now.duration_since(started_at).as_secs().max(0) as u64
}

/// The database file's size as SQLite counts it.
pub async fn database_bytes(db: &SqliteStore) -> Result<u64, StoreError> {
    db.call(|conn| {
        conn.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))?;
        let pages: i64 = conn.query_row("PRAGMA page_count", [], |row| row.get(0))?;
        let page_size: i64 = conn.query_row("PRAGMA page_size", [], |row| row.get(0))?;
        Ok((pages.max(0) as u64) * (page_size.max(0) as u64))
    })
    .await
}

/// `1.2 GB`, `640 KB`, `12 B`.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

async fn database(db: &SqliteStore) -> Check {
    match database_bytes(db).await {
        Ok(bytes) => Check {
            name: "database",
            label: "Database",
            status: Status::Ok,
            detail: format!("SQLite answers; {} on disk", human_bytes(bytes)),
        },
        Err(error) => Check {
            name: "database",
            label: "Database",
            status: Status::Fail,
            detail: format!("SQLite does not answer: {error}"),
        },
    }
}

fn engine(state: &AppState) -> Check {
    let load = state.engine.utilisation();
    Check {
        name: "engine",
        label: "Job engine",
        status: Status::Ok,
        detail: format!(
            "{} {}, {} busy, {} waiting",
            load.workers,
            if load.workers == 1 {
                "worker"
            } else {
                "workers"
            },
            load.active,
            load.waiting
        ),
    }
}

/// Whether a file can be written under `dir`, creating it when it is missing.
async fn directory(name: &'static str, label: &'static str, dir: &Path) -> Check {
    let probe = dir.join(format!(".health-{}", uuid::Uuid::now_v7()));
    let written = async {
        tokio::fs::create_dir_all(dir).await?;
        tokio::fs::write(&probe, b"").await?;
        tokio::fs::remove_file(&probe).await
    }
    .await;
    match written {
        Ok(()) => Check {
            name,
            label,
            status: Status::Ok,
            detail: format!("writable at {}", dir.display()),
        },
        Err(error) => Check {
            name,
            label,
            status: Status::Fail,
            detail: format!("cannot write to {}: {error}", dir.display()),
        },
    }
}

async fn ffmpeg(state: &AppState) -> Check {
    match state.live.ffmpeg.version().await {
        Ok(version) => Check {
            name: "ffmpeg",
            label: "FFmpeg",
            status: Status::Ok,
            detail: version,
        },
        Err(error) => Check {
            name: "ffmpeg",
            label: "FFmpeg",
            status: Status::Fail,
            detail: format!("ffmpeg does not run: {error}"),
        },
    }
}

/// The name each bot state is counted under.
pub fn bot_state_name(state: &BotState) -> &'static str {
    match state {
        BotState::Disabled => "disabled",
        BotState::Stopped => "stopped",
        BotState::Starting => "starting",
        BotState::Connected { .. } => "connected",
        BotState::Retrying { .. } => "retrying",
        BotState::Failed { .. } => "failed",
    }
}

async fn bots(state: &AppState) -> Check {
    let statuses = state.bots.statuses().await;
    if statuses.is_empty() {
        return Check {
            name: "bots",
            label: "Discord bots",
            status: Status::Ok,
            detail: "no Discord applications".into(),
        };
    }
    let mut counts: std::collections::BTreeMap<&'static str, usize> = Default::default();
    for status in statuses.values() {
        *counts.entry(bot_state_name(&status.state)).or_default() += 1;
    }
    let count = |name: &str| counts.get(name).copied().unwrap_or(0);
    let status = if count("failed") > 0 {
        Status::Fail
    } else if count("retrying") > 0 || count("disabled") > 0 {
        Status::Warn
    } else {
        Status::Ok
    };
    let detail = [
        "connected",
        "starting",
        "retrying",
        "stopped",
        "disabled",
        "failed",
    ]
    .into_iter()
    .filter(|name| count(name) > 0)
    .map(|name| format!("{} {name}", count(name)))
    .collect::<Vec<_>>()
    .join(", ");
    Check {
        name: "bots",
        label: "Discord bots",
        status,
        detail,
    }
}

async fn fixtures(state: &AppState) -> Check {
    match state.fixtures.coverage().await {
        Err(error) => Check {
            name: "fixtures",
            label: "Platform fixtures",
            status: Status::Fail,
            detail: format!("fixture results not read: {error}"),
        },
        Ok(platforms) => {
            let with: Vec<_> = platforms
                .iter()
                .filter(|p| !p.fixtures.is_empty())
                .collect();
            if with.is_empty() {
                return Check {
                    name: "fixtures",
                    label: "Platform fixtures",
                    status: Status::Ok,
                    detail: "no platform has fixtures".into(),
                };
            }
            let never = with
                .iter()
                .filter(|p| p.summary.last_run_at.is_none())
                .count();
            let failing = with
                .iter()
                .filter(|p| p.summary.last_run_at.is_some() && p.summary.failed > 0)
                .count();
            let passing = with.len() - never - failing;
            let running = with.iter().filter(|p| p.running).count();
            let mut detail = format!(
                "{passing} of {} {} passing",
                with.len(),
                if with.len() == 1 {
                    "platform"
                } else {
                    "platforms"
                }
            );
            if failing > 0 {
                detail.push_str(&format!(", {failing} failing"));
            }
            if never > 0 {
                detail.push_str(&format!(", {never} not run yet"));
            }
            if running > 0 {
                detail.push_str(&format!(", {running} running now"));
            }
            Check {
                name: "fixtures",
                label: "Platform fixtures",
                status: if failing > 0 {
                    Status::Warn
                } else {
                    Status::Ok
                },
                detail,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::human_bytes;
    use crate::web::testing::{Client, app_with_admin};

    #[test]
    fn bytes_read_at_a_glance() {
        assert_eq!(human_bytes(12), "12 B");
        assert_eq!(human_bytes(640 * 1024), "640 KB");
        assert_eq!(human_bytes(1_288_490_189), "1.2 GB");
    }

    #[tokio::test]
    async fn every_part_is_checked_and_summed_up() {
        let app = app_with_admin().await;
        let mut client = Client::new(&app);
        assert_eq!(client.get("/api/health").await.0, StatusCode::UNAUTHORIZED);
        client.login("nick", "correct horse").await;
        let (status, body) = client.get("/api/health").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "ok", "{body}");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        assert!(body["uptime_secs"].is_u64());
        let checks = body["checks"].as_array().unwrap();
        let named = |name: &str| checks.iter().find(|c| c["name"] == name).unwrap().clone();
        assert!(
            named("database")["detail"]
                .as_str()
                .unwrap()
                .contains("SQLite answers")
        );
        assert!(named("engine")["detail"].as_str().unwrap().contains("busy"));
        assert_eq!(named("cache")["status"], "ok");
        assert_eq!(named("local")["status"], "ok");
        assert!(
            named("ffmpeg")["detail"]
                .as_str()
                .unwrap()
                .starts_with("ffmpeg version")
        );
        assert_eq!(named("bots")["detail"], "no Discord applications");
        assert_eq!(
            named("fixtures")["detail"],
            "0 of 1 platform passing, 1 not run yet"
        );

        // A cache directory nothing can be written to fails the check and the server.
        let blocked = crate::web::testing::test_dir("blocked");
        let mut config = app.state.engine.config();
        config.cache_dir = blocked.clone();
        app.state.engine.reconfigure(config).await.unwrap();
        std::fs::remove_dir_all(&blocked).unwrap();
        std::fs::write(&blocked, b"a file where a directory should be").unwrap();
        let (_, body) = client.get("/api/health").await;
        assert_eq!(body["status"], "fail", "{body}");
        let cache = body["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "cache")
            .unwrap()
            .clone();
        assert_eq!(cache["status"], "fail");
        assert!(cache["detail"].as_str().unwrap().contains("cannot write"));
        std::fs::remove_file(&blocked).unwrap();
    }
}
