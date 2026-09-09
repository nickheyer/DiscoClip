use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, GuildMarker, MessageMarker, UserMarker};

pub const SOURCE_ID: &str = "discord";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscordOrigin {
    pub guild: Option<Id<GuildMarker>>,
    pub channel: Id<ChannelMarker>,
    pub message: Id<MessageMarker>,
    pub author: Id<UserMarker>,
}
