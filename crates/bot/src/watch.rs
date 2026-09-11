use std::sync::Arc;

use discoclip_engine::detect::find_urls;
use discoclip_engine::job::{Request, RequestLimits};
use twilight_model::channel::Message;
use twilight_model::id::Id;
use twilight_model::id::marker::ChannelMarker;
use uuid::Uuid;

use crate::config::WatchRule;
use crate::origin::DiscordOrigin;

/// Where a running bot finds the rule for a channel, as rules are edited while it runs.
pub trait RuleSource: Send + Sync {
    fn rule(&self, application: Uuid, channel: Id<ChannelMarker>) -> Option<WatchRule>;
}

/// Turns messages in watched channels into engine requests. A channel without a rule is
/// not watched.
pub struct Watcher {
    application: Uuid,
    rules: Arc<dyn RuleSource>,
}

impl Watcher {
    pub fn new(application: Uuid, rules: Arc<dyn RuleSource>) -> Self {
        Self { application, rules }
    }

    pub fn requests(&self, message: &Message) -> Vec<Request> {
        if message.author.bot {
            return Vec::new();
        }
        let Some(rule) = self.rules.rule(self.application, message.channel_id) else {
            return Vec::new();
        };
        if !author_allowed(&rule, message) {
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
        let destination = rule.post_to.map(|channel| channel.to_string());
        let limits = RequestLimits {
            max_source_bytes: rule.max_source_bytes,
            max_duration_secs: rule.max_duration_secs,
            max_height: rule.max_height,
        };
        let submitted_by = Some(format!("discord:{}", message.author.id));
        find_urls(&message.content)
            .into_iter()
            .filter(|url| {
                url.host_str()
                    .is_some_and(|h| host_allowed(&rule.allow_hosts, h))
            })
            .map(|url| {
                let mut request = Request::new(origin.clone(), url);
                request.destination = destination.clone();
                request.limits = limits;
                request.submitted_by = submitted_by.clone();
                request
            })
            .collect()
    }
}

/// Whether the message's author is one of the rule's users or holds one of its roles; a
/// rule naming neither takes messages from everyone.
pub fn author_allowed(rule: &WatchRule, message: &Message) -> bool {
    if rule.allow_users.is_empty() && rule.allow_roles.is_empty() {
        return true;
    }
    if rule.allow_users.contains(&message.author.id) {
        return true;
    }
    message.member.as_ref().is_some_and(|member| {
        member
            .roles
            .iter()
            .any(|role| rule.allow_roles.contains(role))
    })
}

/// Whether `host` matches one of `allowed`, or any host when `allowed` is empty. Entries
/// match themselves and their subdomains; a leading `*.` is accepted and ignored.
pub fn host_allowed(allowed: &[String], host: &str) -> bool {
    if allowed.is_empty() {
        return true;
    }
    let host = host.to_ascii_lowercase();
    allowed.iter().any(|entry| {
        let entry = entry.trim_start_matches("*.").to_ascii_lowercase();
        host == entry || host.ends_with(&format!(".{entry}"))
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

    struct Rules(HashMap<u64, WatchRule>);

    impl RuleSource for Rules {
        fn rule(&self, _: Uuid, channel: Id<ChannelMarker>) -> Option<WatchRule> {
            self.0.get(&channel.get()).cloned()
        }
    }

    fn rule(channel: u64, hosts: &[&str]) -> WatchRule {
        let mut rule = WatchRule::for_channel(Id::new(channel));
        rule.allow_hosts = hosts.iter().map(|h| h.to_string()).collect();
        rule
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
        Watcher::new(
            Uuid::from_u128(1),
            Arc::new(Rules(
                rules.into_iter().map(|r| (r.channel.get(), r)).collect(),
            )),
        )
    }

    #[test]
    fn only_channels_with_rules_are_watched() {
        let watcher = watcher(vec![rule(1, &["reddit.com"]), rule(2, &[])]);
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
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].url.as_str(), "https://old.reddit.com/r/v");
        assert!(picked[0].destination.is_none());
        assert_eq!(picked[0].limits, RequestLimits::default());
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
        assert!(host_allowed(&["*.redd.it".to_string()], "v.redd.it"));
        assert!(!host_allowed(&["reddit.com".to_string()], "notreddit.com"));
    }

    #[test]
    fn rules_carry_destination_and_limits() {
        let mut with_limits = rule(1, &[]);
        with_limits.post_to = Some(Id::new(50));
        with_limits.max_source_bytes = Some(1000);
        with_limits.max_duration_secs = Some(30);
        with_limits.max_height = Some(720);
        let watcher = watcher(vec![with_limits]);
        let picked = watcher.requests(&message(1, 9, &[], "https://a.example/v"));
        assert_eq!(picked[0].destination.as_deref(), Some("50"));
        assert_eq!(picked[0].limits.max_source_bytes, Some(1000));
        assert_eq!(picked[0].limits.max_duration_secs, Some(30));
        assert_eq!(picked[0].limits.max_height, Some(720));
        assert_eq!(picked[0].submitted_by.as_deref(), Some("discord:9"));
    }

    #[test]
    fn users_and_roles_limit_who_is_heard() {
        let mut restricted = rule(1, &[]);
        restricted.allow_users = vec![Id::new(9)];
        restricted.allow_roles = vec![Id::new(500)];
        let watcher = watcher(vec![restricted]);
        let link = "https://a.example/v";
        assert_eq!(watcher.requests(&message(1, 9, &[], link)).len(), 1);
        assert_eq!(watcher.requests(&message(1, 10, &[500, 1], link)).len(), 1);
        assert!(watcher.requests(&message(1, 10, &[1], link)).is_empty());
        assert!(watcher.requests(&message(1, 10, &[], link)).is_empty());
    }

    #[test]
    fn bots_are_ignored() {
        let watcher = watcher(vec![rule(1, &[])]);
        let mut from_bot = message(1, 9, &[], "https://a.example/v");
        from_bot.author.bot = true;
        assert!(watcher.requests(&from_bot).is_empty());
    }
}
