//! What the server is doing, in numbers: the process and the machine it runs on, the
//! jobs, the HTTP client, the bots, the cache, the database, the fixtures and the log.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::State;
use discoclip_engine::engine::dir_size;
use jiff::Timestamp;
use serde::Serialize;
use sysinfo::{Disks, MemoryRefreshKind, Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use super::AppState;
use super::auth::Auth;
use super::error::ApiError;
use super::health::{bot_state_name, database_bytes, uptime_secs};
use super::jobs::{JobStats, read_stats};
use crate::telemetry::LOG_CAPACITY;

/// Reads the process and the machine; kept so CPU use is measured between two reads.
pub struct Sampler {
    system: Mutex<System>,
    pid: Pid,
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sampler {
    pub fn new() -> Self {
        Self {
            system: Mutex::new(System::new()),
            pid: sysinfo::get_current_pid().expect("the process has an id"),
        }
    }

    /// The process and the machine as they stand; the process is absent when the OS does
    /// not list it.
    pub fn sample(
        &self,
        dirs: &[(&'static str, PathBuf)],
    ) -> (Option<ProcessMetrics>, SystemMetrics) {
        let mut system = self.system.lock().unwrap_or_else(|e| e.into_inner());
        system.refresh_memory_specifics(MemoryRefreshKind::everything());
        system.refresh_cpu_list(sysinfo::CpuRefreshKind::nothing());
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&[self.pid]),
            true,
            ProcessRefreshKind::nothing().with_memory().with_cpu(),
        );
        let process = system.process(self.pid).map(|process| ProcessMetrics {
            pid: self.pid.as_u32(),
            rss_bytes: process.memory(),
            virtual_bytes: process.virtual_memory(),
            cpu_percent: process.cpu_usage(),
            run_time_secs: process.run_time(),
        });
        let load = System::load_average();
        let machine = SystemMetrics {
            total_memory_bytes: system.total_memory(),
            available_memory_bytes: system.available_memory(),
            load_average: [load.one, load.five, load.fifteen],
            cpus: system.cpus().len(),
            disks: disks_holding(dirs),
        };
        (process, machine)
    }
}

/// The disks that hold the server's directories, each with what it holds.
fn disks_holding(dirs: &[(&'static str, PathBuf)]) -> Vec<DiskMetrics> {
    let disks = Disks::new_with_refreshed_list();
    let mut out: Vec<DiskMetrics> = Vec::new();
    for (name, dir) in dirs {
        let resolved = existing_ancestor(dir);
        let Some(disk) = disks
            .iter()
            .filter(|disk| resolved.starts_with(disk.mount_point()))
            .max_by_key(|disk| disk.mount_point().as_os_str().len())
        else {
            continue;
        };
        let mount = disk.mount_point().display().to_string();
        match out.iter_mut().find(|d| d.mount == mount) {
            Some(entry) => entry.holds.push(name),
            None => out.push(DiskMetrics {
                mount,
                total_bytes: disk.total_space(),
                available_bytes: disk.available_space(),
                holds: vec![name],
            }),
        }
    }
    out
}

/// `path` made absolute, up to its deepest ancestor that exists.
fn existing_ancestor(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
    let mut candidate = absolute.as_path();
    loop {
        if let Ok(real) = candidate.canonicalize() {
            return real;
        }
        match candidate.parent() {
            Some(parent) => candidate = parent,
            None => return absolute,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ProcessMetrics {
    pub pid: u32,
    pub rss_bytes: u64,
    pub virtual_bytes: u64,
    /// Of one CPU, since the previous reading.
    pub cpu_percent: f32,
    pub run_time_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SystemMetrics {
    pub total_memory_bytes: u64,
    pub available_memory_bytes: u64,
    /// Over one, five and fifteen minutes.
    pub load_average: [f64; 3],
    pub cpus: usize,
    pub disks: Vec<DiskMetrics>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct DiskMetrics {
    pub mount: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    /// Which of the server's directories the disk holds: `cache`, `data`, `local`, `archive`.
    pub holds: Vec<&'static str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RequestCount {
    pub host: String,
    pub status: u16,
    pub count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HttpMetrics {
    pub requests: Vec<RequestCount>,
    pub retries: u64,
    pub rate_limit_waits: u64,
    pub bytes_received: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BotMetrics {
    pub applications: usize,
    pub by_state: BTreeMap<&'static str, usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CacheMetrics {
    pub dir: String,
    pub bytes: u64,
    /// Job directories kept.
    pub jobs: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DatabaseMetrics {
    pub path: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FixtureMetrics {
    pub platforms: usize,
    pub with_fixtures: usize,
    pub passing: usize,
    pub failing: usize,
    pub never: usize,
    pub running: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LogMetrics {
    pub buffered: usize,
    pub capacity: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct Metrics {
    pub at: Timestamp,
    pub version: &'static str,
    pub started_at: Timestamp,
    pub uptime_secs: u64,
    pub process: Option<ProcessMetrics>,
    pub system: SystemMetrics,
    pub jobs: JobStats,
    pub http: HttpMetrics,
    pub bots: BotMetrics,
    pub cache: CacheMetrics,
    pub database: DatabaseMetrics,
    pub fixtures: FixtureMetrics,
    pub logs: LogMetrics,
}

pub async fn get(State(state): State<AppState>, Auth(_): Auth) -> Result<Json<Metrics>, ApiError> {
    Ok(Json(read_metrics(&state).await?))
}

pub async fn read_metrics(state: &AppState) -> Result<Metrics, ApiError> {
    let now = Timestamp::now();
    let engine_config = state.engine.config();
    let local_dir = state
        .live
        .local
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .dir
        .clone();
    let mut dirs = vec![
        ("cache", engine_config.cache_dir.clone()),
        ("data", state.data_dir.clone()),
        ("local", local_dir),
    ];
    if let Some(archive) = &engine_config.archive {
        dirs.push(("archive", archive.dir.clone()));
    }
    let sampler = Arc::clone(&state.sampler);
    let (process, system) = tokio::task::spawn_blocking(move || sampler.sample(&dirs))
        .await
        .map_err(|e| ApiError::Internal(format!("sampling the process failed: {e}")))?;

    let jobs = read_stats(state).await?;
    let http = state.engine.http().stats();
    let statuses = state.bots.statuses().await;
    let mut by_state: BTreeMap<&'static str, usize> = BTreeMap::new();
    for status in statuses.values() {
        *by_state.entry(bot_state_name(&status.state)).or_default() += 1;
    }
    let jobs_dir = engine_config.cache_dir.join("jobs");
    let cached_jobs = match tokio::fs::read_dir(&jobs_dir).await {
        Ok(mut entries) => {
            let mut count = 0;
            while let Ok(Some(_)) = entries.next_entry().await {
                count += 1;
            }
            count
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => {
            return Err(ApiError::Internal(format!(
                "reading {}: {error}",
                jobs_dir.display()
            )));
        }
    };
    let platforms = state.fixtures.coverage().await?;
    let with: Vec<_> = platforms
        .iter()
        .filter(|p| !p.fixtures.is_empty())
        .collect();
    let never = with
        .iter()
        .filter(|p| p.summary.last_run_at.is_none())
        .count();
    let failing = with
        .iter()
        .filter(|p| p.summary.last_run_at.is_some() && p.summary.failed > 0)
        .count();
    Ok(Metrics {
        at: now,
        version: env!("CARGO_PKG_VERSION"),
        started_at: state.started_at,
        uptime_secs: uptime_secs(state.started_at, now),
        process,
        system,
        jobs,
        http: HttpMetrics {
            requests: http
                .requests
                .into_iter()
                .map(|(host, status, count)| RequestCount {
                    host,
                    status,
                    count,
                })
                .collect(),
            retries: http.retries,
            rate_limit_waits: http.rate_limit_waits,
            bytes_received: http.bytes_received,
        },
        bots: BotMetrics {
            applications: statuses.len(),
            by_state,
        },
        cache: CacheMetrics {
            dir: engine_config.cache_dir.display().to_string(),
            bytes: dir_size(&jobs_dir).await,
            jobs: cached_jobs,
        },
        database: DatabaseMetrics {
            path: state.data_dir.join("discoclip.db").display().to_string(),
            bytes: database_bytes(&state.db).await?,
        },
        fixtures: FixtureMetrics {
            platforms: platforms.len(),
            with_fixtures: with.len(),
            passing: with.len() - never - failing,
            failing,
            never,
            running: with.iter().filter(|p| p.running).count(),
        },
        logs: LogMetrics {
            buffered: state.live.log.buffer.len(),
            capacity: LOG_CAPACITY,
        },
    })
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use crate::web::testing::{Client, app_with_admin};

    #[tokio::test]
    async fn the_server_is_measured() {
        let app = app_with_admin().await;
        let mut client = Client::new(&app);
        assert_eq!(client.get("/api/metrics").await.0, StatusCode::UNAUTHORIZED);
        client.login("nick", "correct horse").await;
        let (status, body) = client.get("/api/metrics").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["process"]["rss_bytes"].as_u64().unwrap() > 0, "{body}");
        assert!(body["system"]["total_memory_bytes"].as_u64().unwrap() > 0);
        assert!(body["system"]["cpus"].as_u64().unwrap() >= 1);
        let disks = body["system"]["disks"].as_array().unwrap();
        assert!(!disks.is_empty(), "{body}");
        let holds: Vec<&str> = disks
            .iter()
            .flat_map(|d| d["holds"].as_array().unwrap().iter())
            .map(|h| h.as_str().unwrap())
            .collect();
        assert!(holds.contains(&"cache"), "{holds:?}");
        assert!(holds.contains(&"data"), "{holds:?}");
        assert_eq!(body["jobs"]["counts"]["queued"], 0);
        assert_eq!(body["http"]["retries"], 0);
        assert_eq!(body["bots"]["applications"], 0);
        assert_eq!(body["cache"]["jobs"], 0);
        assert!(body["database"]["bytes"].as_u64().unwrap() > 0);
        assert_eq!(body["fixtures"]["with_fixtures"], 1);
        assert_eq!(body["fixtures"]["never"], 1);
        assert_eq!(body["logs"]["capacity"], crate::telemetry::LOG_CAPACITY);
        // The next reading has the process again, with CPU use measured since the first.
        let (status, body) = client.get("/api/metrics").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["process"]["cpu_percent"].is_number());
    }
}
