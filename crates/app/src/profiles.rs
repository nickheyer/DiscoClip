//! Profiles: named settings applied where links are seen. A profile says which platforms
//! are on and which are off; profiles are assigned to scopes, the whole server, a guild,
//! a channel in a guild, or a user in a guild, and the scopes apply from the widest to
//! the narrowest, each profile changing only what it names. The built-in `Default`
//! profile turns every platform on and is assigned to the whole server until another
//! takes its place. Edited in the app, read by the bots and the web app as they run
//! through a cache the store keeps up. Every change is written to the audit log in the
//! same transaction.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, RwLock};

use discoclip_bot::ProfileSource;
use discoclip_engine::StoreError;
use discoclip_engine::resolve::Tag;
use discoclip_engine::rusqlite::{self, Connection, OptionalExtension, params};
use discoclip_engine::store::sqlite::SqliteStore;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::json;
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, GuildMarker, UserMarker};
use uuid::Uuid;

use crate::audit::{self, Action, Actor, Target};
use crate::db::{nanos, timestamp, transact};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProfileId(pub Uuid);

impl ProfileId {
    /// The built-in profile every platform is on in; assigned to the whole server at first.
    pub const DEFAULT: ProfileId = ProfileId(Uuid::from_u128(1));
}

impl std::fmt::Display for ProfileId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for ProfileId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(s.parse()?))
    }
}

/// What a profile says about the platforms it does not name.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlatformDefault {
    /// Left as the wider scope has them.
    #[default]
    Inherit,
    Enabled,
    Disabled,
}

/// Which platforms a profile turns on and off. With `presets` chosen, they are the
/// whitelist: every platform in any chosen preset is on and every other off, which is
/// what the presets add up to. Without any, `default` says what happens to the
/// platforms `overrides` do not name. Overrides win either way.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlatformToggles {
    pub default: PlatformDefault,
    pub presets: Vec<String>,
    pub overrides: BTreeMap<String, bool>,
}

impl PlatformToggles {
    /// Applies the profile on top of `platforms`, as the scope it is assigned to narrows
    /// what the wider one allowed; `presets` maps preset ids to their platforms.
    fn apply(&self, platforms: &mut BTreeMap<String, bool>, presets: &Presets) {
        if self.presets.is_empty() {
            match self.default {
                PlatformDefault::Inherit => {}
                PlatformDefault::Enabled => platforms.values_mut().for_each(|on| *on = true),
                PlatformDefault::Disabled => platforms.values_mut().for_each(|on| *on = false),
            }
        } else {
            let allowed: HashSet<&str> = self
                .presets
                .iter()
                .filter_map(|id| presets.members.get(id.as_str()))
                .flat_map(|members| members.iter().map(String::as_str))
                .collect();
            for (platform, on) in platforms.iter_mut() {
                *on = allowed.contains(platform.as_str());
            }
        }
        for (platform, on) in &self.overrides {
            if let Some(entry) = platforms.get_mut(platform) {
                *entry = *on;
            }
        }
    }
}

/// A preset: a named set of platforms, from the tags the platforms carry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Preset {
    pub id: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    /// The platforms in it, by resolver id.
    pub platforms: Vec<String>,
}

/// How a preset is made from tags: one tag, or everything without one.
enum Membership {
    Tagged(Tag),
    Without(Tag),
}

const PRESET_RULES: [(&str, &str, &str, Membership); 12] = [
    (
        "basic",
        "Basic",
        "The mainstream platforms most links in a chat point at.",
        Membership::Tagged(Tag::Basic),
    ),
    (
        "sfw",
        "Safe for work",
        "Every platform that is not an adult site.",
        Membership::Without(Tag::Nsfw),
    ),
    (
        "nsfw",
        "Adult",
        "Adult sites and sites that carry adult content as a matter of course.",
        Membership::Tagged(Tag::Nsfw),
    ),
    (
        "news",
        "News",
        "News outlets and broadcasters' news programmes.",
        Membership::Tagged(Tag::News),
    ),
    (
        "social",
        "Social",
        "Social networks and forums: posts by people.",
        Membership::Tagged(Tag::Social),
    ),
    (
        "video",
        "Video",
        "General video sharing and hosting.",
        Membership::Tagged(Tag::Video),
    ),
    (
        "music",
        "Music",
        "Tracks, albums and mixes.",
        Membership::Tagged(Tag::Music),
    ),
    (
        "podcasts",
        "Podcasts",
        "Podcasts and spoken audio.",
        Membership::Tagged(Tag::Podcasts),
    ),
    (
        "live",
        "Live",
        "Live streaming, recordings of streams included.",
        Membership::Tagged(Tag::Live),
    ),
    (
        "files",
        "Files",
        "File hosts and archives.",
        Membership::Tagged(Tag::Files),
    ),
    (
        "images",
        "Images",
        "GIF and image hosts.",
        Membership::Tagged(Tag::Images),
    ),
    (
        "players",
        "Players",
        "Embedded players and delivery services other sites build on.",
        Membership::Tagged(Tag::Players),
    ),
];

