//! Private iroh relay for address discovery and encrypted packet forwarding.

use std::{
    collections::BTreeSet,
    net::{Ipv4Addr, SocketAddr},
    sync::Arc,
};

use c2_config::{BridgeConfig, DEFAULT_BRIDGE_QUIC_PORT};
use iroh_relay::server::{
    Access, AccessControl, CertConfig, ClientRequest, QuicConfig, RelayConfig, Server,
    ServerConfig, TlsConfig,
};
use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use sha2::{Digest, Sha256};

use crate::private_state::{
    constant_time_eq, persistent_hex32, persistent_tls_identity, state_dir,
};

struct BridgeAccessControl {
    token_digest: [u8; 32],
}

impl std::fmt::Debug for BridgeAccessControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BridgeAccessControl")
            .finish_non_exhaustive()
    }
}

impl AccessControl for BridgeAccessControl {
    async fn on_connect(&self, request: &ClientRequest) -> Access {
        if request.auth_token().is_some_and(|token| {
            constant_time_eq(&Sha256::digest(token.as_bytes()).into(), &self.token_digest)
        }) {
            Access::Allow
        } else {
            Access::Deny {
                reason: Some("unauthorized bridge token".into()),
            }
        }
    }
}

pub struct BridgeServer {
    server: Server,
    leaf_sha256: [u8; 32],
    token: String,
    self_signed: bool,
}

impl BridgeServer {
    pub async fn start(config: BridgeConfig) -> Result<Self, String> {
        let (bind, quic_bind) = config.validated_listen_addresses()?;
        let self_signed = config.tls_cert.is_none();
        let state = (self_signed || config.token.is_none())
            .then(|| state_dir(config.state_dir.as_deref(), ".c3-bridge"))
            .transpose()?;
        let (cert_path, key_path) = if let (Some(cert), Some(key)) =
            (config.tls_cert, config.tls_key)
        {
            (cert, key)
        } else {
            let mut names = BTreeSet::from(["localhost".into(), "127.0.0.1".into(), "::1".into()]);
            if !bind.ip().is_unspecified() {
                names.insert(bind.ip().to_string());
            }
            persistent_tls_identity(
                state.as_deref().expect("automatic certificate state"),
                "bridge",
                names.into_iter().collect(),
            )?
        };
        let token = match config.token {
            Some(token) => token,
            None => hex::encode(persistent_hex32(
                state.as_deref().expect("automatic token state"),
                "bridge-token",
            )?),
        };

        let certs = CertificateDer::pem_file_iter(&cert_path)
            .and_then(Iterator::collect::<Result<Vec<_>, _>>)
            .map_err(|error| format!("read bridge certificate {}: {error}", cert_path.display()))?;
        let leaf = certs.first().ok_or("bridge certificate chain is empty")?;
        let leaf_sha256 = Sha256::digest(leaf.as_ref()).into();
        let key = PrivateKeyDer::from_pem_file(&key_path)
            .map_err(|error| format!("read bridge private key {}: {error}", key_path.display()))?;
        let server_config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|error| error.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|error| format!("invalid bridge TLS identity: {error}"))?;

