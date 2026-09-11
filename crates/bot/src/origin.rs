use discoclip_engine::job::{Origin, SourceId};
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, GuildMarker, MessageMarker, UserMarker};
use url::Url;
use uuid::Uuid;

pub const SOURCE_ID: &str = "discord";

/// Where a request came from on Discord, and through which of the server's applications.
/// Encoded into the engine's opaque origin reference as
/// `application:guild:channel:message:user`, with `0` for absent parts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscordOrigin {
    pub application: Uuid,
    pub guild: Option<Id<GuildMarker>>,
    pub channel: Id<ChannelMarker>,
    pub message: Option<Id<MessageMarker>>,
    pub author: Option<Id<UserMarker>>,
}

impl DiscordOrigin {
    pub fn reference(&self) -> String {
        format!(
            "{}:{}:{}:{}:{}",
            self.application,
            self.guild.map(|g| g.get()).unwrap_or(0),
            self.channel.get(),
            self.message.map(|m| m.get()).unwrap_or(0),
            self.author.map(|a| a.get()).unwrap_or(0)
        )
    }

    pub fn jump_url(&self) -> Option<Url> {
        let message = self.message?;
        let guild = self
            .guild
            .map(|g| g.get().to_string())
            .unwrap_or_else(|| "@me".to_string());
        Url::parse(&format!(
            "https://discord.com/channels/{guild}/{}/{}",
            self.channel.get(),
            message.get()
        ))
        .ok()
    }

    pub fn to_origin(self) -> Origin {
        Origin {
            source: SourceId::new(SOURCE_ID),
            reference: self.reference(),
            url: self.jump_url(),
        }
    }

    pub fn parse(origin: &Origin) -> Option<Self> {
        if origin.source.0 != SOURCE_ID {
            return None;
        }
        let mut parts = origin.reference.split(':');
        let application: Uuid = parts.next()?.parse().ok()?;
        let guild: u64 = parts.next()?.parse().ok()?;
        let channel: u64 = parts.next()?.parse().ok()?;
        let message: u64 = parts.next()?.parse().ok()?;
        let author: u64 = parts.next()?.parse().ok()?;
        Some(Self {
            application,
            guild: Id::new_checked(guild),
            channel: Id::new_checked(channel)?,
            message: Id::new_checked(message),
            author: Id::new_checked(author),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn references_round_trip_with_the_application() {
        let origin = DiscordOrigin {
            application: Uuid::from_u128(7),
            guild: Some(Id::new(1)),
            channel: Id::new(2),
            message: None,
            author: Some(Id::new(4)),
        };
        let encoded = origin.to_origin();
        assert_eq!(
            encoded.reference,
            "00000000-0000-0000-0000-000000000007:1:2:0:4"
        );
        assert_eq!(DiscordOrigin::parse(&encoded), Some(origin));
        assert!(encoded.url.is_none());
        let bad = Origin {
            source: SourceId::new(SOURCE_ID),
            reference: "1:2:3:4".into(),
            url: None,
        };
        assert!(DiscordOrigin::parse(&bad).is_none());
    }
}
