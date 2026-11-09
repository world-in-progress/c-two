//! ADR 0004: an existing client follows the route before its next dispatch.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

use c2_contract::{ContractRelease, MethodAccess};
use c2_core::{
    CallOptions, CallTimeout, Connect, ConnectAttempt, ConnectOptions, EncodedClient,
    EncodedService, Error, HostOptions, MethodDefinition, Runtime, RuntimeOptions,
    ServiceDefinition,
};
use c2_error::{C2Error, ErrorCode};
use c2_local::{ConnectionEndReason, LocalListener};
use c2_wire::flags;
use c2_wire::frame::{decode_frame_body, encode_frame};
use c2_wire::handshake::{
    MethodEntry, RouteInfo, ServerIdentity, VersionPreambleKind, encode_server_handshake,
    encode_version_preamble,
};
use c2_wire::route_catalog_control::{
    RouteContractWire, RouteLookupResponse, RouteMethodWire, RouteRecordWire, RouteStateReasonWire,
    RouteStateWire, encode_route_lookup_response,
};
use tokio::io::AsyncReadExt;

const DESCRIPTOR: &[u8] =
    include_bytes!("../../../../tests/fixtures/contracts/portable-release.contract.json");
const WAIT: Duration = Duration::from_secs(5);

struct Echo(u8);
impl EncodedService for Echo {
    fn invoke(&self, _: u16, mut request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        request.insert(0, self.0);
        Ok(request)
    }
}

struct CountingEcho {
    value: u8,
    calls: Arc<AtomicUsize>,
}

impl EncodedService for CountingEcho {
    fn invoke(&self, _: u16, mut request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        request.insert(0, self.value);
        Ok(request)
    }
}

fn runtime(root: &std::path::Path, server_id: &str) -> Runtime {
    let runtime = Runtime::new(RuntimeOptions {
        server_id: Some(server_id.into()),
        use_process_relay_anchor: false,
        ..Default::default()
    })
    .unwrap();
    runtime
        .set_local_endpoint_with_sources(
            c2_config::LocalEndpointOptions {
                #[cfg(unix)]
                unix_root: Some(root.to_path_buf()),
                ..Default::default()
            },
            c2_config::ConfigSources::empty(),
        )
        .unwrap();
    runtime
}

fn definition(value: u8) -> ServiceDefinition {
    definition_with_service(Arc::new(Echo(value)))
}

fn definition_with_service(service: Arc<dyn EncodedService>) -> ServiceDefinition {
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap();
    ServiceDefinition::new(
        &release,
        release.reference(),
        "follow",
        [
            MethodDefinition {
                index: 0,
                name: "ping".into(),
                access: MethodAccess::Read,
            },
            MethodDefinition {
                index: 1,
                name: "echo".into(),
                access: MethodAccess::Write,
            },
        ],
        service,
    )
    .unwrap()
}

