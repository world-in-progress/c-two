//! Relay configuration.

/// Configuration for the relay server.
#[derive(Debug, Clone)]
pub struct RelayConfig {
    /// HTTP bind address (e.g. "0.0.0.0:8080").
    pub bind: String,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8080".into(),
        }
    }
}
