//! User-boundary deadline tests use native hosts and event-controlled replies.
//! A timeout is never implemented by dropping a transport future in this fixture.
use std::collections::BTreeMap;
use std::net::TcpListener;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use c2_config::{ClientIpcConfigOverrides, ConfigSources, ServerIpcConfigOverrides};
use c2_contract::{ContractRelease, MethodAccess};
use c2_core::{
    CallExecutionLimitsOverrides, CallOptions, CallTimeout, Client, Connect, EncodedClient,
    EncodedService, Error, Host, HostOptions, MethodDefinition, Registration, Runtime,
    RuntimeOptions, ServiceConcurrencyMode, ServiceDefinition,
};
use c2_error::{C2Error, ErrorCode};
use c2_http::client::RelayRouteInfo;

const DESCRIPTOR: &str =
    include_str!("../../../../tests/fixtures/contracts/portable-release.contract.json");
const DEADLINE: Duration = Duration::from_secs(1);
const EVENT_WAIT: Duration = Duration::from_secs(10);
static TEST_ID: AtomicU64 = AtomicU64::new(0);

fn unique(prefix: &str) -> String {
    format!(
        "{prefix}-{}-{}",
        std::process::id(),
        TEST_ID.fetch_add(1, Ordering::Relaxed)
    )
}

fn finite() -> CallOptions {
    CallOptions::with_timeout(CallTimeout::After(DEADLINE))
}

fn unlimited() -> CallOptions {
    CallOptions::with_timeout(CallTimeout::Unlimited)
}

fn assert_semantic(error: Error, code: ErrorCode, phase: &str) {
    let Error::Semantic(error) = error else {
        panic!("expected semantic {code:?}, got {error:?}")
    };
    assert_eq!(error.code, code);
    assert_eq!(
        error.details.get("transport_phase").map(String::as_str),
        Some(phase)
    );
}

// Observation is bounded, but never cancels execution or creates another waiter.
fn observe_until(mut predicate: impl FnMut() -> bool) {
    let until = Instant::now() + EVENT_WAIT;
    while !predicate() {
        assert!(
            Instant::now() < until,
            "native ownership did not reach the expected state"
        );
        std::thread::yield_now();
    }
}

#[derive(Default)]
struct Gate {
    opened: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    fn open(&self) {
        *self.opened.lock().unwrap() = true;
        self.changed.notify_all();
    }

    fn wait(&self) {
        let opened = self.opened.lock().unwrap();
        let (opened, timeout) = self
            .changed
            .wait_timeout_while(opened, EVENT_WAIT, |v| !*v)
            .unwrap();
        assert!(
            *opened && !timeout.timed_out(),
            "test controller did not release business callback"
        );
    }
}

struct GatedEcho {
    entered: mpsc::Sender<()>,
    gate: Arc<Gate>,
    calls: Arc<AtomicUsize>,
}

impl EncodedService for GatedEcho {
    fn invoke(&self, method: u16, request: &[u8]) -> Result<Vec<u8>, C2Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if method == 1 && request.first() == Some(&0xa5) {
            self.entered.send(()).expect("callback entry event");
            self.gate.wait();
        }
        Ok(request.to_vec())
    }
}

#[derive(Clone, Copy)]
enum Carrier {
    Inline,
    Buddy,
    Dedicated,
    Chunk,
}

struct IpcFixture {
    runtime: Runtime,
    host: Host,
    registration: Registration,
    client: Client,
    gate: Arc<Gate>,
    entered: mpsc::Receiver<()>,
    calls: Arc<AtomicUsize>,
}

impl Drop for IpcFixture {
    fn drop(&mut self) {
        self.gate.open();
        // Callback release precedes close, even when an assertion unwinds.
        let _ = self.registration.close();
        let _ = self.host.shutdown();
    }
}

