use super::*;
use crate::client::{RelayAwareHttpClient, RelayRouteInfo};
use bytes::Bytes;
use c2_config::{RelayTlsTrust, ResolvedRelayClientConfig};
use http::Response;
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
use rustls::pki_types::PrivatePkcs8KeyDer;
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};
use tokio_rustls::TlsAcceptor;

const WAIT: Duration = Duration::from_secs(5);

const OUTCOME: &[u8] = br#"{"status":"unknown","method_name":null,"result_available":false,"result_unavailable_reason":null,"error_code":null,"error_details_available":false}"#;

struct H2Relay {
    url: String,
    requests: mpsc::UnboundedReceiver<(String, usize)>,
    task: JoinHandle<()>,
}

impl Drop for H2Relay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl H2Relay {
    async fn start(
        certificate: &rcgen::Certificate,
        key: &KeyPair,
        resolved_url: Option<&str>,
    ) -> Self {
        let provider = rustls::crypto::ring::default_provider();
        let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(provider))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![certificate.der().clone()],
                PrivatePkcs8KeyDer::from(key.serialize_der()).into(),
            )
            .unwrap();
        tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
        let acceptor = TlsAcceptor::from(Arc::new(tls));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("https://{}", listener.local_addr().unwrap());
        let resolved =
            Bytes::from(serde_json::to_vec(&[route_info(resolved_url.unwrap_or(&url))]).unwrap());
        let (tx, requests) = mpsc::unbounded_channel();
        let task = tokio::spawn(async move {
            let mut children = JoinSet::new();
            let mut next_id = 0;
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (stream, _) = accepted.unwrap();
                        let acceptor = acceptor.clone();
                        let requests = tx.clone();
                        let resolved = resolved.clone();
                        next_id += 1;
                        let connection_id = next_id;
                        children.spawn(async move {
                            let Ok(tls) = acceptor.accept(stream).await else { return };
                            assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"h2"[..]));
                            let mut connection = h2::server::Builder::new()
                                .max_concurrent_streams(1)
                                .handshake::<_, Bytes>(tls)
                                .await
                                .unwrap();
                            let mut held = Vec::new();
                            while let Some(Ok((request, mut respond))) = connection.accept().await {
                                let path = request.uri().path();
                                let _ = requests.send((path.to_owned(), connection_id));
                                let response = Response::builder().status(200);
                                let body = match path {
                                    "/_resolve/grid" => resolved.clone(),
                                    crate::OUTCOME_PATH => Bytes::from_static(OUTCOME),
                                    crate::CALL_PATH => {
                                        // Keep the only business stream slot in use.
                                        held.push(respond.send_response(response.body(()).unwrap(), false).unwrap());
                                        continue;
                                    }
                                    _ => Bytes::new(),
                                };
                                let response = response.header("Content-Type", "application/json").body(()).unwrap();
                                let mut stream = respond.send_response(response, body.is_empty()).unwrap();
                                if !body.is_empty() {
                                    stream.send_data(body, true).unwrap();
                                }
                            }
                        });
                    }
                    Some(result) = children.join_next() => result.unwrap(),
                }
            }
        });
        Self {
            url,
            requests,
            task,
        }
    }

    async fn request(&mut self) -> (String, usize) {
        tokio::time::timeout(WAIT, self.requests.recv())
            .await
            .unwrap()
            .unwrap()
    }
}

fn expected_contract() -> ExpectedRouteContract {
    ExpectedRouteContract {
        route_name: "grid".into(),
        crm_ns: "test".into(),
        crm_name: "Grid".into(),
        crm_ver: "1.0.0".into(),
        abi_hash: "a".repeat(64),
        signature_hash: "b".repeat(64),
    }
}

fn route_info(relay_url: &str) -> RelayRouteInfo {
    let expected = expected_contract();
    RelayRouteInfo {
        name: expected.route_name,
        relay_url: Some(relay_url.into()),
        proxy_via_anchor: false,
        ipc_address: None,
        server_id: None,
        server_instance_id: None,
        crm_ns: expected.crm_ns,
        crm_name: expected.crm_name,
        crm_ver: expected.crm_ver,
        abi_hash: expected.abi_hash,
        signature_hash: expected.signature_hash,
        max_payload_size: 4096,
    }
}

fn policy(trust: RelayTlsTrust) -> ResolvedRelayClientConfig {
    ResolvedRelayClientConfig {
        tls_trust: trust,
        ..Default::default()
    }
}

