//! Front ends expose completed jobs through pages the profiles route media to. A
//! profile's delivery names the view its media is published on and the bot links to.
//! Access can be public or require a shared secret, an account of the front end's own
//! or a login provider, where a Discord login may have to belong to one of the view's
//! servers.
//!
//! Bots and viewer routes read a shared cache. Changes are audited transactionally.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};

use discoclip_bot::{LinkError, LinkTargets, MediaLink};
use discoclip_engine::StoreError;
use discoclip_engine::job::Job;
use discoclip_engine::policy::View;
use discoclip_engine::rusqlite::{self, Connection, OptionalExtension, params};
use discoclip_engine::store::sqlite::SqliteStore;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::json;
use url::Url;
use uuid::Uuid;

use crate::audit::{self, Action, Actor, Target};
use crate::db::{nanos, timestamp, transact};
use crate::profiles::{self, ProfileCache, ProfileError, ProfileId, Scope};
use crate::public_url::PublicUrl;
use crate::secrets::Keyring;
use crate::sessions::{hash_token, random_token};
use crate::users::{check_password, hash_password, hash_secret, verify_password};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct FrontendId(pub Uuid);

impl std::fmt::Display for FrontendId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for FrontendId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

/// What a shared secret is presented as, so the login page asks the right way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SecretKind {
    /// A short code of digits.
    Pin,
    Password,
    /// A long string handed out, pasted rather than remembered.
    Token,
}

/// Who a front end lets in. With `open`, everyone. Otherwise a viewer gets in by any
/// one of the ways set up: the shared secret, an account of the front end's own, or a
/// login through one of the listed providers, where a Discord login may also have to
/// belong to one of the listed servers, or be one of the listed users.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Access {
    pub open: bool,
    /// How the shared secret, when one is stored, is asked for.
    pub secret_kind: Option<SecretKind>,
    /// Whether the front end's own accounts may log in.
    pub accounts: bool,
    /// Login provider ids, as the server's `auth` settings name them, plus `discord`.
    pub providers: Vec<String>,
    /// A Discord login must belong to one of these servers, by id, when any are listed
    pub discord_guilds: Vec<String>,
    /// A Discord login must be one of these users, by id, when any are listed
    pub discord_users: Vec<String>,
}

impl Access {
    /// Whether a way in besides being open is set up, so a viewer can be asked to log in.
    pub fn has_login(&self, has_secret: bool) -> bool {
        (has_secret && self.secret_kind.is_some()) || self.accounts || !self.providers.is_empty()
    }

    /// Whether a Discord login is held to listed servers or users
    pub fn checks_discord(&self) -> bool {
        !self.discord_guilds.is_empty() || !self.discord_users.is_empty()
    }
}

/// What a front end says, as edited. The shared secret is set apart, never read back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrontendInput {
    pub name: String,
    /// The path segment the front end lives under: `/f/<slug>`.
    pub slug: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub access: Access,
    /// Whether viewers may download the media rather than only play it.
    #[serde(default = "yes")]
    pub downloads: bool,
    /// How long the link the page hands Discord to play the media stays good
    #[serde(default = "default_signed_days")]
    pub signed_link_days: u32,
}

fn yes() -> bool {
    true
}

fn default_signed_days() -> u32 {
    30
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Frontend {
    pub id: FrontendId,
    #[serde(flatten)]
    pub input: FrontendInput,
    /// A shared secret is stored.
    pub has_secret: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

impl Frontend {
    /// Whether `job` is published here, its policy naming this view
    pub fn shows(&self, job: &Job) -> bool {
        job.request
            .policy
            .delivery
            .view
            .id()
            .is_some_and(|id| id == self.id.to_string())
    }
}

/// An account of a front end's own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FrontendUser {
    pub id: Uuid,
    pub frontend_id: FrontendId,
    pub username: String,
    pub created_at: Timestamp,
}

/// A viewer let into a front end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Viewer {
    pub frontend_id: FrontendId,
    /// How the viewer got in: `secret`, `account:<username>` or
    /// `provider:<id>:<subject>`.
    pub subject: String,
    /// What to call the viewer.
    pub display: String,
}

/// A viewer's session with a front end.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ViewerSession {
    pub id: Uuid,
    pub frontend_id: FrontendId,
    pub subject: String,
    pub display: String,
    pub created_at: Timestamp,
    pub last_seen_at: Timestamp,
    pub expires_at: Timestamp,
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
}

const SESSION_LIFETIME: SignedDuration = SignedDuration::from_hours(24 * 30);
const TOUCH_INTERVAL: SignedDuration = SignedDuration::from_mins(5);
const SLUG_MIN: usize = 2;
const SLUG_MAX: usize = 40;
const NAME_MAX: usize = 80;
const DESCRIPTION_MAX: usize = 500;
const SECRET_MIN: usize = 4;
const SECRET_MAX: usize = 200;
const USERNAME_MAX: usize = 40;
const RESERVED_SLUGS: &[&str] = &["api", "login", "logout", "setup", "static", "_app"];

#[derive(Debug, thiserror::Error)]
pub enum FrontendError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Profiles(#[from] ProfileError),
    #[error("content view {0} not found")]
    NotFound(FrontendId),
    #[error("{0}")]
    Invalid(String),
    #[error("a content view already uses the slug {0}")]
    DuplicateSlug(String),
    #[error("{0} already has an account called {1}")]
    DuplicateUser(String, String),
    #[error("no login provider is called {0}")]
    UnknownProvider(String),
    #[error("{0} profile(s) send media to this view: point them elsewhere first")]
    InUse(usize),
    #[error("{0}")]
    Password(String),
}

impl From<rusqlite::Error> for FrontendError {
    fn from(error: rusqlite::Error) -> Self {
        FrontendError::Store(error.into())
    }
}

fn snowflake(what: &str, value: &str) -> Result<(), FrontendError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
        .map(|_| ())
        .ok_or_else(|| FrontendError::Invalid(format!("{what} {value:?} is not a Discord id")))
}

/// Whether `slug` may name a front end: lower-case letters, digits and dashes, not
/// starting or ending with a dash, and not a path the app uses itself.
pub fn check_slug(slug: &str) -> Result<(), FrontendError> {
    let len = slug.chars().count();
    if !(SLUG_MIN..=SLUG_MAX).contains(&len) {
        return Err(FrontendError::Invalid(format!(
            "a slug is {SLUG_MIN} to {SLUG_MAX} characters"
        )));
    }
    if !slug
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        || slug.starts_with('-')
        || slug.ends_with('-')
    {
        return Err(FrontendError::Invalid(
            "a slug is lower-case letters, digits and dashes, not starting or ending with a dash"
                .into(),
        ));
    }
    if RESERVED_SLUGS.contains(&slug) {
        return Err(FrontendError::Invalid(format!("{slug} is reserved")));
    }
    Ok(())
}

/// What a front end is checked against, the login providers that exist
#[derive(Clone)]
pub struct Known {
    pub providers: Vec<String>,
}