/// The presets as the platforms make them.
#[derive(Debug, Clone, Default)]
pub struct Presets {
    list: Vec<Preset>,
    members: HashMap<&'static str, Vec<String>>,
}

impl Presets {
    /// From every platform and its tags.
    pub fn from_platforms(platforms: &[(&'static str, &'static [Tag])]) -> Self {
        let list: Vec<Preset> = PRESET_RULES
            .iter()
            .map(|(id, label, description, rule)| Preset {
                id,
                label,
                description,
                platforms: platforms
                    .iter()
                    .filter(|(_, tags)| match rule {
                        Membership::Tagged(tag) => tags.contains(tag),
                        Membership::Without(tag) => !tags.contains(tag),
                    })
                    .map(|(id, _)| id.to_string())
                    .collect(),
            })
            .collect();
        let members = list
            .iter()
            .map(|preset| (preset.id, preset.platforms.clone()))
            .collect();
        Self { list, members }
    }

    pub fn list(&self) -> &[Preset] {
        &self.list
    }

    pub fn has(&self, id: &str) -> bool {
        self.members.contains_key(id)
    }
}

/// What a profile says, as edited.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileInput {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub platforms: PlatformToggles,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Profile {
    pub id: ProfileId,
    #[serde(flatten)]
    pub input: ProfileInput,
    /// Ships with the server and cannot be removed.
    pub builtin: bool,
    pub created_at: Timestamp,
    pub updated_at: Timestamp,
}

/// Where a profile applies.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Scope {
    /// The whole server: every link from anywhere.
    Global,
    Guild {
        guild_id: String,
    },
    Channel {
        guild_id: String,
        channel_id: String,
    },
    User {
        guild_id: String,
        user_id: String,
    },
}

impl Scope {
    /// The one string the scope is stored and addressed by.
    pub fn key(&self) -> String {
        match self {
            Scope::Global => "global".into(),
            Scope::Guild { guild_id } => format!("guild:{guild_id}"),
            Scope::Channel {
                guild_id,
                channel_id,
            } => format!("channel:{guild_id}:{channel_id}"),
            Scope::User { guild_id, user_id } => format!("user:{guild_id}:{user_id}"),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Guild { .. } => "guild",
            Scope::Channel { .. } => "channel",
            Scope::User { .. } => "user",
        }
    }

    pub fn guild_id(&self) -> Option<&str> {
        match self {
            Scope::Global => None,
            Scope::Guild { guild_id }
            | Scope::Channel { guild_id, .. }
            | Scope::User { guild_id, .. } => Some(guild_id),
        }
    }

    /// The scopes that apply to a link seen in `channel` of `guild` from `user`, widest
    /// first.
    pub fn chain(guild: Option<&str>, channel: Option<&str>, user: Option<&str>) -> Vec<Scope> {
        let mut scopes = vec![Scope::Global];
        let Some(guild) = guild else {
            return scopes;
        };
        scopes.push(Scope::Guild {
            guild_id: guild.to_string(),
        });
        if let Some(channel) = channel {
            scopes.push(Scope::Channel {
                guild_id: guild.to_string(),
                channel_id: channel.to_string(),
            });
        }
        if let Some(user) = user {
            scopes.push(Scope::User {
                guild_id: guild.to_string(),
                user_id: user.to_string(),
            });
        }
        scopes
    }
}

impl std::str::FromStr for Scope {
    type Err = String;

    fn from_str(key: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = key.split(':').collect();
        let scope = match parts.as_slice() {
            ["global"] => Scope::Global,
            ["guild", guild] => Scope::Guild {
                guild_id: guild.to_string(),
            },
            ["channel", guild, channel] => Scope::Channel {
                guild_id: guild.to_string(),
                channel_id: channel.to_string(),
            },
            ["user", guild, user] => Scope::User {
                guild_id: guild.to_string(),
                user_id: user.to_string(),
            },
            _ => return Err(format!("{key:?} is not a profile scope")),
        };
        check_scope(&scope).map_err(|e| e.to_string())?;
        Ok(scope)
    }
}

/// A profile applied at a scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Assignment {
    pub scope: Scope,
    pub profile_id: ProfileId,
    pub updated_at: Timestamp,
}

