//! Platform fixtures: every resolver names public links that stand for what it covers.
//! Running a platform's fixtures resolves each link, without downloading anything, and
//! records what came of it, so the platforms page shows which platforms work right now and
//! when each last passed in full. Runs happen on a schedule and on request from the app.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use discoclip_engine::media::MediaKind;
use discoclip_engine::resolve::{Platform, Resolution, ResolveError, SessionSupport, Tag};
use discoclip_engine::rusqlite::{OptionalExtension, params};
use discoclip_engine::store::sqlite::SqliteStore;
use discoclip_engine::{EngineHandle, StoreError};
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::db::{nanos, timestamp, transact};

/// How many platforms' fixtures run at the same time.
const PARALLEL: usize = 4;
/// How often the schedule looks at whether a run is due.
const TICK: Duration = Duration::from_secs(60);
/// How long after startup the first scheduled run may happen.
const STARTUP_DELAY: Duration = Duration::from_secs(30);

/// How fixtures are run: how often on their own, and how long one link may take.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FixtureConfig {
    /// Every platform's fixtures are run this often; `0` runs them only on request.
    pub interval_secs: u64,
    /// The longest one link may take to resolve before it counts as failed.
    pub timeout_secs: u64,
}

impl Default for FixtureConfig {
    fn default() -> Self {
        Self {
            interval_secs: 24 * 60 * 60,
            timeout_secs: 60,
        }
    }
}

/// The `fixtures` settings as the runner reads them, replaced when they change in the app.
pub type SharedFixtureConfig = Arc<RwLock<FixtureConfig>>;

/// How a fixture's last run went.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FixtureStatus {
    Pass,
    Fail,
    /// The link resolves only with a logged-in session the platform's jar lacks.
    LoginRequired,
    /// The link has not been run.
    Never,
}

/// What a link resolved to: media of a kind with so many playable variants, or a
/// playlist of so many entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Found {
    Media { media: MediaKind, variants: u32 },
    Playlist { entries: u32 },
}

impl Found {
    /// The two columns the result is stored in: a kind name and a count.
    fn columns(self) -> (&'static str, u32) {
        match self {
            Found::Media { media, variants } => (media.as_str(), variants),
            Found::Playlist { entries } => ("playlist", entries),
        }
    }

    fn from_columns(kind: Option<String>, count: Option<u32>) -> Result<Option<Found>, StoreError> {
        let (Some(kind), Some(count)) = (kind, count) else {
            return Ok(None);
        };
        if kind == "playlist" {
            return Ok(Some(Found::Playlist { entries: count }));
        }
        let media = kind
            .parse::<MediaKind>()
            .map_err(|e| StoreError::Corrupt(format!("fixture_results.found_kind: {e}")))?;
        Ok(Some(Found::Media {
            media,
            variants: count,
        }))
    }
}

/// One fixture link and what its last run made of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FixtureResult {
    pub url: String,
    pub status: FixtureStatus,
    pub run_at: Option<Timestamp>,
    /// When the link last resolved, whatever the last run found.
    pub last_pass_at: Option<Timestamp>,
    pub error: Option<String>,
    /// The title the link resolved to, when it did.
    pub title: Option<String>,
    /// What the link resolved to, when it did.
    pub found: Option<Found>,
    pub duration_ms: Option<u64>,
}

/// What a platform's runs add up to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PlatformSummary {
    pub last_run_at: Option<Timestamp>,
    /// When every fixture of the platform last passed in one run.
    pub last_pass_at: Option<Timestamp>,
    /// When a run last had a failing fixture.
    pub last_fail_at: Option<Timestamp>,
    /// How the last run went: links that resolved, links that failed, and links that
    /// resolve only with a login the platform's jar lacks, which is neither.
    pub passed: u32,
    pub failed: u32,
    pub login_required: u32,
}

