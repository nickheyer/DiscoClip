use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use discoclip_engine::publish::{DestinationTarget, TargetOverride};
use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use twilight_model::guild::PremiumTier;
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, GuildMarker, RoleMarker, UserMarker};
use url::Url;

/// What one bot runs with: its Discord application's bot token. The channels it watches
/// come from its rule source as it runs, and its slash commands are registered from the
/// app.
#[derive(Debug, Clone)]
pub struct DiscordConfig {
    pub token: SecretString,
}

/// Where Discord is reached. Empty in production. Tests point both at a stand-in.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DiscordEndpoints {
    /// `host[:port]` that takes the REST API's place, over plain HTTP.
    pub http_proxy: Option<String>,
    /// `ws://host[:port]` that takes the gateway's place.
    pub gateway_url: Option<String>,
}

impl DiscordEndpoints {
    /// The REST API root, ending in a slash.
    pub fn api_base(&self) -> Url {
        let text = match &self.http_proxy {
            Some(host) => format!("http://{host}/api/v10/"),
            None => "https://discord.com/api/v10/".to_string(),
        };
        Url::parse(&text).expect("the API base is a valid URL")
    }

    /// Where OAuth sends browsers to authorize an application.
    pub fn authorize_url(&self) -> Url {
        match &self.http_proxy {
            Some(host) => Url::parse(&format!("http://{host}/oauth2/authorize")),
            None => Url::parse("https://discord.com/oauth2/authorize"),
        }
        .expect("the authorize URL is valid")
    }
}

/// A channel to watch for links
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchRule {
    pub channel: Id<ChannelMarker>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_to: Option<Id<ChannelMarker>>,
    #[serde(default)]
    pub allow_users: Vec<Id<UserMarker>>,
    #[serde(default)]
    pub allow_roles: Vec<Id<RoleMarker>>,
}

impl WatchRule {
    /// A rule that takes every link from everyone in `channel` and posts back there.
    pub fn for_channel(channel: Id<ChannelMarker>) -> Self {
        Self {
            channel,
            post_to: None,
            allow_users: Vec::new(),
            allow_roles: Vec::new(),
        }
    }

    pub fn destination(&self) -> Id<ChannelMarker> {
        self.post_to.unwrap_or(self.channel)
    }
}

const MIB: u64 = 1024 * 1024;

/// How many bytes Discord takes in one upload, by the server's boost level. Boosts are
/// what raise a bot's limit: Nitro raises limits for people, so a bot in a direct
/// message or an unboosted server has the base limit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UploadLimits {
    /// Servers without boosts, and direct messages.
    pub base_bytes: u64,
    pub tier1_bytes: u64,
    pub tier2_bytes: u64,
    pub tier3_bytes: u64,
}

impl Default for UploadLimits {
    fn default() -> Self {
        Self {
            base_bytes: 10 * MIB,
            tier1_bytes: 10 * MIB,
            tier2_bytes: 50 * MIB,
            tier3_bytes: 100 * MIB,
        }
    }
}

impl UploadLimits {
    pub fn for_tier(&self, tier: PremiumTier) -> u64 {
        match tier {
            PremiumTier::Tier1 => self.tier1_bytes,
            PremiumTier::Tier2 => self.tier2_bytes,
            PremiumTier::Tier3 => self.tier3_bytes,
            _ => self.base_bytes,
        }
    }
}

/// How an upload to Discord is given time, and how often it is sent again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UploadSettings {
    /// Seconds every upload gets before its size is counted: for Discord to take the
    /// request and answer it.
    pub grace_secs: u64,
    /// The slowest upload rate a job is given credit for, in bytes per second. A file gets
    /// its size at this rate on top of the grace.
    pub min_rate_bytes_per_sec: u64,
    /// How many times an upload is sent when the connection fails or Discord answers with
    /// a server error.
    pub attempts: u32,
}

impl Default for UploadSettings {
    /// A minute, plus the file at one megabit a second, sent up to three times.
    fn default() -> Self {
        Self {
            grace_secs: 60,
            min_rate_bytes_per_sec: 128 * 1024,
            attempts: 3,
        }
    }
}

impl UploadSettings {
    /// How long an upload of `size` bytes is given before it is abandoned.
    pub fn timeout(&self, size: u64) -> Duration {
        let transfer = size.div_ceil(self.min_rate_bytes_per_sec.max(1));
        Duration::from_secs(self.grace_secs.saturating_add(transfer))
    }
}

/// What one server gets instead of the Discord defaults: its own upload limit, and its
/// own target for the media posted in it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GuildOverride {
    /// The upload limit, in place of the one the server's boost level gives.
    pub max_bytes: Option<u64>,
    pub target: TargetOverride,
}

/// How media posted to Discord is made: the upload limits by boost level, the target
/// every server gets, and the servers that get something else.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiscordSettings {
    pub limits: UploadLimits,
    pub upload: UploadSettings,
    pub target: DestinationTarget,
    /// By server id.
    pub guilds: BTreeMap<String, GuildOverride>,
}

/// The `discord` settings as the publisher reads them, replaced when they change.
pub type SharedDiscordSettings = Arc<RwLock<DiscordSettings>>;

impl DiscordSettings {
    /// The upload limit for a server at `tier`, or for a direct message without one.
    pub fn limit_for(&self, guild: Option<Id<GuildMarker>>, tier: PremiumTier) -> u64 {
        let overridden = guild
            .and_then(|guild| self.guilds.get(&guild.get().to_string()))
            .and_then(|over| over.max_bytes);
        match (guild, overridden) {
            (Some(_), Some(limit)) => limit,
            (Some(_), None) => self.limits.for_tier(tier),
            (None, _) => self.limits.base_bytes,
        }
    }

