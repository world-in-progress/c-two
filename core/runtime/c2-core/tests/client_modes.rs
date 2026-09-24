use std::collections::BTreeMap;
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::{
    Json, Router,
    body::Bytes,
    extract::Path,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use c2_contract::{ContractRelease, MethodAccess};
use c2_core::{
    Connect, EncodedClient, EncodedService, Error, HostOptions, MethodDefinition, ObservedPath,
    Runtime, RuntimeOptions, ServiceDefinition, normalize_ipc_error,
};
use c2_error::{C2Error, ErrorCode};
use c2_http::client::RelayRouteInfo;
use c2_http::relay::{RelayConfig, RelayServer};
use c2_ipc::IpcError;
use c2_local::{LocalEndpoint, LocalListener};
use tokio::io::AsyncReadExt;

const DESCRIPTOR: &str =
    include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json");

static TEST_ID: AtomicU64 = AtomicU64::new(0);
static ENV_LOCK: Mutex<()> = Mutex::new(());

struct Echo;

impl EncodedService for Echo {
    fn invoke(&self, method_index: u16, request: &[u8]) -> Result<Vec<u8>, C2Error> {
        match method_index {
            0 => Ok(Vec::new()),
            1 => Ok(request.to_vec()),
            other => Err(C2Error::new(
                ErrorCode::ProtocolViolation,
                format!("unexpected method index {other}"),
            )),
        }
    }
}

fn unique_name(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        TEST_ID.fetch_add(1, Ordering::Relaxed)
    )
}

fn release() -> ContractRelease {
    ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes()).expect("valid release")
}

fn definition(route_name: &str) -> ServiceDefinition {
    let release = release();
    ServiceDefinition::new(
        &release,
        release.reference(),
        route_name,
        [
            MethodDefinition {
                index: 0,
                name: "ping".to_string(),
                access: MethodAccess::Read,
            },
            MethodDefinition {
                index: 1,
                name: "echo".to_string(),
                access: MethodAccess::Write,
            },
        ],
        Arc::new(Echo),
    )
    .expect("definition must match release")
}

fn runtime_options(server_id: String, relay_url: Option<String>) -> RuntimeOptions {
    RuntimeOptions {
        server_id: Some(server_id),
        relay_anchor_address: relay_url,
        use_process_relay_anchor: false,
        ..RuntimeOptions::default()
    }
}

fn with_invalid_relay_proxy_env(test: impl FnOnce()) {
    let _guard = ENV_LOCK.lock().expect("environment lock");
    let previous_env_file = std::env::var_os("C2_ENV_FILE");
    let previous_proxy = std::env::var_os("C2_RELAY_USE_PROXY");
    // SAFETY: this test owns the process-local environment lock and restores
    // both values before releasing it.
    unsafe {
        std::env::set_var("C2_ENV_FILE", "");
        std::env::set_var("C2_RELAY_USE_PROXY", "not-a-bool");
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(test));
    // SAFETY: restoration happens while holding the same process-local lock.
    unsafe {
        match previous_env_file {
            Some(value) => std::env::set_var("C2_ENV_FILE", value),
            None => std::env::remove_var("C2_ENV_FILE"),
        }
        match previous_proxy {
            Some(value) => std::env::set_var("C2_RELAY_USE_PROXY", value),
            None => std::env::remove_var("C2_RELAY_USE_PROXY"),
        }
    }
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn relay() -> (RelayServer, String) {
    let probe = TcpListener::bind("127.0.0.1:0").expect("reserve relay port");
    let address = probe.local_addr().expect("relay address");
    drop(probe);
    let relay_url = format!("http://{address}");
    let relay = RelayServer::start(RelayConfig {
        bind: address.to_string(),
        advertise_url: relay_url.clone(),
        idle_timeout_secs: 0,
        anti_entropy_interval: std::time::Duration::ZERO,
        heartbeat_interval: std::time::Duration::ZERO,
        ..RelayConfig::default()
    })
    .expect("relay starts");
    (relay, relay_url)
}

#[derive(Clone)]
struct RegistryState {
    route: RelayRouteInfo,
    resolve_count: Arc<AtomicUsize>,
}

struct RegistryServer {
    url: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for RegistryServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn resolve_registry(
    State(state): State<RegistryState>,
    Path(route_name): Path<String>,
) -> Json<Vec<RelayRouteInfo>> {
    state.resolve_count.fetch_add(1, Ordering::SeqCst);
    let mut route = state.route;
    route.name = route_name;
    Json(vec![route])
}

fn registry_server(route: RelayRouteInfo, resolve_count: Arc<AtomicUsize>) -> RegistryServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("registry listener");
    listener
        .set_nonblocking(true)
        .expect("nonblocking registry listener");
    let address = listener.local_addr().expect("registry address");
    let url = format!("http://{address}");
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("registry runtime");
        runtime.block_on(async move {
            let listener =
                tokio::net::TcpListener::from_std(listener).expect("tokio registry listener");
            let app = Router::new()
                .route("/_resolve/{name}", get(resolve_registry))
                .with_state(RegistryState {
                    route,
                    resolve_count,
                });
            ready_tx.send(()).expect("registry ready");
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("registry serve");
        });
    });
    ready_rx.recv().expect("registry readiness");
    RegistryServer {
        url,
        shutdown: Some(shutdown_tx),
        thread: Some(thread),
    }
}

