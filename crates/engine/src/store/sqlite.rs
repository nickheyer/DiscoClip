use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use jiff::Timestamp;
use rusqlite::{Connection, OptionalExtension, params, types::Value};

use crate::job::{Job, JobId, StatusKind};
use crate::store::migrate::{self, Migration};
use crate::store::{JobFilter, JobStore, Order, ResolverStats, Stats, StoreError};

const SCOPE: &str = "engine";

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "jobs",
        sql: "
CREATE TABLE IF NOT EXISTS jobs (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    data TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS jobs_created_at ON jobs(created_at DESC);
CREATE INDEX IF NOT EXISTS jobs_status ON jobs(status, created_at DESC);
CREATE INDEX IF NOT EXISTS jobs_source ON jobs(source, created_at DESC);
",
    },
    Migration {
        version: 2,
        name: "jobs.search_columns",
        sql: "
ALTER TABLE jobs ADD COLUMN url TEXT NOT NULL DEFAULT '';
ALTER TABLE jobs ADD COLUMN title TEXT;
ALTER TABLE jobs ADD COLUMN resolver TEXT;
ALTER TABLE jobs ADD COLUMN parent_id TEXT;
ALTER TABLE jobs ADD COLUMN submitted_by TEXT;
ALTER TABLE jobs ADD COLUMN finished_at INTEGER;
UPDATE jobs SET
    url = COALESCE(json_extract(data, '$.request.url'), ''),
    title = json_extract(data, '$.artifacts.resolved.title'),
    resolver = json_extract(data, '$.artifacts.resolved.resolver'),
    parent_id = json_extract(data, '$.request.parent'),
    submitted_by = json_extract(data, '$.request.submitted_by');
CREATE INDEX IF NOT EXISTS jobs_parent ON jobs(parent_id, created_at ASC);
CREATE INDEX IF NOT EXISTS jobs_resolver ON jobs(resolver, status);
",
    },
];

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        StoreError::Database(error.to_string())
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(error: serde_json::Error) -> Self {
        StoreError::Corrupt(error.to_string())
    }
}

impl From<tokio_rusqlite::Error<StoreError>> for StoreError {
    fn from(error: tokio_rusqlite::Error<StoreError>) -> Self {
        match error {
            tokio_rusqlite::Error::Error(error) => error,
            other => StoreError::Database(other.to_string()),
        }
    }
}

/// Jobs persisted in a single SQLite file, served from a dedicated database thread; the full
/// job record is stored as JSON with indexed columns for filtering.
#[derive(Clone)]
pub struct SqliteStore {
    conn: tokio_rusqlite::Connection,
}

impl SqliteStore {
    pub async fn open(path: &Path) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        Self::init(tokio_rusqlite::Connection::open(path).await?).await
    }

    /// A private database that lives only as long as this store; for tests.
    pub async fn open_in_memory() -> Result<Self, StoreError> {
        Self::init(tokio_rusqlite::Connection::open_in_memory().await?).await
    }

    async fn init(conn: tokio_rusqlite::Connection) -> Result<Self, StoreError> {
        let store = Self { conn };
        store
            .call(|conn| {
                conn.busy_timeout(Duration::from_secs(10))?;
                conn.pragma_update(None, "journal_mode", "WAL")?;
                conn.pragma_update(None, "synchronous", "NORMAL")?;
                conn.pragma_update(None, "foreign_keys", "ON")?;
                Ok(())
            })
            .await?;
        store.migrate(SCOPE, MIGRATIONS).await?;
        Ok(store)
    }

    /// Brings `scope`'s tables up to `migrations`; other crates that keep tables in this
    /// database run their own lists through here.
    pub async fn migrate(
        &self,
        scope: &'static str,
        migrations: &'static [Migration],
    ) -> Result<usize, StoreError> {
        self.call(move |conn| migrate::apply(conn, scope, migrations))
            .await
    }

    /// Runs `f` on the database thread. Other tables that share this database, such as the
    /// application's settings, are read and written through here.
    pub async fn call<T, F>(&self, f: F) -> Result<T, StoreError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T, StoreError> + Send + 'static,
    {
        Ok(self.conn.call(move |conn| f(conn)).await?)
    }

    /// Copies the whole database to `dest` with SQLite's online backup, consistent even
    /// while jobs are being written.
    pub async fn backup_to(&self, dest: &Path) -> Result<(), StoreError> {
        let dest = dest.to_path_buf();
        self.call(move |conn| {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut target = Connection::open(&dest)?;
            let backup = rusqlite::backup::Backup::new(conn, &mut target)?;
            backup.run_to_completion(256, Duration::from_millis(5), None)?;
            Ok(())
        })
        .await
    }

    /// The size of the database file and its write-ahead log, when it lives on disk.
    pub async fn size_on_disk(&self) -> Result<u64, StoreError> {
        self.call(|conn| {
            let path: Option<String> = conn
                .query_row(
                    "SELECT file FROM pragma_database_list WHERE name = 'main'",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(path) = path.filter(|p| !p.is_empty()) else {
                return Ok(0);
            };
            let mut total = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            total += std::fs::metadata(format!("{path}-wal"))
                .map(|m| m.len())
                .unwrap_or(0);
            Ok(total)
        })
        .await
    }
}

