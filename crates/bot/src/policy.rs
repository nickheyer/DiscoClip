//! The Discord half of the profile in force where a link was seen, carried on the request
//! so the publisher answers under the policy the link was picked up with.

use discoclip_engine::job::Request;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DiscordPolicy {
    pub message: MessagePolicy,
    pub errors: ErrorsPolicy,
    /// The text of the message being replaced, carried when the policy keeps it
    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_text_value: Option<String>,
}

impl DiscordPolicy {
    /// The policy stamped on `request`, or the defaults for a request made before any was
    pub fn of(request: &Request) -> Result<DiscordPolicy, String> {
        if request.policy.publisher.is_null() {
            return Ok(DiscordPolicy::default());
        }
        serde_json::from_value(request.policy.publisher.clone())
            .map_err(|e| format!("the Discord policy stamped on the request is unreadable: {e}"))
    }

    pub fn stamp(&self, request: &mut Request) {
        request.policy.publisher = serde_json::to_value(self).expect("a Discord policy serializes");
    }
}

/// How the result is posted and what goes with it
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MessagePolicy {
    pub placement: Placement,
    pub replace_as: ReplaceAs,
    pub original_text: OriginalText,
    pub original_embeds: OriginalEmbeds,
    pub permissions: PermissionMode,
    pub include: Include,
}

impl Default for MessagePolicy {
    fn default() -> Self {
        Self {
            placement: Placement::Reply,
            replace_as: ReplaceAs::Author,
            original_text: OriginalText::Keep,
            original_embeds: OriginalEmbeds::Keep,
            permissions: PermissionMode::Check,
            include: Include::default(),
        }
    }
}

/// Where the result goes in relation to the message that carried the link
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    #[default]
    Reply,
    Post,
    /// The message that carried the link is removed and the result posted in its place
    Replace,
}

/// Who a replacement appears to come from
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplaceAs {
    Bot,
    /// A webhook post under the original author's name and avatar
    #[default]
    Author,
}

/// Whether a replacement repeats the text of the message it replaces
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginalText {
    #[default]
    Keep,
    Drop,
}

/// Whether the original message keeps the embeds Discord gave its link
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OriginalEmbeds {
    #[default]
    Keep,
    Suppress,
}

/// Whether the bot checks its permissions before acting or lets Discord refuse
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionMode {
    #[default]
    Check,
    Assume,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Requester {
    #[default]
    None,
    Name,
    Mention,
}

/// Whether messages other bots and webhooks post are read for links
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BotMessages {
    #[default]
    Ignore,
    Accept,
}

/// The lines a post carries beside the media
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Include {
    pub source_link: bool,
    pub title: bool,
    pub platform: bool,
    pub uploader: bool,
    pub requester: Requester,
    pub duration: bool,
    pub brand: bool,
    pub earlier_post: bool,
}

impl Default for Include {
    fn default() -> Self {
        Self {
            source_link: true,
            title: true,
            platform: false,
            uploader: false,
            requester: Requester::None,
            duration: false,
            brand: true,
            earlier_post: true,
        }
    }
}

/// How failures on Discord's side are told
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ErrorsPolicy {
    /// Posts the failure, the job log and where to report it in the channel
    pub debug: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use discoclip_engine::job::{Origin, SourceId};
    use url::Url;

    fn request() -> Request {
        Request::new(
            Origin {
                source: SourceId::new("discord"),
                reference: "x".into(),
                url: None,
                guild: None,
                channel: None,
            },
            Url::parse("https://a.test/v").unwrap(),
        )
    }

    #[test]
    fn the_policy_rides_on_the_request() {
        let mut request = request();
        assert_eq!(
            DiscordPolicy::of(&request).unwrap(),
            DiscordPolicy::default()
        );
        let mut policy = DiscordPolicy::default();
        policy.message.placement = Placement::Replace;
        policy.message.include.requester = Requester::Mention;
        policy.errors.debug = true;
        policy.original_text_value = Some("look".into());
        policy.stamp(&mut request);
        assert_eq!(request.policy.publisher["message"]["placement"], "replace");
        assert_eq!(request.policy.publisher["original_text_value"], "look");
        assert_eq!(DiscordPolicy::of(&request).unwrap(), policy);
        request.policy.publisher = serde_json::json!({"message": {"placement": "sideways"}});
        assert!(
            DiscordPolicy::of(&request)
                .unwrap_err()
                .contains("unreadable")
        );
        let json = serde_json::to_value(DiscordPolicy::default()).unwrap();
        assert!(json.get("original_text_value").is_none());
        assert_eq!(json["message"]["replace_as"], "author");
        assert_eq!(json["message"]["include"]["brand"], true);
        assert_eq!(json["errors"]["debug"], false);
    }
}