    /// The target media posted in `guild` is made to.
    pub fn target_for(&self, guild: Option<Id<GuildMarker>>) -> DestinationTarget {
        match guild.and_then(|guild| self.guilds.get(&guild.get().to_string())) {
            Some(over) => self.target.with(&over.target),
            None => self.target.clone(),
        }
    }

    /// Why the settings could not be used, as `path: message`.
    pub fn check(&self) -> Result<(), (String, String)> {
        for (name, value) in [
            ("base_bytes", self.limits.base_bytes),
            ("tier1_bytes", self.limits.tier1_bytes),
            ("tier2_bytes", self.limits.tier2_bytes),
            ("tier3_bytes", self.limits.tier3_bytes),
        ] {
            if value == 0 {
                return Err((
                    format!("discord.limits.{name}"),
                    "must be at least 1".into(),
                ));
            }
        }
        if self.upload.min_rate_bytes_per_sec == 0 {
            return Err((
                "discord.upload.min_rate_bytes_per_sec".into(),
                "must be at least 1".into(),
            ));
        }
        if self.upload.attempts == 0 {
            return Err((
                "discord.upload.attempts".into(),
                "must be at least 1".into(),
            ));
        }
        self.target
            .check()
            .map_err(|message| ("discord.target".to_string(), message))?;
        for (guild, over) in &self.guilds {
            if guild.is_empty() || guild.len() > 20 || !guild.bytes().all(|b| b.is_ascii_digit()) {
                return Err((
                    "discord.guilds".into(),
                    format!("{guild} is not a Discord server id"),
                ));
            }
            if over.max_bytes == Some(0) {
                return Err((
                    format!("discord.guilds.{guild}.max_bytes"),
                    "must be at least 1".into(),
                ));
            }
            self.target
                .with(&over.target)
                .check()
                .map_err(|message| (format!("discord.guilds.{guild}.target"), message))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use discoclip_engine::media::{Container, VideoCodec};

    #[test]
    fn limits_follow_boosts_and_overrides() {
        let mut settings = DiscordSettings::default();
        let guild = Some(Id::new(5));
        assert_eq!(settings.limit_for(guild, PremiumTier::None), 10 * MIB);
        assert_eq!(settings.limit_for(guild, PremiumTier::Tier2), 50 * MIB);
        assert_eq!(settings.limit_for(guild, PremiumTier::Tier3), 100 * MIB);
        assert_eq!(settings.limit_for(None, PremiumTier::Tier3), 10 * MIB);
        settings.guilds.insert(
            "5".into(),
            GuildOverride {
                max_bytes: Some(25 * MIB),
                target: TargetOverride {
                    max_height: Some(720),
                    ..TargetOverride::default()
                },
            },
        );
        assert_eq!(settings.limit_for(guild, PremiumTier::Tier3), 25 * MIB);
        assert_eq!(
            settings.limit_for(Some(Id::new(6)), PremiumTier::Tier3),
            100 * MIB
        );
        assert_eq!(settings.target_for(guild).max_height, Some(720));
        assert_eq!(settings.target_for(Some(Id::new(6))).max_height, None);
        assert!(settings.check().is_ok());
        settings
            .guilds
            .insert("not-an-id".into(), GuildOverride::default());
        assert_eq!(settings.check().unwrap_err().0, "discord.guilds");
        settings.guilds.remove("not-an-id");
        settings.guilds.get_mut("5").unwrap().target.video_codec = Some(VideoCodec::Vp9);
        assert_eq!(settings.check().unwrap_err().0, "discord.guilds.5.target");
        settings.guilds.get_mut("5").unwrap().target.container = Some(Container::Webm);
        settings.guilds.get_mut("5").unwrap().target.audio_codec =
            Some(discoclip_engine::media::AudioCodec::Opus);
        assert!(settings.check().is_ok());
        settings.limits.tier2_bytes = 0;
        assert_eq!(
            settings.check().unwrap_err().0,
            "discord.limits.tier2_bytes"
        );
        let json = serde_json::to_value(DiscordSettings::default()).unwrap();
        assert_eq!(json["limits"]["base_bytes"], 10 * MIB);
        assert_eq!(json["target"]["container"], "mp4");
        assert_eq!(json["upload"]["grace_secs"], 60);
    }

    #[test]
    fn uploads_get_a_grace_plus_the_file_at_the_slowest_rate() {
        let upload = UploadSettings::default();
        assert_eq!(upload.timeout(0), Duration::from_secs(60));
        // 8.7 MB at 128 KiB/s is 68 s on top of the minute.
        assert_eq!(upload.timeout(8_700_000), Duration::from_secs(60 + 67));
        assert_eq!(upload.timeout(100 * MIB), Duration::from_secs(60 + 800));
        let quick = UploadSettings {
            grace_secs: 5,
            min_rate_bytes_per_sec: 1,
            attempts: 1,
        };
        assert_eq!(quick.timeout(10), Duration::from_secs(15));
        let mut settings = DiscordSettings::default();
        settings.upload.attempts = 0;
        assert_eq!(settings.check().unwrap_err().0, "discord.upload.attempts");
        settings.upload.attempts = 1;
        settings.upload.min_rate_bytes_per_sec = 0;
        assert_eq!(
            settings.check().unwrap_err().0,
            "discord.upload.min_rate_bytes_per_sec"
        );
    }
}
