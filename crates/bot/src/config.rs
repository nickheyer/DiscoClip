use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use twilight_model::id::Id;
use twilight_model::id::marker::{ChannelMarker, RoleMarker, UserMarker};
use url::Url;

/// What one bot runs with: its Discord application's bot token. The channels it watches
/// come from its rule source as it runs, and its slash commands are registered from the
/// app.
#[derive(Debug, Clone)]
pub struct DiscordConfig {
    pub token: SecretString,
}

/// Where Discord is reached. Empty in production; tests point both at a stand-in.
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

/// A channel to watch for links. Results are posted to `post_to`, or back into the
/// watched channel when unset. `allow_hosts` limits which link hosts are picked up, and
/// `allow_users` and `allow_roles` who may post them; an empty list means no limit. The
/// limits tighten the engine's own for links from this channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchRule {
    pub channel: Id<ChannelMarker>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_to: Option<Id<ChannelMarker>>,
    #[serde(default)]
    pub allow_hosts: Vec<String>,
    #[serde(default)]
    pub allow_users: Vec<Id<UserMarker>>,
    #[serde(default)]
    pub allow_roles: Vec<Id<RoleMarker>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_source_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_duration_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_height: Option<u32>,
}

impl WatchRule {
    /// A rule that takes every link from everyone in `channel` and posts back there.
    pub fn for_channel(channel: Id<ChannelMarker>) -> Self {
        Self {
            channel,
            post_to: None,
            allow_hosts: Vec::new(),
            allow_users: Vec::new(),
            allow_roles: Vec::new(),
            max_source_bytes: None,
            max_duration_secs: None,
            max_height: None,
        }
    }

    pub fn destination(&self) -> Id<ChannelMarker> {
        self.post_to.unwrap_or(self.channel)
    }
}
