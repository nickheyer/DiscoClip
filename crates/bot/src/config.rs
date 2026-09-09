use secrecy::SecretString;
use serde::Deserialize;
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, GuildMarker};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BotConfig {
    pub token: SecretString,
    pub watches: Vec<WatchRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchRule {
    pub guild: Id<GuildMarker>,
    pub channel: Id<ChannelMarker>,
    pub post_to: Option<Id<ChannelMarker>>,
}
