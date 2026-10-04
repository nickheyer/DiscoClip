//! Browser-initiated restores. A private, validated copy is staged first. The
//! composition root stops its runtime before replacing the database, then starts
//! a fresh runtime so no worker or in-memory cache survives the restore.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use discoclip_engine::StoreError;
use discoclip_engine::store::sqlite::SqliteStore;
use jiff::Timestamp;
use serde::Serialize;
use uuid::Uuid;

use crate::audit::{Action, Actor, AuditStore, Target};
use crate::backup::{self, BackupError, DATABASE_FILE};
use crate::settings::{Change, Settings, SettingsStore};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    Stopping,
    Restoring,
    Starting,
    Complete,
    Failed,
}

#[derive(Debug, Clone, Serialize)]
pub struct RestoreStatus {
    /// An unguessable receipt: usable after the restored database ends the session.
    pub id: Uuid,
    pub phase: Phase,
    pub error: Option<String>,
}

pub struct PreparedRestore {
    pub staged: PathBuf,
    pub safety: PathBuf,
    pub data_dir: PathBuf,
}

impl Drop for PreparedRestore {
    fn drop(&mut self) {
        // Only temporary files belonging to this request, never a saved backup.
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", self.staged.display()));
        }
    }
}

#[derive(Default)]
pub struct Restores {
    status: Mutex<Option<RestoreStatus>>,
    pending: Mutex<Option<PreparedRestore>>,
}

impl Restores {
    pub fn status(&self, id: Uuid) -> Option<RestoreStatus> {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .filter(|status| status.id == id)
            .cloned()
    }

    pub fn check_idle(&self) -> Result<(), BackupError> {
        let status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        if status
            .as_ref()
            .is_some_and(|status| !matches!(status.phase, Phase::Complete | Phase::Failed))
        {
            return Err(BackupError::Busy(
                "A database restore is already in progress.".into(),
            ));
        }
        Ok(())
    }

