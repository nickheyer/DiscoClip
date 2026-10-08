//! Platform checks: whether each platform's links still resolve. Every platform is
//! checked with links kept in the database: the ones its resolver ships with, ones added
//! by hand, and the newest links of jobs that finished on it, so real use keeps the set
//! fresh as hard-coded links go stale. A run tries the platform's links in turn, the one
//! that passed most recently first, and stops at the first that resolves. A link that
//! fails while another resolves is a dead link, not a broken platform, and is switched
//! off quietly. A platform is working when a link resolved, or when a job finished on it
//! since the last check. Nothing here turns a platform on or off: profiles do that.

use std::collections::HashSet;
use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use discoclip_engine::media::MediaKind;
use discoclip_engine::resolve::{Platform, Resolution, ResolveError, SessionSupport, Tag};
use discoclip_engine::rusqlite::{OptionalExtension, Row, params};
use discoclip_engine::store::sqlite::SqliteStore;
use discoclip_engine::{EngineHandle, StoreError};
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use url::Url;
use uuid::Uuid;

use crate::db::{nanos, timestamp, transact};

/// How many platforms' checks run at the same time.
const PARALLEL: usize = 4;
/// How often the schedule looks at whether a run is due.
const TICK: Duration = Duration::from_secs(60);
/// How long after startup the first scheduled run may happen.
const STARTUP_DELAY: Duration = Duration::from_secs(30);
/// How many links learned from jobs a platform keeps, the newest staying.
pub const LEARNED_LINKS: usize = 3;

/// How checks are run: how often on their own, and how long one link may take.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FixtureConfig {
    /// Every platform's links are checked this often. `0` checks them only on request.
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

/// How a link's last run went.
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

/// Where a check link came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkOrigin {
    /// Shipped with the platform's resolver.
    Builtin,
    /// Added by hand.
    Custom,
    /// The link of a job that finished on the platform.
    Job,
}

impl LinkOrigin {
    fn as_str(self) -> &'static str {
        match self {
            LinkOrigin::Builtin => "builtin",
            LinkOrigin::Custom => "custom",
            LinkOrigin::Job => "job",
        }
    }

    fn parse(text: &str) -> Result<Self, StoreError> {
        match text {
            "builtin" => Ok(LinkOrigin::Builtin),
            "custom" => Ok(LinkOrigin::Custom),
            "job" => Ok(LinkOrigin::Job),
            other => Err(StoreError::Corrupt(format!(
                "fixture_links.origin: {other:?}"
            ))),
        }
    }
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

/// One link a platform is checked with, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FixtureLink {
    pub id: Uuid,
    pub platform: String,
    pub url: String,
    pub origin: LinkOrigin,
    /// Whether runs try the link. A link switched off on its own carries the reason.
    pub enabled: bool,
    /// What the link failed with while another link of the platform resolved, when that
    /// is why it is off.
    pub disabled_reason: Option<String>,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// One check link and what its last run made of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FixtureResult {
    pub id: Uuid,
    pub url: String,
    pub origin: LinkOrigin,
    pub enabled: bool,
    pub disabled_reason: Option<String>,
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

/// Whether a platform works, as far as anything shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformHealth {
    /// A link resolved at the last check, or a job finished on the platform since.
    Working,
    /// Every link tried at the last check failed, and no job has finished since.
    Failing,
    /// Every link tried at the last check wanted a login the platform's jar lacks.
    LoginRequired,
    /// Nothing has been checked and no job has finished.
    Unknown,
}

/// What a platform's links and jobs add up to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlatformSummary {
    /// When a link of the platform was last run.
    pub last_run_at: Option<Timestamp>,
    /// When a link of the platform last resolved.
    pub last_pass_at: Option<Timestamp>,
    /// When a job last finished on the platform: real use, which proves it works.
    pub last_job_at: Option<Timestamp>,
    /// Of the links in use, how many last resolved, failed, or wanted a login.
    pub passed: u32,
    pub failed: u32,
    pub login_required: u32,
    pub health: PlatformHealth,
}

impl Default for PlatformSummary {
    fn default() -> Self {
        Self {
            last_run_at: None,
            last_pass_at: None,
            last_job_at: None,
            passed: 0,
            failed: 0,
            login_required: 0,
            health: PlatformHealth::Unknown,
        }
    }
}

/// A platform as the platforms page shows it: what its resolver covers, and what its
/// links and jobs show of it.
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
    /// Whether profiles take the platform's links without naming it
    pub on_by_default: bool,
    pub fixtures: Vec<FixtureResult>,
    #[serde(flatten)]
    pub summary: PlatformSummary,
    /// A run of the platform's links is in progress.
    pub running: bool,
}

/// What a run of one link found. Stored as the link's latest result.
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
    pub run_at: Timestamp,
    pub ok: bool,
    pub login_required: bool,
    pub error: Option<String>,
    pub title: Option<String>,
    pub found: Option<Found>,
    pub duration_ms: u64,
    pub last_pass_at: Option<Timestamp>,
}

/// A link with its latest result, when it has been run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkRow {
    pub link: FixtureLink,
    pub result: Option<Recorded>,
}

impl LinkRow {
    fn status(&self) -> FixtureStatus {
        match &self.result {
            None => FixtureStatus::Never,
            Some(result) if result.ok => FixtureStatus::Pass,
            Some(result) if result.login_required => FixtureStatus::LoginRequired,
            Some(_) => FixtureStatus::Fail,
        }
    }

    fn view(&self) -> FixtureResult {
        FixtureResult {
            id: self.link.id,
            url: self.link.url.clone(),
            origin: self.link.origin,
            enabled: self.link.enabled,
            disabled_reason: self.link.disabled_reason.clone(),
            status: self.status(),
            run_at: self.result.as_ref().map(|r| r.run_at),
            last_pass_at: self.result.as_ref().and_then(|r| r.last_pass_at),
            error: self.result.as_ref().and_then(|r| r.error.clone()),
            title: self.result.as_ref().and_then(|r| r.title.clone()),
            found: self.result.as_ref().and_then(|r| r.found),
            duration_ms: self.result.as_ref().map(|r| r.duration_ms),
        }
    }
}

