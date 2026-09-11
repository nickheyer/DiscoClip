//! Accounts. The first one is the admin, created on first run from the setup page.
//! Passwords are stored as Argon2id PHC strings.

use argon2::{Argon2, PasswordHasher, PasswordVerifier, password_hash::phc::PasswordHash};
use discoclip_engine::StoreError;
use discoclip_engine::rusqlite::{self, Connection, OptionalExtension, params};
use discoclip_engine::store::sqlite::SqliteStore;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::db::{nanos, timestamp, transact};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserId(pub Uuid);

impl UserId {
    fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl std::fmt::Display for UserId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for UserId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

/// What an account may do; checked wherever the app acts on a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Everything, including accounts and settings.
    Admin,
    /// Jobs and the bot.
    Operator,
    /// Read only.
    Viewer,
}

/// Something a request wants to do; each role allows some of them, and an API token is
/// limited to the ones it was minted with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Create accounts, assign roles, reset passwords, end anyone's sessions.
    ManageUsers,
    /// Add, change and remove the Discord applications the server runs bots for.
    ManageApplications,
    /// Edit the watch rules of any guild; Discord managers of a guild edit theirs regardless.
    ManageWatchRules,
    /// Start, stop and restart the bots.
    ManageBots,
}

impl std::fmt::Display for Permission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Permission::ManageUsers => "managing accounts",
            Permission::ManageApplications => "managing Discord applications",
            Permission::ManageWatchRules => "managing watch rules",
            Permission::ManageBots => "running the bots",
        })
    }
}

impl Role {
    pub const ALL: [Role; 3] = [Role::Admin, Role::Operator, Role::Viewer];

