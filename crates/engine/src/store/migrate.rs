//! Versioned schema migrations. Each scope, such as the engine's own tables or the
//! application's, keeps its own ordered list; a database records which versions of each
//! scope it has, and opening applies the rest inside one transaction per migration.

use jiff::Timestamp;
use rusqlite::{Connection, TransactionBehavior, params};

use crate::store::StoreError;

/// One step of a scope's schema, applied once and never changed afterwards.
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    /// Versions start at 1 and rise by one per migration.
    pub version: u32,
    pub name: &'static str,
    pub sql: &'static str,
}

const LEDGER: &str = "
CREATE TABLE IF NOT EXISTS schema_migrations (
    scope TEXT NOT NULL,
    version INTEGER NOT NULL,
    name TEXT NOT NULL,
    applied_at INTEGER NOT NULL,
    PRIMARY KEY (scope, version)
);
";

/// Applies every migration of `scope` the database does not have yet, in order, and
/// returns how many were applied. Refuses a database that is ahead of `migrations`, since
/// an older build cannot know what a newer schema means.
pub fn apply(
    conn: &mut Connection,
    scope: &str,
    migrations: &[Migration],
) -> Result<usize, StoreError> {
    check_order(scope, migrations)?;
    conn.execute_batch(LEDGER)?;
    let mut applied = 0;
    loop {
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current: u32 = tx.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_migrations WHERE scope = ?1",
            params![scope],
            |row| row.get(0),
        )?;
        let latest = migrations.last().map_or(0, |m| m.version);
        if current > latest {
            return Err(StoreError::Schema(format!(
                "database has {scope} schema version {current}, but this build knows only up to {latest}"
            )));
        }
        let Some(next) = migrations.iter().find(|m| m.version == current + 1) else {
            return Ok(applied);
        };
        tx.execute_batch(next.sql).map_err(|e| {
            StoreError::Schema(format!(
                "{scope} migration {} ({}): {e}",
                next.version, next.name
            ))
        })?;
        tx.execute(
            "INSERT INTO schema_migrations (scope, version, name, applied_at) VALUES (?1, ?2, ?3, ?4)",
            params![scope, next.version, next.name, Timestamp::now().as_millisecond()],
        )?;
        tx.commit()?;
        applied += 1;
    }
}

fn check_order(scope: &str, migrations: &[Migration]) -> Result<(), StoreError> {
    for (index, migration) in migrations.iter().enumerate() {
        let expected = index as u32 + 1;
        if migration.version != expected {
            return Err(StoreError::Schema(format!(
                "{scope} migration list is out of order: found version {} where {expected} was expected",
                migration.version
            )));
        }
    }
    Ok(())
}

/// The versions of `scope` a database has, with when each was applied.
pub fn applied(
    conn: &Connection,
    scope: &str,
) -> Result<Vec<(u32, String, Timestamp)>, StoreError> {
    conn.execute_batch(LEDGER)?;
    let mut stmt = conn.prepare(
        "SELECT version, name, applied_at FROM schema_migrations WHERE scope = ?1 ORDER BY version",
    )?;
    let rows = stmt.query_map(params![scope], |row| {
        Ok((
            row.get::<_, u32>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    rows.map(|row| {
        let (version, name, at) = row?;
        let at = Timestamp::from_millisecond(at)
            .map_err(|e| StoreError::Corrupt(format!("schema_migrations: {e}")))?;
        Ok((version, name, at))
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE: Migration = Migration {
        version: 1,
        name: "things",
        sql: "CREATE TABLE things (id INTEGER PRIMARY KEY, name TEXT NOT NULL);",
    };
    const TWO: Migration = Migration {
        version: 2,
        name: "things.size",
        sql: "ALTER TABLE things ADD COLUMN size INTEGER NOT NULL DEFAULT 0;",
    };

    fn columns(conn: &Connection, table: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .unwrap();
        stmt.query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    #[test]
    fn applies_missing_migrations_in_order_and_records_them() {
        let mut conn = Connection::open_in_memory().unwrap();
        assert_eq!(apply(&mut conn, "test", &[ONE]).unwrap(), 1);
        assert_eq!(columns(&conn, "things"), vec!["id", "name"]);
        assert_eq!(apply(&mut conn, "test", &[ONE]).unwrap(), 0);
        assert_eq!(apply(&mut conn, "test", &[ONE, TWO]).unwrap(), 1);
        assert_eq!(columns(&conn, "things"), vec!["id", "name", "size"]);
        let ledger = applied(&conn, "test").unwrap();
        assert_eq!(ledger.len(), 2);
        assert_eq!(ledger[0].0, 1);
        assert_eq!(ledger[0].1, "things");
        assert_eq!(ledger[1].0, 2);
        assert!(applied(&conn, "other").unwrap().is_empty());
    }

    #[test]
    fn scopes_are_independent() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply(&mut conn, "a", &[ONE]).unwrap();
        let other = Migration {
            version: 1,
            name: "widgets",
            sql: "CREATE TABLE widgets (id INTEGER PRIMARY KEY);",
        };
        assert_eq!(apply(&mut conn, "b", &[other]).unwrap(), 1);
        assert_eq!(applied(&conn, "a").unwrap().len(), 1);
        assert_eq!(applied(&conn, "b").unwrap().len(), 1);
    }

    #[test]
    fn a_newer_database_is_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        apply(&mut conn, "test", &[ONE, TWO]).unwrap();
        let error = apply(&mut conn, "test", &[ONE]).unwrap_err();
        assert!(matches!(error, StoreError::Schema(message) if message.contains("version 2")));
    }

    #[test]
    fn an_out_of_order_list_is_refused() {
        let mut conn = Connection::open_in_memory().unwrap();
        assert!(matches!(
            apply(&mut conn, "test", &[TWO, ONE]),
            Err(StoreError::Schema(_))
        ));
        assert!(matches!(
            apply(&mut conn, "test", &[ONE, ONE]),
            Err(StoreError::Schema(_))
        ));
    }

    #[test]
    fn a_failing_migration_leaves_no_trace() {
        let mut conn = Connection::open_in_memory().unwrap();
        let broken = Migration {
            version: 2,
            name: "broken",
            sql: "CREATE TABLE ok (id INTEGER); ALTER TABLE missing ADD COLUMN x INTEGER;",
        };
        let error = apply(&mut conn, "test", &[ONE, broken]).unwrap_err();
        assert!(matches!(error, StoreError::Schema(message) if message.contains("broken")));
        assert_eq!(applied(&conn, "test").unwrap().len(), 1);
        assert!(conn.prepare("SELECT * FROM ok").is_err());
    }
}
