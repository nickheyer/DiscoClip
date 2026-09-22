use std::path::PathBuf;

use clap::Parser;

use crate::config::CONFIG_PATH_VAR;

const CONFIG_HELP: &str = r#"Settings:
  Config files and environment variables seed database settings at startup.
  Changes saved in the web app take priority.

  --config selects a file. Otherwise, search for discoclip.toml, .yaml, .yml or .json
  in the working directory, the user config directory and /etc/discoclip on Unix.
  A config file is optional.

  Environment variables use DISCOCLIP_<SECTION>__<KEY>. Examples:
  DISCOCLIP_ENGINE__WORKERS, DISCOCLIP_ENGINE__LIMITS__MAX_HEIGHT.

  DISCOCLIP_DATA_DIR sets the database directory (default: data).
  RUST_LOG overrides log.level."#;

/// Runs the DiscoClip server: the web app, the job engine, and the Discord bot.
#[derive(Debug, Parser)]
#[command(name = "discoclip", version, about, after_help = CONFIG_HELP)]
pub struct Args {
    /// Provisioning file (TOML, YAML or JSON)
    #[arg(short, long, env = CONFIG_PATH_VAR, value_name = "FILE")]
    pub config: Option<PathBuf>,
}