    pub fn starting(&self) -> bool {
        self.status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|status| status.phase == Phase::Starting)
    }

    pub fn ready(&self) {
        let mut status = self.status.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(status) = status
            .as_mut()
            .filter(|status| status.phase == Phase::Starting)
        {
            status.phase = Phase::Complete;
        }
    }

    pub fn fail(&self, message: &str) {
        if let Some(status) = self
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            status.phase = Phase::Failed;
            status.error = Some(message.to_owned());
        }
    }

    fn phase(&self, phase: Phase) {
        if let Some(status) = self
            .status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
        {
            status.phase = phase;
        }
    }

    pub fn take_pending(&self) -> Option<PreparedRestore> {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    /// Called under the backups' operation lock. Does not touch the active database.
    pub async fn prepare(
        &self,
        source: &Path,
        data_dir: &Path,
        actor: Actor,
        current: Settings,
    ) -> Result<RestoreStatus, BackupError> {
        self.check_idle()?;
        let id = Uuid::new_v4();
        tokio::fs::create_dir_all(data_dir).await?;
        let prepared = PreparedRestore {
            staged: data_dir.join(format!(".restore-{id}.db")),
            safety: current
                .backup
                .dir
                .join(backup::unique_name(Timestamp::now())),
            data_dir: data_dir.to_path_buf(),
        };
        let original = source.to_path_buf();
        let staged = prepared.staged.clone();
        tokio::task::spawn_blocking(move || backup::snapshot(&original, &staged))
            .await
            .map_err(|e| StoreError::Database(e.to_string()))??;

        let db = SqliteStore::open(&prepared.staged)
            .await
            .map_err(|error| incompatible(source, error))?;
        let result = async {
            crate::migrations::apply(&db)
                .await
                .map_err(|error| incompatible(source, error))?;
            let settings = SettingsStore::new(db.clone());
            // Validate before applying the connection exceptions, too.
            settings.load().await.map_err(|e| unusable(source, e))?;
            settings.apply(&actor, Action::SettingsSet, &Change {
                set: [
                    ("web".into(), serde_json::to_value(&current.web).map_err(StoreError::from)?),
                    ("backup.dir".into(), serde_json::to_value(&current.backup.dir).map_err(StoreError::from)?),
                ].into_iter().collect(),
                reset: Vec::new(),
            }).await.map_err(|e| unusable(source, e))?;
            db.call(|conn| {
                conn.execute_batch("DELETE FROM sessions; DELETE FROM frontend_sessions;")?;
                Ok(())
            }).await?;
            AuditStore::new(db.clone()).record(
                &actor,
                Action::BackupRestore,
                Target::backup(&source.file_name().unwrap_or_default().to_string_lossy()),
                serde_json::json!({
                    "name": source.file_name().unwrap_or_default().to_string_lossy(),
                    "safety_backup": prepared.safety.file_name().unwrap_or_default().to_string_lossy(),
                }),
            ).await?;
            db.call(|conn| {
                conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
                Ok(())
            }).await?;
            Ok::<(), BackupError>(())
        }.await;
        let closed = db.close().await;
        result?;
        closed?;

        let status = RestoreStatus {
            id,
            phase: Phase::Stopping,
            error: None,
        };
        *self.status.lock().unwrap_or_else(|e| e.into_inner()) = Some(status.clone());
        *self.pending.lock().unwrap_or_else(|e| e.into_inner()) = Some(prepared);
        Ok(status)
    }

    /// Runs only after every service and its runtime have stopped. The safety copy is
    /// deliberately not rotated here, even when the retention count is one.
    pub fn apply(&self, prepared: &PreparedRestore) -> Result<(), BackupError> {
        self.phase(Phase::Restoring);
        let partial = prepared.safety.with_extension("db.part");
        if let Err(error) = backup::snapshot(&prepared.data_dir.join(DATABASE_FILE), &partial) {
            let _ = std::fs::remove_file(&partial);
            return Err(error);
        }
        std::fs::rename(&partial, &prepared.safety)?;
        let key = prepared.data_dir.join(crate::secrets::KEY_FILE);
        if key.is_file() {
            let beside = prepared
                .safety
                .parent()
                .unwrap()
                .join(crate::secrets::KEY_FILE);
            if !beside.exists() {
                std::fs::copy(&key, &beside)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(beside, std::fs::Permissions::from_mode(0o600))?;
                }
            }
        }
        backup::restore(&prepared.data_dir, &prepared.staged)?;
        self.phase(Phase::Starting);
        Ok(())
    }
}

