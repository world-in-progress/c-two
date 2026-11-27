use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::{Result, anyhow};
use c2_config::{
    ConfigResolver, ConfigSources, DomainLimitsOverrides, EnvCatalog, LocalEndpointOptions,
    RelayConfigOverrides,
};
use c2_http::relay::RelayServer;
use clap::Args;

#[derive(Args)]
pub struct RelayArgs {
    /// Pre-created Unix IPC root (not applicable to Windows Named Pipes).
    #[arg(long)]
    pub ipc_root: Option<PathBuf>,
    /// Loopback control and business listen address.
    #[arg(long, short = 'b')]
    pub bind: Option<String>,

    /// Business and discovery edge listen address. Non-loopback defaults to TLS.
    #[arg(long)]
    pub edge_bind: Option<String>,

    /// Edge certificate chain in PEM format. Requires --tls-key.
    #[arg(long)]
    pub tls_cert: Option<PathBuf>,

    /// Edge private key in PEM format. Requires --tls-cert.
    #[arg(long)]
    pub tls_key: Option<PathBuf>,

    /// Use a persistent self-signed certificate, including on a loopback edge.
    #[arg(long)]
    pub tls_auto: bool,

    /// Additional DNS names or IP addresses for the self-signed certificate.
    #[arg(long, value_delimiter = ',')]
    pub tls_san: Vec<String>,

    /// Private directory holding the generated TLS and mesh identities.
    #[arg(long)]
    pub state_dir: Option<PathBuf>,

    /// Allow a plaintext edge listener.
    #[arg(long)]
    pub insecure_plaintext: bool,

    /// Pre-register an upstream CRM as NAME=SERVER_ID@ADDRESS. Repeatable.
    #[arg(long = "upstream", short = 'u', value_parser = parse_upstream)]
    pub upstreams: Vec<(String, String, String)>,

    /// Private HTTPS bridge URL, with optional pin and QUIC parameters.
    #[arg(long)]
    pub bridge: Option<String>,

    /// Admission token for the private bridge.
    #[arg(long)]
    pub bridge_token: Option<String>,

    /// Shared secret for mutually authenticating iroh relay peers.
    #[arg(long)]
    pub cluster_secret: Option<String>,

    /// Comma-separated HTTP or iroh mesh seed URLs.
    #[arg(long, short = 's', value_delimiter = ',')]
    pub seeds: Vec<String>,

    /// Stable relay identifier for mesh protocol.
    #[arg(long = "relay-id")]
    pub relay_id: Option<String>,

    /// Publicly reachable URL for this relay.
    #[arg(long = "advertise-url")]
    pub advertise_url: Option<String>,

    /// Relay memory budget in bytes. Zero closes admission to new calls.
    #[arg(long)]
    pub memory_budget_bytes: Option<u64>,

    /// Relay file spill budget in bytes. Zero disables file spill.
    #[arg(long)]
    pub file_spill_budget_bytes: Option<u64>,

    /// Maximum outstanding native forwarding transactions. Zero rejects all calls.
    #[arg(long)]
    pub call_max_outstanding: Option<u64>,

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
    let catalog = EnvCatalog::load(ConfigSources::from_process())?;
    let context = ConfigResolver::resolve_local_endpoint(
        LocalEndpointOptions {
            unix_root: args.ipc_root,
        },
        &catalog,
    )?;
    let config = ConfigResolver::resolve_relay_server(
        RelayConfigOverrides {
            bind: args.bind,
            edge_bind: args.edge_bind,
            tls_cert: args.tls_cert,
            tls_key: args.tls_key,
            tls_auto: args.tls_auto.then_some(true),
            tls_san: (!args.tls_san.is_empty()).then_some(args.tls_san),
            state_dir: args.state_dir,
            insecure_plaintext: args.insecure_plaintext.then_some(true),
            relay_id: args.relay_id,
            bridge: args.bridge,
            bridge_token: args.bridge_token,
            cluster_secret: args.cluster_secret,
            advertise_url: args.advertise_url,
            seeds: (!args.seeds.is_empty()).then_some(args.seeds),
            domain_limits: DomainLimitsOverrides {
                memory_budget_bytes: args.memory_budget_bytes,
                file_spill_budget_bytes: args.file_spill_budget_bytes,
                max_outstanding_calls: args.call_max_outstanding,
            },
            use_proxy: None,
        },
        &catalog,
    )?;

    if args.dry_run {
        let limits = config.domain_limits;
        println!("memory_budget_bytes={}", limits.memory_budget_bytes);
        println!("file_spill_budget_bytes={}", limits.file_spill_budget_bytes);
        println!("call_max_outstanding={}", limits.max_outstanding_calls);
        println!("local_namespace={}", context.namespace_id());
        println!("bind={}", config.bind);
        if let Some(edge) = &config.edge_bind {
            println!("edge_bind={edge}");
            println!(
                "edge_tls_mode={:?}",
                config
                    .edge_tls_mode()
                    .map_err(anyhow::Error::msg)?
                    .expect("configured edge")
            );
        }
        if let Some(cert) = &config.tls_cert {
            println!("tls_cert={}", cert.display());
        }
        if let Some(key) = &config.tls_key {
            println!("tls_key={}", key.display());
        }
        println!("tls_auto={}", config.tls_auto);
        if let Some(dir) = &config.state_dir {
            println!("state_dir={}", dir.display());
        }
        println!("tls_san={}", config.tls_san.join(","));
        println!("insecure_plaintext={}", config.insecure_plaintext);
        println!("relay_id={}", config.relay_id);
        println!("advertise_url={}", config.effective_advertise_url());
        println!("relay_use_proxy={}", config.use_proxy);
        if let Some(bridge) = &config.bridge {
            println!("bridge={bridge}");
        }
        println!(
            "bridge_token={}",
            if config.bridge_token.is_some() {
                "set"
            } else {
                "unset"
            }
        );
        println!(
            "cluster_secret={}",
            if config.cluster_secret.is_some() {
                "set"
            } else {
                "unset"
            }
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

    let mut relay = RelayServer::start_resolved(config, context)
        .map_err(|e| anyhow!("failed to start relay: {e}"))?;
    for (name, server_id, address) in args.upstreams {
        relay
            .register_upstream(&name, &server_id, &address)
            .map_err(|e| anyhow!("failed to register upstream {name:?}: {e}"))?;
    }

    println!("C-Two relay listening at http://{}", relay.local_addr());
    if let Some(addr) = relay.edge_local_addr() {
        println!("C-Two relay edge bound to {addr}");
        println!("C-Two relay edge advertised at {}", relay.advertise_url());
        if let Some(pin) = relay.edge_cert_sha256() {
            let hex: String = pin.iter().map(|byte| format!("{byte:02x}")).collect();
            println!("C-Two relay edge certificate sha256:{hex}");
        }
    }
    if let Some(id) = relay.iroh_endpoint_id() {
        let hex: String = id
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        println!("C-Two relay mesh seed iroh://{hex}");
    }
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
