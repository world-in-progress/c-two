//! Configuration for a private iroh bridge and its relay clients.

use std::{fmt, net::SocketAddr, path::PathBuf};

pub const DEFAULT_BRIDGE_QUIC_PORT: u16 = 7842;

#[derive(Clone)]
pub struct BridgeConfig {
    pub bind: String,
    pub quic_bind: Option<String>,
    pub tls_cert: Option<PathBuf>,
    pub tls_key: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
    pub token: Option<String>,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8443".into(),
            quic_bind: Some(format!("0.0.0.0:{DEFAULT_BRIDGE_QUIC_PORT}")),
            tls_cert: None,
            tls_key: None,
            state_dir: None,
            token: None,
        }
    }
}

impl fmt::Debug for BridgeConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BridgeConfig")
            .field("bind", &self.bind)
            .field("quic_bind", &self.quic_bind)
            .field("tls_cert", &self.tls_cert)
            .field("tls_key", &self.tls_key)
            .field("state_dir", &self.state_dir)
            .field("token", &self.token.as_ref().map_or("auto", |_| "explicit"))
            .finish()
    }
}

impl BridgeConfig {
    pub fn validate(&self) -> Result<(), String> {
        self.validated_listen_addresses().map(|_| ())
    }

    pub fn validated_listen_addresses(&self) -> Result<(SocketAddr, Option<SocketAddr>), String> {
        if self.tls_cert.is_some() != self.tls_key.is_some() {
            return Err("--tls-cert and --tls-key must be set together".into());
        }
        if self
            .token
            .as_deref()
            .is_some_and(|value| !valid_token(value))
        {
            return Err("--token must contain printable ASCII without whitespace".into());
        }
        let bind = self
            .bind
            .parse()
            .map_err(|_| "invalid bridge --bind address or port")?;
        let quic_bind = self
            .quic_bind
            .as_deref()
            .map(str::parse)
            .transpose()
            .map_err(|_| "invalid bridge --quic-bind address or port")?;
        Ok((bind, quic_bind))
    }
}

pub(crate) fn valid_token(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_graphic())
}

fn lower_hex_32(value: &str) -> Option<[u8; 32]> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let mut bytes = [0; 32];
    hex::decode_to_slice(value, &mut bytes).ok()?;
    Some(bytes)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayBridgeSpec {
    base_url: String,
    cert_sha256: Option<[u8; 32]>,
    quic_port: Option<u16>,
}

impl RelayBridgeSpec {
    pub fn parse(value: &str) -> Result<Self, String> {
        let invalid = || {
            "--bridge requires https://host[:port][?pin=<64-lower-hex>&quic=<port|off>]".to_owned()
        };
        let rest = value.strip_prefix("https://").ok_or_else(invalid)?;
        if value.contains(['#', '\\']) || value.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(invalid());
        }
        let (base, query) = rest
            .split_once('?')
            .map_or((rest, None), |(base, query)| (base, Some(query)));
        let authority = base.strip_suffix('/').unwrap_or(base);
        if authority.is_empty() || authority.contains(['/', '@', '*']) || authority.ends_with(':') {
            return Err(invalid());
        }
        let url = url::Url::parse(value).map_err(|_| invalid())?;
        let dialable = match url.host() {
            Some(url::Host::Ipv4(ip)) => !ip.is_unspecified(),
            Some(url::Host::Ipv6(ip)) => {
                !ip.is_unspecified()
                    && ip
                        .to_ipv4_mapped()
                        .is_none_or(|mapped| !mapped.is_unspecified())
            }
            Some(url::Host::Domain(host)) => {
                let host = host.strip_suffix('.').unwrap_or(host);
                host.len() <= 253
                    && host.split('.').all(|label| {
                        !label.is_empty()
                            && label.len() <= 63
                            && !label.starts_with('-')
                            && !label.ends_with('-')
                            && label
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
                    })
            }
            None => false,
        };
        if !dialable || url.port() == Some(0) {
            return Err(invalid());
        }

        let mut cert_sha256 = None;
        let mut quic = None;
        if let Some(query) = query {
            for parameter in query.split('&') {
                match parameter.split_once('=') {
                    Some(("pin", pin)) if cert_sha256.is_none() => {
                        cert_sha256 = Some(
                            lower_hex_32(pin)
                                .ok_or("--bridge pin must be 64 lowercase hex digits")?,
                        );
                    }
                    Some(("quic", port)) if quic.is_none() => {
                        quic = Some(match port {
                            "off" => None,
                            port => Some(
                                port.parse::<u16>()
                                    .ok()
                                    .filter(|p| *p != 0 && port.bytes().all(|b| b.is_ascii_digit()))
                                    .ok_or("--bridge quic must be off or a port in 1..=65535")?,
                            ),
                        });
                    }
                    _ => return Err(invalid()),
                }
            }
        }

