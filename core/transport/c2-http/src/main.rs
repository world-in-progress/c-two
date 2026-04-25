//! C-Two HTTP relay binary — multi-upstream mode.
//!
//! Starts with zero upstreams. CRM processes register themselves
//! dynamically via `POST /_register`.
//!
//! Usage:
//!
//! ```bash
//! c2-relay --bind 0.0.0.0:8080
//! ```

use std::sync::Arc;

use clap::Parser;
use tracing_subscriber::EnvFilter;

use c2_http::relay::{router, RelayConfig, RelayState};

#[derive(Parser, Debug)]
#[command(name = "c2-relay", about = "C-Two HTTP relay server (multi-upstream)")]
struct Cli {
    /// HTTP bind address (e.g. 0.0.0.0:8080).
    #[arg(long, default_value = "0.0.0.0:8080")]
    bind: String,

    /// Stable relay identifier.
    #[arg(long)]
    relay_id: Option<String>,

    /// Publicly reachable URL of this relay.
    #[arg(long)]
    advertise_url: Option<String>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    tracing::info!(bind = %cli.bind, "starting c2-relay (multi-upstream, no initial upstreams)");

    let config = Arc::new(RelayConfig {
        bind: cli.bind.clone(),
        relay_id: cli.relay_id.unwrap_or_else(|| RelayConfig::generate_relay_id()),
        advertise_url: cli.advertise_url.unwrap_or_default(),
        ..Default::default()
    });
    let disseminator: Arc<dyn c2_http::relay::disseminator::Disseminator> =
        Arc::new(c2_http::relay::disseminator::FullBroadcast::new());
    let state = Arc::new(RelayState::new(config, disseminator));
    let app = router::build_router(state);

    let listener = tokio::net::TcpListener::bind(&cli.bind).await?;
    tracing::info!("listening on {}", cli.bind);

    axum::serve(listener, app).await?;

    Ok(())
}
