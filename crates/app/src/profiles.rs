//! Profiles define platform access and media limits. Assignments apply in order: global,
//! guild, channel, user. Each profile overrides only specified values.
//!
//! The built-in Default profile enables all platforms and initially serves as the global
//! default. Bots and web routes read a shared cache. Changes are audited transactionally.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, RwLock};

use discoclip_bot::{InForce, ProfileSource};
use discoclip_engine::StoreError;
use discoclip_engine::job::RequestLimits;
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
    /// The built-in profile every platform is on in. Assigned to the whole server at first.
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
    /// Left as the parent scope has them.
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
    /// what the wider one allowed. `presets` maps preset ids to their platforms.
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

/// How big, long and tall a video may be under a profile. Each limit a profile names
/// replaces the parent scope's. One it leaves unset stays as the parent scope has it, and
/// the engine's own limits cap them all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileLimits {
    pub max_source_bytes: Option<u64>,
    pub max_duration_secs: Option<u64>,
    pub max_height: Option<u32>,
}

impl ProfileLimits {
    /// `limits` with every limit this profile names replaced.
    fn apply(&self, limits: &mut RequestLimits) {
        if self.max_source_bytes.is_some() {
            limits.max_source_bytes = self.max_source_bytes;
        }
        if self.max_duration_secs.is_some() {
            limits.max_duration_secs = self.max_duration_secs;
        }
        if self.max_height.is_some() {
            limits.max_height = self.max_height;
        }
    }
}

