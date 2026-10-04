use std::sync::Arc;

use discoclip_engine::detect::find_urls;
use discoclip_engine::job::Request;
use twilight_model::channel::Message;
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, GuildMarker};
use uuid::Uuid;

use crate::config::WatchRule;
use crate::directory::Directory;
use crate::link::OwnLinks;
use crate::origin::DiscordOrigin;
use crate::policy::{BotMessages, OriginalText, Placement};
use crate::profile::{InForce, PlatformLookup, ProfileSource, turned_off};

/// The most characters a Discord message holds, which bounds the text a replacement repeats
const MESSAGE_MAX: usize = 2000;

/// Where a running bot finds the rule for a channel, as rules are edited while it runs.
pub trait RuleSource: Send + Sync {
    /// The rule for a message in `channel` of `guild`: the channel's own, or the one
    /// watching the guild whole. Outside a guild only a channel's own rule counts.
    fn rule(
        &self,
        application: Uuid,
        guild: Option<Id<GuildMarker>>,
        channel: Id<ChannelMarker>,
    ) -> Option<WatchRule>;
}

/// Turns messages in watched channels into engine requests. A channel is watched by a
/// rule of its own, or by the rule watching its guild whole. The profile in force for the
/// channel and the author says who is heard, which platforms count and how the result is
/// posted. A link that only turned-off platforms would take is left alone, as is a link
/// to one of the app's own pages and anything the bot itself posted.
pub struct Watcher {
    application: Uuid,
    rules: Arc<dyn RuleSource>,
    profiles: Arc<dyn ProfileSource>,
    platforms: Arc<dyn PlatformLookup>,
    own: Arc<dyn OwnLinks>,
    directory: Arc<Directory>,
}

impl Watcher {
    pub fn new(
        application: Uuid,
        rules: Arc<dyn RuleSource>,
        profiles: Arc<dyn ProfileSource>,
        platforms: Arc<dyn PlatformLookup>,
        own: Arc<dyn OwnLinks>,
        directory: Arc<Directory>,
    ) -> Self {
        Self {
            application,
            rules,
            profiles,
            platforms,
            own,
            directory,
        }
    }

    pub fn requests(&self, message: &Message) -> Vec<Request> {
        if self.directory.is_own_poster(message.author.id) {
            return Vec::new();
        }
        if self
            .rules
            .rule(self.application, message.guild_id, message.channel_id)
            .is_none()
        {
            return Vec::new();
        }
        let in_force = self.profiles.in_force(
            message.guild_id,
            Some(message.channel_id),
            Some(message.author.id),
        );
        if message.author.bot && in_force.bot_messages == BotMessages::Ignore {
            return Vec::new();
        }
        if !author_allowed(&in_force, message) {
            return Vec::new();
        }
        let origin = DiscordOrigin {
            application: self.application,
            guild: message.guild_id,
            channel: message.channel_id,
            message: Some(message.id),
            author: Some(message.author.id),
        }
        .to_origin();
        let submitted_by = Some(format!("discord:{}", message.author.id));
        let posting = &in_force.discord.message;
        let kept_text = (posting.placement == Placement::Replace
            && posting.original_text == OriginalText::Keep)
            .then(|| {
                message
                    .content
                    .chars()
                    .take(MESSAGE_MAX)
                    .collect::<String>()
            })
            .filter(|text| !text.trim().is_empty());
        find_urls(&message.content)
            .into_iter()
            .filter(|url| {
                if self.own.is_own(url) {
                    tracing::debug!(%url, channel = %message.channel_id, "link left alone: one of the app's own pages");
                    return false;
                }
                match turned_off(&self.platforms.resolvers_for(url), &in_force.disabled) {
                    Some(platform) => {
                        tracing::debug!(%url, platform, channel = %message.channel_id, "link left alone: platform turned off here");
                        false
                    }
                    None => true,
                }
            })
            .map(|url| {
                let mut request = in_force.request_for(origin.clone(), url, submitted_by.clone());
                if let Some(text) = &kept_text {
                    let mut discord = in_force.discord.clone();
                    discord.original_text_value = Some(text.clone());
                    discord.stamp(&mut request);
                }
                request
            })
            .collect()
    }
}