        Ok(Self {
            base_url: url.origin().ascii_serialization(),
            cert_sha256,
            quic_port: quic.unwrap_or(Some(DEFAULT_BRIDGE_QUIC_PORT)),
        })
    }

    pub fn base_url(&self) -> String {
        self.base_url.clone()
    }

    pub fn cert_sha256(&self) -> Option<[u8; 32]> {
        self.cert_sha256
    }

    pub fn quic_port(&self) -> Option<u16> {
        self.quic_port
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrohSeedAddr {
    pub endpoint_id: [u8; 32],
    pub direct_addr: Option<SocketAddr>,
}

impl IrohSeedAddr {
    pub fn parse(value: &str) -> Result<Self, String> {
        let invalid =
            || "iroh seed requires iroh://<64-lower-hex-endpoint-id>[@<SocketAddr>]".to_owned();
        let value = value.strip_prefix("iroh://").ok_or_else(invalid)?;
        let (id, addr) = value
            .split_once('@')
            .map_or((value, None), |(id, addr)| (id, Some(addr)));
        let endpoint_id = lower_hex_32(id).ok_or_else(invalid)?;
        let direct_addr = addr.map(str::parse).transpose().map_err(|_| invalid())?;
        Ok(Self {
            endpoint_id,
            direct_addr,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_url_keeps_only_normalized_origin_and_exposes_validated_parameters() {
        let hex = "ab".repeat(32);
        let parsed =
            RelayBridgeSpec::parse(&format!("https://[::1]:8443/?pin={hex}&quic=65535")).unwrap();
        assert_eq!(parsed.base_url(), "https://[::1]:8443");
        assert_eq!(parsed.cert_sha256(), Some([0xab; 32]));
        assert_eq!(parsed.quic_port(), Some(65535));
        assert_eq!(
            RelayBridgeSpec::parse("https://example.org")
                .unwrap()
                .quic_port(),
            Some(7842)
        );
        assert_eq!(
            RelayBridgeSpec::parse("https://example.org?quic=off")
                .unwrap()
                .quic_port(),
            None
        );
        for value in [
            "http://example.org",
            "https://",
            "https:///example.org",
            "https://0.0.0.0",
            "https://[::]",
            "https://[::ffff:0.0.0.0]",
            "https://.",
            "https://a..example.org",
            "https://*.example.org",
            "https://example.org:65536",
            "https://example.org:0",
            "https://@example.org",
            "https://user@example.org",
            "https://example.org/#",
            "https://example.org/path/..",
            "https://example.org?pin=",
            "https://example.org?unknown=a",
            "https://example.org?quic=off&quic=1",
            "https://example.org?quic=0",
            "https://example.org?quic=+1",
            "https://example.org?quic=65536",
            "https://example.org?quic=off&",
        ] {
            assert!(RelayBridgeSpec::parse(value).is_err(), "{value}");
        }
        assert!(
            RelayBridgeSpec::parse(&format!("https://example.org?pin={hex}&pin={hex}")).is_err()
        );
        assert!(
            RelayBridgeSpec::parse(&format!("https://example.org?pin={}", hex.to_uppercase()))
                .is_err()
        );
    }

    #[test]
    fn seed_syntax_preserves_binary_identity_and_direct_hint() {
        let hex = "ab".repeat(32);
        let parsed = IrohSeedAddr::parse(&format!("iroh://{hex}@[::1]:1234")).unwrap();
        assert_eq!(parsed.endpoint_id, [0xab; 32]);
        assert_eq!(parsed.direct_addr, Some("[::1]:1234".parse().unwrap()));
        assert_eq!(
            IrohSeedAddr::parse(&format!("iroh://{hex}"))
                .unwrap()
                .direct_addr,
            None
        );
        for value in [
            format!("iroh://{}", hex.to_uppercase()),
            format!("iroh://{hex}@"),
            format!("iroh://{hex}@localhost:9"),
            format!("iroh://{hex}@127.0.0.1:65536"),
            format!("iroh://{hex}/"),
        ] {
            assert!(IrohSeedAddr::parse(&value).is_err(), "{value}");
        }
    }
}
