//! Backups of the database: SQLite's online backup copies it whole and consistent while
//! the server runs, on a schedule and on request, into a directory that keeps the newest
//! few. The browser coordinates a service restart to restore a backup; the standalone
//! restore command is also available when the server is stopped.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use discoclip_engine::StoreError;
use discoclip_engine::rusqlite::{self, Connection, OpenFlags};
use discoclip_engine::store::sqlite::SqliteStore;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::secrets::KEY_FILE;

/// The database file's name under the data directory.
pub const DATABASE_FILE: &str = "discoclip.db";
const PREFIX: &str = "discoclip-";
const SUFFIX: &str = ".db";
/// How long after startup the scheduler first looks at whether a backup is due.
const STARTUP_DELAY: Duration = Duration::from_secs(20);
/// How often the scheduler looks at whether a backup is due.
const TICK: Duration = Duration::from_secs(60);

/// When and where the database is backed up, and how many backups are kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackupConfig {
    pub enabled: bool,
    pub dir: PathBuf,
    /// How often a backup is made. One is also made at startup when the newest is older
    /// than this.
    pub interval_secs: u64,
    /// How many backups are kept. Older ones are removed after each new one.
    pub keep: u32,
}

impl Default for BackupConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: PathBuf::from("data/backups"),
            interval_secs: 24 * 60 * 60,
            keep: 7,
        }
    }
}

/// The `backup` settings as the service reads them, replaced when they change.
pub type SharedBackupConfig = Arc<RwLock<BackupConfig>>;

/// One backup file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackupEntry {
    pub name: String,
    pub bytes: u64,
    /// When it was made, from its name.
    pub at: Timestamp,
}

/// The backups so far.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BackupStatus {
    pub last_at: Option<Timestamp>,
    pub last_bytes: Option<u64>,
    pub last_error: Option<String>,
    pub runs: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("backups are turned off")]
    Disabled,
    #[error("{0} is not the name of a backup")]
    BadName(String),
    #[error("backup {0} not found")]
    NotFound(String),
    #[error("{0}")]
    Busy(String),
    #[error("{path}: {message}")]
    Unusable { path: PathBuf, message: String },
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Makes, lists, hands out and removes backups.
pub struct Backups {
    db: SqliteStore,
    data_dir: PathBuf,
    config: SharedBackupConfig,
    status: Mutex<BackupStatus>,
    /// Held while a backup is made, so one asked for by hand does not overlap the
    /// scheduled one.
    running: tokio::sync::Mutex<()>,
    pub restores: Arc<crate::restore::Restores>,
}

impl Backups {
    pub fn new(db: SqliteStore, data_dir: PathBuf, config: SharedBackupConfig) -> Self {
        Self {
            db,
            data_dir,
            config,
            status: Mutex::new(BackupStatus::default()),
            running: tokio::sync::Mutex::new(()),
            restores: Arc::new(crate::restore::Restores::default()),
        }
    }

    pub fn with_restores(mut self, restores: Arc<crate::restore::Restores>) -> Self {
        self.restores = restores;
        self
    }

    pub fn config(&self) -> BackupConfig {
        self.config
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The settings the service reads, for the parts that replace them.
    pub fn config_handle(&self) -> SharedBackupConfig {
        self.config.clone()
    }

    pub fn status(&self) -> BackupStatus {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The file a backup name stands for. Only names this service makes are taken.
    pub fn path_of(&self, name: &str) -> Result<PathBuf, BackupError> {
        if time_of(name).is_none() {
            return Err(BackupError::BadName(name.to_string()));
        }
        Ok(self.config().dir.join(name))
    }

    /// Every backup in the directory, newest first.
    pub async fn list(&self) -> Result<Vec<BackupEntry>, BackupError> {
        let dir = self.config().dir;
        let mut entries = Vec::new();
        let mut listing = match tokio::fs::read_dir(&dir).await {
            Ok(listing) => listing,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(entries),
            Err(error) => return Err(error.into()),
        };
        while let Some(entry) = listing.next_entry().await? {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some(at) = time_of(&name) else {
                continue;
            };
            let metadata = entry.metadata().await?;
            if !metadata.is_file() {
                continue;
            }
            entries.push(BackupEntry {
                name,
                bytes: metadata.len(),
                at,
            });
        }
        entries.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| b.name.cmp(&a.name)));
        Ok(entries)
    }