async fn relay_echo(body: Bytes) -> (StatusCode, Vec<u8>) {
    (StatusCode::OK, body.to_vec())
}

async fn counted_relay_echo(
    State(call_count): State<Arc<AtomicUsize>>,
    body: Bytes,
) -> (StatusCode, Vec<u8>) {
    call_count.fetch_add(1, Ordering::SeqCst);
    (StatusCode::OK, body.to_vec())
}

fn data_plane_server() -> RegistryServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("data-plane listener");
    listener
        .set_nonblocking(true)
        .expect("nonblocking data-plane listener");
    let address = listener.local_addr().expect("data-plane address");
    let url = format!("http://{address}");
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("data-plane runtime");
        runtime.block_on(async move {
            let listener =
                tokio::net::TcpListener::from_std(listener).expect("tokio data-plane listener");
            let app = Router::new()
                .route("/_probe/{route}", get(|| async { StatusCode::OK }))
                .route("/{route}/{method}", post(relay_echo));
            ready_tx.send(()).expect("data-plane ready");
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("data-plane serve");
        });
    });
    ready_rx.recv().expect("data-plane readiness");
    RegistryServer {
        url,
        shutdown: Some(shutdown_tx),
        thread: Some(thread),
    }
}

fn counted_data_plane_server(call_count: Arc<AtomicUsize>) -> RegistryServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("counted data-plane listener");
    listener
        .set_nonblocking(true)
        .expect("nonblocking counted data-plane listener");
    let address = listener.local_addr().expect("counted data-plane address");
    let url = format!("http://{address}");
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("counted data-plane runtime");
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener)
                .expect("tokio counted data-plane listener");
            let app = Router::new()
                .route("/_probe/{route}", get(|| async { StatusCode::OK }))
                .route("/{route}/{method}", post(counted_relay_echo))
                .with_state(call_count);
            ready_tx.send(()).expect("counted data-plane ready");
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("counted data-plane serve");
        });
    });
    ready_rx.recv().expect("counted data-plane readiness");
    RegistryServer {
        url,
        shutdown: Some(shutdown_tx),
        thread: Some(thread),
    }
}