fn unusable(path: &Path, error: impl std::fmt::Display) -> BackupError {
    BackupError::Unusable {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

fn incompatible(path: &Path, error: StoreError) -> BackupError {
    match error {
        error @ StoreError::Schema(_) => unusable(path, error),
        other => other.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backup::{BackupConfig, Backups};
    use std::sync::{Arc, RwLock};

    #[tokio::test]
    async fn restore_preserves_a_safety_copy_connection_and_backup_location() {
        let dir = crate::web::testing::test_dir("restore");
        let db = SqliteStore::open(&dir.join(DATABASE_FILE)).await.unwrap();
        crate::migrations::apply(&db).await.unwrap();
        crate::users::UserStore::new(db.clone())
            .set_up("nick", "correct horse")
            .await
            .unwrap();
        let config = BackupConfig {
            dir: dir.join("backups"),
            keep: 1,
            ..BackupConfig::default()
        };
        let backups = Backups::new(
            db.clone(),
            dir.clone(),
            Arc::new(RwLock::new(config.clone())),
        );
        let settings = SettingsStore::new(db.clone());
        settings
            .apply(
                &Actor::test(),
                Action::SettingsSet,
                &Change {
                    set: [("log.level".into(), serde_json::json!("warn"))]
                        .into_iter()
                        .collect(),
                    reset: vec![],
                },
            )
            .await
            .unwrap();
        let original = backups.run().await.unwrap();
        settings
            .apply(
                &Actor::test(),
                Action::SettingsSet,
                &Change {
                    set: [
                        ("log.level".into(), serde_json::json!("debug")),
                        ("web.bind".into(), serde_json::json!("127.0.0.1:9123")),
                        ("backup.dir".into(), serde_json::json!(config.dir)),
                    ]
                    .into_iter()
                    .collect(),
                    reset: vec![],
                },
            )
            .await
            .unwrap();
        let current = settings.load().await.unwrap();
        let status = backups
            .prepare_restore(&original.name, Actor::test(), current.clone())
            .await
            .unwrap();
        assert_eq!(status.phase, Phase::Stopping);
        assert!(backups.restores.status(Uuid::new_v4()).is_none());
        assert_eq!(
            settings.load().await.unwrap().log.level,
            "debug",
            "preparing must not alter the live database"
        );
        assert!(matches!(backups.run().await, Err(BackupError::Busy(_))));
        assert!(matches!(
            backups.delete(&original.name).await,
            Err(BackupError::Busy(_))
        ));
        assert!(matches!(
            backups
                .prepare_restore(&original.name, Actor::test(), current.clone())
                .await,
            Err(BackupError::Busy(_))
        ));

        let old_worker = db.clone();
        db.close().await.unwrap();
        let pending = backups.restores.take_pending().unwrap();
        backups.restores.apply(&pending).unwrap();
        assert!(
            old_worker
                .call(|conn| {
                    conn.execute_batch("DROP TABLE settings")?;
                    Ok(())
                })
                .await
                .is_err(),
            "a stale worker cannot write after restore"
        );
        let restored = SqliteStore::open(&dir.join(DATABASE_FILE)).await.unwrap();
        let restored_settings = SettingsStore::new(restored.clone()).load().await.unwrap();
        assert_eq!(restored_settings.log.level, "warn");
        assert_eq!(restored_settings.web, current.web);
        assert_eq!(restored_settings.backup.dir, config.dir);
        let safety = SqliteStore::open(&pending.safety).await.unwrap();
        assert_eq!(
            SettingsStore::new(safety.clone())
                .load()
                .await
                .unwrap()
                .log
                .level,
            "debug"
        );
        assert!(backups.path_of(&original.name).unwrap().is_file());
        assert_eq!(
            backups.list().await.unwrap().len(),
            2,
            "keep=1 must not rotate the safety copy or selected backup during restore"
        );
        let audit: String = restored
            .call(|conn| {
                Ok(conn.query_row(
                    "SELECT action FROM audit_log WHERE action = 'backup.restore'",
                    [],
                    |row| row.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(audit, "backup.restore");
        backups.restores.ready();
        assert_eq!(
            backups.restores.status(status.id).unwrap().phase,
            Phase::Complete
        );
        restored.close().await.unwrap();
        safety.close().await.unwrap();
        drop(pending);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn restore_rejects_corrupt_or_newer_backups_before_stopping() {
        let dir = crate::web::testing::test_dir("restore-invalid");
        let db = SqliteStore::open(&dir.join(DATABASE_FILE)).await.unwrap();
        crate::migrations::apply(&db).await.unwrap();
        let config = BackupConfig {
            dir: dir.join("backups"),
            ..BackupConfig::default()
        };
        let backups = Backups::new(db.clone(), dir.clone(), Arc::new(RwLock::new(config)));
        let entry = backups.run().await.unwrap();
        let source = backups.path_of(&entry.name).unwrap();
        let newer = discoclip_engine::rusqlite::Connection::open(&source).unwrap();
        newer.execute("INSERT INTO schema_migrations (scope, version, name, applied_at) VALUES ('engine', 9999, 'future', 0)", []).unwrap();
        drop(newer);
        assert!(
            backups
                .prepare_restore(&entry.name, Actor::test(), Settings::default())
                .await
                .is_err()
        );
        assert!(backups.restores.take_pending().is_none());
        backups.restores.check_idle().unwrap();
        std::fs::write(&source, b"not a database").unwrap();
        assert!(matches!(
            backups
                .prepare_restore(&entry.name, Actor::test(), Settings::default())
                .await,
            Err(BackupError::Unusable { .. })
        ));
        assert!(backups.restores.take_pending().is_none());
        assert!(SettingsStore::new(db.clone()).load().await.is_ok());
        db.close().await.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
