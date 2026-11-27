use axum::{
    Extension, Router,
    body::{Body, Bytes},
    routing::get,
};
use c2_config::{BridgeConfig, ConfigResolver, ConfigSources, DomainLimits, EnvCatalog};
use http::{Request, StatusCode};
use http_body_util::BodyExt;
use iroh::endpoint::{ConnectError, ConnectingError, ConnectionError, ReadError, ReadToEndError};
use std::{sync::Arc, time::Duration};

use super::*;
use crate::{
    bridge::BridgeServer,
    relay::{
        RelayServer,
        peer::{PeerEnvelope, PeerMessage},
        router::build_mesh_router,
        stream_control::ResponseStreamControl,
        test_support::{TEST_ABI_HASH, TEST_SIGNATURE_HASH, test_state_for_client},
        types::{Locality, RouteEntry},
    },
};

const WAIT: Duration = Duration::from_secs(10);

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    timeout(WAIT, future).await.expect("mesh test timed out")
}

fn get_request(path: &str) -> Request<Body> {
    Request::get(path).body(Body::empty()).unwrap()
}

async fn bridge(dir: &std::path::Path, qad: bool) -> BridgeServer {
    BridgeServer::start(BridgeConfig {
        bind: "127.0.0.1:0".into(),
        quic_bind: qad.then(|| "127.0.0.1:0".into()),
        state_dir: Some(dir.join("bridge")),
        ..BridgeConfig::default()
    })
    .await
    .unwrap()
}

fn config(bridge: &BridgeServer, secret: &str) -> RelayConfig {
    RelayConfig {
        bridge: Some(bridge.bridge_url_template()),
        bridge_token: Some(bridge.token().to_owned()),
        cluster_secret: Some(secret.to_owned()),
        domain_limits: DomainLimits {
            memory_budget_bytes: 128 * 1024,
            ..DomainLimits::default()
        },
        ..RelayConfig::default()
    }
}

