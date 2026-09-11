//! API tokens: bearer secrets an account mints for scripts, each limited to some of the
//! account's permissions, stored hashed, and revocable at any time.

use discoclip_engine::StoreError;
use discoclip_engine::rusqlite::{self, Connection, OptionalExtension, params};
use discoclip_engine::store::sqlite::SqliteStore;
use jiff::{SignedDuration, Timestamp};
use serde::Serialize;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::db::{nanos, timestamp, transact};
use crate::sessions::random_token;
use crate::users::{Permission, UserId};

/// Every token starts with this, so one can be told apart from other secrets.
pub const PREFIX: &str = "dc_";
pub const NAME_MAX: usize = 64;
/// `last_used_at` is written at most this often.
const TOUCH_INTERVAL: SignedDuration = SignedDuration::from_mins(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct TokenId(pub Uuid);

impl std::fmt::Display for TokenId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for TokenId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiToken {
    pub id: TokenId,
    pub user_id: UserId,
    pub name: String,
    /// The first characters of the secret, to tell tokens apart.
    pub prefix: String,
    /// What the token may do, within what its account's role allows.
    pub scopes: Vec<Permission>,
    pub created_at: Timestamp,
    pub last_used_at: Option<Timestamp>,
    pub expires_at: Option<Timestamp>,
}

impl ApiToken {
    pub fn is_live(&self, now: Timestamp) -> bool {
        self.expires_at.is_none_or(|at| now < at)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("a token name is 1 to {NAME_MAX} characters")]
    InvalidName,
    #[error("a token expires in the future or never")]
    InvalidExpiry,
    #[error("token {0} not found")]
    NotFound(TokenId),
}

impl From<rusqlite::Error> for TokenError {
    fn from(error: rusqlite::Error) -> Self {
        TokenError::Store(error.into())
    }
}

fn hash_token(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

#[derive(Clone)]
pub struct TokenStore {
    db: SqliteStore,
}

impl TokenStore {
    /// Over a database the application's migrations have been applied to.
    pub fn new(db: SqliteStore) -> Self {
        Self { db }
    }

    /// Mints a token; the secret is returned this once and never stored.
    pub async fn create(
        &self,
        user: UserId,
        name: &str,
        scopes: Vec<Permission>,
        expires_at: Option<Timestamp>,
    ) -> Result<(ApiToken, String), TokenError> {
        let name = name.trim().to_string();
        if name.is_empty() || name.chars().count() > NAME_MAX {
            return Err(TokenError::InvalidName);
        }
        let now = Timestamp::now();
        if expires_at.is_some_and(|at| at <= now) {
            return Err(TokenError::InvalidExpiry);
        }
        let secret = format!("{PREFIX}{}", random_token());
        let prefix: String = secret.chars().take(PREFIX.len() + 8).collect();
        let token_hash = hash_token(&secret);
        let mut scopes = scopes;
        scopes.sort_by_key(|p| p.to_string());
        scopes.dedup();
        let token = transact(&self.db, move |tx| {
            let id = TokenId(Uuid::now_v7());
            tx.execute(
                "INSERT INTO api_tokens (id, user_id, name, prefix, token_hash, scopes, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    id.to_string(),
                    user.to_string(),
                    name,
                    prefix,
                    token_hash,
                    serde_json::to_string(&scopes).map_err(|e| StoreError::Corrupt(e.to_string()))?,
                    nanos(now),
                    expires_at.map(nanos),
                ],
            )?;
            Ok::<_, TokenError>(ApiToken {
                id,
                user_id: user,
                name,
                prefix,
                scopes,
                created_at: now,
                last_used_at: None,
                expires_at,
            })
        })
        .await?;
        Ok((token, secret))
    }

    /// The live token `secret` names, if any, marking it as used.
    pub async fn authenticate(&self, secret: &str) -> Result<Option<ApiToken>, TokenError> {
        let token_hash = hash_token(secret);
        transact(&self.db, move |tx| {
            let now = Timestamp::now();
            let found = tx
                .query_row(
                    &format!("{SELECT} FROM api_tokens WHERE token_hash = ?1"),
                    params![token_hash],
                    row_to_token,
                )
                .optional()?;
            let Some(mut token) = found else {
                return Ok(None);
            };
            if !token.is_live(now) {
                tx.execute(
                    "DELETE FROM api_tokens WHERE id = ?1",
                    params![token.id.to_string()],
                )?;
                return Ok(None);
            }
            let stale = token
                .last_used_at
                .is_none_or(|at| now.duration_since(at) >= TOUCH_INTERVAL);
            if stale {
                tx.execute(
                    "UPDATE api_tokens SET last_used_at = ?2 WHERE id = ?1",
                    params![token.id.to_string(), nanos(now)],
                )?;
                token.last_used_at = Some(now);
            }
            Ok(Some(token))
        })
        .await
    }

    pub async fn get(&self, id: TokenId) -> Result<Option<ApiToken>, TokenError> {
        transact(&self.db, move |tx| get_in(tx, id)).await
    }

    /// `user`'s live tokens, newest first.
    pub async fn list_for(&self, user: UserId) -> Result<Vec<ApiToken>, TokenError> {
        transact(&self.db, move |tx| {
            let now = Timestamp::now();
            let mut stmt = tx.prepare(&format!(
                "{SELECT} FROM api_tokens WHERE user_id = ?1 ORDER BY created_at DESC, id DESC"
            ))?;
            let rows = stmt.query_map(params![user.to_string()], row_to_token)?;
            let tokens = rows.collect::<Result<Vec<_>, _>>()?;
            Ok(tokens.into_iter().filter(|t| t.is_live(now)).collect())
        })
        .await
    }

    /// Ends `id` for good.
    pub async fn revoke(&self, id: TokenId) -> Result<(), TokenError> {
        transact(&self.db, move |tx| {
            if tx.execute(
                "DELETE FROM api_tokens WHERE id = ?1",
                params![id.to_string()],
            )? == 0
            {
                return Err(TokenError::NotFound(id));
            }
            Ok(())
        })
        .await
    }
}

const SELECT: &str =
    "SELECT id, user_id, name, prefix, scopes, created_at, last_used_at, expires_at";

fn get_in(conn: &Connection, id: TokenId) -> Result<Option<ApiToken>, TokenError> {
    Ok(conn
        .query_row(
            &format!("{SELECT} FROM api_tokens WHERE id = ?1"),
            params![id.to_string()],
            row_to_token,
        )
        .optional()?)
}

fn row_to_token(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApiToken> {
    let corrupt = |message: String| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(StoreError::Corrupt(message)),
        )
    };
    let id: String = row.get(0)?;
    let user_id: String = row.get(1)?;
    let scopes: String = row.get(4)?;
    let last_used_at: Option<i64> = row.get(6)?;
    let expires_at: Option<i64> = row.get(7)?;
    Ok(ApiToken {
        id: id
            .parse()
            .map_err(|e| corrupt(format!("token id {id}: {e}")))?,
        user_id: user_id
            .parse()
            .map_err(|e| corrupt(format!("token user {user_id}: {e}")))?,
        name: row.get(2)?,
        prefix: row.get(3)?,
        scopes: serde_json::from_str(&scopes)
            .map_err(|e| corrupt(format!("token {id} scopes: {e}")))?,
        created_at: timestamp("api_tokens.created_at", row.get(5)?)
            .map_err(|e| corrupt(e.to_string()))?,
        last_used_at: last_used_at
            .map(|at| timestamp("api_tokens.last_used_at", at))
            .transpose()
            .map_err(|e| corrupt(e.to_string()))?,
        expires_at: expires_at
            .map(|at| timestamp("api_tokens.expires_at", at))
            .transpose()
            .map_err(|e| corrupt(e.to_string()))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::users::{Role, UserStore};

    async fn stores() -> (TokenStore, UserId) {
        let db = SqliteStore::open_in_memory().await.unwrap();
        crate::migrations::apply(&db).await.unwrap();
        let users = UserStore::new(db.clone());
        let a = users.set_up("a", "correct horse").await.unwrap().id;
        users.create("b", None, Role::Viewer).await.unwrap();
        (TokenStore::new(db), a)
    }

    #[tokio::test]
    async fn tokens_authenticate_until_revoked() {
        let (tokens, a) = stores().await;
        let (token, secret) = tokens
            .create(
                a,
                "  ci  ",
                vec![Permission::ManageUsers, Permission::ManageUsers],
                None,
            )
            .await
            .unwrap();
        assert!(secret.starts_with("dc_"));
        assert_eq!(secret.len(), 46);
        assert_eq!(token.prefix, &secret[..11]);
        assert_eq!(token.name, "ci");
        assert_eq!(token.scopes, vec![Permission::ManageUsers]);
        assert!(token.last_used_at.is_none());

        let used = tokens.authenticate(&secret).await.unwrap().unwrap();
        assert_eq!(used.id, token.id);
        assert!(used.last_used_at.is_some());
        assert!(tokens.authenticate("dc_nope").await.unwrap().is_none());
        assert!(tokens.authenticate(&token.prefix).await.unwrap().is_none());
        assert_eq!(tokens.list_for(a).await.unwrap(), vec![used.clone()]);
        assert_eq!(tokens.get(token.id).await.unwrap(), Some(used));

        tokens.revoke(token.id).await.unwrap();
        assert!(matches!(
            tokens.revoke(token.id).await,
            Err(TokenError::NotFound(id)) if id == token.id
        ));
        assert!(tokens.authenticate(&secret).await.unwrap().is_none());
        assert!(tokens.list_for(a).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn names_and_expiries_are_checked_and_honoured() {
        let (tokens, a) = stores().await;
        assert!(matches!(
            tokens.create(a, "   ", vec![], None).await,
            Err(TokenError::InvalidName)
        ));
        assert!(matches!(
            tokens.create(a, &"x".repeat(65), vec![], None).await,
            Err(TokenError::InvalidName)
        ));
        assert!(matches!(
            tokens
                .create(
                    a,
                    "past",
                    vec![],
                    Some(Timestamp::now() - SignedDuration::from_secs(1))
                )
                .await,
            Err(TokenError::InvalidExpiry)
        ));
        let soon = Timestamp::now() + SignedDuration::from_millis(200);
        let (token, secret) = tokens.create(a, "short", vec![], Some(soon)).await.unwrap();
        assert_eq!(token.expires_at, Some(soon));
        assert!(tokens.authenticate(&secret).await.unwrap().is_some());
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        assert!(tokens.authenticate(&secret).await.unwrap().is_none());
        assert!(tokens.get(token.id).await.unwrap().is_none());
        assert!(matches!(
            tokens.create(a, "later", vec![], Some(soon)).await,
            Err(TokenError::InvalidExpiry)
        ));
        assert!(tokens.list_for(a).await.unwrap().is_empty());
    }
}
