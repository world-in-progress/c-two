use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};
use c2_config::RelayConfig;
use c2_http::relay::RelayServer;
use clap::Args;

#[derive(Debug, Args)]
pub struct RelayArgs {
    /// HTTP listen address.
    #[arg(
        long,
        short = 'b',
        env = "C2_RELAY_BIND",
        default_value = "0.0.0.0:8080"
    )]
    pub bind: String,

    /// Pre-register an upstream CRM as NAME=ADDRESS. Repeatable.
    #[arg(long = "upstream", short = 'u', value_parser = parse_upstream)]
    pub upstreams: Vec<(String, String)>,

    /// Disconnect idle upstream IPC connections after this many seconds. 0 disables eviction.
    #[arg(
        long = "idle-timeout",
        env = "C2_RELAY_IDLE_TIMEOUT",
        default_value_t = 300
    )]
    pub idle_timeout_secs: u64,

    /// Comma-separated seed relay URLs for mesh mode.
    #[arg(long, short = 's', env = "C2_RELAY_SEEDS", value_delimiter = ',')]
    pub seeds: Vec<String>,

    /// Stable relay identifier for mesh protocol.
    #[arg(long = "relay-id", env = "C2_RELAY_ID")]
    pub relay_id: Option<String>,

    /// Publicly reachable URL for this relay.
    #[arg(long = "advertise-url", env = "C2_RELAY_ADVERTISE_URL")]
    pub advertise_url: Option<String>,

    /// Validate and print relay configuration without starting the server.
    #[arg(long, hide = true)]
    pub dry_run: bool,
}

pub fn parse_upstream(value: &str) -> Result<(String, String), String> {
    let Some((name, address)) = value.split_once('=') else {
        return Err(format!("Invalid upstream {value:?}. Expected NAME=ADDRESS"));
    };
    let name = name.trim();
    let address = address.trim();
    if name.is_empty() {
        return Err("Upstream name cannot be empty".to_string());
    }
    if address.is_empty() {
        return Err("Upstream address cannot be empty".to_string());
    }
    Ok((name.to_string(), address.to_string()))
}

pub fn run(args: RelayArgs) -> Result<()> {
    let config = RelayConfig {
        bind: args.bind.clone(),
        relay_id: args
            .relay_id
            .clone()
            .unwrap_or_else(RelayConfig::generate_relay_id),
        advertise_url: args.advertise_url.clone().unwrap_or_default(),
        seeds: args.seeds.clone(),
        idle_timeout_secs: args.idle_timeout_secs,
        ..Default::default()
    };

    if args.dry_run {
        println!("bind={}", config.bind);
        println!("relay_id={}", config.relay_id);
        println!("advertise_url={}", config.effective_advertise_url());
        println!("idle_timeout={}", config.idle_timeout_secs);
        for seed in &config.seeds {
            println!("seed={seed}");
        }
        for (name, address) in &args.upstreams {
            println!("upstream={name}={address}");
        }
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut relay =
        RelayServer::start(config).map_err(|e| anyhow!("failed to start relay: {e}"))?;
    for (name, address) in args.upstreams {
        relay
            .register_upstream(&name, &address)
            .map_err(|e| anyhow!("failed to register upstream {name:?}: {e}"))?;
    }

    println!("C-Two relay listening at {}", display_url(&args.bind));
    println!("Press Ctrl+C to stop.");

    let (tx, rx) = std::sync::mpsc::channel();
    ctrlc::set_handler(move || {
        let _ = tx.send(());
    })
    .map_err(|e| anyhow!("failed to install Ctrl+C handler: {e}"))?;

    let _ = rx.recv();
    relay
        .stop()
        .map_err(|e| anyhow!("failed to stop relay: {e}"))?;
    thread::sleep(Duration::from_millis(20));
    Ok(())
}

fn display_url(bind: &str) -> String {
    let Some((host, port)) = bind.rsplit_once(':') else {
        return format!("http://127.0.0.1:{bind}");
    };
    let host = if host == "0.0.0.0" || host == "::" {
        "127.0.0.1"
    } else {
        host
    };
    format!("http://{host}:{port}")
}