fn fixture(carrier: Carrier) -> IpcFixture {
    let release = ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes()).unwrap();
    let route = unique("deadline");
    let mut server = ServerIpcConfigOverrides {
        pool_segment_size: Some(1024 * 1024),
        pool_prewarm_segments: Some(0),
        pool_min_retained_segments: Some(0),
        chunk_size: Some(4096),
        chunk_gc_interval_secs: Some(0.05),
        ..Default::default()
    };
    let mut outgoing = ClientIpcConfigOverrides {
        pool_segment_size: Some(1024 * 1024),
        pool_prewarm_segments: Some(0),
        pool_min_retained_segments: Some(0),
        chunk_size: Some(4096),
        chunk_gc_interval_secs: Some(0.05),
        ..Default::default()
    };
    match carrier {
        Carrier::Inline | Carrier::Buddy => {}
        Carrier::Dedicated => {
            server.pool_enabled = Some(false);
            outgoing.pool_enabled = Some(false);
        }
        Carrier::Chunk => {
            server.pool_enabled = Some(false);
            outgoing.pool_enabled = Some(false);
            server.shm_backing_budget_bytes = Some(0);
            outgoing.shm_backing_budget_bytes = Some(0);
            // Chunk reception still needs legal owned reassembly backing.
            // SHM stays forbidden; a finite file budget supplies that backing.
            server.file_backing_budget_bytes = Some(2 * 1024 * 1024);
            outgoing.file_backing_budget_bytes = Some(2 * 1024 * 1024);
        }
    }
    let runtime = Runtime::new(RuntimeOptions {
        server_id: Some(unique("deadline-host")),
        server_ipc_overrides: Some(server),
        client_ipc_overrides: Some(outgoing),
        shm_threshold: Some(1024),
        use_process_relay_anchor: false,
        ..Default::default()
    })
    .unwrap();
    let host = runtime
        .host(HostOptions::default().without_relay())
        .unwrap();
    let gate = Arc::new(Gate::default());
    let calls = Arc::new(AtomicUsize::new(0));
    let (entered_tx, entered) = mpsc::channel();
    let definition = ServiceDefinition::new(
        &release,
        release.reference(),
        &route,
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
        Arc::new(GatedEcho {
            entered: entered_tx,
            gate: gate.clone(),
            calls: calls.clone(),
        }),
    )
    .unwrap()
    .with_concurrency(ServiceConcurrencyMode::Parallel, None, None)
    .unwrap();
    let registration = host.register(definition).unwrap();
    let client = runtime
        .connect(
            release.expected_route(route).unwrap(),
            Connect::DirectIpc {
                address: runtime.server_address().unwrap(),
            },
        )
        .unwrap();
    IpcFixture {
        runtime,
        host,
        registration,
        client,
        gate,
        entered,
        calls,
    }
}

fn check_late_ipc(carrier: Carrier) {
    let fixture = fixture(carrier);
    let bytes = vec![
        0xa5;
        if matches!(carrier, Carrier::Inline) {
            512
        } else {
            128 * 1024
        }
    ];
    let nbytes = bytes.len() as u64;
    let caller = fixture.client.with_call_options(finite());
    let call = std::thread::spawn(move || {
        let result = caller
            .begin_call("echo")
            .unwrap()
            .encode_vec(bytes)
            .unwrap()
            .call_held();
        caller.close();
        result
    });
    fixture
        .entered
        .recv_timeout(EVENT_WAIT)
        .expect("real business dispatch before expiry");
    let error = match call.join().unwrap() {
        Err(error) => error,
        Ok(_) => panic!("gated call cannot succeed"),
    };
    assert_semantic(error, ErrorCode::CallDeadlineExceeded, "dispatch_uncertain");
    let snapshot = fixture.runtime.call_execution_snapshot().unwrap();
    assert_eq!(snapshot.used_operations, 1);
    assert_eq!(snapshot.used_retained_bytes, nbytes);
    assert_eq!(
        fixture.runtime.clone().call_execution_snapshot().unwrap(),
        snapshot
    );
    // A distinct view on the same native connection must still finish while
    // the timed-out call's transport/request owners are live.
    assert_eq!(
        fixture
            .client
            .with_call_options(unlimited())
            .call_owned("ping", b"other")
            .unwrap(),
        b"other"
    );
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
    fixture.gate.open();
    observe_until(|| {
        fixture
            .runtime
            .call_execution_snapshot()
            .unwrap()
            .used_operations
            == 0
    });
    assert_eq!(
        fixture
            .runtime
            .call_execution_snapshot()
            .unwrap()
            .used_retained_bytes,
        0
    );
    if matches!(carrier, Carrier::Dedicated | Carrier::Chunk) {
        // Late held responses are released on execution completion, rather
        // than leaving the server response handle or reassembly retained.
        observe_until(|| {
            let stats = fixture.host.memory_stats().server.unwrap();
            stats.shm.used_bytes == 0
                && stats.file.used_bytes == 0
                && stats.reassembly.used_bytes == 0
        });
    }
    let stats = fixture.host.memory_stats().server.unwrap();
    let outgoing = fixture.runtime.outgoing_memory_stats().unwrap();
    match carrier {
        Carrier::Inline => {
            assert_eq!(stats.shm.peak_bytes, 0);
            assert_eq!(stats.reassembly.peak_bytes, 0);
            assert_eq!(outgoing.shm.peak_bytes, 0);
        }
        Carrier::Buddy | Carrier::Dedicated => {
            // No chunk reassembly/file fallback occurred in either direction.
            assert!(stats.shm.peak_bytes >= nbytes);
            assert!(outgoing.shm.peak_bytes >= nbytes);
            assert_eq!(stats.reassembly.peak_bytes, 0);
            assert_eq!(outgoing.reassembly.peak_bytes, 0);
            assert_eq!(stats.file.peak_bytes, 0);
            assert_eq!(outgoing.file.peak_bytes, 0);
        }
        Carrier::Chunk => {
            assert_eq!(stats.shm.peak_bytes, 0);
            assert_eq!(outgoing.shm.peak_bytes, 0);
            assert!(stats.reassembly.peak_bytes >= nbytes);
            assert!(outgoing.reassembly.peak_bytes >= nbytes);
            assert!(stats.file.peak_bytes >= nbytes);
            assert!(outgoing.file.peak_bytes >= nbytes);
        }
    }
    assert_eq!(
        fixture.client.call_owned("echo", b"after-late").unwrap(),
        b"after-late"
    );
}

