use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};
use c2_config::{
    ClientIpcConfigOverrides, ConfigResolver, ConfigSources, LocalEndpointOptions,
    RelayConfigOverrides, RuntimeConfigOverrides,
};
use c2_http::relay::RelayServer;
use clap::Args;

#[derive(Debug, Args)]
pub struct RelayArgs {
    /// Pre-created Unix IPC root (not applicable to Windows Named Pipes).
    #[arg(long)]
    pub ipc_root: Option<PathBuf>,
    /// HTTP listen address.
    #[arg(long, short = 'b')]
    pub bind: Option<String>,

    /// Pre-register an upstream CRM as NAME=SERVER_ID@ADDRESS. Repeatable.
    #[arg(long = "upstream", short = 'u', value_parser = parse_upstream)]
    pub upstreams: Vec<(String, String, String)>,

    /// Disconnect idle upstream IPC connections after this many seconds. 0 disables eviction.
    #[arg(long = "idle-timeout")]
    pub idle_timeout_secs: Option<u64>,

    /// Comma-separated seed relay URLs for mesh mode.
    #[arg(long, short = 's', value_delimiter = ',')]
    pub seeds: Vec<String>,

    /// Stable relay identifier for mesh protocol.
    #[arg(long = "relay-id")]
    pub relay_id: Option<String>,

    /// Publicly reachable URL for this relay.
    #[arg(long = "advertise-url")]
    pub advertise_url: Option<String>,

    /// Enable buddy SHM for data-plane upstream IPC clients.
    #[arg(long, action = clap::ArgAction::Set)]
    pub ipc_pool_enabled: Option<bool>,

    /// Shared upstream IPC SHM backing budget in bytes. Zero rejects positive charges.
    #[arg(long)]
    pub ipc_shm_backing_budget_bytes: Option<u64>,

    /// Shared upstream IPC file backing budget in bytes. Zero rejects positive charges.
    #[arg(long)]
    pub ipc_file_backing_budget_bytes: Option<u64>,

    /// Shared upstream IPC live reassembly budget in bytes. Zero rejects positive charges.
    #[arg(long)]
    pub ipc_live_reassembly_budget_bytes: Option<u64>,

    /// Validate and print relay configuration without starting the server.
    #[arg(long, hide = true)]
    pub dry_run: bool,
}

pub fn parse_upstream(value: &str) -> Result<(String, String, String), String> {
    let Some((name, owner_and_address)) = value.split_once('=') else {
        return Err(format!(
            "Invalid upstream {value:?}. Expected NAME=SERVER_ID@ADDRESS"
        ));
    };
    let Some((server_id, address)) = owner_and_address.split_once('@') else {
        return Err(format!(
            "Invalid upstream {value:?}. Expected NAME=SERVER_ID@ADDRESS"
        ));
    };
    let name = name.trim();
    let address = address.trim();
    if name.is_empty() {
        return Err("Upstream name cannot be empty".to_string());
    }
    c2_config::validate_server_id(server_id).map_err(|reason| format!("Upstream {reason}"))?;
    if address.is_empty() {
        return Err("Upstream address cannot be empty".to_string());
    }
    Ok((name.to_string(), server_id.to_string(), address.to_string()))
}

pub fn run(args: RelayArgs) -> Result<()> {
    let overrides = RuntimeConfigOverrides {
        client_ipc: ClientIpcConfigOverrides {
            pool_enabled: args.ipc_pool_enabled,
            shm_backing_budget_bytes: args.ipc_shm_backing_budget_bytes,
            file_backing_budget_bytes: args.ipc_file_backing_budget_bytes,
            live_reassembly_budget_bytes: args.ipc_live_reassembly_budget_bytes,
            ..Default::default()
        },
        relay: RelayConfigOverrides {
            bind: args.bind.clone(),
            relay_id: args.relay_id.clone(),
            advertise_url: args.advertise_url.clone(),
            seeds: if args.seeds.is_empty() {
                None
            } else {
                Some(args.seeds.clone())
            },
            idle_timeout_secs: args.idle_timeout_secs,
            ..Default::default()
        },
        ..Default::default()
    };
    let sources = ConfigSources::from_process();
    let context = ConfigResolver::resolve_local_endpoint(
        LocalEndpointOptions {
            unix_root: args.ipc_root.clone(),
        },
        sources.clone(),
    )
    .map_err(|e| anyhow!("{e}"))?;
    let resolved =
        ConfigResolver::resolve_relay_server(overrides, sources).map_err(|e| anyhow!("{e}"))?;
    let config = resolved.relay;
    let display_bind = config.bind.clone();

    if args.dry_run {
        println!("local_namespace={}", context.namespace_id());
        println!("bind={}", config.bind);
        println!("relay_id={}", config.relay_id);
        println!("advertise_url={}", config.effective_advertise_url());
        println!("idle_timeout={}", config.idle_timeout_secs);
        println!("relay_use_proxy={}", config.use_proxy);
        println!(
            "remote_payload_chunk_size={}",
            config.remote_payload_chunk_size
        );
        println!("ipc_pool_enabled={}", config.upstream_ipc.pool_enabled);
        println!(
            "ipc_pool_prewarm_segments={}",
            config.upstream_ipc.pool_prewarm_segments
        );
        println!(
            "ipc_shm_backing_budget_bytes={}",
            config.upstream_ipc.shm_backing_budget_bytes
        );
        println!(
            "ipc_file_backing_budget_bytes={}",
            config.upstream_ipc.file_backing_budget_bytes
        );
        println!(
            "ipc_live_reassembly_budget_bytes={}",
            config.upstream_ipc.live_reassembly_budget_bytes
        );
        for seed in &config.seeds {
            println!("seed={seed}");
        }
        for (name, server_id, address) in &args.upstreams {
            println!("upstream={name} server_id={server_id} address={address}");
        }
        return Ok(());
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut relay = RelayServer::start_with_context(config, context)
        .map_err(|e| anyhow!("failed to start relay: {e}"))?;
    for (name, server_id, address) in args.upstreams {
        relay
            .register_upstream(&name, &server_id, &address)
            .map_err(|e| anyhow!("failed to register upstream {name:?}: {e}"))?;
    }

    println!("C-Two relay listening at {}", display_url(&display_bind));
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
