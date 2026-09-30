//! The server's health: each part it runs on checked, and summed up.

use std::path::Path;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use discoclip_bot::supervisor::BotState;
use discoclip_engine::StoreError;
use discoclip_engine::store::sqlite::SqliteStore;
use jiff::Timestamp;
use serde::Serialize;

use super::AppState;
use super::auth::Auth;
use super::error::ApiError;

/// How a part is doing. The worst of them is the server's status.
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

/// How long one reading of the health serves `/healthz` before the checks run again.
const HEALTHZ_CACHE: Duration = Duration::from_secs(5);

/// What `/healthz` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Liveness {
    Ok,
    Warn,
    Fail,
    /// The server is shutting down: send no more traffic.
    Stopping,
}

#[derive(Debug, Clone, Serialize)]
pub struct Healthz {
    pub status: Liveness,
    pub version: &'static str,
    pub at: Timestamp,
}

/// The health as `/healthz` reports it: the cached reading when it is fresh, else a new
/// one.
pub async fn read_healthz(state: &AppState) -> Healthz {
    if state.stopping.is_cancelled() {
        return Healthz {
            status: Liveness::Stopping,
            version: env!("CARGO_PKG_VERSION"),
            at: Timestamp::now(),
        };
    }
    let cached = state
        .health_cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .filter(|(read_at, _)| read_at.elapsed() < HEALTHZ_CACHE)
        .map(|(_, health)| health);
    let health = match cached {
        Some(health) => health,
        None => {
            let health = read_health(state).await;
            *state.health_cache.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((Instant::now(), health.clone()));
            health
        }
    };
    Healthz {
        status: match health.status {
            Status::Ok => Liveness::Ok,
            Status::Warn => Liveness::Warn,
            Status::Fail => Liveness::Fail,
        },
        version: health.version,
        at: health.at,
    }
}

