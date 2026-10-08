//! Runtime projection of endpoint namespaces; namespace agreement is only an IPC hint.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    Json, Router,
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use c2_contract::{ContractRelease, MethodAccess};
use c2_core::{
    CallOptions, CallTimeout, ConfigSources, Connect, EncodedClient, EncodedService, Error,
    HostOptions, LocalEndpointOptions, MethodDefinition, ObservedPath, Runtime, RuntimeOptions,
    ServiceDefinition,
};
use c2_error::{C2Error, ErrorCode};
use c2_http::client::{LOCAL_ENDPOINT_NAMESPACE_HEADER, RelayRouteInfo};
use serde_json::Value;

const DESCRIPTOR: &[u8] =
    include_bytes!("../../../../tests/fixtures/contracts/portable-release.contract.json");
const WAIT: Duration = Duration::from_secs(5);

fn expected() -> c2_contract::ExpectedRouteContract {
    ContractRelease::from_descriptor_json(DESCRIPTOR)
        .unwrap()
        .expected_route("route")
        .unwrap()
}

fn runtime(relay: Option<String>) -> Runtime {
    Runtime::new(RuntimeOptions {
        server_id: Some("same-owner".into()),
        relay_anchor_address: relay,
        use_process_relay_anchor: false,
        ..Default::default()
    })
    .unwrap()
}

struct Marker(u8);
impl EncodedService for Marker {
    fn invoke(&self, method: u16, request: &[u8]) -> Result<Vec<u8>, C2Error> {
        match method {
            0 => Ok(Vec::new()),
            1 => Ok([&[self.0], request].concat()),
            _ => Err(C2Error::new(ErrorCode::ProtocolViolation, "unknown method")),
        }
    }
}

fn definition(marker: u8) -> ServiceDefinition {
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap();
    ServiceDefinition::new(
        &release,
        release.reference(),
        "route",
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
        Arc::new(Marker(marker)),
    )
    .unwrap()
}

#[derive(Clone)]
struct HttpState {
    route: RelayRouteInfo,
    namespace: Option<String>,
    requests: Arc<Mutex<Vec<(String, Option<String>, Value)>>>,
    before_resolve: Arc<dyn Fn() + Send + Sync>,
    calls: Arc<AtomicUsize>,
}

fn record(state: &HttpState, path: &str, headers: HeaderMap, body: Value) {
    state.requests.lock().unwrap().push((
        path.into(),
        headers
            .get(LOCAL_ENDPOINT_NAMESPACE_HEADER)
            .map(|v| v.to_str().unwrap().into()),
        body,
    ));
}

async fn resolve(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Query(query): Query<std::collections::BTreeMap<String, String>>,
) -> (HeaderMap, Json<Vec<RelayRouteInfo>>) {
    let expected = expected();
    for (key, value) in [
        ("crm_ns", expected.crm_ns),
        ("crm_name", expected.crm_name),
        ("crm_ver", expected.crm_ver),
        ("abi_hash", expected.abi_hash),
        ("signature_hash", expected.signature_hash),
    ] {
        assert_eq!(query.get(key), Some(&value), "full route contract query");
    }
    record(
        &state,
        "resolve",
        headers,
        serde_json::to_value(query).unwrap(),
    );
    (state.before_resolve)();
    let mut headers = HeaderMap::new();
    if let Some(namespace) = state.namespace {
        headers.insert(LOCAL_ENDPOINT_NAMESPACE_HEADER, namespace.parse().unwrap());
    }
    (headers, Json(vec![state.route]))
}