struct MeshNode {
    transport: Arc<IrohMeshTransport>,
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl MeshNode {
    async fn start(config: &RelayConfig, relay_only: bool, router: Router) -> Self {
        let transport = Arc::new(
            IrohMeshTransport::bind_with(
                config,
                SecretKey::generate(),
                config.validate_with_bridge_spec().unwrap(),
                relay_only,
            )
            .await
            .unwrap(),
        );
        let stop = CancellationToken::new();
        let task = {
            let transport = transport.clone();
            let stop = stop.clone();
            tokio::spawn(async move { transport.serve(router, stop).await })
        };
        if config.bridge.is_some() {
            bounded(transport.endpoint.online()).await;
        }
        Self {
            transport,
            stop,
            task,
        }
    }

    fn relay_hint(&self) -> EndpointAddr {
        let addr = self.transport.endpoint_addr();
        EndpointAddr::new(addr.id).with_relay_url(addr.relay_urls().next().unwrap().clone())
    }

    async fn done(self) {
        self.stop.cancel();
        bounded(self.task).await.unwrap();
    }
}

#[test]
fn hmac_sha256_matches_rfc_4231() {
    for (key, text, expected) in [
        (
            vec![0x0b; 20],
            "Hi There",
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7",
        ),
        (
            vec![0xaa; 131],
            "Test Using Larger Than Block-Size Key - Hash Key First",
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54",
        ),
    ] {
        assert_eq!(
            hex::encode(HmacSha256::new(&key).sign(&[text.as_bytes()])),
            expected
        );
    }
}

#[tokio::test]
async fn authenticated_mesh_uses_direct_ip_when_available_and_bridge_without_udp() {
    let parent = tempfile::tempdir().unwrap();
    let bridge = bridge(parent.path(), true).await;
    let cfg = config(&bridge, "mesh-proof");
    for relay_only in [false, true] {
        let router = Router::new().route(
            "/peer",
            get(
                |Extension(peer): Extension<AuthenticatedIrohPeer>| async move {
                    assert_ne!(peer.endpoint_id.as_bytes(), &[0; 32]);
                    hex::encode(peer.endpoint_id.as_bytes())
                },
            ),
        );
        let b = MeshNode::start(&cfg, relay_only, router).await;
        let a = MeshNode::start(&cfg, relay_only, Router::new()).await;
        let addr = b.relay_hint();
        let response = bounded(a.transport.send_request(
            MeshLane::Control,
            &addr,
            get_request("/peer"),
        ))
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = bounded(response.into_body().collect())
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(body, hex::encode(a.transport.endpoint_id().as_bytes()));
        let conn = a
            .transport
            .connection(MeshLane::Control, &addr)
            .await
            .unwrap();
        if relay_only {
            assert!(a.transport.endpoint_addr().ip_addrs().next().is_none());
            assert!(conn.paths().iter().any(|p| p.is_selected() && p.is_relay()));
            assert!(!conn.paths().iter().any(|p| p.is_ip()));
        } else {
            bounded(
                conn.paths_stream()
                    .wait_any(|p| p.is_selected() && p.is_ip()),
            )
            .await
            .expect("direct IP path");
        }
        a.done().await;
        b.done().await;
    }
    bridge.stop().await.unwrap();
}

#[tokio::test]
async fn wrong_secret_and_unannounced_business_alpn_are_rejected() {
    let parent = tempfile::tempdir().unwrap();
    let bridge = bridge(parent.path(), false).await;
    let a = MeshNode::start(&config(&bridge, "a"), true, Router::new()).await;
    let b = MeshNode::start(&config(&bridge, "b"), true, Router::new()).await;
    for (from, to) in [(&a, &b), (&b, &a)] {
        assert!(
            bounded(
                from.transport
                    .connection(MeshLane::Control, &to.relay_hint())
            )
            .await
            .is_err()
        );
        assert!(
            !from
                .transport
                .auth
                .peers
                .lock()
                .contains(&to.transport.endpoint_id())
        );
    }
    let spec = RelayBridgeSpec::parse(&bridge.bridge_url_template()).unwrap();
    let pin = spec.cert_sha256().unwrap();
    let relay = iroh::RelayConfig::new(spec.base_url().parse().unwrap(), None)
        .with_auth_token(bridge.token());
    let unannounced = Endpoint::builder(presets::Minimal)
        .clear_ip_transports()
        .relay_mode(RelayMode::Custom(relay.into()))
        .ca_tls_config(CaTlsConfig::custom_server_cert_verifier(Arc::new(
            move |provider| {
                relay_server_cert_verifier(&RelayTlsTrust::CertSha256(pin), provider)
                    .map_err(std::io::Error::other)
            },
        )))
        .bind()
        .await
        .unwrap();
    bounded(unannounced.online()).await;
    for lane in [MeshLane::Control, MeshLane::Data] {
        let closed = match bounded(unannounced.connect(b.relay_hint(), lane.alpn())).await {
            Ok(conn) => bounded(conn.closed()).await,
            Err(ConnectError::Connection { source, .. })
            | Err(ConnectError::Connecting {
                source: ConnectingError::ConnectionError { source, .. },
                ..
            }) => source,
            Err(error) => panic!("business connection failed before mesh admission: {error}"),
        };
        let ConnectionError::ApplicationClosed(closed) = closed else {
            panic!("expected mesh authentication rejection, got {closed:?}")
        };
        assert_eq!(closed.error_code, AUTH_DENIED);
        assert_eq!(closed.reason.as_ref(), AUTH_DENIED_REASON);
    }
    unannounced.close().await;
    a.done().await;
    b.done().await;
    bridge.stop().await.unwrap();
}

#[tokio::test]
async fn mesh_control_is_independent_and_slow_reader_reset_does_not_touch_siblings() {
    let parent = tempfile::tempdir().unwrap();
    let bridge = bridge(parent.path(), false).await;
    let cfg = config(&bridge, "mesh-proof");
    let (control_tx, mut control_rx) = tokio::sync::mpsc::unbounded_channel();
    let show = Router::new()
        .route(
            "/large",
            get(
                move |Extension(control): Extension<Arc<ResponseStreamControl>>| {
                    let tx = control_tx.clone();
                    async move {
                        tx.send(control).unwrap();
                        let chunk = Bytes::from(vec![b'x'; 8192]);
                        Body::from_stream(futures::stream::repeat(
                            Ok::<_, std::convert::Infallible>(chunk),
                        ))
                    }
                },
            ),
        )
        .route(
            "/continuous",
            get(|| async {
                let chunks = futures::stream::unfold(0, |sent| async move {
                    if sent == 120 {
                        return None;
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    let chunk = Bytes::from(vec![b'x'; 32 * 1024]);
                    Some((Ok::<_, std::convert::Infallible>(chunk), sent + 1))
                });
                Body::from_stream(chunks)
            }),
        )
        .route("/small", get(|| async { "ok" }));
    let a = MeshNode::start(&cfg, true, Router::new()).await;
    let b = MeshNode::start(&cfg, true, show).await;
    let addr = b.relay_hint();
    let data = bounded(a.transport.connection(MeshLane::Data, &addr))
        .await
        .unwrap();
    let (mut send, mut recv) = data.open_bi().await.unwrap();
    send.write_all(b"GET /large HTTP/1.1\r\nHost: mesh\r\n\r\n")
        .await
        .unwrap();
    send.finish().unwrap();
    let control = bounded(control_rx.recv()).await.unwrap();
    bounded(async {
        while !control.is_blocked() {
            control.changed.notified().await;
        }
    })
    .await;
    let response = bounded(a.transport.send_request(
        MeshLane::Data,
        &addr,
        get_request("/continuous"),
    ))
    .await
    .unwrap();
    let mut incoming = response.into_body();
    let first = bounded(incoming.frame())
        .await
        .unwrap()
        .unwrap()
        .into_data()
        .unwrap()
        .len();
    let transfer =
        tokio::spawn(async move { first + incoming.collect().await.unwrap().to_bytes().len() });
    let controls = futures::future::try_join_all((0..16).map(|_| async {
        let response = a
            .transport
            .send_request(MeshLane::Control, &addr, get_request("/small"))
            .await?;
        assert_eq!(response.into_body().collect().await?.to_bytes(), "ok");
        Ok::<_, MeshError>(())
    }));
    timeout(Duration::from_secs(2), controls)
        .await
        .expect("control requests starved by data")
        .unwrap();
    let control_conn = a
        .transport
        .connection(MeshLane::Control, &addr)
        .await
        .unwrap();
    assert_ne!(control_conn.stable_id(), data.stable_id());
    assert!(
        !transfer.is_finished(),
        "control requests did not complete during the transfer"
    );
    assert_eq!(bounded(transfer).await.unwrap(), 120 * 32 * 1024);

    let sibling = bounded(
        a.transport
            .send_request(MeshLane::Data, &addr, get_request("/small")),
    )
    .await
    .unwrap();
    assert_eq!(
        bounded(sibling.into_body().collect())
            .await
            .unwrap()
            .to_bytes(),
        "ok"
    );
    control.abort();
    assert!(matches!(bounded(recv.read_to_end(1024 * 1024)).await,
        Err(ReadToEndError::Read(ReadError::Reset(code))) if code.into_inner() == 1));
    let sibling = bounded(
        a.transport
            .send_request(MeshLane::Data, &addr, get_request("/small")),
    )
    .await
    .unwrap();
    assert_eq!(
        bounded(sibling.into_body().collect())
            .await
            .unwrap()
            .to_bytes(),
        "ok"
    );
    assert_eq!(
        a.transport
            .connection(MeshLane::Data, &addr)
            .await
            .unwrap()
            .stable_id(),
        data.stable_id()
    );

    // A disconnected lane is re-established on the next request.
    data.close(VarInt::from_u32(0), b"test reconnect");
    let fresh = bounded(a.transport.connection(MeshLane::Data, &addr))
        .await
        .unwrap();
    assert_ne!(fresh.stable_id(), data.stable_id());
    drop((data, fresh, control_conn));
    a.done().await;
    b.done().await;
    bridge.stop().await.unwrap();
}

#[tokio::test]
async fn surface_c_isolated_and_refusals_finish_before_stopping_upload() {
    let parent = tempfile::tempdir().unwrap();
    let bridge = bridge(parent.path(), false).await;
    let cfg = config(&bridge, "mesh-proof");
    let state = test_state_for_client();
    assert!(
        state.with_route_table_mut(|rt| rt.register_route(RouteEntry {
            name: "grid".into(),
            relay_id: state.relay_id().to_owned(),
            relay_url: "http://localhost:8888".into(),
            server_id: Some("private-server-id".into()),
            server_instance_id: Some("private-server-instance".into()),
            ipc_address: Some("ipc://private-address".into()),
            crm_ns: "test.echo".into(),
            crm_name: "Echo".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: TEST_ABI_HASH.into(),
            signature_hash: TEST_SIGNATURE_HASH.into(),
            max_payload_size: 4,
            route_uid: Some("local-route".into()),
            locality: Locality::Local,
            registered_at: 1.0,
        }))
    );

    let a = MeshNode::start(&cfg, true, Router::new()).await;
    let b = MeshNode::start(&cfg, true, build_mesh_router(state)).await;
    let addr = b.relay_hint();
    for (method, path, expected) in [
        ("POST", "/_register", 404),
        ("POST", "/_unregister", 404),
        ("POST", "/_admin/force-leave", 404),
        ("GET", "/_peers", 404),
        ("GET", "/_resolve?route=grid", 404),
        ("GET", "/_routes", 404),
        ("GET", "/health", 200),
        ("GET", "/_peer/sync", 200),
    ] {
        let response = bounded(
            a.transport.send_request(
                MeshLane::Control,
                &addr,
                Request::builder()
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            ),
        )
        .await
        .unwrap();
        assert_eq!(response.status().as_u16(), expected, "{path}");
        let body = bounded(response.into_body().collect())
            .await
            .unwrap()
            .to_bytes();
        let body = String::from_utf8_lossy(&body);
        for private in [
            "private-server-id",
            "private-server-instance",
            "ipc://private-address",
        ] {
            assert!(!body.contains(private), "{path}: {body}");
        }
    }

    let sender = RelayConfig::derive_iroh_relay_id(a.transport.endpoint_id().as_bytes());
    for (label, expected) in [("different-relay", 403), (sender.as_str(), 200)] {
        let message = PeerEnvelope::new(
            label,
            PeerMessage::RelayJoin {
                relay_id: label.into(),
                url: "http://127.0.0.1:8080".into(),
            },
        );
        let request = Request::post("/_peer/join")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&message).unwrap()))
            .unwrap();
        let response = bounded(a.transport.send_request(MeshLane::Control, &addr, request))
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), expected);
        bounded(response.into_body().collect()).await.unwrap();
    }

    let data = bounded(a.transport.connection(MeshLane::Data, &addr))
        .await
        .unwrap();
    for (path, status, error) in [
        (
            "/_c2/calls/call?route=grid&method=ping",
            413,
            "CallCapacityExceeded",
        ),
        (
            "/_c2/topics/subscribe?route=grid&topic=events",
            400,
            "ProtocolViolation",
        ),
    ] {
        let (mut send, mut recv) = data.open_bi().await.unwrap();
        let header = format!(
            concat!(
                "POST {} HTTP/1.1\r\nHost: mesh\r\nContent-Length: 1000000\r\n",
                "x-c2-expected-crm-ns: test.echo\r\nx-c2-expected-crm-name: Echo\r\n",
                "x-c2-expected-crm-ver: 0.1.0\r\nx-c2-expected-abi-hash: {}\r\n",
                "x-c2-expected-signature-hash: {}\r\nAccept: {}\r\n\r\na"
            ),
            path,
            TEST_ABI_HASH,
            TEST_SIGNATURE_HASH,
            crate::topic_wire::MEDIA_TYPE
        );
        send.write_all(header.as_bytes()).await.unwrap();
        // Keep the request open and parse the actual JSON bytes received before FIN.
        let response = timeout(Duration::from_secs(1), recv.read_to_end(16 * 1024))
            .await
            .expect("early rejection waited on the request body")
            .unwrap();
        let response = String::from_utf8(response).unwrap();
        let (headers, body) = response.split_once("\r\n\r\n").unwrap();
        assert!(
            headers.starts_with(&format!("HTTP/1.1 {status} ")),
            "{response}"
        );
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["name"], error);
        assert_eq!(body["details"]["dispatch_phase"], "pre_dispatch");
        assert_eq!(
            bounded(send.stopped()).await.unwrap(),
            Some(VarInt::from_u32(0))
        );
    }
    a.done().await;
    b.done().await;
    bridge.stop().await.unwrap();
}