async fn health(url: &str, trust: RelayTlsTrust) -> Result<reqwest::Response, reqwest::Error> {
    crate::relay_client_builder(&policy(trust))
        .unwrap()
        .timeout(WAIT)
        .build()
        .unwrap()
        .get(format!("{url}/health"))
        .send()
        .await
}

#[tokio::test]
async fn private_ca_enables_h2_and_enforces_the_ip_san() {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "C-Two test root");
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);
    let key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["127.0.0.1".into()])
        .unwrap()
        .signed_by(&key, &issuer)
        .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let ca_path = directory.path().join("relay-root.pem");
    std::fs::write(&ca_path, ca.pem()).unwrap();
    let trust = RelayTlsTrust::CustomCaCert(ca_path);
    let relay = H2Relay::start(&leaf, &key, None).await;
    let response = health(&relay.url, trust.clone()).await.unwrap();
    assert_eq!(response.version(), reqwest::Version::HTTP_2);
    assert!(
        health(&relay.url.replace("127.0.0.1", "localhost"), trust)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn leaf_der_pin_authenticates_a_self_signed_h2_relay() {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let relay = H2Relay::start(&cert, &signing_key, None).await;
    let pin = Sha256::digest(cert.der().as_ref()).into();
    let response = health(&relay.url, RelayTlsTrust::CertSha256(pin))
        .await
        .unwrap();
    assert_eq!(response.version(), reqwest::Version::HTTP_2);
    let mut wrong = pin;
    wrong[0] ^= 1;
    for trust in [
        RelayTlsTrust::CertSha256(wrong),
        RelayTlsTrust::SystemDefault,
    ] {
        assert!(health(&relay.url, trust).await.unwrap_err().is_connect());
    }
}

#[tokio::test]
async fn https_discovery_refuses_a_downgrade_or_a_new_pinned_origin() {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let policy = policy(RelayTlsTrust::CertSha256(
        Sha256::digest(cert.der().as_ref()).into(),
    ));
    for (target, reason) in [
        (
            "http://127.external.invalid:8080",
            "non-loopback HTTP target",
        ),
        ("https://127.0.0.1:1", "different HTTPS origin"),
    ] {
        let mut relay = H2Relay::start(&cert, &signing_key, Some(target)).await;
        let client =
            RelayAwareHttpClient::new(&relay.url, expected_contract(), &policy, Default::default())
                .unwrap();
        let error = tokio::task::spawn_blocking(move || client.connect())
            .await
            .unwrap()
            .unwrap_err();
        assert!(
            matches!(&error, HttpError::InvalidInput(message) if message.contains(reason)),
            "{error}"
        );
        assert_eq!(relay.request().await.0, "/_resolve/grid");
        assert!(relay.requests.try_recv().is_err());
    }
}

#[tokio::test]
async fn control_requests_use_a_distinct_h2_connection_during_a_pending_call() {
    let rcgen::CertifiedKey { cert, signing_key } =
        rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let mut relay = H2Relay::start(&cert, &signing_key, None).await;
    let policy = policy(RelayTlsTrust::CertSha256(
        Sha256::digest(cert.der().as_ref()).into(),
    ));
    let client = Arc::new(HttpClient::new(&relay.url, 2, &policy).unwrap());

    assert!(client.health_async().await.unwrap());
    let (health_path, control_connection) = relay.request().await;
    assert_eq!(health_path, "/health");

    let business_client = Arc::clone(&client);
    let call = tokio::spawn(async move {
        business_client
            .call_route_async(
                &expected_contract(),
                "run",
                HttpCallInput::Borrowed(&[]),
                &DomainBudget::from_limits(&c2_config::DomainLimits::default()),
                None,
                HttpUnaryMode::Plain,
                &CallMetadata::default(),
            )
            .await
    });
    let (business_path, data_connection) = relay.request().await;
    assert_eq!(business_path, crate::CALL_PATH);
    assert_ne!(control_connection, data_connection);

    let outcome = tokio::time::timeout(
        WAIT,
        client.query_call_route_async(&expected_contract(), "pending", &CallMetadata::default()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(outcome.status, c2_wire::rpc::WireCallOutcomeStatus::Unknown);
    assert_eq!(
        relay.request().await,
        (crate::OUTCOME_PATH.into(), control_connection)
    );
    assert!(!call.is_finished());
    call.abort();
}
