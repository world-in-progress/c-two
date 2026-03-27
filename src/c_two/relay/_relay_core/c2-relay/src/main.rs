//! C-Two HTTP relay binary.
//!
//! Bridges HTTP requests to a Python ServerV2 via IPC v3.
//!
//! Usage:
//!
//! ```bash
//! c2-relay --bind 0.0.0.0:8080 --upstream ipc-v3://my_server
//! ```

use clap::Parser;
use tracing_subscriber::EnvFilter;

use c2_ipc::IpcClient;
use c2_relay::{router, state::RelayState};

#[derive(Parser, Debug)]
#[command(name = "c2-relay", about = "C-Two HTTP relay server")]
struct Cli {
    /// HTTP bind address (e.g. 0.0.0.0:8080).
    #[arg(long, default_value = "0.0.0.0:8080")]
    bind: String,

    /// Upstream IPC v3 address (e.g. ipc-v3://my_server).
    #[arg(long)]
    upstream: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging.
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    tracing::info!(bind = %cli.bind, upstream = %cli.upstream, "starting c2-relay");

    // Connect to upstream Python ServerV2.
    let mut client = IpcClient::new(&cli.upstream);
    client.connect().await.map_err(|e| {
        anyhow::anyhow!("failed to connect to upstream {}: {e}", cli.upstream)
    })?;

    tracing::info!(
        routes = ?client.route_names(),
        "connected to upstream, routes discovered"
    );

    let state = RelayState::new(client);
    let app = router::build_router(state);

    let listener = tokio::net::TcpListener::bind(&cli.bind).await?;
    tracing::info!("listening on {}", cli.bind);

    axum::serve(listener, app).await?;

    Ok(())
}
