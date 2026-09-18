//! Front ends: public sites the server hosts, each a browser over the media the server
//! has made, scoped to a guild, a channel or everything and to the platforms a profile
//! turns on. A front end lets viewers in as its access says: to anyone, with a shared
//! secret, with an account of its own, or through a login provider, Discord among them
//! with membership of the scoped guilds required. A front end can also be where Discord
//! is sent instead of an upload: when a job's media cannot be uploaded well, the bot
//! posts the front end's page for it, which plays the media inline. Edited in the app,
//! read by the bots and the public routes through a cache the store keeps up. Every
//! change is written to the audit log in the same transaction.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};

use discoclip_bot::{LinkTargets, MediaLink};
use discoclip_engine::StoreError;
use discoclip_engine::job::Job;
use discoclip_engine::publish::{Fallback, QualityFloor};
use discoclip_engine::rusqlite::{self, Connection, OptionalExtension, params};
use discoclip_engine::store::sqlite::SqliteStore;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use serde_json::json;
use url::Url;
use uuid::Uuid;

use crate::audit::{self, Action, Actor, Target};
use crate::db::{nanos, timestamp, transact};
use crate::profiles::{ProfileCache, ProfileId};
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

/// Which jobs a front end shows: those seen in any of `guilds` and `channels`, or every
/// job when both are empty. A job in a listed channel counts whatever its guild.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContentScope {
    pub guilds: Vec<String>,
    pub channels: Vec<String>,
}

impl ContentScope {
    pub fn is_everything(&self) -> bool {
        self.guilds.is_empty() && self.channels.is_empty()
    }

    /// Whether a job seen in `guild` and `channel` is in scope.
    pub fn covers(&self, guild: Option<&str>, channel: Option<&str>) -> bool {
        if self.is_everything() {
            return true;
        }
        guild.is_some_and(|g| self.guilds.iter().any(|x| x == g))
            || channel.is_some_and(|c| self.channels.iter().any(|x| x == c))
    }

    /// How closely the scope fits a job: a listed channel is closer than a listed guild,
    /// which is closer than everything.
    fn closeness(&self, guild: Option<&str>, channel: Option<&str>) -> u8 {
        if channel.is_some_and(|c| self.channels.iter().any(|x| x == c)) {
            2
        } else if guild.is_some_and(|g| self.guilds.iter().any(|x| x == g)) {
            1
        } else {
            0
        }
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
/// login through one of the listed providers, where a Discord login may also have to be
/// a member of every guild in the front end's scope, or one of the listed users.
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
    /// A Discord login must belong to every guild in the scope.
    pub discord_members: bool,
    /// A Discord login must be one of these users, by id, when any are listed.
    pub discord_users: Vec<String>,
}

impl Access {
    /// Whether a way in besides being open is set up, so a viewer can be asked to log in.
    pub fn has_login(&self, has_secret: bool) -> bool {
        (has_secret && self.secret_kind.is_some()) || self.accounts || !self.providers.is_empty()
    }
}

/// When the bot posts the front end's page instead of uploading: never, or whenever an
/// upload would be too large or, for a video, reduced under the floor. The page's
/// output is bounded by `max_bytes`, and the link the page hands Discord to play the
/// media stays good for `signed_link_days`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LinkPolicy {
    pub enabled: bool,
    pub min_height: u32,
    pub min_bitrate: u64,
    pub max_bytes: u64,
    pub signed_link_days: u32,
}

impl Default for LinkPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            min_height: 720,
            min_bitrate: 1_500_000,
            max_bytes: 2 * 1024 * 1024 * 1024,
            signed_link_days: 30,
        }
    }
}