/// Everything stored about one platform's links.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stored {
    pub links: Vec<LinkRow>,
}

/// What a platform's links and its newest finished job add up to. Only links in use count
/// towards the verdict: one switched off failed while another resolved, which says nothing
/// against the platform.
pub fn summarize(links: &[LinkRow], last_job_at: Option<Timestamp>) -> PlatformSummary {
    let last_run_at = links
        .iter()
        .filter_map(|row| row.result.as_ref().map(|r| r.run_at))
        .max();
    let last_pass_at = links
        .iter()
        .filter_map(|row| row.result.as_ref().and_then(|r| r.last_pass_at))
        .max();
    let in_use = links.iter().filter(|row| row.link.enabled);
    let (mut passed, mut failed, mut login_required) = (0u32, 0u32, 0u32);
    for row in in_use {
        match row.status() {
            FixtureStatus::Pass => passed += 1,
            FixtureStatus::Fail => failed += 1,
            FixtureStatus::LoginRequired => login_required += 1,
            FixtureStatus::Never => {}
        }
    }
    let job_since_run = match (last_job_at, last_run_at) {
        (Some(job), Some(run)) => job >= run,
        (Some(_), None) => true,
        (None, _) => false,
    };
    let health = if passed > 0 || job_since_run {
        PlatformHealth::Working
    } else if failed > 0 {
        PlatformHealth::Failing
    } else if login_required > 0 {
        PlatformHealth::LoginRequired
    } else {
        PlatformHealth::Unknown
    };
    PlatformSummary {
        last_run_at,
        last_pass_at,
        last_job_at,
        passed,
        failed,
        login_required,
        health,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("no platform is called {0}")]
    Unknown(String),
    #[error("{0} has no check links in use")]
    NoFixtures(String),
    #[error("no platform has check links in use")]
    Nothing,
    #[error("the checks of {0} are already running")]
    Busy(String),
    #[error("every platform's checks are already running")]
    AllBusy,
    #[error("no check link is called {0}")]
    UnknownLink(Uuid),
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    #[error("{platform} already checks {url}")]
    Duplicate { platform: String, url: String },
    #[error("no check link is called {0}")]
    NotFound(Uuid),
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl From<discoclip_engine::rusqlite::Error> for LinkError {
    fn from(error: discoclip_engine::rusqlite::Error) -> Self {
        LinkError::Store(error.into())
    }
}

/// Check links and their results, kept in the application's database.
#[derive(Clone)]
pub struct FixtureStore {
    db: SqliteStore,
}

const LINK_SELECT: &str = "SELECT l.id, l.platform, l.url, l.origin, l.enabled, l.disabled_reason, \
     l.created_at, l.updated_at, r.run_at, r.ok, r.login_required, r.error, r.title, \
     r.duration_ms, r.last_pass_at, r.found_kind, r.found_count \
     FROM fixture_links l LEFT JOIN fixture_results r ON r.platform = l.platform AND r.url = l.url";

/// Links in use first by when they last resolved, newest first, links never run after
/// them in the order they were added: what a run tries, in order.
const LINK_ORDER: &str =
    "ORDER BY r.last_pass_at IS NULL, r.last_pass_at DESC, l.created_at ASC, l.id";

fn row_to_link(row: &Row<'_>) -> Result<LinkRow, StoreError> {
    let id: String = row.get(0)?;
    let origin: String = row.get(3)?;
    let created_at: i64 = row.get(6)?;
    let updated_at: i64 = row.get(7)?;
    let run_at: Option<i64> = row.get(8)?;
    let result = match run_at {
        None => None,
        Some(run_at) => {
            let last_pass: Option<i64> = row.get(14)?;
            let duration_ms: i64 = row.get(13)?;
            Some(Recorded {
                run_at: timestamp("fixture_results.run_at", run_at)?,
                ok: row.get(9)?,
                login_required: row.get(10)?,
                error: row.get(11)?,
                title: row.get(12)?,
                duration_ms: duration_ms.max(0) as u64,
                last_pass_at: last_pass
                    .map(|n| timestamp("fixture_results.last_pass_at", n))
                    .transpose()?,
                found: Found::from_columns(row.get(15)?, row.get(16)?)?,
            })
        }
    };
    Ok(LinkRow {
        link: FixtureLink {
            id: id
                .parse()
                .map_err(|e| StoreError::Corrupt(format!("fixture_links.id {id}: {e}")))?,
            platform: row.get(1)?,
            url: row.get(2)?,
            origin: LinkOrigin::parse(&origin)?,
            enabled: row.get(4)?,
            disabled_reason: row.get(5)?,
            created_at: timestamp("fixture_links.created_at", created_at)?,
            updated_at: timestamp("fixture_links.updated_at", updated_at)?,
        },
        result,
    })
}

fn links_where(
    tx: &discoclip_engine::rusqlite::Transaction<'_>,
    condition: &str,
    args: &[&dyn discoclip_engine::rusqlite::ToSql],
) -> Result<Vec<LinkRow>, StoreError> {
    let mut stmt = tx.prepare(&format!(
        "{LINK_SELECT} WHERE l.removed_at IS NULL AND {condition} {LINK_ORDER}"
    ))?;
    let rows = stmt.query_map(args, |row| {
        row_to_link(row).map_err(|e| {
            discoclip_engine::rusqlite::Error::FromSqlConversionFailure(
                0,
                discoclip_engine::rusqlite::types::Type::Text,
                Box::new(e),
            )
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn link_in(
    tx: &discoclip_engine::rusqlite::Transaction<'_>,
    id: Uuid,
) -> Result<FixtureLink, LinkError> {
    let mut rows = links_where(tx, "l.id = ?1", &[&id.to_string()])?;
    rows.pop()
        .map(|row| row.link)
        .ok_or(LinkError::NotFound(id))
}

impl FixtureStore {
    pub fn new(db: SqliteStore) -> Self {
        Self { db }
    }

    /// Adds the links `platforms` ship with that are not stored yet. A shipped link that
    /// was removed by hand stays removed. How many were added.
    pub async fn seed(&self, platforms: &[Platform]) -> Result<usize, StoreError> {
        let shipped: Vec<(&'static str, &'static str)> = platforms
            .iter()
            .flat_map(|p| p.examples.iter().map(move |url| (p.id, *url)))
            .collect();
        transact(&self.db, move |tx| {
            let now = nanos(Timestamp::now());
            let mut added = 0;
            for (platform, url) in shipped {
                added += tx.execute(
                    "INSERT OR IGNORE INTO fixture_links
                        (id, platform, url, origin, enabled, disabled_reason, created_at, updated_at, removed_at)
                     VALUES (?1, ?2, ?3, 'builtin', 1, NULL, ?4, ?4, NULL)",
                    params![Uuid::now_v7().to_string(), platform, url, now],
                )?;
            }
            Ok::<_, StoreError>(added)
        })
        .await
    }

    /// Every platform's links with their latest results, by platform id.
    pub async fn all(&self) -> Result<BTreeMap<String, Stored>, StoreError> {
        transact(&self.db, |tx| {
            let mut out: BTreeMap<String, Stored> = BTreeMap::new();
            for row in links_where(tx, "1 = 1", &[])? {
                out.entry(row.link.platform.clone())
                    .or_default()
                    .links
                    .push(row);
            }
            Ok(out)
        })
        .await
    }

    /// `platform`'s links with their latest results, in the order a run tries them.
    pub async fn links(&self, platform: &str) -> Result<Vec<LinkRow>, StoreError> {
        let platform = platform.to_string();
        transact(&self.db, move |tx| {
            links_where(tx, "l.platform = ?1", &[&platform])
        })
        .await
    }

    /// One link, by id.
    pub async fn link(&self, id: Uuid) -> Result<Option<FixtureLink>, StoreError> {
        transact(&self.db, move |tx| {
            Ok::<_, StoreError>(
                links_where(tx, "l.id = ?1", &[&id.to_string()])?
                    .pop()
                    .map(|row| row.link),
            )
        })
        .await
    }

    /// Adds a link for `platform`.
    pub async fn add_link(
        &self,
        platform: &str,
        url: &str,
        origin: LinkOrigin,
    ) -> Result<FixtureLink, LinkError> {
        let platform = platform.to_string();
        let url = url.to_string();
        transact(&self.db, move |tx| {
            let taken: Option<String> = tx
                .query_row(
                    "SELECT id FROM fixture_links WHERE platform = ?1 AND url = ?2",
                    params![platform, url],
                    |row| row.get(0),
                )
                .optional()?;
            if taken.is_some() {
                return Err(LinkError::Duplicate { platform, url });
            }
            let id = Uuid::now_v7();
            let now = nanos(Timestamp::now());
            tx.execute(
                "INSERT INTO fixture_links
                    (id, platform, url, origin, enabled, disabled_reason, created_at, updated_at, removed_at)
                 VALUES (?1, ?2, ?3, ?4, 1, NULL, ?5, ?5, NULL)",
                params![id.to_string(), platform, url, origin.as_str(), now],
            )?;
            link_in(tx, id)
        })
        .await
    }

    /// Points a link at `url`. Its result goes with the old address, and it is in use again.
    pub async fn edit_link(&self, id: Uuid, url: &str) -> Result<FixtureLink, LinkError> {
        let url = url.to_string();
        transact(&self.db, move |tx| {
            let current = link_in(tx, id)?;
            if current.url != url {
                let taken: Option<String> = tx
                    .query_row(
                        "SELECT id FROM fixture_links WHERE platform = ?1 AND url = ?2",
                        params![current.platform, url],
                        |row| row.get(0),
                    )
                    .optional()?;
                if taken.is_some() {
                    return Err(LinkError::Duplicate {
                        platform: current.platform,
                        url,
                    });
                }
                tx.execute(
                    "DELETE FROM fixture_results WHERE platform = ?1 AND url = ?2",
                    params![current.platform, current.url],
                )?;
            }
            tx.execute(
                "UPDATE fixture_links SET url = ?2, enabled = 1, disabled_reason = NULL, updated_at = ?3
                 WHERE id = ?1",
                params![id.to_string(), url, nanos(Timestamp::now())],
            )?;
            link_in(tx, id)
        })
        .await
    }

    /// Switches a link on or off by hand. Switching it on clears why it went off.
    pub async fn set_enabled(&self, id: Uuid, enabled: bool) -> Result<FixtureLink, LinkError> {
        transact(&self.db, move |tx| {
            link_in(tx, id)?;
            tx.execute(
                "UPDATE fixture_links SET enabled = ?2, disabled_reason = NULL, updated_at = ?3
                 WHERE id = ?1",
                params![id.to_string(), enabled, nanos(Timestamp::now())],
            )?;
            link_in(tx, id)
        })
        .await
    }

    /// Removes a link and its result. A shipped link is marked removed so seeding does not
    /// bring it back. Whether there was one.
    pub async fn remove_link(&self, id: Uuid) -> Result<bool, StoreError> {
        transact(&self.db, move |tx| {
            let Ok(link) = link_in(tx, id) else {
                return Ok::<_, StoreError>(false);
            };
            tx.execute(
                "DELETE FROM fixture_results WHERE platform = ?1 AND url = ?2",
                params![link.platform, link.url],
            )?;
            match link.origin {
                LinkOrigin::Builtin => tx.execute(
                    "UPDATE fixture_links SET removed_at = ?2, updated_at = ?2 WHERE id = ?1",
                    params![id.to_string(), nanos(Timestamp::now())],
                )?,
                LinkOrigin::Custom | LinkOrigin::Job => tx.execute(
                    "DELETE FROM fixture_links WHERE id = ?1",
                    params![id.to_string()],
                )?,
            };
            Ok(true)
        })
        .await
    }

    /// Keeps the link of a job that finished on `platform` as a check link, when the
    /// platform does not have it already, dropping the oldest learned links beyond
    /// [`LEARNED_LINKS`]. A link removed by hand is not learned again. Whether it was added.
    pub async fn learn(&self, platform: &str, url: &str) -> Result<bool, StoreError> {
        let platform = platform.to_string();
        let url = url.to_string();
        transact(&self.db, move |tx| {
            let known: Option<String> = tx
                .query_row(
                    "SELECT id FROM fixture_links WHERE platform = ?1 AND url = ?2",
                    params![platform, url],
                    |row| row.get(0),
                )
                .optional()?;
            if known.is_some() {
                return Ok::<_, StoreError>(false);
            }
            let now = nanos(Timestamp::now());
            tx.execute(
                "INSERT INTO fixture_links
                    (id, platform, url, origin, enabled, disabled_reason, created_at, updated_at, removed_at)
                 VALUES (?1, ?2, ?3, 'job', 1, NULL, ?4, ?4, NULL)",
                params![Uuid::now_v7().to_string(), platform, url, now],
            )?;
            let mut stmt = tx.prepare(
                "SELECT id, url FROM fixture_links
                 WHERE platform = ?1 AND origin = 'job' AND removed_at IS NULL
                 ORDER BY created_at DESC, id DESC",
            )?;
            let learned: Vec<(String, String)> = stmt
                .query_map(params![platform], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<_, _>>()?;
            for (id, old) in learned.into_iter().skip(LEARNED_LINKS) {
                tx.execute(
                    "DELETE FROM fixture_results WHERE platform = ?1 AND url = ?2",
                    params![platform, old],
                )?;
                tx.execute("DELETE FROM fixture_links WHERE id = ?1", params![id])?;
            }
            Ok(true)
        })
        .await
    }

    /// Stores what a run of `platform`'s links found, as their latest results. With
    /// `retire_failed`, a link that failed while another resolved in the same run is
    /// switched off, with what it failed with as the reason. The platform's links as they
    /// stand.
    pub async fn record(
        &self,
        platform: &str,
        finished_at: Timestamp,
        outcomes: Vec<Outcome>,
        retire_failed: bool,
    ) -> Result<Vec<LinkRow>, StoreError> {
        let platform = platform.to_string();
        transact(&self.db, move |tx| {
            let at = nanos(finished_at);
            for outcome in &outcomes {
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
            if retire_failed && outcomes.iter().any(|o| o.ok) {
                for dead in outcomes.iter().filter(|o| !o.ok && !o.login_required) {
                    tx.execute(
                        "UPDATE fixture_links SET enabled = 0, disabled_reason = ?3, updated_at = ?4
                         WHERE platform = ?1 AND url = ?2 AND removed_at IS NULL AND enabled = 1",
                        params![
                            platform,
                            dead.url,
                            dead.error.clone().unwrap_or_else(|| "did not resolve".into()),
                            at
                        ],
                    )?;
                }
            }
            links_where(tx, "l.platform = ?1", &[&platform])
        })
        .await
    }
}

/// Runs platforms' checks through the engine's resolvers and keeps the results.
pub struct FixtureRunner {
    engine: EngineHandle,
    store: FixtureStore,
    pub config: SharedFixtureConfig,
    /// The platforms whose checks are running right now.
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

    fn platform_named(&self, id: &str) -> Result<Platform, RunError> {
        self.engine
            .platforms()
            .into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| RunError::Unknown(id.to_string()))
    }

    /// When a job last finished on each platform.
    async fn last_jobs(&self) -> Result<HashMap<String, Timestamp>, StoreError> {
        Ok(self
            .engine
            .resolver_stats()
            .await?
            .into_iter()
            .filter_map(|stats| stats.last_done_at.map(|at| (stats.resolver, at)))
            .collect())
    }

    /// Every registered platform with what its links and jobs show of it.
    pub async fn coverage(&self) -> Result<Vec<PlatformCoverage>, StoreError> {
        let stored = self.store.all().await?;
        let jobs = self.last_jobs().await?;
        let running = self.running();
        Ok(self
            .engine
            .platforms()
            .into_iter()
            .map(|platform| {
                let links = stored
                    .get(platform.id)
                    .map(|s| s.links.as_slice())
                    .unwrap_or(&[]);
                let last_job_at = jobs.get(platform.id).copied();
                let busy = running.contains(platform.id);
                assemble(platform, links, last_job_at, busy)
            })
            .collect())
    }

    /// One platform with what its links and jobs show of it.
    pub async fn platform(&self, id: &str) -> Result<Option<PlatformCoverage>, StoreError> {
        let Some(platform) = self.engine.platforms().into_iter().find(|p| p.id == id) else {
            return Ok(None);
        };
        let links = self.store.links(id).await?;
        let last_job_at = self.last_jobs().await?.get(id).copied();
        Ok(Some(assemble(
            platform,
            &links,
            last_job_at,
            self.is_running(id),
        )))
    }

    /// Adds a link for `platform` by hand.
    pub async fn add_link(&self, platform: &str, url: &str) -> Result<FixtureLink, LinkError> {
        self.store.add_link(platform, url, LinkOrigin::Custom).await
    }

    pub async fn edit_link(&self, id: Uuid, url: &str) -> Result<FixtureLink, LinkError> {
        self.store.edit_link(id, url).await
    }

    pub async fn set_enabled(&self, id: Uuid, enabled: bool) -> Result<FixtureLink, LinkError> {
        self.store.set_enabled(id, enabled).await
    }

    pub async fn remove_link(&self, id: Uuid) -> Result<bool, StoreError> {
        self.store.remove_link(id).await
    }

    pub async fn link(&self, id: Uuid) -> Result<Option<FixtureLink>, StoreError> {
        self.store.link(id).await
    }

    /// Keeps the link of a job that finished on `platform`, when it is new there.
    pub async fn learn(&self, platform: &str, url: &str) -> Result<bool, StoreError> {
        self.store.learn(platform, url).await
    }

    /// Starts a run of `platform`'s links in the background.
    pub async fn start(self: &Arc<Self>, platform: &str) -> Result<(), RunError> {
        let found = self.platform_named(platform)?;
        let links: Vec<FixtureLink> = self
            .store
            .links(found.id)
            .await?
            .into_iter()
            .filter(|row| row.link.enabled)
            .map(|row| row.link)
            .collect();
        if links.is_empty() {
            return Err(RunError::NoFixtures(platform.to_string()));
        }
        if !self.claim(found.id) {
            return Err(RunError::Busy(platform.to_string()));
        }
        self.spawn(found, Work::Platform(links));
        Ok(())
    }

    /// Starts a run of one of `platform`'s links in the background, whether or not the
    /// link is in use. Its result is recorded; nothing is switched off over it.
    pub async fn start_link(self: &Arc<Self>, platform: &str, link: Uuid) -> Result<(), RunError> {
        let found = self.platform_named(platform)?;
        let link = self
            .store
            .link(link)
            .await?
            .filter(|l| l.platform == found.id)
            .ok_or(RunError::UnknownLink(link))?;
        if !self.claim(found.id) {
            return Err(RunError::Busy(platform.to_string()));
        }
        self.spawn(found, Work::Link(link));
        Ok(())
    }

    /// Starts a run for every platform with links in use that is not already running.
    /// The ids started.
    pub async fn start_all(self: &Arc<Self>) -> Result<Vec<&'static str>, RunError> {
        let stored = self.store.all().await?;
        let platforms: Vec<(Platform, Vec<FixtureLink>)> = self
            .engine
            .platforms()
            .into_iter()
            .filter_map(|p| {
                let links = in_use(stored.get(p.id));
                (!links.is_empty()).then_some((p, links))
            })
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

    /// Starts a run for every platform whose links are due: never run, or run longer ago
    /// than the interval. The ids started.
    pub async fn start_due(self: &Arc<Self>) -> Result<Vec<&'static str>, StoreError> {
        let interval = SignedDuration::from_secs(self.config().interval_secs as i64);
        let stored = self.store.all().await?;
        let now = Timestamp::now();
        let due: Vec<(Platform, Vec<FixtureLink>)> = self
            .engine
            .platforms()
            .into_iter()
            .filter_map(|p| {
                let rows = stored.get(p.id);
                let links = in_use(rows);
                if links.is_empty() {
                    return None;
                }
                let last_run = rows.and_then(|s| {
                    s.links
                        .iter()
                        .filter(|row| row.link.enabled)
                        .filter_map(|row| row.result.as_ref().map(|r| r.run_at))
                        .max()
                });
                last_run
                    .is_none_or(|at| now.duration_since(at) >= interval)
                    .then_some((p, links))
            })
            .collect();
        Ok(self.start_each(due))
    }

    fn start_each(
        self: &Arc<Self>,
        platforms: Vec<(Platform, Vec<FixtureLink>)>,
    ) -> Vec<&'static str> {
        let mut started = Vec::new();
        for (platform, links) in platforms {
            if self.claim(platform.id) {
                started.push(platform.id);
                self.spawn(platform, Work::Platform(links));
            }
        }
        started
    }

    /// Marks `platform` as running. False when it already was.
    fn claim(&self, platform: &'static str) -> bool {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(platform)
    }

    fn spawn(self: &Arc<Self>, platform: Platform, work: Work) {
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
            let result = match work {
                Work::Platform(links) => runner.run(&platform, links).await,
                Work::Link(link) => runner.run_link(&platform, link).await,
            };
            match result {
                Ok(links) => {
                    let summary = summarize(&links, None);
                    tracing::info!(
                        platform = platform.id,
                        passed = summary.passed,
                        failed = summary.failed,
                        "platform checked"
                    );
                }
                Err(error) => {
                    tracing::error!(platform = platform.id, "check results not stored: {error}")
                }
            }
        });
    }

    /// Resolves one link within the configured time.
    async fn try_link(&self, url: &str, timeout: Duration) -> Outcome {
        let began = Instant::now();
        let outcome = match Url::parse(url) {
            Err(error) => Outcome {
                url: url.to_string(),
                ok: false,
                login_required: false,
                error: Some(format!("Not a URL: {error}")),
                title: None,
                found: None,
                duration: began.elapsed(),
            },
            Ok(parsed) => match tokio::time::timeout(timeout, self.engine.resolve(&parsed)).await {
                Err(_) => Outcome {
                    url: url.to_string(),
                    ok: false,
                    login_required: false,
                    error: Some(format!("No answer within {}s", timeout.as_secs())),
                    title: None,
                    found: None,
                    duration: began.elapsed(),
                },
                Ok(Err(error)) => Outcome {
                    url: url.to_string(),
                    ok: false,
                    login_required: matches!(error, ResolveError::LoginRequired { .. }),
                    error: Some(message(url, &error)),
                    title: None,
                    found: None,
                    duration: began.elapsed(),
                },
                Ok(Ok(resolution)) => judge(url, resolution, began.elapsed()),
            },
        };
        tracing::debug!(
            url,
            ok = outcome.ok,
            error = outcome.error.as_deref().unwrap_or(""),
            "check link resolved"
        );
        outcome
    }

    /// Tries `links` in turn until one resolves, and records what came of each. Links that
    /// failed on the way to one that resolved are switched off.
    async fn run(
        &self,
        platform: &Platform,
        links: Vec<FixtureLink>,
    ) -> Result<Vec<LinkRow>, StoreError> {
        let timeout = Duration::from_secs(self.config().timeout_secs.max(1));
        let mut outcomes = Vec::with_capacity(links.len());
        for link in &links {
            let outcome = self.try_link(&link.url, timeout).await;
            let ok = outcome.ok;
            outcomes.push(outcome);
            if ok {
                break;
            }
        }
        self.store
            .record(platform.id, Timestamp::now(), outcomes, true)
            .await
    }

    /// Resolves one link and records what came of it.
    async fn run_link(
        &self,
        platform: &Platform,
        link: FixtureLink,
    ) -> Result<Vec<LinkRow>, StoreError> {
        let timeout = Duration::from_secs(self.config().timeout_secs.max(1));
        let outcome = self.try_link(&link.url, timeout).await;
        self.store
            .record(platform.id, Timestamp::now(), vec![outcome], false)
            .await
    }

    /// Runs the checks that are due, on the configured interval, until `shutdown`.
    pub async fn schedule(self: Arc<Self>, shutdown: CancellationToken) {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(STARTUP_DELAY) => {}
        }
        loop {
            if self.config().interval_secs > 0 {
                match self.start_due().await {
                    Ok(started) if !started.is_empty() => {
                        tracing::info!(platforms = %started.join(", "), "scheduled platform checks");
                    }
                    Ok(_) => {}
                    Err(error) => tracing::error!("check runs not read: {error}"),
                }
            }
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(TICK) => {}
            }
        }
    }

    /// Keeps the link of every job that finishes as a check link of its platform, until
    /// `shutdown`.
    pub async fn learn_from_jobs(self: Arc<Self>, shutdown: CancellationToken) {
        use discoclip_engine::{EventKind, JobStatus};
        let mut events = self.engine.subscribe();
        loop {
            let event = tokio::select! {
                _ = shutdown.cancelled() => return,
                event = events.recv() => event,
            };
            let event = match event {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            };
            if !matches!(
                event.kind,
                EventKind::Status {
                    status: JobStatus::Done
                }
            ) {
                continue;
            }
            let job = match self.engine.get(event.job).await {
                Ok(Some(job)) => job,
                Ok(None) => continue,
                Err(error) => {
                    tracing::warn!(job = %event.job, "finished job not read: {error}");
                    continue;
                }
            };
            let Some(platform) = job.resolver() else {
                continue;
            };
            match self.learn(platform, job.request.url.as_str()).await {
                Ok(true) => tracing::info!(
                    platform,
                    url = %job.request.url,
                    "check link learned from a finished job"
                ),
                Ok(false) => {}
                Err(error) => tracing::warn!(platform, "check link not learned: {error}"),
            }
        }
    }
}

/// What a background run does.
enum Work {
    Platform(Vec<FixtureLink>),
    Link(FixtureLink),
}

/// The links of a platform that runs try.
fn in_use(stored: Option<&Stored>) -> Vec<FixtureLink> {
    stored
        .map(|s| {
            s.links
                .iter()
                .filter(|row| row.link.enabled)
                .map(|row| row.link.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn assemble(
    platform: Platform,
    links: &[LinkRow],
    last_job_at: Option<Timestamp>,
    running: bool,
) -> PlatformCoverage {
    PlatformCoverage {
        id: platform.id,
        name: platform.name,
        hosts: platform.hosts,
        features: platform.features,
        formats: platform.formats,
        media: platform.media,
        tags: platform.tags,
        session: platform.session,
        on_by_default: platform.on_by_default,
        fixtures: links.iter().map(LinkRow::view).collect(),
        summary: summarize(links, last_job_at),
        running,
    }
}

/// What a resolution counts as: media with something playable, or a playlist with
/// entries. And what it was, for the page to show what the link resolves to.
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
                    Some(format!("Every variant is locked with {system} DRM")),
                    resolved.title,
                    None,
                )
            } else {
                (
                    false,
                    Some("Resolved without any media variant".to_string()),
                    resolved.title,
                    None,
                )
            }
        }
        Resolution::Playlist(playlist) => {
            if playlist.entries.is_empty() {
                (
                    false,
                    Some("The playlist has no entries".to_string()),
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

/// What a link's failure reads as: the resolver's words about what went wrong, as a
/// sentence and without the link itself, which every row shows beside the message.
fn message(url: &str, error: &ResolveError) -> String {
    match error {
        ResolveError::Unsupported(_) => "No resolver takes this link".to_string(),
        ResolveError::NotFound(_) => "No video found".to_string(),
        ResolveError::Unavailable { reason, .. } => format!("Unavailable: {reason}"),
        ResolveError::Malformed { detail, .. } => format!("Unexpected response: {detail}"),
        ResolveError::Http(error) => sentence(&error.to_string().replace(url, "the link")),
        ResolveError::Redirect(to) => format!("The media lives at {to}"),
        ResolveError::Drm { system, .. } => format!("Protected by {system} DRM"),
        ResolveError::LoginRequired {
            platform, reason, ..
        } => format!("Needs a logged-in {platform} session: {reason}"),
        ResolveError::RateLimited(_) => "Rate limited. Try again later.".to_string(),
        ResolveError::Disabled { platform, .. } => {
            format!("Links for {platform} are turned off here")
        }
    }
}

/// The text with its first letter in upper case.
fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use discoclip_engine::http::{HttpError, StatusCode};
    use discoclip_engine::resolve::{Playlist, PlaylistEntry, Resolved, Variant};

    async fn store() -> FixtureStore {
        let db = SqliteStore::open_in_memory().await.unwrap();
        crate::migrations::apply(&db).await.unwrap();
        FixtureStore::new(db)
    }

    fn platform(examples: &'static [&'static str]) -> Platform {
        Platform {
            id: "p",
            name: "P",
            hosts: &["p"],
            features: &[],
            formats: &[],
            media: &[MediaKind::Video],
            tags: &[],
            session: SessionSupport::None,
            on_by_default: true,
            examples,
        }
    }

    fn outcome(url: &str, ok: bool) -> Outcome {
        Outcome {
            url: url.into(),
            ok,
            login_required: false,
            error: (!ok).then(|| "No video found".to_string()),
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
            error: Some("Needs a logged-in p session: posts are read with the sid cookie".into()),
            title: None,
            found: None,
            duration: Duration::from_millis(12),
        }
    }

    fn by_url<'a>(links: &'a [LinkRow], url: &str) -> &'a LinkRow {
        links.iter().find(|row| row.link.url == url).unwrap()
    }

    #[tokio::test]
    async fn shipped_links_are_seeded_once_and_stay_removed_once_removed() {
        let store = store().await;
        let shipped = platform(&["https://p/a", "https://p/b"]);
        assert_eq!(store.seed(std::slice::from_ref(&shipped)).await.unwrap(), 2);
        assert_eq!(store.seed(std::slice::from_ref(&shipped)).await.unwrap(), 0);
        let links = store.links("p").await.unwrap();
        assert_eq!(links.len(), 2);
        assert!(
            links
                .iter()
                .all(|row| row.link.origin == LinkOrigin::Builtin)
        );
        assert!(links.iter().all(|row| row.link.enabled));
        let a = by_url(&links, "https://p/a").link.id;
        assert!(store.remove_link(a).await.unwrap());
        assert!(!store.remove_link(a).await.unwrap());
        assert_eq!(
            store.seed(&[shipped]).await.unwrap(),
            0,
            "a removed link stays removed"
        );
        let links = store.links("p").await.unwrap();
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].link.url, "https://p/b");
        assert!(store.all().await.unwrap()["p"].links.len() == 1);
    }

    #[tokio::test]
    async fn links_are_added_edited_switched_and_removed() {
        let store = store().await;
        store.seed(&[platform(&["https://p/a"])]).await.unwrap();
        let added = store
            .add_link("p", "https://p/mine", LinkOrigin::Custom)
            .await
            .unwrap();
        assert_eq!(added.origin, LinkOrigin::Custom);
        assert!(matches!(
            store
                .add_link("p", "https://p/mine", LinkOrigin::Custom)
                .await,
            Err(LinkError::Duplicate { .. })
        ));
        store
            .record(
                "p",
                Timestamp::now(),
                vec![outcome("https://p/mine", true)],
                false,
            )
            .await
            .unwrap();
        let links = store.links("p").await.unwrap();
        assert_eq!(
            by_url(&links, "https://p/mine").status(),
            FixtureStatus::Pass
        );

        // A new address starts over, and may not be one the platform already checks.
        assert!(matches!(
            store.edit_link(added.id, "https://p/a").await,
            Err(LinkError::Duplicate { .. })
        ));
        let edited = store.edit_link(added.id, "https://p/moved").await.unwrap();
        assert_eq!(edited.url, "https://p/moved");
        let links = store.links("p").await.unwrap();
        assert_eq!(
            by_url(&links, "https://p/moved").status(),
            FixtureStatus::Never
        );
        assert!(links.iter().all(|row| row.link.url != "https://p/mine"));

        let off = store.set_enabled(added.id, false).await.unwrap();
        assert!(!off.enabled);
        assert!(off.disabled_reason.is_none());
        let on = store.set_enabled(added.id, true).await.unwrap();
        assert!(on.enabled);
        assert!(matches!(
            store.set_enabled(Uuid::now_v7(), true).await,
            Err(LinkError::NotFound(_))
        ));
        assert!(store.remove_link(added.id).await.unwrap());
        assert_eq!(store.links("p").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_link_that_fails_while_another_resolves_is_switched_off() {
        let store = store().await;
        store
            .seed(&[platform(&[
                "https://p/dead",
                "https://p/live",
                "https://p/post",
            ])])
            .await
            .unwrap();
        let at = Timestamp::from_second(1_700_000_000).unwrap();
        let links = store
            .record(
                "p",
                at,
                vec![
                    outcome("https://p/dead", false),
                    wants_login("https://p/post"),
                    outcome("https://p/live", true),
                ],
                true,
            )
            .await
            .unwrap();
        let dead = by_url(&links, "https://p/dead");
        assert!(!dead.link.enabled);
        assert_eq!(dead.link.disabled_reason.as_deref(), Some("No video found"));
        assert_eq!(dead.status(), FixtureStatus::Fail);
        let post = by_url(&links, "https://p/post");
        assert!(post.link.enabled, "a link wanting a login is not dead");
        assert_eq!(post.status(), FixtureStatus::LoginRequired);
        let live = by_url(&links, "https://p/live");
        assert!(live.link.enabled);
        assert_eq!(live.result.as_ref().unwrap().last_pass_at, Some(at));
        let summary = summarize(&links, None);
        assert_eq!(summary.health, PlatformHealth::Working);
        assert_eq!(
            (summary.passed, summary.failed, summary.login_required),
            (1, 0, 1),
            "the link switched off does not count"
        );
        assert_eq!(summary.last_run_at, Some(at));
        assert_eq!(summary.last_pass_at, Some(at));

        // With nothing resolving, nothing is switched off: the platform may be down.
        let later = Timestamp::from_second(1_700_000_600).unwrap();
        let links = store
            .record(
                "p",
                later,
                vec![
                    outcome("https://p/live", false),
                    wants_login("https://p/post"),
                ],
                true,
            )
            .await
            .unwrap();
        assert!(by_url(&links, "https://p/live").link.enabled);
        let summary = summarize(&links, None);
        assert_eq!(summary.health, PlatformHealth::Failing);
        assert_eq!(summary.last_pass_at, Some(at), "when a link last resolved");
        assert_eq!(summary.last_run_at, Some(later));
        // A job finishing on the platform since says it works after all.
        let worked = Timestamp::from_second(1_700_000_700).unwrap();
        assert_eq!(
            summarize(&links, Some(worked)).health,
            PlatformHealth::Working
        );
        let before = Timestamp::from_second(1_700_000_500).unwrap();
        assert_eq!(
            summarize(&links, Some(before)).health,
            PlatformHealth::Failing
        );
        // A single link run records its result and switches nothing off.
        let links = store
            .record("p", later, vec![outcome("https://p/dead", false)], false)
            .await
            .unwrap();
        assert!(!by_url(&links, "https://p/dead").link.enabled);
        assert!(by_url(&links, "https://p/live").link.enabled);
    }

    #[tokio::test]
    async fn links_wanting_a_login_alone_make_the_platform_want_a_login() {
        let store = store().await;
        store.seed(&[platform(&["https://p/post"])]).await.unwrap();
        let links = store
            .record(
                "p",
                Timestamp::now(),
                vec![wants_login("https://p/post")],
                true,
            )
            .await
            .unwrap();
        let summary = summarize(&links, None);
        assert_eq!(summary.health, PlatformHealth::LoginRequired);
        assert_eq!(summary.login_required, 1);
        assert_eq!(summary.last_pass_at, None);
        // With the session in place the link passes, and the platform with it.
        let links = store
            .record(
                "p",
                Timestamp::now(),
                vec![outcome("https://p/post", true)],
                true,
            )
            .await
            .unwrap();
        assert_eq!(summarize(&links, None).health, PlatformHealth::Working);
        assert!(
            !by_url(&links, "https://p/post")
                .result
                .as_ref()
                .unwrap()
                .login_required
        );
    }

    #[tokio::test]
    async fn nothing_checked_and_no_jobs_is_unknown() {
        let store = store().await;
        store.seed(&[platform(&["https://p/a"])]).await.unwrap();
        let links = store.links("p").await.unwrap();
        assert_eq!(summarize(&links, None).health, PlatformHealth::Unknown);
        let job = Timestamp::now();
        let summary = summarize(&links, Some(job));
        assert_eq!(summary.health, PlatformHealth::Working);
        assert_eq!(summary.last_job_at, Some(job));
        assert_eq!(summarize(&[], None).health, PlatformHealth::Unknown);
    }

    #[tokio::test]
    async fn links_are_learned_from_jobs_and_the_newest_three_stay() {
        let store = store().await;
        store.seed(&[platform(&["https://p/a"])]).await.unwrap();
        assert!(
            !store.learn("p", "https://p/a").await.unwrap(),
            "known already"
        );
        for n in 1..=4 {
            assert!(
                store
                    .learn("p", &format!("https://p/job{n}"))
                    .await
                    .unwrap()
            );
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert!(!store.learn("p", "https://p/job4").await.unwrap());
        let links = store.links("p").await.unwrap();
        let learned: Vec<&str> = links
            .iter()
            .filter(|row| row.link.origin == LinkOrigin::Job)
            .map(|row| row.link.url.as_str())
            .collect();
        assert_eq!(learned.len(), LEARNED_LINKS);
        assert!(!learned.contains(&"https://p/job1"), "{learned:?}");
        assert!(learned.contains(&"https://p/job4"));
        assert_eq!(links.len(), 1 + LEARNED_LINKS);
        // A learned link removed by hand is not learned again.
        let job4 = by_url(&links, "https://p/job4").link.id;
        assert!(store.remove_link(job4).await.unwrap());
        assert!(
            store.learn("p", "https://p/job4").await.unwrap(),
            "a deleted learned link may come back"
        );
    }

    #[tokio::test]
    async fn runs_try_the_last_resolving_link_first() {
        let store = store().await;
        store
            .seed(&[platform(&["https://p/a", "https://p/b", "https://p/c"])])
            .await
            .unwrap();
        let first = Timestamp::from_second(1_700_000_000).unwrap();
        store
            .record("p", first, vec![outcome("https://p/b", true)], true)
            .await
            .unwrap();
        let second = Timestamp::from_second(1_700_000_600).unwrap();
        let links = store
            .record("p", second, vec![outcome("https://p/c", true)], true)
            .await
            .unwrap();
        let order: Vec<&str> = links.iter().map(|row| row.link.url.as_str()).collect();
        assert_eq!(order, vec!["https://p/c", "https://p/b", "https://p/a"]);
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

    #[test]
    fn a_failure_reads_as_a_sentence_without_the_link() {
        let url = Url::parse("https://p/v").unwrap();
        let at = url.as_str();
        for (error, expected) in [
            (
                ResolveError::unavailable(&url, "HTTP 502 Bad Gateway"),
                "Unavailable: HTTP 502 Bad Gateway",
            ),
            (ResolveError::NotFound(url.clone()), "No video found"),
            (
                ResolveError::login_required(&url, "p", "posts are read with the sid cookie"),
                "Needs a logged-in p session: posts are read with the sid cookie",
            ),
            (
                ResolveError::drm(&url, "widevine"),
                "Protected by widevine DRM",
            ),
            (
                ResolveError::malformed(&url, "the page carries no player"),
                "Unexpected response: the page carries no player",
            ),
            (
                ResolveError::Http(HttpError::Status {
                    url: url.to_string(),
                    status: StatusCode::BAD_GATEWAY,
                }),
                "The link answered HTTP 502 Bad Gateway",
            ),
        ] {
            let text = message(at, &error);
            assert_eq!(text, expected);
            assert!(!text.contains(at), "{text} repeats the link");
        }
    }
}
