//! Cookie jars per platform: the logged-in sessions resolvers use, kept sealed under the
//! keyring in the database, loaded into the HTTP client at startup, and replaced or
//! cleared from the app, with what the platform last said about them. Every change is
//! written to the audit log in the same transaction, never with the cookies themselves.

use std::collections::HashMap;

use discoclip_engine::StoreError;
use discoclip_engine::http::{Cookie, Jar};
use discoclip_engine::resolve::SessionCheck;
use discoclip_engine::rusqlite::{self, OptionalExtension, params};
use discoclip_engine::store::sqlite::SqliteStore;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::audit::{self, Action, Actor, Target};
use crate::db::{nanos, timestamp, transact};
use crate::secrets::{Keyring, SecretError};

#[derive(Debug, thiserror::Error)]
pub enum CookieError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("the stored cookies of {platform} cannot be unsealed: {source}")]
    Sealed {
        platform: String,
        source: SecretError,
    },
    #[error("the stored cookies of {0} are corrupt: {1}")]
    Corrupt(String, String),
}

impl From<rusqlite::Error> for CookieError {
    fn from(error: rusqlite::Error) -> Self {
        CookieError::Store(error.into())
    }
}

/// What a platform said about its stored cookies when last asked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionCheckResult {
    #[serde(flatten)]
    pub check: SessionCheck,
    pub at: Timestamp,
}

/// A platform's stored session as the app shows it: never the cookies themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StoredSession {
    pub platform: String,
    pub cookies: usize,
    pub updated_at: Option<Timestamp>,
    pub check: Option<SessionCheckResult>,
}

fn context(platform: &str) -> String {
    format!("cookies:{platform}")
}

#[derive(Clone)]
pub struct CookieStore {
    db: SqliteStore,
    keyring: Keyring,
}

impl CookieStore {
    pub fn new(db: SqliteStore, keyring: Keyring) -> Self {
        Self { db, keyring }
    }

    /// Every platform's jar, for loading into the HTTP client.
    pub async fn jars(&self) -> Result<Vec<(String, Jar)>, CookieError> {
        let keyring = self.keyring.clone();
        transact(&self.db, move |tx| {
            let mut stmt = tx.prepare(
                "SELECT platform, jar FROM platform_cookies WHERE jar IS NOT NULL ORDER BY platform",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            let mut jars = Vec::new();
            for row in rows {
                let (platform, sealed) = row?;
                jars.push((platform.clone(), unseal(&keyring, &platform, &sealed)?));
            }
            Ok(jars)
        })
        .await
    }

    /// Every platform's session as the app shows it, by platform.
    pub async fn summaries(&self) -> Result<HashMap<String, StoredSession>, CookieError> {
        transact(&self.db, |tx| {
            let mut stmt = tx.prepare(
                "SELECT platform, cookies, updated_at, check_json, checked_at FROM platform_cookies",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                ))
            })?;
            let mut out = HashMap::new();
            for row in rows {
                let (platform, cookies, updated, check_json, checked) = row?;
                let check = match (check_json, checked) {
                    (Some(text), Some(at)) => Some(SessionCheckResult {
                        check: serde_json::from_str(&text)
                            .map_err(|e| CookieError::Corrupt(platform.clone(), e.to_string()))?,
                        at: timestamp("checked_at", at)?,
                    }),
                    _ => None,
                };
                out.insert(
                    platform.clone(),
                    StoredSession {
                        platform,
                        cookies: cookies.max(0) as usize,
                        updated_at: updated.map(|n| timestamp("updated_at", n)).transpose()?,
                        check,
                    },
                );
            }
            Ok(out)
        })
        .await
    }

    /// Replaces `platform`'s cookies with `jar`, as done by `actor`; `format` names how
    /// they were given, for the audit log.
    pub async fn replace(
        &self,
        actor: &Actor,
        platform: &str,
        jar: &Jar,
        format: &str,
    ) -> Result<StoredSession, CookieError> {
        let serialized = serde_json::to_string(jar.cookies())
            .map_err(|e| CookieError::Corrupt(platform.to_string(), e.to_string()))?;
        let sealed = self.keyring.seal_str(&serialized, &context(platform));
        let platform = platform.to_string();
        let actor = actor.clone();
        let format = format.to_string();
        let count = jar.len();
        transact(&self.db, move |tx| {
            let previous: i64 = tx
                .query_row(
                    "SELECT cookies FROM platform_cookies WHERE platform = ?1",
                    params![platform],
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(0);
            let now = Timestamp::now();
            tx.execute(
                "INSERT INTO platform_cookies (platform, jar, cookies, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(platform) DO UPDATE SET
                    jar = excluded.jar, cookies = excluded.cookies, updated_at = excluded.updated_at",
                params![platform, sealed, count as i64, nanos(now)],
            )?;
            audit::record(
                tx,
                &actor,
                Action::SessionImport,
                Target::platform(&platform),
                json!({"cookies": count, "previous_cookies": previous, "format": format}),
            )?;
            Ok(StoredSession {
                platform,
                cookies: count,
                updated_at: Some(now),
                check: None,
            })
        })
        .await
    }

    /// Removes `platform`'s cookies, as done by `actor`; whether there were any.
    pub async fn clear(&self, actor: &Actor, platform: &str) -> Result<bool, CookieError> {
        let platform = platform.to_string();
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let previous: i64 = tx
                .query_row(
                    "SELECT cookies FROM platform_cookies WHERE platform = ?1",
                    params![platform],
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or(0);
            tx.execute(
                "UPDATE platform_cookies SET jar = NULL, cookies = 0, updated_at = ?2 WHERE platform = ?1",
                params![platform, nanos(Timestamp::now())],
            )?;
            if previous > 0 {
                audit::record(
                    tx,
                    &actor,
                    Action::SessionClear,
                    Target::platform(&platform),
                    json!({"cookies": previous}),
                )?;
            }
            Ok(previous > 0)
        })
        .await
    }

    /// Keeps what `platform` said about its session.
    pub async fn record_check(
        &self,
        platform: &str,
        check: &SessionCheck,
    ) -> Result<SessionCheckResult, CookieError> {
        let result = SessionCheckResult {
            check: check.clone(),
            at: Timestamp::now(),
        };
        let text = serde_json::to_string(check)
            .map_err(|e| CookieError::Corrupt(platform.to_string(), e.to_string()))?;
        let platform = platform.to_string();
        let at = nanos(result.at);
        transact::<(), CookieError, _>(&self.db, move |tx| {
            tx.execute(
                "INSERT INTO platform_cookies (platform, cookies, check_json, checked_at)
                 VALUES (?1, 0, ?2, ?3)
                 ON CONFLICT(platform) DO UPDATE SET
                    check_json = excluded.check_json, checked_at = excluded.checked_at",
                params![platform, text, at],
            )?;
            Ok(())
        })
        .await?;
        Ok(result)
    }
}

