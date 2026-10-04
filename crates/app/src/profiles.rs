//! Profiles are the policy: platform access, limits, output, upload size, delivery, how
//! the bot answers, error detail and duplicate handling. The built-in Default names every
//! value. Other profiles name only what they change, and assignments apply them in order:
//! global, guild, channel, user.
//!
//! Bots and web routes read a shared cache. Changes are audited transactionally.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, RwLock};

use discoclip_bot::{InForce, ProfileSource};
use discoclip_engine::StoreError;
use discoclip_engine::policy::{DeliveryMode, OverLimit, UnderFloor, UploadLimit, View};
use discoclip_engine::publish::{DestinationTarget, TargetOverride};
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
pub use crate::policy::{
    DedupeOverlay, DeliveryOverlay, EffectivePolicy, ErrorsOverlay, FloorOverlay, IncludeOverlay,
    IntakeOverlay, Known, MessageOverlay, PlaylistsOverlay, ProfileLimits, Sections, SectionsPatch,
    UploadOverlay, check_sections,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProfileId(pub Uuid);

impl ProfileId {
    /// The built-in profile that names every value, in force everywhere underneath the rest
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
    /// On, except platforms off by default, which stay as the parent scope has them
    Enabled,
    Disabled,
}

/// Which platforms a profile turns on and off. With `presets` chosen, they are the
/// whitelist: every platform in any chosen preset is on and every other off, which is
/// what the presets add up to. Without any, `default` says what happens to the
/// platforms `overrides` do not name. Overrides win either way. A platform off by
/// default is on only where a chosen preset holds it or an override names it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PlatformToggles {
    pub default: PlatformDefault,
    pub presets: Vec<String>,
    pub overrides: BTreeMap<String, bool>,
}

