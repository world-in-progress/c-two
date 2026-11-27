//! TLS identity and acceptor for the relay edge listener.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use c2_config::{RelayConfig, RelayEdgeTlsMode};
use c2_local_security::{create_private_dir, verify_private_file, write_private_file};
use rustls::ServerConfig;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
use sha2::{Digest, Sha256};
use tokio_rustls::TlsAcceptor;

pub(super) struct EdgeTls {
    pub(super) acceptor: TlsAcceptor,
    pub(super) leaf_sha256: [u8; 32],
}

impl EdgeTls {
    pub(super) fn load(config: &RelayConfig) -> Result<Option<Self>, String> {
        let (cert_path, key_path) = match config.edge_tls_mode()? {
            Some(RelayEdgeTlsMode::ExplicitCert) => (
                config.tls_cert.clone().expect("validated TLS certificate"),
                config.tls_key.clone().expect("validated TLS key"),
            ),
            Some(RelayEdgeTlsMode::AutoSelfSigned) => persistent_identity(config)?,
            Some(RelayEdgeTlsMode::Plaintext) | None => return Ok(None),
        };

        let certs = CertificateDer::pem_file_iter(&cert_path)
            .and_then(Iterator::collect::<Result<Vec<_>, _>>)
            .map_err(|error| format!("read edge certificate {}: {error}", cert_path.display()))?;
        let leaf = certs.first().ok_or("edge certificate chain is empty")?;
        let leaf_sha256 = Sha256::digest(leaf.as_ref()).into();
        let key = PrivateKeyDer::from_pem_file(&key_path)
            .map_err(|error| format!("read edge private key {}: {error}", key_path.display()))?;

        let mut tls =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .map_err(|error| error.to_string())?
                .with_no_client_auth()
                .with_single_cert(certs, key)
                .map_err(|error| format!("invalid edge TLS identity: {error}"))?;
        tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        Ok(Some(Self {
            acceptor: TlsAcceptor::from(Arc::new(tls)),
            leaf_sha256,
        }))
    }
}

fn persistent_identity(config: &RelayConfig) -> Result<(PathBuf, PathBuf), String> {
    let dir = match &config.state_dir {
        Some(dir) => dir.clone(),
        None => std::env::home_dir()
            .ok_or("cannot find the home directory for edge TLS; specify --state-dir")?
            .join(".c3-relay"),
    };
    create_private_dir(&dir).map_err(|e| file_error(&dir, e))?;
    let cert = dir.join("edge-cert.pem");
    let key = dir.join("edge-key.pem");
    match (private_file_exists(&cert)?, private_file_exists(&key)?) {
        (true, true) => {}
        (false, false) => {
            let names = edge_subject_alt_names(config);
            // rcgen's default key algorithm is ECDSA P-256 with SHA-256.
            let identity = rcgen::generate_simple_self_signed(names)
                .map_err(|error| format!("generate edge TLS identity: {error}"))?;
            write_private_file(&key, identity.signing_key.serialize_pem().as_bytes())
                .map_err(|e| file_error(&key, e))?;
            if let Err(error) = write_private_file(&cert, identity.cert.pem().as_bytes()) {
                let _ = fs::remove_file(&key);
                return Err(file_error(&cert, error));
            }
            #[cfg(unix)]
            fs::File::open(&dir)
                .and_then(|dir| dir.sync_all())
                .map_err(|e| file_error(&dir, e))?;
        }
        _ => return Err(format!("incomplete edge TLS identity in {}", dir.display())),
    }
    Ok((cert, key))
}

fn private_file_exists(path: &Path) -> Result<bool, String> {
    match verify_private_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(file_error(path, error)),
    }
}

fn file_error(path: &Path, error: io::Error) -> String {
    format!("edge TLS state {}: {error}", path.display())
}

fn edge_subject_alt_names(config: &RelayConfig) -> Vec<String> {
    let mut names = BTreeSet::from(["127.0.0.1".into(), "::1".into(), "localhost".into()]);
    names.extend(config.tls_san.iter().cloned());
    if let Ok(url) = url::Url::parse(&config.advertise_url)
        && let Some(host) = url.host()
    {
        // URL formatting brackets IPv6 literals. rcgen expects the raw IP.
        names.insert(match host {
            url::Host::Domain(name) => name.to_owned(),
            url::Host::Ipv4(ip) => ip.to_string(),
            url::Host::Ipv6(ip) => ip.to_string(),
        });
    }
    if let Some(bind) = config.edge_bind_addr()
        && !bind.ip().is_unspecified()
    {
        names.insert(bind.ip().to_string());
    }
    names.into_iter().collect()
}
