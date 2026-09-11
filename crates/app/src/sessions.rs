//! Browser sessions: a random token carried in a cookie and stored hashed, tied to an
//! account, with a CSRF token the browser must echo on every state-changing request.

use std::net::IpAddr;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use discoclip_engine::StoreError;
use discoclip_engine::rusqlite::{self, Connection, OptionalExtension, params};
use discoclip_engine::store::sqlite::SqliteStore;
use jiff::{SignedDuration, Timestamp};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::db::{nanos, timestamp, transact};
use crate::users::UserId;

/// A session ends 30 days after it began, or 14 days after it was last used.
pub const ABSOLUTE_LIFETIME: SignedDuration = SignedDuration::from_hours(30 * 24);
pub const IDLE_LIFETIME: SignedDuration = SignedDuration::from_hours(14 * 24);
/// `last_seen_at` is written at most this often, so reads do not become writes.
const TOUCH_INTERVAL: SignedDuration = SignedDuration::from_mins(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct SessionId(pub Uuid);

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for SessionId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Session {
    pub id: SessionId,
    pub user_id: UserId,
    pub created_at: Timestamp,
    pub last_seen_at: Timestamp,
    pub user_agent: Option<String>,
    pub ip: Option<IpAddr>,
}

impl Session {
    pub fn expires_at(&self) -> Timestamp {
        (self.created_at + ABSOLUTE_LIFETIME).min(self.last_seen_at + IDLE_LIFETIME)
    }

    pub fn is_live(&self, now: Timestamp) -> bool {
        now < self.expires_at()
    }
}

/// A session just created, with the token its cookie carries.
#[derive(Debug, Clone)]
pub struct Created {
    pub session: Session,
    pub token: String,
    pub csrf_token: String,
}

/// A session found by its token.
#[derive(Debug, Clone)]
pub struct Authenticated {
    pub session: Session,
    pub csrf_token: String,
}

/// 32 random bytes as URL-safe base64.
pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn hash_token(token: &str) -> String {
    hex::encode(Sha256::digest(token.as_bytes()))
}

#[derive(Clone)]
pub struct SessionStore {
    db: SqliteStore,
}

impl SessionStore {
    /// Over a database the application's migrations have been applied to.
    pub fn new(db: SqliteStore) -> Self {
        Self { db }
    }

    /// Opens a session for `user`; expired sessions of everyone are dropped on the way.
    pub async fn create(
        &self,
        user: UserId,
        user_agent: Option<String>,
        ip: Option<IpAddr>,
    ) -> Result<Created, StoreError> {
        let token = random_token();
        let csrf_token = random_token();
        let token_hash = hash_token(&token);
        transact(&self.db, move |tx| {
            let now = Timestamp::now();
            purge_expired(tx, now)?;
            let session = Session {
                id: SessionId(Uuid::now_v7()),
                user_id: user,
                created_at: now,
                last_seen_at: now,
                user_agent,
                ip,
            };
            tx.execute(
                "INSERT INTO sessions (id, token_hash, user_id, csrf_token, created_at, last_seen_at, user_agent, ip)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    session.id.to_string(),
                    token_hash,
                    session.user_id.to_string(),
                    csrf_token,
                    nanos(now),
                    nanos(now),
                    session.user_agent,
                    session.ip.map(|ip| ip.to_string()),
                ],
            )?;
            Ok(Created {
                session,
                token,
                csrf_token,
            })
        })
        .await
    }

    /// The live session `token` names, if any, marking it as seen.
    pub async fn authenticate(&self, token: &str) -> Result<Option<Authenticated>, StoreError> {
        let token_hash = hash_token(token);
        transact(&self.db, move |tx| {
            let now = Timestamp::now();
            let found = tx
                .query_row(
                    &format!("{SELECT}, csrf_token FROM sessions WHERE token_hash = ?1"),
                    params![token_hash],
                    |row| Ok((row_to_session(row)?, row.get::<_, String>(6)?)),
                )
                .optional()?;
            let Some((mut session, csrf_token)) = found else {
                return Ok(None);
            };
            if !session.is_live(now) {
                tx.execute(
                    "DELETE FROM sessions WHERE id = ?1",
                    params![session.id.to_string()],
                )?;
                return Ok(None);
            }
            if now.duration_since(session.last_seen_at) >= TOUCH_INTERVAL {
                tx.execute(
                    "UPDATE sessions SET last_seen_at = ?2 WHERE id = ?1",
                    params![session.id.to_string(), nanos(now)],
                )?;
                session.last_seen_at = now;
            }
            Ok(Some(Authenticated {
                session,
                csrf_token,
            }))
        })
        .await
    }

    pub async fn get(&self, id: SessionId) -> Result<Option<Session>, StoreError> {
        transact(&self.db, move |tx| {
            Ok(tx
                .query_row(
                    &format!("{SELECT} FROM sessions WHERE id = ?1"),
                    params![id.to_string()],
                    row_to_session,
                )
                .optional()?)
        })
        .await
    }

    /// `user`'s live sessions, newest first.
    pub async fn list_for(&self, user: UserId) -> Result<Vec<Session>, StoreError> {
        transact(&self.db, move |tx| {
            let now = Timestamp::now();
            let mut stmt = tx.prepare(&format!(
                "{SELECT} FROM sessions WHERE user_id = ?1 ORDER BY created_at DESC, id DESC"
            ))?;
            let rows = stmt.query_map(params![user.to_string()], row_to_session)?;
            let sessions = rows.collect::<Result<Vec<_>, _>>()?;
            Ok(sessions.into_iter().filter(|s| s.is_live(now)).collect())
        })
        .await
    }

    /// Ends `id`; whether it existed.
    pub async fn revoke(&self, id: SessionId) -> Result<bool, StoreError> {
        transact(&self.db, move |tx| {
            Ok(tx.execute(
                "DELETE FROM sessions WHERE id = ?1",
                params![id.to_string()],
            )? > 0)
        })
        .await
    }

    /// Ends every session of `user` but `keep`; how many ended.
    pub async fn revoke_all_for(
        &self,
        user: UserId,
        keep: Option<SessionId>,
    ) -> Result<usize, StoreError> {
        transact(&self.db, move |tx| {
            Ok(tx.execute(
                "DELETE FROM sessions WHERE user_id = ?1 AND (?2 IS NULL OR id != ?2)",
                params![user.to_string(), keep.map(|id| id.to_string())],
            )?)
        })
        .await
    }
}