/// A platform as the profiles need it: its id, the kinds it is tagged with, and the
/// hosts its links come from.
#[derive(Debug, Clone, Copy)]
pub struct PlatformFacts {
    pub id: &'static str,
    pub tags: &'static [Tag],
    pub hosts: &'static [&'static str],
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
        "Commonly shared media platforms.",
        Membership::Tagged(Tag::Basic),
    ),
    (
        "sfw",
        "Safe for work",
        "Platforms without the adult content tag.",
        Membership::Without(Tag::Nsfw),
    ),
    (
        "nsfw",
        "Adult",
        "Platforms with adult content.",
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
        "Social networks and forums.",
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
        "Live streams and recordings.",
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
    pub fn from_platforms(platforms: &[PlatformFacts]) -> Self {
        let list: Vec<Preset> = PRESET_RULES
            .iter()
            .map(|(id, label, description, rule)| Preset {
                id,
                label,
                description,
                platforms: platforms
                    .iter()
                    .filter(|p| match rule {
                        Membership::Tagged(tag) => p.tags.contains(tag),
                        Membership::Without(tag) => !p.tags.contains(tag),
                    })
                    .map(|p| p.id.to_string())
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
    #[serde(default)]
    pub limits: ProfileLimits,
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

/// What the profiles assigned at a place add up to: every platform, on or off, the
/// limits named along the way, and the assignments that were applied to get there,
/// widest first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EffectiveProfile {
    pub platforms: BTreeMap<String, bool>,
    /// The limits assigned, each the narrowest profile that names it. Unset ones leave
    /// the engine's own.
    pub limits: RequestLimits,
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

    /// The same, as the bots hand it to the engine.
    pub fn in_force(&self) -> InForce {
        InForce {
            disabled: self.disabled(),
            limits: self.limits,
        }
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
    #[error("{0} is the global default. Assign another profile before deleting it.")]
    InUse(String),
    #[error("no platform is called {0}")]
    UnknownPlatform(String),
    #[error("no preset is called {0}")]
    UnknownPreset(String),
    #[error("Assign another global profile before removing this assignment.")]
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
    if input.limits.max_source_bytes == Some(0) {
        return Err(ProfileError::Invalid("max_source_bytes must be above zero".into()));
    }
    if input.limits.max_height == Some(0) {
        return Err(ProfileError::Invalid("max_height must be above zero".into()));
    }
    Ok(ProfileInput {
        name,
        description,
        platforms: input.platforms.clone(),
        limits: input.limits,
    })
}

#[derive(Default)]
struct CacheInner {
    profiles: HashMap<ProfileId, (PlatformToggles, ProfileLimits)>,
    assignments: HashMap<String, (ProfileId, Timestamp)>,
}

/// The profiles and assignments as the bots and the web app read them, with the
/// platforms the engine has so a profile's default can be spelled out.
#[derive(Clone)]
pub struct ProfileCache {
    facts: Arc<Vec<PlatformFacts>>,
    platforms: Arc<Vec<&'static str>>,
    presets: Arc<Presets>,
    inner: Arc<RwLock<CacheInner>>,
}

impl ProfileCache {
    fn new(platforms: Vec<PlatformFacts>) -> Self {
        Self {
            presets: Arc::new(Presets::from_platforms(&platforms)),
            platforms: Arc::new(platforms.iter().map(|p| p.id).collect()),
            facts: Arc::new(platforms),
            inner: Arc::new(RwLock::new(CacheInner::default())),
        }
    }

    /// The resolver ids the cache knows.
    pub fn platforms(&self) -> &[&'static str] {
        &self.platforms
    }

    /// The platforms whose links come from `host`: those with a host it is, or is under,
    /// or that are under it, as a rule's host allowlist named them.
    pub fn platforms_for_host(&self, host: &str) -> Vec<&'static str> {
        let host = host.trim().trim_start_matches("*.").to_ascii_lowercase();
        if host.is_empty() {
            return Vec::new();
        }
        self.facts
            .iter()
            .filter(|p| {
                p.hosts.iter().any(|h| {
                    let h = h.to_ascii_lowercase();
                    h == host
                        || host.ends_with(&format!(".{h}"))
                        || h.ends_with(&format!(".{host}"))
                })
            })
            .map(|p| p.id)
            .collect()
    }

    /// The presets the platforms make.
    pub fn presets(&self) -> &Presets {
        &self.presets
    }

    /// Effective settings for one profile: every platform on, then the profile
    /// applied. `None` when no such profile exists.
    pub fn alone(&self, profile: ProfileId) -> Option<BTreeMap<String, bool>> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let (toggles, _) = inner.profiles.get(&profile)?;
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
        let mut limits = RequestLimits::default();
        let mut applied = Vec::new();
        for scope in scopes {
            let Some((profile_id, updated_at)) = inner.assignments.get(&scope.key()) else {
                continue;
            };
            let Some((toggles, named)) = inner.profiles.get(profile_id) else {
                continue;
            };
            toggles.apply(&mut platforms, &self.presets);
            named.apply(&mut limits);
            applied.push(Assignment {
                scope: scope.clone(),
                profile_id: *profile_id,
                updated_at: *updated_at,
            });
        }
        EffectiveProfile {
            platforms,
            limits,
            applied,
        }
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
    fn in_force(
        &self,
        guild: Option<Id<GuildMarker>>,
        channel: Option<Id<ChannelMarker>>,
        user: Option<Id<UserMarker>>,
    ) -> InForce {
        let guild = guild.map(|id| id.to_string());
        let channel = channel.map(|id| id.to_string());
        let user = user.map(|id| id.to_string());
        self.effective(guild.as_deref(), channel.as_deref(), user.as_deref())
            .in_force()
    }
}

#[derive(Clone)]
pub struct ProfileStore {
    db: SqliteStore,
    cache: ProfileCache,
}

const SELECT: &str = "SELECT id, name, description, platforms, builtin, created_at, updated_at, \
     max_source_bytes, max_duration_secs, max_height FROM profiles";

impl ProfileStore {
    /// Over a database the application's migrations have been applied to, for the
    /// engine's `platforms` with their tags. `load` fills the cache before anything
    /// reads it.
    pub fn new(db: SqliteStore, platforms: Vec<PlatformFacts>) -> Self {
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

    /// Fills the cache from the database, after turning the policies watch rules used to
    /// carry, their host allowlists and limits, into profiles assigned for their channels.
    pub async fn load(&self) -> Result<usize, ProfileError> {
        let cache = self.cache.clone();
        transact(&self.db, move |tx| {
            refresh(tx, &cache)?;
            let converted = convert_rule_policies(tx, &cache)?;
            if converted > 0 {
                tracing::info!(profiles = converted, "watch rule policies turned into channel profiles");
                refresh(tx, &cache)?;
            }
            refresh(tx, &cache)
        })
        .await
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
            let inserted = insert(tx, id, &input, now);
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
                "UPDATE profiles SET name = ?2, description = ?3, platforms = ?4, updated_at = ?5,
                    max_source_bytes = ?6, max_duration_secs = ?7, max_height = ?8
                 WHERE id = ?1",
                params![
                    id.to_string(),
                    input.name,
                    input.description,
                    encode(&input.platforms)?,
                    nanos(now),
                    input.limits.max_source_bytes.map(|n| n as i64),
                    input.limits.max_duration_secs.map(|n| n as i64),
                    input.limits.max_height.map(i64::from),
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

    /// Removes a profile and every assignment of it. The built-in profile and the one
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

    /// The assignments assigned: the whole server's, and with `guild`, that guild's
    /// scopes. Without, every guild's.
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

    /// Puts `profile` assigned at `scope`, replacing whatever was.
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

    /// Takes the profile off `scope`, so the parent scope's applies there again. The
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

    /// What the profiles assigned add up to for a link seen in `channel` of `guild` from
    /// `user`. The whole server's alone without a guild.
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

/// Inserts a checked profile as `id`, created and updated `now`.
fn insert(conn: &Connection, id: ProfileId, input: &ProfileInput, now: Timestamp) -> rusqlite::Result<usize> {
    let platforms = match encode(&input.platforms) {
        Ok(text) => text,
        Err(error) => {
            return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(
                std::io::Error::other(error.to_string()),
            )));
        }
    };
    conn.execute(
        "INSERT INTO profiles (id, name, description, platforms, builtin, created_at, updated_at,
            max_source_bytes, max_duration_secs, max_height)
         VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5, ?6, ?7, ?8)",
        params![
            id.to_string(),
            input.name,
            input.description,
            platforms,
            nanos(now),
            input.limits.max_source_bytes.map(|n| n as i64),
            input.limits.max_duration_secs.map(|n| n as i64),
            input.limits.max_height.map(i64::from),
        ],
    )
}

/// One policy a watch rule carried before profiles took them over, staged by the
/// migration that dropped the columns.
struct RulePolicy {
    rule_id: String,
    application_id: String,
    guild_id: String,
    channel_id: String,
    hosts: Vec<String>,
    limits: ProfileLimits,
}

/// Migrate staged watch-rule policies into channel profiles. Convert allowed hosts to
/// enabled platforms and preserve limits. Unmatched hosts use the web resolver.
///
/// Existing channel restrictions remain. Audit each conversion, delete its staged row and
/// return the number of created profiles.
fn convert_rule_policies(conn: &Connection, cache: &ProfileCache) -> Result<usize, ProfileError> {
    let staged: bool = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'watch_rule_policies'",
        [],
        |row| row.get::<_, i64>(0).map(|n| n > 0),
    )?;
    if !staged {
        return Ok(0);
    }
    let mut stmt = conn.prepare(
        "SELECT rule_id, application_id, guild_id, channel_id, allow_hosts, max_source_bytes,
                max_duration_secs, max_height
         FROM watch_rule_policies ORDER BY guild_id, channel_id, rule_id",
    )?;
    let policies = stmt
        .query_map([], |row| {
            let hosts: String = row.get(4)?;
            let max_source_bytes: Option<i64> = row.get(5)?;
            let max_duration_secs: Option<i64> = row.get(6)?;
            let max_height: Option<i64> = row.get(7)?;
            Ok((
                RulePolicy {
                    rule_id: row.get(0)?,
                    application_id: row.get(1)?,
                    guild_id: row.get(2)?,
                    channel_id: row.get(3)?,
                    hosts: Vec::new(),
                    limits: ProfileLimits {
                        max_source_bytes: max_source_bytes.map(|n| n.max(1) as u64),
                        max_duration_secs: max_duration_secs.map(|n| n.max(0) as u64),
                        max_height: max_height.map(|n| u32::try_from(n.max(1)).unwrap_or(u32::MAX)),
                    },
                },
                hosts,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    let actor = Actor::Provisioning { file: None };
    let mut converted = 0;
    for (mut policy, hosts) in policies {
        policy.hosts = serde_json::from_str(&hosts)
            .map_err(|e| StoreError::Corrupt(format!("watch_rule_policies.allow_hosts: {e}")))?;
        let scope = Scope::Channel {
            guild_id: policy.guild_id.clone(),
            channel_id: policy.channel_id.clone(),
        };
        let existing: Option<String> = conn
            .query_row(
                "SELECT profile_id FROM profile_assignments WHERE scope = ?1",
                params![scope.key()],
                |row| row.get(0),
            )
            .optional()?;
        let existing: Option<ProfileId> = existing
            .map(|id| id.parse())
            .transpose()
            .map_err(|e| StoreError::Corrupt(format!("profile_assignments.profile_id: {e}")))?;
        let mut unmatched: Vec<String> = Vec::new();
        let mut allowed: HashSet<&'static str> = HashSet::new();
        for host in &policy.hosts {
            let takers = cache.platforms_for_host(host);
            if takers.is_empty() {
                unmatched.push(host.clone());
                if cache.platforms.contains(&"web") {
                    allowed.insert("web");
                }
            }
            allowed.extend(takers);
        }
        let platforms = match (policy.hosts.is_empty(), existing) {
            (true, None) => PlatformToggles::default(),
            (true, Some(previous)) => {
                cache
                    .inner
                    .read()
                    .unwrap_or_else(|e| e.into_inner())
                    .profiles
                    .get(&previous)
                    .map(|(toggles, _)| toggles.clone())
                    .unwrap_or_default()
            }
            (false, None) => PlatformToggles {
                default: PlatformDefault::Disabled,
                presets: Vec::new(),
                overrides: allowed.iter().map(|id| (id.to_string(), true)).collect(),
            },
            (false, Some(previous)) => {
                let before = cache.alone(previous).unwrap_or_default();
                PlatformToggles {
                    default: PlatformDefault::Disabled,
                    presets: Vec::new(),
                    overrides: cache
                        .platforms
                        .iter()
                        .map(|id| {
                            let on = allowed.contains(id) && before.get(*id).copied().unwrap_or(true);
                            (id.to_string(), on)
                        })
                        .collect(),
                }
            }
        };
        let mut limits = policy.limits;
        if let Some(previous) = existing
            && let Some((_, named)) = cache
                .inner
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .profiles
                .get(&previous)
        {
            let mut merged = RequestLimits::default();
            named.apply(&mut merged);
            policy.limits.apply(&mut merged);
            limits = ProfileLimits {
                max_source_bytes: merged.max_source_bytes,
                max_duration_secs: merged.max_duration_secs,
                max_height: merged.max_height,
            };
        }
        let base_name = format!("Channel {} rule", policy.channel_id);
        let description = format!(
            "What the watch rule for channel {} in guild {} used to say about platforms and limits.",
            policy.channel_id, policy.guild_id
        );
        let now = Timestamp::now();
        let id = ProfileId(Uuid::now_v7());
        let mut input = ProfileInput {
            name: base_name.clone(),
            description,
            platforms,
            limits,
        };
        let mut attempt = 0;
        loop {
            match insert(conn, id, &input, now) {
                Ok(_) => break,
                Err(rusqlite::Error::SqliteFailure(error, _))
                    if error.code == rusqlite::ErrorCode::ConstraintViolation && attempt < 100 =>
                {
                    attempt += 1;
                    input.name = format!("{base_name} ({attempt})");
                }
                Err(error) => return Err(error.into()),
            }
        }
        audit::record(
            conn,
            &actor,
            Action::ProfileCreate,
            Target::profile(id, &input.name),
            json!({
                "profile": input,
                "converted_from_rule": {
                    "rule_id": policy.rule_id,
                    "application_id": policy.application_id,
                    "guild_id": policy.guild_id,
                    "channel_id": policy.channel_id,
                    "allow_hosts": policy.hosts,
                    "unmatched_hosts": unmatched,
                    "max_source_bytes": policy.limits.max_source_bytes,
                    "max_duration_secs": policy.limits.max_duration_secs,
                    "max_height": policy.limits.max_height,
                },
            }),
        )?;
        conn.execute(
            "INSERT INTO profile_assignments (scope, kind, guild_id, profile_id, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(scope) DO UPDATE SET profile_id = excluded.profile_id,
                updated_at = excluded.updated_at",
            params![scope.key(), scope.kind(), scope.guild_id(), id.to_string(), nanos(now)],
        )?;
        audit::record(
            conn,
            &actor,
            Action::ProfileAssign,
            Target::profile(id, &input.name),
            json!({ "scope": scope, "previous_profile_id": existing }),
        )?;
        conn.execute(
            "DELETE FROM watch_rule_policies WHERE rule_id = ?1",
            params![policy.rule_id],
        )?;
        refresh(conn, cache)?;
        converted += 1;
    }
    Ok(converted)
}

/// Reloads the cache from every profile and assignment.
fn refresh(conn: &Connection, cache: &ProfileCache) -> Result<usize, ProfileError> {
    let mut stmt = conn.prepare(SELECT)?;
    let profiles: HashMap<ProfileId, (PlatformToggles, ProfileLimits)> = stmt
        .query_map([], row_to_profile)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|p| (p.id, (p.input.platforms, p.input.limits)))
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
    let max_source_bytes: Option<i64> = row.get(7)?;
    let max_duration_secs: Option<i64> = row.get(8)?;
    let max_height: Option<i64> = row.get(9)?;
    Ok(Profile {
        id: id
            .parse()
            .map_err(|e| corrupt(format!("profiles.id: {e}")))?,
        input: ProfileInput {
            name: row.get(1)?,
            description: row.get(2)?,
            platforms: serde_json::from_str(&platforms)
                .map_err(|e| corrupt(format!("profiles.platforms: {e}")))?,
            limits: ProfileLimits {
                max_source_bytes: max_source_bytes.map(|n| n.max(0) as u64),
                max_duration_secs: max_duration_secs.map(|n| n.max(0) as u64),
                max_height: max_height.map(|n| u32::try_from(n.max(0)).unwrap_or(u32::MAX)),
            },
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
        let store = ProfileStore::new(db, test_platforms());
        store.load().await.unwrap();
        store
    }

    fn test_platforms() -> Vec<PlatformFacts> {
        vec![
            PlatformFacts {
                id: "youtube",
                tags: &[Tag::Basic, Tag::Video],
                hosts: &["youtube.com", "youtu.be"],
            },
            PlatformFacts {
                id: "reddit",
                tags: &[Tag::Basic, Tag::Social],
                hosts: &["reddit.com", "redd.it"],
            },
            PlatformFacts {
                id: "web",
                tags: &[Tag::Video],
                hosts: &[],
            },
            PlatformFacts {
                id: "redgifs",
                tags: &[Tag::Nsfw, Tag::Images],
                hosts: &["redgifs.com"],
            },
        ]
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
            limits: ProfileLimits::default(),
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
            cache.in_force(Some(Id::new(5)), Some(Id::new(1)), Some(Id::new(8))).disabled,
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
        assert!(matches!(
            store
                .create(
                    &actor(),
                    ProfileInput {
                        limits: ProfileLimits {
                            max_source_bytes: Some(0),
                            ..ProfileLimits::default()
                        },
                        ..input("Zero", PlatformToggles::default())
                    }
                )
                .await
                .unwrap_err(),
            ProfileError::Invalid(_)
        ));
    }

    #[tokio::test]
    async fn limits_come_from_the_narrowest_profile_that_names_them() {
        let store = store().await;
        let guild_wide = store
            .create(
                &actor(),
                ProfileInput {
                    limits: ProfileLimits {
                        max_source_bytes: Some(50_000_000),
                        max_duration_secs: Some(600),
                        max_height: None,
                    },
                    ..input("Guild limits", PlatformToggles::default())
                },
            )
            .await
            .unwrap();
        let channel_wide = store
            .create(
                &actor(),
                ProfileInput {
                    limits: ProfileLimits {
                        max_source_bytes: None,
                        max_duration_secs: Some(60),
                        max_height: Some(480),
                    },
                    ..input("Channel limits", PlatformToggles::default())
                },
            )
            .await
            .unwrap();
        assert_eq!(store.get(guild_wide.id).await.unwrap().unwrap().input.limits.max_duration_secs, Some(600));
        store
            .assign(&actor(), Scope::Guild { guild_id: "5".into() }, guild_wide.id)
            .await
            .unwrap();
        store
            .assign(
                &actor(),
                Scope::Channel {
                    guild_id: "5".into(),
                    channel_id: "1".into(),
                },
                channel_wide.id,
            )
            .await
            .unwrap();
        let nothing = store.effective(None, None, None).limits;
        assert_eq!(nothing, RequestLimits::default());
        let guild = store.effective(Some("5"), Some("2"), None).limits;
        assert_eq!(guild.max_source_bytes, Some(50_000_000));
        assert_eq!(guild.max_duration_secs, Some(600));
        assert_eq!(guild.max_height, None);
        let channel = store.cache().in_force(Some(Id::new(5)), Some(Id::new(1)), None).limits;
        assert_eq!(channel.max_source_bytes, Some(50_000_000));
        assert_eq!(channel.max_duration_secs, Some(60));
        assert_eq!(channel.max_height, Some(480));
        let updated = store
            .update(
                &actor(),
                channel_wide.id,
                ProfileInput {
                    limits: ProfileLimits::default(),
                    ..channel_wide.input.clone()
                },
            )
            .await
            .unwrap();
        assert_eq!(updated.input.limits, ProfileLimits::default());
        let channel = store.effective(Some("5"), Some("1"), None).limits;
        assert_eq!(channel.max_duration_secs, Some(600));
    }

    /// The columns watch rules carried before profiles took them over, as an older
    /// database has them: the migration stages them and the store converts them.
    #[tokio::test]
    async fn rule_policies_become_channel_profiles() {
        use discoclip_engine::store::migrate::Migration;

        let db = SqliteStore::open_in_memory().await.unwrap();
        // Up to the schema before the columns were dropped, with rules in it.
        let before: &'static [Migration] = Box::leak(
            crate::migrations::MIGRATIONS
                .iter()
                .copied()
                .take_while(|m| m.name != "profile_limits")
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );
        db.migrate(crate::migrations::SCOPE, before).await.unwrap();
        db.call(|conn| {
            Ok(conn.execute_batch(
                "INSERT INTO discord_applications (id, name, client_id, client_secret, bot_token, created_at, updated_at)
                 VALUES ('0193b000-0000-7000-8000-000000000001', 'A', '1', NULL, 't', 0, 0);
                 INSERT INTO watch_rules (id, application_id, guild_id, channel_id, post_to, allow_hosts, allow_users,
                     allow_roles, max_source_bytes, max_duration_secs, max_height, enabled, created_at, updated_at)
                 VALUES
                 ('r1', '0193b000-0000-7000-8000-000000000001', '5', '1', NULL,
                     '[\"youtube.com\", \"v.redd.it\", \"nobody.example\"]', '[]', '[]', 1000, 30, 720, 1, 0, 0),
                 ('r2', '0193b000-0000-7000-8000-000000000001', '5', '2', NULL, '[]', '[\"9\"]', '[]', NULL, NULL, NULL, 1, 0, 0),
                 ('r3', '0193b000-0000-7000-8000-000000000001', '5', '3', NULL, '[]', '[]', '[]', NULL, 90, NULL, 1, 0, 0);",
            )?)
        })
        .await
        .unwrap();
        crate::migrations::apply(&db).await.unwrap();
        let store = ProfileStore::new(db.clone(), test_platforms());
        store.load().await.unwrap();

        let profiles = store.list().await.unwrap();
        let names: Vec<&str> = profiles.iter().map(|p| p.input.name.as_str()).collect();
        assert_eq!(names, vec!["Default", "Channel 1 rule", "Channel 3 rule"]);
        let one = profiles.iter().find(|p| p.input.name == "Channel 1 rule").unwrap();
        assert_eq!(one.input.limits.max_source_bytes, Some(1000));
        assert_eq!(one.input.limits.max_duration_secs, Some(30));
        assert_eq!(one.input.limits.max_height, Some(720));
        assert_eq!(one.input.platforms.default, PlatformDefault::Disabled);
        assert!(one.input.platforms.presets.is_empty());
        let on: Vec<&str> = one
            .input
            .platforms
            .overrides
            .iter()
            .filter(|(_, on)| **on)
            .map(|(id, _)| id.as_str())
            .collect();
        assert_eq!(on, vec!["reddit", "web", "youtube"]);
        let channel = store.effective(Some("5"), Some("1"), None);
        assert_eq!(channel.disabled(), vec!["redgifs".to_string()]);
        assert_eq!(channel.limits.max_height, Some(720));
        assert_eq!(channel.applied.len(), 2);
        // A rule with only who-may-post left nothing to convert.
        assert!(store.effective(Some("5"), Some("2"), None).applied.len() == 1);
        let three = store.effective(Some("5"), Some("3"), None);
        assert!(three.disabled().is_empty());
        assert_eq!(three.limits.max_duration_secs, Some(90));
        // The staged rows are gone, so loading again converts nothing more.
        store.load().await.unwrap();
        assert_eq!(store.list().await.unwrap().len(), 3);
        let staged: i64 = db
            .call(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM watch_rule_policies", [], |row| row.get(0))?))
            .await
            .unwrap();
        assert_eq!(staged, 0);
        // The log says where each came from.
        let entries = crate::audit::AuditStore::new(db.clone())
            .list(&crate::audit::Filter::default())
            .await
            .unwrap();
        let created: Vec<&crate::audit::Entry> = entries
            .entries
            .iter()
            .filter(|e| e.action == Action::ProfileCreate)
            .collect();
        assert_eq!(created.len(), 2);
        let from = created
            .iter()
            .find(|e| e.details["converted_from_rule"]["channel_id"] == "1")
            .unwrap();
        assert_eq!(from.details["converted_from_rule"]["rule_id"], "r1");
        assert_eq!(from.details["converted_from_rule"]["unmatched_hosts"], json!(["nobody.example"]));
        assert!(entries.entries.iter().any(|e| e.action == Action::ProfileAssign));
    }

    #[test]
    fn hosts_name_their_platforms_either_way_round() {
        let cache = ProfileCache::new(test_platforms());
        assert_eq!(cache.platforms_for_host("youtube.com"), vec!["youtube"]);
        assert_eq!(cache.platforms_for_host("www.youtube.com"), vec!["youtube"]);
        assert_eq!(cache.platforms_for_host("*.redd.it"), vec!["reddit"]);
        assert_eq!(cache.platforms_for_host("v.redd.it"), vec!["reddit"]);
        assert_eq!(cache.platforms_for_host("redd.it"), vec!["reddit"]);
        assert!(cache.platforms_for_host("notreddit.com").is_empty());
        assert!(cache.platforms_for_host("").is_empty());
    }
}