struct MalformedIpcServer {
    address: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for MalformedIpcServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn malformed_ipc_server() -> MalformedIpcServer {
    let address = format!("ipc://{}", unique_name("malformed-handshake"));
    let endpoint = LocalEndpoint::from_address(&address).expect("IPC endpoint");
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("malformed IPC runtime");
        runtime.block_on(async move {
            let mut listener = LocalListener::bind(&endpoint).expect("malformed IPC listener");
            ready_tx.send(()).expect("malformed IPC ready");
            tokio::select! {
                _ = shutdown_rx => {},
                result = tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    let mut stream = listener.accept().await.expect("malformed IPC accept");
                    let mut length = [0_u8; 4];
                    stream.read_exact(&mut length).await.expect("read client handshake length");
                    let body_len = u32::from_le_bytes(length) as usize;
                    let mut body = vec![0_u8; body_len];
                    stream.read_exact(&mut body).await.expect("read client handshake body");

                    let mut malformed = Vec::with_capacity(16);
                    malformed.extend_from_slice(&12_u32.to_le_bytes());
                    malformed.extend_from_slice(&0_u64.to_le_bytes());
                    malformed.extend_from_slice(&(1_u32 << 1).to_le_bytes());
                    stream.write_all(&malformed).await.expect("write non-handshake response");
                    // Keep the pipe alive until the client consumes the invalid
                    // response and closes; closing a Windows server pipe early
                    // can discard bytes still waiting in its buffer.
                    let mut extra = [0_u8; 1];
                    let _ = stream.read(&mut extra).await;
                }) => result.expect("malformed IPC exchange must finish"),
            }
        });
    });
    ready_rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("malformed IPC readiness");
    MalformedIpcServer {
        address,
        shutdown: Some(shutdown_tx),
        thread: Some(thread),
    }
}

async fn stale_relay_call(
    State(call_count): State<Arc<AtomicUsize>>,
    Path((route, _method)): Path<(String, String)>,
) -> (StatusCode, Json<c2_error::C2ErrorEnvelope>) {
    call_count.fetch_add(1, Ordering::SeqCst);
    (
        StatusCode::CONFLICT,
        Json(
            C2Error::new(ErrorCode::RouteStale, "stale route token")
                .with_details(BTreeMap::from([("route".to_string(), route)]))
                .envelope(),
        ),
    )
}

fn stale_data_plane_server(call_count: Arc<AtomicUsize>) -> RegistryServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("stale data-plane listener");
    listener
        .set_nonblocking(true)
        .expect("nonblocking stale data-plane listener");
    let address = listener.local_addr().expect("stale data-plane address");
    let url = format!("http://{address}");
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("stale data-plane runtime");
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener)
                .expect("tokio stale data-plane listener");
            let app = Router::new()
                .route("/_probe/{route}", get(|| async { StatusCode::OK }))
                .route("/{route}/{method}", post(stale_relay_call))
                .with_state(call_count);
            ready_tx.send(()).expect("stale data-plane ready");
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("stale data-plane serve");
        });
    });
    ready_rx.recv().expect("stale data-plane readiness");
    RegistryServer {
        url,
        shutdown: Some(shutdown_tx),
        thread: Some(thread),
    }
}

fn local_candidate(
    expected: &c2_contract::ExpectedRouteContract,
    address: String,
    server_id: String,
    server_instance_id: String,
) -> RelayRouteInfo {
    RelayRouteInfo {
        name: expected.route_name.clone(),
        relay_url: "http://127.0.0.1:9".to_string(),
        route_uid: "route-uid-1".to_string(),
        route_revision: 1,
        ipc_address: Some(address),
        server_id: Some(server_id),
        server_instance_id: Some(server_instance_id),
        crm_ns: expected.crm_ns.clone(),
        crm_name: expected.crm_name.clone(),
        crm_ver: expected.crm_ver.clone(),
        abi_hash: expected.abi_hash.clone(),
        signature_hash: expected.signature_hash.clone(),
        max_payload_size: 1024 * 1024,
    }
}

