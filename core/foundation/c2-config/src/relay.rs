//! Relay server configuration.

use crate::{IrohSeedAddr, RelayBridgeSpec, bridge::valid_token};
use std::fmt;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelayEdgeTlsMode {
    ExplicitCert,
    AutoSelfSigned,
    Plaintext,
}

/// Configuration for the relay server.
#[derive(Clone)]
pub struct RelayConfig {
    /// Capacity shared by this relay's data plane.
    pub domain_limits: crate::DomainLimits,
    /// Loopback control listen address.
    pub bind: String,
    /// Optional business and discovery listener.
    pub edge_bind: Option<String>,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub tls_auto: bool,
    pub tls_san: Vec<String>,
    pub state_dir: Option<PathBuf>,
    pub insecure_plaintext: bool,
    /// Stable relay identifier. Default: "{hostname}_{pid}_{uuid8}".
    pub relay_id: String,
    /// A configured identity takes precedence over the persistent endpoint identity.
    pub relay_id_explicit: bool,
    pub bridge: Option<String>,
    pub bridge_token: Option<String>,
    pub cluster_secret: Option<String>,
    /// Publicly reachable URL of this relay (e.g. "http://node1:8080").
    /// May name an external reverse proxy.
    pub advertise_url: String,
    /// Seed relay URLs for mesh bootstrap. Empty = standalone mode.
    pub seeds: Vec<String>,
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
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            domain_limits: crate::DomainLimits::default(),
            bind: "127.0.0.1:8080".into(),
            edge_bind: None,
            tls_cert: None,
            tls_key: None,
            tls_auto: false,
            tls_san: Vec::new(),
            state_dir: None,
            insecure_plaintext: false,
            relay_id: Self::generate_relay_id(),
            relay_id_explicit: false,
            bridge: None,
            bridge_token: None,
            cluster_secret: None,
            advertise_url: String::new(),
            seeds: Vec::new(),
            heartbeat_interval: Duration::from_secs(5),
            heartbeat_miss_threshold: 3,
            dead_peer_probe_interval: Duration::from_secs(30),
            seed_retry_interval: Duration::from_secs(10),
            use_proxy: false,
        }
    }
}

impl fmt::Debug for RelayConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RelayConfig")
            .field("bind", &self.bind)
            .field("edge_bind", &self.edge_bind)
            .field("relay_id", &self.relay_id)
            .field("advertise_url", &self.advertise_url)
            .field("seeds", &self.seeds)
            .field("iroh_enabled", &self.iroh_enabled())
            .finish_non_exhaustive()
    }
}

impl RelayConfig {
    pub fn iroh_enabled(&self) -> bool {
        self.bridge.is_some()
            || self.cluster_secret.is_some()
            || self.seeds.iter().any(|seed| seed.starts_with("iroh://"))
    }

    pub fn derive_iroh_relay_id(endpoint_id: &[u8; 32]) -> String {
        format!("iroh-{}", hex::encode(&endpoint_id[..8]))
    }

    fn validate_iroh(&self) -> Result<Option<RelayBridgeSpec>, String> {
        let bridge = self
            .bridge
            .as_deref()
            .map(RelayBridgeSpec::parse)
            .transpose()?;
        if bridge.is_some() {
            if !self.bridge_token.as_deref().is_some_and(valid_token) {
                return Err(
                    "--bridge requires --bridge-token with printable ASCII and no whitespace"
                        .into(),
                );
            }
        } else if self.bridge_token.is_some() {
            return Err("--bridge-token requires --bridge".into());
        }
        if self.iroh_enabled()
            && self
                .cluster_secret
                .as_deref()
                .is_none_or(|s| s.trim().is_empty())
        {
            return Err("iroh requires a nonempty --cluster-secret".into());
        }
        if self.bridge_token.is_some() && self.bridge_token == self.cluster_secret {
            return Err("--cluster-secret and --bridge-token must differ".into());
        }
        if self.iroh_enabled() {
            for seed in &self.seeds {
                if !seed.starts_with("iroh://") {
                    return Err("iroh relays require iroh:// --seeds".into());
                }
                if IrohSeedAddr::parse(seed)?.direct_addr.is_none() && self.bridge.is_none() {
                    return Err(
                        "iroh seeds without --bridge require a direct @<SocketAddr>".into(),
                    );
                }
            }
        }
        Ok(bridge)
    }

    pub fn validate(&self) -> Result<(), String> {
        self.validate_with_bridge_spec().map(|_| ())
    }

