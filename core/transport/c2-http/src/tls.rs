use std::sync::Arc;

use c2_config::{RelayTlsTrust, ResolvedRelayClientConfig};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime, pem::PemObject};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, Error, RootCertStore, SignatureScheme,
};
use sha2::{Digest, Sha256};

use crate::client::HttpError;

/// Build the shared TLS configuration for asynchronous and blocking relay clients.
pub fn relay_tls_config(trust: &RelayTlsTrust) -> Result<ClientConfig, HttpError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier = relay_server_cert_verifier(trust, provider.clone())?;
    let mut config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(config)
}

pub(crate) fn relay_server_cert_verifier(
    trust: &RelayTlsTrust,
    provider: Arc<rustls::crypto::CryptoProvider>,
) -> Result<Arc<dyn ServerCertVerifier>, HttpError> {
    let roots = match trust {
        RelayTlsTrust::SystemDefault => RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        },
        RelayTlsTrust::CustomCaCert(path) => {
            let ca_error = |error: &dyn std::fmt::Display| {
                HttpError::InvalidInput(format!(
                    "invalid relay CA file {}: {error}",
                    path.display()
                ))
            };
            let mut roots = RootCertStore::empty();
            for cert in CertificateDer::pem_file_iter(path).map_err(|error| ca_error(&error))? {
                roots
                    .add(cert.map_err(|error| ca_error(&error))?)
                    .map_err(|error| ca_error(&error))?;
            }
            if roots.is_empty() {
                return Err(ca_error(&"no PEM certificates found"));
            }
            roots
        }
        RelayTlsTrust::CertSha256(digest) => {
            return Ok(Arc::new(LeafDerPinVerifier {
                digest: *digest,
                algorithms: provider.signature_verification_algorithms,
            }));
        }
    };
    let verifier =
        rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider)
            .build()
            .map_err(|error| HttpError::InvalidInput(error.to_string()))?;
    Ok(verifier)
}

/// Build an HTTP client with the resolved relay trust and proxy policy.
pub fn relay_client_builder(
    policy: &ResolvedRelayClientConfig,
) -> Result<reqwest::ClientBuilder, HttpError> {
    Ok(relay_client_builder_with_tls(
        policy.use_proxy,
        relay_tls_config(&policy.tls_trust)?,
    ))
}

pub(crate) fn relay_client_builder_with_tls(
    use_proxy: bool,
    tls: ClientConfig,
) -> reqwest::ClientBuilder {
    let builder = reqwest::Client::builder()
        .pool_idle_timeout(c2_config::IDLE_CONNECTION_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .use_preconfigured_tls(tls);
    if use_proxy {
        builder
    } else {
        builder.no_proxy()
    }
}

#[derive(Debug)]
struct LeafDerPinVerifier {
    digest: [u8; 32],
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for LeafDerPinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let digest: [u8; 32] = Sha256::digest(end_entity.as_ref()).into();
        if digest != self.digest {
            return Err(Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(message, cert, signature, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, signature, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}