#[test]
fn ipc_inline_waiter_expiry_keeps_transport_and_other_calls_alive() {
    check_late_ipc(Carrier::Inline);
}
#[test]
fn ipc_buddy_waiter_expiry_keeps_transport_and_other_calls_alive() {
    check_late_ipc(Carrier::Buddy);
}
#[test]
fn ipc_dedicated_late_held_releases_native_response() {
    check_late_ipc(Carrier::Dedicated);
}
#[test]
fn ipc_chunk_late_held_releases_native_reassembly() {
    check_late_ipc(Carrier::Chunk);
}

#[test]
fn zero_deadline_and_zero_budget_reject_before_serializer_or_business() {
    let fixture = fixture(Carrier::Inline);
    fixture
        .runtime
        .set_call_execution_limits(CallExecutionLimitsOverrides {
            max_outstanding_calls: Some(0),
            retained_input_budget_bytes: Some(0),
        })
        .unwrap();
    let zero = fixture
        .client
        .with_call_options(CallOptions::with_timeout(CallTimeout::After(
            Duration::ZERO,
        )));
    let error = match zero.begin_call("echo") {
        Err(error) => error,
        Ok(_) => panic!("zero deadline admitted"),
    };
    assert_semantic(error, ErrorCode::CallDeadlineExceeded, "pre_dispatch");
    let finite = fixture.client.with_call_options(finite());
    let error = match finite.begin_call("echo") {
        Err(error) => error,
        Ok(_) => panic!("zero slots admitted"),
    };
    assert_semantic(error, ErrorCode::CallCapacityExceeded, "pre_dispatch");
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture
            .runtime
            .call_execution_snapshot()
            .unwrap()
            .used_operations,
        0
    );
    assert_eq!(
        fixture
            .client
            .with_call_options(unlimited())
            .call_owned("echo", b"unlimited")
            .unwrap(),
        b"unlimited"
    );
}

#[test]
fn reservation_precedes_serializer_and_byte_rejection_precedes_materialization() {
    let fixture = fixture(Carrier::Inline);
    fixture
        .runtime
        .set_call_execution_limits(CallExecutionLimitsOverrides {
            max_outstanding_calls: Some(1),
            retained_input_budget_bytes: Some(8),
        })
        .unwrap();
    let client = fixture.client.with_call_options(finite());
    let mut first = client.begin_call("echo").unwrap();
    first.charge_input(8).unwrap();
    let serializers = AtomicUsize::new(0);
    let second = client.begin_call("echo").and_then(|prepared| {
        serializers.fetch_add(1, Ordering::SeqCst);
        prepared.encode_vec(vec![1])
    });
    let error = match second {
        Err(error) => error,
        Ok(_) => panic!("slot exhaustion admitted"),
    };
    assert_semantic(error, ErrorCode::CallCapacityExceeded, "pre_dispatch");
    assert_eq!(serializers.load(Ordering::SeqCst), 0);
    drop(first);
    let materializations = Arc::new(AtomicUsize::new(0));
    let count = materializations.clone();
    let error = match client.begin_call("echo").unwrap().encode(9, move || {
        count.fetch_add(1, Ordering::SeqCst);
        Ok(vec![1; 9])
    }) {
        Err(error) => error,
        Ok(encoded) => match encoded.call_owned() {
            Err(error) => error,
            Ok(_) => panic!("byte excess admitted"),
        },
    };
    assert_semantic(error, ErrorCode::CallCapacityExceeded, "pre_dispatch");
    assert_eq!(materializations.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture
            .runtime
            .call_execution_snapshot()
            .unwrap()
            .used_operations,
        0
    );
}

