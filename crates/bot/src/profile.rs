//! Profiles as the bot sees them: which platforms are turned off and which limits hold
//! where a link was seen. The app resolves the profiles assigned to the guild, channel
//! and user. The bot only asks, and hands the answer to the engine with every request.

use discoclip_engine::EngineHandle;
use discoclip_engine::job::RequestLimits;
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, GuildMarker, UserMarker};
use url::Url;

/// What the profiles assigned add up to where a link was seen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InForce {
    /// The resolver ids turned off.
    pub disabled: Vec<String>,
    /// The limits named, each tightening the engine's own.
    pub limits: RequestLimits,
}

/// Where a running bot finds what the profiles say for links seen in a channel from a
/// user, as profiles are edited while it runs.
pub trait ProfileSource: Send + Sync {
    /// The platforms turned off and the limits assigned for a link seen in `channel` of
    /// `guild` from `user`. A direct message has no guild.
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
    use super::turned_off;

    #[test]
    fn a_link_is_off_only_when_every_taker_is() {
        let off = vec!["youtube".to_string(), "web".to_string()];
        assert_eq!(turned_off(&["youtube", "web"], &off), Some("youtube"));
        assert_eq!(turned_off(&["youtube", "mastodon", "web"], &off), None);
        assert_eq!(turned_off(&[], &off), None);
        assert_eq!(turned_off(&["web"], &[]), None);
    }
}
