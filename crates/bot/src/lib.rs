//! Discord adapter for the engine: watches channels, submits requests, publishes results.

pub mod client;
pub mod commands;
pub mod config;
pub mod origin;
pub mod publish;
pub mod watch;

pub use client::Bot;
pub use config::{BotConfig, WatchRule};
pub use origin::{DiscordOrigin, SOURCE_ID};
pub use publish::DiscordPublisher;