#[test]
fn non_send_caller_serializer_keeps_original_deadline_and_never_materializes_expired_input() {
    let fixture = fixture(Carrier::Inline);
    let client = fixture.client.with_call_options(finite());
    let prepared = client.begin_call("echo").unwrap();
    assert_eq!(
        fixture
            .runtime
            .call_execution_snapshot()
            .unwrap()
            .used_operations,
        1
    );
    assert_eq!(
        fixture
            .runtime
            .call_execution_snapshot()
            .unwrap()
            .used_retained_bytes,
        0
    );
    // This Rc-backed codec is deliberately not Send and is invoked directly
    // on the caller after slot admission. Its synchronous work is not promised
    // to finish by D; only the subsequent native boundary enforces original D.
    let codec_state = std::rc::Rc::new(std::cell::Cell::new(0));
    let (_never_sent, wait) = mpsc::channel::<()>();
    let serialized = {
        codec_state.set(codec_state.get() + 1);
        assert!(
            wait.recv_timeout(DEADLINE + Duration::from_millis(100))
                .is_err()
        );
        b"unknown-pickle-length".to_vec()
    };
    let materializations = Arc::new(AtomicUsize::new(0));
    let count = materializations.clone();
    let error = match prepared.encode(serialized.len(), move || {
        count.fetch_add(1, Ordering::SeqCst);
        Ok(serialized)
    }) {
        Err(error) => error,
        Ok(_) => panic!("serialization reset original deadline"),
    };
    assert_semantic(error, ErrorCode::CallDeadlineExceeded, "pre_dispatch");
    assert_eq!(codec_state.get(), 1);
    assert_eq!(materializations.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture
            .runtime
            .call_execution_snapshot()
            .unwrap()
            .used_operations,
        0
    );
}

#[test]
fn runtime_clones_and_different_clients_share_outstanding_domain_without_reset() {
    let fixture = fixture(Carrier::Dedicated);
    fixture
        .runtime
        .set_call_execution_limits(CallExecutionLimitsOverrides {
            max_outstanding_calls: Some(1),
            retained_input_budget_bytes: Some(256 * 1024),
        })
        .unwrap();
    let runtime_clone = fixture.runtime.clone();
    let second = runtime_clone
        .connect(
            fixture.client.expected_route().clone(),
            Connect::DirectIpc {
                address: fixture.runtime.server_address().unwrap(),
            },
        )
        .unwrap()
        .with_call_options(finite());
    let caller = fixture.client.with_call_options(finite());
    let thread = std::thread::spawn(move || caller.call_owned("echo", &vec![0xa5; 128 * 1024]));
    fixture.entered.recv_timeout(EVENT_WAIT).unwrap();
    assert_semantic(
        thread.join().unwrap().unwrap_err(),
        ErrorCode::CallDeadlineExceeded,
        "dispatch_uncertain",
    );
    let error = match second.begin_call("echo") {
        Err(error) => error,
        Ok(_) => panic!("different client obtained a second domain"),
    };
    assert_semantic(error, ErrorCode::CallCapacityExceeded, "pre_dispatch");
    assert!(
        runtime_clone
            .set_call_execution_limits(CallExecutionLimitsOverrides {
                max_outstanding_calls: Some(2),
                retained_input_budget_bytes: Some(256 * 1024),
            })
            .is_err()
    );
    assert_eq!(
        runtime_clone
            .call_execution_snapshot()
            .unwrap()
            .used_operations,
        1
    );
    second.close();
    assert_eq!(
        fixture
            .runtime
            .call_execution_snapshot()
            .unwrap()
            .used_retained_bytes,
        128 * 1024
    );
    fixture.gate.open();
    observe_until(|| {
        runtime_clone
            .call_execution_snapshot()
            .unwrap()
            .used_operations
            == 0
    });
}