async fn register(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> StatusCode {
    let prepare = body.get("prepare_only").and_then(Value::as_bool) == Some(true);
    record(&state, "register", headers, body);
    if prepare {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    }
}

async fn unregister(
    State(state): State<HttpState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> StatusCode {
    record(&state, "unregister", headers, body);
    StatusCode::OK
}

async fn call(
    State(state): State<HttpState>,
    Path((_, method)): Path<(String, String)>,
    body: Bytes,
) -> (StatusCode, Vec<u8>) {
    state.calls.fetch_add(1, Ordering::SeqCst);
    if method == "absent" {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            C2Error::new(ErrorCode::ResourceFunctionExecuting, "service failure").to_wire_bytes(),
        )
    } else {
        (StatusCode::OK, [&b"H"[..], body.as_ref()].concat())
    }
}

struct HttpFixture {
    url: String,
    state: HttpState,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl HttpFixture {
    fn new(namespace: Option<String>, before_resolve: impl Fn() + Send + Sync + 'static) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let expected = expected();
        let state = HttpState {
            route: RelayRouteInfo {
                name: expected.route_name,
                relay_url: url.clone(),
                route_uid: "route-uid".into(),
                route_revision: 1,
                ipc_address: Some("ipc://same-owner".into()),
                server_id: Some("same-owner".into()),
                server_instance_id: Some("candidate-instance".into()),
                crm_ns: expected.crm_ns,
                crm_name: expected.crm_name,
                crm_ver: expected.crm_ver,
                abi_hash: expected.abi_hash,
                signature_hash: expected.signature_hash,
                max_payload_size: 1024 * 1024,
            },
            namespace,
            requests: Arc::default(),
            before_resolve: Arc::new(before_resolve),
            calls: Arc::default(),
        };
        let serving = state.clone();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                let app = Router::new()
                    .route("/_resolve/{name}", get(resolve))
                    .route("/_register", post(register))
                    .route("/_unregister", post(unregister))
                    .route("/_probe/{name}", get(|| async { StatusCode::OK }))
                    .route("/{name}/{method}", post(call))
                    .with_state(serving);
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                ready_tx.send(()).unwrap();
                axum::serve(listener, app)
                    .with_graceful_shutdown(async {
                        let _ = rx.await;
                    })
                    .await
                    .unwrap();
            });
        });
        ready_rx.recv_timeout(WAIT).unwrap();
        Self {
            url,
            state,
            shutdown: Some(tx),
            thread: Some(thread),
        }
    }
}

impl Drop for HttpFixture {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn assert_http_call(client: &c2_core::Client, fixture: &HttpFixture) {
    let calls = fixture.state.calls.load(Ordering::SeqCst);
    let controlled = client.with_call_options(CallOptions::with_timeout(CallTimeout::After(WAIT)));
    assert_eq!(
        controlled.call_owned("echo", b"request").unwrap(),
        b"Hrequest"
    );
    assert!(
        matches!(controlled.call_owned("absent", b""), Err(Error::Semantic(error)) if error.code == ErrorCode::ResourceFunctionExecuting)
    );
    assert_eq!(
        fixture.state.calls.load(Ordering::SeqCst),
        calls + 2,
        "canonical service errors must not replay business calls"
    );
}

#[test]
fn explicit_http_with_invalid_process_root_stays_usable() {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "http_only_child", "--ignored", "--nocapture"])
        .env("C2_IPC_ROOT", "invalid-relative-root")
        .env_remove("C2_ENV_FILE")
        .env_remove("C2_RELAY_ANCHOR_ADDRESS")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
#[ignore = "isolated process fixture, invoked by parent"]
fn http_only_child() {
    let fixture = HttpFixture::new(None, || {});
    let runtime = runtime(None);
    let client = runtime
        .connect(
            expected(),
            Connect::ExplicitRelay {
                relay_url: fixture.url.clone(),
            },
        )
        .unwrap();
    assert_eq!(client.observed_path(), ObservedPath::ExplicitRelay);
    assert_http_call(&client, &fixture);
    runtime.set_relay_anchor_address(Some(fixture.url.clone()));
    let client = runtime.connect(expected(), Connect::RelayAware).unwrap();
    assert_eq!(client.observed_path(), ObservedPath::RelayAwareRelay);
    assert_http_call(&client, &fixture);
    assert!(!runtime.local_endpoint_frozen());
    assert!(!runtime.client_config_frozen());
    assert!(runtime.outgoing_memory_stats().is_none());
    assert!(runtime.local_endpoint_context().is_err());
    assert!(
        fixture
            .state
            .requests
            .lock()
            .unwrap()
            .iter()
            .all(|(_, namespace, _)| namespace.is_none())
    );
}