impl PlatformToggles {
    /// Applies the profile on top of `platforms`, with `baseline` saying which are on by default
    fn apply(
        &self,
        platforms: &mut BTreeMap<String, bool>,
        presets: &Presets,
        baseline: &BTreeMap<String, bool>,
    ) {
        if self.presets.is_empty() {
            match self.default {
                PlatformDefault::Inherit => {}
                PlatformDefault::Enabled => {
                    for (platform, on_by_default) in baseline {
                        if *on_by_default && let Some(on) = platforms.get_mut(platform) {
                            *on = true;
                        }
                    }
                }
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

/// A platform as the profiles need it, with whether it is on before any profile names it
#[derive(Debug, Clone, Copy)]
pub struct PlatformFacts {
    pub id: &'static str,
    pub tags: &'static [Tag],
    pub hosts: &'static [&'static str],
    /// Off for platforms whose links flood a chat, GIF hosts above all
    pub on_by_default: bool,
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

/// What a profile says, as edited. Every section leaf left out inherits the wider scope's
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileInput {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub platforms: PlatformToggles,
    /// The language of the sound taken when a source offers several, as a language
    /// tag. Unset leaves the parent scope's.
    #[serde(default)]
    pub audio_language: Option<String>,
    #[serde(default)]
    pub limits: ProfileLimits,
    #[serde(default)]
    pub intake: IntakeOverlay,
    #[serde(default)]
    pub output: TargetOverride,
    #[serde(default)]
    pub upload: UploadOverlay,
    #[serde(default)]
    pub delivery: DeliveryOverlay,
    #[serde(default)]
    pub message: MessageOverlay,
    #[serde(default)]
    pub errors: ErrorsOverlay,
    #[serde(default)]
    pub dedupe: DedupeOverlay,
}

impl ProfileInput {
    /// A profile that names nothing but its name
    pub fn named(name: &str) -> Self {
        Self {
            name: name.to_string(),
            description: String::new(),
            platforms: PlatformToggles::default(),
            audio_language: None,
            limits: ProfileLimits::default(),
            intake: IntakeOverlay::default(),
            output: TargetOverride::default(),
            upload: UploadOverlay::default(),
            delivery: DeliveryOverlay::default(),
            message: MessageOverlay::default(),
            errors: ErrorsOverlay::default(),
            dedupe: DedupeOverlay::default(),
        }
    }

    pub fn sections(&self) -> Sections {
        Sections {
            limits: self.limits,
            intake: self.intake.clone(),
            output: self.output.clone(),
            upload: self.upload,
            delivery: self.delivery.clone(),
            message: self.message.clone(),
            errors: self.errors,
            dedupe: self.dedupe,
        }
    }

    pub fn set_sections(&mut self, sections: Sections) {
        self.limits = sections.limits;
        self.intake = sections.intake;
        self.output = sections.output;
        self.upload = sections.upload;
        self.delivery = sections.delivery;
        self.message = sections.message;
        self.errors = sections.errors;
        self.dedupe = sections.dedupe;
    }
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

    /// What a profile made for this scope alone is called
    fn label(&self) -> String {
        match self {
            Scope::Global => "Server options".to_string(),
            Scope::Guild { guild_id } => format!("Server {guild_id} options"),
            Scope::Channel { channel_id, .. } => format!("Channel {channel_id} options"),
            Scope::User { user_id, .. } => format!("Member {user_id} options"),
        }
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
    #[error("no content view has the id {0}")]
    UnknownView(String),
    #[error("content view {0} is turned off")]
    ViewDisabled(String),
    #[error(
        "posting links needs the app's public address: open the app at the address people reach it by, or set web.public_url"
    )]
    NoPublicUrl,
    #[error("posting links needs a content view: choose one under Delivery")]
    NoView,
    #[error("the built-in profile has to name every value, and leaves out {0}")]
    Incomplete(String),
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
    platforms: &[&'static str],
    presets: &Presets,
    known: &Known,
    builtin: bool,
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
        if !platforms.contains(&platform.as_str()) {
            return Err(ProfileError::UnknownPlatform(platform.clone()));
        }
    }
    for preset in &input.platforms.presets {
        if !presets.has(preset) {
            return Err(ProfileError::UnknownPreset(preset.clone()));
        }
    }
    check_sections(&input.sections(), known, builtin)?;
    let audio_language = match input.audio_language.as_deref().map(str::trim) {
        None | Some("") => None,
        Some(tag) => {
            let tag = tag.to_ascii_lowercase();
            let well_formed = tag.split('-').enumerate().all(|(n, part)| {
                let length = if n == 0 { 2..=3 } else { 2..=8 };
                length.contains(&part.len()) && part.chars().all(|c| c.is_ascii_alphanumeric())
            });
            if !well_formed {
                return Err(ProfileError::Invalid(
                    "audio_language must be a language tag such as en or pt-br".into(),
                ));
            }
            Some(tag)
        }
    };
    Ok(ProfileInput {
        name,
        description,
        audio_language,
        ..input.clone()
    })
}

/// What a profile says, as the cache applies it.
#[derive(Debug, Clone)]
struct Cached {
    toggles: PlatformToggles,
    audio_language: Option<String>,
    sections: Sections,
    builtin: bool,
}

#[derive(Default)]
struct CacheInner {
    profiles: HashMap<ProfileId, Cached>,
    assignments: HashMap<String, (ProfileId, Timestamp)>,
}

/// The profiles and assignments as the bots and the web app read them, with the
/// platforms the engine has so a profile's default can be spelled out.
#[derive(Clone)]
pub struct ProfileCache {
    facts: Arc<Vec<PlatformFacts>>,
    platforms: Arc<Vec<&'static str>>,
    /// Every platform as the server has it before any profile applies
    baseline: Arc<BTreeMap<String, bool>>,
    presets: Arc<Presets>,
    inner: Arc<RwLock<CacheInner>>,
}

impl ProfileCache {
    fn new(platforms: Vec<PlatformFacts>) -> Self {
        Self {
            presets: Arc::new(Presets::from_platforms(&platforms)),
            platforms: Arc::new(platforms.iter().map(|p| p.id).collect()),
            baseline: Arc::new(
                platforms
                    .iter()
                    .map(|p| (p.id.to_string(), p.on_by_default))
                    .collect(),
            ),
            facts: Arc::new(platforms),
            inner: Arc::new(RwLock::new(CacheInner::default())),
        }
    }

    /// The platforms the engine leaves off until a profile names them
    pub fn off_by_default(&self) -> Vec<&'static str> {
        self.facts
            .iter()
            .filter(|p| !p.on_by_default)
            .map(|p| p.id)
            .collect()
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

    /// One profile applied on top of the baseline, or `None` when no such profile exists
    pub fn alone(&self, profile: ProfileId) -> Option<BTreeMap<String, bool>> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let cached = inner.profiles.get(&profile)?;
        let mut platforms = (*self.baseline).clone();
        cached
            .toggles
            .apply(&mut platforms, &self.presets, &self.baseline);
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

    /// What the profiles assigned along `scopes` add up to, laid over the built-in profile
    pub fn effective_for(&self, scopes: &[Scope]) -> EffectivePolicy {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let mut eff = EffectivePolicy::base((*self.baseline).clone());
        let lay = |eff: &mut EffectivePolicy, cached: &Cached| {
            cached
                .toggles
                .apply(&mut eff.platforms, &self.presets, &self.baseline);
            cached.sections.apply_to(eff);
            if cached.audio_language.is_some() {
                eff.audio_language = cached.audio_language.clone();
            }
        };
        if let Some(default) = inner.profiles.get(&ProfileId::DEFAULT) {
            lay(&mut eff, default);
        }
        for scope in scopes {
            let Some((profile_id, updated_at)) = inner.assignments.get(&scope.key()) else {
                continue;
            };
            let Some(cached) = inner.profiles.get(profile_id) else {
                continue;
            };
            lay(&mut eff, cached);
            eff.applied.push(Assignment {
                scope: scope.clone(),
                profile_id: *profile_id,
                updated_at: *updated_at,
            });
        }
        eff
    }

    pub fn effective(
        &self,
        guild: Option<&str>,
        channel: Option<&str>,
        user: Option<&str>,
    ) -> EffectivePolicy {
        self.effective_for(&Scope::chain(guild, channel, user))
    }

    /// Whether any profile posts links, so a public address must stay known
    pub fn any_delivery_links(&self) -> bool {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        inner.profiles.values().any(|cached| {
            cached.sections.can_link() || cached.sections.delivery.mode == Some(DeliveryMode::Link)
        })
    }

    /// The names of the profiles whose delivery names content view `view`
    pub fn profiles_naming_view(&self, view: &str) -> Vec<ProfileId> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        inner
            .profiles
            .iter()
            .filter(|(_, cached)| {
                matches!(&cached.sections.delivery.view, Some(View::Id(id)) if id == view)
            })
            .map(|(id, _)| *id)
            .collect()
    }

    /// The content view the profile assigned at `scope` names itself, when it names one
    pub fn view_named_at(&self, scope: &Scope) -> Option<String> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let (id, _) = inner.assignments.get(&scope.key())?;
        match &inner.profiles.get(id)?.sections.delivery.view {
            Some(View::Id(view)) => Some(view.clone()),
            _ => None,
        }
    }

    /// The profile assigned at `scope`, and whether it is assigned elsewhere too or built in
    pub fn overlay_at(&self, scope: &Scope) -> Option<(ProfileId, bool, bool)> {
        let inner = self.inner.read().unwrap_or_else(|e| e.into_inner());
        let (id, _) = inner.assignments.get(&scope.key())?;
        let shared = inner
            .assignments
            .iter()
            .filter(|(key, (assigned, _))| assigned == id && **key != scope.key())
            .count()
            > 0;
        let builtin = inner.profiles.get(id).is_some_and(|c| c.builtin);
        Some((*id, shared, builtin))
    }

    /// What profiles are checked against, as far as the cache knows
    pub fn known(&self, views: BTreeMap<String, bool>, public_url: bool) -> Known {
        Known {
            views,
            public_url,
            base_output: self.effective(None, None, None).output,
        }
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
     audio_language, limits, intake, output, upload, delivery, message, errors, dedupe \
     FROM profiles";

/// What the staging tables of older schemas are turned into profiles by
pub(crate) const CONVERTED_BY: Actor = Actor::Provisioning { file: None };

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

    /// Fills the cache from the database, after turning what older schemas kept
    /// elsewhere, in watch rules, settings and content views, into profiles.
    pub async fn load(&self) -> Result<usize, ProfileError> {
        let cache = self.cache.clone();
        transact(&self.db, move |tx| {
            refresh(tx, &cache)?;
            let converted = convert_rule_policies(tx, &cache)?;
            if converted > 0 {
                tracing::info!(
                    profiles = converted,
                    "watch rule policies turned into channel profiles"
                );
                refresh(tx, &cache)?;
            }
            if complete_builtin(tx)? {
                tracing::info!("the built-in profile now names every value");
                refresh(tx, &cache)?;
            }
            let moved = convert_settings_policies(tx, &cache)?;
            if moved > 0 {
                tracing::info!(keys = moved, "settings moved into profiles");
                refresh(tx, &cache)?;
            }
            let rules = convert_rule_intake(tx, &cache)?;
            if rules > 0 {
                tracing::info!(
                    rules,
                    "who may post and where results go moved from watch rules into profiles"
                );
                refresh(tx, &cache)?;
            }
            if convert_frontend_links(tx, &cache)? {
                tracing::info!("content view link settings moved into the built-in profile");
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
        known: &Known,
    ) -> Result<Profile, ProfileError> {
        let input = check(&input, self.platforms(), self.cache.presets(), known, false)?;
        let cache = self.cache.clone();
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let id = ProfileId(Uuid::now_v7());
            let now = Timestamp::now();
            match insert(tx, id, &input, now) {
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
        known: &Known,
    ) -> Result<Profile, ProfileError> {
        let builtin = self
            .cache
            .inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .profiles
            .get(&id)
            .map(|c| c.builtin)
            .ok_or(ProfileError::NotFound(id))?;
        let input = check(
            &input,
            self.platforms(),
            self.cache.presets(),
            known,
            builtin,
        )?;
        let cache = self.cache.clone();
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let previous = get_in(tx, id)?.ok_or(ProfileError::NotFound(id))?;
            let now = Timestamp::now();
            match update_row(tx, id, &input, now) {
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
            let previous = assigned_at(tx, &scope)?;
            let now = Timestamp::now();
            put_assignment(tx, &scope, profile, now)?;
            refresh(tx, &cache)?;
            if previous != Some(profile) {
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
            let Some(previous) = assigned_at(tx, &scope)? else {
                return Ok(());
            };
            tx.execute(
                "DELETE FROM profile_assignments WHERE scope = ?1",
                params![scope.key()],
            )?;
            refresh(tx, &cache)?;
            let name = get_in(tx, previous)?
                .map(|p| p.input.name)
                .unwrap_or_default();
            audit::record(
                tx,
                &actor,
                Action::ProfileUnassign,
                Target::profile(previous, &name),
                json!({ "scope": scope }),
            )?;
            Ok(())
        })
        .await
    }

    /// Replaces the sections `patch` names at `scope`: in the profile assigned there when
    /// it is this scope's alone, else in a new profile made for the scope and assigned
    pub async fn patch_overlay(
        &self,
        actor: &Actor,
        scope: Scope,
        patch: SectionsPatch,
        known: &Known,
    ) -> Result<Profile, ProfileError> {
        check_scope(&scope)?;
        if scope == Scope::Global {
            return Err(ProfileError::Invalid(
                "the whole server's options are the built-in profile's: edit it instead".into(),
            ));
        }
        if patch.is_empty() {
            return Err(ProfileError::Invalid("the patch names no section".into()));
        }
        let mut sections = {
            let inner = self.cache.inner.read().unwrap_or_else(|e| e.into_inner());
            match inner.assignments.get(&scope.key()) {
                Some((id, _)) if *id != ProfileId::DEFAULT => inner
                    .profiles
                    .get(id)
                    .map(|c| c.sections.clone())
                    .unwrap_or_default(),
                _ => Sections::default(),
            }
        };
        patch.apply(&mut sections);
        check_sections(&sections, known, false)?;
        let cache = self.cache.clone();
        let actor = actor.clone();
        transact(&self.db, move |tx| {
            let id = ensure_overlay(tx, &cache, &scope, &|s| patch.apply(s), &actor, None)?;
            get_in(tx, id)?.ok_or(ProfileError::NotFound(id))
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
    ) -> EffectivePolicy {
        self.cache.effective(guild, channel, user)
    }
}

fn encode<T: Serialize>(value: &T) -> Result<String, ProfileError> {
    serde_json::to_string(value)
        .map_err(|e| ProfileError::Store(StoreError::Corrupt(e.to_string())))
}

/// The JSON of every column `input` is stored in, after the name and description
fn columns(input: &ProfileInput) -> rusqlite::Result<[String; 9]> {
    let json = |r: Result<String, ProfileError>| {
        r.map_err(|e| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(e.to_string())))
        })
    };
    Ok([
        json(encode(&input.platforms))?,
        json(encode(&input.limits))?,
        json(encode(&input.intake))?,
        json(encode(&input.output))?,
        json(encode(&input.upload))?,
        json(encode(&input.delivery))?,
        json(encode(&input.message))?,
        json(encode(&input.errors))?,
        json(encode(&input.dedupe))?,
    ])
}

/// Inserts a checked profile as `id`, created and updated `now`.
fn insert(
    conn: &Connection,
    id: ProfileId,
    input: &ProfileInput,
    now: Timestamp,
) -> rusqlite::Result<usize> {
    let [
        platforms,
        limits,
        intake,
        output,
        upload,
        delivery,
        message,
        errors,
        dedupe,
    ] = columns(input)?;
    conn.execute(
        "INSERT INTO profiles (id, name, description, platforms, builtin, created_at, updated_at,
            audio_language, limits, intake, output, upload, delivery, message, errors, dedupe)
         VALUES (?1, ?2, ?3, ?4, 0, ?5, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            id.to_string(),
            input.name,
            input.description,
            platforms,
            nanos(now),
            input.audio_language,
            limits,
            intake,
            output,
            upload,
            delivery,
            message,
            errors,
            dedupe,
        ],
    )
}

/// Replaces what the profile `id` says, updated `now`
fn update_row(
    conn: &Connection,
    id: ProfileId,
    input: &ProfileInput,
    now: Timestamp,
) -> rusqlite::Result<usize> {
    let [
        platforms,
        limits,
        intake,
        output,
        upload,
        delivery,
        message,
        errors,
        dedupe,
    ] = columns(input)?;
    conn.execute(
        "UPDATE profiles SET name = ?2, description = ?3, platforms = ?4, updated_at = ?5,
            audio_language = ?6, limits = ?7, intake = ?8, output = ?9, upload = ?10,
            delivery = ?11, message = ?12, errors = ?13, dedupe = ?14
         WHERE id = ?1",
        params![
            id.to_string(),
            input.name,
            input.description,
            platforms,
            nanos(now),
            input.audio_language,
            limits,
            intake,
            output,
            upload,
            delivery,
            message,
            errors,
            dedupe,
        ],
    )
}

fn assigned_at(conn: &Connection, scope: &Scope) -> Result<Option<ProfileId>, ProfileError> {
    let found: Option<String> = conn
        .query_row(
            "SELECT profile_id FROM profile_assignments WHERE scope = ?1",
            params![scope.key()],
            |row| row.get(0),
        )
        .optional()?;
    found
        .map(|id| id.parse())
        .transpose()
        .map_err(|e| StoreError::Corrupt(format!("profile_assignments.profile_id: {e}")).into())
}

fn put_assignment(
    conn: &Connection,
    scope: &Scope,
    profile: ProfileId,
    now: Timestamp,
) -> rusqlite::Result<usize> {
    conn.execute(
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
    )
}

/// Inserts `input` under its name, or the name numbered when another profile has it
fn insert_uniquely(
    conn: &Connection,
    id: ProfileId,
    input: &mut ProfileInput,
    now: Timestamp,
) -> Result<(), ProfileError> {
    let base_name = input.name.clone();
    let mut attempt = 0;
    loop {
        match insert(conn, id, input, now) {
            Ok(_) => return Ok(()),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if error.code == rusqlite::ErrorCode::ConstraintViolation && attempt < 100 =>
            {
                attempt += 1;
                input.name = format!("{base_name} ({attempt})");
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Applies `change` to what is in force at `scope` alone: the profile assigned there when
/// it is nobody else's, else a new profile for the scope, assigned and audited. `from`
/// names where the values came from in the log.
pub(crate) fn ensure_overlay(
    conn: &Connection,
    cache: &ProfileCache,
    scope: &Scope,
    change: &dyn Fn(&mut Sections),
    actor: &Actor,
    from: Option<serde_json::Value>,
) -> Result<ProfileId, ProfileError> {
    let assigned = assigned_at(conn, scope)?;
    let now = Timestamp::now();
    if let Some(id) = assigned {
        let shared: i64 = conn.query_row(
            "SELECT COUNT(*) FROM profile_assignments WHERE profile_id = ?1 AND scope <> ?2",
            params![id.to_string(), scope.key()],
            |row| row.get(0),
        )?;
        let profile = get_in(conn, id)?.ok_or(ProfileError::NotFound(id))?;
        if !profile.builtin && shared == 0 {
            let mut input = profile.input.clone();
            let mut sections = input.sections();
            change(&mut sections);
            input.set_sections(sections);
            update_row(conn, id, &input, now)?;
            refresh(conn, cache)?;
            let mut details = json!({ "profile": input, "previous": profile.input });
            if let Some(from) = from {
                details["converted_from"] = from;
            }
            audit::record(
                conn,
                actor,
                Action::ProfileUpdate,
                Target::profile(id, &input.name),
                details,
            )?;
            return Ok(id);
        }
    }
    // A shared profile stays as it is for the other scopes: this scope gets a copy of it
    // with the patch, and the built-in or nothing gets an empty profile with the patch.
    let source = match assigned {
        Some(id) if id != ProfileId::DEFAULT => get_in(conn, id)?,
        _ => None,
    };
    let mut input = ProfileInput::named(&scope.label());
    input.description = format!("Options set for {}.", scope.key());
    if let Some(source) = source {
        input.platforms = source.input.platforms.clone();
        input.audio_language = source.input.audio_language.clone();
        input.set_sections(source.input.sections());
    }
    let mut sections = input.sections();
    change(&mut sections);
    input.set_sections(sections);
    let id = ProfileId(Uuid::now_v7());
    insert_uniquely(conn, id, &mut input, now)?;
    let mut details = json!({ "profile": input });
    if let Some(from) = from {
        details["converted_from"] = from;
    }
    audit::record(
        conn,
        actor,
        Action::ProfileCreate,
        Target::profile(id, &input.name),
        details,
    )?;
    put_assignment(conn, scope, id, now)?;
    audit::record(
        conn,
        actor,
        Action::ProfileAssign,
        Target::profile(id, &input.name),
        json!({ "scope": scope, "previous_profile_id": assigned }),
    )?;
    refresh(conn, cache)?;
    Ok(id)
}

pub(crate) fn table_exists(conn: &Connection, name: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        params![name],
        |row| row.get::<_, i64>(0).map(|n| n > 0),
    )
}

fn remembered(conn: &Connection, key: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT COUNT(*) FROM remembered WHERE key = ?1",
        params![key],
        |row| row.get::<_, i64>(0).map(|n| n > 0),
    )
}

fn remember(conn: &Connection, key: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "INSERT INTO remembered (key, value, updated_at) VALUES (?1, '1', ?2)
         ON CONFLICT(key) DO UPDATE SET value = '1', updated_at = excluded.updated_at",
        params![key, nanos(Timestamp::now())],
    )
}

const LIVE_FROM_ZERO: &str = "profiles.live_from_zero_duration";

/// Fills every value the built-in profile leaves unnamed, and turns the zero duration
/// bound older profiles used to refuse live streams into the live switch, once
fn complete_builtin(conn: &Connection) -> Result<bool, ProfileError> {
    let mut changed = false;
    let now = Timestamp::now();
    if !remembered(conn, LIVE_FROM_ZERO)? {
        let mut stmt = conn.prepare(SELECT)?;
        let profiles = stmt
            .query_map([], row_to_profile)?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        for profile in profiles {
            if profile.input.limits.max_duration_secs == Some(Some(0))
                && profile.input.intake.live.is_none()
            {
                let mut input = profile.input.clone();
                input.intake.live = Some(false);
                update_row(conn, profile.id, &input, now)?;
                audit::record(
                    conn,
                    &CONVERTED_BY,
                    Action::ProfileUpdate,
                    Target::profile(profile.id, &input.name),
                    json!({ "profile": input, "previous": profile.input, "converted_from": "max_duration_secs = 0" }),
                )?;
                changed = true;
            }
        }
        remember(conn, LIVE_FROM_ZERO)?;
    }
    let Some(builtin) = get_in(conn, ProfileId::DEFAULT)? else {
        return Ok(changed);
    };
    let filled = builtin.input.sections().filled_from(&Sections::defaults());
    if filled == builtin.input.sections() {
        return Ok(changed);
    }
    let mut input = builtin.input.clone();
    input.set_sections(filled);
    update_row(conn, ProfileId::DEFAULT, &input, now)?;
    audit::record(
        conn,
        &CONVERTED_BY,
        Action::ProfileUpdate,
        Target::profile(ProfileId::DEFAULT, &input.name),
        json!({ "profile": input, "previous": builtin.input, "converted_from": "defaults" }),
    )?;
    Ok(true)
}

/// Names content view `view` in the built-in profile's delivery when it names none yet
pub(crate) fn route_builtin_to_view(
    conn: &Connection,
    cache: &ProfileCache,
    view: &str,
    from: serde_json::Value,
) -> Result<bool, ProfileError> {
    let Some(builtin) = get_in(conn, ProfileId::DEFAULT)? else {
        return Ok(false);
    };
    if matches!(builtin.input.delivery.view, Some(View::Id(_))) {
        return Ok(false);
    }
    let mut input = builtin.input.clone();
    input.delivery.view = Some(View::Id(view.to_string()));
    update_builtin(conn, &builtin, &input, from)?;
    refresh(conn, cache)?;
    Ok(true)
}

/// Writes `input` as the built-in profile, audited as a conversion from `from`
fn update_builtin(
    conn: &Connection,
    previous: &Profile,
    input: &ProfileInput,
    from: serde_json::Value,
) -> Result<(), ProfileError> {
    if *input == previous.input {
        return Ok(());
    }
    update_row(conn, ProfileId::DEFAULT, input, Timestamp::now())?;
    audit::record(
        conn,
        &CONVERTED_BY,
        Action::ProfileUpdate,
        Target::profile(ProfileId::DEFAULT, &input.name),
        json!({ "profile": input, "previous": previous.input, "converted_from": from }),
    )?;
    Ok(())
}

/// Turns the settings an older schema staged, the engine's limits and playlist rules,
/// the Discord target and limits, and the per-server overrides, into the built-in
/// profile and overlays for the servers. Values provisioning wrote are dropped: the
/// provisioning file no longer carries them.
fn convert_settings_policies(
    conn: &Connection,
    cache: &ProfileCache,
) -> Result<usize, ProfileError> {
    if !table_exists(conn, "policy_settings")? {
        return Ok(0);
    }
    let mut stmt = conn.prepare("SELECT key, value, source FROM policy_settings ORDER BY key")?;
    let rows: Vec<(String, String, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<_, _>>()?;
    drop(stmt);
    if rows.is_empty() {
        return Ok(0);
    }
    let Some(builtin) = get_in(conn, ProfileId::DEFAULT)? else {
        return Ok(0);
    };
    let mut input = builtin.input.clone();
    let mut discord_target = serde_json::Map::new();
    let mut local_target = serde_json::Map::new();
    let mut guilds: Option<serde_json::Value> = None;
    let mut moved: Vec<String> = Vec::new();
    let mut dropped: Vec<String> = Vec::new();
    for (key, text, source) in &rows {
        let value: serde_json::Value = serde_json::from_str(text)
            .map_err(|e| StoreError::Corrupt(format!("policy_settings.{key}: {e}")))?;
        if source != "app" {
            tracing::warn!(key, %value, "setting written by provisioning is not carried over: it is a profile value now");
            dropped.push(key.clone());
            continue;
        }
        let number = |value: &serde_json::Value| value.as_u64();
        let mut took = true;
        match key.as_str() {
            "engine.limits.max_source_bytes" => {
                input.limits.max_source_bytes = number(&value).or(input.limits.max_source_bytes)
            }
            "engine.limits.max_duration_secs" => match number(&value) {
                Some(0) => {
                    input.limits.max_duration_secs = Some(Some(0));
                    input.intake.live = Some(false);
                }
                Some(n) => input.limits.max_duration_secs = Some(Some(n)),
                None => input.limits.max_duration_secs = Some(None),
            },
            "engine.limits.max_height" => {
                input.limits.max_height = number(&value)
                    .and_then(|n| u32::try_from(n).ok())
                    .or(input.limits.max_height)
            }
            "engine.live.max_capture_secs" => {
                input.limits.max_capture_secs = number(&value).or(input.limits.max_capture_secs)
            }
            "engine.playlists.enabled" => {
                input.intake.playlists.enabled = value.as_bool().or(input.intake.playlists.enabled)
            }
            "engine.playlists.max_entries" => {
                input.intake.playlists.max_entries = number(&value)
                    .and_then(|n| usize::try_from(n).ok())
                    .or(input.intake.playlists.max_entries)
            }
            "discord.guilds" => guilds = Some(value),
            other => {
                if let Some(leaf) = other.strip_prefix("discord.target.") {
                    discord_target.insert(leaf.to_string(), value);
                } else if let Some(leaf) = other.strip_prefix("local.target.") {
                    local_target.insert(leaf.to_string(), value);
                } else {
                    took = false;
                    tracing::warn!(key, %value, "setting has no profile value to move into and is dropped");
                    dropped.push(key.clone());
                }
            }
        }
        if took {
            moved.push(key.clone());
        }
    }
    for (leaf, value) in local_target {
        discord_target.entry(leaf).or_insert(value);
    }
    if !discord_target.is_empty() {
        let over: TargetOverride =
            serde_json::from_value(serde_json::Value::Object(discord_target))
                .map_err(|e| StoreError::Corrupt(format!("policy_settings discord.target: {e}")))?;
        let merged = DestinationTarget::default().with(&input.output).with(&over);
        input.output = TargetOverride {
            container: Some(merged.container),
            video_codec: Some(merged.video_codec),
            audio_codec: Some(merged.audio_codec),
            max_height: merged.max_height,
            max_fps: merged.max_fps,
            audio_over_still: Some(merged.audio_over_still),
            audio_containers: Some(merged.audio_containers),
            image_containers: Some(merged.image_containers),
            files: Some(merged.files),
        };
    }
    update_builtin(
        conn,
        &builtin,
        &input,
        json!({ "settings": moved, "dropped": dropped }),
    )?;
    refresh(conn, cache)?;
    if let Some(serde_json::Value::Object(guilds)) = guilds {
        for (guild_id, over) in guilds {
            if guild_id.parse::<u64>().is_err() {
                tracing::warn!(
                    guild = guild_id,
                    "discord.guilds entry is not a server id and is dropped"
                );
                continue;
            }
            let max_bytes = over.get("max_bytes").and_then(|v| v.as_u64());
            let target: TargetOverride = over
                .get("target")
                .cloned()
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| StoreError::Corrupt(format!("discord.guilds.{guild_id}.target: {e}")))?
                .unwrap_or_default();
            let patch = SectionsPatch {
                intake: None,
                message: None,
                output: (target != TargetOverride::default()).then_some(target),
                upload: max_bytes.map(|n| UploadOverlay {
                    max_bytes: Some(UploadLimit::Bytes(n.max(1))),
                }),
            };
            if patch.is_empty() {
                continue;
            }
            let scope = Scope::Guild {
                guild_id: guild_id.clone(),
            };
            ensure_overlay(
                conn,
                cache,
                &scope,
                &|s| patch.apply(s),
                &CONVERTED_BY,
                Some(json!({ "setting": format!("discord.guilds.{guild_id}") })),
            )?;
        }
    }
    conn.execute("DELETE FROM policy_settings", [])?;
    Ok(moved.len())
}

/// One watch rule's options as an older schema staged them
type IntakeRow = (
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
);

/// Turns who may post and where results go, as the watch rules of an older schema kept
/// them, into profiles for their channels and servers
fn convert_rule_intake(conn: &Connection, cache: &ProfileCache) -> Result<usize, ProfileError> {
    if !table_exists(conn, "watch_rule_intake")? {
        return Ok(0);
    }
    let mut stmt = conn.prepare(
        "SELECT rule_id, application_id, guild_id, channel_id, post_to, allow_users, allow_roles
         FROM watch_rule_intake ORDER BY guild_id, channel_id, rule_id",
    )?;
    let rows: Vec<IntakeRow> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    drop(stmt);
    let mut converted = 0;
    for (rule_id, application_id, guild_id, channel_id, post_to, users, roles) in rows {
        let list = |text: &str| -> Result<Vec<String>, ProfileError> {
            serde_json::from_str(text)
                .map_err(|e| StoreError::Corrupt(format!("watch_rule_intake lists: {e}")).into())
        };
        let allow_users = list(&users)?;
        let allow_roles = list(&roles)?;
        let scope = match &channel_id {
            Some(channel) => Scope::Channel {
                guild_id: guild_id.clone(),
                channel_id: channel.clone(),
            },
            None => Scope::Guild {
                guild_id: guild_id.clone(),
            },
        };
        let intake = (!allow_users.is_empty() || !allow_roles.is_empty()).then(|| IntakeOverlay {
            allow_users: (!allow_users.is_empty()).then_some(allow_users.clone()),
            allow_roles: (!allow_roles.is_empty()).then_some(allow_roles.clone()),
            ..IntakeOverlay::default()
        });
        let message = post_to.as_ref().map(|channel| MessageOverlay {
            destination: Some(Some(channel.clone())),
            ..MessageOverlay::default()
        });
        let patch = SectionsPatch {
            intake,
            message,
            output: None,
            upload: None,
        };
        if !patch.is_empty() {
            ensure_overlay(
                conn,
                cache,
                &scope,
                &|s| patch.apply(s),
                &CONVERTED_BY,
                Some(json!({
                    "rule_id": rule_id,
                    "application_id": application_id,
                    "guild_id": guild_id,
                    "channel_id": channel_id,
                    "post_to": post_to,
                    "allow_users": allow_users,
                    "allow_roles": allow_roles,
                })),
            )?;
            converted += 1;
        }
        conn.execute(
            "DELETE FROM watch_rule_intake WHERE rule_id = ?1",
            params![rule_id],
        )?;
    }
    Ok(converted)
}

/// Turns the link settings content views of an older schema carried into the built-in
/// profile's delivery: the one view that posted links is named, several leave the choice
/// to the closest, and the floor and page bound come along
fn convert_frontend_links(conn: &Connection, cache: &ProfileCache) -> Result<bool, ProfileError> {
    if !table_exists(conn, "frontend_links")? {
        return Ok(false);
    }
    let mut stmt = conn.prepare(
        "SELECT frontend_id, slug, enabled, scope_everything, links FROM frontend_links ORDER BY slug",
    )?;
    let rows: Vec<(String, String, bool, bool, String)> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })?
        .collect::<Result<_, _>>()?;
    drop(stmt);
    if rows.is_empty() {
        return Ok(false);
    }
    let Some(builtin) = get_in(conn, ProfileId::DEFAULT)? else {
        return Ok(false);
    };
    let (id, _, _, _, links) = rows
        .iter()
        .find(|(_, _, _, everything, _)| *everything)
        .unwrap_or(&rows[0]);
    let links: serde_json::Value = serde_json::from_str(links)
        .map_err(|e| StoreError::Corrupt(format!("frontend_links.links: {e}")))?;
    let mut input = builtin.input.clone();
    input.delivery.view = Some(if rows.len() == 1 {
        View::Id(id.clone())
    } else {
        View::None
    });
    input.delivery.under_floor = Some(UnderFloor::Link);
    input.delivery.over_limit = Some(OverLimit::Link);
    if let Some(n) = links["min_height"]
        .as_u64()
        .and_then(|n| u32::try_from(n).ok())
    {
        input.delivery.floor.min_height = Some(n.max(1));
    }
    if let Some(n) = links["min_bitrate"].as_u64() {
        input.delivery.floor.min_bitrate = Some(n.max(1));
    }
    if let Some(n) = links["max_bytes"].as_u64() {
        input.delivery.link_max_bytes = Some(n.max(1));
    }
    let views: Vec<&String> = rows.iter().map(|(id, _, _, _, _)| id).collect();
    update_builtin(conn, &builtin, &input, json!({ "content_views": views }))?;
    conn.execute("DELETE FROM frontend_links", [])?;
    refresh(conn, cache)?;
    Ok(true)
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
    if !table_exists(conn, "watch_rule_policies")? {
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
                        max_duration_secs: max_duration_secs.map(|n| Some(n.max(0) as u64)),
                        max_height: max_height.map(|n| u32::try_from(n.max(1)).unwrap_or(u32::MAX)),
                        max_capture_secs: None,
                    },
                },
                hosts,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(stmt);
    let mut converted = 0;
    for (mut policy, hosts) in policies {
        policy.hosts = serde_json::from_str(&hosts)
            .map_err(|e| StoreError::Corrupt(format!("watch_rule_policies.allow_hosts: {e}")))?;
        let scope = Scope::Channel {
            guild_id: policy.guild_id.clone(),
            channel_id: policy.channel_id.clone(),
        };
        let existing = assigned_at(conn, &scope)?;
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
        let previous_toggles = existing.and_then(|previous| {
            cache
                .inner
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .profiles
                .get(&previous)
                .map(|cached| cached.toggles.clone())
        });
        let platforms = match (policy.hosts.is_empty(), existing) {
            (true, None) => PlatformToggles::default(),
            (true, Some(_)) => previous_toggles.unwrap_or_default(),
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
                            let on =
                                allowed.contains(id) && before.get(*id).copied().unwrap_or(true);
                            (id.to_string(), on)
                        })
                        .collect(),
                }
            }
        };
        let mut limits = policy.limits;
        if let Some(previous) = existing
            && let Some(cached) = cache
                .inner
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .profiles
                .get(&previous)
        {
            let merged = ProfileLimits {
                max_source_bytes: policy
                    .limits
                    .max_source_bytes
                    .or(cached.sections.limits.max_source_bytes),
                max_duration_secs: policy
                    .limits
                    .max_duration_secs
                    .or(cached.sections.limits.max_duration_secs),
                max_height: policy
                    .limits
                    .max_height
                    .or(cached.sections.limits.max_height),
                max_capture_secs: cached.sections.limits.max_capture_secs,
            };
            limits = merged;
        }
        let description = format!(
            "What the watch rule for channel {} in guild {} used to say about platforms and limits.",
            policy.channel_id, policy.guild_id
        );
        let now = Timestamp::now();
        let id = ProfileId(Uuid::now_v7());
        let mut input = ProfileInput::named(&format!("Channel {} rule", policy.channel_id));
        input.description = description;
        input.platforms = platforms;
        input.limits = limits;
        insert_uniquely(conn, id, &mut input, now)?;
        audit::record(
            conn,
            &CONVERTED_BY,
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
        put_assignment(conn, &scope, id, now)?;
        audit::record(
            conn,
            &CONVERTED_BY,
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
    let profiles: HashMap<ProfileId, Cached> = stmt
        .query_map([], row_to_profile)?
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|p| {
            (
                p.id,
                Cached {
                    toggles: p.input.platforms.clone(),
                    audio_language: p.input.audio_language.clone(),
                    sections: p.input.sections(),
                    builtin: p.builtin,
                },
            )
        })
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
    fn section<T: serde::de::DeserializeOwned>(
        row: &rusqlite::Row<'_>,
        index: usize,
        name: &str,
    ) -> rusqlite::Result<T> {
        let text: String = row.get(index)?;
        serde_json::from_str(&text).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                index,
                rusqlite::types::Type::Text,
                Box::new(std::io::Error::other(format!("profiles.{name}: {e}"))),
            )
        })
    }
    let id: String = row.get(0)?;
    Ok(Profile {
        id: id
            .parse()
            .map_err(|e| corrupt(format!("profiles.id: {e}")))?,
        input: ProfileInput {
            name: row.get(1)?,
            description: row.get(2)?,
            platforms: section(row, 3, "platforms")?,
            audio_language: row.get(7)?,
            limits: section(row, 8, "limits")?,
            intake: section(row, 9, "intake")?,
            output: section(row, 10, "output")?,
            upload: section(row, 11, "upload")?,
            delivery: section(row, 12, "delivery")?,
            message: section(row, 13, "message")?,
            errors: section(row, 14, "errors")?,
            dedupe: section(row, 15, "dedupe")?,
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
        store_with(test_platforms()).await
    }

    async fn store_with(platforms: Vec<PlatformFacts>) -> ProfileStore {
        let db = SqliteStore::open_in_memory().await.unwrap();
        crate::migrations::apply(&db).await.unwrap();
        let store = ProfileStore::new(db, platforms);
        store.load().await.unwrap();
        store
    }

    /// The test platforms and a GIF host the engine leaves off by default
    fn with_giphy() -> Vec<PlatformFacts> {
        let mut platforms = test_platforms();
        platforms.push(PlatformFacts {
            id: "giphy",
            tags: &[Tag::Images],
            hosts: &["giphy.com"],
            on_by_default: false,
        });
        platforms
    }

    fn test_platforms() -> Vec<PlatformFacts> {
        vec![
            PlatformFacts {
                id: "youtube",
                tags: &[Tag::Basic, Tag::Video],
                hosts: &["youtube.com", "youtu.be"],
                on_by_default: true,
            },
            PlatformFacts {
                id: "reddit",
                tags: &[Tag::Basic, Tag::Social],
                hosts: &["reddit.com", "redd.it"],
                on_by_default: true,
            },
            PlatformFacts {
                id: "web",
                tags: &[Tag::Video],
                hosts: &[],
                on_by_default: true,
            },
            PlatformFacts {
                id: "redgifs",
                tags: &[Tag::Nsfw, Tag::Images],
                hosts: &["redgifs.com"],
                on_by_default: true,
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
            .create(&actor(), input("Social and adult", toggles), &known())
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
            store
                .create(&actor(), input("Odd", odd), &known())
                .await
                .unwrap_err(),
            ProfileError::UnknownPreset(_)
        ));
    }

    fn input(name: &str, platforms: PlatformToggles) -> ProfileInput {
        ProfileInput {
            platforms,
            ..ProfileInput::named(name)
        }
    }

    /// A server with a public address and no content views
    fn known() -> Known {
        Known {
            public_url: true,
            ..Known::default()
        }
    }

    #[tokio::test]
    async fn the_default_profile_turns_on_everything_on_by_default_everywhere() {
        let store = store_with(with_giphy()).await;
        let profiles = store.list().await.unwrap();
        assert_eq!(profiles.len(), 1);
        assert_eq!(profiles[0].id, ProfileId::DEFAULT);
        assert!(profiles[0].builtin);
        assert_eq!(
            profiles[0].input.description,
            "Every platform that is on by default. In force wherever nothing else is assigned."
        );
        let effective = store.effective(Some("5"), Some("1"), Some("9"));
        assert!(
            effective
                .platforms
                .iter()
                .all(|(id, on)| *on == (id != "giphy"))
        );
        assert_eq!(effective.applied.len(), 1);
        assert_eq!(effective.applied[0].scope, Scope::Global);
        assert_eq!(effective.disabled(), vec!["giphy".to_string()]);
        assert_eq!(store.cache().off_by_default(), vec!["giphy"]);
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
    async fn platforms_off_by_default_wait_to_be_named() {
        let store = store_with(with_giphy()).await;
        let giphy = vec!["giphy".to_string()];
        assert_eq!(store.effective(None, None, None).disabled(), giphy);

        // Every platform on leaves it off
        let everything = store
            .create(
                &actor(),
                input("Everything", toggles(PlatformDefault::Enabled, &[])),
                &known(),
            )
            .await
            .unwrap();
        let guild = Scope::Guild {
            guild_id: "5".into(),
        };
        store.assign(&actor(), guild, everything.id).await.unwrap();
        assert_eq!(store.effective(Some("5"), None, None).disabled(), giphy);
        assert!(!store.cache().alone(everything.id).unwrap()["giphy"]);

        // An exception turns it on and a narrower every platform on keeps it on
        let gifs = store
            .create(
                &actor(),
                input(
                    "GIFs",
                    toggles(PlatformDefault::Inherit, &[("giphy", true)]),
                ),
                &known(),
            )
            .await
            .unwrap();
        let channel = Scope::Channel {
            guild_id: "5".into(),
            channel_id: "1".into(),
        };
        store.assign(&actor(), channel, gifs.id).await.unwrap();
        assert!(
            store
                .effective(Some("5"), Some("1"), None)
                .disabled()
                .is_empty()
        );
        assert!(store.cache().alone(gifs.id).unwrap()["giphy"]);
        let user = Scope::User {
            guild_id: "5".into(),
            user_id: "9".into(),
        };
        store.assign(&actor(), user, everything.id).await.unwrap();
        assert!(
            store
                .effective(Some("5"), Some("1"), Some("9"))
                .disabled()
                .is_empty()
        );

        // Every platform off takes it off with the rest
        let quiet = store
            .create(
                &actor(),
                input("Quiet", toggles(PlatformDefault::Disabled, &[])),
                &known(),
            )
            .await
            .unwrap();
        let other = Scope::User {
            guild_id: "5".into(),
            user_id: "8".into(),
        };
        store.assign(&actor(), other, quiet.id).await.unwrap();
        assert_eq!(
            store
                .effective(Some("5"), Some("1"), Some("8"))
                .disabled()
                .len(),
            5
        );

        // A chosen preset that holds it turns it on
        let mut images = toggles(PlatformDefault::Inherit, &[]);
        images.presets = vec!["images".into()];
        let images = store
            .create(&actor(), input("Images", images), &known())
            .await
            .unwrap();
        let elsewhere = Scope::Channel {
            guild_id: "5".into(),
            channel_id: "2".into(),
        };
        store.assign(&actor(), elsewhere, images.id).await.unwrap();
        let effective = store.effective(Some("5"), Some("2"), None);
        assert!(effective.platforms["giphy"]);
        assert!(effective.platforms["redgifs"]);
        assert_eq!(
            effective.disabled(),
            vec![
                "reddit".to_string(),
                "web".to_string(),
                "youtube".to_string()
            ]
        );
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
                &known(),
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
                &known(),
            )
            .await
            .unwrap();
        let everything = store
            .create(
                &actor(),
                input("Everything", toggles(PlatformDefault::Enabled, &[])),
                &known(),
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
            cache
                .in_force(Some(Id::new(5)), Some(Id::new(1)), Some(Id::new(8)))
                .disabled,
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
                &known(),
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
                .create(&actor(), input("  ", PlatformToggles::default()), &known())
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
                    ),
                    &known(),
                )
                .await
                .unwrap_err(),
            ProfileError::UnknownPlatform(_)
        ));
        store
            .create(
                &actor(),
                input("Twice", PlatformToggles::default()),
                &known(),
            )
            .await
            .unwrap();
        assert!(matches!(
            store
                .create(
                    &actor(),
                    input("twice", PlatformToggles::default()),
                    &known()
                )
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
                    },
                    &known(),
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
                        max_duration_secs: Some(Some(600)),
                        max_height: None,
                        max_capture_secs: Some(1800),
                    },
                    ..input("Guild limits", PlatformToggles::default())
                },
                &known(),
            )
            .await
            .unwrap();
        let channel_wide = store
            .create(
                &actor(),
                ProfileInput {
                    limits: ProfileLimits {
                        max_source_bytes: None,
                        max_duration_secs: Some(Some(60)),
                        max_height: Some(480),
                        max_capture_secs: None,
                    },
                    ..input("Channel limits", PlatformToggles::default())
                },
                &known(),
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .get(guild_wide.id)
                .await
                .unwrap()
                .unwrap()
                .input
                .limits
                .max_duration_secs,
            Some(Some(600))
        );
        store
            .assign(
                &actor(),
                Scope::Guild {
                    guild_id: "5".into(),
                },
                guild_wide.id,
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
                channel_wide.id,
            )
            .await
            .unwrap();
        let nothing = store.effective(None, None, None).limits;
        assert_eq!(nothing, discoclip_engine::policy::Limits::default());
        let guild = store.effective(Some("5"), Some("2"), None).limits;
        assert_eq!(guild.max_source_bytes, 50_000_000);
        assert_eq!(guild.max_duration_secs, Some(600));
        assert_eq!(guild.max_height, 1080);
        assert_eq!(guild.max_capture_secs, 1800);
        let channel = store
            .cache()
            .in_force(Some(Id::new(5)), Some(Id::new(1)), None)
            .policy
            .limits;
        assert_eq!(channel.max_source_bytes, 50_000_000);
        assert_eq!(channel.max_duration_secs, Some(60));
        assert_eq!(channel.max_height, 480);
        let updated = store
            .update(
                &actor(),
                channel_wide.id,
                ProfileInput {
                    limits: ProfileLimits::default(),
                    ..channel_wide.input.clone()
                },
                &known(),
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
        assert_eq!(
            names,
            vec![
                "Default",
                "Channel 1 rule",
                "Channel 2 options",
                "Channel 3 rule"
            ]
        );
        let one = profiles
            .iter()
            .find(|p| p.input.name == "Channel 1 rule")
            .unwrap();
        assert_eq!(one.input.limits.max_source_bytes, Some(1000));
        assert_eq!(one.input.limits.max_duration_secs, Some(Some(30)));
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
        assert_eq!(channel.limits.max_height, 720);
        assert_eq!(channel.applied.len(), 2);
        // A rule with only who-may-post becomes a profile naming them.
        let two = store.effective(Some("5"), Some("2"), None);
        assert_eq!(two.applied.len(), 2);
        assert_eq!(two.intake.allow_users, vec!["9".to_string()]);
        assert!(two.intake.allow_roles.is_empty());
        let three = store.effective(Some("5"), Some("3"), None);
        assert!(three.disabled().is_empty());
        assert_eq!(three.limits.max_duration_secs, Some(90));
        // The staged rows are gone, so loading again converts nothing more.
        store.load().await.unwrap();
        assert_eq!(store.list().await.unwrap().len(), 4);
        let staged: i64 = db
            .call(|conn| {
                Ok(
                    conn.query_row("SELECT COUNT(*) FROM watch_rule_policies", [], |row| {
                        row.get(0)
                    })?,
                )
            })
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
        assert_eq!(created.len(), 3);
        let from = created
            .iter()
            .find(|e| e.details["converted_from_rule"]["channel_id"] == "1")
            .unwrap();
        assert_eq!(from.details["converted_from_rule"]["rule_id"], "r1");
        assert_eq!(
            from.details["converted_from_rule"]["unmatched_hosts"],
            json!(["nobody.example"])
        );
        assert!(
            entries
                .entries
                .iter()
                .any(|e| e.action == Action::ProfileAssign)
        );
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

    #[tokio::test]
    async fn sections_fold_from_the_widest_to_the_narrowest() {
        use discoclip_bot::Placement;
        use discoclip_engine::policy::{UnderFloor, UploadLimit};

        let store = store().await;
        let whole = store.effective(None, None, None);
        assert_eq!(whole.limits.max_height, 1080);
        assert_eq!(whole.upload.max_bytes, UploadLimit::AUTO);
        assert_eq!(whole.delivery.under_floor, UnderFloor::Skip);
        assert!(whole.dedupe.enabled);
        let mut guild_wide = input("Guild options", PlatformToggles::default());
        guild_wide.output.max_height = Some(720);
        guild_wide.upload.max_bytes = Some(UploadLimit::Bytes(25_000_000));
        guild_wide.message.placement = Some(Placement::Replace);
        let guild_wide = store.create(&actor(), guild_wide, &known()).await.unwrap();
        let mut channel_wide = input("Channel options", PlatformToggles::default());
        channel_wide.intake.allow_users = Some(vec!["9".into()]);
        channel_wide.message.destination = Some(Some("50".into()));
        channel_wide.delivery.under_floor = Some(UnderFloor::Link);
        let channel_wide = store
            .create(&actor(), channel_wide, &known())
            .await
            .unwrap();
        let mut member_wide = input("Member options", PlatformToggles::default());
        member_wide.message.destination = Some(None);
        member_wide.errors.debug = Some(true);
        let member_wide = store.create(&actor(), member_wide, &known()).await.unwrap();
        let guild = Scope::Guild {
            guild_id: "5".into(),
        };
        store.assign(&actor(), guild, guild_wide.id).await.unwrap();
        let channel = Scope::Channel {
            guild_id: "5".into(),
            channel_id: "1".into(),
        };
        store
            .assign(&actor(), channel, channel_wide.id)
            .await
            .unwrap();
        let member = Scope::User {
            guild_id: "5".into(),
            user_id: "9".into(),
        };
        store
            .assign(&actor(), member, member_wide.id)
            .await
            .unwrap();

        let in_guild = store.effective(Some("5"), Some("2"), Some("8"));
        assert_eq!(in_guild.output.max_height, Some(720));
        assert_eq!(in_guild.upload.max_bytes, UploadLimit::Bytes(25_000_000));
        assert_eq!(in_guild.message.policy.placement, Placement::Replace);
        assert!(in_guild.intake.allow_users.is_empty());
        assert_eq!(in_guild.message.destination, None);
        let in_channel = store.effective(Some("5"), Some("1"), Some("8"));
        assert_eq!(in_channel.intake.allow_users, vec!["9".to_string()]);
        assert_eq!(in_channel.message.destination.as_deref(), Some("50"));
        assert_eq!(in_channel.delivery.under_floor, UnderFloor::Link);
        assert_eq!(in_channel.output.max_height, Some(720));
        assert!(!in_channel.errors.debug);
        let for_member = store.effective(Some("5"), Some("1"), Some("9"));
        assert_eq!(for_member.message.destination, None);
        assert!(for_member.errors.debug);
        assert_eq!(for_member.intake.allow_users, vec!["9".to_string()]);
        assert_eq!(for_member.applied.len(), 4);

        let in_force = store
            .cache()
            .in_force(Some(Id::new(5)), Some(Id::new(1)), Some(Id::new(8)));
        assert_eq!(in_force.message_destination, Some(Id::new(50)));
        assert_eq!(in_force.allow_users, vec![Id::new(9)]);
        assert_eq!(
            in_force.policy.upload.max_bytes,
            UploadLimit::Bytes(25_000_000)
        );
        assert_eq!(in_force.discord.message.placement, Placement::Replace);
        assert!(store.cache().any_delivery_links());
    }

    #[tokio::test]
    async fn the_builtin_profile_names_every_value() {
        let store = store().await;
        let builtin = store.get(ProfileId::DEFAULT).await.unwrap().unwrap();
        assert!(builtin.input.sections().missing().is_empty());
        assert_eq!(builtin.input.limits.max_duration_secs, Some(Some(10800)));
        let mut partial = builtin.input.clone();
        partial.delivery.mode = None;
        assert!(matches!(
            store
                .update(&actor(), ProfileId::DEFAULT, partial, &known())
                .await
                .unwrap_err(),
            ProfileError::Incomplete(message) if message == "delivery.mode"
        ));
        let mut lifted = builtin.input.clone();
        lifted.limits.max_duration_secs = Some(None);
        store
            .update(&actor(), ProfileId::DEFAULT, lifted, &known())
            .await
            .unwrap();
        assert_eq!(
            store.effective(None, None, None).limits.max_duration_secs,
            None
        );
        let mut links = builtin.input.clone();
        links.delivery.under_floor = Some(discoclip_engine::policy::UnderFloor::Link);
        assert!(matches!(
            store
                .update(
                    &actor(),
                    ProfileId::DEFAULT,
                    links.clone(),
                    &Known::default()
                )
                .await
                .unwrap_err(),
            ProfileError::NoPublicUrl
        ));
        links.delivery.view = Some(discoclip_engine::policy::View::Id("nope".into()));
        assert!(matches!(
            store
                .update(&actor(), ProfileId::DEFAULT, links, &known())
                .await
                .unwrap_err(),
            ProfileError::UnknownView(_)
        ));
    }

    #[tokio::test]
    async fn overlays_are_made_updated_or_copied_at_a_scope() {
        let store = store().await;
        let channel = Scope::Channel {
            guild_id: "5".into(),
            channel_id: "1".into(),
        };
        let patch: SectionsPatch = serde_json::from_value(json!({
            "intake": {"allow_users": ["9"]},
            "message": {"destination": "50"}
        }))
        .unwrap();
        let made = store
            .patch_overlay(&actor(), channel.clone(), patch, &known())
            .await
            .unwrap();
        assert_eq!(made.input.name, "Channel 1 options");
        assert_eq!(made.input.intake.allow_users, Some(vec!["9".to_string()]));
        assert!(made.input.limits.max_height.is_none());
        let effective = store.effective(Some("5"), Some("1"), None);
        assert_eq!(effective.message.destination.as_deref(), Some("50"));
        assert_eq!(effective.applied[1].profile_id, made.id);

        // The scope's own profile is updated in place.
        let again: SectionsPatch =
            serde_json::from_value(json!({"message": {"destination": null}})).unwrap();
        let updated = store
            .patch_overlay(&actor(), channel.clone(), again, &known())
            .await
            .unwrap();
        assert_eq!(updated.id, made.id);
        assert_eq!(updated.input.message.destination, Some(None));
        assert_eq!(
            updated.input.intake.allow_users,
            Some(vec!["9".to_string()])
        );
        assert_eq!(store.list().await.unwrap().len(), 2);

        // A profile shared with another scope is copied for this one.
        let other = Scope::Channel {
            guild_id: "5".into(),
            channel_id: "2".into(),
        };
        store
            .assign(&actor(), other.clone(), made.id)
            .await
            .unwrap();
        let roles: SectionsPatch =
            serde_json::from_value(json!({"intake": {"allow_roles": ["500"]}})).unwrap();
        let copy = store
            .patch_overlay(&actor(), other.clone(), roles, &known())
            .await
            .unwrap();
        assert_ne!(copy.id, made.id);
        assert_eq!(copy.input.name, "Channel 2 options");
        assert_eq!(copy.input.intake.allow_roles, Some(vec!["500".to_string()]));
        assert_eq!(copy.input.intake.allow_users, None);
        assert_eq!(copy.input.message.destination, Some(None));
        assert_eq!(
            store.effective(Some("5"), Some("2"), None).applied[1].profile_id,
            copy.id
        );
        assert_eq!(
            store
                .effective(Some("5"), Some("1"), None)
                .intake
                .allow_roles
                .len(),
            0
        );
        assert!(matches!(
            store
                .patch_overlay(&actor(), Scope::Global, SectionsPatch::default(), &known())
                .await
                .unwrap_err(),
            ProfileError::Invalid(_)
        ));
        let bad: SectionsPatch =
            serde_json::from_value(json!({"message": {"destination": "x"}})).unwrap();
        assert!(matches!(
            store
                .patch_overlay(&actor(), channel, bad, &known())
                .await
                .unwrap_err(),
            ProfileError::Invalid(_)
        ));
    }

    /// Settings, watch rule options and content view links as an older database kept
    /// them: the migrations stage them and the store moves them into profiles.
    #[tokio::test]
    async fn settings_rules_and_view_links_move_into_profiles() {
        use discoclip_engine::policy::{UnderFloor, UploadLimit, View};
        use discoclip_engine::store::migrate::Migration;

        let db = SqliteStore::open_in_memory().await.unwrap();
        let before: &'static [Migration] = Box::leak(
            crate::migrations::MIGRATIONS
                .iter()
                .copied()
                .take_while(|m| m.name != "profile_sections")
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );
        db.migrate(crate::migrations::SCOPE, before).await.unwrap();
        db.call(|conn| {
            Ok(conn.execute_batch(
                "INSERT INTO settings (key, value, source, updated_at) VALUES
                    ('engine.limits.max_height', '720', 'app', 0),
                    ('engine.limits.max_duration_secs', 'null', 'app', 0),
                    ('engine.playlists.enabled', 'false', 'provisioning', 0),
                    ('discord.target.max_fps', '30', 'app', 0),
                    ('local.target.audio_over_still', 'true', 'app', 0),
                    ('discord.limits.base_bytes', '26214400', 'app', 0),
                    ('discord.guilds', '{\"5\": {\"max_bytes\": 26214400, \"target\": {\"max_height\": 480}}}', 'app', 0),
                    ('engine.workers', '3', 'app', 0);
                 INSERT INTO profiles (id, name, description, platforms, builtin, created_at, updated_at,
                     max_source_bytes, max_duration_secs, max_height, max_capture_secs, audio_language)
                 VALUES ('0193b000-0000-7000-8000-0000000000aa', 'No live', '', '{}', 0, 0, 0,
                     NULL, 0, NULL, NULL, NULL);
                 INSERT INTO discord_applications (id, name, client_id, client_secret, bot_token, created_at, updated_at)
                 VALUES ('0193b000-0000-7000-8000-000000000001', 'A', '1', NULL, 't', 0, 0);
                 INSERT INTO watch_rules (id, application_id, guild_id, channel_id, post_to, allow_users,
                     allow_roles, enabled, created_at, updated_at)
                 VALUES ('r1', '0193b000-0000-7000-8000-000000000001', '5', NULL, '77', '[]', '[\"500\"]', 1, 0, 0);
                 INSERT INTO frontends (id, slug, name, description, enabled, profile_id, config, secret_hash, created_at, updated_at)
                 VALUES ('0193b000-0000-7000-8000-0000000000f1', 'clips', 'Clips', '', 1,
                     '00000000-0000-0000-0000-000000000001',
                     '{\"scope\":{\"guilds\":[],\"channels\":[]},\"access\":{},\"downloads\":true,\"links\":{\"enabled\":true,\"min_height\":480,\"min_bitrate\":800000,\"max_bytes\":1000000000,\"signed_link_days\":14}}',
                     NULL, 0, 0);",
            )?)
        })
        .await
        .unwrap();
        crate::migrations::apply(&db).await.unwrap();
        let store = ProfileStore::new(db.clone(), test_platforms());
        store.load().await.unwrap();

        let builtin = store.get(ProfileId::DEFAULT).await.unwrap().unwrap();
        assert_eq!(builtin.input.limits.max_height, Some(720));
        assert_eq!(builtin.input.limits.max_duration_secs, Some(None));
        assert_eq!(builtin.input.intake.playlists.enabled, Some(true));
        assert_eq!(builtin.input.output.max_fps, Some(30));
        assert_eq!(builtin.input.output.audio_over_still, Some(true));
        assert_eq!(builtin.input.upload.max_bytes, Some(UploadLimit::AUTO));
        assert_eq!(
            builtin.input.delivery.view,
            Some(View::Id("0193b000-0000-7000-8000-0000000000f1".into()))
        );
        assert_eq!(builtin.input.delivery.under_floor, Some(UnderFloor::Link));
        assert_eq!(builtin.input.delivery.floor.min_height, Some(480));
        assert_eq!(builtin.input.delivery.floor.min_bitrate, Some(800_000));
        assert_eq!(builtin.input.delivery.link_max_bytes, Some(1_000_000_000));
        assert!(builtin.input.sections().missing().is_empty());

        let guild = store.effective(Some("5"), Some("1"), None);
        assert_eq!(guild.upload.max_bytes, UploadLimit::Bytes(26_214_400));
        assert_eq!(guild.output.max_height, Some(480));
        assert_eq!(guild.output.max_fps, Some(30));
        assert_eq!(guild.message.destination.as_deref(), Some("77"));
        assert_eq!(guild.intake.allow_roles, vec!["500".to_string()]);
        assert_eq!(
            guild.applied.len(),
            2,
            "one overlay carries both conversions"
        );

        let no_live = store
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.input.name == "No live")
            .unwrap();
        assert_eq!(no_live.input.intake.live, Some(false));
        assert_eq!(no_live.input.limits.max_duration_secs, Some(Some(0)));

        let (staged, removed, view_config): (i64, i64, String) = db
            .call(|conn| {
                Ok((
                    conn.query_row("SELECT COUNT(*) FROM policy_settings", [], |r| r.get(0))?,
                    conn.query_row(
                        "SELECT COUNT(*) FROM settings WHERE key LIKE 'engine.limits.%' \
                         OR key LIKE 'discord.%' OR key LIKE 'local.target.%'",
                        [],
                        |r| r.get(0),
                    )?,
                    conn.query_row("SELECT config FROM frontends", [], |r| r.get(0))?,
                ))
            })
            .await
            .unwrap();
        assert_eq!(staged, 0);
        assert_eq!(removed, 0);
        let view_config: serde_json::Value = serde_json::from_str(&view_config).unwrap();
        assert_eq!(view_config["signed_link_days"], 14);
        assert!(view_config.get("links").is_none());
        let kept: i64 = db
            .call(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM settings WHERE key = 'engine.workers'",
                    [],
                    |r| r.get(0),
                )?)
            })
            .await
            .unwrap();
        assert_eq!(kept, 1);
        // Loading again moves nothing more.
        store.load().await.unwrap();
        assert_eq!(store.list().await.unwrap().len(), 3);
    }
}
