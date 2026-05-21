mod contract;
mod registry;
mod relay;
mod version;

use anyhow::Result;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use std::io::IsTerminal;

const BANNER: &str = include_str!("../assets/banner_unicode.txt");
const BANNER_COLOR: &str = "\x1b[38;2;102;237;173m";
const RESET_COLOR: &str = "\x1b[0m";

#[derive(Debug, Parser)]
#[command(
    name = "c3",
    version = version::VERSION,
    about = "C-Two command-line interface",
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Validate and inspect portable C-Two contract descriptors.
    Contract(contract::ContractArgs),
    /// Start the C-Two HTTP relay server.
    Relay(relay::RelayArgs),
    /// Query relay registry state.
    Registry(registry::RegistryArgs),
}

fn main() -> Result<()> {
    let matches = Cli::command().before_help(render_banner()).get_matches();
    let cli = Cli::from_arg_matches(&matches)?;
    match cli.command {
        Commands::Contract(args) => contract::run(args),
        Commands::Relay(args) => relay::run(args),
        Commands::Registry(args) => registry::run(args),
    }
}

fn render_banner() -> String {
    if should_color_banner() {
        format!("{BANNER_COLOR}{BANNER}{RESET_COLOR}")
    } else {
        BANNER.to_string()
    }
}

fn should_color_banner() -> bool {
    if std::env::var_os("NO_COLOR").is_some() {
        return false;
    }
    if matches!(std::env::var("CLICOLOR").as_deref(), Ok("0")) {
        return false;
    }
    if std::env::var_os("CLICOLOR_FORCE").is_some() {
        return true;
    }
    std::io::stdout().is_terminal()
}