const SELECT: &str = "SELECT id, user_id, created_at, last_seen_at, user_agent, ip";

fn purge_expired(conn: &Connection, now: Timestamp) -> Result<usize, StoreError> {
    Ok(conn.execute(
        "DELETE FROM sessions WHERE created_at <= ?1 OR last_seen_at <= ?2",
        params![nanos(now - ABSOLUTE_LIFETIME), nanos(now - IDLE_LIFETIME)],
    )?)
}

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    let corrupt = |message: String| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(StoreError::Corrupt(message)),
        )
    };
    let id: String = row.get(0)?;
    let user_id: String = row.get(1)?;
    let ip: Option<String> = row.get(5)?;
    Ok(Session {
        id: id
            .parse()
            .map_err(|e| corrupt(format!("session id {id}: {e}")))?,
        user_id: user_id
            .parse()
            .map_err(|e| corrupt(format!("session user {user_id}: {e}")))?,
        created_at: timestamp("sessions.created_at", row.get(2)?)
            .map_err(|e| corrupt(e.to_string()))?,
        last_seen_at: timestamp("sessions.last_seen_at", row.get(3)?)
            .map_err(|e| corrupt(e.to_string()))?,
        user_agent: row.get(4)?,
        ip: ip
            .map(|ip| {
                ip.parse()
                    .map_err(|e| corrupt(format!("session ip {ip}: {e}")))
            })
            .transpose()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::users::{Role, UserStore};

    async fn stores() -> (SessionStore, UserId, UserId) {
        let db = SqliteStore::open_in_memory().await.unwrap();
        crate::migrations::apply(&db).await.unwrap();
        let users = UserStore::new(db.clone());
        let a = users.set_up("a", "correct horse").await.unwrap().id;
        let b = users.create("b", None, Role::Viewer).await.unwrap().id;
        (SessionStore::new(db), a, b)
    }

    #[tokio::test]
    async fn tokens_authenticate_until_revoked() {
        let (sessions, a, _) = stores().await;
        let created = sessions
            .create(a, Some("agent".into()), Some("10.0.0.1".parse().unwrap()))
            .await
            .unwrap();
        assert_eq!(created.token.len(), 43);
        assert_ne!(created.token, created.csrf_token);
        let auth = sessions
            .authenticate(&created.token)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(auth.session, created.session);
        assert_eq!(auth.csrf_token, created.csrf_token);
        assert_eq!(auth.session.user_agent.as_deref(), Some("agent"));
        assert_eq!(auth.session.ip, Some("10.0.0.1".parse().unwrap()));
        assert!(sessions.authenticate("nope").await.unwrap().is_none());
        assert!(
            sessions
                .authenticate(&created.csrf_token)
                .await
                .unwrap()
                .is_none()
        );

        assert!(sessions.revoke(created.session.id).await.unwrap());
        assert!(!sessions.revoke(created.session.id).await.unwrap());
        assert!(
            sessions
                .authenticate(&created.token)
                .await
                .unwrap()
                .is_none()
        );
        assert!(sessions.get(created.session.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn sessions_are_listed_per_user_and_revoked_in_bulk() {
        let (sessions, a, b) = stores().await;
        let a1 = sessions.create(a, None, None).await.unwrap();
        let a2 = sessions.create(a, None, None).await.unwrap();
        let b1 = sessions.create(b, None, None).await.unwrap();
        let listed: Vec<SessionId> = sessions
            .list_for(a)
            .await
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(listed, vec![a2.session.id, a1.session.id]);
        assert_eq!(
            sessions
                .revoke_all_for(a, Some(a2.session.id))
                .await
                .unwrap(),
            1
        );
        assert!(sessions.authenticate(&a1.token).await.unwrap().is_none());
        assert!(sessions.authenticate(&a2.token).await.unwrap().is_some());
        assert!(sessions.authenticate(&b1.token).await.unwrap().is_some());
        assert_eq!(sessions.revoke_all_for(a, None).await.unwrap(), 1);
        assert!(sessions.list_for(a).await.unwrap().is_empty());
        assert_eq!(sessions.list_for(b).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn expired_sessions_are_gone() {
        let (sessions, a, _) = stores().await;
        let idle = sessions.create(a, None, None).await.unwrap();
        let old = sessions.create(a, None, None).await.unwrap();
        let now = Timestamp::now();
        sessions
            .db
            .call(move |conn| {
                conn.execute(
                    "UPDATE sessions SET last_seen_at = ?2 WHERE id = ?1",
                    params![idle.session.id.to_string(), nanos(now - IDLE_LIFETIME)],
                )?;
                conn.execute(
                    "UPDATE sessions SET created_at = ?2 WHERE id = ?1",
                    params![old.session.id.to_string(), nanos(now - ABSOLUTE_LIFETIME)],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(sessions.authenticate(&idle.token).await.unwrap().is_none());
        assert!(sessions.list_for(a).await.unwrap().is_empty());
        // Creating a session purges what remains.
        sessions.create(a, None, None).await.unwrap();
        assert!(sessions.get(old.session.id).await.unwrap().is_none());
        assert_eq!(sessions.list_for(a).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn deleting_a_user_drops_their_sessions() {
        let (sessions, a, b) = stores().await;
        sessions.create(b, None, None).await.unwrap();
        sessions
            .db
            .call(move |conn| {
                conn.execute("DELETE FROM users WHERE id = ?1", params![b.to_string()])?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(sessions.list_for(b).await.unwrap().is_empty());
        assert!(sessions.list_for(a).await.unwrap().is_empty());
    }
}