impl LinkPolicy {
    pub fn fallback(&self) -> Fallback {
        Fallback {
            max_bytes: self.max_bytes,
            floor: QualityFloor {
                min_height: self.min_height,
                min_bitrate: self.min_bitrate,
            },
        }
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
    /// Which platforms' jobs are shown: the profile's toggles, applied on their own.
    #[serde(default = "default_profile")]
    pub profile_id: ProfileId,
    #[serde(default)]
    pub scope: ContentScope,
    #[serde(default)]
    pub access: Access,
    /// Whether viewers may download the media rather than only play it.
    #[serde(default = "yes")]
    pub downloads: bool,
    #[serde(default)]
    pub links: LinkPolicy,
}

fn yes() -> bool {
    true
}

fn default_profile() -> ProfileId {
    ProfileId::DEFAULT
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
    #[error("front end {0} not found")]
    NotFound(FrontendId),
    #[error("{0}")]
    Invalid(String),
    #[error("a front end already uses the slug {0}")]
    DuplicateSlug(String),
    #[error("{0} already has an account called {1}")]
    DuplicateUser(String, String),
    #[error("no profile is called {0}")]
    UnknownProfile(ProfileId),
    #[error("no login provider is called {0}")]
    UnknownProvider(String),
    #[error("posting links needs web.public_url to be set")]
    NoPublicUrl,
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

/// What a front end is checked against: the profiles and providers that exist, and
/// whether the server has a public URL to build links from.
#[derive(Clone)]
pub struct Known {
    pub profiles: ProfileCache,
    pub providers: Vec<String>,
    pub public_url: bool,
}

fn check(input: &FrontendInput, known: &Known) -> Result<FrontendInput, FrontendError> {
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err(FrontendError::Invalid("a front end needs a name".into()));
    }
    if name.chars().count() > NAME_MAX {
        return Err(FrontendError::Invalid(format!(
            "a front end's name is at most {NAME_MAX} characters"
        )));
    }
    let slug = input.slug.trim().to_string();
    check_slug(&slug)?;
    let description = input.description.trim().to_string();
    if description.chars().count() > DESCRIPTION_MAX {
        return Err(FrontendError::Invalid(format!(
            "a front end's description is at most {DESCRIPTION_MAX} characters"
        )));
    }
    if !known.profiles.has(input.profile_id) {
        return Err(FrontendError::UnknownProfile(input.profile_id));
    }
    for guild in &input.scope.guilds {
        snowflake("guild", guild)?;
    }
    for channel in &input.scope.channels {
        snowflake("channel", channel)?;
    }
    for user in &input.access.discord_users {
        snowflake("user", user)?;
    }
    for provider in &input.access.providers {
        if !known.providers.iter().any(|p| p == provider) {
            return Err(FrontendError::UnknownProvider(provider.clone()));
        }
    }
    if (input.access.discord_members || !input.access.discord_users.is_empty())
        && !input.access.providers.iter().any(|p| p == "discord")
    {
        return Err(FrontendError::Invalid(
            "Discord membership checks need the discord provider among the front end's providers"
                .into(),
        ));
    }
    if input.access.discord_members && input.scope.guilds.is_empty() {
        return Err(FrontendError::Invalid(
            "requiring Discord membership needs a guild in the front end's scope".into(),
        ));
    }
    if input.links.enabled {
        if !known.public_url {
            return Err(FrontendError::NoPublicUrl);
        }
        if input.links.min_height == 0 || input.links.min_bitrate == 0 {
            return Err(FrontendError::Invalid(
                "the quality floor's height and bitrate must be above zero".into(),
            ));
        }
        if input.links.max_bytes == 0 {
            return Err(FrontendError::Invalid(
                "the page's byte bound must be above zero".into(),
            ));
        }
        if input.links.signed_link_days == 0 {
            return Err(FrontendError::Invalid(
                "signed links must last at least a day".into(),
            ));
        }
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
    profiles: ProfileCache,
    public_url: Arc<RwLock<Option<Url>>>,
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

    /// Whether a job of `resolver` seen in `guild` and `channel` is shown by `frontend`.
    pub fn shows(
        &self,
        frontend: &Frontend,
        resolver: Option<&str>,
        guild: Option<&str>,
        channel: Option<&str>,
    ) -> bool {
        if !frontend.input.scope.covers(guild, channel) {
            return false;
        }
        match resolver {
            Some(resolver) => self
                .profiles
                .alone(frontend.input.profile_id)
                .is_some_and(|platforms| platforms.get(resolver).copied().unwrap_or(false)),
            None => true,
        }
    }

    /// The platforms `frontend` shows, by resolver id.
    pub fn platforms_of(&self, frontend: &Frontend) -> Vec<String> {
        self.profiles
            .alone(frontend.input.profile_id)
            .map(|platforms| {
                platforms
                    .into_iter()
                    .filter(|(_, on)| *on)
                    .map(|(id, _)| id)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The server's public URL, when set.
    pub fn public_url(&self) -> Option<Url> {
        self.public_url
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// The page of `job` on `frontend`, under the public URL.
    pub fn page_url(&self, frontend: &Frontend, job: &Job) -> Option<Url> {
        let base = self.public_url()?;
        base.join(&format!("f/{}/j/{}", frontend.input.slug, job.id))
            .ok()
    }

    /// Whether any front end posts links, so the public URL must stay set.
    pub fn any_posting_links(&self) -> bool {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .by_slug
            .values()
            .any(|c| c.frontend.input.enabled && c.frontend.input.links.enabled)
    }
}

impl LinkTargets for FrontendCache {
    /// The closest enabled front end that posts links and shows the job: a listed
    /// channel over a listed guild over everything, then by slug.
    fn link_for(&self, job: &Job) -> Option<MediaLink> {
        let guild = job.request.origin.guild.as_deref();
        let channel = job.request.origin.channel.as_deref();
        let resolver = job.resolver();
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let mut candidates: Vec<&Frontend> = inner
            .by_slug
            .values()
            .map(|c| &c.frontend)
            .filter(|f| f.input.enabled && f.input.links.enabled)
            .filter(|f| self.shows(f, resolver, guild, channel))
            .collect();
        candidates.sort_by(|a, b| {
            b.input
                .scope
                .closeness(guild, channel)
                .cmp(&a.input.scope.closeness(guild, channel))
                .then_with(|| a.input.slug.cmp(&b.input.slug))
        });
        let frontend = candidates.first()?;
        let Some(page) = self.page_url(frontend, job) else {
            tracing::warn!(
                frontend = frontend.input.slug,
                job = %job.id,
                "web.public_url is not set; the media will be uploaded, not linked"
            );
            return None;
        };
        Some(MediaLink {
            page,
            fallback: frontend.input.links.fallback(),
        })
    }
}

#[derive(Clone)]
pub struct FrontendStore {
    db: SqliteStore,
    keyring: Keyring,
    cache: FrontendCache,
}

const SELECT: &str = "SELECT id, slug, name, description, enabled, profile_id, config, \
     secret_hash IS NOT NULL, created_at, updated_at FROM frontends";

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
    /// Over a database the application's migrations have been applied to; `load` fills
    /// the cache before anything reads it.
    pub fn new(
        db: SqliteStore,
        keyring: Keyring,
        profiles: ProfileCache,
        public_url: Arc<RwLock<Option<Url>>>,
    ) -> Self {
        Self {
            db,
            keyring,
            cache: FrontendCache {
                profiles,
                public_url,
                inner: Arc::new(RwLock::new(CacheInner::default())),
            },
        }
    }

    /// What the bots and the public routes read.
    pub fn cache(&self) -> FrontendCache {
        self.cache.clone()
    }

    pub async fn load(&self) -> Result<usize, FrontendError> {
        let cache = self.cache.clone();
        transact(&self.db, move |tx| refresh(tx, &cache)).await
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
                "INSERT INTO frontends (id, slug, name, description, enabled, profile_id, config, secret_hash, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, ?8)",
                params![
                    id.to_string(),
                    input.slug,
                    input.name,
                    input.description,
                    input.enabled,
                    input.profile_id.to_string(),
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
                "UPDATE frontends SET slug = ?2, name = ?3, description = ?4, enabled = ?5, profile_id = ?6,
                 config = ?7, updated_at = ?8 WHERE id = ?1",
                params![
                    id.to_string(),
                    input.slug,
                    input.name,
                    input.description,
                    input.enabled,
                    input.profile_id.to_string(),
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

    /// Removes a front end with its accounts and sessions.
    pub async fn delete(&self, actor: &Actor, id: FrontendId) -> Result<(), FrontendError> {
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

    /// Stores the shared secret, hashed; `None` removes it.
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

    /// Opens a session for a viewer let in as `subject`; the token to set as the cookie.
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
    /// front end's signed links run out; what the page hands Discord to play the media.
    pub fn sign_media(&self, frontend: &Frontend, job: Uuid) -> String {
        let days = frontend.input.links.signed_link_days.max(1);
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
    scope: ContentScope,
    access: Access,
    downloads: bool,
    links: LinkPolicy,
}

fn encode_config(input: &FrontendInput) -> Result<String, FrontendError> {
    serde_json::to_string(&Config {
        scope: input.scope.clone(),
        access: input.access.clone(),
        downloads: input.downloads,
        links: input.links.clone(),
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
    let profile: String = row.get(5)?;
    let config: String = row.get(6)?;
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
            profile_id: profile
                .parse()
                .map_err(|e| corrupt(format!("frontends.profile_id: {e}")))?,
            scope: config.scope,
            access: config.access,
            downloads: config.downloads,
            links: config.links,
        },
        has_secret: row.get(7)?,
        created_at: timestamp("created_at", row.get(8)?).map_err(|e| corrupt(e.to_string()))?,
        updated_at: timestamp("updated_at", row.get(9)?).map_err(|e| corrupt(e.to_string()))?,
    })
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

    use super::*;
    use crate::profiles::{PlatformDefault, PlatformToggles, ProfileInput, ProfileStore};

    async fn stores() -> (FrontendStore, ProfileStore) {
        let db = SqliteStore::open_in_memory().await.unwrap();
        crate::migrations::apply(&db).await.unwrap();
        let profiles = ProfileStore::new(
            db.clone(),
            vec![("youtube", &[][..]), ("reddit", &[][..]), ("web", &[][..])],
        );
        profiles.load().await.unwrap();
        let store = FrontendStore::new(
            db,
            Keyring::from_key([9; 32]),
            profiles.cache(),
            Arc::new(RwLock::new(Some(
                Url::parse("https://clips.example").unwrap(),
            ))),
        );
        store.load().await.unwrap();
        (store, profiles)
    }

    fn actor() -> Actor {
        Actor::Provisioning { file: None }
    }

    fn known(profiles: &ProfileStore) -> Known {
        Known {
            profiles: profiles.cache(),
            providers: vec!["discord".to_string(), "github".to_string()],
            public_url: true,
        }
    }

    fn input(slug: &str) -> FrontendInput {
        FrontendInput {
            name: slug.to_uppercase(),
            slug: slug.into(),
            description: String::new(),
            enabled: true,
            profile_id: ProfileId::DEFAULT,
            scope: ContentScope::default(),
            access: Access::default(),
            downloads: true,
            links: LinkPolicy::default(),
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

    #[tokio::test]
    async fn front_ends_are_checked_and_cached_by_slug() {
        let (store, profiles) = stores().await;
        let known = known(&profiles);
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
        let mut odd = input("odd");
        odd.profile_id = ProfileId(Uuid::from_u128(77));
        assert!(matches!(
            store.create(&actor(), odd, &known).await.unwrap_err(),
            FrontendError::UnknownProfile(_)
        ));
        let mut members = input("members");
        members.access.discord_members = true;
        assert!(matches!(
            store
                .create(&actor(), members.clone(), &known)
                .await
                .unwrap_err(),
            FrontendError::Invalid(_)
        ));
        members.access.providers = vec!["discord".into()];
        members.scope.guilds = vec!["5".into()];
        let created = store.create(&actor(), members, &known).await.unwrap();
        assert_eq!(created.input.slug, "members");
        assert!(!created.has_secret);
        assert!(matches!(
            store
                .create(&actor(), input("members"), &known)
                .await
                .unwrap_err(),
            FrontendError::DuplicateSlug(_)
        ));
        let mut linking = input("links");
        linking.links.enabled = true;
        let without_url = Known {
            public_url: false,
            ..known.clone()
        };
        assert!(matches!(
            store
                .create(&actor(), linking.clone(), &without_url)
                .await
                .unwrap_err(),
            FrontendError::NoPublicUrl
        ));
        let linking = store.create(&actor(), linking, &known).await.unwrap();
        let cache = store.cache();
        assert_eq!(cache.get("LINKS").unwrap().id, linking.id);
        assert!(cache.get("nothing").is_none());
        assert!(cache.any_posting_links());

        let mut disabled = linking.input.clone();
        disabled.enabled = false;
        store
            .update(&actor(), linking.id, disabled, &known)
            .await
            .unwrap();
        assert!(store.cache().get("links").is_none());
        assert!(!store.cache().any_posting_links());
        store.delete(&actor(), linking.id).await.unwrap();
        assert!(store.get(linking.id).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_closest_front_end_that_posts_links_is_linked() {
        let (store, profiles) = stores().await;
        let known = known(&profiles);
        let no_youtube = profiles
            .create(
                &actor(),
                ProfileInput {
                    name: "No YouTube".into(),
                    description: String::new(),
                    platforms: PlatformToggles {
                        default: PlatformDefault::Inherit,
                        presets: Vec::new(),
                        overrides: [("youtube".to_string(), false)].into_iter().collect(),
                    },
                },
            )
            .await
            .unwrap();
        let mut everything = input("all");
        everything.links.enabled = true;
        store.create(&actor(), everything, &known).await.unwrap();
        let mut guild = input("guild");
        guild.scope.guilds = vec!["5".into()];
        guild.links.enabled = true;
        guild.profile_id = no_youtube.id;
        store.create(&actor(), guild, &known).await.unwrap();
        let mut channel = input("chan");
        channel.scope.channels = vec!["1".into()];
        channel.links.enabled = true;
        store.create(&actor(), channel, &known).await.unwrap();
        let mut silent = input("silent");
        silent.scope.channels = vec!["1".into()];
        store.create(&actor(), silent, &known).await.unwrap();
        let cache = store.cache();

        let seen = job(Some("5"), Some("1"), "reddit");
        let link = cache.link_for(&seen).unwrap();
        assert_eq!(
            link.page.as_str(),
            format!("https://clips.example/f/chan/j/{}", seen.id)
        );
        let link = cache
            .link_for(&job(Some("5"), Some("2"), "reddit"))
            .unwrap();
        assert!(
            link.page
                .as_str()
                .starts_with("https://clips.example/f/guild/j/")
        );
        assert_eq!(link.fallback.floor.min_height, 720);
        // The guild's front end hides YouTube, so the one for everything takes it.
        let link = cache
            .link_for(&job(Some("5"), Some("2"), "youtube"))
            .unwrap();
        assert!(
            link.page
                .as_str()
                .starts_with("https://clips.example/f/all/j/")
        );
        let link = cache.link_for(&job(None, None, "web")).unwrap();
        assert!(
            link.page
                .as_str()
                .starts_with("https://clips.example/f/all/j/")
        );

        let all = cache.get("all").unwrap();
        assert!(cache.shows(&all, Some("youtube"), None, None));
        let guild = cache.get("guild").unwrap();
        assert!(!cache.shows(&guild, Some("youtube"), Some("5"), None));
        assert!(cache.shows(&guild, Some("reddit"), Some("5"), None));
        assert!(!cache.shows(&guild, Some("reddit"), Some("6"), None));
        assert_eq!(cache.platforms_of(&guild), vec!["reddit", "web"]);
    }

    #[tokio::test]
    async fn secrets_accounts_sessions_and_media_tokens() {
        let (store, profiles) = stores().await;
        let known = known(&profiles);
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