fn check(input: &FrontendInput, known: &Known) -> Result<FrontendInput, FrontendError> {
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err(FrontendError::Invalid("a content view needs a name".into()));
    }
    if name.chars().count() > NAME_MAX {
        return Err(FrontendError::Invalid(format!(
            "a content view's name is at most {NAME_MAX} characters"
        )));
    }
    let slug = input.slug.trim().to_string();
    check_slug(&slug)?;
    let description = input.description.trim().to_string();
    if description.chars().count() > DESCRIPTION_MAX {
        return Err(FrontendError::Invalid(format!(
            "a content view's description is at most {DESCRIPTION_MAX} characters"
        )));
    }
    for guild in &input.access.discord_guilds {
        snowflake("guild", guild)?;
    }
    for user in &input.access.discord_users {
        snowflake("user", user)?;
    }
    for provider in &input.access.providers {
        if !known.providers.iter().any(|p| p == provider) {
            return Err(FrontendError::UnknownProvider(provider.clone()));
        }
    }
    if input.access.checks_discord() && !input.access.providers.iter().any(|p| p == "discord") {
        return Err(FrontendError::Invalid(
            "Discord server and user checks need the discord provider among the view's providers"
                .into(),
        ));
    }
    if input.signed_link_days == 0 {
        return Err(FrontendError::Invalid(
            "signed links must last at least a day".into(),
        ));
    }
    Ok(FrontendInput {
        name,
        slug,
        description,
        ..input.clone()
    })
}

fn check_secret(secret: &str) -> Result<(), FrontendError> {
    let len = secret.chars().count();
    if !(SECRET_MIN..=SECRET_MAX).contains(&len) {
        return Err(FrontendError::Invalid(format!(
            "a secret is {SECRET_MIN} to {SECRET_MAX} characters"
        )));
    }
    Ok(())
}

fn check_username(username: &str) -> Result<String, FrontendError> {
    let username = username.trim().to_string();
    let len = username.chars().count();
    if len == 0 || len > USERNAME_MAX {
        return Err(FrontendError::Invalid(format!(
            "a username is 1 to {USERNAME_MAX} characters"
        )));
    }
    if username.chars().any(char::is_whitespace) {
        return Err(FrontendError::Invalid("a username has no spaces".into()));
    }
    Ok(username)
}

/// A front end as the cache holds it.
#[derive(Debug, Clone)]
struct Cached {
    frontend: Frontend,
}

#[derive(Default)]
struct CacheInner {
    by_slug: HashMap<String, Cached>,
}

/// The front ends as the bots and the public routes read them.
#[derive(Clone)]
pub struct FrontendCache {
    public_url: Arc<PublicUrl>,
    inner: Arc<RwLock<CacheInner>>,
}

impl FrontendCache {
    /// The enabled front end at `slug`.
    pub fn get(&self, slug: &str) -> Option<Frontend> {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .by_slug
            .get(&slug.to_ascii_lowercase())
            .filter(|c| c.frontend.input.enabled)
            .map(|c| c.frontend.clone())
    }

    /// The server's public address, configured or learned, when there is one.
    pub fn public_url(&self) -> Option<Url> {
        self.public_url.get()
    }

    /// The page of `job` on `frontend`, under the public address.
    pub fn page_url(&self, frontend: &Frontend, job: &Job) -> Option<Url> {
        let base = self.public_url()?;
        base.join(&format!("f/{}/j/{}", frontend.input.slug, job.id))
            .ok()
    }

    /// Every front end by id, and whether it is on
    pub fn views(&self) -> std::collections::BTreeMap<String, bool> {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .by_slug
            .values()
            .map(|c| (c.frontend.id.to_string(), c.frontend.input.enabled))
            .collect()
    }

    /// Whether any front end is on, so a profile that posts links has somewhere to point
    pub fn any_enabled(&self) -> bool {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .by_slug
            .values()
            .any(|c| c.frontend.input.enabled)
    }
}

impl LinkTargets for FrontendCache {
    /// The page of the view the job's policy names, when the view is on
    fn link_for(&self, job: &Job) -> Result<MediaLink, LinkError> {
        let id = job
            .request
            .policy
            .delivery
            .view
            .id()
            .ok_or(LinkError::NoView)?;
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let frontend = inner
            .by_slug
            .values()
            .map(|c| &c.frontend)
            .find(|f| f.id.to_string() == id)
            .ok_or_else(|| LinkError::Unknown(id.to_string()))?;
        if !frontend.input.enabled {
            return Err(LinkError::Disabled(frontend.input.slug.clone()));
        }
        let page = self.page_url(frontend, job).ok_or(LinkError::NoPublicUrl)?;
        Ok(MediaLink {
            page,
            view: frontend.id.to_string(),
        })
    }
}

#[derive(Clone)]
pub struct FrontendStore {
    db: SqliteStore,
    keyring: Keyring,
    profiles: ProfileCache,
    cache: FrontendCache,
}

const SELECT: &str = "SELECT id, slug, name, description, enabled, config, secret_hash IS NOT NULL, \
     created_at, updated_at FROM frontends";

/// The signed part of a media link: which front end and job, until when.
#[derive(Serialize, Deserialize)]
struct MediaTicket {
    f: Uuid,
    j: Uuid,
    /// Expiry, seconds since the Unix epoch.
    e: i64,
}

const MEDIA_CONTEXT: &str = "frontend-media";

impl FrontendStore {
    /// Over a database the application's migrations have been applied to. `load` fills
    /// the cache before anything reads it.
    pub fn new(
        db: SqliteStore,
        keyring: Keyring,
        profiles: ProfileCache,
        public_url: Arc<PublicUrl>,
    ) -> Self {
        Self {
            db,
            keyring,
            profiles,
            cache: FrontendCache {
                public_url,
                inner: Arc::new(RwLock::new(CacheInner::default())),
            },
        }
    }

    /// What the bots and the public routes read.
    pub fn cache(&self) -> FrontendCache {
        self.cache.clone()
    }

    /// Fills the cache from the database once older views are turned into profile routing
    pub async fn load(&self) -> Result<usize, FrontendError> {
        let cache = self.cache.clone();
        let profiles = self.profiles.clone();
        transact(&self.db, move |tx| {
            convert_routing(tx, &profiles)?;
            refresh(tx, &cache)
        })
        .await
    }

    pub async fn list(&self) -> Result<Vec<Frontend>, FrontendError> {
        transact(&self.db, |tx| {
            let mut stmt = tx.prepare(&format!("{SELECT} ORDER BY name COLLATE NOCASE"))?;
            let rows = stmt.query_map([], row_to_frontend)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    pub async fn get(&self, id: FrontendId) -> Result<Option<Frontend>, FrontendError> {
        transact(&self.db, move |tx| get_in(tx, id)).await
    }

    pub async fn create(
        &self,
        actor: &Actor,
        input: FrontendInput,
        known: &Known,
    ) -> Result<Frontend, FrontendError> {
        let input = check(&input, known)?;
        let cache = self.cache.clone();
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let id = FrontendId(Uuid::now_v7());
            let now = Timestamp::now();
            let inserted = tx.execute(
                "INSERT INTO frontends (id, slug, name, description, enabled, config, secret_hash, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, NULL, ?7, ?7)",
                params![
                    id.to_string(),
                    input.slug,
                    input.name,
                    input.description,
                    input.enabled,
                    encode_config(&input)?,
                    nanos(now),
                ],
            );
            match inserted {
                Ok(_) => {}
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    return Err(FrontendError::DuplicateSlug(input.slug));
                }
                Err(error) => return Err(error.into()),
            }
            refresh(tx, &cache)?;
            let frontend = get_in(tx, id)?.ok_or(FrontendError::NotFound(id))?;
            audit::record(
                tx,
                &actor,
                Action::FrontendCreate,
                Target::frontend(id, &frontend.input.slug),
                json!({ "frontend": frontend.input }),
            )?;
            Ok(frontend)
        })
        .await
    }

