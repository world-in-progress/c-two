use std::path::PathBuf;
use std::time::Duration;

use c2_config::{LocalEndpointContext, RelayConfig, RelayTlsTrust, ResolvedRelayClientConfig};
use http::StatusCode;
use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
use reqwest::{Client, Version};

use super::RelayServer;

const WAIT: Duration = Duration::from_secs(5);

fn start(config: RelayConfig) -> RelayServer {
    RelayServer::start_resolved(
        config,
        LocalEndpointContext::default_for_platform().unwrap(),
    )
    .unwrap()
}

fn client(trust: RelayTlsTrust) -> Client {
    crate::relay_client_builder(&ResolvedRelayClientConfig {
        tls_trust: trust,
        ..Default::default()
    })
    .unwrap()
    .timeout(WAIT)
    .build()
    .unwrap()
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn edge_config(edge_bind: &str) -> RelayConfig {
    RelayConfig {
        bind: "127.0.0.1:0".into(),
        edge_bind: Some(edge_bind.into()),
        heartbeat_interval: Duration::ZERO,
        ..Default::default()
    }
}

fn health(url: &str, trust: RelayTlsTrust) -> Result<reqwest::Response, reqwest::Error> {
    runtime().block_on(async { client(trust).get(format!("{url}/health")).send().await })
}

#[test]
fn public_edge_defaults_to_tls_and_persists_the_leaf_fingerprint() {
    let root = tempfile::tempdir().unwrap();
    let state_dir = root.path().join("relay-state");
    let config = RelayConfig {
        state_dir: Some(state_dir.clone()),
        ..edge_config("0.0.0.0:0")
    };
    let mut first = start(config.clone());
    let pin = first.edge_cert_sha256().unwrap();
    assert!(state_dir.join("edge-cert.pem").is_file());
    assert!(state_dir.join("edge-key.pem").is_file());
    assert!(first.advertise_url().starts_with("https://127.0.0.1:"));
    assert_eq!(
        health(&first.advertise_url(), RelayTlsTrust::CertSha256(pin))
            .unwrap()
            .version(),
        Version::HTTP_2,
    );
    first.stop().unwrap();

    let mut restarted = start(config);
    assert_eq!(restarted.edge_cert_sha256(), Some(pin));
    let mut wrong = pin;
    wrong[0] ^= 1;
    assert!(
        health(&restarted.advertise_url(), RelayTlsTrust::CertSha256(wrong))
            .unwrap_err()
            .is_connect()
    );
    restarted.stop().unwrap();

    let mut plaintext = start(RelayConfig {
        insecure_plaintext: true,
        ..edge_config("0.0.0.0:0")
    });
    assert_eq!(plaintext.edge_cert_sha256(), None);
    let response = health(&plaintext.advertise_url(), RelayTlsTrust::SystemDefault).unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.version(), Version::HTTP_11);
    plaintext.stop().unwrap();
}

fn write_signed_identity(dir: &std::path::Path) -> (PathBuf, PathBuf, PathBuf) {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::default();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let root = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);
    let key = KeyPair::generate().unwrap();
    let leaf = CertificateParams::new(vec!["127.0.0.1".into()])
        .unwrap()
        .signed_by(&key, &issuer)
        .unwrap();
    let cert_path = dir.join("chain.pem");
    let key_path = dir.join("key.pem");
    let ca_path = dir.join("root.pem");
    std::fs::write(&cert_path, leaf.pem() + &root.pem()).unwrap();
    std::fs::write(&key_path, key.serialize_pem()).unwrap();
    std::fs::write(&ca_path, root.pem()).unwrap();
    (cert_path, key_path, ca_path)
}