#[cfg(windows)]
#[test]
fn windows_default_projection_remains_named_pipe_and_rejects_unix_root() {
    let runtime = runtime(None);
    runtime
        .set_local_endpoint_with_sources(LocalEndpointOptions::default(), ConfigSources::empty())
        .unwrap();
    let context = runtime.local_endpoint_context().unwrap();
    assert_eq!(
        context.platform_kind(),
        c2_core::LocalEndpointNamespace::WindowsNamedPipe
    );
    assert!(context.windows_logon_scope_id().is_some());
    assert!(
        runtime
            .local_endpoint("ipc://same-owner")
            .unwrap()
            .os_name()
            .to_string_lossy()
            .starts_with(r"\\.\pipe\")
    );
    let error = runtime
        .set_local_endpoint_with_sources(
            LocalEndpointOptions {
                unix_root: Some(r"C:\root".into()),
            },
            ConfigSources::empty(),
        )
        .unwrap_err();
    assert!(error.to_string().contains("not applicable"));
    assert_eq!(runtime.local_endpoint_context().unwrap(), context);
    assert!(!runtime.local_endpoint_frozen());
}

#[cfg(unix)]
mod unix {
    use super::*;
    use c2_http::relay::{RelayConfig, RelayServer};
    use std::path::{Path as FsPath, PathBuf};
    use std::sync::atomic::AtomicBool;

    struct Roots(PathBuf);
    impl Roots {
        fn new() -> Self {
            let path = PathBuf::from(format!(
                "/tmp/r{}",
                &uuid::Uuid::new_v4().simple().to_string()[..8]
            ));
            std::fs::create_dir(&path).unwrap();
            for name in ["a", "b", "c"] {
                std::fs::create_dir(path.join(name)).unwrap();
            }
            Self(path)
        }
        fn a(&self) -> PathBuf {
            self.0.join("a")
        }
        fn b(&self) -> PathBuf {
            self.0.join("b")
        }
        fn c(&self) -> PathBuf {
            self.0.join("c")
        }
    }
    impl Drop for Roots {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).unwrap();
        }
    }
    fn at(runtime: &Runtime, root: &FsPath) {
        runtime
            .set_local_endpoint_with_sources(
                LocalEndpointOptions {
                    unix_root: Some(root.into()),
                },
                ConfigSources::empty(),
            )
            .unwrap();
    }
    fn from_map(runtime: &Runtime, root: &FsPath) {
        runtime
            .set_local_endpoint_with_sources(
                LocalEndpointOptions::default(),
                ConfigSources {
                    env_file: c2_core::EnvFilePolicy::Disabled,
                    process_env: c2_core::EnvMap::from([(
                        "C2_IPC_ROOT".into(),
                        root.to_str().unwrap().into(),
                    )]),
                },
            )
            .unwrap();
    }

    #[test]
    fn real_relay_and_two_roots_select_the_correct_owner() {
        let roots = Roots::new();
        let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = probe.local_addr().unwrap();
        drop(probe);
        let url = format!("http://{address}");
        let context = c2_core::LocalEndpointContext::with_unix_root(&roots.a()).unwrap();
        let mut relay = RelayServer::start_with_context(
            RelayConfig {
                bind: address.to_string(),
                advertise_url: url.clone(),
                idle_timeout_secs: 0,
                anti_entropy_interval: Duration::ZERO,
                heartbeat_interval: Duration::ZERO,
                ..Default::default()
            },
            context,
        )
        .unwrap();
        let owner_a = runtime(Some(url.clone()));
        at(&owner_a, &roots.a());
        let host_a = owner_a.host(HostOptions::default()).unwrap();
        let registration_a = host_a.register(definition(b'A')).unwrap();
        assert!(registration_a.outcome().relay_registered);
        let owner_b = runtime(None);
        at(&owner_b, &roots.b());
        let host_b = owner_b
            .host(HostOptions::default().without_relay())
            .unwrap();
        let _registration_b = host_b.register(definition(b'B')).unwrap();
        assert_eq!(
            host_a.local_endpoint().address(),
            host_b.local_endpoint().address()
        );
        assert_ne!(
            host_a.local_endpoint_context(),
            host_b.local_endpoint_context()
        );
        let client_a = runtime(Some(url.clone()));
        from_map(&client_a, &roots.a());
        let ipc = client_a.connect(expected(), Connect::RelayAware).unwrap();
        assert!(client_a.local_endpoint_frozen());
        assert_eq!(
            client_a.local_endpoint_context().unwrap(),
            *host_a.local_endpoint_context()
        );
        assert_eq!(ipc.observed_path(), ObservedPath::RelayAwareLocalIpc);
        assert_eq!(ipc.call_owned("echo", b"x").unwrap(), b"Ax");
        let client_b = runtime(Some(url.clone()));
        at(&client_b, &roots.b());
        let http = client_b.connect(expected(), Connect::RelayAware).unwrap();
        assert_eq!(http.observed_path(), ObservedPath::RelayAwareRelay);
        assert_eq!(
            http.with_call_options(CallOptions::with_timeout(CallTimeout::After(WAIT)))
                .call_owned("echo", b"x")
                .unwrap(),
            b"Ax"
        );
        assert!(!client_b.local_endpoint_frozen());
        let local_b = client_b
            .connect(
                expected(),
                Connect::DirectIpc {
                    address: "ipc://same-owner".into(),
                },
            )
            .unwrap();
        assert_eq!(local_b.call_owned("echo", b"x").unwrap(), b"Bx");
        // Namespace mismatch never withdraws the owner's route.
        let explicit = client_a
            .connect(expected(), Connect::ExplicitRelay { relay_url: url })
            .unwrap();
        assert_eq!(
            explicit.call_owned("echo", b"still-live").unwrap(),
            b"Astill-live"
        );
        let mut wrong = expected();
        wrong.signature_hash = "0".repeat(64);
        let error = client_a
            .connect(wrong, Connect::RelayAware)
            .expect_err("contract-scoped relay resolve must have no matching route");
        let Error::Semantic(error) = error else {
            panic!("relay resolve contract filtering must return a semantic error");
        };
        assert_eq!(error.code, ErrorCode::ResourceNotFound);
        assert_eq!(
            error.details.get("route").map(String::as_str),
            Some("route")
        );
        let after_mismatch = client_a.connect(expected(), Connect::RelayAware).unwrap();
        assert_eq!(
            after_mismatch.observed_path(),
            ObservedPath::RelayAwareLocalIpc
        );
        assert_eq!(after_mismatch.observed_route(), ipc.observed_route());
        assert_eq!(
            after_mismatch
                .call_owned("echo", b"after-mismatch")
                .unwrap(),
            b"Aafter-mismatch"
        );
        drop((ipc, http, explicit, local_b, after_mismatch));
        client_a.shutdown_without_host(WAIT);
        client_b.shutdown_without_host(WAIT);
        assert!(host_a.shutdown().relay_errors.is_empty());
        assert!(host_b.shutdown().runtime_barrier_error.is_none());
        relay.stop().unwrap();
    }

    #[test]
    fn custom_root_legacy_or_wrong_namespace_uses_http_without_local_io() {
        let roots = Roots::new();
        let context = c2_core::LocalEndpointContext::with_unix_root(&roots.b()).unwrap();
        for namespace in [None, Some(context.namespace_id().into())] {
            let fixture = HttpFixture::new(namespace, || {});
            let runtime = runtime(Some(fixture.url.clone()));
            at(&runtime, &roots.a());
            let client = runtime.connect(expected(), Connect::RelayAware).unwrap();
            assert_eq!(client.observed_path(), ObservedPath::RelayAwareRelay);
            assert_http_call(&client, &fixture);
            assert!(!runtime.local_endpoint_frozen());
            assert!(runtime.outgoing_memory_stats().is_none());
            assert!(
                fixture
                    .state
                    .requests
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|(path, _, _)| path != "unregister")
            );
        }
    }

    #[test]
    fn setter_between_resolve_and_acquire_reselects_http_instead_of_another_root() {
        let roots = Roots::new();
        let runtime = runtime(None);
        at(&runtime, &roots.a());
        let captured = runtime.local_endpoint_context().unwrap();
        let identity = runtime.ensure_server().unwrap();
        let setter = runtime.clone();
        let root_b = roots.b();
        let changed = AtomicBool::new(false);
        let fixture = HttpFixture::new(Some(captured.namespace_id().into()), move || {
            if !changed.swap(true, Ordering::SeqCst) {
                from_map(&setter, &root_b);
            }
        });
        runtime.set_relay_anchor_address(Some(fixture.url.clone()));
        let client = runtime.connect(expected(), Connect::RelayAware).unwrap();
        assert_eq!(client.observed_path(), ObservedPath::RelayAwareRelay);
        assert_http_call(&client, &fixture);
        assert!(!runtime.local_endpoint_frozen());
        assert_eq!(client.expected_route(), &expected());
        assert_eq!(runtime.ensure_server().unwrap(), identity);
        assert_eq!(
            runtime.local_endpoint_context().unwrap().unix_root(),
            Some(roots.b().as_path())
        );
        assert!(!runtime.client_config_frozen());
        assert!(runtime.outgoing_memory_stats().is_none());
        assert!(
            fixture
                .state
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|(path, namespace, _)| path != "unregister"
                    && namespace.as_deref() == Some(captured.namespace_id()))
        );
        at(&runtime, &roots.c());
        assert!(!runtime.local_endpoint_frozen());
        let owner = super::runtime(None);
        at(&owner, &roots.c());
        let host = owner.host(HostOptions::default().without_relay()).unwrap();
        let _registration = host.register(definition(b'C')).unwrap();
        let local = runtime
            .connect(
                expected(),
                Connect::DirectIpc {
                    address: "ipc://same-owner".into(),
                },
            )
            .unwrap();
        assert_eq!(
            local.call_owned("echo", b"correct-root").unwrap(),
            b"Ccorrect-root"
        );
        assert!(runtime.local_endpoint_frozen());
        assert_eq!(
            runtime.local_endpoint_context().unwrap(),
            *host.local_endpoint_context()
        );
        assert!(
            runtime
                .set_local_endpoint_with_sources(
                    LocalEndpointOptions {
                        unix_root: Some(roots.b())
                    },
                    ConfigSources::empty()
                )
                .is_err()
        );
        // Namespace remains a hint: a matching namespace with a wrong instance
        // must still fail the native handshake identity check.
        let bad_identity = HttpFixture::new(
            Some(host.local_endpoint_context().namespace_id().into()),
            || {},
        );
        let checker = super::runtime(Some(bad_identity.url.clone()));
        at(&checker, &roots.c());
        assert!(
            matches!(checker.connect(expected(), Connect::RelayAware), Err(Error::Semantic(error)) if error.code == ErrorCode::IdentityMismatch)
        );
        assert!(checker.local_endpoint_frozen());
        assert_eq!(
            checker.local_endpoint_context().unwrap(),
            *host.local_endpoint_context()
        );
        checker.shutdown_without_host(WAIT);
        let mut wrong = expected();
        wrong.signature_hash = "0".repeat(64);
        assert!(
            matches!(runtime.connect(wrong, Connect::DirectIpc { address: "ipc://same-owner".into() }), Err(Error::Semantic(error)) if error.code == ErrorCode::ContractMismatch)
        );
        assert_eq!(
            local.call_owned("echo", b"still-live").unwrap(),
            b"Cstill-live"
        );
        assert!(
            bad_identity
                .state
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|(path, _, _)| path != "unregister")
        );
        assert_http_call(&client, &fixture);
        drop(local);
        runtime.shutdown_without_host(WAIT);
        assert!(host.shutdown().runtime_barrier_error.is_none());
    }

    #[test]
    fn process_root_flip_between_selection_and_acquire_is_confined_to_child() {
        let roots = Roots::new();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "unix::environment_flip_child",
                "--ignored",
                "--nocapture",
            ])
            .env("C2_IPC_ROOT", roots.a())
            .env("C2_NAMESPACE_OTHER_ROOT", roots.b())
            .env("C2_NAMESPACE_FINAL_ROOT", roots.c())
            .env_remove("C2_ENV_FILE")
            .env_remove("C2_RELAY_ANCHOR_ADDRESS")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    #[ignore = "isolated environment fixture, invoked by parent"]
    fn environment_flip_child() {
        let runtime = runtime(None);
        let captured = runtime.local_endpoint_context().unwrap();
        let root_b = PathBuf::from(std::env::var("C2_NAMESPACE_OTHER_ROOT").unwrap());
        let new_root = root_b.clone();
        let fixture = HttpFixture::new(Some(captured.namespace_id().into()), move || {
            // Only this exact child test changes its environment, before the
            // caller leaves resolve and performs the Runtime freeze.
            unsafe {
                std::env::set_var("C2_IPC_ROOT", &new_root);
            }
        });
        runtime.set_relay_anchor_address(Some(fixture.url.clone()));
        let client = runtime.connect(expected(), Connect::RelayAware).unwrap();
        assert_eq!(client.observed_path(), ObservedPath::RelayAwareRelay);
        assert_http_call(&client, &fixture);
        assert!(!runtime.local_endpoint_frozen());
        assert_eq!(client.expected_route(), &expected());
        assert_eq!(
            runtime.local_endpoint_context().unwrap().unix_root(),
            Some(root_b.as_path())
        );
        assert!(runtime.outgoing_memory_stats().is_none());
        assert!(!runtime.client_config_frozen());
        assert!(
            fixture
                .state
                .requests
                .lock()
                .unwrap()
                .iter()
                .all(|(path, header, _)| path != "unregister"
                    && header.as_deref() == Some(captured.namespace_id()))
        );
        let root_c = PathBuf::from(std::env::var("C2_NAMESPACE_FINAL_ROOT").unwrap());
        unsafe {
            std::env::set_var("C2_IPC_ROOT", &root_c);
        }
        let current = runtime.local_endpoint_context().unwrap();
        assert_eq!(current.unix_root(), Some(root_c.as_path()));
        assert!(!runtime.local_endpoint_frozen());
        // A real failed local connection attempt still fixes the selected domain.
        assert!(matches!(
            runtime.connect(
                expected(),
                Connect::DirectIpc {
                    address: "ipc://absent-owner".into()
                }
            ),
            Err(Error::Transport(_))
        ));
        assert!(runtime.local_endpoint_frozen());
        assert!(runtime.client_config_frozen());
        assert_eq!(runtime.local_endpoint_context().unwrap(), current);
        assert!(
            runtime
                .set_local_endpoint_with_sources(
                    LocalEndpointOptions {
                        unix_root: Some(root_b)
                    },
                    ConfigSources::empty()
                )
                .is_err()
        );
        runtime.shutdown_without_host(WAIT);
    }

    #[test]
    fn resource_prepare_publish_and_cleanup_use_the_host_frozen_header() {
        let roots = Roots::new();
        let fixture = HttpFixture::new(None, || {});
        let runtime = runtime(Some(fixture.url.clone()));
        at(&runtime, &roots.a());
        // Discovery used a headerless control object with a query-specific
        // context. Registration must rebuild that control with the Host context.
        let discovered = runtime.connect(expected(), Connect::RelayAware).unwrap();
        assert_http_call(&discovered, &fixture);
        assert!(!runtime.local_endpoint_frozen());
        let host = runtime.host(HostOptions::default()).unwrap();
        let namespace = host.local_endpoint_context().namespace_id().to_owned();
        let explicit = runtime
            .connect(
                expected(),
                Connect::ExplicitRelay {
                    relay_url: fixture.url.clone(),
                },
            )
            .unwrap();
        assert_http_call(&explicit, &fixture);
        for marker in [b'A', b'B'] {
            let mut registration = host.register(definition(marker)).unwrap();
            registration.close().unwrap();
        }
        let requests = fixture.state.requests.lock().unwrap();
        let controls: Vec<_> = requests
            .iter()
            .filter(|(path, _, _)| path != "resolve")
            .collect();
        assert_eq!(controls.len(), 6);
        for chunk in controls.chunks(3) {
            assert_eq!(chunk[0].0, "register");
            assert_eq!(chunk[0].2["prepare_only"], true);
            assert_eq!(chunk[1].0, "register");
            assert!(chunk[1].2.get("prepare_only").is_none());
            assert_eq!(chunk[2].0, "unregister");
            assert_eq!(
                chunk[2].2["server_instance_id"],
                runtime.ensure_server().unwrap().server_instance_id
            );
            assert!(
                chunk
                    .iter()
                    .all(|(_, header, _)| header.as_deref() == Some(namespace.as_str()))
            );
        }
        drop(requests);
        assert!(host.shutdown().relay_errors.is_empty());
    }
}