    pub async fn update(
        &self,
        actor: &Actor,
        id: FrontendId,
        input: FrontendInput,
        known: &Known,
    ) -> Result<Frontend, FrontendError> {
        let input = check(&input, known)?;
        let cache = self.cache.clone();
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let previous = get_in(tx, id)?.ok_or(FrontendError::NotFound(id))?;
            let now = Timestamp::now();
            let updated = tx.execute(
                "UPDATE frontends SET slug = ?2, name = ?3, description = ?4, enabled = ?5,
                 config = ?6, updated_at = ?7 WHERE id = ?1",
                params![
                    id.to_string(),
                    input.slug,
                    input.name,
                    input.description,
                    input.enabled,
                    encode_config(&input)?,
                    nanos(now),
                ],
            );
            match updated {
                Ok(0) => return Err(FrontendError::NotFound(id)),
                Ok(_) => {}
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    return Err(FrontendError::DuplicateSlug(input.slug));
                }
                Err(error) => return Err(error.into()),
            }
            refresh(tx, &cache)?;
            let frontend = get_in(tx, id)?.ok_or(FrontendError::NotFound(id))?;
            if frontend.input != previous.input {
                audit::record(
                    tx,
                    &actor,
                    Action::FrontendUpdate,
                    Target::frontend(id, &frontend.input.slug),
                    json!({ "frontend": frontend.input, "previous": previous.input }),
                )?;
            }
            Ok(frontend)
        })
        .await
    }

    /// Removes a front end with its accounts and sessions, unless a profile sends media to it
    pub async fn delete(&self, actor: &Actor, id: FrontendId) -> Result<(), FrontendError> {
        let naming = self.profiles.profiles_naming_view(&id.to_string());
        if !naming.is_empty() {
            return Err(FrontendError::InUse(naming.len()));
        }
        let cache = self.cache.clone();
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let frontend = get_in(tx, id)?.ok_or(FrontendError::NotFound(id))?;
            tx.execute(
                "DELETE FROM frontend_sessions WHERE frontend_id = ?1",
                params![id.to_string()],
            )?;
            tx.execute(
                "DELETE FROM frontend_users WHERE frontend_id = ?1",
                params![id.to_string()],
            )?;
            tx.execute(
                "DELETE FROM frontends WHERE id = ?1",
                params![id.to_string()],
            )?;
            refresh(tx, &cache)?;
            audit::record(
                tx,
                &actor,
                Action::FrontendDelete,
                Target::frontend(id, &frontend.input.slug),
                json!({ "frontend": frontend.input }),
            )?;
            Ok(())
        })
        .await
    }

    /// Stores the shared secret, hashed. `None` removes it.
    pub async fn set_secret(
        &self,
        actor: &Actor,
        id: FrontendId,
        secret: Option<&str>,
    ) -> Result<Frontend, FrontendError> {
        let hash = match secret {
            Some(secret) => {
                check_secret(secret)?;
                Some(hash_secret(secret).map_err(|e| FrontendError::Password(e.to_string()))?)
            }
            None => None,
        };
        let cache = self.cache.clone();
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let now = Timestamp::now();
            let changed = tx.execute(
                "UPDATE frontends SET secret_hash = ?2, updated_at = ?3 WHERE id = ?1",
                params![id.to_string(), hash, nanos(now)],
            )?;
            if changed == 0 {
                return Err(FrontendError::NotFound(id));
            }
            refresh(tx, &cache)?;
            let frontend = get_in(tx, id)?.ok_or(FrontendError::NotFound(id))?;
            audit::record(
                tx,
                &actor,
                if frontend.has_secret {
                    Action::FrontendSecretSet
                } else {
                    Action::FrontendSecretClear
                },
                Target::frontend(id, &frontend.input.slug),
                json!({}),
            )?;
            Ok(frontend)
        })
        .await
    }

    /// Whether `secret` is the front end's shared secret.
    pub async fn verify_secret(&self, id: FrontendId, secret: &str) -> Result<bool, FrontendError> {
        let secret = secret.to_string();
        transact(&self.db, move |tx| {
            let hash: Option<String> = tx
                .query_row(
                    "SELECT secret_hash FROM frontends WHERE id = ?1",
                    params![id.to_string()],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            let Some(hash) = hash else {
                return Ok(false);
            };
            verify_password(&hash, &secret).map_err(|e| FrontendError::Password(e.to_string()))
        })
        .await
    }

    pub async fn users(&self, id: FrontendId) -> Result<Vec<FrontendUser>, FrontendError> {
        transact(&self.db, move |tx| {
            let mut stmt = tx.prepare(
                "SELECT id, frontend_id, username, created_at FROM frontend_users
                 WHERE frontend_id = ?1 ORDER BY username COLLATE NOCASE",
            )?;
            let rows = stmt.query_map(params![id.to_string()], row_to_user)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    pub async fn create_user(
        &self,
        actor: &Actor,
        id: FrontendId,
        username: &str,
        password: &str,
    ) -> Result<FrontendUser, FrontendError> {
        let username = check_username(username)?;
        check_password(password).map_err(|e| FrontendError::Password(e.to_string()))?;
        let hash = hash_password(password).map_err(|e| FrontendError::Password(e.to_string()))?;
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let frontend = get_in(tx, id)?.ok_or(FrontendError::NotFound(id))?;
            let user_id = Uuid::now_v7();
            let now = Timestamp::now();
            let inserted = tx.execute(
                "INSERT INTO frontend_users (id, frontend_id, username, password_hash, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    user_id.to_string(),
                    id.to_string(),
                    username,
                    hash,
                    nanos(now)
                ],
            );
            match inserted {
                Ok(_) => {}
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    return Err(FrontendError::DuplicateUser(frontend.input.slug, username));
                }
                Err(error) => return Err(error.into()),
            }
            audit::record(
                tx,
                &actor,
                Action::FrontendUserCreate,
                Target::frontend(id, &frontend.input.slug),
                json!({ "username": username }),
            )?;
            Ok(FrontendUser {
                id: user_id,
                frontend_id: id,
                username,
                created_at: now,
            })
        })
        .await
    }

    pub async fn set_user_password(
        &self,
        actor: &Actor,
        id: FrontendId,
        user: Uuid,
        password: &str,
    ) -> Result<FrontendUser, FrontendError> {
        check_password(password).map_err(|e| FrontendError::Password(e.to_string()))?;
        let hash = hash_password(password).map_err(|e| FrontendError::Password(e.to_string()))?;
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let frontend = get_in(tx, id)?.ok_or(FrontendError::NotFound(id))?;
            let changed = tx.execute(
                "UPDATE frontend_users SET password_hash = ?3 WHERE id = ?1 AND frontend_id = ?2",
                params![user.to_string(), id.to_string(), hash],
            )?;
            if changed == 0 {
                return Err(FrontendError::Invalid(format!(
                    "{} has no account {user}",
                    frontend.input.slug
                )));
            }
            let found = tx.query_row(
                "SELECT id, frontend_id, username, created_at FROM frontend_users WHERE id = ?1",
                params![user.to_string()],
                row_to_user,
            )?;
            audit::record(
                tx,
                &actor,
                Action::FrontendUserPassword,
                Target::frontend(id, &frontend.input.slug),
                json!({ "username": found.username }),
            )?;
            Ok(found)
        })
        .await
    }

    /// Removes an account and every session it holds.
    pub async fn delete_user(
        &self,
        actor: &Actor,
        id: FrontendId,
        user: Uuid,
    ) -> Result<(), FrontendError> {
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let frontend = get_in(tx, id)?.ok_or(FrontendError::NotFound(id))?;
            let username: Option<String> = tx
                .query_row(
                    "SELECT username FROM frontend_users WHERE id = ?1 AND frontend_id = ?2",
                    params![user.to_string(), id.to_string()],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(username) = username else {
                return Err(FrontendError::Invalid(format!(
                    "{} has no account {user}",
                    frontend.input.slug
                )));
            };
            tx.execute(
                "DELETE FROM frontend_sessions WHERE frontend_id = ?1 AND subject = ?2",
                params![id.to_string(), format!("account:{username}")],
            )?;
            tx.execute(
                "DELETE FROM frontend_users WHERE id = ?1",
                params![user.to_string()],
            )?;
            audit::record(
                tx,
                &actor,
                Action::FrontendUserDelete,
                Target::frontend(id, &frontend.input.slug),
                json!({ "username": username }),
            )?;
            Ok(())
        })
        .await
    }

    /// The account `username` names when `password` is its password.
    pub async fn verify_user(
        &self,
        id: FrontendId,
        username: &str,
        password: &str,
    ) -> Result<Option<FrontendUser>, FrontendError> {
        let username = username.trim().to_string();
        let password = password.to_string();
        transact(&self.db, move |tx| {
            let found = tx
                .query_row(
                    "SELECT id, frontend_id, username, created_at, password_hash FROM frontend_users
                     WHERE frontend_id = ?1 AND username = ?2 COLLATE NOCASE",
                    params![id.to_string(), username],
                    |row| Ok((row_to_user(row)?, row.get::<_, String>(4)?)),
                )
                .optional()?;
            let Some((user, hash)) = found else {
                return Ok(None);
            };
            let ok = verify_password(&hash, &password)
                .map_err(|e| FrontendError::Password(e.to_string()))?;
            Ok(ok.then_some(user))
        })
        .await
    }

    /// Opens a session for a viewer let in as `subject`. The token to set as the cookie.
    pub async fn open_session(
        &self,
        viewer: &Viewer,
        ip: Option<IpAddr>,
        user_agent: Option<String>,
    ) -> Result<String, FrontendError> {
        let token = random_token();
        let token_hash = hash_token(&token);
        let viewer = viewer.clone();
        transact(&self.db, move |tx| {
            let now = Timestamp::now();
            tx.execute(
                "DELETE FROM frontend_sessions WHERE expires_at < ?1",
                params![nanos(now)],
            )?;
            tx.execute(
                "INSERT INTO frontend_sessions (id, frontend_id, token_hash, subject, display, created_at,
                 last_seen_at, expires_at, ip, user_agent)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, ?8, ?9)",
                params![
                    Uuid::now_v7().to_string(),
                    viewer.frontend_id.to_string(),
                    token_hash,
                    viewer.subject,
                    viewer.display,
                    nanos(now),
                    nanos(now + SESSION_LIFETIME),
                    ip.map(|ip| ip.to_string()),
                    user_agent,
                ],
            )?;
            Ok(token)
        })
        .await
    }

    /// The viewer a session token names at `frontend`, if the session is live.
    pub async fn authenticate(
        &self,
        frontend: FrontendId,
        token: &str,
    ) -> Result<Option<Viewer>, FrontendError> {
        let token_hash = hash_token(token);
        transact(&self.db, move |tx| {
            let now = Timestamp::now();
            let found = tx
                .query_row(
                    "SELECT id, subject, display, last_seen_at, expires_at FROM frontend_sessions
                     WHERE frontend_id = ?1 AND token_hash = ?2",
                    params![frontend.to_string(), token_hash],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, i64>(3)?,
                            row.get::<_, i64>(4)?,
                        ))
                    },
                )
                .optional()?;
            let Some((id, subject, display, last_seen, expires)) = found else {
                return Ok(None);
            };
            if timestamp("expires_at", expires)? <= now {
                tx.execute("DELETE FROM frontend_sessions WHERE id = ?1", params![id])?;
                return Ok(None);
            }
            if now.duration_since(timestamp("last_seen_at", last_seen)?) >= TOUCH_INTERVAL {
                tx.execute(
                    "UPDATE frontend_sessions SET last_seen_at = ?2 WHERE id = ?1",
                    params![id, nanos(now)],
                )?;
            }
            Ok(Some(Viewer {
                frontend_id: frontend,
                subject,
                display,
            }))
        })
        .await
    }

    /// Ends the session `token` names.
    pub async fn close_session(
        &self,
        frontend: FrontendId,
        token: &str,
    ) -> Result<(), FrontendError> {
        let token_hash = hash_token(token);
        transact(&self.db, move |tx| {
            tx.execute(
                "DELETE FROM frontend_sessions WHERE frontend_id = ?1 AND token_hash = ?2",
                params![frontend.to_string(), token_hash],
            )?;
            Ok(())
        })
        .await
    }

    pub async fn sessions(&self, id: FrontendId) -> Result<Vec<ViewerSession>, FrontendError> {
        transact(&self.db, move |tx| {
            let now = Timestamp::now();
            let mut stmt = tx.prepare(
                "SELECT id, frontend_id, subject, display, created_at, last_seen_at, expires_at, ip, user_agent
                 FROM frontend_sessions WHERE frontend_id = ?1 AND expires_at > ?2
                 ORDER BY last_seen_at DESC",
            )?;
            let rows = stmt.query_map(params![id.to_string(), nanos(now)], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, Option<String>>(8)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (id, frontend_id, subject, display, created, seen, expires, ip, user_agent) =
                    row?;
                out.push(ViewerSession {
                    id: id
                        .parse()
                        .map_err(|e| StoreError::Corrupt(format!("frontend_sessions.id: {e}")))?,
                    frontend_id: frontend_id.parse().map_err(|e| {
                        StoreError::Corrupt(format!("frontend_sessions.frontend_id: {e}"))
                    })?,
                    subject,
                    display,
                    created_at: timestamp("created_at", created)?,
                    last_seen_at: timestamp("last_seen_at", seen)?,
                    expires_at: timestamp("expires_at", expires)?,
                    ip: ip.and_then(|ip| ip.parse().ok()),
                    user_agent,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Ends one viewer session, or with `session` absent, every session of the front end.
    pub async fn revoke_sessions(
        &self,
        actor: &Actor,
        id: FrontendId,
        session: Option<Uuid>,
    ) -> Result<usize, FrontendError> {
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let frontend = get_in(tx, id)?.ok_or(FrontendError::NotFound(id))?;
            let removed = match session {
                Some(session) => tx.execute(
                    "DELETE FROM frontend_sessions WHERE frontend_id = ?1 AND id = ?2",
                    params![id.to_string(), session.to_string()],
                )?,
                None => tx.execute(
                    "DELETE FROM frontend_sessions WHERE frontend_id = ?1",
                    params![id.to_string()],
                )?,
            };
            audit::record(
                tx,
                &actor,
                Action::FrontendSessionsRevoke,
                Target::frontend(id, &frontend.input.slug),
                json!({ "sessions": removed, "session_id": session }),
            )?;
            Ok(removed)
        })
        .await
    }

    /// A token that opens `job`'s media on `frontend` without a session, until the
    /// front end's signed links run out. What the page hands Discord to play the media.
    pub fn sign_media(&self, frontend: &Frontend, job: Uuid) -> String {
        let days = frontend.input.signed_link_days.max(1);
        let expires = Timestamp::now() + SignedDuration::from_hours(24 * i64::from(days));
        let ticket = MediaTicket {
            f: frontend.id.0,
            j: job,
            e: expires.as_second(),
        };
        let json = serde_json::to_string(&ticket).expect("a ticket serializes");
        self.keyring.seal_str(&json, MEDIA_CONTEXT)
    }

    /// Whether `token` opens `job`'s media on `frontend` now.
    pub fn verify_media(&self, frontend: FrontendId, job: Uuid, token: &str) -> bool {
        let Ok(json) = self.keyring.open_str(token, MEDIA_CONTEXT) else {
            return false;
        };
        let Ok(ticket) = serde_json::from_str::<MediaTicket>(&json) else {
            return false;
        };
        ticket.f == frontend.0 && ticket.j == job && ticket.e > Timestamp::now().as_second()
    }
}

/// The parts of the input stored as one JSON column.
#[derive(Serialize, Deserialize)]
struct Config {
    access: Access,
    downloads: bool,
    #[serde(default = "default_signed_days")]
    signed_link_days: u32,
}

fn encode_config(input: &FrontendInput) -> Result<String, FrontendError> {
    serde_json::to_string(&Config {
        access: input.access.clone(),
        downloads: input.downloads,
        signed_link_days: input.signed_link_days,
    })
    .map_err(|e| FrontendError::Store(StoreError::Corrupt(e.to_string())))
}

fn refresh(conn: &Connection, cache: &FrontendCache) -> Result<usize, FrontendError> {
    let mut stmt = conn.prepare(SELECT)?;
    let frontends = stmt
        .query_map([], row_to_frontend)?
        .collect::<Result<Vec<_>, _>>()?;
    let count = frontends.len();
    let by_slug = frontends
        .into_iter()
        .map(|frontend| {
            (
                frontend.input.slug.to_ascii_lowercase(),
                Cached { frontend },
            )
        })
        .collect();
    cache
        .inner
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .by_slug = by_slug;
    Ok(count)
}

fn get_in(conn: &Connection, id: FrontendId) -> Result<Option<Frontend>, FrontendError> {
    Ok(conn
        .query_row(
            &format!("{SELECT} WHERE id = ?1"),
            params![id.to_string()],
            row_to_frontend,
        )
        .optional()?)
}

fn corrupt(message: String) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::other(message)),
    )
}

