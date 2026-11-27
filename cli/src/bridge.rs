use std::path::PathBuf;

use anyhow::{Result, anyhow};
use c2_config::{BridgeConfigOverrides, ConfigResolver, ConfigSources, EnvCatalog};
use c2_http::bridge::BridgeServer;
use clap::Args;

#[derive(Args)]
pub struct BridgeArgs {
    /// HTTPS relay listen address.
    #[arg(long)]
    bind: Option<String>,
    /// QUIC address discovery listen address, or off.
    #[arg(long)]
    quic_bind: Option<String>,
    /// PEM certificate chain. Requires --tls-key.
    #[arg(long)]
    tls_cert: Option<PathBuf>,
    /// PEM private key. Requires --tls-cert.
    #[arg(long)]
    tls_key: Option<PathBuf>,
    /// Private state directory for the generated certificate, key and token.
    #[arg(long)]
    state_dir: Option<PathBuf>,
    /// Bridge admission token. Generated and saved if omitted.
    #[arg(long)]
    token: Option<String>,
    /// Print resolved settings without starting the bridge.
    #[arg(long)]
    dry_run: bool,
}

pub fn run(args: BridgeArgs) -> Result<()> {
    let catalog = EnvCatalog::load(ConfigSources::from_process())?;
    let config = ConfigResolver::resolve_bridge(
        BridgeConfigOverrides {
            bind: args.bind,
            quic_bind: args.quic_bind,
            tls_cert: args.tls_cert,
            tls_key: args.tls_key,
            state_dir: args.state_dir,
            token: args.token,
        },
        &catalog,
    )?;
    if args.dry_run {
        println!("bind={}", config.bind);
        println!("quic_bind={}", config.quic_bind.as_deref().unwrap_or("off"));
        for (key, path) in [
            ("tls_cert", config.tls_cert.as_ref()),
            ("tls_key", config.tls_key.as_ref()),
        ] {
            if let Some(path) = path {
                println!("{key}={}", path.display());
            }
        }
        if let Some(dir) = &config.state_dir {
            println!("state_dir={}", dir.display());
        } else {
            println!("state_dir=~/.c3-bridge");
        }
        println!(
            "token={}",
            if config.token.is_some() {
                "explicit"
            } else {
                "auto"
            }
        );
        return Ok(());
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
    ctrlc::set_handler(move || {
        let _ = tx.try_send(());
    })?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let mut bridge = BridgeServer::start(config)
                .await
                .map_err(|error| anyhow!("failed to start bridge: {error}"))?;
            println!("C-Two bridge HTTPS bound to {}", bridge.https_addr());
            if let Some(addr) = bridge.quic_addr() {
                println!("C-Two bridge QUIC address discovery bound to {addr}");
            }
            println!("--bridge '{}'", bridge.bridge_url_template());
            let hex: String = bridge
                .leaf_sha256()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect();
            println!("C-Two bridge certificate sha256:{hex}");
            println!("Bridge token: {}", bridge.token());
            println!("Press Ctrl+C to stop.");
            tokio::select! {
                result = bridge.wait() => result.map_err(anyhow::Error::msg),
                _ = rx.recv() => bridge.stop().await.map_err(anyhow::Error::msg),
            }
        })
}