    /// Makes a backup now, whatever the schedule says, and removes the ones past `keep`.
    /// The key the database's secrets are sealed with is copied beside the backups, since
    /// a backup is worthless without it.
    pub async fn run(&self) -> Result<BackupEntry, BackupError> {
        let _running = self.running.lock().await;
        let config = self.config();
        self.restores.check_idle()?;
        let result = self.make(&config).await;
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        status.runs += 1;
        match &result {
            Ok(entry) => {
                status.last_at = Some(entry.at);
                status.last_bytes = Some(entry.bytes);
                status.last_error = None;
                tracing::info!(name = entry.name, bytes = entry.bytes, "database backed up");
            }
            Err(error) => {
                status.last_error = Some(error.to_string());
                tracing::error!("database backup failed: {error}");
            }
        }
        result
    }

    async fn make(&self, config: &BackupConfig) -> Result<BackupEntry, BackupError> {
        tokio::fs::create_dir_all(&config.dir).await?;
        let at = Timestamp::now();
        let name = unique_name(at);
        let dest = config.dir.join(&name);
        let part = config.dir.join(format!("{name}.part"));
        let _ = tokio::fs::remove_file(&part).await;
        self.db.backup_to(&part).await?;
        tokio::fs::rename(&part, &dest).await?;
        let key = self.data_dir.join(KEY_FILE);
        let key_copy = config.dir.join(KEY_FILE);
        if key.is_file() && !key_copy.exists() {
            tokio::fs::copy(&key, &key_copy).await?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                tokio::fs::set_permissions(&key_copy, std::fs::Permissions::from_mode(0o600))
                    .await?;
            }
        }
        let bytes = tokio::fs::metadata(&dest).await?.len();
        self.rotate(config).await?;
        Ok(BackupEntry { name, bytes, at })
    }

    /// Removes every backup past the newest `keep`.
    async fn rotate(&self, config: &BackupConfig) -> Result<(), BackupError> {
        let entries = self.list().await?;
        for old in entries.iter().skip(config.keep.max(1) as usize) {
            tokio::fs::remove_file(config.dir.join(&old.name)).await?;
            tracing::info!(name = old.name, "old backup removed");
        }
        Ok(())
    }

    pub async fn delete(&self, name: &str) -> Result<(), BackupError> {
        let _running = self.running.lock().await;
        self.restores.check_idle()?;
        let path = self.path_of(name)?;
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Err(BackupError::NotFound(name.to_string()))
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Stage and validate a restore while holding off rotation and deletion of its source.
    pub async fn prepare_restore(
        &self,
        name: &str,
        actor: crate::audit::Actor,
        settings: crate::settings::Settings,
    ) -> Result<crate::restore::RestoreStatus, BackupError> {
        let _running = self.running.lock().await;
        self.restores.check_idle()?;
        let source = self.path_of(name)?;
        if !source.is_file() {
            return Err(BackupError::NotFound(name.to_owned()));
        }
        self.restores
            .prepare(&source, &self.data_dir, actor, settings)
            .await
    }

    /// Whether a backup is due: none yet, or the newest older than the interval.
    async fn due(&self, config: &BackupConfig) -> Result<bool, BackupError> {
        let newest = self.list().await?.into_iter().next();
        let interval = SignedDuration::from_secs(config.interval_secs.min(i64::MAX as u64) as i64);
        Ok(match newest {
            Some(entry) => Timestamp::now().duration_since(entry.at) >= interval,
            None => true,
        })
    }

    /// Makes backups on the settings' interval until `shutdown`: one at startup when the
    /// newest is older than the interval, then whenever the newest ages past it.
    pub async fn schedule(self: Arc<Self>, shutdown: CancellationToken) {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tokio::time::sleep(STARTUP_DELAY) => {}
        }
        loop {
            let config = self.config();
            if config.enabled {
                match self.due(&config).await {
                    Ok(true) => {
                        let _ = self.run().await;
                    }
                    Ok(false) => {}
                    Err(error) => tracing::error!("backups not listed: {error}"),
                }
            }
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(TICK) => {}
            }
        }
    }
}

/// `discoclip-20260924T171500Z.db` for a moment.
fn name_for(at: Timestamp) -> String {
    format!("{PREFIX}{}{SUFFIX}", at.strftime("%Y%m%dT%H%M%SZ"))
}

pub(crate) fn unique_name(at: Timestamp) -> String {
    format!(
        "{}-{}{SUFFIX}",
        name_for(at).trim_end_matches(SUFFIX),
        uuid::Uuid::now_v7().simple()
    )
}

