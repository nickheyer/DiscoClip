use clap::Parser;

fn main() -> std::process::ExitCode {
    discoclip::compose::run(discoclip::args::Args::parse())
}