#[test]
fn relay_aware_connect_without_anchor_ignores_proxy_configuration() {
    with_invalid_relay_proxy_env(|| {
        let route_name = unique_name("missing-relay");
        let expected = release()
            .expected_route(route_name)
            .expect("expected route");
        let runtime = Runtime::new(runtime_options(unique_name("no-relay"), None))
            .expect("runtime without relay");

        let error = runtime
            .connect(expected, Connect::RelayAware)
            .expect_err("relay-aware connect requires an anchor");

        assert!(matches!(
            error,
            Error::Lifecycle(c2_core::LifecycleError::MissingRelayAddress)
        ));
    });
}

#[test]
fn all_connection_modes_return_the_same_client_surface_and_record_distinct_paths() {
    fn accepts_client(_: &c2_core::Client) {}

    let (mut relay, relay_url) = relay();
    let runtime = Runtime::new(runtime_options(
        unique_name("client-modes"),
        Some(relay_url.clone()),
    ))
    .expect("runtime");
    let host = runtime.host(HostOptions::default()).expect("host");
    let route_name = unique_name("echo");
    let mut registration = host
        .register(definition(&route_name))
        .expect("route registration");
    let registered_route_uid = registration.outcome().route_uid.clone();
    let registered_route_revision = registration.outcome().route_revision;
    let expected = release()
        .expected_route(&route_name)
        .expect("expected route");
    let before = runtime.path_counters();

    let direct = runtime
        .connect(
            expected.clone(),
            Connect::DirectIpc {
                address: runtime.server_address().expect("server address"),
            },
        )
        .expect("direct client");
    accepts_client(&direct);
    assert_eq!(direct.observed_path(), ObservedPath::DirectIpc);
    assert_eq!(direct.observed_route().route_uid, registered_route_uid);
    assert_eq!(
        direct.observed_route().route_revision,
        registered_route_revision
    );
    assert_eq!(direct.expected_route(), &expected);
    assert_eq!(
        direct.call_owned("echo", b"direct").expect("direct call"),
        b"direct"
    );

    let explicit = runtime
        .connect(
            expected.clone(),
            Connect::ExplicitRelay {
                relay_url: relay_url.clone(),
            },
        )
        .expect("explicit relay client");
    accepts_client(&explicit);
    assert_eq!(explicit.observed_path(), ObservedPath::ExplicitRelay);
    assert_eq!(explicit.observed_route().route_uid, registered_route_uid);
    assert_eq!(
        explicit.observed_route().route_revision,
        registered_route_revision
    );
    assert_eq!(
        explicit
            .call_owned("echo", b"explicit")
            .expect("explicit relay call"),
        b"explicit"
    );

    let relay_aware = runtime
        .connect(expected.clone(), Connect::RelayAware)
        .expect("relay-aware client");
    accepts_client(&relay_aware);
    assert_eq!(
        relay_aware.observed_path(),
        ObservedPath::RelayAwareLocalIpc
    );
    assert_eq!(relay_aware.observed_route().route_uid, registered_route_uid);
    assert_eq!(
        relay_aware.observed_route().route_revision,
        registered_route_revision
    );
    assert_eq!(
        relay_aware
            .call_owned("echo", b"aware")
            .expect("relay-aware call"),
        b"aware"
    );

    let after = runtime.path_counters();
    assert_eq!(after.direct_ipc(), before.direct_ipc() + 1);
    assert_eq!(after.explicit_relay(), before.explicit_relay() + 1);
    assert_eq!(
        after.relay_aware_local_ipc(),
        before.relay_aware_local_ipc() + 1
    );
    assert_eq!(after.relay_aware_relay(), before.relay_aware_relay());

    let first = registration.close().expect("first close");
    let second = registration.close().expect("idempotent close");
    assert_eq!(first, second);
    drop(host);
    relay.stop().expect("relay stops");
}

