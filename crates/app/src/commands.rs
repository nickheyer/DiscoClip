//! Slash command registration: globally, in chosen guilds, or nowhere, as set per
//! application in the app and applied through the application's bot.

use std::sync::Arc;

use discoclip_bot::commands::definitions;
use serde::{Deserialize, Serialize};
use twilight_http::Client;
use twilight_model::id::Id;
use twilight_model::id::marker::ApplicationMarker;

/// Where an application's slash commands are registered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandMode {
    /// Nowhere: any earlier registration is removed.
    Off,
    /// Everywhere the bot is; Discord takes up to an hour to show them.
    Global,
    /// In the listed guilds only, at once.
    Guilds,
}

impl CommandMode {
    pub fn as_str(self) -> &'static str {
        match self {
            CommandMode::Off => "off",
            CommandMode::Global => "global",
            CommandMode::Guilds => "guilds",
        }
    }
}

impl std::str::FromStr for CommandMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "off" => Ok(CommandMode::Off),
            "global" => Ok(CommandMode::Global),
            "guilds" => Ok(CommandMode::Guilds),
            other => Err(format!("unknown command mode {other}")),
        }
    }
}

/// What is registered where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandScope {
    pub mode: CommandMode,
    /// The guilds, for [`CommandMode::Guilds`].
    #[serde(default)]
    pub guilds: Vec<String>,
}

impl Default for CommandScope {
    fn default() -> Self {
        Self {
            mode: CommandMode::Global,
            guilds: Vec::new(),
        }
    }
}

/// One command, as shown in the app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CommandSummary {
    pub name: String,
    pub description: String,
}

pub fn summaries() -> Vec<CommandSummary> {
    definitions()
        .into_iter()
        .map(|command| CommandSummary {
            name: command.name,
            description: command.description,
        })
        .collect()
}

/// Makes Discord's registrations match `desired`, given what `previous` had registered:
/// registrations that `desired` no longer covers are removed. Removing one in a guild the
/// bot cannot reach is logged and skipped, since there is nothing there to remove.
pub async fn apply(
    http: &Arc<Client>,
    application: Id<ApplicationMarker>,
    previous: &CommandScope,
    desired: &CommandScope,
) -> Result<(), twilight_http::Error> {
    let commands = definitions();
    let interaction = http.interaction(application);
    let none: [twilight_model::application::command::Command; 0] = [];

    match desired.mode {
        CommandMode::Global => {
            interaction.set_global_commands(&commands).await?;
        }
        CommandMode::Guilds | CommandMode::Off => {
            if previous.mode == CommandMode::Global {
                interaction.set_global_commands(&none).await?;
            }
        }
    }
    let wanted: &[String] = if desired.mode == CommandMode::Guilds {
        &desired.guilds
    } else {
        &[]
    };
    for guild in wanted {
        if let Some(id) = guild.parse().ok().and_then(Id::new_checked) {
            interaction.set_guild_commands(id, &commands).await?;
        }
    }
    if previous.mode == CommandMode::Guilds {
        for guild in previous.guilds.iter().filter(|g| !wanted.contains(g)) {
            if let Some(id) = guild.parse().ok().and_then(Id::new_checked)
                && let Err(error) = interaction.set_guild_commands(id, &none).await
            {
                tracing::warn!(%application, guild, "could not remove commands from a guild: {error}");
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes_round_trip_and_summaries_name_the_commands() {
        for mode in [CommandMode::Off, CommandMode::Global, CommandMode::Guilds] {
            assert_eq!(mode.as_str().parse::<CommandMode>().unwrap(), mode);
        }
        assert!("everywhere".parse::<CommandMode>().is_err());
        let names: Vec<String> = summaries().into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["clip", "status"]);
        assert_eq!(CommandScope::default().mode, CommandMode::Global);
    }
}