fn unseal(keyring: &Keyring, platform: &str, sealed: &str) -> Result<Jar, CookieError> {
    let text = keyring
        .open_str(sealed, &context(platform))
        .map_err(|source| CookieError::Sealed {
            platform: platform.to_string(),
            source,
        })?;
    let cookies: Vec<Cookie> = serde_json::from_str(&text)
        .map_err(|e| CookieError::Corrupt(platform.to_string(), e.to_string()))?;
    Ok(Jar::from_cookies(cookies))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::AuditStore;

    async fn store() -> (CookieStore, AuditStore) {
        let db = SqliteStore::open_in_memory().await.unwrap();
        crate::migrations::apply(&db).await.unwrap();
        (
            CookieStore::new(db.clone(), Keyring::from_key([7; 32])),
            AuditStore::new(db),
        )
    }

    #[tokio::test]
    async fn jars_are_sealed_stored_and_read_back() {
        let (store, audit) = store().await;
        assert!(store.jars().await.unwrap().is_empty());
        let jar = Jar::from_cookies(vec![
            Cookie::new("reddit_session", "secret-value", "reddit.com"),
            Cookie::new("over18", "1", "reddit.com"),
        ]);
        let stored = store
            .replace(&Actor::test(), "reddit", &jar, "netscape")
            .await
            .unwrap();
        assert_eq!(stored.cookies, 2);
        assert!(stored.updated_at.is_some());
        let jars = store.jars().await.unwrap();
        assert_eq!(jars.len(), 1);
        assert_eq!(jars[0].0, "reddit");
        assert_eq!(jars[0].1, jar);

        let check = store
            .record_check(
                "reddit",
                &SessionCheck::LoggedIn {
                    account: "u/nick".into(),
                },
            )
            .await
            .unwrap();
        let summaries = store.summaries().await.unwrap();
        assert_eq!(summaries["reddit"].cookies, 2);
        assert_eq!(summaries["reddit"].check, Some(check));
        // A check for a platform without cookies is kept too.
        store
            .record_check("twitter", &SessionCheck::Unsupported)
            .await
            .unwrap();
        let summaries = store.summaries().await.unwrap();
        assert_eq!(summaries["twitter"].cookies, 0);
        assert!(summaries["twitter"].updated_at.is_none());

        assert!(store.clear(&Actor::test(), "reddit").await.unwrap());
        assert!(!store.clear(&Actor::test(), "reddit").await.unwrap());
        assert!(store.jars().await.unwrap().is_empty());
        assert_eq!(store.summaries().await.unwrap()["reddit"].cookies, 0);

        // The audit log names the platform and counts, never a cookie.
        let entries = audit
            .list(&crate::audit::Filter::default())
            .await
            .unwrap()
            .entries;
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].action, Action::SessionClear);
        assert_eq!(entries[0].details["cookies"], 2);
        assert_eq!(entries[1].action, Action::SessionImport);
        assert_eq!(entries[1].target.kind, crate::audit::TargetKind::Platform);
        assert_eq!(entries[1].target.id, "reddit");
        assert_eq!(entries[1].details["format"], "netscape");
        assert_eq!(entries[1].details["previous_cookies"], 0);
        assert!(!entries[1].details.to_string().contains("secret-value"));

        // Another key cannot read the jar.
        let jar = Jar::from_cookies(vec![Cookie::new("a", "b", "x.test")]);
        store
            .replace(&Actor::test(), "x", &jar, "header")
            .await
            .unwrap();
        let other = CookieStore::new(store.db.clone(), Keyring::from_key([8; 32]));
        assert!(matches!(
            other.jars().await.unwrap_err(),
            CookieError::Sealed { platform, .. } if platform == "x"
        ));
    }
}