/// What the profiles in force at a place add up to: every platform, on or off, and the
/// assignments that were applied to get there, widest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EffectiveProfile {
    pub platforms: BTreeMap<String, bool>,
    pub applied: Vec<Assignment>,
}

impl EffectiveProfile {
    /// The platforms turned off, by resolver id.
    pub fn disabled(&self) -> Vec<String> {
        self.platforms
            .iter()
            .filter(|(_, on)| !**on)
            .map(|(id, _)| id.clone())
            .collect()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("profile {0} not found")]
    NotFound(ProfileId),
    #[error("{0}")]
    Invalid(String),
    #[error("a profile is already called {0}")]
    Duplicate(String),
    #[error("the {0} profile ships with the server and cannot be removed")]
    Builtin(String),
    #[error("the {0} profile is assigned to the whole server; assign another first")]
    InUse(String),
    #[error("no platform is called {0}")]
    UnknownPlatform(String),
    #[error("no preset is called {0}")]
    UnknownPreset(String),
    #[error("the whole server always has a profile; assign another instead")]
    GlobalRequired,
}

impl From<rusqlite::Error> for ProfileError {
    fn from(error: rusqlite::Error) -> Self {
        ProfileError::Store(error.into())
    }
}

fn snowflake(what: &str, value: &str) -> Result<(), ProfileError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
        .map(|_| ())
        .ok_or_else(|| ProfileError::Invalid(format!("{what} {value:?} is not a Discord id")))
}

fn check_scope(scope: &Scope) -> Result<(), ProfileError> {
    match scope {
        Scope::Global => Ok(()),
        Scope::Guild { guild_id } => snowflake("guild", guild_id),
        Scope::Channel {
            guild_id,
            channel_id,
        } => {
            snowflake("guild", guild_id)?;
            snowflake("channel", channel_id)
        }
        Scope::User { guild_id, user_id } => {
            snowflake("guild", guild_id)?;
            snowflake("user", user_id)
        }
    }
}

const NAME_MAX: usize = 80;
const DESCRIPTION_MAX: usize = 500;

fn check(
    input: &ProfileInput,
    known: &[&'static str],
    presets: &Presets,
) -> Result<ProfileInput, ProfileError> {
    let name = input.name.trim().to_string();
    if name.is_empty() {
        return Err(ProfileError::Invalid("a profile needs a name".into()));
    }
    if name.chars().count() > NAME_MAX {
        return Err(ProfileError::Invalid(format!(
            "a profile's name is at most {NAME_MAX} characters"
        )));
    }
    let description = input.description.trim().to_string();
    if description.chars().count() > DESCRIPTION_MAX {
        return Err(ProfileError::Invalid(format!(
            "a profile's description is at most {DESCRIPTION_MAX} characters"
        )));
    }
    for platform in input.platforms.overrides.keys() {
        if !known.contains(&platform.as_str()) {
            return Err(ProfileError::UnknownPlatform(platform.clone()));
        }
    }
    for preset in &input.platforms.presets {
        if !presets.has(preset) {
            return Err(ProfileError::UnknownPreset(preset.clone()));
        }
    }
    Ok(ProfileInput {
        name,
        description,
        platforms: input.platforms.clone(),
    })
}

#[derive(Default)]
struct CacheInner {
    profiles: HashMap<ProfileId, PlatformToggles>,
    assignments: HashMap<String, (ProfileId, Timestamp)>,
}

/// The profiles and assignments as the bots and the web app read them, with the
/// platforms the engine has so a profile's default can be spelled out.
#[derive(Clone)]
pub struct ProfileCache {
    platforms: Arc<Vec<&'static str>>,
    presets: Arc<Presets>,
    inner: Arc<RwLock<CacheInner>>,
}

impl ProfileCache {
    fn new(platforms: Vec<(&'static str, &'static [Tag])>) -> Self {
        Self {
            presets: Arc::new(Presets::from_platforms(&platforms)),
            platforms: Arc::new(platforms.into_iter().map(|(id, _)| id).collect()),
            inner: Arc::new(RwLock::new(CacheInner::default())),
        }
    }

    /// The resolver ids the cache knows.
    pub fn platforms(&self) -> &[&'static str] {
        &self.platforms
    }

    /// The presets the platforms make.
    pub fn presets(&self) -> &Presets {
        &self.presets
    }

    /// What one profile amounts to on its own: every platform on, then the profile
    /// applied. `None` when no such profile exists.
    pub fn alone(&self, profile: ProfileId) -> Option<BTreeMap<String, bool>> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let toggles = inner.profiles.get(&profile)?;
        let mut platforms: BTreeMap<String, bool> = self
            .platforms
            .iter()
            .map(|id| (id.to_string(), true))
            .collect();
        toggles.apply(&mut platforms, &self.presets);
        Some(platforms)
    }

    /// Whether a profile exists.
    pub fn has(&self, profile: ProfileId) -> bool {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .profiles
            .contains_key(&profile)
    }

    /// What the profiles assigned along `scopes` add up to.
    pub fn effective_for(&self, scopes: &[Scope]) -> EffectiveProfile {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let mut platforms: BTreeMap<String, bool> = self
            .platforms
            .iter()
            .map(|id| (id.to_string(), true))
            .collect();
        let mut applied = Vec::new();
        for scope in scopes {
            let Some((profile_id, updated_at)) = inner.assignments.get(&scope.key()) else {
                continue;
            };
            let Some(toggles) = inner.profiles.get(profile_id) else {
                continue;
            };
            toggles.apply(&mut platforms, &self.presets);
            applied.push(Assignment {
                scope: scope.clone(),
                profile_id: *profile_id,
                updated_at: *updated_at,
            });
        }
        EffectiveProfile { platforms, applied }
    }

    pub fn effective(
        &self,
        guild: Option<&str>,
        channel: Option<&str>,
        user: Option<&str>,
    ) -> EffectiveProfile {
        self.effective_for(&Scope::chain(guild, channel, user))
    }
}

