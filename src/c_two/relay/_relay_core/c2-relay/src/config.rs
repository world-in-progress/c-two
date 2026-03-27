//! Relay configuration.

/// Configuration for the relay server.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// HTTP bind address (e.g. "0.0.0.0:8080").
    pub bind: String,
    /// Upstream IPC v3 address (e.g. "ipc-v3://my_server").
    pub upstream: String,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8080".into(),
            upstream: "ipc-v3://default".into(),
        }
    }
}