#[test]
fn closing_last_client_view_after_timeout_keeps_actual_transport_until_late_held_release() {
    let fixture = fixture(Carrier::Dedicated);
    // Separate Runtime gives this client its own real connection; the fixture
    // control client cannot keep this connection alive for the continuation.
    let observer = Runtime::new(RuntimeOptions {
        use_process_relay_anchor: false,
        ..Default::default()
    })
    .unwrap();
    let client = observer
        .connect(
            fixture.client.expected_route().clone(),
            Connect::DirectIpc {
                address: fixture.runtime.server_address().unwrap(),
            },
        )
        .unwrap();
    let caller = client.with_call_options(finite());
    let thread = std::thread::spawn(move || caller.call_held("echo", &vec![0xa5; 128 * 1024]));
    fixture.entered.recv_timeout(EVENT_WAIT).unwrap();
    let error = match thread.join().unwrap() {
        Err(error) => error,
        Ok(_) => panic!("gated response succeeded"),
    };
    assert_semantic(error, ErrorCode::CallDeadlineExceeded, "dispatch_uncertain");
    client.close();
    assert_eq!(
        observer.call_execution_snapshot().unwrap().used_operations,
        1
    );
    fixture.gate.open();
    observe_until(|| observer.call_execution_snapshot().unwrap().used_operations == 0);
    observe_until(|| fixture.host.memory_stats().server.unwrap().shm.used_bytes == 0);
    assert_eq!(
        observer
            .call_execution_snapshot()
            .unwrap()
            .used_retained_bytes,
        0
    );
}

#[test]
fn concurrent_options_views_and_successful_held_ignore_other_deadline() {
    let fixture = fixture(Carrier::Dedicated);
    let bytes = vec![0x5a; 128 * 1024];
    let mut held = fixture
        .client
        .with_call_options(finite())
        .call_held("echo", &bytes)
        .unwrap();
    let short = fixture.client.with_call_options(finite());
    let long = fixture
        .client
        .with_call_options(CallOptions::with_timeout(CallTimeout::After(
            Duration::from_secs(20),
        )));
    let caller = std::thread::spawn(move || short.call_owned("echo", &vec![0xa5; 128 * 1024]));
    let long_caller = std::thread::spawn(move || long.call_owned("echo", &vec![0xa5; 128 * 1024]));
    fixture.entered.recv_timeout(EVENT_WAIT).unwrap();
    fixture.entered.recv_timeout(EVENT_WAIT).unwrap();
    assert_semantic(
        caller.join().unwrap().unwrap_err(),
        ErrorCode::CallDeadlineExceeded,
        "dispatch_uncertain",
    );
    assert_eq!(held.bytes(), bytes);
    assert!(!held.is_released());
    // Held completed before short began, so the short expiry also establishes
    // that this held result's own original timer has passed harmlessly.
    fixture.gate.open();
    assert_eq!(long_caller.join().unwrap().unwrap(), vec![0xa5; 128 * 1024]);
    observe_until(|| {
        fixture
            .runtime
            .call_execution_snapshot()
            .unwrap()
            .used_operations
            == 0
    });
    assert!(fixture.host.memory_stats().server.unwrap().shm.used_bytes > 0);
    held.invalidate_then_release(|| Ok(())).unwrap();
    observe_until(|| fixture.host.memory_stats().server.unwrap().shm.used_bytes == 0);
}