fn nanos(ts: jiff::Timestamp) -> i64 {
    ts.as_nanosecond().clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

fn decode(data: String) -> Result<Job, StoreError> {
    Ok(serde_json::from_str(&data)?)
}

fn columns(job: &Job) -> Result<[Value; 11], StoreError> {
    let data = serde_json::to_string(job)?;
    Ok([
        Value::Text(job.id.to_string()),
        Value::Text(job.request.origin.source.0.clone()),
        Value::Text(job.status.kind().as_str().to_string()),
        Value::Integer(nanos(job.created_at)),
        Value::Integer(nanos(job.updated_at)),
        Value::Text(data),
        Value::Text(job.request.url.to_string()),
        job.title()
            .map_or(Value::Null, |t| Value::Text(t.to_string())),
        job.resolver()
            .map_or(Value::Null, |r| Value::Text(r.to_string())),
        job.request
            .parent
            .map_or(Value::Null, |p| Value::Text(p.to_string())),
        job.request
            .submitted_by
            .clone()
            .map_or(Value::Null, Value::Text),
    ])
}

/// The `WHERE` clause and bindings of a filter.
fn clauses(filter: &JobFilter) -> (String, Vec<Value>) {
    let mut clauses: Vec<String> = Vec::new();
    let mut values: Vec<Value> = Vec::new();
    if let Some(source) = &filter.source {
        clauses.push("source = ?".into());
        values.push(Value::Text(source.0.clone()));
    }
    if let Some(status) = filter.status {
        clauses.push("status = ?".into());
        values.push(Value::Text(status.as_str().to_string()));
    }
    if let Some(before) = filter.before {
        clauses.push("created_at < ?".into());
        values.push(Value::Integer(nanos(before)));
    }
    if let Some(after) = filter.after {
        clauses.push("created_at > ?".into());
        values.push(Value::Integer(nanos(after)));
    }
    if let Some(resolver) = &filter.resolver {
        clauses.push("resolver = ?".into());
        values.push(Value::Text(resolver.clone()));
    }
    if let Some(parent) = filter.parent {
        clauses.push("parent_id = ?".into());
        values.push(Value::Text(parent.to_string()));
    } else if filter.top_level {
        clauses.push("parent_id IS NULL".into());
    }
    if let Some(q) = filter.q.as_deref().map(str::trim).filter(|q| !q.is_empty()) {
        let like = format!(
            "%{}%",
            q.replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_")
        );
        clauses.push(
            "(url LIKE ? ESCAPE '\\' OR title LIKE ? ESCAPE '\\' OR submitted_by LIKE ? ESCAPE '\\' OR id LIKE ? ESCAPE '\\')"
                .into(),
        );
        for _ in 0..4 {
            values.push(Value::Text(like.clone()));
        }
    }
    let sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    (sql, values)
}

fn read_stats(conn: &Connection, sql: &str, values: Vec<Value>) -> Result<Stats, StoreError> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(rusqlite::params_from_iter(values), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut stats = Stats::default();
    for row in rows {
        let (status, count) = row?;
        let kind = status
            .parse::<StatusKind>()
            .map_err(|_| StoreError::Corrupt(format!("unknown status {status}")))?;
        stats.set(kind, count.max(0) as u64);
    }
    Ok(stats)
}

#[async_trait]
impl JobStore for SqliteStore {
    async fn insert(&self, job: &Job) -> Result<(), StoreError> {
        let job = job.clone();
        self.call(move |conn| {
            let values = columns(&job)?;
            conn.execute(
                "INSERT INTO jobs (id, source, status, created_at, updated_at, data, url, title, resolver, parent_id, submitted_by, finished_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                rusqlite::params_from_iter(values.into_iter().chain(std::iter::once(
                    job.finished_at.map_or(Value::Null, |t| Value::Integer(nanos(t))),
                ))),
            )?;
            Ok(())
        })
        .await
    }

    async fn update(&self, job: &Job) -> Result<(), StoreError> {
        let job = job.clone();
        self.call(move |conn| {
            let values = columns(&job)?;
            let changed = conn.execute(
                "UPDATE jobs SET source = ?2, status = ?3, created_at = ?4, updated_at = ?5, data = ?6, url = ?7,
                 title = ?8, resolver = ?9, parent_id = ?10, submitted_by = ?11, finished_at = ?12 WHERE id = ?1",
                rusqlite::params_from_iter(values.into_iter().chain(std::iter::once(
                    job.finished_at.map_or(Value::Null, |t| Value::Integer(nanos(t))),
                ))),
            )?;
            if changed == 0 {
                return Err(StoreError::NotFound(job.id));
            }
            Ok(())
        })
        .await
    }

    async fn get(&self, id: JobId) -> Result<Option<Job>, StoreError> {
        self.call(move |conn| {
            let data: Option<String> = conn
                .query_row(
                    "SELECT data FROM jobs WHERE id = ?1",
                    params![id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            data.map(decode).transpose()
        })
        .await
    }

    async fn list(&self, filter: &JobFilter) -> Result<Vec<Job>, StoreError> {
        let filter = filter.clone();
        self.call(move |conn| {
            let (where_sql, mut values) = clauses(&filter);
            let order = match filter.order {
                Order::Newest => "DESC",
                Order::Oldest => "ASC",
            };
            let sql = format!(
                "SELECT data FROM jobs{where_sql} ORDER BY created_at {order}, id {order} LIMIT ? OFFSET ?"
            );
            values.push(Value::Integer(filter.effective_limit() as i64));
            values.push(Value::Integer(filter.offset.unwrap_or(0) as i64));
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params_from_iter(values), |row| {
                row.get::<_, String>(0)
            })?;
            rows.map(|row| decode(row?)).collect()
        })
        .await
    }

    async fn count(&self, filter: &JobFilter) -> Result<u64, StoreError> {
        let filter = filter.clone();
        self.call(move |conn| {
            let (where_sql, values) = clauses(&filter);
            let count: i64 = conn.query_row(
                &format!("SELECT COUNT(*) FROM jobs{where_sql}"),
                rusqlite::params_from_iter(values),
                |row| row.get(0),
            )?;
            Ok(count.max(0) as u64)
        })
        .await
    }

    async fn list_active(&self) -> Result<Vec<Job>, StoreError> {
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT data FROM jobs WHERE status IN ('queued', 'running') ORDER BY created_at ASC, id ASC",
            )?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.map(|row| decode(row?)).collect()
        })
        .await
    }

    async fn children(&self, parent: JobId) -> Result<Vec<Job>, StoreError> {
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT data FROM jobs WHERE parent_id = ?1 ORDER BY created_at ASC, id ASC",
            )?;
            let rows =
                stmt.query_map(params![parent.to_string()], |row| row.get::<_, String>(0))?;
            rows.map(|row| decode(row?)).collect()
        })
        .await
    }

    async fn delete(&self, id: JobId) -> Result<bool, StoreError> {
        self.call(move |conn| {
            Ok(conn.execute("DELETE FROM jobs WHERE id = ?1", params![id.to_string()])? > 0)
        })
        .await
    }

    async fn purge(
        &self,
        before: Timestamp,
        kinds: &[StatusKind],
    ) -> Result<Vec<JobId>, StoreError> {
        let kinds: Vec<String> = kinds.iter().map(|k| k.as_str().to_string()).collect();
        self.call(move |conn| {
            if kinds.is_empty() {
                return Ok(Vec::new());
            }
            let placeholders = vec!["?"; kinds.len()].join(", ");
            let mut values: Vec<Value> = vec![Value::Integer(nanos(before))];
            values.extend(kinds.into_iter().map(Value::Text));
            let mut stmt = conn.prepare(&format!(
                "DELETE FROM jobs WHERE created_at < ? AND status IN ({placeholders}) RETURNING id"
            ))?;
            let rows = stmt.query_map(rusqlite::params_from_iter(values), |row| {
                row.get::<_, String>(0)
            })?;
            let mut ids = Vec::new();
            for row in rows {
                let id = row?;
                ids.push(
                    id.parse()
                        .map_err(|e| StoreError::Corrupt(format!("job id {id}: {e}")))?,
                );
            }
            Ok(ids)
        })
        .await
    }

    async fn stats(&self) -> Result<Stats, StoreError> {
        self.call(move |conn| {
            read_stats(
                conn,
                "SELECT status, COUNT(*) FROM jobs GROUP BY status",
                Vec::new(),
            )
        })
        .await
    }

    async fn stats_since(&self, since: Timestamp) -> Result<Stats, StoreError> {
        self.call(move |conn| {
            read_stats(
                conn,
                "SELECT status, COUNT(*) FROM jobs WHERE created_at >= ? GROUP BY status",
                vec![Value::Integer(nanos(since))],
            )
        })
        .await
    }

    async fn resolver_stats(&self) -> Result<Vec<ResolverStats>, StoreError> {
        self.call(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT resolver, status, COUNT(*), MAX(updated_at) FROM jobs \
                 WHERE resolver IS NOT NULL AND status IN ('done', 'failed') GROUP BY resolver, status ORDER BY resolver",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                ))
            })?;
            let mut all: Vec<ResolverStats> = Vec::new();
            for row in rows {
                let (resolver, status, count, last) = row?;
                let entry = match all.iter_mut().find(|s| s.resolver == resolver) {
                    Some(entry) => entry,
                    None => {
                        all.push(ResolverStats {
                            resolver: resolver.clone(),
                            ..ResolverStats::default()
                        });
                        all.last_mut().expect("just pushed")
                    }
                };
                let at = last.and_then(|n| Timestamp::from_nanosecond(n as i128).ok());
                if status == "done" {
                    entry.done = count.max(0) as u64;
                    entry.last_done_at = at;
                } else {
                    entry.failed = count.max(0) as u64;
                    entry.last_failed_at = at;
                }
            }
            Ok(all)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{Origin, Request, SourceId};
    use url::Url;

    fn request(url: &str) -> Request {
        Request::new(
            Origin {
                source: SourceId::new("local"),
                reference: "web".into(),
                url: None,
            },
            Url::parse(url).unwrap(),
        )
    }

    #[tokio::test]
    async fn jobs_round_trip_with_search_columns_and_children() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let mut parent = Job::new(request("https://a.test/playlist"));
        parent.request.submitted_by = Some("nick".into());
        store.insert(&parent).await.unwrap();
        let mut child = Job::new(request("https://a.test/v1"));
        child.request.parent = Some(parent.id);
        let mut resolved = crate::resolve::Resolved::new("web");
        resolved.title = Some("First Clip".into());
        child.artifacts.resolved = Some(resolved);
        child.status = crate::job::JobStatus::Done;
        store.insert(&child).await.unwrap();

        assert_eq!(store.get(parent.id).await.unwrap().unwrap(), parent);
        assert_eq!(
            store.children(parent.id).await.unwrap(),
            vec![child.clone()]
        );
        let found = store
            .list(&JobFilter {
                q: Some("first clip".into()),
                ..JobFilter::default()
            })
            .await
            .unwrap();
        assert_eq!(found, vec![child.clone()]);
        let by_submitter = store
            .list(&JobFilter {
                q: Some("nick".into()),
                ..JobFilter::default()
            })
            .await
            .unwrap();
        assert_eq!(by_submitter.len(), 1);
        assert_eq!(by_submitter[0].id, parent.id);
        let top = store
            .list(&JobFilter {
                top_level: true,
                ..JobFilter::default()
            })
            .await
            .unwrap();
        assert_eq!(top.len(), 1);
        assert_eq!(
            store
                .count(&JobFilter {
                    resolver: Some("web".into()),
                    ..JobFilter::default()
                })
                .await
                .unwrap(),
            1
        );
        let oldest = store
            .list(&JobFilter {
                order: Order::Oldest,
                ..JobFilter::default()
            })
            .await
            .unwrap();
        assert_eq!(oldest[0].id, parent.id);
        let paged = store
            .list(&JobFilter {
                limit: Some(1),
                offset: Some(1),
                order: Order::Oldest,
                ..JobFilter::default()
            })
            .await
            .unwrap();
        assert_eq!(paged[0].id, child.id);
        let resolvers = store.resolver_stats().await.unwrap();
        assert_eq!(resolvers.len(), 1);
        assert_eq!(resolvers[0].done, 1);
        assert!(resolvers[0].last_done_at.is_some());

        assert!(store.delete(child.id).await.unwrap());
        assert!(!store.delete(child.id).await.unwrap());
        assert!(store.children(parent.id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn purge_removes_old_finished_jobs_only() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        let old = Timestamp::UNIX_EPOCH + jiff::SignedDuration::from_hours(24);
        let mut done = Job::new(request("https://a.test/done"));
        done.status = crate::job::JobStatus::Done;
        done.created_at = old;
        let mut failed = Job::new(request("https://a.test/failed"));
        failed.status = crate::job::JobStatus::Failed {
            stage: crate::job::Stage::Resolve,
            message: "x".into(),
        };
        failed.created_at = old;
        let mut queued = Job::new(request("https://a.test/queued"));
        queued.created_at = old;
        let fresh = Job::new(request("https://a.test/fresh"));
        for job in [&done, &failed, &queued, &fresh] {
            store.insert(job).await.unwrap();
        }
        let cutoff = Timestamp::UNIX_EPOCH + jiff::SignedDuration::from_hours(48);
        let removed = store.purge(cutoff, &[StatusKind::Done]).await.unwrap();
        assert_eq!(removed, vec![done.id]);
        let removed = store
            .purge(cutoff, &[StatusKind::Failed, StatusKind::Cancelled])
            .await
            .unwrap();
        assert_eq!(removed, vec![failed.id]);
        assert!(store.purge(cutoff, &[]).await.unwrap().is_empty());
        let stats = store.stats().await.unwrap();
        assert_eq!(stats.queued, 2);
        assert_eq!(stats.total(), 2);
        let recent = store.stats_since(cutoff).await.unwrap();
        assert_eq!(recent.queued, 1);
    }

    #[tokio::test]
    async fn backups_copy_the_database() {
        let store = SqliteStore::open_in_memory().await.unwrap();
        store
            .insert(&Job::new(request("https://a.test/v")))
            .await
            .unwrap();
        let dir = std::env::temp_dir().join(format!("discoclip-backup-{}", uuid::Uuid::now_v7()));
        let dest = dir.join("copy.db");
        store.backup_to(&dest).await.unwrap();
        let copy = SqliteStore::open(&dest).await.unwrap();
        assert_eq!(copy.stats().await.unwrap().queued, 1);
        assert!(copy.size_on_disk().await.unwrap() > 0);
        assert_eq!(store.size_on_disk().await.unwrap(), 0);
        drop(copy);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
