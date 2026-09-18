//! What a bot knows about its guilds without asking Discord: the guilds, channels, roles
//! and the members it has seen, kept from the gateway's events as they arrive. The web
//! app and the publisher read it instead of calling Discord's REST API, whose rate
//! limits made requests wait for half a minute at a time.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};

use twilight_cache_inmemory::{DefaultInMemoryCache, ResourceType};
use twilight_model::channel::ChannelType;
use twilight_model::gateway::event::Event;
use twilight_model::gateway::payload::incoming::GuildCreate;
use twilight_model::guild::PremiumTier;
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, GuildMarker, RoleMarker, UserMarker};
use uuid::Uuid;

/// A guild the bot is in, as the gateway described it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuildInfo {
    pub id: Id<GuildMarker>,
    pub name: String,
    pub icon: Option<String>,
    pub member_count: Option<u64>,
    pub premium_tier: PremiumTier,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelInfo {
    pub id: Id<ChannelMarker>,
    pub guild_id: Option<Id<GuildMarker>>,
    pub name: String,
    pub kind: ChannelType,
    pub parent_id: Option<Id<ChannelMarker>>,
    pub position: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleInfo {
    pub id: Id<RoleMarker>,
    pub name: String,
    pub color: u32,
    pub position: i64,
    pub managed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberInfo {
    pub id: Id<UserMarker>,
    pub username: String,
    pub display_name: Option<String>,
    pub nick: Option<String>,
    pub avatar: Option<String>,
    pub bot: bool,
}

/// One bot's directory, filled from its gateway events.
pub struct Directory {
    cache: DefaultInMemoryCache,
    /// Guilds whose `GUILD_CREATE` has arrived, so their channels and roles are whole.
    loaded: RwLock<HashSet<Id<GuildMarker>>>,
}

impl Default for Directory {
    fn default() -> Self {
        Self::new()
    }
}

impl Directory {
    pub fn new() -> Self {
        Self {
            cache: DefaultInMemoryCache::builder()
                .resource_types(
                    ResourceType::GUILD
                        | ResourceType::CHANNEL
                        | ResourceType::ROLE
                        | ResourceType::MEMBER
                        | ResourceType::USER,
                )
                .build(),
            loaded: RwLock::new(HashSet::new()),
        }
    }

    /// Takes a gateway event in.
    pub fn update(&self, event: &Event) {
        match event {
            Event::GuildCreate(created) => {
                if let GuildCreate::Available(guild) = &**created {
                    self.loaded
                        .write()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(guild.id);
                }
            }
            Event::GuildDelete(deleted) => {
                self.loaded
                    .write()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&deleted.id);
            }
            _ => {}
        }
        self.cache.update(event);
    }

    /// Whether the guild's channels and roles have arrived.
    pub fn has_guild(&self, guild: Id<GuildMarker>) -> bool {
        self.loaded
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&guild)
    }

    pub fn guild(&self, guild: Id<GuildMarker>) -> Option<GuildInfo> {
        let cached = self.cache.guild(guild)?;
        Some(GuildInfo {
            id: cached.id(),
            name: cached.name().to_string(),
            icon: cached.icon().map(|hash| hash.to_string()),
            member_count: cached.member_count(),
            premium_tier: cached.premium_tier(),
        })
    }

    /// The guild's channels, threads included; `None` until the guild has loaded.
    pub fn channels(&self, guild: Id<GuildMarker>) -> Option<Vec<ChannelInfo>> {
        if !self.has_guild(guild) {
            return None;
        }
        let ids: Vec<Id<ChannelMarker>> = self
            .cache
            .guild_channels(guild)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default();
        Some(ids.into_iter().filter_map(|id| self.channel(id)).collect())
    }

    pub fn channel(&self, id: Id<ChannelMarker>) -> Option<ChannelInfo> {
        let channel = self.cache.channel(id)?;
        Some(ChannelInfo {
            id: channel.id,
            guild_id: channel.guild_id,
            name: channel.name.clone().unwrap_or_default(),
            kind: channel.kind,
            parent_id: channel.parent_id,
            position: channel.position.unwrap_or(0),
        })
    }

    /// The guild's roles; `None` until the guild has loaded.
    pub fn roles(&self, guild: Id<GuildMarker>) -> Option<Vec<RoleInfo>> {
        if !self.has_guild(guild) {
            return None;
        }
        let ids: Vec<Id<RoleMarker>> = self
            .cache
            .guild_roles(guild)
            .map(|set| set.iter().copied().collect())
            .unwrap_or_default();
        Some(
            ids.into_iter()
                .filter_map(|id| {
                    let role = self.cache.role(id)?;
                    Some(RoleInfo {
                        id: role.id,
                        name: role.name.clone(),
                        color: role.colors.primary_color,
                        position: role.position,
                        managed: role.managed,
                    })
                })
                .collect(),
        )
    }

    /// A member the bot has seen: through a member event, or as the author of a message.
    pub fn member(&self, guild: Id<GuildMarker>, user: Id<UserMarker>) -> Option<MemberInfo> {
        let member = self.cache.member(guild, user)?;
        let account = self.cache.user(user)?;
        Some(MemberInfo {
            id: user,
            username: account.name.clone(),
            display_name: account.global_name.clone(),
            nick: member.nick().map(str::to_string),
            avatar: account.avatar.map(|hash| hash.to_string()),
            bot: account.bot,
        })
    }
}

/// The directories of the running applications, by the server's id for each; shared
/// with whatever starts and stops the bots.
pub type Directories = Arc<RwLock<HashMap<Uuid, Arc<Directory>>>>;
