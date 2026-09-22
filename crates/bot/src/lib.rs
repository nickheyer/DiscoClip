//! Discord adapter for the engine: watches channels, submits requests, publishes results,
//! and keeps the bot supervised alongside the web app.

pub mod client;
pub mod commands;
pub mod config;
pub mod directory;
pub mod link;
pub mod origin;
pub mod profile;
pub mod publish;
pub mod supervisor;
pub mod watch;

pub use client::{Bot, BotError, GuildEvent, http_client};
pub use config::{DiscordConfig, DiscordEndpoints, WatchRule};
pub use directory::{ChannelInfo, Directories, Directory, GuildInfo, MemberInfo, RoleInfo};
pub use link::{LinkTargets, MediaLink, NoLinks};
pub use origin::{DiscordOrigin, SOURCE_ID};
pub use profile::{InForce, PlatformLookup, ProfileSource, turned_off};
pub use publish::{Clients, DiscordPublisher};
pub use supervisor::{
    BotCommand, BotControl, BotRuntime, BotState, BotStatus, ControlError, supervise,
};
pub use watch::{RuleSource, Watcher};