#[test]
fn explicit_edge_certificate_supports_ca_and_der_pin_with_h2_and_http1() {
    let root = tempfile::tempdir().unwrap();
    let (cert, key, ca) = write_signed_identity(root.path());
    let mut relay = start(RelayConfig {
        tls_cert: Some(cert),
        tls_key: Some(key),
        ..edge_config("127.0.0.1:0")
    });
    for trust in [
        RelayTlsTrust::CustomCaCert(ca),
        RelayTlsTrust::CertSha256(relay.edge_cert_sha256().unwrap()),
    ] {
        let response = health(&relay.advertise_url(), trust).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.version(), Version::HTTP_2);
    }
    runtime().block_on(async {
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let pin = RelayTlsTrust::CertSha256(relay.edge_cert_sha256().unwrap());
        let mut config = crate::relay_tls_config(&pin).unwrap();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let tcp = tokio::net::TcpStream::connect(relay.edge_local_addr().unwrap())
            .await
            .unwrap();
        let mut tls = tokio_rustls::TlsConnector::from(Arc::new(config))
            .connect("127.0.0.1".try_into().unwrap(), tcp)
            .await
            .unwrap();
        assert_eq!(tls.get_ref().1.alpn_protocol(), Some(&b"http/1.1"[..]));
        tls.write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(WAIT, tls.read_to_end(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert!(response.starts_with(b"HTTP/1.1 200 "));
    });
    relay.stop().unwrap();
}

#[test]
fn plaintext_listeners_accept_h2_prior_knowledge() {
    let mut relay = start(edge_config("127.0.0.1:0"));
    runtime().block_on(async {
        let client = Client::builder()
            .no_proxy()
            .timeout(WAIT)
            .http2_prior_knowledge()
            .build()
            .unwrap();
        for url in [
            format!("http://{}", relay.local_addr()),
            relay.advertise_url(),
        ] {
            let response = client.get(format!("{url}/health")).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.version(), Version::HTTP_2);
        }
    });
    relay.stop().unwrap();
}

#[test]
fn loopback_edge_omits_local_control_and_ipc_discovery() {
    use super::state::RegisterCommitResult;
    use super::test_support::{TEST_ABI_HASH, TEST_SIGNATURE_HASH};

    const TEST_CRM_NS: &str = "test.relay";
    const TEST_CRM_NAME: &str = "Grid";
    const TEST_CRM_VER: &str = "1.0.0";

    let mut relay = start(edge_config("127.0.0.1:0"));
    let state = relay.state_for_test();
    assert!(matches!(
        test_commit_registration!(
            state,
            "grid".into(),
            "grid".into(),
            "test-instance".into(),
            "ipc://grid".into(),
            TEST_CRM_NS.into(),
            TEST_CRM_NAME.into(),
            TEST_CRM_VER.into(),
            TEST_ABI_HASH.into(),
            TEST_SIGNATURE_HASH.into(),
            1024,
            "test-route-uid".into(),
            None,
        ),
        RegisterCommitResult::Registered { .. }
    ));
    let edge = relay.advertise_url();
    let local = format!("http://{}", relay.local_addr());
    runtime().block_on(async {
        let client = client(RelayTlsTrust::SystemDefault);
        for (method, path) in [
            (reqwest::Method::POST, "/_register"),
            (reqwest::Method::POST, "/_unregister"),
            (reqwest::Method::POST, "/_admin/unregister"),
            (reqwest::Method::POST, "/_admin/force-leave"),
            (reqwest::Method::GET, "/_peers"),
            (reqwest::Method::POST, "/_peer/join"),
        ] {
            let response = client.request(method, format!("{edge}{path}")).send().await.unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
        let path = format!("/_resolve?route=grid&crm_ns={TEST_CRM_NS}&crm_name={TEST_CRM_NAME}&crm_ver={TEST_CRM_VER}&abi_hash={TEST_ABI_HASH}&signature_hash={TEST_SIGNATURE_HASH}");
        for (base, expose) in [(local, true), (edge, false)] {
            let response = client.get(format!("{base}{path}"))
                .header(crate::client::LOCAL_ENDPOINT_NAMESPACE_HEADER, state.endpoint_context().namespace_id())
                .send().await.unwrap().error_for_status().unwrap();
            let routes: Vec<crate::client::RelayRouteInfo> = response.json().await.unwrap();
            assert_eq!(routes.len(), 1);
            assert_eq!(routes[0].relay_url, base);
            assert_eq!(routes[0].ipc_address.as_deref(), expose.then_some("ipc://grid"));
            assert_eq!(routes[0].server_id.as_deref(), expose.then_some("grid"));
            assert_eq!(routes[0].server_instance_id.as_deref(), expose.then_some("test-instance"));
        }
    });
    relay.stop().unwrap();
}