#[test]
fn shutdown_closes_finite_domain_without_refunding_prepared_owner_or_blocking_unlimited() {
    let fixture = fixture(Carrier::Inline);
    let runtime = Runtime::new(RuntimeOptions {
        use_process_relay_anchor: false,
        ..Default::default()
    })
    .unwrap();
    runtime
        .set_call_execution_limits_with_sources(
            CallExecutionLimitsOverrides {
                max_outstanding_calls: Some(1),
                retained_input_budget_bytes: Some(8),
            },
            ConfigSources::empty(),
        )
        .unwrap();
    let expected = fixture.client.expected_route().clone();
    let address = fixture.runtime.server_address().unwrap();
    let client = runtime
        .connect(
            expected.clone(),
            Connect::DirectIpc {
                address: address.clone(),
            },
        )
        .unwrap()
        .with_call_options(finite());
    let mut prepared = client.begin_call("echo").unwrap();
    prepared.charge_input(8).unwrap();
    runtime.shutdown_without_host(EVENT_WAIT);
    let observed = runtime.clone().call_execution_snapshot().unwrap();
    assert!(observed.closed);
    assert_eq!(observed.used_operations, 1);
    assert_eq!(observed.used_retained_bytes, 8);
    assert!(
        runtime
            .set_call_execution_limits(CallExecutionLimitsOverrides {
                max_outstanding_calls: Some(10),
                retained_input_budget_bytes: Some(100),
            })
            .is_err()
    );
    drop(prepared);
    assert_eq!(
        runtime.call_execution_snapshot().unwrap().used_operations,
        0
    );
    assert_eq!(
        runtime
            .call_execution_snapshot()
            .unwrap()
            .used_retained_bytes,
        0
    );
    let fresh = runtime
        .connect(expected, Connect::DirectIpc { address })
        .unwrap();
    let error = match fresh.with_call_options(finite()).begin_call("echo") {
        Err(error) => error,
        Ok(_) => panic!("shutdown-created closed finite budget reopened"),
    };
    assert_semantic(error, ErrorCode::CallCapacityExceeded, "pre_dispatch");
    assert_eq!(
        fresh
            .with_call_options(unlimited())
            .call_owned("echo", b"after-shutdown")
            .unwrap(),
        b"after-shutdown"
    );
}

#[test]
fn snapshot_does_not_freeze_limits_and_clones_observe_same_configuration() {
    let runtime = Runtime::new(RuntimeOptions {
        use_process_relay_anchor: false,
        ..Default::default()
    })
    .unwrap();
    let clone = runtime.clone();
    let sources = ConfigSources::empty();
    let initial = runtime.call_execution_snapshot().unwrap();
    assert_eq!(initial.used_operations, 0);
    clone
        .set_call_execution_limits_with_sources(
            CallExecutionLimitsOverrides {
                max_outstanding_calls: Some(7),
                retained_input_budget_bytes: Some(11),
            },
            sources,
        )
        .unwrap();
    let observed = runtime.call_execution_snapshot().unwrap();
    assert_eq!(observed.max_operations, 7);
    assert_eq!(observed.max_retained_bytes, 11);
    assert_eq!(observed, clone.call_execution_snapshot().unwrap());
}

struct HttpServer {
    url: String,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn http_server(build: impl FnOnce(String) -> Router) -> HttpServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("HTTP fixture bind");
    listener.set_nonblocking(true).unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let app = build(url.clone());
    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
    let (ready, ready_rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            ready.send(()).unwrap();
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });
    });
    ready_rx.recv_timeout(EVENT_WAIT).unwrap();
    HttpServer {
        url,
        shutdown: Some(shutdown),
        thread: Some(thread),
    }
}

struct AsyncGateRelease(Arc<tokio::sync::Semaphore>);

impl Drop for AsyncGateRelease {
    fn drop(&mut self) {
        self.0.add_permits(64);
    }
}

#[derive(Clone)]
struct HttpCallState {
    calls: Arc<AtomicUsize>,
    stale: bool,
    uncertain_error: bool,
}

async fn http_echo(State(state): State<HttpCallState>, body: Bytes) -> (StatusCode, Vec<u8>) {
    state.calls.fetch_add(1, Ordering::SeqCst);
    if state.stale {
        return (
            StatusCode::CONFLICT,
            serde_json::to_vec(
                &C2Error::new(ErrorCode::RouteStale, "authoritative stale token")
                    .with_details(BTreeMap::from([(
                        "dispatch_phase".into(),
                        "pre_dispatch".into(),
                    )]))
                    .envelope(),
            )
            .unwrap(),
        );
    }
    if state.uncertain_error {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            C2Error::new(
                ErrorCode::RouteStale,
                "business error is not stale-route authority",
            )
            .to_wire_bytes(),
        );
    }
    (StatusCode::OK, body.to_vec())
}

fn http_data(stale: bool, uncertain_error: bool, calls: Arc<AtomicUsize>) -> HttpServer {
    http_server(|_| {
        Router::new()
            .route("/_probe/{route}", get(|| async { StatusCode::OK }))
            .route("/{route}/{method}", post(http_echo))
            .with_state(HttpCallState {
                calls,
                stale,
                uncertain_error,
            })
    })
}

#[derive(Clone)]
struct RefreshState {
    first: RelayRouteInfo,
    next: RelayRouteInfo,
    resolves: Arc<AtomicUsize>,
    refresh_entered: mpsc::Sender<()>,
    gate: Arc<tokio::sync::Semaphore>,
}

