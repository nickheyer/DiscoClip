//! Slash command definitions and parsing.

use twilight_model::application::command::{Command, CommandType};
use twilight_model::application::interaction::application_command::{
    CommandData, CommandOptionValue,
};
use twilight_util::builder::command::{CommandBuilder, StringBuilder};
use url::Url;

pub const CLIP: &str = "clip";
pub const STATUS: &str = "status";

pub fn definitions() -> Vec<Command> {
    vec![
        CommandBuilder::new(
            CLIP,
            "Download a video link and post it here",
            CommandType::ChatInput,
        )
        .option(
            StringBuilder::new("url", "Link to a video or to a page that contains one")
                .required(true),
        )
        .build(),
        CommandBuilder::new(STATUS, "Show DiscoClip job counts", CommandType::ChatInput).build(),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    Clip { url: Result<Url, String> },
    Status,
    Unknown(String),
}

pub fn parse(data: &CommandData) -> Invocation {
    match data.name.as_str() {
        CLIP => {
            let raw = data
                .options
                .iter()
                .find(|o| o.name == "url")
                .and_then(|o| match &o.value {
                    CommandOptionValue::String(s) => Some(s.trim().to_string()),
                    _ => None,
                })
                .unwrap_or_default();
            let url = Url::parse(&raw)
                .ok()
                .filter(|u| matches!(u.scheme(), "http" | "https") && u.host_str().is_some())
                .ok_or_else(|| format!("`{raw}` is not an http(s) link"));
            Invocation::Clip { url }
        }
        STATUS => Invocation::Status,
        other => Invocation::Unknown(other.to_string()),
    }
}
