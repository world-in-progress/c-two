//! Relay server configuration.

use std::time::Duration;

/// Configuration for the relay server.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// HTTP bind address (e.g. "0.0.0.0:8080").
    pub bind: String,
    /// Stable relay identifier. Default: "{hostname}_{pid}_{uuid8}".
    pub relay_id: String,
    /// Publicly reachable URL of this relay (e.g. "http://node1:8080").
    /// Distinct from `bind` — bind may be 0.0.0.0 but advertise must be routable.
    pub advertise_url: String,
    /// Seed relay URLs for mesh bootstrap. Empty = standalone mode.
    pub seeds: Vec<String>,
    /// Upstream idle-connection timeout. 0 = disabled.
    pub idle_timeout_secs: u64,
    /// Anti-entropy digest exchange interval. 0 = disabled.
    pub anti_entropy_interval: Duration,
    /// Heartbeat interval for peer liveness. 0 = disabled.
    pub heartbeat_interval: Duration,
    /// Consecutive heartbeat misses before marking peer Dead.
    pub heartbeat_miss_threshold: u32,
    /// Dead-peer health probe interval.
    pub dead_peer_probe_interval: Duration,
    /// Seed retry interval when no peers available.
    pub seed_retry_interval: Duration,
    /// Skip IPC validation in `/_register`. For testing only.
    pub skip_ipc_validation: bool,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8080".into(),
            relay_id: Self::generate_relay_id(),
            advertise_url: String::new(),
            seeds: Vec::new(),
            idle_timeout_secs: 0,
            anti_entropy_interval: Duration::from_secs(60),
            heartbeat_interval: Duration::from_secs(5),
            heartbeat_miss_threshold: 3,
            dead_peer_probe_interval: Duration::from_secs(30),
            seed_retry_interval: Duration::from_secs(10),
            skip_ipc_validation: false,
        }
    }
}

impl RelayConfig {
    pub fn generate_relay_id() -> String {
        let host = gethostname::gethostname()
            .to_string_lossy()
            .to_string();
        let pid = std::process::id();
        let uuid = uuid::Uuid::new_v4();
        format!("{host}_{pid}_{}", &uuid.to_string()[..8])
    }

    /// Derive advertise_url from bind if not explicitly set.
    pub fn effective_advertise_url(&self) -> String {
        if !self.advertise_url.is_empty() {
            return self.advertise_url.clone();
        }
        let bind = &self.bind;
        if let Some((host, port)) = bind.rsplit_once(':') {
            let host = if host == "0.0.0.0" || host == "::" { "127.0.0.1" } else { host };
            format!("http://{host}:{port}")
        } else {
            format!("http://127.0.0.1:{bind}")
        }
    }
}