impl ProfileSource for ProfileCache {
    fn disabled_platforms(
        &self,
        guild: Option<Id<GuildMarker>>,
        channel: Option<Id<ChannelMarker>>,
        user: Option<Id<UserMarker>>,
    ) -> Vec<String> {
        let guild = guild.map(|id| id.to_string());
        let channel = channel.map(|id| id.to_string());
        let user = user.map(|id| id.to_string());
        self.effective(guild.as_deref(), channel.as_deref(), user.as_deref())
            .disabled()
    }
}

#[derive(Clone)]
pub struct ProfileStore {
    db: SqliteStore,
    cache: ProfileCache,
}

const SELECT: &str =
    "SELECT id, name, description, platforms, builtin, created_at, updated_at FROM profiles";

impl ProfileStore {
    /// Over a database the application's migrations have been applied to, for the
    /// engine's `platforms` with their tags; `load` fills the cache before anything
    /// reads it.
    pub fn new(db: SqliteStore, platforms: Vec<(&'static str, &'static [Tag])>) -> Self {
        Self {
            db,
            cache: ProfileCache::new(platforms),
        }
    }

    /// What the bots and the web app read.
    pub fn cache(&self) -> ProfileCache {
        self.cache.clone()
    }

    /// The resolver ids a profile may name.
    pub fn platforms(&self) -> &[&'static str] {
        self.cache.platforms()
    }

    /// Fills the cache from the database.
    pub async fn load(&self) -> Result<usize, ProfileError> {
        let cache = self.cache.clone();
        transact(&self.db, move |tx| refresh(tx, &cache)).await
    }

    pub async fn list(&self) -> Result<Vec<Profile>, ProfileError> {
        transact(&self.db, |tx| {
            let mut stmt = tx.prepare(&format!(
                "{SELECT} ORDER BY builtin DESC, name COLLATE NOCASE"
            ))?;
            let rows = stmt.query_map([], row_to_profile)?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    pub async fn get(&self, id: ProfileId) -> Result<Option<Profile>, ProfileError> {
        transact(&self.db, move |tx| get_in(tx, id)).await
    }

    pub async fn create(
        &self,
        actor: &Actor,
        input: ProfileInput,
    ) -> Result<Profile, ProfileError> {
        let input = check(&input, self.platforms(), self.cache.presets())?;
        let cache = self.cache.clone();
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let id = ProfileId(Uuid::now_v7());
            let now = Timestamp::now();
            let inserted = tx.execute(
                "INSERT INTO profiles (id, name, description, platforms, builtin, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5)",
                params![
                    id.to_string(),
                    input.name,
                    input.description,
                    encode(&input.platforms)?,
                    nanos(now),
                ],
            );
            match inserted {
                Ok(_) => {}
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    return Err(ProfileError::Duplicate(input.name));
                }
                Err(error) => return Err(error.into()),
            }
            refresh(tx, &cache)?;
            let profile = get_in(tx, id)?.ok_or(ProfileError::NotFound(id))?;
            audit::record(
                tx,
                &actor,
                Action::ProfileCreate,
                Target::profile(id, &profile.input.name),
                json!({ "profile": profile.input }),
            )?;
            Ok(profile)
        })
        .await
    }