/// `GET /healthz`, for load balancers, container health checks and orchestrators: `200`
/// while the server is up and its parts work, `503` when a part has failed or the server
/// is shutting down. Takes no credentials and says nothing beyond the status.
pub async fn healthz(State(state): State<AppState>) -> Response {
    let healthz = read_healthz(&state).await;
    let code = match healthz.status {
        Liveness::Ok | Liveness::Warn => StatusCode::OK,
        Liveness::Fail | Liveness::Stopping => StatusCode::SERVICE_UNAVAILABLE,
    };
    (
        code,
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(healthz),
    )
        .into_response()
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
        encoder(state),
        fonts(state).await,
        bots(state).await,
        fixtures(state).await,
        retention(state),
        backups(state).await,
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
            detail: format!("SQLite answers. {} on disk.", human_bytes(bytes)),
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

/// Which encoders the settings asked for and which run, as the transcoder has them.
fn encoder(state: &AppState) -> Check {
    let set = state.live.ffmpeg.encoders();
    match &set.shortfall {
        Some(shortfall) => Check {
            name: "encoder",
            label: "Video encoder",
            status: Status::Fail,
            detail: shortfall.clone(),
        },
        None => Check {
            name: "encoder",
            label: "Video encoder",
            status: Status::Ok,
            detail: format!("{} ({} chosen)", set.summary(), set.choice.as_str()),
        },
    }
}

/// Whether subtitles can be drawn: a test line rendered with the fonts on this machine.
async fn fonts(state: &AppState) -> Check {
    let dir = state.engine.cache_dir().join("health");
    match state.live.ffmpeg.check_fonts(&dir).await {
        Ok(font) => Check {
            name: "fonts",
            label: "Subtitle fonts",
            status: Status::Ok,
            detail: format!("subtitles render with {font}"),
        },
        Err(error) => Check {
            name: "fonts",
            label: "Subtitle fonts",
            status: Status::Fail,
            detail: format!("subtitles cannot be burnt in: {error}"),
        },
    }
}

/// What the last retention sweep did, and whether it went through.
fn retention(state: &AppState) -> Check {
    let status = state.retention.status();
    let config = state.retention.config();
    match status.last {
        None => Check {
            name: "retention",
            label: "Retention",
            status: Status::Ok,
            detail: format!(
                "no sweep yet. Sweeps run every {}.",
                human_secs(config.sweep_interval_secs)
            ),
        },
        Some(report) => {
            let summary = format!(
                "last sweep {} ago removed {} jobs and freed {}",
                human_secs(uptime_secs(report.at, Timestamp::now())),
                report.jobs_removed + report.failed_removed,
                human_bytes(report.bytes_freed)
            );
            match report.error {
                Some(error) => Check {
                    name: "retention",
                    label: "Retention",
                    status: Status::Warn,
                    detail: format!("{summary}. {error}"),
                },
                None => Check {
                    name: "retention",
                    label: "Retention",
                    status: Status::Ok,
                    detail: summary,
                },
            }
        }
    }
}

/// Whether the database has been backed up lately: off, never, overdue, failed, or fine.
async fn backups(state: &AppState) -> Check {
    let config = state.backups.config();
    if !config.enabled {
        return Check {
            name: "backups",
            label: "Backups",
            status: Status::Ok,
            detail: "backups are turned off".into(),
        };
    }
    let status = state.backups.status();
    let listed = match state.backups.list().await {
        Ok(listed) => listed,
        Err(error) => {
            return Check {
                name: "backups",
                label: "Backups",
                status: Status::Fail,
                detail: format!("backups not listed: {error}"),
            };
        }
    };
    if let Some(error) = status.last_error {
        return Check {
            name: "backups",
            label: "Backups",
            status: Status::Fail,
            detail: format!("the last backup failed: {error}"),
        };
    }
    let Some(newest) = listed.first() else {
        return Check {
            name: "backups",
            label: "Backups",
            status: Status::Ok,
            detail: format!(
                "no backup yet. One is made every {} in {}.",
                human_secs(config.interval_secs),
                config.dir.display()
            ),
        };
    };
    let age = uptime_secs(newest.at, Timestamp::now());
    let detail = format!(
        "{} {} in {}, the newest {} ago, {}",
        listed.len(),
        if listed.len() == 1 {
            "backup"
        } else {
            "backups"
        },
        config.dir.display(),
        human_secs(age),
        human_bytes(newest.bytes)
    );
    Check {
        name: "backups",
        label: "Backups",
        status: if age > config.interval_secs.saturating_mul(2) {
            Status::Warn
        } else {
            Status::Ok
        },
        detail,
    }
}

/// `2 days`, `3 hours`, `15 minutes`, `40 seconds`.
pub fn human_secs(secs: u64) -> String {
    let (value, unit) = if secs >= 2 * 86_400 {
        (secs / 86_400, "day")
    } else if secs >= 2 * 3600 {
        (secs / 3600, "hour")
    } else if secs >= 2 * 60 {
        (secs / 60, "minute")
    } else {
        (secs, "second")
    };
    format!("{value} {unit}{}", if value == 1 { "" } else { "s" })
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
    async fn healthz_answers_without_credentials_and_says_when_stopping() {
        let app = app_with_admin().await;
        let mut client = Client::new(&app);
        let (status, headers, body) = client.raw("/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers["cache-control"], "no-store");
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["status"], "ok", "{body}");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
        assert!(body.get("checks").is_none());
        // A failed part makes the answer 503 once the cached reading has aged out.
        let blocked = crate::web::testing::test_dir("blocked-healthz");
        let mut config = app.state.engine.config();
        config.cache_dir = blocked.clone();
        app.state.engine.reconfigure(config).await.unwrap();
        std::fs::remove_dir_all(&blocked).unwrap();
        std::fs::write(&blocked, b"a file where a directory should be").unwrap();
        let (status, _, _) = client.raw("/healthz").await;
        assert_eq!(status, StatusCode::OK, "the reading is cached for a moment");
        *app.state.health_cache.lock().unwrap() = None;
        let (status, _, body) = client.raw("/healthz").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["status"],
            "fail"
        );
        std::fs::remove_file(&blocked).unwrap();
        app.state.stopping.cancel();
        let (status, _, body) = client.raw("/healthz").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap()["status"],
            "stopping"
        );
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
        assert_eq!(named("encoder")["status"], "ok", "{body}");
        assert!(
            named("encoder")["detail"]
                .as_str()
                .unwrap()
                .contains("software encoding through libx264")
        );
        assert_eq!(named("fonts")["status"], "ok", "{body}");
        assert!(named("fonts")["detail"].as_str().unwrap().contains('/'));
        assert_eq!(named("bots")["detail"], "no Discord applications");
        assert_eq!(
            named("fixtures")["detail"],
            "0 of 1 platform passing, 1 not run yet"
        );
        assert_eq!(named("retention")["status"], "ok");
        assert!(
            named("retention")["detail"]
                .as_str()
                .unwrap()
                .starts_with("no sweep yet")
        );
        assert_eq!(named("backups")["status"], "ok");
        assert!(
            named("backups")["detail"]
                .as_str()
                .unwrap()
                .starts_with("no backup yet")
        );
        app.state.retention.sweep().await;
        app.state.backups.run().await.unwrap();
        let (_, body) = client.get("/api/health").await;
        let checks = body["checks"].as_array().unwrap().clone();
        let named = |name: &str| checks.iter().find(|c| c["name"] == name).unwrap().clone();
        assert!(
            named("retention")["detail"]
                .as_str()
                .unwrap()
                .starts_with("last sweep")
        );
        assert!(
            named("backups")["detail"]
                .as_str()
                .unwrap()
                .starts_with("1 backup in")
        );
        assert_eq!(super::human_secs(86_400 * 3), "3 days");
        assert_eq!(super::human_secs(3600), "60 minutes");
        assert_eq!(super::human_secs(1), "1 second");

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