fn row_to_frontend(row: &rusqlite::Row<'_>) -> rusqlite::Result<Frontend> {
    let id: String = row.get(0)?;
    let config: String = row.get(5)?;
    let config: Config =
        serde_json::from_str(&config).map_err(|e| corrupt(format!("frontends.config: {e}")))?;
    Ok(Frontend {
        id: id
            .parse()
            .map_err(|e| corrupt(format!("frontends.id: {e}")))?,
        input: FrontendInput {
            slug: row.get(1)?,
            name: row.get(2)?,
            description: row.get(3)?,
            enabled: row.get(4)?,
            access: config.access,
            downloads: config.downloads,
            signed_link_days: config.signed_link_days,
        },
        has_secret: row.get(6)?,
        created_at: timestamp("created_at", row.get(7)?).map_err(|e| corrupt(e.to_string()))?,
        updated_at: timestamp("updated_at", row.get(8)?).map_err(|e| corrupt(e.to_string()))?,
    })
}

/// What an older view picked its jobs by, staged by the migration that took it off the view
struct OldRouting {
    id: FrontendId,
    slug: String,
    enabled: bool,
    profile: ProfileId,
    scope: OldScope,
    discord_members: bool,
}

/// The guilds and channels an older view showed jobs from, every job when both are empty
#[derive(Default, Deserialize)]
#[serde(default)]
struct OldScope {
    guilds: Vec<String>,
    channels: Vec<String>,
}