        let mut relay = RelayConfig::new((Ipv4Addr::LOCALHOST, 0));
        relay.tls = Some(TlsConfig::new(bind, CertConfig::Manual { server_config }));
        relay.access = Arc::new(BridgeAccessControl {
            token_digest: Sha256::digest(token.as_bytes()).into(),
        });
        let mut cfg = ServerConfig::default();
        cfg.relay = Some(relay);
        cfg.quic = quic_bind.map(QuicConfig::new);
        let server = Server::spawn(cfg)
            .await
            .map_err(|error| error.to_string())?;
        Ok(Self {
            server,
            leaf_sha256,
            token,
            self_signed,
        })
    }

    pub fn https_addr(&self) -> SocketAddr {
        self.server.https_addr().expect("HTTPS bridge listener")
    }

    pub fn quic_addr(&self) -> Option<SocketAddr> {
        self.server.quic_addr()
    }

    pub fn leaf_sha256(&self) -> [u8; 32] {
        self.leaf_sha256
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn bridge_url_template(&self) -> String {
        let addr = self.https_addr();
        let mut url = if addr.ip().is_unspecified() {
            format!("https://<HOST>:{}", addr.port())
        } else {
            format!("https://{addr}")
        };
        if self.self_signed {
            url.push_str(&format!("?pin={}", hex::encode(self.leaf_sha256)));
        }
        let quic = match self.quic_addr().map(|addr| addr.port()) {
            Some(DEFAULT_BRIDGE_QUIC_PORT) => return url,
            Some(port) => port.to_string(),
            None => "off".into(),
        };
        url.push(if self.self_signed { '&' } else { '?' });
        url.push_str(&format!("quic={quic}"));
        url
    }

    pub async fn wait(&mut self) -> Result<(), String> {
        self.server
            .join()
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())
    }

    pub async fn stop(self) -> Result<(), String> {
        self.server
            .shutdown()
            .await
            .map_err(|error| error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use c2_config::{RelayBridgeSpec, RelayTlsTrust};
    use futures::StreamExt;
    use iroh::{Endpoint, RelayMode, Watcher, endpoint::presets};
    use iroh_relay::tls::CaTlsConfig;
    use std::time::Duration;
    use tokio::time::timeout;

    fn bridge_config(parent: &std::path::Path) -> BridgeConfig {
        BridgeConfig {
            bind: "127.0.0.1:0".into(),
            quic_bind: None,
            state_dir: Some(parent.join("bridge")),
            ..BridgeConfig::default()
        }
    }

    #[tokio::test]
    async fn bridge_persists_identity_and_starts_without_quic() {
        let parent = tempfile::tempdir().unwrap();
        let config = bridge_config(parent.path());
        let bridge = BridgeServer::start(config.clone()).await.unwrap();
        let expected_pin = bridge.leaf_sha256();
        let token = bridge.token().to_owned();
        assert_eq!(token.len(), 64);
        assert!(
            token
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
        assert_eq!(bridge.quic_addr(), None);
        assert!(bridge.server.http_addr().unwrap().ip().is_loopback());
        bridge.stop().await.unwrap();

        let bridge = BridgeServer::start(config).await.unwrap();
        assert_eq!(bridge.leaf_sha256(), expected_pin);
        assert_eq!(bridge.token(), token);
        let spec = RelayBridgeSpec::parse(&bridge.bridge_url_template()).unwrap();
        assert_eq!(spec.cert_sha256(), Some(expected_pin));
        assert_eq!(spec.quic_port(), None);
        bridge.stop().await.unwrap();
    }

    #[tokio::test]
    async fn bridge_accepts_only_its_token_over_pinned_tls() {
        let parent = tempfile::tempdir().unwrap();
        let bridge = BridgeServer::start(bridge_config(parent.path()))
            .await
            .unwrap();
        let spec = RelayBridgeSpec::parse(&bridge.bridge_url_template()).unwrap();
        let expected_pin = spec.cert_sha256().unwrap();
        let token = bridge.token().to_owned();
        for candidate in [None, Some("wrong-token"), Some(token.as_str())] {
            let mut relay = iroh::RelayConfig::new(spec.base_url().parse().unwrap(), None);
            if let Some(candidate) = candidate {
                relay = relay.with_auth_token(candidate);
            }
            let ep = Endpoint::builder(presets::Minimal)
                .relay_mode(RelayMode::Custom(relay.into()))
                .ca_tls_config(CaTlsConfig::custom_server_cert_verifier(Arc::new(
                    move |provider| {
                        crate::tls::relay_server_cert_verifier(
                            &RelayTlsTrust::CertSha256(expected_pin),
                            provider,
                        )
                        .map_err(std::io::Error::other)
                    },
                )))
                .bind()
                .await
                .unwrap();
            if candidate == Some(token.as_str()) {
                timeout(Duration::from_secs(10), ep.online()).await.unwrap();
            } else {
                let mut status = ep.home_relay_status().stream();
                timeout(Duration::from_secs(10), async {
                    while let Some(relays) = status.next().await {
                        assert!(!relays.iter().any(|relay| relay.is_connected()));
                        if relays.iter().any(|relay| {
                            relay.auth_denied_reason() == Some("unauthorized bridge token")
                        }) {
                            return;
                        }
                    }
                    panic!("relay status ended before token denial");
                })
                .await
                .unwrap();
            }
            ep.close().await;
        }
        bridge.stop().await.unwrap();
    }
}
