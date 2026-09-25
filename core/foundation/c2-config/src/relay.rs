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
    /// Whether relay HTTP clients should honor system HTTP proxy variables.
    pub use_proxy: bool,
    /// Target maximum payload chunk size for remote transport streams.
    ///
    /// This controls C-Two's protocol-independent remote payload batching. It
    /// does not guarantee concrete TCP packets, HTTP/1 chunks, or HTTP/2 DATA
    /// frames have the same size.
    pub remote_payload_chunk_size: u64,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8080".into(),
            relay_id: Self::generate_relay_id(),
            advertise_url: String::new(),
            seeds: Vec::new(),
            idle_timeout_secs: 60,
            anti_entropy_interval: Duration::from_secs(60),
            heartbeat_interval: Duration::from_secs(5),
            heartbeat_miss_threshold: 3,
            dead_peer_probe_interval: Duration::from_secs(30),
            seed_retry_interval: Duration::from_secs(10),
            use_proxy: false,
            remote_payload_chunk_size: crate::DEFAULT_REMOTE_PAYLOAD_CHUNK_SIZE,
        }
    }
}

impl RelayConfig {
    pub fn validate(&self) -> Result<(), String> {
        crate::validate_relay_id(&self.relay_id)?;
        if self.idle_timeout_secs != 0 && self.idle_timeout_secs.checked_mul(1000).is_none() {
            return Err("idle_timeout_secs must fit in milliseconds".into());
        }
        crate::validate_remote_payload_chunk_size(self.remote_payload_chunk_size)
            .map_err(|reason| format!("remote_payload_chunk_size {reason}"))?;
        Ok(())
    }

    pub fn generate_relay_id() -> String {
        let host = gethostname::gethostname().to_string_lossy().to_string();
        let pid = std::process::id();
        let uuid = uuid::Uuid::new_v4();
        Self::build_generated_relay_id(&host, pid, &uuid.to_string()[..8])
    }

    fn build_generated_relay_id(host: &str, pid: u32, suffix: &str) -> String {
        let suffix = suffix
            .trim()
            .chars()
            .map(|ch| {
                if ch.is_control() || ch == '/' || ch == '\\' {
                    '_'
                } else {
                    ch
                }
            })
            .collect::<String>();
        let suffix = if suffix.is_empty() || suffix == "." || suffix == ".." {
            "00000000".to_string()
        } else {
            suffix
        };
        let tail = format!("_{pid}_{suffix}");

        let mut host = host
            .trim()
            .chars()
            .map(|ch| {
                if ch.is_control() || ch == '/' || ch == '\\' {
                    '_'
                } else {
                    ch
                }
            })
            .collect::<String>();
        if host.is_empty() || host == "." || host == ".." {
            host = "relay".to_string();
        }

        let max_host_bytes = 255usize.saturating_sub(tail.len()).max(1);
        if host.len() > max_host_bytes {
            let mut end = max_host_bytes;
            while !host.is_char_boundary(end) {
                end -= 1;
            }
            host.truncate(end);
            host = host.trim().to_string();
            if host.is_empty() || host == "." || host == ".." {
                host = "relay".to_string();
            }
        }

        format!("{host}{tail}")
    }

    /// Derive advertise_url from bind if not explicitly set.
    pub fn effective_advertise_url(&self) -> String {
        if !self.advertise_url.is_empty() {
            return self.advertise_url.clone();
        }
        let bind = &self.bind;
        if let Some((host, port)) = bind.rsplit_once(':') {
            let host = if host == "0.0.0.0" || host == "::" {
                "127.0.0.1"
            } else {
                host
            };
            format!("http://{host}:{port}")
        } else {
            format!("http://127.0.0.1:{bind}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_timeout_must_fit_milliseconds() {
        let config = RelayConfig {
            idle_timeout_secs: u64::MAX,
            ..RelayConfig::default()
        };

        let err = config
            .validate()
            .expect_err("idle timeout should reject millisecond overflow");

        assert!(err.contains("idle_timeout_secs"));
        assert!(err.contains("milliseconds"));
    }

    #[test]
    fn relay_config_rejects_invalid_relay_id() {
        let config = RelayConfig {
            relay_id: "bad\nrelay".to_string(),
            ..RelayConfig::default()
        };

        let err = config
            .validate()
            .expect_err("relay config should reject invalid relay_id");

        assert!(err.contains("relay_id"));
    }

    #[test]
    fn relay_config_rejects_invalid_remote_payload_chunk_size() {
        let config = RelayConfig {
            remote_payload_chunk_size: 0,
            ..RelayConfig::default()
        };

        let err = config
            .validate()
            .expect_err("zero remote chunk size should fail");

        assert!(err.contains("remote_payload_chunk_size"));
        assert!(err.contains("> 0"));
    }

    #[test]
    fn generated_relay_id_truncates_long_hostname_to_valid_id() {
        let host = "h".repeat(300);
        let relay_id = RelayConfig::build_generated_relay_id(&host, 1234, "abcd1234");

        assert!(relay_id.ends_with("_1234_abcd1234"));
        assert!(relay_id.len() <= 255);
        crate::validate_relay_id(&relay_id).unwrap();
    }

    #[test]
    fn generated_relay_id_sanitizes_invalid_hostname_characters() {
        let relay_id = RelayConfig::build_generated_relay_id("bad/host\n", 1234, "abcd1234");

        assert!(!relay_id.contains('/'));
        assert!(!relay_id.contains('\n'));
        crate::validate_relay_id(&relay_id).unwrap();
    }
}