/// The moment a backup's name carries, when it is one of ours.
fn time_of(name: &str) -> Option<Timestamp> {
    let raw = name.strip_prefix(PREFIX)?.strip_suffix(SUFFIX)?;
    let stamp = if let Some((stamp, id)) = raw.split_once('-') {
        uuid::Uuid::parse_str(id).ok()?;
        stamp
    } else {
        raw
    };
    if stamp.len() != 16 || !stamp.ends_with('Z') || stamp.as_bytes()[8] != b'T' {
        return None;
    }
    if !stamp.is_ascii() {
        return None;
    }
    let digits = |range: std::ops::Range<usize>| -> Option<i64> {
        let text = &stamp[range];
        text.bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| text.parse().ok())?
    };
    let civil = jiff::civil::Date::new(
        digits(0..4)? as i16,
        digits(4..6)? as i8,
        digits(6..8)? as i8,
    )
    .ok()?
    .to_datetime(
        jiff::civil::Time::new(
            digits(9..11)? as i8,
            digits(11..13)? as i8,
            digits(13..15)? as i8,
            0,
        )
        .ok()?,
    );
    civil
        .to_zoned(jiff::tz::TimeZone::UTC)
        .ok()
        .map(|z| z.timestamp())
}

/// Puts `backup` in place of the database under `data_dir`. The server has to be stopped:
/// the database is taken with an exclusive lock first, and a server holding it stops the
/// restore. The backup is checked whole before anything is touched. A `secret.key` beside
/// the backup is put beside the database when there is none there.
pub(crate) fn checked(backup: &Path) -> Result<Connection, BackupError> {
    let unusable = |message: String| BackupError::Unusable {
        path: backup.to_path_buf(),
        message,
    };
    if !backup.is_file() {
        return Err(unusable("no such file".into()));
    }
    let check = Connection::open_with_flags(backup, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| unusable(format!("not a database: {e}")))?;
    let integrity: String = check
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .map_err(|e| unusable(format!("integrity check failed: {e}")))?;
    if integrity != "ok" {
        return Err(unusable(format!("integrity check: {integrity}")));
    }
    let tables: i64 = check
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name IN ('jobs', 'settings', 'users')",
            [],
            |row| row.get(0),
        )
        .map_err(|e| unusable(format!("tables not read: {e}")))?;
    if tables < 3 {
        return Err(unusable("not a DiscoClip database".into()));
    }
    Ok(check)
}

/// Copy a complete SQLite snapshot without copying a live WAL file by hand.
pub(crate) fn snapshot(source: &Path, target: &Path) -> Result<(), BackupError> {
    let source = checked(source)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut target = Connection::open(target).map_err(StoreError::from)?;
    let copy = rusqlite::backup::Backup::new(&source, &mut target).map_err(StoreError::from)?;
    copy.run_to_completion(256, Duration::from_millis(5), None)
        .map_err(StoreError::from)?;
    Ok(())
}

