//! Retention: finished jobs past their days and the cache past its size are removed on a
//! schedule, and what the last sweep did is kept for the health and metrics pages.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use discoclip_engine::{EngineHandle, RetentionConfig, StatusKind};
use jiff::{SignedDuration, Timestamp};
use serde::Serialize;
use tokio_util::sync::CancellationToken;

/// How long after startup the first sweep runs.
const STARTUP_DELAY: Duration = Duration::from_secs(30);

/// What one sweep did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SweepReport {
    pub at: Timestamp,
    /// Done jobs removed for their age.
    pub jobs_removed: u64,
    /// Failed and cancelled jobs removed for their age.
    pub failed_removed: u64,
    /// Bytes the cache was trimmed by.
    pub bytes_freed: u64,
    /// What went wrong along the way, when something did. The other steps still ran.
    pub error: Option<String>,
}

/// The sweeps so far.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct RetentionStatus {
    pub last: Option<SweepReport>,
    pub sweeps: u64,
    pub jobs_removed_total: u64,
    pub bytes_freed_total: u64,
}

/// Runs the sweeps and keeps their account.
pub struct Retention {
    engine: EngineHandle,
    status: Mutex<RetentionStatus>,
    /// Held while a sweep runs, so a sweep asked for by hand does not overlap the
    /// scheduled one.
    sweeping: tokio::sync::Mutex<()>,
}

impl Retention {
    pub fn new(engine: EngineHandle) -> Self {
        Self {
            engine,
            status: Mutex::new(RetentionStatus::default()),
            sweeping: tokio::sync::Mutex::new(()),
        }
    }

    pub fn status(&self) -> RetentionStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The retention settings as the engine has them now.
    pub fn config(&self) -> RetentionConfig {
        self.engine.config().retention
    }

    /// Removes what the settings say has been kept long enough, and trims the cache.
    pub async fn sweep(&self) -> SweepReport {
        let _sweeping = self.sweeping.lock().await;
        let config = self.config();
        let now = Timestamp::now();
        let mut report = SweepReport {
            at: now,
            jobs_removed: 0,
            failed_removed: 0,
            bytes_freed: 0,
            error: None,
        };
        let mut problems = Vec::new();
        if config.jobs_days > 0 {
            let before = now - SignedDuration::from_hours(24 * i64::from(config.jobs_days));
            match self.engine.purge(before, &[StatusKind::Done]).await {
                Ok(count) => report.jobs_removed = count as u64,
                Err(error) => problems.push(format!("done jobs not purged: {error}")),
            }
        }
        if config.failed_jobs_days > 0 {
            let before = now - SignedDuration::from_hours(24 * i64::from(config.failed_jobs_days));
            match self
                .engine
                .purge(before, &[StatusKind::Failed, StatusKind::Cancelled])
                .await
            {
                Ok(count) => report.failed_removed = count as u64,
                Err(error) => problems.push(format!("failed jobs not purged: {error}")),
            }
        }
        if config.cache_max_bytes > 0 {
            match self.engine.trim_cache(config.cache_max_bytes).await {
                Ok(freed) => report.bytes_freed = freed,
                Err(error) => problems.push(format!("cache not trimmed: {error}")),
            }
        }
        if !problems.is_empty() {
            report.error = Some(problems.join("; "));
        }
        match &report.error {
            Some(error) => tracing::error!(
                jobs_removed = report.jobs_removed,
                failed_removed = report.failed_removed,
                bytes_freed = report.bytes_freed,
                "retention sweep had problems: {error}"
            ),
            None => tracing::info!(
                jobs_removed = report.jobs_removed,
                failed_removed = report.failed_removed,
                bytes_freed = report.bytes_freed,
                "retention sweep done"
            ),
        }
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        status.sweeps += 1;
        status.jobs_removed_total += report.jobs_removed + report.failed_removed;
        status.bytes_freed_total += report.bytes_freed;
        status.last = Some(report.clone());
        report
    }

    /// Sweeps on the settings' interval until `shutdown`, the first time shortly after
    /// startup. A changed interval takes effect at the next sweep.
    pub async fn schedule(self: Arc<Self>, shutdown: CancellationToken) {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(STARTUP_DELAY) => {}
        }
        loop {
            self.sweep().await;
            let interval = Duration::from_secs(self.config().sweep_interval_secs.max(1));
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(interval) => {}
            }
        }
    }
}