async fn http_resolve(State(state): State<RefreshState>) -> Json<Vec<RelayRouteInfo>> {
    let attempt = state.resolves.fetch_add(1, Ordering::SeqCst);
    if attempt == 0 {
        return Json(vec![state.first]);
    }
    state.refresh_entered.send(()).unwrap();
    tokio::time::timeout(EVENT_WAIT, state.gate.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    Json(vec![state.next])
}

fn http_route(
    expected: &c2_contract::ExpectedRouteContract,
    url: &str,
    revision: u64,
) -> RelayRouteInfo {
    RelayRouteInfo {
        name: expected.route_name.clone(),
        relay_url: url.into(),
        route_uid: format!("route-{revision}"),
        route_revision: revision,
        ipc_address: None,
        server_id: None,
        server_instance_id: None,
        crm_ns: expected.crm_ns.clone(),
        crm_name: expected.crm_name.clone(),
        crm_ver: expected.crm_ver.clone(),
        abi_hash: expected.abi_hash.clone(),
        signature_hash: expected.signature_hash.clone(),
        max_payload_size: 1024 * 1024,
    }
}

fn check_http_refresh(expire: bool, uncertain_error: bool, initial_resolve: bool) {
    let first_calls = Arc::new(AtomicUsize::new(0));
    let next_calls = Arc::new(AtomicUsize::new(0));
    let first = http_data(!uncertain_error, uncertain_error, first_calls.clone());
    let next = http_data(false, false, next_calls.clone());
    let expected = ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes())
        .unwrap()
        .expected_route(unique("http-deadline"))
        .unwrap();
    let resolves = Arc::new(AtomicUsize::new(0));
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let (entered, refresh_entered) = mpsc::channel();
    let registry = http_server(|_| {
        Router::new()
            .route("/_resolve/{route}", get(http_resolve))
            .with_state(RefreshState {
                first: http_route(&expected, &first.url, 1),
                next: http_route(&expected, &next.url, 2),
                resolves: resolves.clone(),
                refresh_entered: entered,
                gate: gate.clone(),
            })
    });
    let _release_on_unwind = AsyncGateRelease(gate.clone());
    let runtime = Runtime::new(RuntimeOptions {
        relay_anchor_address: Some(registry.url.clone()),
        use_process_relay_anchor: false,
        ..Default::default()
    })
    .unwrap();
    let client = runtime
        .connect(expected, Connect::RelayAware)
        .unwrap()
        .with_call_options(finite());
    // Initial connect/probe completed independently of this business scope.
    assert_eq!(
        runtime.call_execution_snapshot().unwrap().used_operations,
        0
    );
    if initial_resolve {
        runtime.clear_relay_projection_cache();
    }
    let caller = std::thread::spawn(move || client.call_owned("echo", b"original-input"));
    if uncertain_error {
        assert!(caller.join().unwrap().is_err());
        assert_eq!(resolves.load(Ordering::SeqCst), 1);
        assert_eq!(next_calls.load(Ordering::SeqCst), 0);
    } else {
        refresh_entered
            .recv_timeout(EVENT_WAIT)
            .expect("stale route must initiate authoritative refresh");
        if expire {
            assert_semantic(
                caller.join().unwrap().unwrap_err(),
                ErrorCode::CallDeadlineExceeded,
                if initial_resolve {
                    "pre_dispatch"
                } else {
                    "dispatch_uncertain"
                },
            );
            let still_owned = runtime.call_execution_snapshot().unwrap();
            assert_eq!(still_owned.used_operations, 1);
            assert_eq!(
                still_owned.used_retained_bytes,
                b"original-input".len() as u64
            );
            gate.add_permits(1);
            observe_until(|| runtime.call_execution_snapshot().unwrap().used_operations == 0);
            assert_eq!(
                next_calls.load(Ordering::SeqCst),
                0,
                "expired original scope must not dispatch on refreshed route"
            );
        } else {
            gate.add_permits(1);
            assert_eq!(caller.join().unwrap().unwrap(), b"original-input");
            assert_eq!(next_calls.load(Ordering::SeqCst), 1);
        }
    }
    assert_eq!(
        first_calls.load(Ordering::SeqCst),
        if initial_resolve { 0 } else { 1 },
        "business POST must never replay to stale/uncertain endpoint"
    );
}

