mod cli;
mod compose;
mod config;

use clap::Parser;

fn main() -> std::process::ExitCode {
    compose::run(cli::Args::parse())
}
