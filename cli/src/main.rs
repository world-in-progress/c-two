mod registry;
mod relay;
mod version;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

const BANNER: &str = include_str!("../assets/banner_unicode.txt");

#[derive(Debug, Parser)]
#[command(
    name = "c3",
    version = version::VERSION,
    about = "C-Two command-line interface",
    before_help = BANNER,
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Start the C-Two HTTP relay server.
    Relay(relay::RelayArgs),
    /// Query relay registry state.
    Registry(registry::RegistryArgs),
}

fn main() -> Result<()> {
    load_env_file();
    let cli = Cli::parse();
    match cli.command {
        Commands::Relay(args) => relay::run(args),
        Commands::Registry(args) => registry::run(args),
    }
}

fn load_env_file() {
    let env_file = std::env::var_os("C2_ENV_FILE");
    let Some(path) = env_file
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(".env")))
        .filter(|path| !path.as_os_str().is_empty())
    else {
        return;
    };
    let _ = dotenvy::from_path(path);
}