#[test]
fn relay_server_owns_mesh_lifetime_and_reuses_endpoint_identity() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = RelayConfig {
        bind: "127.0.0.1:0".into(),
        cluster_secret: Some("local-mesh".into()),
        state_dir: Some(dir.path().join("mesh")),
        ..RelayConfig::default()
    };
    let catalog = EnvCatalog::load(ConfigSources::empty()).unwrap();
    let context = ConfigResolver::resolve_local_endpoint(Default::default(), &catalog).unwrap();
    let mut relay = RelayServer::start_resolved(cfg.clone(), context.clone()).unwrap();
    let id = relay.iroh_endpoint_id().unwrap();
    assert_eq!(relay.iroh_endpoint_addr().unwrap().id, id);
    assert_eq!(
        relay.state_for_test().relay_id(),
        RelayConfig::derive_iroh_relay_id(id.as_bytes())
    );
    let endpoint = relay.iroh_transport().unwrap().endpoint.clone();
    relay.stop().unwrap();
    assert!(endpoint.is_closed());

    let mut relay = RelayServer::start_resolved(
        RelayConfig {
            relay_id: "custom-relay".to_owned(),
            relay_id_explicit: true,
            ..cfg
        },
        context,
    )
    .unwrap();
    assert_eq!(relay.iroh_endpoint_id(), Some(id));
    assert_eq!(relay.state_for_test().relay_id(), "custom-relay");
    relay.stop().unwrap();
}