/// Whether the author is among the users or holds a role the profile names, everyone when it names none
pub fn author_allowed(in_force: &InForce, message: &Message) -> bool {
    if in_force.allow_users.is_empty() && in_force.allow_roles.is_empty() {
        return true;
    }
    if in_force.allow_users.contains(&message.author.id) {
        return true;
    }
    message.member.as_ref().is_some_and(|member| {
        member
            .roles
            .iter()
            .any(|role| in_force.allow_roles.contains(role))
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use twilight_model::channel::message::MessageType;
    use twilight_model::guild::{MemberFlags, PartialMember};
    use twilight_model::user::User;
    use twilight_model::util::Timestamp;

    use super::*;
    use crate::policy::DiscordPolicy;

    struct Rules(HashMap<u64, WatchRule>);

    impl RuleSource for Rules {
        fn rule(
            &self,
            _: Uuid,
            _: Option<Id<GuildMarker>>,
            channel: Id<ChannelMarker>,
        ) -> Option<WatchRule> {
            self.0.get(&channel.get()).cloned()
        }
    }

    /// The same profile everywhere, whatever the scope
    struct Everywhere(InForce);

    impl ProfileSource for Everywhere {
        fn in_force(
            &self,
            _: Option<Id<GuildMarker>>,
            _: Option<Id<ChannelMarker>>,
            _: Option<Id<twilight_model::id::marker::UserMarker>>,
        ) -> InForce {
            self.0.clone()
        }
    }

    /// Every link is taken by the resolver named for its host's first label, then `web`.
    struct ByHost;

    impl PlatformLookup for ByHost {
        fn resolvers_for(&self, url: &url::Url) -> Vec<&'static str> {
            let host = url.host_str().unwrap_or_default();
            let mut takers: Vec<&'static str> = Vec::new();
            if host.contains("reddit") {
                takers.push("reddit");
            } else if host.contains("youtube") {
                takers.push("youtube");
            }
            takers.push("web");
            takers
        }
    }

    /// The app is reached at one host
    struct OwnHost(&'static str);

    impl OwnLinks for OwnHost {
        fn is_own(&self, url: &url::Url) -> bool {
            url.host_str() == Some(self.0)
        }
    }

    fn rule(channel: u64) -> WatchRule {
        WatchRule::for_channel(Id::new(channel))
    }

    fn message(channel: u64, author: u64, roles: &[u64], content: &str) -> Message {
        let user = User {
            accent_color: None,
            avatar: None,
            avatar_decoration: None,
            avatar_decoration_data: None,
            banner: None,
            bot: false,
            discriminator: 0,
            email: None,
            flags: None,
            global_name: None,
            id: Id::new(author),
            locale: None,
            mfa_enabled: None,
            name: "someone".into(),
            premium_type: None,
            primary_guild: None,
            public_flags: None,
            system: None,
            verified: None,
        };
        let json = serde_json::json!({
            "id": "77", "channel_id": channel.to_string(), "guild_id": "5",
            "author": serde_json::to_value(&user).unwrap(),
            "content": content, "timestamp": "2026-01-01T00:00:00.000000+00:00",
            "tts": false, "mention_everyone": false, "mentions": [], "mention_roles": [],
            "attachments": [], "embeds": [], "pinned": false, "type": 0, "edited_timestamp": null,
            "member": {
                "roles": roles.iter().map(|r| r.to_string()).collect::<Vec<_>>(),
                "joined_at": null, "deaf": false, "mute": false, "flags": 0, "nick": null,
                "communication_disabled_until": null
            }
        });
        let _: (MessageType, Timestamp, MemberFlags, Option<PartialMember>) = (
            MessageType::Regular,
            Timestamp::from_secs(0).unwrap(),
            MemberFlags::empty(),
            None,
        );
        serde_json::from_value(json).unwrap()
    }

    fn watcher(rules: Vec<WatchRule>) -> Watcher {
        watcher_with(rules, InForce::default(), Arc::new(Directory::new()))
    }

    fn watcher_with(
        rules: Vec<WatchRule>,
        in_force: InForce,
        directory: Arc<Directory>,
    ) -> Watcher {
        Watcher::new(
            Uuid::from_u128(1),
            Arc::new(Rules(
                rules.into_iter().map(|r| (r.channel.get(), r)).collect(),
            )),
            Arc::new(Everywhere(in_force)),
            Arc::new(ByHost),
            Arc::new(OwnHost("clips.example")),
            directory,
        )
    }

    #[test]
    fn links_to_the_apps_own_pages_are_left_alone() {
        let watcher = watcher(vec![rule(1)]);
        let picked = watcher.requests(&message(
            1,
            9,
            &[],
            "https://clips.example/f/clips/j/0199 and https://youtube.com/w",
        ));
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].url.as_str(), "https://youtube.com/w");
    }

    #[test]
    fn links_only_turned_off_platforms_take_are_left_alone() {
        let in_force = InForce {
            disabled: vec!["youtube".into(), "web".into()],
            ..InForce::default()
        };
        let watcher = watcher_with(vec![rule(1)], in_force, Arc::new(Directory::new()));
        let picked = watcher.requests(&message(
            1,
            9,
            &[],
            "https://youtube.com/w and https://reddit.com/r and https://other.example/p",
        ));
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].url.as_str(), "https://reddit.com/r");
        assert_eq!(
            picked[0].disabled_platforms,
            vec!["youtube".to_string(), "web".to_string()]
        );
    }

    #[test]
    fn only_channels_with_rules_are_watched() {
        let watcher = watcher(vec![rule(1), rule(2)]);
        assert!(
            watcher
                .requests(&message(3, 9, &[], "https://reddit.com/x"))
                .is_empty()
        );
        let picked = watcher.requests(&message(
            1,
            9,
            &[],
            "see https://old.reddit.com/r/v and https://youtube.com/w",
        ));
        assert_eq!(picked.len(), 2);
        assert_eq!(picked[0].url.as_str(), "https://old.reddit.com/r/v");
        assert!(picked[0].destination.is_none());
        let default = discoclip_engine::policy::Policy::default();
        assert_eq!(picked[0].policy.limits, default.limits);
        assert_eq!(picked[0].policy.delivery, default.delivery);
        assert_eq!(
            DiscordPolicy::of(&picked[0]).unwrap(),
            DiscordPolicy::default()
        );
        assert!(picked[0].disabled_platforms.is_empty());
        assert!(
            picked[0]
                .origin
                .reference
                .starts_with("00000000-0000-0000-0000-000000000001:5:1:77:9")
        );
        assert_eq!(
            watcher
                .requests(&message(2, 9, &[], "https://anything.example/v"))
                .len(),
            1
        );
    }

    #[test]
    fn the_profile_carries_the_destination_and_the_policy() {
        let mut in_force = InForce {
            message_destination: Some(Id::new(50)),
            ..InForce::default()
        };
        in_force.policy.limits.max_height = 720;
        in_force.discord.errors.debug = true;
        let watcher = watcher_with(vec![rule(1)], in_force.clone(), Arc::new(Directory::new()));
        let picked = watcher.requests(&message(1, 9, &[], "https://a.example/v"));
        assert_eq!(picked[0].destination.as_deref(), Some("50"));
        assert_eq!(picked[0].policy.limits.max_height, 720);
        assert_eq!(picked[0].submitted_by.as_deref(), Some("discord:9"));
        assert_eq!(DiscordPolicy::of(&picked[0]).unwrap(), in_force.discord);
    }

    #[test]
    fn users_and_roles_limit_who_is_heard() {
        let in_force = InForce {
            allow_users: vec![Id::new(9)],
            allow_roles: vec![Id::new(500)],
            ..InForce::default()
        };
        let watcher = watcher_with(vec![rule(1)], in_force, Arc::new(Directory::new()));
        let link = "https://a.example/v";
        assert_eq!(watcher.requests(&message(1, 9, &[], link)).len(), 1);
        assert_eq!(watcher.requests(&message(1, 10, &[500, 1], link)).len(), 1);
        assert!(watcher.requests(&message(1, 10, &[1], link)).is_empty());
        assert!(watcher.requests(&message(1, 10, &[], link)).is_empty());
    }

    #[test]
    fn bots_are_ignored_unless_the_profile_takes_them() {
        let mut from_bot = message(1, 9, &[], "https://a.example/v");
        from_bot.author.bot = true;
        assert!(watcher(vec![rule(1)]).requests(&from_bot).is_empty());
        let in_force = InForce {
            bot_messages: BotMessages::Accept,
            ..InForce::default()
        };
        let accepting = watcher_with(vec![rule(1)], in_force, Arc::new(Directory::new()));
        assert_eq!(accepting.requests(&from_bot).len(), 1);
    }

    #[test]
    fn own_posts_are_never_picked_up() {
        let directory = Arc::new(Directory::new());
        directory.add_own_poster(Id::new(9));
        let in_force = InForce {
            bot_messages: BotMessages::Accept,
            ..InForce::default()
        };
        let watcher = watcher_with(vec![rule(1)], in_force, directory);
        assert!(
            watcher
                .requests(&message(1, 9, &[], "https://a.example/v"))
                .is_empty()
        );
        assert_eq!(
            watcher
                .requests(&message(1, 10, &[], "https://a.example/v"))
                .len(),
            1
        );
    }

    #[test]
    fn a_replacement_carries_the_text_it_replaces() {
        let mut in_force = InForce::default();
        in_force.discord.message.placement = Placement::Replace;
        let watcher = watcher_with(vec![rule(1)], in_force.clone(), Arc::new(Directory::new()));
        let picked = watcher.requests(&message(1, 9, &[], "look at this https://a.example/v"));
        let policy = DiscordPolicy::of(&picked[0]).unwrap();
        assert_eq!(
            policy.original_text_value.as_deref(),
            Some("look at this https://a.example/v")
        );
        in_force.discord.message.original_text = OriginalText::Drop;
        let dropping = watcher_with(vec![rule(1)], in_force.clone(), Arc::new(Directory::new()));
        let picked = dropping.requests(&message(1, 9, &[], "look https://a.example/v"));
        assert!(
            DiscordPolicy::of(&picked[0])
                .unwrap()
                .original_text_value
                .is_none()
        );
        in_force.discord.message.original_text = OriginalText::Keep;
        in_force.discord.message.placement = Placement::Reply;
        let replying = watcher_with(vec![rule(1)], in_force, Arc::new(Directory::new()));
        let picked = replying.requests(&message(1, 9, &[], "look https://a.example/v"));
        assert!(
            DiscordPolicy::of(&picked[0])
                .unwrap()
                .original_text_value
                .is_none()
        );
    }
}
