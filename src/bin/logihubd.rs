#![windows_subsystem = "windows"]

use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(name = "logihubd", version, about = "better-logihub resident daemon")]
struct Cli {
    #[arg(long)]
    config: Option<PathBuf>,
    #[arg(long)]
    verbose: bool,
}

fn main() {
    let cli = Cli::parse();
    if better_logihub::daemon::run_resident(cli.config, cli.verbose).is_err() {
        std::process::exit(1);
    }
}