    pub async fn update(
        &self,
        actor: &Actor,
        id: ProfileId,
        input: ProfileInput,
    ) -> Result<Profile, ProfileError> {
        let input = check(&input, self.platforms(), self.cache.presets())?;
        let cache = self.cache.clone();
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let previous = get_in(tx, id)?.ok_or(ProfileError::NotFound(id))?;
            let now = Timestamp::now();
            let updated = tx.execute(
                "UPDATE profiles SET name = ?2, description = ?3, platforms = ?4, updated_at = ?5
                 WHERE id = ?1",
                params![
                    id.to_string(),
                    input.name,
                    input.description,
                    encode(&input.platforms)?,
                    nanos(now),
                ],
            );
            match updated {
                Ok(0) => return Err(ProfileError::NotFound(id)),
                Ok(_) => {}
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == rusqlite::ErrorCode::ConstraintViolation =>
                {
                    return Err(ProfileError::Duplicate(input.name));
                }
                Err(error) => return Err(error.into()),
            }
            refresh(tx, &cache)?;
            let profile = get_in(tx, id)?.ok_or(ProfileError::NotFound(id))?;
            if profile.input != previous.input {
                audit::record(
                    tx,
                    &actor,
                    Action::ProfileUpdate,
                    Target::profile(id, &profile.input.name),
                    json!({ "profile": profile.input, "previous": previous.input }),
                )?;
            }
            Ok(profile)
        })
        .await
    }

    /// Removes a profile and every assignment of it; the built-in profile and the one
    /// assigned to the whole server stay.
    pub async fn delete(&self, actor: &Actor, id: ProfileId) -> Result<(), ProfileError> {
        let cache = self.cache.clone();
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let profile = get_in(tx, id)?.ok_or(ProfileError::NotFound(id))?;
            if profile.builtin {
                return Err(ProfileError::Builtin(profile.input.name));
            }
            let global: Option<String> = tx
                .query_row(
                    "SELECT profile_id FROM profile_assignments WHERE scope = 'global'",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            if global.as_deref() == Some(&id.to_string()) {
                return Err(ProfileError::InUse(profile.input.name));
            }
            let unassigned = tx.execute(
                "DELETE FROM profile_assignments WHERE profile_id = ?1",
                params![id.to_string()],
            )?;
            if tx.execute(
                "DELETE FROM profiles WHERE id = ?1",
                params![id.to_string()],
            )? == 0
            {
                return Err(ProfileError::NotFound(id));
            }
            refresh(tx, &cache)?;
            audit::record(
                tx,
                &actor,
                Action::ProfileDelete,
                Target::profile(id, &profile.input.name),
                json!({ "profile": profile.input, "assignments_removed": unassigned }),
            )?;
            Ok(())
        })
        .await
    }

    /// The assignments in force: the whole server's, and with `guild`, that guild's
    /// scopes; without, every guild's.
    pub async fn assignments(&self, guild: Option<&str>) -> Result<Vec<Assignment>, ProfileError> {
        let guild = guild.map(str::to_string);
        transact(&self.db, move |tx| {
            let mut stmt = tx.prepare(
                "SELECT scope, profile_id, updated_at FROM profile_assignments
                 WHERE ?1 IS NULL OR guild_id IS NULL OR guild_id = ?1
                 ORDER BY kind, guild_id, scope",
            )?;
            let rows = stmt.query_map(params![guild], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (scope, profile_id, updated_at) = row?;
                out.push(Assignment {
                    scope: scope.parse().map_err(|e: String| {
                        StoreError::Corrupt(format!("profile_assignments.scope: {e}"))
                    })?,
                    profile_id: profile_id.parse().map_err(|e| {
                        StoreError::Corrupt(format!("profile_assignments.profile_id: {e}"))
                    })?,
                    updated_at: timestamp("updated_at", updated_at)?,
                });
            }
            // The whole server's comes first, then guilds, channels and users.
            out.sort_by_key(|a| match a.scope {
                Scope::Global => 0,
                Scope::Guild { .. } => 1,
                Scope::Channel { .. } => 2,
                Scope::User { .. } => 3,
            });
            Ok(out)
        })
        .await
    }

    /// Puts `profile` in force at `scope`, replacing whatever was.
    pub async fn assign(
        &self,
        actor: &Actor,
        scope: Scope,
        profile: ProfileId,
    ) -> Result<Assignment, ProfileError> {
        check_scope(&scope)?;
        let cache = self.cache.clone();
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let found = get_in(tx, profile)?.ok_or(ProfileError::NotFound(profile))?;
            let previous: Option<String> = tx
                .query_row(
                    "SELECT profile_id FROM profile_assignments WHERE scope = ?1",
                    params![scope.key()],
                    |row| row.get(0),
                )
                .optional()?;
            let now = Timestamp::now();
            tx.execute(
                "INSERT INTO profile_assignments (scope, kind, guild_id, profile_id, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(scope) DO UPDATE SET profile_id = excluded.profile_id,
                    updated_at = excluded.updated_at",
                params![
                    scope.key(),
                    scope.kind(),
                    scope.guild_id(),
                    profile.to_string(),
                    nanos(now)
                ],
            )?;
            refresh(tx, &cache)?;
            if previous.as_deref() != Some(&profile.to_string()) {
                audit::record(
                    tx,
                    &actor,
                    Action::ProfileAssign,
                    Target::profile(profile, &found.input.name),
                    json!({ "scope": scope, "previous_profile_id": previous }),
                )?;
            }
            Ok(Assignment {
                scope,
                profile_id: profile,
                updated_at: now,
            })
        })
        .await
    }

    /// Takes the profile off `scope`, so the wider scope's applies there again. The
    /// whole server always has one.
    pub async fn unassign(&self, actor: &Actor, scope: Scope) -> Result<(), ProfileError> {
        if scope == Scope::Global {
            return Err(ProfileError::GlobalRequired);
        }
        check_scope(&scope)?;
        let cache = self.cache.clone();
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let previous: Option<String> = tx
                .query_row(
                    "SELECT profile_id FROM profile_assignments WHERE scope = ?1",
                    params![scope.key()],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(previous) = previous else {
                return Ok(());
            };
            tx.execute(
                "DELETE FROM profile_assignments WHERE scope = ?1",
                params![scope.key()],
            )?;
            refresh(tx, &cache)?;
            let id: ProfileId = previous
                .parse()
                .map_err(|e| StoreError::Corrupt(format!("profile_assignments.profile_id: {e}")))?;
            let name = get_in(tx, id)?.map(|p| p.input.name).unwrap_or_default();
            audit::record(
                tx,
                &actor,
                Action::ProfileUnassign,
                Target::profile(id, &name),
                json!({ "scope": scope }),
            )?;
            Ok(())
        })
        .await
    }

    /// What the profiles in force add up to for a link seen in `channel` of `guild` from
    /// `user`; the whole server's alone without a guild.
    pub fn effective(
        &self,
        guild: Option<&str>,
        channel: Option<&str>,
        user: Option<&str>,
    ) -> EffectiveProfile {
        self.cache.effective(guild, channel, user)
    }
}