#[test]
fn direct_ipc_uses_only_the_supplied_address_and_never_resolves_relay() {
    let runtime = Runtime::new(runtime_options(
        unique_name("direct-only"),
        Some("http://127.0.0.1:9".to_string()),
    ))
    .expect("runtime");
    let expected = release()
        .expected_route(unique_name("missing"))
        .expect("expected route");
    let error = runtime
        .connect(
            expected,
            Connect::DirectIpc {
                address: format!("ipc://{}", unique_name("absent")),
            },
        )
        .expect_err("missing direct address must fail");

    let Error::Transport(error) = error else {
        panic!("direct connect must report its IPC failure, got {error}");
    };
    assert_eq!(error.kind(), c2_core::TransportKind::Ipc);
    assert!(error.is_fallback_eligible());
    assert_eq!(runtime.path_counters().explicit_relay(), 0);
    assert_eq!(runtime.path_counters().relay_aware_relay(), 0);
}

#[test]
fn explicit_relay_uses_the_http_data_plane_even_when_local_ipc_is_valid() {
    let runtime =
        Runtime::new(runtime_options(unique_name("explicit-http"), None)).expect("runtime");
    let host = runtime
        .host(HostOptions::default().without_relay())
        .expect("host");
    let route_name = unique_name("explicit-route");
    let _registration = host
        .register(definition(&route_name))
        .expect("local registration");
    let expected = release()
        .expected_route(&route_name)
        .expect("expected route");
    let identity = runtime.ensure_server().expect("runtime identity");
    let call_count = Arc::new(AtomicUsize::new(0));
    let data_plane = counted_data_plane_server(call_count.clone());
    let resolve_count = Arc::new(AtomicUsize::new(0));
    let mut route = local_candidate(
        &expected,
        runtime.server_address().expect("server address"),
        identity.server_id,
        identity.server_instance_id,
    );
    route.relay_url = data_plane.url.clone();
    let registry = registry_server(route, resolve_count.clone());

    let client = runtime
        .connect(
            expected,
            Connect::ExplicitRelay {
                relay_url: registry.url.clone(),
            },
        )
        .expect("explicit relay client");
    assert_eq!(client.observed_path(), ObservedPath::ExplicitRelay);
    assert_eq!(
        client
            .call_owned("echo", b"http-only")
            .expect("explicit relay call"),
        b"http-only"
    );
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "explicit relay must cross the HTTP data plane"
    );
    assert_eq!(resolve_count.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.path_counters().explicit_relay(), 1);
    assert_eq!(runtime.path_counters().direct_ipc(), 0);
}

#[test]
fn route_state_and_terminal_identity_errors_use_the_canonical_registry() {
    let cases = [
        (
            IpcError::RouteNotFound("route".to_string()),
            ErrorCode::ResourceNotFound,
        ),
        (
            IpcError::RouteRemoved {
                route_name: "route".to_string(),
                route_uid: Some("uid".to_string()),
            },
            ErrorCode::ResourceRemoved,
        ),
        (
            IpcError::RouteClosed {
                route_name: "route".to_string(),
                route_uid: "uid".to_string(),
                reason: "draining".to_string(),
            },
            ErrorCode::ResourceClosed,
        ),
        (
            IpcError::RouteStale {
                route_name: "route".to_string(),
                current_route_uid: "uid".to_string(),
                current_route_revision: 2,
            },
            ErrorCode::RouteStale,
        ),
        (
            IpcError::ContractMismatch("wrong release".to_string()),
            ErrorCode::ContractMismatch,
        ),
        (
            IpcError::IdentityMismatch {
                expected_server_id: "expected".to_string(),
                expected_server_instance_id: "expected-instance".to_string(),
                actual_server_id: "actual".to_string(),
                actual_server_instance_id: "actual-instance".to_string(),
            },
            ErrorCode::IdentityMismatch,
        ),
        (
            IpcError::Protocol("malformed route reply".to_string()),
            ErrorCode::ProtocolViolation,
        ),
    ];

    for (source, expected_code) in cases {
        let Error::Semantic(error) =
            normalize_ipc_error(source, c2_core::TransportPhase::PreDispatch)
        else {
            panic!("route and identity state must normalize semantically");
        };
        assert_eq!(error.code, expected_code);
    }
}