/// A platform as the platforms page shows it: what its resolver covers, and what its
/// fixtures last found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlatformCoverage {
    pub id: &'static str,
    pub name: &'static str,
    pub hosts: &'static [&'static str],
    pub features: &'static [&'static str],
    pub formats: &'static [&'static str],
    /// What its links resolve to: videos, audio, images or other files.
    pub media: &'static [MediaKind],
    /// What kind of place it is: the presets it belongs to.
    pub tags: &'static [Tag],
    pub session: SessionSupport,
    /// How many cookies the platform's jar holds.
    pub cookies: usize,
    pub fixtures: Vec<FixtureResult>,
    #[serde(flatten)]
    pub summary: PlatformSummary,
    /// A run of the platform's fixtures is in progress.
    pub running: bool,
}

/// What a run of one link found; stored as the link's latest result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub url: String,
    pub ok: bool,
    /// The link was turned away for want of a logged-in session, not broken.
    pub login_required: bool,
    pub error: Option<String>,
    pub title: Option<String>,
    pub found: Option<Found>,
    pub duration: Duration,
}

/// A link's stored result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub url: String,
    pub run_at: Timestamp,
    pub ok: bool,
    pub login_required: bool,
    pub error: Option<String>,
    pub title: Option<String>,
    pub found: Option<Found>,
    pub duration_ms: u64,
    pub last_pass_at: Option<Timestamp>,
}

/// Everything stored about one platform.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stored {
    pub summary: PlatformSummary,
    pub results: Vec<Recorded>,
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("no platform is called {0}")]
    Unknown(String),
    #[error("{0} has no fixtures")]
    NoFixtures(String),
    #[error("no platform has fixtures")]
    Nothing,
    #[error("the fixtures of {0} are already running")]
    Busy(String),
    #[error("every platform's fixtures are already running")]
    AllBusy,
}

/// Fixture results, kept in the application's database.
#[derive(Clone)]
pub struct FixtureStore {
    db: SqliteStore,
}

impl FixtureStore {
    pub fn new(db: SqliteStore) -> Self {
        Self { db }
    }