pub fn restore(data_dir: &Path, backup: &Path) -> Result<(), BackupError> {
    drop(checked(backup)?);

    let live = data_dir.join(DATABASE_FILE);
    std::fs::create_dir_all(data_dir)?;
    if live.exists() {
        let lock = Connection::open(&live).map_err(|e| {
            BackupError::Busy(format!(
                "the database at {} cannot be opened: {e}",
                live.display()
            ))
        })?;
        lock.busy_timeout(Duration::from_millis(0))
            .map_err(|e| BackupError::Busy(e.to_string()))?;
        if let Err(error) = lock.execute_batch("PRAGMA locking_mode = EXCLUSIVE; BEGIN EXCLUSIVE;")
        {
            return Err(match error {
                rusqlite::Error::SqliteFailure(code, _)
                    if code.code == rusqlite::ErrorCode::DatabaseBusy
                        || code.code == rusqlite::ErrorCode::DatabaseLocked =>
                {
                    BackupError::Busy(
                        "the server is running and holds the database. Stop it, then restore."
                            .into(),
                    )
                }
                other => BackupError::Busy(format!("the database cannot be locked: {other}")),
            });
        }
        drop(lock);
    }
    for suffix in ["-wal", "-shm"] {
        let side = data_dir.join(format!("{DATABASE_FILE}{suffix}"));
        if side.exists() {
            std::fs::remove_file(&side)?;
        }
    }
    let part = data_dir.join(format!("{DATABASE_FILE}.restore"));
    std::fs::copy(backup, &part)?;
    std::fs::rename(&part, &live)?;
    let key = data_dir.join(KEY_FILE);
    let beside = backup
        .parent()
        .map(|dir| dir.join(KEY_FILE))
        .unwrap_or_default();
    if !key.exists() && beside.is_file() {
        std::fs::copy(&beside, &key)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("discoclip-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn names_carry_their_moment() {
        let at = "2026-09-24T17:15:00Z".parse::<Timestamp>().unwrap();
        let name = name_for(at);
        assert_eq!(name, "discoclip-20260924T171500Z.db");
        assert_eq!(time_of(&name), Some(at));
        assert_eq!(time_of("discoclip-2026.db"), None);
        assert_eq!(time_of("other-20260924T171500Z.db"), None);
        assert_eq!(time_of("discoclip-20260924T171500Z.db.part"), None);
        assert_eq!(time_of("discoclip-2026092xT171500Z.db"), None);
    }

    #[tokio::test]
    async fn backups_are_made_listed_rotated_and_restored() {
        let dir = temp_dir("backup");
        let data_dir = dir.join("data");
        std::fs::create_dir_all(&data_dir).unwrap();
        std::fs::write(data_dir.join(KEY_FILE), b"k").unwrap();
        let db = SqliteStore::open(&data_dir.join(DATABASE_FILE))
            .await
            .unwrap();
        crate::migrations::apply(&db).await.unwrap();
        let users = crate::users::UserStore::new(db.clone());
        users.set_up("nick", "correct horse").await.unwrap();
        let config = Arc::new(RwLock::new(BackupConfig {
            enabled: true,
            dir: dir.join("backups"),
            interval_secs: 3600,
            keep: 2,
        }));
        let backups = Backups::new(db.clone(), data_dir.clone(), config.clone());
        assert!(backups.list().await.unwrap().is_empty());
        assert!(backups.due(&backups.config()).await.unwrap());

        let first = backups.run().await.unwrap();
        assert!(first.bytes > 0);
        assert!(dir.join("backups").join(KEY_FILE).is_file());
        assert!(!backups.due(&backups.config()).await.unwrap());
        let status = backups.status();
        assert_eq!(status.runs, 1);
        assert_eq!(status.last_at, Some(first.at));
        assert!(status.last_error.is_none());

        // Two more, a second apart in name, leave the newest two.
        let older = dir.join("backups").join("discoclip-20200101T000000Z.db");
        std::fs::copy(dir.join("backups").join(&first.name), &older).unwrap();
        let listed = backups.list().await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].name, first.name);
        assert_eq!(listed[1].name, "discoclip-20200101T000000Z.db");
        let second_name = loop {
            let entry = backups.run().await.unwrap();
            if entry.name != first.name {
                break entry.name;
            }
            tokio::time::sleep(Duration::from_millis(300)).await;
        };
        let listed = backups.list().await.unwrap();
        assert_eq!(listed.len(), 2, "{listed:?}");
        assert_eq!(listed[0].name, second_name);
        assert_eq!(listed[1].name, first.name);
        assert!(!older.exists());

        assert!(matches!(
            backups.path_of("../etc/passwd"),
            Err(BackupError::BadName(_))
        ));
        assert!(matches!(
            backups.delete("discoclip-20200101T000000Z.db").await,
            Err(BackupError::NotFound(_))
        ));
        backups.delete(&first.name).await.unwrap();
        assert_eq!(backups.list().await.unwrap().len(), 1);

        config.write().unwrap().enabled = false;
        // Turning off the schedule does not prevent a backup requested by a person.
        assert!(backups.run().await.is_ok());

        // A restore is refused while the database is held, and goes through once it is not.
        let backup = dir.join("backups").join(&second_name);
        let held = Connection::open(data_dir.join(DATABASE_FILE)).unwrap();
        held.execute_batch("BEGIN IMMEDIATE;").unwrap();
        let refused = restore(&data_dir, &backup).unwrap_err();
        assert!(matches!(refused, BackupError::Busy(_)), "{refused}");
        drop(held);
        drop(users);
        drop(db);
        let fresh_data = dir.join("fresh");
        restore(&fresh_data, &backup).unwrap();
        assert!(fresh_data.join(DATABASE_FILE).is_file());
        assert!(fresh_data.join(KEY_FILE).is_file());
        let restored = SqliteStore::open(&fresh_data.join(DATABASE_FILE))
            .await
            .unwrap();
        let users = crate::users::UserStore::new(restored);
        assert!(users.is_set_up().await.unwrap());

        std::fs::write(dir.join("junk.db"), b"not a database").unwrap();
        assert!(matches!(
            restore(&fresh_data, &dir.join("junk.db")),
            Err(BackupError::Unusable { .. })
        ));
        assert!(matches!(
            restore(&fresh_data, &dir.join("missing.db")),
            Err(BackupError::Unusable { .. })
        ));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
