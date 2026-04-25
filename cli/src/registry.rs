use anyhow::{Result, anyhow};
use clap::{Args, Subcommand};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde_json::Value;

#[derive(Debug, Args)]
pub struct RegistryArgs {
    #[command(subcommand)]
    pub command: RegistryCommand,
}

#[derive(Debug, Subcommand)]
pub enum RegistryCommand {
    /// List all registered routes on a relay.
    ListRoutes(RelayOnly),
    /// Resolve a resource name through a relay.
    Resolve(ResolveArgs),
    /// List known peer relays.
    Peers(RelayOnly),
}

#[derive(Debug, Args)]
pub struct RelayOnly {
    /// Relay HTTP address.
    #[arg(long, short = 'r')]
    pub relay: String,
}

#[derive(Debug, Args)]
pub struct ResolveArgs {
    /// Relay HTTP address.
    #[arg(long, short = 'r')]
    pub relay: String,
    /// Resource name to resolve.
    pub name: String,
}

pub fn run(args: RegistryArgs) -> Result<()> {
    match args.command {
        RegistryCommand::ListRoutes(args) => list_routes(&args.relay),
        RegistryCommand::Resolve(args) => resolve(&args.relay, &args.name),
        RegistryCommand::Peers(args) => peers(&args.relay),
    }
}

fn get_json(relay: &str, path: &str) -> Result<Value> {
    let url = format!(
        "{}/{}",
        relay.trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    let builder =
        reqwest::blocking::Client::builder().timeout(std::time::Duration::from_secs(5));
    let builder = if c2_http::relay_use_proxy() {
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
        .json::<Value>()
        .map_err(|e| anyhow!("invalid JSON from {url}: {e}"))
}

fn list_routes(relay: &str) -> Result<()> {
    let value = get_json(relay, "/_routes")?;
    let routes = value
        .get("routes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if routes.is_empty() {
        println!("No routes registered.");
        return Ok(());
    }
    for route in routes {
        if let Some(name) = route.get("name").and_then(Value::as_str) {
            println!("{name}");
        }
    }
    Ok(())
}

fn resolve(relay: &str, name: &str) -> Result<()> {
    let value = get_json(relay, &resolve_path(name))?;
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}

fn resolve_path(name: &str) -> String {
    let name = utf8_percent_encode(name, NON_ALPHANUMERIC).to_string();
    format!("/_resolve/{name}")
}

fn peers(relay: &str) -> Result<()> {
    let value = get_json(relay, "/_peers")?;
    let peers = value.as_array().cloned().unwrap_or_default();
    if peers.is_empty() {
        println!("No peers known.");
        return Ok(());
    }
    for peer in peers {
        let relay_id = peer.get("relay_id").and_then(Value::as_str).unwrap_or("?");
        let url = peer.get("url").and_then(Value::as_str).unwrap_or("?");
        let status = peer
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        println!("{relay_id} ({url}) - {status}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_path_percent_encodes_route_name() {
        assert_eq!(resolve_path("grid/a b"), "/_resolve/grid%2Fa%20b");
    }
}
