use std::sync::{Arc, RwLock};
use std::time::Duration;

use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use twilight_model::id::Id;
use twilight_model::id::marker::ChannelMarker;
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

/// A channel watched for links, with everything else about what happens to them in the profile
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchRule {
    pub channel: Id<ChannelMarker>,
}

impl WatchRule {
    pub fn for_channel(channel: Id<ChannelMarker>) -> Self {
        Self { channel }
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

/// How the bot talks to Discord, apart from what the profiles decide
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiscordSettings {
    pub upload: UploadSettings,
}

/// The `discord` settings as the publisher reads them, replaced when they change.
pub type SharedDiscordSettings = Arc<RwLock<DiscordSettings>>;

impl DiscordSettings {
    /// Why the settings could not be used, as `path: message`.
    pub fn check(&self) -> Result<(), (String, String)> {
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

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
        let json = serde_json::to_value(DiscordSettings::default()).unwrap();
        assert_eq!(json["upload"]["grace_secs"], 60);
        assert!(json.get("limits").is_none());
    }
}