    /// Everything stored, by platform id.
    pub async fn all(&self) -> Result<BTreeMap<String, Stored>, StoreError> {
        transact(&self.db, |tx| {
            let mut out: BTreeMap<String, Stored> = BTreeMap::new();
            let mut platforms = tx.prepare(
                "SELECT platform, last_run_at, last_pass_at, last_fail_at, passed, failed,
                        login_required
                 FROM fixture_platforms",
            )?;
            let rows = platforms.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, u32>(4)?,
                    row.get::<_, u32>(5)?,
                    row.get::<_, u32>(6)?,
                ))
            })?;
            for row in rows {
                let (platform, run, pass, fail, passed, failed, login_required) = row?;
                out.entry(platform).or_default().summary = PlatformSummary {
                    last_run_at: Some(timestamp("last_run_at", run)?),
                    last_pass_at: pass.map(|n| timestamp("last_pass_at", n)).transpose()?,
                    last_fail_at: fail.map(|n| timestamp("last_fail_at", n)).transpose()?,
                    passed,
                    failed,
                    login_required,
                };
            }
            let mut results = tx.prepare(
                "SELECT platform, url, run_at, ok, login_required, error, title, duration_ms,
                        last_pass_at, found_kind, found_count
                 FROM fixture_results ORDER BY platform, url",
            )?;
            let rows = results.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, bool>(3)?,
                    row.get::<_, bool>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, Option<i64>>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<u32>>(10)?,
                ))
            })?;
            for row in rows {
                let (
                    platform,
                    url,
                    run_at,
                    ok,
                    login_required,
                    error,
                    title,
                    duration_ms,
                    last_pass,
                    found_kind,
                    found_count,
                ) = row?;
                out.entry(platform).or_default().results.push(Recorded {
                    url,
                    run_at: timestamp("run_at", run_at)?,
                    ok,
                    login_required,
                    error,
                    title,
                    found: Found::from_columns(found_kind, found_count)?,
                    duration_ms: duration_ms.max(0) as u64,
                    last_pass_at: last_pass
                        .map(|n| timestamp("last_pass_at", n))
                        .transpose()?,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Stores what a run of `platform`'s fixtures found, as its latest results, and
    /// returns the platform's summary. Results for links the platform no longer names go.
    pub async fn record(
        &self,
        platform: &str,
        finished_at: Timestamp,
        outcomes: Vec<Outcome>,
    ) -> Result<PlatformSummary, StoreError> {
        let platform = platform.to_string();
        transact(&self.db, move |tx| {
            let urls: HashSet<&str> = outcomes.iter().map(|o| o.url.as_str()).collect();
            let stale: Vec<String> = {
                let mut stmt =
                    tx.prepare("SELECT url FROM fixture_results WHERE platform = ?1")?;
                let rows = stmt.query_map(params![platform], |row| row.get::<_, String>(0))?;
                rows.collect::<Result<Vec<_>, _>>()?
                    .into_iter()
                    .filter(|url| !urls.contains(url.as_str()))
                    .collect()
            };
            for url in stale {
                tx.execute(
                    "DELETE FROM fixture_results WHERE platform = ?1 AND url = ?2",
                    params![platform, url],
                )?;
            }
            let at = nanos(finished_at);
            let (mut passed, mut failed, mut login_required) = (0u32, 0u32, 0u32);
            for outcome in &outcomes {
                if outcome.ok {
                    passed += 1;
                } else if outcome.login_required {
                    login_required += 1;
                } else {
                    failed += 1;
                }
                let (found_kind, found_count) = match outcome.found {
                    Some(found) => {
                        let (kind, count) = found.columns();
                        (Some(kind), Some(count))
                    }
                    None => (None, None),
                };
                tx.execute(
                    "INSERT INTO fixture_results
                        (platform, url, run_at, ok, login_required, error, title, duration_ms,
                         last_pass_at, found_kind, found_count)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                     ON CONFLICT(platform, url) DO UPDATE SET
                        run_at = excluded.run_at, ok = excluded.ok,
                        login_required = excluded.login_required, error = excluded.error,
                        title = excluded.title, duration_ms = excluded.duration_ms,
                        last_pass_at = COALESCE(excluded.last_pass_at, fixture_results.last_pass_at),
                        found_kind = excluded.found_kind, found_count = excluded.found_count",
                    params![
                        platform,
                        outcome.url,
                        at,
                        outcome.ok,
                        outcome.login_required && !outcome.ok,
                        outcome.error,
                        outcome.title,
                        outcome.duration.as_millis().min(i64::MAX as u128) as i64,
                        outcome.ok.then_some(at),
                        found_kind,
                        found_count,
                    ],
                )?;
            }
            let previous: Option<(Option<i64>, Option<i64>)> = tx
                .query_row(
                    "SELECT last_pass_at, last_fail_at FROM fixture_platforms WHERE platform = ?1",
                    params![platform],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let (previous_pass, previous_fail) = previous.unwrap_or((None, None));
            // A link that wants a login neither passes nor fails: the platform did not pass
            // in full, and nothing is broken.
            let last_pass_at = if failed == 0 && login_required == 0 && passed > 0 {
                Some(at)
            } else {
                previous_pass
            };
            let last_fail_at = if failed > 0 { Some(at) } else { previous_fail };
            tx.execute(
                "INSERT INTO fixture_platforms
                    (platform, last_run_at, last_pass_at, last_fail_at, passed, failed,
                     login_required)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(platform) DO UPDATE SET
                    last_run_at = excluded.last_run_at, last_pass_at = excluded.last_pass_at,
                    last_fail_at = excluded.last_fail_at, passed = excluded.passed,
                    failed = excluded.failed, login_required = excluded.login_required",
                params![
                    platform,
                    at,
                    last_pass_at,
                    last_fail_at,
                    passed,
                    failed,
                    login_required
                ],
            )?;
            Ok(PlatformSummary {
                last_run_at: Some(finished_at),
                last_pass_at: last_pass_at
                    .map(|n| timestamp("last_pass_at", n))
                    .transpose()?,
                last_fail_at: last_fail_at
                    .map(|n| timestamp("last_fail_at", n))
                    .transpose()?,
                passed,
                failed,
                login_required,
            })
        })
        .await
    }
}

/// Runs platforms' fixtures through the engine's resolvers and keeps the results.
pub struct FixtureRunner {
    engine: EngineHandle,
    store: FixtureStore,
    pub config: SharedFixtureConfig,
    /// The platforms whose fixtures are running right now.
    running: Mutex<HashSet<&'static str>>,
    slots: Arc<Semaphore>,
}

/// Marks a platform as running until the run is over, however it ends.
struct Claim {
    runner: Arc<FixtureRunner>,
    platform: &'static str,
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.runner
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(self.platform);
    }
}

impl FixtureRunner {
    pub fn new(engine: EngineHandle, store: FixtureStore, config: SharedFixtureConfig) -> Self {
        Self {
            engine,
            store,
            config,
            running: Mutex::new(HashSet::new()),
            slots: Arc::new(Semaphore::new(PARALLEL)),
        }
    }

    pub fn config(&self) -> FixtureConfig {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn running(&self) -> HashSet<&'static str> {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn is_running(&self, platform: &str) -> bool {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(platform)
    }

    /// Every registered platform with what its fixtures last found.
    pub async fn coverage(&self) -> Result<Vec<PlatformCoverage>, StoreError> {
        let stored = self.store.all().await?;
        let running = self.running();
        Ok(self
            .engine
            .platforms()
            .into_iter()
            .map(|platform| self.assemble(platform, &stored, &running))
            .collect())
    }

    /// One platform with what its fixtures last found.
    pub async fn platform(&self, id: &str) -> Result<Option<PlatformCoverage>, StoreError> {
        let Some(platform) = self.engine.platforms().into_iter().find(|p| p.id == id) else {
            return Ok(None);
        };
        let stored = self.store.all().await?;
        let running = self.running();
        Ok(Some(self.assemble(platform, &stored, &running)))
    }

    fn assemble(
        &self,
        platform: Platform,
        stored: &BTreeMap<String, Stored>,
        running: &HashSet<&'static str>,
    ) -> PlatformCoverage {
        let record = stored.get(platform.id);
        let fixtures = platform
            .examples
            .iter()
            .map(|url| {
                match record.and_then(|r| r.results.iter().find(|result| result.url == *url)) {
                    Some(result) => FixtureResult {
                        url: url.to_string(),
                        status: if result.ok {
                            FixtureStatus::Pass
                        } else if result.login_required {
                            FixtureStatus::LoginRequired
                        } else {
                            FixtureStatus::Fail
                        },
                        run_at: Some(result.run_at),
                        last_pass_at: result.last_pass_at,
                        error: result.error.clone(),
                        title: result.title.clone(),
                        found: result.found,
                        duration_ms: Some(result.duration_ms),
                    },
                    None => FixtureResult {
                        url: url.to_string(),
                        status: FixtureStatus::Never,
                        run_at: None,
                        last_pass_at: None,
                        error: None,
                        title: None,
                        found: None,
                        duration_ms: None,
                    },
                }
            })
            .collect();
        PlatformCoverage {
            id: platform.id,
            name: platform.name,
            hosts: platform.hosts,
            features: platform.features,
            formats: platform.formats,
            media: platform.media,
            tags: platform.tags,
            session: platform.session,
            cookies: self.engine.http().jar(platform.id).len(),
            fixtures,
            summary: record.map(|r| r.summary.clone()).unwrap_or_default(),
            running: running.contains(platform.id),
        }
    }

    /// Starts a run of `platform`'s fixtures in the background.
    pub fn start(self: &Arc<Self>, platform: &str) -> Result<(), RunError> {
        let found = self
            .engine
            .platforms()
            .into_iter()
            .find(|p| p.id == platform)
            .ok_or_else(|| RunError::Unknown(platform.to_string()))?;
        if found.examples.is_empty() {
            return Err(RunError::NoFixtures(platform.to_string()));
        }
        if !self.claim(found.id) {
            return Err(RunError::Busy(platform.to_string()));
        }
        self.spawn(found);
        Ok(())
    }

    /// Starts a run for every platform with fixtures that is not already running; the
    /// ids started.
    pub fn start_all(self: &Arc<Self>) -> Result<Vec<&'static str>, RunError> {
        let platforms: Vec<Platform> = self
            .engine
            .platforms()
            .into_iter()
            .filter(|p| !p.examples.is_empty())
            .collect();
        if platforms.is_empty() {
            return Err(RunError::Nothing);
        }
        let started = self.start_each(platforms);
        if started.is_empty() {
            return Err(RunError::AllBusy);
        }
        Ok(started)
    }

    /// Starts a run for every platform whose fixtures are due: never run, or run longer
    /// ago than the interval; the ids started.
    pub async fn start_due(self: &Arc<Self>) -> Result<Vec<&'static str>, StoreError> {
        let interval = SignedDuration::from_secs(self.config().interval_secs as i64);
        let stored = self.store.all().await?;
        let now = Timestamp::now();
        let due: Vec<Platform> = self
            .engine
            .platforms()
            .into_iter()
            .filter(|p| !p.examples.is_empty())
            .filter(|p| {
                stored
                    .get(p.id)
                    .and_then(|s| s.summary.last_run_at)
                    .is_none_or(|at| now.duration_since(at) >= interval)
            })
            .collect();
        Ok(self.start_each(due))
    }

    fn start_each(self: &Arc<Self>, platforms: Vec<Platform>) -> Vec<&'static str> {
        let mut started = Vec::new();
        for platform in platforms {
            if self.claim(platform.id) {
                started.push(platform.id);
                self.spawn(platform);
            }
        }
        started
    }

    /// Marks `platform` as running; false when it already was.
    fn claim(&self, platform: &'static str) -> bool {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(platform)
    }

    fn spawn(self: &Arc<Self>, platform: Platform) {
        let claim = Claim {
            runner: self.clone(),
            platform: platform.id,
        };
        let runner = self.clone();
        tokio::spawn(async move {
            let _claim = claim;
            let Ok(_slot) = runner.slots.clone().acquire_owned().await else {
                return;
            };
            match runner.run(&platform).await {
                Ok(summary) => tracing::info!(
                    platform = platform.id,
                    passed = summary.passed,
                    failed = summary.failed,
                    "fixtures run"
                ),
                Err(error) => tracing::error!(
                    platform = platform.id,
                    "fixture results not stored: {error}"
                ),
            }
        });
    }

    /// Resolves each of `platform`'s fixture links and records what came of it.
    async fn run(&self, platform: &Platform) -> Result<PlatformSummary, StoreError> {
        let timeout = Duration::from_secs(self.config().timeout_secs.max(1));
        let mut outcomes = Vec::with_capacity(platform.examples.len());
        for example in platform.examples {
            let began = Instant::now();
            let outcome = match Url::parse(example) {
                Err(error) => Outcome {
                    url: example.to_string(),
                    ok: false,
                    login_required: false,
                    error: Some(format!("not a URL: {error}")),
                    title: None,
                    found: None,
                    duration: began.elapsed(),
                },
                Ok(url) => match tokio::time::timeout(timeout, self.engine.resolve(&url)).await {
                    Err(_) => Outcome {
                        url: example.to_string(),
                        ok: false,
                        login_required: false,
                        error: Some(format!("no answer within {}s", timeout.as_secs())),
                        title: None,
                        found: None,
                        duration: began.elapsed(),
                    },
                    Ok(Err(error)) => Outcome {
                        url: example.to_string(),
                        ok: false,
                        login_required: matches!(error, ResolveError::LoginRequired { .. }),
                        error: Some(error.to_string()),
                        title: None,
                        found: None,
                        duration: began.elapsed(),
                    },
                    Ok(Ok(resolution)) => judge(example, resolution, began.elapsed()),
                },
            };
            tracing::debug!(
                platform = platform.id,
                url = example,
                ok = outcome.ok,
                error = outcome.error.as_deref().unwrap_or(""),
                "fixture resolved"
            );
            outcomes.push(outcome);
        }
        self.store
            .record(platform.id, Timestamp::now(), outcomes)
            .await
    }

    /// Runs the fixtures that are due, on the configured interval, until `shutdown`.
    pub async fn schedule(self: Arc<Self>, shutdown: CancellationToken) {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(STARTUP_DELAY) => {}
        }
        loop {
            if self.config().interval_secs > 0 {
                match self.start_due().await {
                    Ok(started) if !started.is_empty() => {
                        tracing::info!(platforms = ?started, "scheduled fixture run");
                    }
                    Ok(_) => {}
                    Err(error) => tracing::error!("fixture runs not read: {error}"),
                }
            }
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(TICK) => {}
            }
        }
    }
}