fn encode(toggles: &PlatformToggles) -> Result<String, ProfileError> {
    serde_json::to_string(toggles)
        .map_err(|e| ProfileError::Store(StoreError::Corrupt(e.to_string())))
}

/// Reloads the cache from every profile and assignment.
fn refresh(conn: &Connection, cache: &ProfileCache) -> Result<usize, ProfileError> {
    let mut stmt = conn.prepare(SELECT)?;
    let profiles: HashMap<ProfileId, PlatformToggles> = stmt
        .query_map([], row_to_profile)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|p| (p.id, p.input.platforms))
        .collect();
    let mut stmt = conn.prepare("SELECT scope, profile_id, updated_at FROM profile_assignments")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut assignments = HashMap::new();
    for row in rows {
        let (scope, profile_id, updated_at) = row?;
        let profile_id: ProfileId = profile_id
            .parse()
            .map_err(|e| StoreError::Corrupt(format!("profile_assignments.profile_id: {e}")))?;
        assignments.insert(scope, (profile_id, timestamp("updated_at", updated_at)?));
    }
    let known: HashSet<ProfileId> = profiles.keys().copied().collect();
    assignments.retain(|_, (id, _)| known.contains(id));
    let count = profiles.len();
    let mut inner = cache.inner.write().unwrap_or_else(|e| e.into_inner());
    inner.profiles = profiles;
    inner.assignments = assignments;
    Ok(count)
}

fn get_in(conn: &Connection, id: ProfileId) -> Result<Option<Profile>, ProfileError> {
    Ok(conn
        .query_row(
            &format!("{SELECT} WHERE id = ?1"),
            params![id.to_string()],
            row_to_profile,
        )
        .optional()?)
}

