use anyhow::{Result, anyhow};
use c2_config::{ConfigResolver, ConfigSources};
use clap::{Args, Subcommand};
use serde::Deserialize;
use serde::de::DeserializeOwned;

#[derive(Deserialize)]
struct RoutesResponse {
    routes: Vec<RouteRecord>,
}

#[derive(Deserialize)]
// Deserialize every endpoint field so a partial route cannot appear valid.
#[allow(dead_code)]
struct RouteRecord {
    name: String,
    relay_id: String,
    relay_url: String,
    locality: RouteLocality,
    crm_ns: String,
    crm_name: String,
    crm_ver: String,
    abi_hash: String,
    signature_hash: String,
    registered_at: f64,
    relay_status: PeerStatus,
    conflict: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum RouteLocality {
    Local,
    Peer,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct PeerRecord {
    relay_id: String,
    url: String,
    route_count: u32,
    status: PeerStatus,
}

#[derive(Deserialize)]
enum PeerStatus {
    Alive,
    Suspect,
    Dead,
}

impl PeerStatus {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Alive => "Alive",
            Self::Suspect => "Suspect",
            Self::Dead => "Dead",
        }
    }
}

#[derive(Debug, Args)]
pub struct RegistryArgs {
    #[command(subcommand)]
    pub command: RegistryCommand,
}

#[derive(Debug, Subcommand)]
pub enum RegistryCommand {
    /// List all registered routes on a relay.
    ListRoutes(RelayOnly),
    /// List known peer relays.
    Peers(RelayOnly),
    /// Force an unreachable relay to leave the mesh.
    ForceLeave(ForceLeaveArgs),
}

#[derive(Debug, Args)]
pub struct ForceLeaveArgs {
    pub relay_id: String,
    /// Relay HTTP address.
    #[arg(long, short = 'r')]
    pub relay: String,
}

#[derive(Deserialize)]
struct ForceLeaveResponse {
    force_left: String,
}

#[derive(Debug, Args)]
pub struct RelayOnly {
    /// Relay HTTP address.
    #[arg(long, short = 'r')]
    pub relay: String,
}

pub fn run(args: RegistryArgs) -> Result<()> {
    match args.command {
        RegistryCommand::ListRoutes(args) => list_routes(&args.relay),
        RegistryCommand::Peers(args) => peers(&args.relay),
        RegistryCommand::ForceLeave(args) => force_leave(&args.relay, &args.relay_id),
    }
}

fn force_leave(relay: &str, relay_id: &str) -> Result<()> {
    let url = format!("{}/_admin/force-leave", relay.trim_end_matches('/'));
    let builder = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5));
    let builder = if resolve_relay_use_proxy(ConfigSources::from_process())? {
        builder
    } else {
        builder.no_proxy()
    };
    let client = builder
        .build()
        .map_err(|e| anyhow!("failed to build HTTP client: {e}"))?;
    let response = client
        .post(&url)
        .json(&serde_json::json!({"relay_id": relay_id}))
        .send()
        .map_err(|e| anyhow!("request failed for {url}: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        let reason = response
            .text()
            .map_err(|e| anyhow!("failed to read rejection from {url}: {e}"))?;
        return Err(anyhow!("relay returned HTTP {status} for {url}: {reason}"));
    }
    let result = response
        .json::<ForceLeaveResponse>()
        .map_err(|e| anyhow!("invalid JSON from {url}: {e}"))?;
    if result.force_left != relay_id {
        return Err(anyhow!(
            "unexpected force-left relay from {url}: {}",
            result.force_left
        ));
    }
    println!("Force-left {relay_id}.");
    Ok(())
}

fn get_json<T: DeserializeOwned>(relay: &str, path: &str) -> Result<T> {
    let url = format!(
        "{}/{}",
        relay.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    let builder = reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5));
    let builder = if resolve_relay_use_proxy(ConfigSources::from_process())? {
        builder
    } else {
        builder.no_proxy()
    };
    let client = builder
        .build()
        .map_err(|e| anyhow!("failed to build HTTP client: {e}"))?;
    let response = client
        .get(&url)
        .send()
        .map_err(|e| anyhow!("request failed for {url}: {e}"))?;
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow!("relay returned HTTP {status} for {url}"));
    }
    response
        .json::<T>()
        .map_err(|e| anyhow!("invalid JSON from {url}: {e}"))
}

fn resolve_relay_use_proxy(sources: ConfigSources) -> Result<bool> {
    ConfigResolver::resolve_relay_use_proxy(sources).map_err(|e| anyhow!("{e}"))
}

fn list_routes(relay: &str) -> Result<()> {
    let RoutesResponse { routes } = get_json(relay, "/_routes")?;
    if routes.is_empty() {
        println!("No routes registered.");
        return Ok(());
    }
    for route in routes {
        println!(
            "{} ({}, {}) - registered_at={} - {}{}",
            route.name,
            route.relay_id,
            route.relay_url,
            route.registered_at,
            route.relay_status.as_str(),
            if route.conflict {
                " - route conflict"
            } else {
                ""
            }
        );
    }
    Ok(())
}

fn peers(relay: &str) -> Result<()> {
    let peers: Vec<PeerRecord> = get_json(relay, "/_peers")?;
    if peers.is_empty() {
        println!("No peers known.");
        return Ok(());
    }
    for peer in peers {
        println!(
            "{} ({}) - {}",
            peer.relay_id,
            peer.url,
            peer.status.as_str()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proxy_policy_uses_env_file() {
        let tempdir = tempfile::tempdir().unwrap();
        let env_file = tempdir.path().join(".env");
        std::fs::write(&env_file, "C2_RELAY_USE_PROXY=1\n").unwrap();

        let use_proxy = resolve_relay_use_proxy(ConfigSources {
            env_file: c2_config::EnvFilePolicy::Path(env_file),
            process_env: Default::default(),
        })
        .unwrap();

        assert!(use_proxy);
    }

    #[test]
    fn proxy_policy_ignores_relay_server_env() {
        let use_proxy = resolve_relay_use_proxy(ConfigSources {
            env_file: c2_config::EnvFilePolicy::Disabled,
            process_env: [
                ("C2_RELAY_USE_PROXY".to_string(), "1".to_string()),
                (
                    "C2_RELAY_IDLE_TIMEOUT".to_string(),
                    "not-a-number".to_string(),
                ),
            ]
            .into_iter()
            .collect(),
        })
        .unwrap();

        assert!(use_proxy);
    }
}