#[test]
fn peer_end_before_dispatch_follows_once_for_concurrent_views() {
    let root = tempfile::Builder::new()
        .prefix("c2-follow-")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let name = format!("follow-{}", uuid::Uuid::new_v4().simple());
    let server = runtime(root.path(), &name);
    let host = server.host(HostOptions::default().without_relay()).unwrap();
    let registration = host.register(definition(1)).unwrap();
    let consumer = runtime(root.path(), &format!("client-{name}"));
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap();
    let client = consumer
        .connect(
            release.expected_route("follow").unwrap(),
            Connect::DirectIpc {
                address: host.local_endpoint().address().into(),
            },
        )
        .unwrap();
    let view = client.with_call_options(CallOptions::new());
    let prepared = client.begin_call("echo").unwrap();
    let old = client.connection_observation().unwrap();
    assert_eq!(client.call_owned("echo", b"old").unwrap(), b"\x01old");
    assert!(host.shutdown().runtime_barrier_error.is_none());
    let io = tokio::runtime::Runtime::new().unwrap();
    assert!(io.block_on(old.wait_closed(Some(WAIT))).is_some());
    drop((registration, host, server));

    let replacement = runtime(root.path(), &name);
    let host = replacement
        .host(HostOptions::default().without_relay())
        .unwrap();
    let _registration = host.register(definition(2)).unwrap();
    let expired = client.with_call_options(CallOptions::with_timeout(CallTimeout::After(
        Duration::ZERO,
    )));
    let Error::Semantic(error) = expired.call_owned("echo", b"expired").unwrap_err() else {
        panic!("an expired call must refuse before acquiring a replacement");
    };
    assert_eq!(error.code, ErrorCode::CallDeadlineExceeded);
    assert_eq!(consumer.path_counters().direct_ipc(), 1);
    // All eight preparations retain the ended target before any one can publish a replacement.
    let barrier = Arc::new(Barrier::new(9));
    let calls: Vec<_> = (0..8)
        .map(|_| {
            let client = client.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let prepared = client.begin_call("echo").unwrap();
                barrier.wait();
                assert_eq!(
                    prepared
                        .encode_vec(b"next".to_vec())
                        .unwrap()
                        .call_owned()
                        .unwrap(),
                    b"\x02next"
                );
                client.connection_observation().unwrap().connection_id()
            })
        })
        .collect();
    barrier.wait();
    let connections: Vec<_> = calls.into_iter().map(|call| call.join().unwrap()).collect();
    let new = view.connection_observation().unwrap();
    assert_ne!(old.connection_id(), new.connection_id());
    assert!(connections.iter().all(|id| *id == new.connection_id()));
    assert_eq!(consumer.path_counters().direct_ipc(), 2);
    // The already-prepared call must check again after encoding.
    assert_eq!(
        prepared
            .encode_vec(b"prepared".to_vec())
            .unwrap()
            .call_owned()
            .unwrap(),
        b"\x02prepared"
    );
    assert!(old.snapshot().is_some());
    drop((client, view));
    assert!(host.shutdown().runtime_barrier_error.is_none());
}