fn row_to_profile(row: &rusqlite::Row<'_>) -> rusqlite::Result<Profile> {
    let corrupt = |message: String| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::other(message)),
        )
    };
    let id: String = row.get(0)?;
    let platforms: String = row.get(3)?;
    Ok(Profile {
        id: id
            .parse()
            .map_err(|e| corrupt(format!("profiles.id: {e}")))?,
        input: ProfileInput {
            name: row.get(1)?,
            description: row.get(2)?,
            platforms: serde_json::from_str(&platforms)
                .map_err(|e| corrupt(format!("profiles.platforms: {e}")))?,
        },
        builtin: row.get(4)?,
        created_at: timestamp("created_at", row.get(5)?).map_err(|e| corrupt(e.to_string()))?,
        updated_at: timestamp("updated_at", row.get(6)?).map_err(|e| corrupt(e.to_string()))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> ProfileStore {
        let db = SqliteStore::open_in_memory().await.unwrap();
        crate::migrations::apply(&db).await.unwrap();
        let store = ProfileStore::new(
            db,
            vec![
                ("youtube", &[Tag::Basic, Tag::Video][..]),
                ("reddit", &[Tag::Basic, Tag::Social][..]),
                ("web", &[Tag::Video][..]),
                ("redgifs", &[Tag::Nsfw, Tag::Images][..]),
            ],
        );
        store.load().await.unwrap();
        store
    }

    fn actor() -> Actor {
        Actor::Provisioning { file: None }
    }

    fn toggles(default: PlatformDefault, overrides: &[(&str, bool)]) -> PlatformToggles {
        PlatformToggles {
            default,
            presets: Vec::new(),
            overrides: overrides.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        }
    }

    #[tokio::test]
    async fn presets_whitelist_their_platforms_and_overrides_still_win() {
        let store = store().await;
        let presets = store.cache().presets().list().to_vec();
        let sfw = presets.iter().find(|p| p.id == "sfw").unwrap();
        assert_eq!(sfw.platforms, vec!["youtube", "reddit", "web"]);
        let social = presets.iter().find(|p| p.id == "social").unwrap();
        assert_eq!(social.platforms, vec!["reddit"]);
        let mut toggles = toggles(PlatformDefault::Enabled, &[("reddit", false)]);
        toggles.presets = vec!["social".into(), "nsfw".into()];
        let profile = store
            .create(&actor(), input("Social and adult", toggles))
            .await
            .unwrap();
        let alone = store.cache().alone(profile.id).unwrap();
        assert!(alone["redgifs"]);
        assert!(!alone["reddit"]);
        assert!(!alone["youtube"]);
        assert!(!alone["web"]);
        let odd = PlatformToggles {
            presets: vec!["gaming".into()],
            ..Default::default()
        };
        assert!(matches!(
            store.create(&actor(), input("Odd", odd)).await.unwrap_err(),
            ProfileError::UnknownPreset(_)
        ));
    }

    fn input(name: &str, platforms: PlatformToggles) -> ProfileInput {
        ProfileInput {
            name: name.into(),
            description: String::new(),
            platforms,
        }
    }

    #[tokio::test]
    async fn the_default_profile_turns_everything_on_everywhere() {
        let store = store().await;
        let profiles = store.list().await.unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, ProfileId::DEFAULT);
        assert!(profiles[0].builtin);
        let effective = store.effective(Some("5"), Some("1"), Some("9"));
        assert!(effective.platforms.values().all(|on| *on));
        assert_eq!(effective.applied.len(), 1);
        assert_eq!(effective.applied[0].scope, Scope::Global);
        assert!(effective.disabled().is_empty());
        assert!(matches!(
            store
                .delete(&actor(), ProfileId::DEFAULT)
                .await
                .unwrap_err(),
            ProfileError::Builtin(_)
        ));
        assert!(matches!(
            store.unassign(&actor(), Scope::Global).await.unwrap_err(),
            ProfileError::GlobalRequired
        ));
    }

    #[tokio::test]
    async fn scopes_apply_from_the_widest_to_the_narrowest() {
        let store = store().await;
        let no_youtube = store
            .create(
                &actor(),
                input(
                    "No YouTube",
                    toggles(PlatformDefault::Inherit, &[("youtube", false)]),
                ),
            )
            .await
            .unwrap();
        let reddit_only = store
            .create(
                &actor(),
                input(
                    "Reddit only",
                    toggles(PlatformDefault::Disabled, &[("reddit", true)]),
                ),
            )
            .await
            .unwrap();
        let everything = store
            .create(
                &actor(),
                input("Everything", toggles(PlatformDefault::Enabled, &[])),
            )
            .await
            .unwrap();
        store
            .assign(
                &actor(),
                Scope::Guild {
                    guild_id: "5".into(),
                },
                no_youtube.id,
            )
            .await
            .unwrap();
        store
            .assign(
                &actor(),
                Scope::Channel {
                    guild_id: "5".into(),
                    channel_id: "1".into(),
                },
                reddit_only.id,
            )
            .await
            .unwrap();
        store
            .assign(
                &actor(),
                Scope::User {
                    guild_id: "5".into(),
                    user_id: "9".into(),
                },
                everything.id,
            )
            .await
            .unwrap();

        let guild = store.effective(Some("5"), Some("2"), Some("8"));
        assert_eq!(guild.disabled(), vec!["youtube".to_string()]);
        assert_eq!(guild.applied.len(), 2);
        let channel = store.effective(Some("5"), Some("1"), Some("8"));
        assert_eq!(
            channel.disabled(),
            vec![
                "redgifs".to_string(),
                "web".to_string(),
                "youtube".to_string()
            ]
        );
        let user = store.effective(Some("5"), Some("1"), Some("9"));
        assert!(user.disabled().is_empty());
        assert_eq!(user.applied.len(), 4);
        let elsewhere = store.effective(Some("6"), Some("1"), Some("9"));
        assert!(elsewhere.disabled().is_empty());
        assert_eq!(elsewhere.applied.len(), 1);
        let web = store.effective(None, None, None);
        assert!(web.disabled().is_empty());

        // The bot asks by ids and gets the same answer.
        let cache = store.cache();
        assert_eq!(
            cache.disabled_platforms(Some(Id::new(5)), Some(Id::new(1)), Some(Id::new(8))),
            vec![
                "redgifs".to_string(),
                "web".to_string(),
                "youtube".to_string()
            ]
        );

        let assignments = store.assignments(Some("5")).await.unwrap();
        assert_eq!(assignments.len(), 4);
        assert_eq!(assignments[0].scope, Scope::Global);
        assert_eq!(store.assignments(Some("6")).await.unwrap().len(), 1);
        assert_eq!(store.assignments(None).await.unwrap().len(), 4);

        // Deleting a profile takes it off every scope it was on.
        store.delete(&actor(), reddit_only.id).await.unwrap();
        let channel = store.effective(Some("5"), Some("1"), Some("8"));
        assert_eq!(channel.disabled(), vec!["youtube".to_string()]);
        store
            .unassign(
                &actor(),
                Scope::Guild {
                    guild_id: "5".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            store
                .effective(Some("5"), Some("2"), Some("8"))
                .disabled()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn the_global_profile_can_be_replaced_but_not_removed() {
        let store = store().await;
        let quiet = store
            .create(
                &actor(),
                input("Quiet", toggles(PlatformDefault::Disabled, &[])),
            )
            .await
            .unwrap();
        store
            .assign(&actor(), Scope::Global, quiet.id)
            .await
            .unwrap();
        assert_eq!(store.effective(None, None, None).disabled().len(), 4);
        assert!(matches!(
            store.delete(&actor(), quiet.id).await.unwrap_err(),
            ProfileError::InUse(_)
        ));
        store
            .assign(&actor(), Scope::Global, ProfileId::DEFAULT)
            .await
            .unwrap();
        store.delete(&actor(), quiet.id).await.unwrap();
        assert!(store.effective(None, None, None).disabled().is_empty());
    }

    #[tokio::test]
    async fn inputs_are_checked() {
        let store = store().await;
        assert!(matches!(
            store
                .create(&actor(), input("  ", PlatformToggles::default()))
                .await
                .unwrap_err(),
            ProfileError::Invalid(_)
        ));
        assert!(matches!(
            store
                .create(
                    &actor(),
                    input(
                        "Odd",
                        toggles(PlatformDefault::Inherit, &[("myspace", true)])
                    )
                )
                .await
                .unwrap_err(),
            ProfileError::UnknownPlatform(_)
        ));
        store
            .create(&actor(), input("Twice", PlatformToggles::default()))
            .await
            .unwrap();
        assert!(matches!(
            store
                .create(&actor(), input("twice", PlatformToggles::default()))
                .await
                .unwrap_err(),
            ProfileError::Duplicate(_)
        ));
        assert!(matches!(
            store
                .assign(
                    &actor(),
                    Scope::Guild {
                        guild_id: "x".into()
                    },
                    ProfileId::DEFAULT
                )
                .await
                .unwrap_err(),
            ProfileError::Invalid(_)
        ));
        assert!(matches!(
            store
                .assign(&actor(), Scope::Global, ProfileId(Uuid::from_u128(77)))
                .await
                .unwrap_err(),
            ProfileError::NotFound(_)
        ));
        assert_eq!("channel:5:1".parse::<Scope>().unwrap().key(), "channel:5:1");
        assert!("channel:5".parse::<Scope>().is_err());
    }
}
