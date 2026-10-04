//! Conventions shared by the application's tables.

use discoclip_engine::StoreError;
use discoclip_engine::rusqlite::Transaction;
use discoclip_engine::store::sqlite::SqliteStore;
use jiff::Timestamp;

/// Timestamps are stored as nanoseconds since the Unix epoch.
pub fn nanos(ts: Timestamp) -> i64 {
    ts.as_nanosecond().clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

pub fn timestamp(column: &str, nanos: i64) -> Result<Timestamp, StoreError> {
    Timestamp::from_nanosecond(nanos as i128)
        .map_err(|e| StoreError::Corrupt(format!("{column}: {e}")))
}

/// Runs `f` in one transaction on the database thread, committing when it succeeds.
pub async fn transact<T, E, F>(db: &SqliteStore, f: F) -> Result<T, E>
where
    T: Send + 'static,
    E: From<StoreError> + Send + 'static,
    F: FnOnce(&Transaction) -> Result<T, E> + Send + 'static,
{
    db.call(move |conn| {
        let tx = conn.transaction()?;
        let outcome = f(&tx);
        if outcome.is_ok() {
            tx.commit()?;
        }
        Ok(outcome)
    })
    .await?
}
