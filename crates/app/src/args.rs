use std::path::PathBuf;

use clap::Parser;

use crate::config::CONFIG_PATH_VAR;

const CONFIG_HELP: &str = "\
Provisioning:
  Settings live in the database and are managed from the web app. A config file and
  environment variables provision them: their values are written into the database at
  startup, and the database is what the server reads. A value changed in the web app is
  kept even when the file still names the old one.
  --config names the file; otherwise discoclip.toml, .yaml, .yml or .json is searched for
  in the working directory, the user config directory (for example ~/.config/discoclip)
  and, on Unix, /etc/discoclip. No file is required.
  Environment variables are named DISCOCLIP_<SECTION>__<KEY>, with two underscores
  between levels: DISCOCLIP_DISCORD__TOKEN, DISCOCLIP_ENGINE__LIMITS__MAX_HEIGHT.
  data_dir (DISCOCLIP_DATA_DIR, default \"data\") holds the database and is read from the
  file and the environment only. RUST_LOG overrides log.level.";

/// Runs the DiscoClip server: the web app, the job engine, and the Discord bot.
#[derive(Debug, Parser)]
#[command(name = "discoclip", version, about, after_help = CONFIG_HELP)]
pub struct Args {
    /// Provisioning file (TOML, YAML or JSON)
    #[arg(short, long, env = CONFIG_PATH_VAR, value_name = "FILE")]
    pub config: Option<PathBuf>,
}