#[test]
fn retained_release_identity_is_unchanged_by_route_failures() {
    let release = release();
    let retained_ref = release.reference();
    let expected_json = retained_ref.to_canonical_json().expect("reference JSON");
    let _ = normalize_ipc_error(
        IpcError::RouteStale {
            route_name: "route".to_string(),
            current_route_uid: "uid".to_string(),
            current_route_revision: 9,
        },
        c2_core::TransportPhase::PreDispatch,
    );

    retained_ref
        .verify_release(&release)
        .expect("route state cannot mutate release identity");
    assert_eq!(
        retained_ref.to_canonical_json().expect("reference JSON"),
        expected_json
    );
}

#[test]
fn relay_aware_rejects_a_fallback_to_the_same_failed_local_candidate() {
    let release = release();
    let expected = release
        .expected_route(unique_name("same-candidate"))
        .expect("expected route");
    let resolve_count = Arc::new(AtomicUsize::new(0));
    let registry = registry_server(
        local_candidate(
            &expected,
            format!("ipc://{}", unique_name("absent-local")),
            unique_name("absent-server"),
            unique_name("absent-instance"),
        ),
        resolve_count.clone(),
    );
    let runtime = Runtime::new(runtime_options(
        unique_name("fallback"),
        Some(registry.url.clone()),
    ))
    .expect("runtime");

    let Error::Semantic(error) = runtime
        .connect(expected, Connect::RelayAware)
        .expect_err("same failed local candidate must not be retried")
    else {
        panic!("same-path fallback denial must be semantic");
    };
    assert_eq!(error.code, ErrorCode::FallbackDenied);
    assert_eq!(resolve_count.load(Ordering::SeqCst), 2);
    assert_eq!(runtime.path_counters(), Default::default());
}

#[test]
fn relay_aware_identity_mismatch_is_terminal_without_fallback_resolution() {
    let release = release();
    let route_name = unique_name("identity-terminal");
    let expected = release.expected_route(&route_name).expect("expected route");
    let resolve_count = Arc::new(AtomicUsize::new(0));
    let runtime =
        Runtime::new(runtime_options(unique_name("identity-host"), None)).expect("runtime");
    let host = runtime
        .host(HostOptions::default().without_relay())
        .expect("host");
    let _registration = host
        .register(definition(&route_name))
        .expect("local registration");
    let registry = registry_server(
        local_candidate(
            &expected,
            runtime.server_address().expect("server address"),
            "wrong-server-id".to_string(),
            "wrong-server-instance".to_string(),
        ),
        resolve_count.clone(),
    );
    runtime.set_relay_anchor_address(Some(registry.url.clone()));

    let Error::Semantic(error) = runtime
        .connect(expected, Connect::RelayAware)
        .expect_err("identity mismatch must be terminal")
    else {
        panic!("identity mismatch must be semantic");
    };
    assert_eq!(error.code, ErrorCode::IdentityMismatch);
    assert_eq!(
        resolve_count.load(Ordering::SeqCst),
        1,
        "terminal identity mismatch must perform zero fallback resolutions"
    );
    assert_eq!(runtime.path_counters(), Default::default());
}