/// What a resolution counts as: media with something playable, or a playlist with
/// entries; and what it was, for the page to show what the link resolves to.
fn judge(url: &str, resolution: Resolution, duration: Duration) -> Outcome {
    let (ok, error, title, found) = match resolution {
        Resolution::Media(resolved) => {
            let playable = resolved.variants.iter().filter(|v| v.is_playable()).count();
            if playable > 0 {
                (
                    true,
                    None,
                    resolved.title,
                    Some(Found::Media {
                        media: resolved.media,
                        variants: playable as u32,
                    }),
                )
            } else if let Some(system) = resolved.drm() {
                (
                    false,
                    Some(format!("every variant is locked with {system} DRM")),
                    resolved.title,
                    None,
                )
            } else {
                (
                    false,
                    Some("resolved without any media variant".to_string()),
                    resolved.title,
                    None,
                )
            }
        }
        Resolution::Playlist(playlist) => {
            if playlist.entries.is_empty() {
                (
                    false,
                    Some("the playlist has no entries".to_string()),
                    playlist.title,
                    None,
                )
            } else {
                let entries = playlist.total.unwrap_or(playlist.entries.len()) as u32;
                (
                    true,
                    None,
                    playlist.title,
                    Some(Found::Playlist { entries }),
                )
            }
        }
    };
    Outcome {
        url: url.to_string(),
        ok,
        login_required: false,
        error,
        title,
        found,
        duration,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use discoclip_engine::resolve::{Playlist, PlaylistEntry, Resolved, Variant};

    async fn store() -> FixtureStore {
        let db = SqliteStore::open_in_memory().await.unwrap();
        crate::migrations::apply(&db).await.unwrap();
        FixtureStore::new(db)
    }

    fn outcome(url: &str, ok: bool) -> Outcome {
        Outcome {
            url: url.into(),
            ok,
            login_required: false,
            error: (!ok).then(|| "no video found".to_string()),
            title: ok.then(|| "A clip".to_string()),
            found: ok.then_some(Found::Media {
                media: MediaKind::Video,
                variants: 2,
            }),
            duration: Duration::from_millis(12),
        }
    }

    fn wants_login(url: &str) -> Outcome {
        Outcome {
            url: url.into(),
            ok: false,
            login_required: true,
            error: Some(format!(
                "{url} needs a logged-in p session: posts are read with the sid cookie"
            )),
            title: None,
            found: None,
            duration: Duration::from_millis(12),
        }
    }

    #[tokio::test]
    async fn links_wanting_a_login_neither_pass_nor_fail() {
        let store = store().await;
        let first = Timestamp::from_second(1_700_000_000).unwrap();
        let summary = store
            .record(
                "p",
                first,
                vec![outcome("https://p/a", true), wants_login("https://p/post")],
            )
            .await
            .unwrap();
        assert_eq!(
            (summary.passed, summary.failed, summary.login_required),
            (1, 0, 1)
        );
        assert_eq!(
            summary.last_pass_at, None,
            "the platform did not pass in full"
        );
        assert_eq!(summary.last_fail_at, None, "nothing is broken");
        let stored = store.all().await.unwrap();
        assert_eq!(stored["p"].summary.login_required, 1);
        let post = stored["p"]
            .results
            .iter()
            .find(|r| r.url == "https://p/post")
            .unwrap();
        assert!(!post.ok);
        assert!(post.login_required);
        assert!(post.error.as_ref().unwrap().contains("needs a logged-in"));
        assert_eq!(post.last_pass_at, None);

        // With the session in place the link passes, and the platform with it.
        let second = Timestamp::from_second(1_700_000_600).unwrap();
        let summary = store
            .record(
                "p",
                second,
                vec![
                    outcome("https://p/a", true),
                    outcome("https://p/post", true),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            (summary.passed, summary.failed, summary.login_required),
            (2, 0, 0)
        );
        assert_eq!(summary.last_pass_at, Some(second));
        let stored = store.all().await.unwrap();
        let post = stored["p"]
            .results
            .iter()
            .find(|r| r.url == "https://p/post")
            .unwrap();
        assert!(post.ok && !post.login_required);
        assert_eq!(post.last_pass_at, Some(second));
    }

    #[tokio::test]
    async fn results_keep_the_latest_run_and_when_each_link_last_passed() {
        let store = store().await;
        let first = Timestamp::from_second(1_700_000_000).unwrap();
        let summary = store
            .record(
                "p",
                first,
                vec![outcome("https://p/a", true), outcome("https://p/b", false)],
            )
            .await
            .unwrap();
        assert_eq!(summary.last_run_at, Some(first));
        assert_eq!(summary.last_pass_at, None);
        assert_eq!(summary.last_fail_at, Some(first));
        assert_eq!((summary.passed, summary.failed), (1, 1));

        let second = Timestamp::from_second(1_700_000_600).unwrap();
        let summary = store
            .record(
                "p",
                second,
                vec![outcome("https://p/a", false), outcome("https://p/c", true)],
            )
            .await
            .unwrap();
        assert_eq!(summary.last_pass_at, None);
        assert_eq!(summary.last_fail_at, Some(second));
        let stored = store.all().await.unwrap();
        let results = &stored["p"].results;
        assert_eq!(results.len(), 2, "the link no longer named is gone");
        let a = results.iter().find(|r| r.url == "https://p/a").unwrap();
        assert!(!a.ok);
        assert_eq!(a.run_at, second);
        assert_eq!(a.last_pass_at, Some(first));
        let c = results.iter().find(|r| r.url == "https://p/c").unwrap();
        assert_eq!(c.last_pass_at, Some(second));

        let third = Timestamp::from_second(1_700_001_200).unwrap();
        let summary = store
            .record(
                "p",
                third,
                vec![outcome("https://p/a", true), outcome("https://p/c", true)],
            )
            .await
            .unwrap();
        assert_eq!(summary.last_pass_at, Some(third));
        assert_eq!(summary.last_fail_at, Some(second));
        let summary = store.record("p", third, vec![]).await.unwrap();
        assert_eq!(
            summary.last_pass_at,
            Some(third),
            "a run of nothing passes nothing"
        );
        assert!(store.all().await.unwrap()["p"].results.is_empty());
    }

    #[test]
    fn a_resolution_counts_when_something_can_be_played() {
        let url = Url::parse("https://p/v").unwrap();
        let mut resolved = Resolved::new("p");
        resolved.title = Some("Clip".into());
        let empty = judge("https://p/v", resolved.clone().into(), Duration::ZERO);
        assert!(!empty.ok);
        assert!(empty.error.unwrap().contains("without any media"));
        let mut locked = Variant::dash(url.clone());
        locked.drm = Some("widevine".into());
        resolved.variants.push(locked);
        let drm = judge("https://p/v", resolved.clone().into(), Duration::ZERO);
        assert!(!drm.ok);
        assert!(drm.error.unwrap().contains("widevine"));
        resolved.variants.push(Variant::file(url.clone()));
        let playable = judge("https://p/v", resolved.into(), Duration::ZERO);
        assert!(playable.ok);
        assert_eq!(playable.title.as_deref(), Some("Clip"));
        let playlist = |entries: Vec<PlaylistEntry>| {
            Resolution::Playlist(Playlist {
                resolver: "p".into(),
                id: None,
                title: Some("List".into()),
                entries,
                total: None,
            })
        };
        assert!(!judge("https://p/l", playlist(vec![]), Duration::ZERO).ok);
        assert!(
            judge(
                "https://p/l",
                playlist(vec![PlaylistEntry {
                    url,
                    title: None,
                    duration: None
                }]),
                Duration::ZERO
            )
            .ok
        );
    }
}