impl OldScope {
    fn is_everything(&self) -> bool {
        self.guilds.is_empty() && self.channels.is_empty()
    }

    /// How closely the scope fit a job, a listed channel over a listed guild over everything
    fn closeness(&self, guild: Option<&str>, channel: Option<&str>) -> Option<u8> {
        if channel.is_some_and(|c| self.channels.iter().any(|x| x == c)) {
            Some(2)
        } else if guild.is_some_and(|g| self.guilds.iter().any(|x| x == g)) {
            Some(1)
        } else if self.is_everything() {
            Some(0)
        } else {
            None
        }
    }
}

/// The guild a channel is in, as a watch rule or a job seen there recorded it
fn guild_of_channel(conn: &Connection, channel: &str) -> rusqlite::Result<Option<String>> {
    if let Some(guild) = conn
        .query_row(
            "SELECT guild_id FROM watch_rules WHERE channel_id = ?1 LIMIT 1",
            params![channel],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(Some(guild));
    }
    conn.query_row(
        "SELECT guild_id FROM jobs WHERE channel_id = ?1 AND guild_id IS NOT NULL LIMIT 1",
        params![channel],
        |row| row.get::<_, String>(0),
    )
    .optional()
}

/// Routes each older view through the profiles of the scopes it showed and stamps the jobs it showed
fn convert_routing(conn: &Connection, profiles: &ProfileCache) -> Result<bool, FrontendError> {
    if !profiles::table_exists(conn, "frontend_routing")? {
        return Ok(false);
    }
    let mut stmt = conn.prepare(
        "SELECT frontend_id, slug, enabled, profile_id, scope, discord_members FROM frontend_routing
         ORDER BY slug",
    )?;
    let views: Vec<OldRouting> = stmt
        .query_map([], |row| {
            let id: String = row.get(0)?;
            let profile: String = row.get(3)?;
            let scope: String = row.get(4)?;
            Ok(OldRouting {
                id: id
                    .parse()
                    .map_err(|e| corrupt(format!("frontend_routing.frontend_id: {e}")))?,
                slug: row.get(1)?,
                enabled: row.get(2)?,
                profile: profile
                    .parse()
                    .map_err(|e| corrupt(format!("frontend_routing.profile_id: {e}")))?,
                scope: serde_json::from_str(&scope)
                    .map_err(|e| corrupt(format!("frontend_routing.scope: {e}")))?,
                discord_members: row.get(5)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    drop(stmt);
    for view in views.iter().filter(|v| v.enabled) {
        let mut scopes: Vec<Scope> = view
            .scope
            .guilds
            .iter()
            .map(|guild| Scope::Guild {
                guild_id: guild.clone(),
            })
            .collect();
        for channel in &view.scope.channels {
            match guild_of_channel(conn, channel)? {
                Some(guild) => scopes.push(Scope::Channel {
                    guild_id: guild,
                    channel_id: channel.clone(),
                }),
                None => tracing::warn!(
                    view = view.slug,
                    channel,
                    "content view scope dropped: no watch rule or job names the channel's server, so nothing was ever routed from it"
                ),
            }
        }
        for scope in scopes {
            let chain = match &scope {
                Scope::Channel {
                    guild_id,
                    channel_id,
                } => Scope::chain(Some(guild_id), Some(channel_id), None),
                other => Scope::chain(other.guild_id(), None, None),
            };
            let named = view.id.to_string();
            let already = profiles.effective_for(&chain).delivery.view == View::Id(named.clone());
            if already || profiles.view_named_at(&scope).is_some() {
                continue;
            }
            profiles::ensure_overlay(
                conn,
                profiles,
                &scope,
                &|sections| sections.delivery.view = Some(View::Id(named.clone())),
                &profiles::CONVERTED_BY,
                Some(json!({ "content_view": view.slug })),
            )?;
        }
    }
    if let Some(widest) = views.iter().find(|v| v.enabled && v.scope.is_everything()) {
        profiles::route_builtin_to_view(
            conn,
            profiles,
            &widest.id.to_string(),
            json!({ "content_view": widest.slug }),
        )?;
    }
    let stamped = stamp_jobs(conn, &views, profiles)?;
    for view in views.iter().filter(|v| v.discord_members) {
        let mut guilds = view.scope.guilds.clone();
        for channel in &view.scope.channels {
            if let Some(guild) = guild_of_channel(conn, channel)?
                && !guilds.contains(&guild)
            {
                guilds.push(guild);
            }
        }
        conn.execute(
            "UPDATE frontends SET config = json_set(config, '$.access.discord_guilds', json(?2))
             WHERE id = ?1",
            params![
                view.id.to_string(),
                serde_json::to_string(&guilds)
                    .map_err(|e| FrontendError::Store(StoreError::Corrupt(e.to_string())))?
            ],
        )?;
    }
    conn.execute("DROP TABLE frontend_routing", [])?;
    tracing::info!(
        views = views.len(),
        jobs = stamped,
        "content views now get their media from the profiles routing to them"
    );
    Ok(true)
}

/// Stamps every job naming no view with the closest older view that showed it, counting them
fn stamp_jobs(
    conn: &Connection,
    views: &[OldRouting],
    profiles: &ProfileCache,
) -> Result<usize, FrontendError> {
    let shown: Vec<&OldRouting> = views.iter().filter(|v| v.enabled).collect();
    if shown.is_empty() {
        return Ok(0);
    }
    let ids: Vec<String> = {
        let mut stmt = conn.prepare("SELECT id FROM jobs WHERE view_id IS NULL")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect::<Result<_, _>>()?
    };
    let mut stamped = 0;
    for id in ids {
        let data: String =
            conn.query_row("SELECT data FROM jobs WHERE id = ?1", params![id], |row| {
                row.get(0)
            })?;
        let mut job: Job = serde_json::from_str(&data)
            .map_err(|e| StoreError::Corrupt(format!("jobs.data of {id}: {e}")))?;
        if job.request.policy.delivery.view != View::None {
            continue;
        }
        let guild = job.request.origin.guild.as_deref();
        let channel = job.request.origin.channel.as_deref();
        let resolver = job.resolver();
        let Some(view) = shown
            .iter()
            .filter(|v| {
                resolver.is_none_or(|r| {
                    profiles
                        .alone(v.profile)
                        .is_some_and(|platforms| platforms.get(r).copied().unwrap_or(false))
                })
            })
            .filter_map(|v| v.scope.closeness(guild, channel).map(|c| (c, *v)))
            .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.slug.cmp(&a.1.slug)))
            .map(|(_, v)| v)
        else {
            continue;
        };
        job.request.policy.delivery.view = View::Id(view.id.to_string());
        let data = serde_json::to_string(&job).map_err(|e| StoreError::Corrupt(e.to_string()))?;
        conn.execute(
            "UPDATE jobs SET data = ?2, view_id = ?3 WHERE id = ?1",
            params![id, data, view.id.to_string()],
        )?;
        stamped += 1;
    }
    Ok(stamped)
}

fn row_to_user(row: &rusqlite::Row<'_>) -> rusqlite::Result<FrontendUser> {
    let id: String = row.get(0)?;
    let frontend: String = row.get(1)?;
    Ok(FrontendUser {
        id: id
            .parse()
            .map_err(|e| corrupt(format!("frontend_users.id: {e}")))?,
        frontend_id: frontend
            .parse()
            .map_err(|e| corrupt(format!("frontend_users.frontend_id: {e}")))?,
        username: row.get(2)?,
        created_at: timestamp("created_at", row.get(3)?).map_err(|e| corrupt(e.to_string()))?,
    })
}

#[cfg(test)]
mod tests {
    use discoclip_engine::job::{Origin, Request, SourceId};
    use discoclip_engine::store::JobStore;

    use super::*;
    use crate::profiles::{PlatformFacts, ProfileInput, ProfileStore};

    fn test_platforms() -> Vec<PlatformFacts> {
        let facts = |id: &'static str| PlatformFacts {
            id,
            tags: &[],
            hosts: &[],
            on_by_default: true,
        };
        vec![facts("youtube"), facts("reddit"), facts("web")]
    }

    async fn stores() -> (FrontendStore, ProfileStore, SqliteStore) {
        let db = SqliteStore::open_in_memory().await.unwrap();
        crate::migrations::apply(&db).await.unwrap();
        let profiles = ProfileStore::new(db.clone(), test_platforms());
        profiles.load().await.unwrap();
        let store = FrontendStore::new(
            db.clone(),
            Keyring::from_key([9; 32]),
            profiles.cache(),
            Arc::new(PublicUrl::new(
                Some(Url::parse("https://clips.example").unwrap()),
                db.clone(),
            )),
        );
        store.load().await.unwrap();
        (store, profiles, db)
    }

    fn actor() -> Actor {
        Actor::Provisioning { file: None }
    }

    fn known() -> Known {
        Known {
            providers: vec!["discord".to_string(), "github".to_string()],
        }
    }

    fn input(slug: &str) -> FrontendInput {
        FrontendInput {
            name: slug.to_uppercase(),
            slug: slug.into(),
            description: String::new(),
            enabled: true,
            access: Access::default(),
            downloads: true,
            signed_link_days: 30,
        }
    }

    fn job(guild: Option<&str>, channel: Option<&str>, resolver: &str) -> Job {
        let mut job = Job::new(Request::new(
            Origin {
                source: SourceId::new("discord"),
                reference: "x".into(),
                url: None,
                guild: guild.map(String::from),
                channel: channel.map(String::from),
            },
            Url::parse("https://a.test/v").unwrap(),
        ));
        let mut resolved = discoclip_engine::resolve::Resolved::new(resolver);
        resolved.title = Some("t".into());
        job.artifacts.resolved = Some(resolved);
        job
    }

    /// `job` as a profile naming `view` would have stamped it
    fn published_on(mut job: Job, view: &str) -> Job {
        job.request.policy.delivery.view = View::Id(view.to_string());
        job
    }

    #[tokio::test]
    async fn front_ends_are_checked_and_cached_by_slug() {
        let (store, profiles, _) = stores().await;
        let known = known();
        assert!(matches!(
            store
                .create(&actor(), input("Bad Slug"), &known)
                .await
                .unwrap_err(),
            FrontendError::Invalid(_)
        ));
        assert!(matches!(
            store
                .create(&actor(), input("api"), &known)
                .await
                .unwrap_err(),
            FrontendError::Invalid(_)
        ));
        let mut members = input("members");
        members.access.discord_guilds = vec!["5".into()];
        assert!(matches!(
            store
                .create(&actor(), members.clone(), &known)
                .await
                .unwrap_err(),
            FrontendError::Invalid(_)
        ));
        members.access.providers = vec!["discord".into()];
        let created = store.create(&actor(), members, &known).await.unwrap();
        assert_eq!(created.input.slug, "members");
        assert_eq!(created.input.access.discord_guilds, vec!["5".to_string()]);
        assert!(!created.has_secret);
        let mut odd = input("odd");
        odd.access.providers = vec!["discord".into()];
        odd.access.discord_guilds = vec!["five".into()];
        assert!(matches!(
            store.create(&actor(), odd, &known).await.unwrap_err(),
            FrontendError::Invalid(_)
        ));
        let mut unknown = input("unknown");
        unknown.access.providers = vec!["myspace".into()];
        assert!(matches!(
            store.create(&actor(), unknown, &known).await.unwrap_err(),
            FrontendError::UnknownProvider(_)
        ));
        assert!(matches!(
            store
                .create(&actor(), input("members"), &known)
                .await
                .unwrap_err(),
            FrontendError::DuplicateSlug(_)
        ));
        let mut short = input("links");
        short.signed_link_days = 0;
        assert!(matches!(
            store.create(&actor(), short, &known).await.unwrap_err(),
            FrontendError::Invalid(_)
        ));
        let linking = store
            .create(&actor(), input("links"), &known)
            .await
            .unwrap();
        assert_eq!(linking.input.signed_link_days, 30);
        let cache = store.cache();
        assert_eq!(cache.get("LINKS").unwrap().id, linking.id);
        assert!(cache.get("nothing").is_none());
        assert!(cache.any_enabled());
        assert_eq!(cache.views().get(&linking.id.to_string()), Some(&true));

        // A profile that sends media to the view keeps it from being deleted.
        let mut names_it = ProfileInput::named("Links here");
        names_it.delivery.view = Some(View::Id(linking.id.to_string()));
        let names_it = profiles
            .create(
                &actor(),
                names_it,
                &profiles.cache().known(cache.views(), true),
            )
            .await
            .unwrap();
        assert!(matches!(
            store.delete(&actor(), linking.id).await.unwrap_err(),
            FrontendError::InUse(1)
        ));
        profiles.delete(&actor(), names_it.id).await.unwrap();

        let mut disabled = linking.input.clone();
        disabled.enabled = false;
        store
            .update(&actor(), linking.id, disabled, &known)
            .await
            .unwrap();
        assert!(store.cache().get("links").is_none());
        assert_eq!(
            store.cache().views().get(&linking.id.to_string()),
            Some(&false)
        );
        store.delete(&actor(), linking.id).await.unwrap();
        assert!(store.get(linking.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_view_the_policy_names_is_linked_and_shows_the_job() {
        let (store, profiles, db) = stores().await;
        let known = known();
        let seen = job(Some("5"), Some("1"), "reddit");
        assert!(matches!(
            store.cache().link_for(&seen),
            Err(LinkError::NoView)
        ));
        let all = store.create(&actor(), input("all"), &known).await.unwrap();
        let mut off = input("off");
        off.enabled = false;
        let off = store.create(&actor(), off, &known).await.unwrap();
        let cache = store.cache();

        let on_all = published_on(seen.clone(), &all.id.to_string());
        let link = cache.link_for(&on_all).unwrap();
        assert_eq!(
            link.page.as_str(),
            format!("https://clips.example/f/all/j/{}", on_all.id)
        );
        assert_eq!(link.view, all.id.to_string());
        assert!(all.shows(&on_all));
        assert!(!all.shows(&seen));
        assert!(!off.shows(&on_all));
        // The job's origin and platform no longer matter: the profile decided.
        let elsewhere = published_on(job(None, None, "youtube"), &all.id.to_string());
        assert!(all.shows(&elsewhere));
        assert!(cache.link_for(&elsewhere).is_ok());
        assert!(matches!(
            cache.link_for(&published_on(seen.clone(), &off.id.to_string())),
            Err(LinkError::Disabled(_))
        ));
        assert!(matches!(
            cache.link_for(&published_on(seen.clone(), "nope")),
            Err(LinkError::Unknown(_))
        ));

        // Without a public address there is no page to point at.
        let unaddressed = FrontendStore::new(
            db.clone(),
            Keyring::from_key([9; 32]),
            profiles.cache(),
            Arc::new(PublicUrl::new(None, db)),
        );
        unaddressed.load().await.unwrap();
        assert!(matches!(
            unaddressed.cache().link_for(&on_all),
            Err(LinkError::NoPublicUrl)
        ));
    }

    /// Content views as an older database kept them, picking jobs by scope and profile:
    /// the migration stages what they picked by and the store turns it into routing.
    #[tokio::test]
    async fn older_views_route_through_the_profiles_and_stamp_their_jobs() {
        use discoclip_engine::store::migrate::Migration;

        const ALL: &str = "0193b000-0000-7000-8000-0000000000a1";
        const GUILD: &str = "0193b000-0000-7000-8000-0000000000a2";
        const CHAN: &str = "0193b000-0000-7000-8000-0000000000a3";
        const OFF: &str = "0193b000-0000-7000-8000-0000000000a4";
        const NO_YOUTUBE: &str = "0193b000-0000-7000-8000-0000000000bb";

        let db = SqliteStore::open_in_memory().await.unwrap();
        let before: &'static [Migration] = Box::leak(
            crate::migrations::MIGRATIONS
                .iter()
                .copied()
                .take_while(|m| m.name != "frontend_routing")
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );
        db.migrate(crate::migrations::SCOPE, before).await.unwrap();
        db.call(|conn| {
            Ok(conn.execute_batch(&format!(
                "INSERT INTO profiles (id, name, description, platforms, builtin, created_at, updated_at)
                 VALUES ('{NO_YOUTUBE}', 'No YouTube', '', '{{\"default\":\"inherit\",\"overrides\":{{\"youtube\":false}}}}', 0, 0, 0);
                 INSERT INTO discord_applications (id, name, client_id, client_secret, bot_token, created_at, updated_at)
                 VALUES ('0193b000-0000-7000-8000-000000000001', 'A', '1', NULL, 't', 0, 0);
                 INSERT INTO watch_rules (id, application_id, guild_id, channel_id, enabled, created_at, updated_at)
                 VALUES ('r1', '0193b000-0000-7000-8000-000000000001', '5', '1', 1, 0, 0);
                 INSERT INTO frontends (id, slug, name, description, enabled, profile_id, config, secret_hash, created_at, updated_at)
                 VALUES
                 ('{ALL}', 'all', 'All', '', 1, '{}',
                     '{{\"scope\":{{\"guilds\":[],\"channels\":[]}},\"access\":{{\"open\":true}},\"downloads\":true,\"signed_link_days\":30}}', NULL, 0, 0),
                 ('{GUILD}', 'guild', 'Guild', '', 1, '{NO_YOUTUBE}',
                     '{{\"scope\":{{\"guilds\":[\"5\"],\"channels\":[\"1\"]}},\"access\":{{\"providers\":[\"discord\"],\"discord_members\":true}},\"downloads\":true,\"signed_link_days\":30}}', NULL, 0, 0),
                 ('{CHAN}', 'chan', 'Chan', '', 1, '{}',
                     '{{\"scope\":{{\"guilds\":[],\"channels\":[\"1\"]}},\"access\":{{\"open\":true,\"discord_members\":false}},\"downloads\":true,\"signed_link_days\":30}}', NULL, 0, 0),
                 ('{OFF}', 'off', 'Off', '', 0, '{}',
                     '{{\"scope\":{{\"guilds\":[],\"channels\":[]}},\"access\":{{\"open\":true}},\"downloads\":true,\"signed_link_days\":30}}', NULL, 0, 0);
                 INSERT INTO frontend_users (id, frontend_id, username, password_hash, created_at)
                 VALUES ('0193b000-0000-7000-8000-0000000000e1', '{ALL}', 'alice', 'h', 0);
                 INSERT INTO frontend_sessions (id, frontend_id, token_hash, subject, display, created_at, last_seen_at, expires_at)
                 VALUES ('0193b000-0000-7000-8000-0000000000e2', '{ALL}', 't', 'account:alice', 'alice', 0, 0, 9223372036854775807);",
                ProfileId::DEFAULT, ProfileId::DEFAULT, ProfileId::DEFAULT,
            ))?)
        })
        .await
        .unwrap();
        let in_channel = job(Some("5"), Some("1"), "reddit");
        let in_guild = job(Some("5"), Some("2"), "reddit");
        let hidden_in_guild = job(Some("5"), Some("2"), "youtube");
        let local = job(None, None, "web");
        let kept = published_on(job(Some("5"), Some("1"), "reddit"), "keep");
        for job in [&in_channel, &in_guild, &hidden_in_guild, &local, &kept] {
            db.insert(job).await.unwrap();
        }

        crate::migrations::apply(&db).await.unwrap();
        let profiles = ProfileStore::new(db.clone(), test_platforms());
        profiles.load().await.unwrap();
        let store = FrontendStore::new(
            db.clone(),
            Keyring::from_key([9; 32]),
            profiles.cache(),
            Arc::new(PublicUrl::new(
                Some(Url::parse("https://clips.example").unwrap()),
                db.clone(),
            )),
        );
        assert_eq!(store.load().await.unwrap(), 4);

        // Accounts and sessions survived the rebuild of the table they hang from.
        let all_id: FrontendId = ALL.parse().unwrap();
        assert_eq!(store.users(all_id).await.unwrap().len(), 1);
        assert_eq!(store.sessions(all_id).await.unwrap().len(), 1);
        // The membership check kept the scope's server, the channel's included.
        let guild_view = store.get(GUILD.parse().unwrap()).await.unwrap().unwrap();
        assert_eq!(
            guild_view.input.access.discord_guilds,
            vec!["5".to_string()]
        );
        let chan_view = store.get(CHAN.parse().unwrap()).await.unwrap().unwrap();
        assert!(chan_view.input.access.discord_guilds.is_empty());
        assert!(chan_view.input.access.open);

        // The widest view is the built-in profile's, the scoped ones their scopes' own. A
        // channel already under its guild's view gets no profile of its own.
        let cache = profiles.cache();
        let named: Vec<String> = profiles
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|p| p.input.name)
            .collect();
        assert!(named.contains(&"Server 5 options".to_string()), "{named:?}");
        assert!(
            named.contains(&"Channel 1 options".to_string()),
            "{named:?}"
        );
        assert_eq!(named.len(), 4, "{named:?}");
        assert_eq!(
            cache.effective(None, None, None).delivery.view,
            View::Id(ALL.into())
        );
        assert_eq!(
            cache.effective(Some("5"), Some("2"), None).delivery.view,
            View::Id(GUILD.into())
        );
        assert_eq!(
            cache.effective(Some("5"), Some("1"), None).delivery.view,
            View::Id(CHAN.into())
        );
        assert_eq!(
            cache.effective(Some("6"), None, None).delivery.view,
            View::Id(ALL.into())
        );

        // Every job carries the view that showed it, and a named one keeps its own.
        let view_of = |job: &Job| {
            let id = job.id;
            let db = db.clone();
            async move {
                db.get(id)
                    .await
                    .unwrap()
                    .unwrap()
                    .request
                    .policy
                    .delivery
                    .view
            }
        };
        assert_eq!(view_of(&in_channel).await, View::Id(CHAN.into()));
        assert_eq!(view_of(&in_guild).await, View::Id(GUILD.into()));
        assert_eq!(view_of(&hidden_in_guild).await, View::Id(ALL.into()));
        assert_eq!(view_of(&local).await, View::Id(ALL.into()));
        assert_eq!(view_of(&kept).await, View::Id("keep".into()));
        let on_chan = db
            .list(&discoclip_engine::store::JobFilter {
                view: Some(CHAN.into()),
                ..Default::default()
            })
            .await
            .unwrap();
        assert_eq!(on_chan.len(), 1);
        assert_eq!(on_chan[0].id, in_channel.id);

        // The staging is gone, so loading again converts nothing.
        assert_eq!(store.load().await.unwrap(), 4);
        assert!(
            !db.call(|conn| Ok(profiles::table_exists(conn, "frontend_routing")?))
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn secrets_accounts_sessions_and_media_tokens() {
        let (store, _, _) = stores().await;
        let known = known();
        let mut gated = input("gated");
        gated.access.secret_kind = Some(SecretKind::Pin);
        gated.access.accounts = true;
        let gated = store.create(&actor(), gated, &known).await.unwrap();
        assert!(!store.verify_secret(gated.id, "1234").await.unwrap());
        assert!(matches!(
            store
                .set_secret(&actor(), gated.id, Some("12"))
                .await
                .unwrap_err(),
            FrontendError::Invalid(_)
        ));
        let gated = store
            .set_secret(&actor(), gated.id, Some("1234"))
            .await
            .unwrap();
        assert!(gated.has_secret);
        assert!(store.verify_secret(gated.id, "1234").await.unwrap());
        assert!(!store.verify_secret(gated.id, "4321").await.unwrap());

        let user = store
            .create_user(&actor(), gated.id, "alice", "correct horse")
            .await
            .unwrap();
        assert!(matches!(
            store
                .create_user(&actor(), gated.id, "Alice", "correct horse")
                .await
                .unwrap_err(),
            FrontendError::DuplicateUser(..)
        ));
        assert!(
            store
                .verify_user(gated.id, "alice", "correct horse")
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .verify_user(gated.id, "alice", "wrong")
                .await
                .unwrap()
                .is_none()
        );
        store
            .set_user_password(&actor(), gated.id, user.id, "battery staple")
            .await
            .unwrap();
        assert!(
            store
                .verify_user(gated.id, "alice", "battery staple")
                .await
                .unwrap()
                .is_some()
        );

        let viewer = Viewer {
            frontend_id: gated.id,
            subject: "account:alice".into(),
            display: "alice".into(),
        };
        let token = store.open_session(&viewer, None, None).await.unwrap();
        assert_eq!(
            store.authenticate(gated.id, &token).await.unwrap(),
            Some(viewer.clone())
        );
        assert!(
            store
                .authenticate(gated.id, "nope")
                .await
                .unwrap()
                .is_none()
        );
        let other = store
            .create(&actor(), input("other"), &known)
            .await
            .unwrap();
        assert!(
            store
                .authenticate(other.id, &token)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(store.sessions(gated.id).await.unwrap().len(), 1);
        store
            .delete_user(&actor(), gated.id, user.id)
            .await
            .unwrap();
        assert!(
            store
                .authenticate(gated.id, &token)
                .await
                .unwrap()
                .is_none()
        );
        assert!(store.users(gated.id).await.unwrap().is_empty());

        let token = store.open_session(&viewer, None, None).await.unwrap();
        assert_eq!(
            store
                .revoke_sessions(&actor(), gated.id, None)
                .await
                .unwrap(),
            1
        );
        assert!(
            store
                .authenticate(gated.id, &token)
                .await
                .unwrap()
                .is_none()
        );

        let job = Uuid::now_v7();
        let media = store.sign_media(&gated, job);
        assert!(store.verify_media(gated.id, job, &media));
        assert!(!store.verify_media(other.id, job, &media));
        assert!(!store.verify_media(gated.id, Uuid::now_v7(), &media));
        assert!(!store.verify_media(gated.id, job, "garbage"));
        let cleared = store.set_secret(&actor(), gated.id, None).await.unwrap();
        assert!(!cleared.has_secret);
    }
}