#[test]
fn relay_aware_contract_mismatch_is_terminal_without_fallback_resolution() {
    let release = release();
    let route_name = unique_name("contract-terminal");
    let actual_expected = release.expected_route(&route_name).expect("expected route");
    let runtime =
        Runtime::new(runtime_options(unique_name("contract-host"), None)).expect("runtime");
    let host = runtime
        .host(HostOptions::default().without_relay())
        .expect("host");
    let _registration = host
        .register(definition(&route_name))
        .expect("local registration");
    let identity = runtime.ensure_server().expect("runtime identity");
    let address = runtime.server_address().expect("server address");
    let inspector = c2_ipc::ClientPool::instance()
        .acquire(&address, Some(&c2_ipc::ClientIpcConfig::default()))
        .expect("inspect local route token");
    let binding = inspector
        .acquire_route(&actual_expected)
        .expect("actual route binding");

    let mut claimed_expected = actual_expected;
    claimed_expected.crm_name = "DifferentPortableContract".to_string();
    let resolve_count = Arc::new(AtomicUsize::new(0));
    let mut candidate = local_candidate(
        &claimed_expected,
        address.clone(),
        identity.server_id,
        identity.server_instance_id,
    );
    candidate.route_uid = binding.route_uid().to_string();
    candidate.route_revision = binding.route_revision();
    let registry = registry_server(candidate, resolve_count.clone());
    runtime.set_relay_anchor_address(Some(registry.url.clone()));

    let result = runtime.connect(claimed_expected, Connect::RelayAware);
    c2_ipc::ClientPool::instance().release(&address);
    let Error::Semantic(error) = result.expect_err("contract mismatch must be terminal") else {
        panic!("contract mismatch must be semantic");
    };
    assert_eq!(error.code, ErrorCode::ContractMismatch);
    assert_eq!(
        resolve_count.load(Ordering::SeqCst),
        1,
        "terminal contract mismatch must perform zero fallback resolutions"
    );
    assert_eq!(runtime.path_counters(), Default::default());
}

#[test]
fn relay_aware_protocol_violation_is_terminal_without_fallback_resolution() {
    let expected = release()
        .expected_route(unique_name("protocol-terminal"))
        .expect("expected route");
    let malformed = malformed_ipc_server();
    let resolve_count = Arc::new(AtomicUsize::new(0));
    let registry = registry_server(
        local_candidate(
            &expected,
            malformed.address.clone(),
            unique_name("claimed-server"),
            unique_name("claimed-instance"),
        ),
        resolve_count.clone(),
    );
    let runtime = Runtime::new(runtime_options(
        unique_name("protocol-runtime"),
        Some(registry.url.clone()),
    ))
    .expect("runtime");

    let Error::Semantic(error) = runtime
        .connect(expected, Connect::RelayAware)
        .expect_err("protocol violation must be terminal")
    else {
        panic!("protocol violation must be semantic");
    };
    assert_eq!(error.code, ErrorCode::ProtocolViolation);
    assert_eq!(
        resolve_count.load(Ordering::SeqCst),
        1,
        "terminal protocol violation must perform zero fallback resolutions"
    );
    assert_eq!(runtime.path_counters(), Default::default());
}

#[test]
fn relay_aware_selects_an_independent_http_path_when_no_local_candidate_exists() {
    let release = release();
    let expected = release
        .expected_route(unique_name("http-only"))
        .expect("expected route");
    let data_plane = data_plane_server();
    let resolve_count = Arc::new(AtomicUsize::new(0));
    let registry = registry_server(
        RelayRouteInfo {
            name: expected.route_name.clone(),
            relay_url: data_plane.url.clone(),
            route_uid: "route-uid-http".to_string(),
            route_revision: 1,
            ipc_address: None,
            server_id: None,
            server_instance_id: None,
            crm_ns: expected.crm_ns.clone(),
            crm_name: expected.crm_name.clone(),
            crm_ver: expected.crm_ver.clone(),
            abi_hash: expected.abi_hash.clone(),
            signature_hash: expected.signature_hash.clone(),
            max_payload_size: 1024 * 1024,
        },
        resolve_count.clone(),
    );
    let runtime = Runtime::new(runtime_options(
        unique_name("http-only-runtime"),
        Some(registry.url.clone()),
    ))
    .expect("runtime");

    let client = runtime
        .connect(expected, Connect::RelayAware)
        .expect("relay-aware HTTP client");
    assert_eq!(client.observed_path(), ObservedPath::RelayAwareRelay);
    assert_eq!(
        client
            .call_owned("echo", b"independent")
            .expect("relay call"),
        b"independent"
    );
    assert_eq!(runtime.path_counters().relay_aware_relay(), 1);
    assert_eq!(resolve_count.load(Ordering::SeqCst), 1);
}

