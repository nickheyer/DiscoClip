//! Profiles as the bot sees them: which platforms are turned off where a link was seen.
//! The app resolves the profiles assigned to the guild, channel and user; the bot only
//! asks, and hands the answer to the engine with every request.

use discoclip_engine::EngineHandle;
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, GuildMarker, UserMarker};
use url::Url;

/// Where a running bot finds which platforms are turned off for links seen in a channel
/// from a user, as profiles are edited while it runs.
pub trait ProfileSource: Send + Sync {
    /// The resolver ids the profiles in force turn off for a link seen in `channel` of
    /// `guild` from `user`; a direct message has no guild.
    fn disabled_platforms(
        &self,
        guild: Option<Id<GuildMarker>>,
        channel: Option<Id<ChannelMarker>>,
        user: Option<Id<UserMarker>>,
    ) -> Vec<String>;
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
/// it is; `None` when some resolver may still have it.
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