#[test]
fn http_original_deadline_survives_authoritative_stale_route_refresh() {
    check_http_refresh(true, false, false);
}
#[test]
fn http_authoritative_pre_dispatch_stale_can_retry_once_in_same_scope() {
    check_http_refresh(false, false, false);
}
#[test]
fn http_business_error_has_no_authority_to_repeat_unknown_post() {
    check_http_refresh(false, true, false);
}
#[test]
fn http_initial_call_route_wait_expires_pre_dispatch_without_business_post() {
    check_http_refresh(true, false, true);
}

#[derive(Clone)]
struct HeldHttpState {
    gate: Arc<tokio::sync::Semaphore>,
    entered: mpsc::Sender<()>,
}

async fn http_gated_body(State(state): State<HeldHttpState>, body: Bytes) -> (StatusCode, Vec<u8>) {
    state.entered.send(()).unwrap();
    tokio::time::timeout(EVENT_WAIT, state.gate.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    (StatusCode::OK, body.to_vec())
}

#[test]
fn http_explicit_unlimited_and_600_seconds_bypass_legacy_total_request_timer() {
    const CHILD: &str = "C2_CORE_CALL_DEADLINE_POLICY_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // Environment policy is exercised in one isolated test process; no
        // concurrent test in this binary observes the artificially tiny timer.
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "http_explicit_unlimited_and_600_seconds_bypass_legacy_total_request_timer",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("C2_ENV_FILE", "")
            .env("C2_RELAY_CALL_TIMEOUT", "0.25")
            .env_remove("C2_RELAY_ANCHOR_ADDRESS")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "isolated HTTP policy proof failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let expected = ContractRelease::from_descriptor_json(DESCRIPTOR.as_bytes())
        .unwrap()
        .expected_route(unique("http-held-policy"))
        .unwrap();
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let (entered_tx, entered) = mpsc::channel();
    let data = http_server(|url| {
        let route = http_route(&expected, &url, 1);
        Router::new()
            .route(
                "/_resolve/{route}",
                get(move || {
                    let route = route.clone();
                    async move { Json(vec![route]) }
                }),
            )
            .route("/_probe/{route}", get(|| async { StatusCode::OK }))
            .route("/{route}/{method}", post(http_gated_body))
            .with_state(HeldHttpState {
                gate: gate.clone(),
                entered: entered_tx,
            })
    });
    let _release_on_unwind = AsyncGateRelease(gate.clone());
    let runtime = Runtime::new(RuntimeOptions {
        use_process_relay_anchor: false,
        ..Default::default()
    })
    .unwrap();
    let client = runtime
        .connect(
            expected,
            Connect::ExplicitRelay {
                relay_url: data.url.clone(),
            },
        )
        .unwrap();
    let six_hundred = client.with_call_options(CallOptions::with_timeout(CallTimeout::After(
        Duration::from_secs(600),
    )));
    let forever = client.with_call_options(unlimited());
    let long = std::thread::spawn(move || six_hundred.call_held("echo", b"six-hundred"));
    let unlimited = std::thread::spawn(move || forever.call_held("echo", b"unlimited"));
    entered.recv_timeout(EVENT_WAIT).unwrap();
    entered.recv_timeout(EVENT_WAIT).unwrap();
    // Starting this scope after both POST entry events proves the other
    // responses remain pending far beyond the old 250ms reqwest total timer.
    let short = client.with_call_options(finite());
    let timer = std::thread::spawn(move || short.call_owned("echo", b"finite-timer"));
    entered.recv_timeout(EVENT_WAIT).unwrap();
    assert_semantic(
        timer.join().unwrap().unwrap_err(),
        ErrorCode::CallDeadlineExceeded,
        "dispatch_uncertain",
    );
    assert_eq!(
        runtime.call_execution_snapshot().unwrap().used_operations,
        2,
        "Unlimited must not consume a continuation slot; finite and 600s still own theirs"
    );
    gate.add_permits(3);
    let mut long = long.join().unwrap().unwrap();
    let mut unlimited = unlimited.join().unwrap().unwrap();
    assert_eq!(long.bytes(), b"six-hundred");
    assert_eq!(unlimited.bytes(), b"unlimited");
    long.invalidate_then_release(|| Ok(())).unwrap();
    unlimited.invalidate_then_release(|| Ok(())).unwrap();
    observe_until(|| runtime.call_execution_snapshot().unwrap().used_operations == 0);
    assert_eq!(
        runtime
            .call_execution_snapshot()
            .unwrap()
            .used_retained_bytes,
        0
    );
}