    /// Validate startup settings and retain the parsed bridge URL for the endpoint.
    pub fn validate_with_bridge_spec(&self) -> Result<Option<RelayBridgeSpec>, String> {
        crate::validate_relay_id(&self.relay_id)?;
        if !listener_address(&self.bind).is_some_and(|addr| addr.ip().is_loopback()) {
            return Err(
                "--bind requires a loopback address and port; use --edge-bind for external traffic"
                    .into(),
            );
        }
        let bridge = self.validate_iroh()?;
        self.edge_tls_mode()?;
        if !self.advertise_url.is_empty() {
            let valid = url::Url::parse(&self.advertise_url)
                .ok()
                .is_some_and(|url| {
                    matches!(url.scheme(), "http" | "https")
                        && url.query().is_none()
                        && url.fragment().is_none()
                        && url.username().is_empty()
                        && url.password().is_none()
                        && url.host().is_some_and(|host| match host {
                            url::Host::Ipv4(ip) => !ip.is_unspecified(),
                            url::Host::Ipv6(ip) => !ip.is_unspecified(),
                            url::Host::Domain(_) => true,
                        })
                });
            if !valid {
                return Err("advertise_url must be an http(s) URL with a dialable host".into());
            }
        }
        self.domain_limits
            .validate()
            .map_err(|error| error.to_string())?;
        Ok(bridge)
    }

    pub fn generate_relay_id() -> String {
        let host = gethostname::gethostname().to_string_lossy().to_string();
        let pid = std::process::id();
        let uuid = uuid::Uuid::new_v4();
        Self::build_generated_relay_id(&host, pid, &uuid.to_string()[..8])
    }

    fn build_generated_relay_id(host: &str, pid: u32, suffix: &str) -> String {
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

    /// Resolve the edge transport after validating mutually exclusive options.
    pub fn edge_tls_mode(&self) -> Result<Option<RelayEdgeTlsMode>, String> {
        use RelayEdgeTlsMode::*;
        let has_cert = self.tls_cert.is_some();
        if has_cert != self.tls_key.is_some() {
            return Err("--tls-cert and --tls-key must be set together".into());
        }
        if [has_cert, self.tls_auto, self.insecure_plaintext]
            .into_iter()
            .filter(|enabled| *enabled)
            .count()
            > 1
        {
            return Err(
                "--tls-cert/--tls-key, --tls-auto and --insecure-plaintext are mutually exclusive"
                    .into(),
            );
        }
        let Some(bind) = &self.edge_bind else {
            if has_cert
                || self.tls_auto
                || self.insecure_plaintext
                || !self.tls_san.is_empty()
                || (self.state_dir.is_some() && !self.iroh_enabled())
            {
                return Err("edge TLS settings require --edge-bind".into());
            }
            return Ok(None);
        };
        let addr = listener_address(bind).ok_or("invalid --edge-bind address or port")?;
        let mode = if has_cert {
            ExplicitCert
        } else if self.tls_auto || (!addr.ip().is_loopback() && !self.insecure_plaintext) {
            AutoSelfSigned
        } else {
            Plaintext
        };
        Ok(Some(mode))
    }

    pub fn local_bind_addr(&self) -> Option<SocketAddr> {
        listener_address(&self.bind)
    }

    pub fn edge_bind_addr(&self) -> Option<SocketAddr> {
        self.edge_bind.as_deref().and_then(listener_address)
    }

    /// An explicit external URL can name the front of a reverse proxy.
    pub fn effective_advertise_url(&self) -> String {
        if !self.advertise_url.is_empty() {
            return self.advertise_url.clone();
        }
        let Some(mut addr) = self.edge_bind_addr() else {
            return format!("http://{}", self.bind);
        };
        if addr.ip().is_unspecified() {
            // A wildcard socket is reachable through the matching loopback.
            addr.set_ip(if addr.is_ipv4() {
                IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
            } else {
                IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
            });
        }
        let scheme = match self.edge_tls_mode() {
            Ok(Some(RelayEdgeTlsMode::ExplicitCert | RelayEdgeTlsMode::AutoSelfSigned)) => "https",
            _ => "http",
        };
        format!("{scheme}://{addr}")
    }

    /// Base URL used by HTTP mesh peers.
    pub fn mesh_url(&self) -> String {
        if self.edge_bind.is_none() {
            self.effective_advertise_url()
        } else {
            format!("http://{}", self.bind)
        }
    }
}

fn listener_address(value: &str) -> Option<SocketAddr> {
    value.parse().ok().or_else(|| {
        let (host, port) = value.rsplit_once(':')?;
        if !host.eq_ignore_ascii_case("localhost") {
            return None;
        }
        Some(SocketAddr::from((
            [127, 0, 0, 1],
            port.parse::<u16>().ok()?,
        )))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn iroh_relay_rejects_http_seeds() {
        let config = RelayConfig {
            cluster_secret: Some("mesh-secret".into()),
            seeds: vec!["http://127.0.0.1:8081".into()],
            ..RelayConfig::default()
        };

        let err = config
            .validate()
            .expect_err("iroh relay should reject http:// seeds");
        assert!(err.contains("iroh://"));
    }
}