#[test]
fn a_cached_stale_route_is_refreshed_once_and_a_second_stale_result_is_terminal() {
    let release = release();
    let expected = release
        .expected_route(unique_name("stale-once"))
        .expect("expected route");
    let call_count = Arc::new(AtomicUsize::new(0));
    let data_plane = stale_data_plane_server(call_count.clone());
    let resolve_count = Arc::new(AtomicUsize::new(0));
    let registry = registry_server(
        RelayRouteInfo {
            name: expected.route_name.clone(),
            relay_url: data_plane.url.clone(),
            route_uid: "route-uid-stale".to_string(),
            route_revision: 1,
            ipc_address: None,
            server_id: None,
            server_instance_id: None,
            crm_ns: expected.crm_ns.clone(),
            crm_name: expected.crm_name.clone(),
            crm_ver: expected.crm_ver.clone(),
            abi_hash: expected.abi_hash.clone(),
            signature_hash: expected.signature_hash.clone(),
            max_payload_size: 1024 * 1024,
        },
        resolve_count.clone(),
    );
    let runtime = Runtime::new(runtime_options(
        unique_name("stale-runtime"),
        Some(registry.url.clone()),
    ))
    .expect("runtime");
    let client = runtime
        .connect(expected, Connect::RelayAware)
        .expect("relay-aware client");

    let Error::Semantic(error) = client
        .call_owned("echo", b"request")
        .expect_err("second stale result must be terminal")
    else {
        panic!("stale route must remain a semantic error");
    };
    assert_eq!(error.code, ErrorCode::RouteStale);
    assert_eq!(
        resolve_count.load(Ordering::SeqCst),
        2,
        "Core permits exactly one fresh resolution after the cached route is stale"
    );
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "the refreshed resolution must not replay through the already-rejected relay endpoint"
    );
}

#[test]
fn service_error_is_not_replayed() {
    struct Fails {
        calls: AtomicU64,
    }

    impl EncodedService for Fails {
        fn invoke(&self, _method_index: u16, _request: &[u8]) -> Result<Vec<u8>, C2Error> {
            self.calls.fetch_add(1, Ordering::AcqRel);
            Err(
                C2Error::new(ErrorCode::ResourceFunctionExecuting, "service failed")
                    .with_details(BTreeMap::from([("source".to_string(), "test".to_string())])),
            )
        }
    }

    let service = Arc::new(Fails {
        calls: AtomicU64::new(0),
    });
    let runtime = Runtime::new(runtime_options(unique_name("no-replay"), None)).expect("runtime");
    let host = runtime.host(HostOptions::default()).expect("host");
    let route_name = unique_name("failure");
    let release = release();
    let definition = ServiceDefinition::new(
        &release,
        release.reference(),
        &route_name,
        [
            MethodDefinition {
                index: 0,
                name: "ping".to_string(),
                access: MethodAccess::Read,
            },
            MethodDefinition {
                index: 1,
                name: "echo".to_string(),
                access: MethodAccess::Write,
            },
        ],
        service.clone(),
    )
    .expect("definition");
    let _registration = host.register(definition).expect("registration");
    let client = runtime
        .connect(
            release.expected_route(route_name).expect("expected route"),
            Connect::DirectIpc {
                address: runtime.server_address().expect("server address"),
            },
        )
        .expect("client");

    let Error::Semantic(error) = client
        .call_owned("echo", b"request")
        .expect_err("service error")
    else {
        panic!("service C2Error must remain semantic");
    };
    assert_eq!(error.code, ErrorCode::ResourceFunctionExecuting);
    assert_eq!(service.calls.load(Ordering::Acquire), 1);
}