#[test]
fn endpoint_identity_is_checked_before_dispatch_while_unpinned_endpoint_follows() {
    let root = tempfile::Builder::new()
        .prefix("c2-follow-identity-")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let name = format!("follow-{}", uuid::Uuid::new_v4().simple());
    let server = runtime(root.path(), &name);
    let host = server.host(HostOptions::default().without_relay()).unwrap();
    let registration = host.register(definition(1)).unwrap();
    let address = host.local_endpoint().address().to_owned();
    let consumer = runtime(root.path(), &format!("client-{name}"));
    let expected = ContractRelease::from_descriptor_json(DESCRIPTOR)
        .unwrap()
        .expected_route("follow")
        .unwrap();
    let attempt = ConnectAttempt::start(ConnectOptions::new()).unwrap();
    let unpinned = consumer
        .native_endpoint_context(&address, None, &attempt)
        .unwrap();
    let client = unpinned.prepare(expected.clone(), &attempt).unwrap();
    let identity = client.ipc_server_identity().unwrap();
    let pinned = consumer
        .native_endpoint_context(&address, Some(identity), &attempt)
        .unwrap();
    let pinned_client = pinned.prepare(expected.clone(), &attempt).unwrap();
    let old = client.connection_observation().unwrap();
    let pinned_old = pinned_client.connection_observation().unwrap();
    assert_eq!(
        pinned_client.call_owned("echo", b"old").unwrap(),
        b"\x01old"
    );

    assert!(host.shutdown().runtime_barrier_error.is_none());
    let io = tokio::runtime::Runtime::new().unwrap();
    assert!(io.block_on(old.wait_closed(Some(WAIT))).is_some());
    assert!(io.block_on(pinned_old.wait_closed(Some(WAIT))).is_some());
    drop((registration, host, server));

    let replacement = runtime(root.path(), &name);
    let host = replacement
        .host(HostOptions::default().without_relay())
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let _registration = host
        .register(definition_with_service(Arc::new(CountingEcho {
            value: 2,
            calls: calls.clone(),
        })))
        .unwrap();
    let Error::Semantic(error) = pinned_client.call_owned("echo", b"pinned").unwrap_err() else {
        panic!("pinned endpoint must report a semantic identity refusal");
    };
    assert_eq!(error.code, ErrorCode::IdentityMismatch);
    assert_eq!(
        error.details.get("transport_phase").map(String::as_str),
        Some("pre_dispatch")
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        client.call_owned("echo", b"unpinned").unwrap(),
        b"\x02unpinned"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(host.shutdown().runtime_barrier_error.is_none());
}

#[test]
fn deactivated_endpoint_context_cannot_reacquire_after_peer_end() {
    let root = tempfile::Builder::new()
        .prefix("c2-follow-deactivate-")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let name = format!("follow-{}", uuid::Uuid::new_v4().simple());
    let server = runtime(root.path(), &name);
    let host = server.host(HostOptions::default().without_relay()).unwrap();
    let registration = host.register(definition(1)).unwrap();
    let address = host.local_endpoint().address().to_owned();
    let consumer = runtime(root.path(), &format!("client-{name}"));
    let expected = ContractRelease::from_descriptor_json(DESCRIPTOR)
        .unwrap()
        .expected_route("follow")
        .unwrap();
    let attempt = ConnectAttempt::start(ConnectOptions::new()).unwrap();
    let endpoint = consumer
        .native_endpoint_context(&address, None, &attempt)
        .unwrap();
    let client = endpoint.prepare(expected.clone(), &attempt).unwrap();
    let old = client.connection_observation().unwrap();
    assert_eq!(client.call_owned("echo", b"old").unwrap(), b"\x01old");
    assert!(host.shutdown().runtime_barrier_error.is_none());
    let io = tokio::runtime::Runtime::new().unwrap();
    assert!(io.block_on(old.wait_closed(Some(WAIT))).is_some());
    drop((registration, host, server));

    let replacement = runtime(root.path(), &name);
    let host = replacement
        .host(HostOptions::default().without_relay())
        .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let _registration = host
        .register(definition_with_service(Arc::new(CountingEcho {
            value: 2,
            calls: calls.clone(),
        })))
        .unwrap();
    endpoint.deactivate();
    assert!(client.call_owned("echo", b"new").is_err());
    assert!(
        endpoint
            .prepare(
                expected,
                &ConnectAttempt::start(ConnectOptions::new()).unwrap()
            )
            .is_err()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert!(host.shutdown().runtime_barrier_error.is_none());
}

#[test]
fn malformed_response_frame_does_not_reacquire_a_protocol_failed_connection() {
    let root = tempfile::Builder::new()
        .prefix("c2-follow-malformed-")
        .tempdir_in(std::env::temp_dir())
        .unwrap();
    let name = format!("malformed-{}", uuid::Uuid::new_v4().simple());
    let address = format!("ipc://{name}");
    let consumer = runtime(root.path(), &format!("client-{name}"));
    let endpoint = consumer.local_endpoint(&address).unwrap();
    let expected = ContractRelease::from_descriptor_json(DESCRIPTOR)
        .unwrap()
        .expected_route("follow")
        .unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let accepted = Arc::new(AtomicUsize::new(0));
    let accepted_peer = accepted.clone();
    let peer = std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async move {
                let mut listener = LocalListener::bind(&endpoint).unwrap();
                ready_tx.send(()).unwrap();
                let mut stream = listener.accept().await.unwrap();
                accepted_peer.fetch_add(1, Ordering::SeqCst);
                let mut preamble = [0; 25];
                stream.read_exact(&mut preamble).await.unwrap();
                assert_eq!(
                    preamble,
                    encode_version_preamble(VersionPreambleKind::Hello)
                );
                stream
                    .write_all(&encode_version_preamble(VersionPreambleKind::Accepted))
                    .await
                    .unwrap();
                let (header, _) = read_peer_frame(&mut stream).await;
                assert!(header.is_handshake());
                let route = RouteInfo {
                    name: expected.route_name.clone(),
                    crm_ns: expected.crm_ns.clone(),
                    crm_name: expected.crm_name.clone(),
                    crm_ver: expected.crm_ver.clone(),
                    abi_hash: expected.abi_hash.clone(),
                    signature_hash: expected.signature_hash.clone(),
                    max_payload_size: 1024,
                    methods: vec![
                        MethodEntry {
                            name: "ping".into(),
                            index: 0,
                        },
                        MethodEntry {
                            name: "echo".into(),
                            index: 1,
                        },
                    ],
                };
                let identity = ServerIdentity {
                    server_id: name,
                    server_instance_id: uuid::Uuid::new_v4().to_string(),
                };
                let handshake = encode_server_handshake(0, &[route], "", &identity).unwrap();
                stream
                    .write_all(&encode_frame(
                        0,
                        flags::FLAG_HANDSHAKE | flags::FLAG_RESPONSE,
                        &handshake,
                    ))
                    .await
                    .unwrap();
                let (lookup, _) = read_peer_frame(&mut stream).await;
                assert!(lookup.is_ctrl());
                let response = RouteLookupResponse::Ready {
                    current: RouteRecordWire {
                        route_name: expected.route_name.clone(),
                        route_uid: "malformed-uid".into(),
                        owner_server_id: identity.server_id,
                        owner_server_instance_id: identity.server_instance_id,
                        contract: RouteContractWire {
                            route_name: expected.route_name,
                            crm_ns: expected.crm_ns,
                            crm_name: expected.crm_name,
                            crm_ver: expected.crm_ver,
                            abi_hash: expected.abi_hash,
                            signature_hash: expected.signature_hash,
                        },
                        methods: vec![
                            RouteMethodWire {
                                name: "ping".into(),
                                index: 0,
                            },
                            RouteMethodWire {
                                name: "echo".into(),
                                index: 1,
                            },
                        ],
                        max_payload_size: 1024,
                        state: RouteStateWire::Ready,
                        state_reason: RouteStateReasonWire::RegisterCommitted,
                    },
                };
                stream
                    .write_all(&encode_frame(
                        lookup.request_id,
                        flags::FLAG_CTRL | flags::FLAG_RESPONSE,
                        &encode_route_lookup_response(&response).unwrap(),
                    ))
                    .await
                    .unwrap();
                let (call, _) = read_peer_frame(&mut stream).await;
                assert!(!call.is_ctrl());
                // A length below the fixed frame header ends the connection as
                // ProtocolError, while the dispatched call receives Closed.
                let mut malformed = [0_u8; 16];
                malformed[..4].copy_from_slice(&11_u32.to_le_bytes());
                stream.write_all(&malformed).await.unwrap();
                tokio::select! {
                    _ = stop_rx => {},
                    next = listener.accept() => {
                        next.unwrap();
                        accepted_peer.fetch_add(1, Ordering::SeqCst);
                    },
                }
                drop(stream);
                let _ = listener.close();
            });
    });
    ready_rx.recv_timeout(WAIT).unwrap();

    let client = consumer
        .connect(expected_route(), Connect::DirectIpc { address })
        .unwrap();
    let original = client.connection_observation().unwrap();
    let error = client.call_owned("echo", b"first").unwrap_err();
    let Error::Transport(error) = error else {
        panic!("malformed frame must fail the dispatched call as transport uncertain");
    };
    assert_eq!(error.phase(), c2_core::TransportPhase::DispatchUncertain);
    let io = tokio::runtime::Runtime::new().unwrap();
    let closed = io.block_on(original.wait_closed(Some(WAIT))).unwrap();
    assert_eq!(closed.reason(), ConnectionEndReason::ProtocolError);

    let bounded = client.with_call_options(CallOptions::with_timeout(CallTimeout::After(
        Duration::from_millis(200),
    )));
    let Error::Transport(next_error) = bounded.call_owned("echo", b"next").unwrap_err() else {
        panic!("the terminated connection must refuse the next call before dispatch");
    };
    assert_eq!(next_error.phase(), c2_core::TransportPhase::PreDispatch);
    assert_eq!(accepted.load(Ordering::SeqCst), 1);
    assert_eq!(consumer.path_counters().direct_ipc(), 1);
    assert_eq!(
        client.connection_observation().unwrap().connection_id(),
        original.connection_id()
    );
    stop_tx.send(()).ok();
    peer.join().unwrap();
}

fn expected_route() -> c2_contract::ExpectedRouteContract {
    ContractRelease::from_descriptor_json(DESCRIPTOR)
        .unwrap()
        .expected_route("follow")
        .unwrap()
}

async fn read_peer_frame(
    stream: &mut c2_local::LocalStream,
) -> (c2_wire::frame::FrameHeader, Vec<u8>) {
    let mut length = [0; 4];
    stream.read_exact(&mut length).await.unwrap();
    let total_len = u32::from_le_bytes(length);
    let mut body = vec![0; total_len as usize];
    stream.read_exact(&mut body).await.unwrap();
    let (header, payload) = decode_frame_body(&body, total_len).unwrap();
    (header, payload.to_vec())
}
