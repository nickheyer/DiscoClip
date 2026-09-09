use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "discoclip", version, about)]
pub struct Args {
    #[arg(short, long, default_value = "discoclip.toml")]
    pub config: PathBuf,
}
