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

use clap::Parser;
use tracing_subscriber::EnvFilter;

use c2_relay::{router, RelayState};

#[derive(Parser, Debug)]
#[command(name = "c2-relay", about = "C-Two HTTP relay server (multi-upstream)")]
struct Cli {
    /// HTTP bind address (e.g. 0.0.0.0:8080).
    #[arg(long, default_value = "0.0.0.0:8080")]
    bind: String,
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

    let state = RelayState::new();
    let app = router::build_router(state);

    let listener = tokio::net::TcpListener::bind(&cli.bind).await?;
    tracing::info!("listening on {}", cli.bind);

    axum::serve(listener, app).await?;

    Ok(())
}
