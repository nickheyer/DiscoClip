//! Profiles as the bot sees them: everything in force where a link was seen. The app
//! resolves the profiles assigned to the guild, channel and user. The bot only asks, and
//! stamps the answer on every request.

use discoclip_engine::EngineHandle;
use discoclip_engine::job::{Origin, Request};
use discoclip_engine::policy::Policy;
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, GuildMarker, RoleMarker, UserMarker};
use url::Url;

use crate::policy::{BotMessages, DiscordPolicy};

/// What the profiles assigned add up to where a link was seen
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InForce {
    /// The resolver ids turned off
    pub disabled: Vec<String>,
    /// The language of the sound wanted, when a profile names one
    pub audio_language: Option<String>,
    /// What the engine runs the job under
    pub policy: Policy,
    pub bot_messages: BotMessages,
    /// Users whose links count, everyone when this and the roles are empty
    pub allow_users: Vec<Id<UserMarker>>,
    pub allow_roles: Vec<Id<RoleMarker>>,
    /// Where results go, the channel the link was seen in when unset
    pub message_destination: Option<Id<ChannelMarker>>,
    pub discord: DiscordPolicy,
}

impl InForce {
    /// A request for `url` seen at `origin`, carrying everything in force there
    pub fn request_for(&self, origin: Origin, url: Url, submitted_by: Option<String>) -> Request {
        let mut request = Request::new(origin, url);
        request.destination = self.message_destination.map(|channel| channel.to_string());
        if let Some(language) = &self.audio_language {
            request.options.audio_language = language.clone();
        }
        request.submitted_by = submitted_by;
        request.disabled_platforms = self.disabled.clone();
        request.policy = self.policy.clone();
        self.discord.stamp(&mut request);
        request
    }
}

/// Where a running bot finds what the profiles say for links seen in a channel from a
/// user, as profiles are edited while it runs.
pub trait ProfileSource: Send + Sync {
    /// Everything in force for a link seen in `channel` of `guild` from `user`. A direct
    /// message has no guild.
    fn in_force(
        &self,
        guild: Option<Id<GuildMarker>>,
        channel: Option<Id<ChannelMarker>>,
        user: Option<Id<UserMarker>>,
    ) -> InForce;
}

/// Which resolvers would take a link, so a link every taker is turned off for is not
/// even submitted.
pub trait PlatformLookup: Send + Sync {
    /// The resolver ids that take `url`, in the order they are offered it.
    fn resolvers_for(&self, url: &Url) -> Vec<&'static str>;
}

impl PlatformLookup for EngineHandle {
    fn resolvers_for(&self, url: &Url) -> Vec<&'static str> {
        EngineHandle::resolvers_for(self, url)
    }
}

/// The resolver that would take `url` and is turned off, when every resolver that takes
/// it is. `None` when some resolver may still have it.
pub fn turned_off<'a>(takers: &[&'a str], disabled: &[String]) -> Option<&'a str> {
    if takers.is_empty() {
        return None;
    }
    let all_off = takers
        .iter()
        .all(|taker| disabled.iter().any(|d| d == taker));
    all_off.then(|| takers[0])
}

#[cfg(test)]
mod tests {
    use super::*;
    use discoclip_engine::job::SourceId;
    use discoclip_engine::policy::UploadLimit;

    #[test]
    fn a_link_is_off_only_when_every_taker_is() {
        let off = vec!["youtube".to_string(), "web".to_string()];
        assert_eq!(turned_off(&["youtube", "web"], &off), Some("youtube"));
        assert_eq!(turned_off(&["youtube", "mastodon", "web"], &off), None);
        assert_eq!(turned_off(&[], &off), None);
        assert_eq!(turned_off(&["web"], &[]), None);
    }

    #[test]
    fn requests_carry_everything_in_force() {
        let mut in_force = InForce {
            disabled: vec!["youtube".into()],
            audio_language: Some("de".into()),
            message_destination: Some(Id::new(50)),
            ..InForce::default()
        };
        in_force.policy.upload.max_bytes = UploadLimit::Bytes(25);
        in_force.discord.errors.debug = true;
        let origin = Origin {
            source: SourceId::new("discord"),
            reference: "x".into(),
            url: None,
            guild: None,
            channel: None,
        };
        let request = in_force.request_for(
            origin,
            Url::parse("https://a.test/v").unwrap(),
            Some("discord:9".into()),
        );
        assert_eq!(request.destination.as_deref(), Some("50"));
        assert_eq!(request.options.audio_language, "de");
        assert_eq!(request.disabled_platforms, vec!["youtube".to_string()]);
        assert_eq!(request.policy.upload.max_bytes, UploadLimit::Bytes(25));
        assert_eq!(request.submitted_by.as_deref(), Some("discord:9"));
        assert_eq!(DiscordPolicy::of(&request).unwrap(), in_force.discord);
    }
}