    pub fn allows(self, permission: Permission) -> bool {
        match permission {
            Permission::ManageUsers | Permission::ManageApplications => self == Role::Admin,
            Permission::ManageWatchRules | Permission::ManageBots => {
                matches!(self, Role::Admin | Role::Operator)
            }
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Operator => "operator",
            Role::Viewer => "viewer",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Role {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Role::ALL
            .into_iter()
            .find(|role| role.as_str() == s)
            .ok_or_else(|| format!("unknown role {s}"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct User {
    pub id: UserId,
    pub username: String,
    pub role: Role,
    /// Whether the account can log in with a password.
    pub has_password: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

pub const USERNAME_MAX: usize = 32;
pub const PASSWORD_MIN: usize = 8;
pub const PASSWORD_MAX: usize = 256;

#[derive(Debug, thiserror::Error)]
pub enum UserError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("a username is 1 to {USERNAME_MAX} letters, digits, '.', '_' or '-'")]
    InvalidUsername,
    #[error("a password is {PASSWORD_MIN} to {PASSWORD_MAX} characters")]
    InvalidPassword,
    #[error("the username {0} is taken")]
    Taken(String),
    #[error("an account exists already; the first admin is set up only once")]
    AlreadySetUp,
    #[error("password hashing failed: {0}")]
    Hash(String),
    #[error("account {0} not found")]
    NotFound(UserId),
    #[error("the last admin cannot be removed or demoted")]
    LastAdmin,
}

impl From<rusqlite::Error> for UserError {
    fn from(error: rusqlite::Error) -> Self {
        UserError::Store(error.into())
    }
}

/// Argon2id with the library's recommended parameters, as a PHC string.
pub fn hash_password(password: &str) -> Result<String, UserError> {
    check_password(password)?;
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|e| UserError::Hash(e.to_string()))
}

/// Whether `password` is the one `hash` was made from.
pub fn verify_password(hash: &str, password: &str) -> Result<bool, UserError> {
    let parsed = PasswordHash::new(hash).map_err(|e| UserError::Hash(e.to_string()))?;
    match Argon2::default().verify_password(password.as_bytes(), &parsed) {
        Ok(()) => Ok(true),
        Err(argon2::password_hash::Error::PasswordInvalid) => Ok(false),
        Err(e) => Err(UserError::Hash(e.to_string())),
    }
}

pub fn check_username(username: &str) -> Result<(), UserError> {
    let valid = !username.is_empty()
        && username.len() <= USERNAME_MAX
        && username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if valid {
        Ok(())
    } else {
        Err(UserError::InvalidUsername)
    }
}

pub fn check_password(password: &str) -> Result<(), UserError> {
    let length = password.chars().count();
    if (PASSWORD_MIN..=PASSWORD_MAX).contains(&length) {
        Ok(())
    } else {
        Err(UserError::InvalidPassword)
    }
}

#[derive(Clone)]
pub struct UserStore {
    db: SqliteStore,
}

impl UserStore {
    /// Over a database the application's migrations have been applied to.
    pub fn new(db: SqliteStore) -> Self {
        Self { db }
    }

    /// Whether any account exists.
    pub async fn is_set_up(&self) -> Result<bool, UserError> {
        transact(&self.db, |tx| Ok(count(tx)? > 0)).await
    }

    /// Creates the first account, an admin. Fails once any account exists.
    pub async fn set_up(&self, username: &str, password: &str) -> Result<User, UserError> {
        check_username(username)?;
        let hash = hash_blocking(password).await?;
        let username = username.to_string();
        transact(&self.db, move |tx| {
            if count(tx)? > 0 {
                return Err(UserError::AlreadySetUp);
            }
            insert(tx, &username, Some(&hash), Role::Admin)
        })
        .await
    }

    /// Creates an account. Without a password it can only log in another way.
    pub async fn create(
        &self,
        username: &str,
        password: Option<&str>,
        role: Role,
    ) -> Result<User, UserError> {
        check_username(username)?;
        let hash = match password {
            Some(password) => Some(hash_blocking(password).await?),
            None => None,
        };
        let username = username.to_string();
        transact(&self.db, move |tx| {
            insert(tx, &username, hash.as_deref(), role)
        })
        .await
    }

    /// The account `username` names when `password` is its password. Takes as long for an
    /// unknown name or an account without a password as for a wrong password.
    pub async fn verify_login(
        &self,
        username: &str,
        password: &str,
    ) -> Result<Option<User>, UserError> {
        let name = username.to_string();
        let found = transact(&self.db, move |tx| {
            tx.query_row(
                &format!("{SELECT}, password_hash FROM users WHERE username = ?1 COLLATE NOCASE"),
                params![name],
                |row| Ok((row_to_user(row)?, row.get::<_, Option<String>>(6)?)),
            )
            .optional()
            .map_err(UserError::from)
        })
        .await?;
        let (user, hash) = match found {
            Some((user, Some(hash))) => (Some(user), hash),
            _ => (None, DUMMY_HASH.clone()),
        };
        let password = password.to_string();
        let matches = tokio::task::spawn_blocking(move || verify_password(&hash, &password))
            .await
            .map_err(|e| UserError::Hash(e.to_string()))??;
        Ok(if matches { user } else { None })
    }

    pub async fn get(&self, id: UserId) -> Result<Option<User>, UserError> {
        transact(&self.db, move |tx| get_in(tx, id)).await
    }

    /// The account with `username`, compared without regard to case.
    pub async fn find(&self, username: &str) -> Result<Option<User>, UserError> {
        let username = username.to_string();
        transact(&self.db, move |tx| {
            tx.query_row(
                &format!("{SELECT} FROM users WHERE username = ?1 COLLATE NOCASE"),
                params![username],
                row_to_user,
            )
            .optional()
            .map_err(Into::into)
        })
        .await
    }

    /// Changes `id`'s role; the last admin keeps theirs.
    pub async fn set_role(&self, id: UserId, role: Role) -> Result<User, UserError> {
        transact(&self.db, move |tx| {
            let user = get_in(tx, id)?.ok_or(UserError::NotFound(id))?;
            if user.role == Role::Admin && role != Role::Admin && other_admins(tx, id)? == 0 {
                return Err(UserError::LastAdmin);
            }
            let now = Timestamp::now();
            tx.execute(
                "UPDATE users SET role = ?2, updated_at = ?3 WHERE id = ?1",
                params![id.to_string(), role.as_str(), nanos(now)],
            )?;
            Ok(User {
                role,
                updated_at: now,
                ..user
            })
        })
        .await
    }

    /// Replaces `id`'s password.
    pub async fn set_password(&self, id: UserId, password: &str) -> Result<User, UserError> {
        let hash = hash_blocking(password).await?;
        transact(&self.db, move |tx| {
            let user = get_in(tx, id)?.ok_or(UserError::NotFound(id))?;
            let now = Timestamp::now();
            tx.execute(
                "UPDATE users SET password_hash = ?2, updated_at = ?3 WHERE id = ?1",
                params![id.to_string(), hash, nanos(now)],
            )?;
            Ok(User {
                has_password: true,
                updated_at: now,
                ..user
            })
        })
        .await
    }

    /// Removes `id`, and through the database their sessions; the last admin stays.
    pub async fn delete(&self, id: UserId) -> Result<(), UserError> {
        transact(&self.db, move |tx| {
            let user = get_in(tx, id)?.ok_or(UserError::NotFound(id))?;
            if user.role == Role::Admin && other_admins(tx, id)? == 0 {
                return Err(UserError::LastAdmin);
            }
            tx.execute("DELETE FROM users WHERE id = ?1", params![id.to_string()])?;
            Ok(())
        })
        .await
    }

    /// Every account, oldest first.
    pub async fn list(&self) -> Result<Vec<User>, UserError> {
        transact(&self.db, |tx| {
            let mut stmt = tx.prepare(&format!("{SELECT} FROM users ORDER BY created_at, id"))?;
            let rows = stmt.query_map([], row_to_user)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })
        .await
    }
}

/// Verified against when there is no real hash, so timing does not reveal which names exist.
static DUMMY_HASH: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| hash_password("no such password").expect("hashing works"));

const SELECT: &str = "SELECT id, username, role, password_hash IS NOT NULL, created_at, updated_at";

async fn hash_blocking(password: &str) -> Result<String, UserError> {
    let password = password.to_string();
    tokio::task::spawn_blocking(move || hash_password(&password))
        .await
        .map_err(|e| UserError::Hash(e.to_string()))?
}

fn get_in(conn: &Connection, id: UserId) -> Result<Option<User>, UserError> {
    conn.query_row(
        &format!("{SELECT} FROM users WHERE id = ?1"),
        params![id.to_string()],
        row_to_user,
    )
    .optional()
    .map_err(Into::into)
}

/// How many admins there are besides `id`.
fn other_admins(conn: &Connection, id: UserId) -> Result<i64, UserError> {
    Ok(conn.query_row(
        "SELECT COUNT(*) FROM users WHERE role = 'admin' AND id != ?1",
        params![id.to_string()],
        |row| row.get(0),
    )?)
}

fn count(conn: &Connection) -> Result<i64, UserError> {
    Ok(conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?)
}

fn insert(
    conn: &Connection,
    username: &str,
    hash: Option<&str>,
    role: Role,
) -> Result<User, UserError> {
    let id = UserId::new();
    let now = Timestamp::now();
    let inserted = conn.execute(
        "INSERT INTO users (id, username, password_hash, role, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id.to_string(),
            username,
            hash,
            role.as_str(),
            nanos(now),
            nanos(now)
        ],
    );
    match inserted {
        Ok(_) => Ok(User {
            id,
            username: username.to_string(),
            role,
            has_password: hash.is_some(),
            created_at: now,
            updated_at: now,
        }),
        Err(rusqlite::Error::SqliteFailure(error, _))
            if error.code == rusqlite::ErrorCode::ConstraintViolation =>
        {
            Err(UserError::Taken(username.to_string()))
        }
        Err(error) => Err(error.into()),
    }
}

fn row_to_user(row: &rusqlite::Row<'_>) -> rusqlite::Result<User> {
    let corrupt = |message: String| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(StoreError::Corrupt(message)),
        )
    };
    let id: String = row.get(0)?;
    let role: String = row.get(2)?;
    Ok(User {
        id: id
            .parse()
            .map_err(|e| corrupt(format!("user id {id}: {e}")))?,
        username: row.get(1)?,
        role: role.parse().map_err(corrupt)?,
        has_password: row.get(3)?,
        created_at: timestamp("users.created_at", row.get(4)?)
            .map_err(|e| corrupt(e.to_string()))?,
        updated_at: timestamp("users.updated_at", row.get(5)?)
            .map_err(|e| corrupt(e.to_string()))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> UserStore {
        let db = SqliteStore::open_in_memory().await.unwrap();
        crate::migrations::apply(&db).await.unwrap();
        UserStore::new(db)
    }

    #[tokio::test]
    async fn first_run_creates_the_admin_once() {
        let store = store().await;
        assert!(!store.is_set_up().await.unwrap());
        let admin = store.set_up("nick", "correct horse").await.unwrap();
        assert_eq!(admin.username, "nick");
        assert_eq!(admin.role, Role::Admin);
        assert!(admin.has_password);
        assert!(store.is_set_up().await.unwrap());
        assert!(matches!(
            store.set_up("other", "correct horse").await,
            Err(UserError::AlreadySetUp)
        ));
        assert_eq!(store.list().await.unwrap(), vec![admin.clone()]);
        assert_eq!(store.get(admin.id).await.unwrap(), Some(admin));
    }

    #[tokio::test]
    async fn usernames_are_unique_without_regard_to_case() {
        let store = store().await;
        store.set_up("Nick", "correct horse").await.unwrap();
        assert!(matches!(
            store.create("nick", None, Role::Viewer).await,
            Err(UserError::Taken(name)) if name == "nick"
        ));
        let found = store.find("NICK").await.unwrap().unwrap();
        assert_eq!(found.username, "Nick");
        assert!(store.find("nobody").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn accounts_without_passwords_and_with_roles() {
        let store = store().await;
        store.set_up("admin", "correct horse").await.unwrap();
        let viewer = store.create("viewer", None, Role::Viewer).await.unwrap();
        assert!(!viewer.has_password);
        assert_eq!(viewer.role, Role::Viewer);
        let operator = store
            .create("op.1_x-y", Some("battery staple"), Role::Operator)
            .await
            .unwrap();
        assert!(operator.has_password);
        let names: Vec<String> = store
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|u| u.username)
            .collect();
        assert_eq!(names, vec!["admin", "viewer", "op.1_x-y"]);
    }

    #[tokio::test]
    async fn names_and_passwords_are_checked() {
        let store = store().await;
        for bad in ["", "with space", "a/b", "é", &"x".repeat(33)] {
            assert!(
                matches!(
                    store.set_up(bad, "correct horse").await,
                    Err(UserError::InvalidUsername)
                ),
                "{bad:?}"
            );
        }
        for bad in ["", "short", &"x".repeat(257)] {
            assert!(
                matches!(
                    store.set_up("nick", bad).await,
                    Err(UserError::InvalidPassword)
                ),
                "{bad:?}"
            );
        }
        assert!(!store.is_set_up().await.unwrap());
        assert!(matches!(
            store.create("x", Some("short"), Role::Viewer).await,
            Err(UserError::InvalidPassword)
        ));
    }

    #[tokio::test]
    async fn logins_verify_passwords() {
        let store = store().await;
        let nick = store.set_up("nick", "correct horse").await.unwrap();
        store.create("ghost", None, Role::Viewer).await.unwrap();
        assert_eq!(
            store.verify_login("NICK", "correct horse").await.unwrap(),
            Some(nick)
        );
        assert!(store.verify_login("nick", "wrong").await.unwrap().is_none());
        assert!(
            store
                .verify_login("ghost", "anything")
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .verify_login("nobody", "anything")
                .await
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn passwords_hash_with_argon2id_and_verify() {
        let hash = hash_password("correct horse").unwrap();
        assert!(hash.starts_with("$argon2id$v=19$"));
        assert!(verify_password(&hash, "correct horse").unwrap());
        assert!(!verify_password(&hash, "wrong horse").unwrap());
        assert_ne!(hash, hash_password("correct horse").unwrap());
        assert!(matches!(
            verify_password("not a hash", "x"),
            Err(UserError::Hash(_))
        ));
    }

    #[tokio::test]
    async fn roles_change_but_the_last_admin_stays() {
        let store = store().await;
        let admin = store.set_up("admin", "correct horse").await.unwrap();
        let viewer = store.create("viewer", None, Role::Viewer).await.unwrap();
        assert!(matches!(
            store.set_role(admin.id, Role::Viewer).await,
            Err(UserError::LastAdmin)
        ));
        assert!(matches!(
            store.delete(admin.id).await,
            Err(UserError::LastAdmin)
        ));
        let promoted = store.set_role(viewer.id, Role::Admin).await.unwrap();
        assert_eq!(promoted.role, Role::Admin);
        assert!(promoted.updated_at >= viewer.updated_at);
        let demoted = store.set_role(admin.id, Role::Operator).await.unwrap();
        assert_eq!(demoted.role, Role::Operator);
        assert_eq!(
            store.get(admin.id).await.unwrap().unwrap().role,
            Role::Operator
        );
        store.delete(admin.id).await.unwrap();
        assert!(store.get(admin.id).await.unwrap().is_none());
        assert!(matches!(
            store.delete(admin.id).await,
            Err(UserError::NotFound(id)) if id == admin.id
        ));
        assert!(matches!(
            store.set_role(admin.id, Role::Viewer).await,
            Err(UserError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn passwords_change() {
        let store = store().await;
        let viewer = store.create("viewer", None, Role::Viewer).await.unwrap();
        assert!(!viewer.has_password);
        assert!(matches!(
            store.set_password(viewer.id, "short").await,
            Err(UserError::InvalidPassword)
        ));
        let changed = store
            .set_password(viewer.id, "battery staple")
            .await
            .unwrap();
        assert!(changed.has_password);
        assert!(
            store
                .verify_login("viewer", "battery staple")
                .await
                .unwrap()
                .is_some()
        );
        assert!(matches!(
            store
                .set_password(UserId(Uuid::nil()), "battery staple")
                .await,
            Err(UserError::NotFound(_))
        ));
    }

    #[test]
    fn roles_allow_permissions() {
        assert!(Role::Admin.allows(Permission::ManageUsers));
        assert!(!Role::Operator.allows(Permission::ManageUsers));
        assert!(!Role::Viewer.allows(Permission::ManageUsers));
        assert!(Role::Admin.allows(Permission::ManageApplications));
        assert!(!Role::Operator.allows(Permission::ManageApplications));
        assert!(Role::Operator.allows(Permission::ManageWatchRules));
        assert!(!Role::Viewer.allows(Permission::ManageWatchRules));
        assert!(Role::Operator.allows(Permission::ManageBots));
        assert!(!Role::Viewer.allows(Permission::ManageBots));
    }

    #[test]
    fn roles_round_trip() {
        for role in Role::ALL {
            assert_eq!(role.as_str().parse::<Role>().unwrap(), role);
        }
        assert!("root".parse::<Role>().is_err());
    }
}
