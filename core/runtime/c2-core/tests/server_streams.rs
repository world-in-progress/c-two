//! End-to-end server-stream behavior through the public Core Host and direct IPC client.
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use c2_config::{ConfigSources, LocalEndpointOptions, ServerIpcConfigOverrides};
use c2_contract::{ContractRelease, MethodAccess};
use c2_core::{
    CallExecutionLimitsOverrides, CallOptions, CallTimeout, Client, Connect, CoreResponseStream,
    EncodedClient, EncodedService, EncodedStream, EncodedStreamItem, EncodedStreamService, Error,
    Host, HostOptions, LocalObjectStream, MethodDefinition, Registration, Runtime, RuntimeOptions,
    RuntimeShutdownTarget, ServiceConcurrencyMode, ServiceDefinition,
};
use c2_error::{C2Error, ErrorCode};
use c2_http::relay::{RelayConfig, RelayServer};

const WAIT: Duration = Duration::from_secs(10);
const DESCRIPTOR: &[u8] =
    include_bytes!("../../../foundation/c2-contract/tests/fixtures/server-stream.contract.json");

fn unique(prefix: &str) -> String {
    format!("{prefix}-{}", uuid::Uuid::new_v4().simple())
}

struct LocalObjects {
    values: Vec<Arc<Vec<u8>>>,
    cursor: usize,
    steps: mpsc::Sender<usize>,
    finished: mpsc::Sender<()>,
    step_gate: Option<Arc<Gate>>,
    finish_gate: Option<Arc<Gate>>,
}

impl LocalObjectStream<Arc<Vec<u8>>> for LocalObjects {
    fn step(&mut self) -> Result<Option<Arc<Vec<u8>>>, C2Error> {
        self.steps.send(self.cursor).unwrap();
        if let Some(gate) = self.step_gate.take() {
            gate.wait();
        }
        let item = self.values.get(self.cursor).cloned();
        self.cursor += 1;
        Ok(item)
    }

    fn finish(self: Box<Self>) -> Result<(), C2Error> {
        self.finished.send(()).unwrap();
        if let Some(gate) = self.finish_gate.as_ref() {
            gate.wait();
        }
        Ok(())
    }
}

#[test]
fn local_object_stream_preserves_identity_and_releases_per_step_permit() {
    let fixture = Fixture::new(false, 1, ServiceConcurrencyMode::ReadParallel, None, None);
    let route = fixture.registration.as_ref().unwrap().route_concurrency();
    let values = vec![Arc::new(vec![1]), Arc::new(vec![2]), Arc::new(vec![3])];
    let (steps_tx, steps_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let factory_values = values.clone();
    let stream = route
        .open_local_stream(1, move || {
            Ok(Box::new(LocalObjects {
                values: factory_values,
                cursor: 0,
                steps: steps_tx,
                finished: finished_tx,
                step_gate: None,
                finish_gate: None,
            }))
        })
        .unwrap();

    assert_eq!(steps_rx.recv_timeout(WAIT).unwrap(), 0);
    assert_eq!(steps_rx.recv_timeout(WAIT).unwrap(), 1);
    observe_until(|| route.snapshot().active_workers == 0);
    assert!(
        steps_rx.try_recv().is_err(),
        "one queued item grants only one further step"
    );
    assert!(Arc::ptr_eq(
        &stream.next_item().unwrap().unwrap(),
        &values[0]
    ));
    assert_eq!(steps_rx.recv_timeout(WAIT).unwrap(), 2);
    let snapshot = route.snapshot();
    assert_eq!(snapshot.pending, 1, "original call slot remains");
    assert_eq!(snapshot.active_workers, 0, "yield releases route permit");
    let mut unary = route.blocking_acquire(0).unwrap();
    unary.commit().unwrap();
    drop(unary);

    assert!(Arc::ptr_eq(
        &stream.next_item().unwrap().unwrap(),
        &values[1]
    ));
    assert!(Arc::ptr_eq(
        &stream.next_item().unwrap().unwrap(),
        &values[2]
    ));
    assert!(stream.next_item().unwrap().is_none());
    assert_eq!(steps_rx.recv_timeout(WAIT).unwrap(), 3);
    finished_rx.recv_timeout(WAIT).unwrap();
    let receipt = stream.close();
    assert!(receipt.completed);
    assert!(receipt.terminal.unwrap().is_ok());
    assert_eq!(receipt.complete_items, Some(3));
    assert_eq!(route.snapshot().pending, 0);
}

#[test]
fn local_callback_panics_keep_a_truthful_terminal_and_run_cleanup() {
    struct Panics {
        phase: u8,
        emitted: bool,
        finished: Arc<AtomicUsize>,
    }
    impl LocalObjectStream<u8> for Panics {
        fn step(&mut self) -> Result<Option<u8>, C2Error> {
            assert_ne!(self.phase, 1, "step panic");
            Ok((!std::mem::replace(&mut self.emitted, true)).then_some(7))
        }
        fn finish(self: Box<Self>) -> Result<(), C2Error> {
            self.finished.fetch_add(1, Ordering::SeqCst);
            assert_ne!(self.phase, 2, "finish panic");
            Ok(())
        }
    }
    for phase in 0..3 {
        let fixture = Fixture::new(false, 1, ServiceConcurrencyMode::ReadParallel, None, None);
        let route = fixture.registration.as_ref().unwrap().route_concurrency();
        let finished = Arc::new(AtomicUsize::new(0));
        let finish_count = finished.clone();
        let stream = route
            .open_local_stream(1, move || {
                assert_ne!(phase, 0, "factory panic");
                Ok(Box::new(Panics {
                    phase,
                    emitted: false,
                    finished: finish_count,
                }))
            })
            .unwrap();
        if phase == 2 {
            assert_eq!(stream.next_item().unwrap(), Some(7));
        }
        assert_semantic(
            stream.next_item().expect_err("resource panic"),
            ErrorCode::ResourceFunctionExecuting,
            "panicked",
        );
        let receipt = stream.close();
        assert!(receipt.completed);
        assert_eq!(receipt.complete_items, Some(u64::from(phase == 2)));
        assert_semantic(
            receipt
                .terminal
                .unwrap()
                .expect_err("original error retained"),
            ErrorCode::ResourceFunctionExecuting,
            "panicked",
        );
        assert_eq!(finished.load(Ordering::SeqCst), usize::from(phase != 0));
        assert_eq!(route.snapshot().pending, 0);
    }
}

#[test]
fn local_close_is_nonwaiting_and_receipt_tracks_blocked_cleanup() {
    let fixture = Fixture::new(false, 1, ServiceConcurrencyMode::ReadParallel, None, None);
    let route = fixture.registration.as_ref().unwrap().route_concurrency();
    let (steps_tx, steps_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let finish_gate = Arc::new(Gate::default());
    let gate = finish_gate.clone();
    let stream = route
        .open_local_stream(1, move || {
            Ok(Box::new(LocalObjects {
                values: vec![Arc::new(vec![1]), Arc::new(vec![2]), Arc::new(vec![3])],
                cursor: 0,
                steps: steps_tx,
                finished: finished_tx,
                step_gate: None,
                finish_gate: Some(gate),
            }))
        })
        .unwrap();
    assert_eq!(steps_rx.recv_timeout(WAIT).unwrap(), 0);
    assert!(stream.next_item().unwrap().is_some());
    let pending = stream.close_with_timeout(Some(Duration::ZERO));
    assert!(!pending.completed);
    finished_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(route.snapshot().pending, 1);
    finish_gate.release();
    let receipt = stream.close_with_timeout(None);
    assert!(receipt.completed);
    assert!(matches!(receipt.complete_items, Some(1 | 2)));
    assert_semantic(
        receipt
            .terminal
            .unwrap()
            .expect_err("consumer cancellation"),
        ErrorCode::CallCancelled,
        "consumer",
    );
}

#[test]
fn local_cleanup_error_replaces_cancelled_terminal() {
    struct BadFinish(bool);
    impl LocalObjectStream<u8> for BadFinish {
        fn step(&mut self) -> Result<Option<u8>, C2Error> {
            if std::mem::replace(&mut self.0, false) {
                Ok(Some(4))
            } else {
                Ok(None)
            }
        }

        fn finish(self: Box<Self>) -> Result<(), C2Error> {
            Err(C2Error::new(
                ErrorCode::ResourceFunctionExecuting,
                "local generator cleanup failed",
            ))
        }
    }

    let fixture = Fixture::new(false, 1, ServiceConcurrencyMode::ReadParallel, None, None);
    let route = fixture.registration.as_ref().unwrap().route_concurrency();
    let stream = route
        .open_local_stream(1, || Ok(Box::new(BadFinish(true))))
        .unwrap();
    assert_eq!(stream.next_item().unwrap(), Some(4));
    let receipt = stream.close();
    assert_eq!(receipt.complete_items, Some(1));
    assert_semantic(
        receipt.terminal.unwrap().expect_err("cleanup error wins"),
        ErrorCode::ResourceFunctionExecuting,
        "cleanup failed",
    );
}

#[test]
fn local_unregistration_delivers_running_item_before_cancelled_terminal() {
    let mut fixture = Fixture::new(false, 1, ServiceConcurrencyMode::ReadParallel, None, None);
    let route = fixture.registration.as_ref().unwrap().route_concurrency();
    let (steps_tx, steps_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let step_gate = Arc::new(Gate::default());
    let gate = step_gate.clone();
    let value = Arc::new(vec![7]);
    let expected = value.clone();
    let stream = route
        .open_local_stream(1, move || {
            Ok(Box::new(LocalObjects {
                values: vec![value],
                cursor: 0,
                steps: steps_tx,
                finished: finished_tx,
                step_gate: Some(gate),
                finish_gate: None,
            }))
        })
        .unwrap();
    assert_eq!(steps_rx.recv_timeout(WAIT).unwrap(), 0);
    let mut registration = fixture.registration.take().unwrap();
    let close = std::thread::spawn(move || registration.close().unwrap());
    observe_until(|| route.snapshot().closed);
    step_gate.release();

    assert!(Arc::ptr_eq(
        &stream.next_item().unwrap().unwrap(),
        &expected
    ));
    assert_semantic(
        stream.next_item().expect_err("unregistration ends stream"),
        ErrorCode::CallCancelled,
        "unregister",
    );
    finished_rx.recv_timeout(WAIT).unwrap();
    assert!(close.join().unwrap().local_removed);
    let receipt = stream.close();
    assert_eq!(receipt.complete_items, Some(1));
    assert_semantic(
        receipt
            .terminal
            .unwrap()
            .expect_err("unregistration terminal"),
        ErrorCode::CallCancelled,
        "unregister",
    );
}

#[test]
fn local_shutdown_delivers_running_item_then_drains_original_call_slot() {
    let fixture = Fixture::new(false, 1, ServiceConcurrencyMode::ReadParallel, None, None);
    let route = fixture.registration.as_ref().unwrap().route_concurrency();
    let (steps_tx, steps_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    let step_gate = Arc::new(Gate::default());
    let gate = step_gate.clone();
    let stream = route
        .open_local_stream(1, move || {
            Ok(Box::new(LocalObjects {
                values: vec![Arc::new(vec![9])],
                cursor: 0,
                steps: steps_tx,
                finished: finished_tx,
                step_gate: Some(gate),
                finish_gate: None,
            }))
        })
        .unwrap();
    assert_eq!(steps_rx.recv_timeout(WAIT).unwrap(), 0);
    let receipt = fixture
        .server
        .begin_session_retirement(
            RuntimeShutdownTarget::Host(&fixture.host),
            Instant::now() + WAIT,
        )
        .unwrap();
    observe_until(|| route.snapshot().closed);
    step_gate.release();
    assert_eq!(stream.next_item().unwrap().unwrap().as_slice(), &[9]);
    assert_semantic(
        stream.next_item().expect_err("shutdown ends stream"),
        ErrorCode::CallCancelled,
        "shutdown",
    );
    finished_rx.recv_timeout(WAIT).unwrap();
    let (outcome, completed) = receipt.observe_until(Instant::now() + WAIT);
    assert!(completed, "{outcome:?}");
    assert!(outcome.runtime_barrier_error.is_none(), "{outcome:?}");
    assert_eq!(route.snapshot().pending, 0);
}

fn observe_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(
            Instant::now() < deadline,
            "expected native state was not reached"
        );
        std::thread::yield_now();
    }
}

#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    fn release(&self) {
        *self.open.lock().unwrap() = true;
        self.changed.notify_all();
    }

    fn wait(&self) {
        let (open, elapsed) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), WAIT, |open| !*open)
            .unwrap();
        assert!(
            *open && !elapsed.timed_out(),
            "test controller did not release callback"
        );
    }
}

struct Probe {
    step_gate: Gate,
    finish_gate: Gate,
    encode_gate: Gate,
    unary_gate: Gate,
    step_entered: mpsc::Sender<(u8, usize)>,
    item_encoded: mpsc::Sender<(u8, usize)>,
    finish_entered: mpsc::Sender<u8>,
    unary_entered: mpsc::Sender<()>,
    opens: AtomicUsize,
    steps: AtomicUsize,
    finishes: AtomicUsize,
}

struct ProbeService(Arc<Probe>);

impl EncodedService for ProbeService {
    fn invoke(&self, index: u16, request: Vec<u8>) -> Result<Vec<u8>, C2Error> {
        assert_eq!(index, 0, "only ping is unary");
        if request == b"gated" {
            self.0.unary_entered.send(()).unwrap();
            self.0.unary_gate.wait();
        }
        Ok(b"pong".to_vec())
    }
}

impl EncodedStreamService for ProbeService {
    fn open_stream(&self, index: u16, request: Vec<u8>) -> Result<Box<dyn EncodedStream>, C2Error> {
        assert_eq!(index, 1, "only echo is a server stream");
        assert_eq!(request.len(), 2);
        self.0.opens.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(ProbeStream {
            probe: self.0.clone(),
            count: if request[1] == 6 {
                usize::MAX
            } else {
                request[0] as usize
            },
            mode: request[1],
            cursor: 0,
        }))
    }
}

struct ProbeStream {
    probe: Arc<Probe>,
    count: usize,
    mode: u8,
    cursor: usize,
}

impl EncodedStream for ProbeStream {
    fn step(&mut self) -> Result<Option<Box<dyn EncodedStreamItem>>, C2Error> {
        self.probe.steps.fetch_add(1, Ordering::SeqCst);
        self.probe
            .step_entered
            .send((self.mode, self.cursor))
            .unwrap();
        if (self.mode == 3 && self.cursor == 0) || (self.mode == 6 && self.cursor == 2) {
            self.probe.step_gate.wait();
        }
        if self.cursor == self.count {
            return if self.mode == 1 {
                Err(C2Error::new(
                    ErrorCode::ResourceFunctionExecuting,
                    "step failed after complete items",
                ))
            } else {
                Ok(None)
            };
        }
        self.cursor += 1;
        Ok(Some(Box::new(ProbeItem {
            probe: self.probe.clone(),
            mode: self.mode,
            index: self.cursor,
        })))
    }

    fn finish(self: Box<Self>) -> Result<(), C2Error> {
        self.probe.finishes.fetch_add(1, Ordering::SeqCst);
        self.probe.finish_entered.send(self.mode).unwrap();
        if self.mode == 4 {
            self.probe.finish_gate.wait();
        }
        if self.mode == 2 {
            Err(C2Error::new(
                ErrorCode::ResourceFunctionExecuting,
                "finish failed after complete items",
            ))
        } else {
            Ok(())
        }
    }
}

struct ProbeItem {
    probe: Arc<Probe>,
    mode: u8,
    index: usize,
}

impl EncodedStreamItem for ProbeItem {
    fn len(&self) -> usize {
        4
    }

    fn write_range(&self, offset: usize, destination: &mut [u8]) -> Result<(), C2Error> {
        assert!(offset + destination.len() <= self.len());
        self.probe
            .item_encoded
            .send((self.mode, self.index))
            .unwrap();
        if self.mode == 5 && self.index == 1 {
            self.probe.encode_gate.wait();
        }
        destination.fill(self.index as u8);
        Ok(())
    }
}

struct Fixture {
    server: Runtime,
    host: Host,
    registration: Option<Registration>,
    _client_runtime: Runtime,
    client: Client,
    probe: Arc<Probe>,
    step_events: mpsc::Receiver<(u8, usize)>,
    encoded_events: mpsc::Receiver<(u8, usize)>,
    finish_events: mpsc::Receiver<u8>,
    unary_events: mpsc::Receiver<()>,
    _endpoint_root: tempfile::TempDir,
    _relay: Option<RelayServer>,
}

impl Fixture {
    fn new(
        separate_client: bool,
        workers: u32,
        mode: ServiceConcurrencyMode,
        max_calls: Option<u64>,
        budget_bytes: Option<u64>,
    ) -> Self {
        Self::with_relay(
            separate_client,
            workers,
            mode,
            max_calls,
            budget_bytes,
            false,
        )
    }

    fn with_relay(
        separate_client: bool,
        workers: u32,
        mode: ServiceConcurrencyMode,
        max_calls: Option<u64>,
        budget_bytes: Option<u64>,
        use_relay: bool,
    ) -> Self {
        let root = tempfile::Builder::new()
            .prefix("c2-core-stream-")
            .tempdir()
            .unwrap();
        let server = Runtime::new(RuntimeOptions {
            server_id: Some(unique("stream-server")),
            use_process_relay_anchor: false,
            shm_threshold: Some(1024),
            server_ipc_overrides: Some(ServerIpcConfigOverrides {
                max_pending_requests: Some(32),
                max_execution_workers: Some(workers),
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap();
        server
            .set_local_endpoint_with_sources(
                LocalEndpointOptions {
                    #[cfg(unix)]
                    unix_root: Some(root.path().to_path_buf()),
                    ..Default::default()
                },
                ConfigSources::empty(),
            )
            .unwrap();
        let relay = if use_relay {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = probe.local_addr().unwrap();
            drop(probe);
            let url = format!("http://{address}");
            let relay = RelayServer::start_with_context(
                RelayConfig {
                    bind: address.to_string(),
                    advertise_url: url.clone(),
                    idle_timeout_secs: 0,
                    anti_entropy_interval: Duration::ZERO,
                    heartbeat_interval: Duration::ZERO,
                    ..Default::default()
                },
                server.local_endpoint_context().unwrap(),
            )
            .unwrap();
            server.set_relay_anchor_address(Some(url));
            Some(relay)
        } else {
            None
        };
        if max_calls.is_some() || budget_bytes.is_some() {
            server
                .set_call_execution_limits_with_sources(
                    CallExecutionLimitsOverrides {
                        max_outstanding_calls: max_calls,
                        payload_buffer_budget_bytes: budget_bytes,
                    },
                    ConfigSources::empty(),
                )
                .unwrap();
        }
        let client_runtime = if separate_client {
            let peer = Runtime::new(RuntimeOptions {
                server_id: Some(unique("stream-peer")),
                use_process_relay_anchor: false,
                ..Default::default()
            })
            .unwrap();
            peer.inherit_local_endpoint_selection(&server).unwrap();
            peer
        } else {
            server.clone()
        };
        let (step_tx, step_events) = mpsc::channel();
        let (encoded_tx, encoded_events) = mpsc::channel();
        let (finish_tx, finish_events) = mpsc::channel();
        let (unary_tx, unary_events) = mpsc::channel();
        let probe = Arc::new(Probe {
            step_gate: Gate::default(),
            finish_gate: Gate::default(),
            encode_gate: Gate::default(),
            unary_gate: Gate::default(),
            step_entered: step_tx,
            item_encoded: encoded_tx,
            finish_entered: finish_tx,
            unary_entered: unary_tx,
            opens: AtomicUsize::new(0),
            steps: AtomicUsize::new(0),
            finishes: AtomicUsize::new(0),
        });
        let release = ContractRelease::from_descriptor_json(DESCRIPTOR).unwrap();
        let route_name = unique("stream-route");
        let host = server
            .host(if use_relay {
                HostOptions::default()
            } else {
                HostOptions::default().without_relay()
            })
            .unwrap();
        let registration = host
            .register(
                ServiceDefinition::new_stream(
                    &release,
                    release.reference(),
                    &route_name,
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
                    Arc::new(ProbeService(probe.clone())),
                )
                .unwrap()
                .with_concurrency(mode, Some(32), Some(workers as usize))
                .unwrap(),
            )
            .unwrap();
        let client = client_runtime
            .connect(
                release.expected_route(&route_name).unwrap(),
                Connect::DirectIpc {
                    address: host.local_endpoint().address().into(),
                },
            )
            .unwrap();
        Self {
            server,
            host,
            registration: Some(registration),
            _client_runtime: client_runtime,
            client,
            probe,
            step_events,
            encoded_events,
            finish_events,
            unary_events,
            _endpoint_root: root,
            _relay: relay,
        }
    }

    fn stream(&self, count: u8, mode: u8) -> CoreResponseStream {
        self.client
            .begin_stream("echo")
            .unwrap()
            .encode_vec(vec![count, mode])
            .unwrap()
            .stream()
            .unwrap()
    }
}

fn assert_item(stream: &mut CoreResponseStream, index: u8) -> c2_core::HeldResponse {
    let item = stream.next_item().unwrap().expect("complete item");
    assert_eq!(item.bytes(), &[index; 4]);
    item
}

fn assert_semantic(error: Error, code: ErrorCode, reason: &str) {
    let Error::Semantic(error) = error else {
        panic!("expected semantic {code:?}, got {error:?}");
    };
    assert_eq!(error.code, code);
    assert!(error.message.contains(reason), "{error:?}");
}

fn expect_stream_error(stream: &mut CoreResponseStream) -> Error {
    match stream.next_item() {
        Ok(_) => panic!("expected stream terminal error"),
        Err(error) => error,
    }
}

#[test]
fn zero_one_and_multiple_items_end_naturally_after_cleanup() {
    let fixture = Fixture::new(false, 2, ServiceConcurrencyMode::ReadParallel, None, None);
    for count in [0, 1, 3] {
        let mut stream = fixture.stream(count, 0);
        for index in 1..=count {
            drop(assert_item(&mut stream, index));
        }
        assert!(stream.next_item().unwrap().is_none());
        let receipt = stream.close_with_timeout(Some(WAIT));
        assert!(receipt.completed);
        assert!(receipt.terminal.unwrap().is_ok());
        assert_eq!(receipt.complete_items, Some(count as u64));
    }
    assert_eq!(fixture.probe.finishes.load(Ordering::SeqCst), 3);
}

#[test]
fn resource_step_and_cleanup_errors_follow_prior_complete_items() {
    let fixture = Fixture::new(false, 2, ServiceConcurrencyMode::ReadParallel, None, None);
    for (mode, reason) in [
        (1, "step failed after complete items"),
        (2, "finish failed after complete items"),
    ] {
        let mut stream = fixture.stream(1, mode);
        drop(assert_item(&mut stream, 1));
        assert_semantic(
            expect_stream_error(&mut stream),
            ErrorCode::ResourceFunctionExecuting,
            reason,
        );
        let receipt = stream.close_with_timeout(Some(WAIT));
        assert!(receipt.completed);
        assert_eq!(receipt.complete_items, Some(1));
        assert_semantic(
            receipt.terminal.unwrap().expect_err("original failure"),
            ErrorCode::ResourceFunctionExecuting,
            reason,
        );
    }
    assert_eq!(fixture.probe.finishes.load(Ordering::SeqCst), 2);
}

#[test]
fn cleanup_error_preserves_its_cause_when_the_consumer_cancels() {
    let fixture = Fixture::new(false, 2, ServiceConcurrencyMode::ReadParallel, None, None);
    let mut stream = fixture.stream(100, 2);
    drop(assert_item(&mut stream, 1));
    let receipt = stream.close_with_timeout(Some(WAIT));
    assert!(
        receipt.completed,
        "steps={}, finishes={}, route={:?}",
        fixture.probe.steps.load(Ordering::SeqCst),
        fixture.probe.finishes.load(Ordering::SeqCst),
        fixture
            .registration
            .as_ref()
            .unwrap()
            .route_concurrency()
            .snapshot()
    );
    assert!(receipt.complete_items.unwrap() >= 1);
    assert_semantic(
        receipt.terminal.unwrap().expect_err("cleanup failure"),
        ErrorCode::ResourceFunctionExecuting,
        "finish failed after complete items",
    );
    assert_eq!(fixture.probe.finishes.load(Ordering::SeqCst), 1);
}

#[test]
fn close_receipt_stays_pending_until_the_original_cleanup_finishes() {
    let fixture = Fixture::new(false, 2, ServiceConcurrencyMode::ReadParallel, None, None);
    let mut stream = fixture.stream(3, 4);
    drop(assert_item(&mut stream, 1));
    let pending = stream.close_with_timeout(Some(Duration::ZERO));
    assert!(!pending.completed);
    assert!(pending.terminal.is_none());
    assert_eq!(fixture.finish_events.recv_timeout(WAIT).unwrap(), 4);
    let still_pending = stream.close_with_timeout(Some(Duration::ZERO));
    assert!(!still_pending.completed);
    fixture.probe.finish_gate.release();
    let terminal = stream.close_with_timeout(Some(WAIT));
    assert!(terminal.completed);
    assert_semantic(
        terminal
            .terminal
            .unwrap()
            .expect_err("cancelled after cleanup"),
        ErrorCode::CallCancelled,
        "consumer",
    );
}

#[test]
fn default_and_unbounded_close_wait_for_the_same_cleanup_barrier() {
    for use_default in [true, false] {
        let fixture = Fixture::new(false, 2, ServiceConcurrencyMode::ReadParallel, None, None);
        let mut stream = fixture.stream(3, 4);
        drop(assert_item(&mut stream, 1));
        let (returned, observed) = mpsc::channel();
        let close = std::thread::spawn(move || {
            let receipt = if use_default {
                stream.close()
            } else {
                stream.close_with_timeout(None)
            };
            returned.send(receipt).unwrap();
        });
        assert_eq!(fixture.finish_events.recv_timeout(WAIT).unwrap(), 4);
        assert!(
            observed.try_recv().is_err(),
            "close returned before cleanup"
        );
        fixture.probe.finish_gate.release();
        let receipt = observed.recv_timeout(WAIT).unwrap();
        assert!(receipt.completed);
        assert_semantic(
            receipt
                .terminal
                .unwrap()
                .expect_err("cancelled after cleanup"),
            ErrorCode::CallCancelled,
            "consumer",
        );
        close.join().unwrap();
    }
}

#[test]
fn application_question_mark_drops_a_stream_without_waiting_and_refunds_its_slot() {
    fn application(fixture: &Fixture) -> Result<(), Error> {
        let mut stream = fixture.stream(3, 4);
        drop(assert_item(&mut stream, 1));
        Err::<(), Error>(
            C2Error::new(ErrorCode::ResourceUnavailable, "application failed").into(),
        )?;
        Ok(())
    }

    let fixture = Fixture::new(
        false,
        2,
        ServiceConcurrencyMode::ReadParallel,
        Some(1),
        None,
    );
    assert_semantic(
        application(&fixture).expect_err("application failure"),
        ErrorCode::ResourceUnavailable,
        "application failed",
    );
    assert_eq!(fixture.finish_events.recv_timeout(WAIT).unwrap(), 4);
    assert_eq!(
        fixture
            .server
            .call_execution_snapshot()
            .unwrap()
            .used_operations,
        1
    );
    fixture.probe.finish_gate.release();
    observe_until(|| {
        fixture
            .server
            .call_execution_snapshot()
            .unwrap()
            .used_operations
            == 0
    });
    let mut recovered = fixture.stream(1, 0);
    drop(assert_item(&mut recovered, 1));
    assert!(recovered.next_item().unwrap().is_none());
}

#[test]
fn runtime_shutdown_preserves_the_running_step_and_entered_unary_call() {
    let fixture = Fixture::new(true, 2, ServiceConcurrencyMode::Parallel, None, None);
    let mut stream = fixture.stream(2, 3);
    assert_eq!(fixture.step_events.recv_timeout(WAIT).unwrap(), (3, 0));
    let unary_client = fixture.client.clone();
    let unary = std::thread::spawn(move || unary_client.call_owned("ping", b"gated"));
    fixture.unary_events.recv_timeout(WAIT).unwrap();
    let route = fixture.registration.as_ref().unwrap().route_concurrency();
    let receipt = fixture
        .server
        .begin_session_retirement(
            RuntimeShutdownTarget::Host(&fixture.host),
            Instant::now() + WAIT,
        )
        .unwrap();
    observe_until(|| route.snapshot().closed);
    assert!(!receipt.observe_until(Instant::now()).1);
    fixture.probe.step_gate.release();
    fixture.probe.unary_gate.release();
    assert_eq!(unary.join().unwrap().unwrap(), b"pong");
    drop(assert_item(&mut stream, 1));
    assert_semantic(
        expect_stream_error(&mut stream),
        ErrorCode::CallCancelled,
        "shutdown",
    );
    assert_eq!(fixture.probe.steps.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.probe.finishes.load(Ordering::SeqCst), 1);
    let (outcome, completed) = receipt.observe_until(Instant::now() + WAIT);
    assert!(completed, "{outcome:?}");
    assert!(outcome.runtime_barrier_error.is_none(), "{outcome:?}");
    assert!(fixture.host.lifecycle_snapshot().finished);
}

#[test]
fn unregister_preserves_the_running_step_then_reports_its_cause() {
    let mut fixture = Fixture::new(true, 2, ServiceConcurrencyMode::ReadParallel, None, None);
    let mut stream = fixture.stream(2, 3);
    assert_eq!(fixture.step_events.recv_timeout(WAIT).unwrap(), (3, 0));
    let mut registration = fixture.registration.take().unwrap();
    let route = registration.route_concurrency();
    let closing = std::thread::spawn(move || registration.close());
    observe_until(|| route.snapshot().closed);
    fixture.probe.step_gate.release();
    drop(assert_item(&mut stream, 1));
    assert_semantic(
        expect_stream_error(&mut stream),
        ErrorCode::CallCancelled,
        "unregister",
    );
    assert_eq!(fixture.probe.steps.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.probe.finishes.load(Ordering::SeqCst), 1);
    let outcome = closing.join().unwrap().unwrap();
    assert!(outcome.close.active_drained, "{outcome:?}");
}

#[test]
fn same_runtime_inline_item_larger_than_half_budget_does_not_wait_on_itself() {
    let fixture = Fixture::new(false, 2, ServiceConcurrencyMode::Parallel, None, Some(7));
    let mut stream = fixture
        .client
        .with_call_options(CallOptions::with_timeout(CallTimeout::After(WAIT)))
        .begin_stream("echo")
        .unwrap()
        .encode_vec(vec![1, 0])
        .unwrap()
        .stream()
        .unwrap();
    drop(assert_item(&mut stream, 1));
    assert!(stream.next_item().unwrap().is_none());
    let receipt = stream.close_with_timeout(Some(WAIT));
    assert!(receipt.completed);
    assert!(receipt.terminal.unwrap().is_ok());
    assert_eq!(receipt.complete_items, Some(1));
    assert_eq!(fixture.server.payload_buffer_stats().unwrap().used_bytes, 0);
}

#[test]
fn unregister_during_final_step_reports_its_cancellation_cause() {
    let mut fixture = Fixture::new(true, 2, ServiceConcurrencyMode::ReadParallel, None, None);
    let mut stream = fixture.stream(0, 3);
    assert_eq!(fixture.step_events.recv_timeout(WAIT).unwrap(), (3, 0));
    let mut registration = fixture.registration.take().unwrap();
    let route = registration.route_concurrency();
    let closing = std::thread::spawn(move || registration.close());
    observe_until(|| route.snapshot().closed);
    fixture.probe.step_gate.release();
    assert_semantic(
        expect_stream_error(&mut stream),
        ErrorCode::CallCancelled,
        "unregister",
    );
    let receipt = stream.close_with_timeout(Some(WAIT));
    assert!(receipt.completed);
    assert_eq!(receipt.complete_items, Some(0));
    assert_semantic(
        receipt
            .terminal
            .unwrap()
            .expect_err("unregister cancelled final step"),
        ErrorCode::CallCancelled,
        "unregister",
    );
    assert_eq!(fixture.probe.finishes.load(Ordering::SeqCst), 1);
    assert!(closing.join().unwrap().unwrap().close.active_drained);
}

#[test]
fn shutdown_during_final_step_reports_its_cancellation_cause() {
    let fixture = Fixture::new(true, 2, ServiceConcurrencyMode::ReadParallel, None, None);
    let mut stream = fixture.stream(0, 3);
    assert_eq!(fixture.step_events.recv_timeout(WAIT).unwrap(), (3, 0));
    let route = fixture.registration.as_ref().unwrap().route_concurrency();
    let retirement = fixture
        .server
        .begin_session_retirement(
            RuntimeShutdownTarget::Host(&fixture.host),
            Instant::now() + WAIT,
        )
        .unwrap();
    observe_until(|| route.snapshot().closed);
    assert!(!retirement.observe_until(Instant::now()).1);
    fixture.probe.step_gate.release();
    assert_semantic(
        expect_stream_error(&mut stream),
        ErrorCode::CallCancelled,
        "shutdown",
    );
    let receipt = stream.close_with_timeout(Some(WAIT));
    assert!(receipt.completed);
    assert_eq!(receipt.complete_items, Some(0));
    assert_semantic(
        receipt
            .terminal
            .unwrap()
            .expect_err("shutdown cancelled final step"),
        ErrorCode::CallCancelled,
        "shutdown",
    );
    assert_eq!(fixture.probe.finishes.load(Ordering::SeqCst), 1);
    assert!(retirement.observe_until(Instant::now() + WAIT).1);
}

#[test]
fn unregister_during_cleanup_does_not_rewrite_an_already_natural_end() {
    let mut fixture = Fixture::new(true, 2, ServiceConcurrencyMode::ReadParallel, None, None);
    let mut stream = fixture.stream(0, 4);
    assert_eq!(fixture.finish_events.recv_timeout(WAIT).unwrap(), 4);
    let mut registration = fixture.registration.take().unwrap();
    let route = registration.route_concurrency();
    let closing = std::thread::spawn(move || registration.close());
    observe_until(|| route.snapshot().closed);
    fixture.probe.finish_gate.release();
    assert!(stream.next_item().unwrap().is_none());
    let receipt = stream.close_with_timeout(Some(WAIT));
    assert!(receipt.completed);
    assert!(receipt.terminal.unwrap().is_ok());
    assert_eq!(receipt.complete_items, Some(0));
    assert!(closing.join().unwrap().unwrap().close.active_drained);
}

#[test]
fn shutdown_during_cleanup_does_not_rewrite_an_already_natural_end() {
    let fixture = Fixture::new(true, 2, ServiceConcurrencyMode::ReadParallel, None, None);
    let mut stream = fixture.stream(0, 4);
    assert_eq!(fixture.finish_events.recv_timeout(WAIT).unwrap(), 4);
    let route = fixture.registration.as_ref().unwrap().route_concurrency();
    let retirement = fixture
        .server
        .begin_session_retirement(
            RuntimeShutdownTarget::Host(&fixture.host),
            Instant::now() + WAIT,
        )
        .unwrap();
    observe_until(|| route.snapshot().closed);
    assert!(!retirement.observe_until(Instant::now()).1);
    fixture.probe.finish_gate.release();
    assert!(stream.next_item().unwrap().is_none());
    let receipt = stream.close_with_timeout(Some(WAIT));
    assert!(receipt.completed);
    assert!(receipt.terminal.unwrap().is_ok());
    assert_eq!(receipt.complete_items, Some(0));
    assert!(retirement.observe_until(Instant::now() + WAIT).1);
}

#[test]
fn parked_streams_release_the_execution_worker_for_same_route_unary() {
    let fixture = Fixture::new(
        false,
        1,
        ServiceConcurrencyMode::ReadParallel,
        Some(16),
        None,
    );
    let mut streams = (0..6).map(|_| fixture.stream(100, 0)).collect::<Vec<_>>();
    let route = fixture.registration.as_ref().unwrap().route_concurrency();
    observe_until(|| {
        fixture.probe.opens.load(Ordering::SeqCst) == streams.len()
            && fixture.probe.steps.load(Ordering::SeqCst) >= streams.len()
            && route.snapshot().active_workers == 0
            && route.snapshot().pending == streams.len()
            && fixture
                .server
                .call_execution_snapshot()
                .unwrap()
                .used_operations
                == streams.len() as u64
    });
    let short = fixture
        .client
        .with_call_options(CallOptions::with_timeout(CallTimeout::After(WAIT)));
    assert_eq!(short.call_owned("ping", b"").unwrap(), b"pong");
    assert_eq!(
        fixture
            .server
            .call_execution_snapshot()
            .unwrap()
            .used_operations,
        6
    );
    for stream in streams.drain(..) {
        let receipt = stream.close_with_timeout(Some(WAIT));
        assert!(receipt.completed);
        assert_semantic(
            receipt
                .terminal
                .unwrap()
                .expect_err("consumer cancellation"),
            ErrorCode::CallCancelled,
            "consumer",
        );
    }
    assert_eq!(fixture.probe.finishes.load(Ordering::SeqCst), 6);
}

#[test]
fn one_runtime_budget_counts_receive_copy_and_multiple_client_holds() {
    let fixture = Fixture::new(
        false,
        2,
        ServiceConcurrencyMode::ReadParallel,
        None,
        Some(8),
    );
    let mut stream = fixture.stream(3, 5);
    assert_eq!(fixture.encoded_events.recv_timeout(WAIT).unwrap(), (5, 1));
    // The receiver owns the only four-byte B reservation. The gated encoder
    // writes one transport frame directly, with no whole-item Host copy.
    observe_until(|| fixture.server.payload_buffer_stats().unwrap().used_bytes == 4);
    assert_eq!(
        fixture
            .registration
            .as_ref()
            .unwrap()
            .route_concurrency()
            .snapshot()
            .active_workers,
        0
    );
    fixture.probe.encode_gate.release();
    let first = assert_item(&mut stream, 1);
    let second = assert_item(&mut stream, 2);
    assert_eq!(fixture.encoded_events.recv_timeout(WAIT).unwrap(), (5, 2));
    observe_until(|| fixture.server.payload_buffer_stats().unwrap().used_bytes >= 8);
    // Two holds fill B. The next resource step may produce its domain item,
    // but no frame is encoded until the receiver can reserve the third item.
    observe_until(|| fixture.probe.steps.load(Ordering::SeqCst) >= 3);
    observe_until(|| fixture.server.payload_buffer_stats().unwrap().used_bytes == 8);
    assert!(fixture.encoded_events.try_recv().is_err());
    let (started, started_rx) = mpsc::channel();
    let (returned, returned_rx) = mpsc::channel();
    let waiting = std::thread::spawn(move || {
        started.send(()).unwrap();
        let item = stream.next_item();
        returned.send(item).unwrap();
        stream
    });
    started_rx.recv_timeout(WAIT).unwrap();
    assert!(
        returned_rx.try_recv().is_err(),
        "third item bypassed full B"
    );
    drop(first);
    let third = returned_rx.recv_timeout(WAIT).unwrap().unwrap().unwrap();
    assert_eq!(third.bytes(), &[3; 4]);
    drop(second);
    drop(third);
    let mut stream = waiting.join().unwrap();
    assert!(stream.next_item().unwrap().is_none());
    let receipt = stream.close_with_timeout(Some(WAIT));
    assert!(receipt.completed);
    assert!(receipt.terminal.unwrap().is_ok());
    observe_until(|| fixture.server.payload_buffer_stats().unwrap().used_bytes == 0);
}

#[test]
fn unlimited_stream_uses_an_operation_slot_until_real_terminal() {
    let fixture = Fixture::new(
        false,
        2,
        ServiceConcurrencyMode::ReadParallel,
        Some(1),
        None,
    );
    let mut first = fixture.stream(3, 0);
    drop(assert_item(&mut first, 1));
    assert_eq!(
        fixture
            .server
            .call_execution_snapshot()
            .unwrap()
            .used_operations,
        1
    );
    let refusal = match fixture.client.begin_stream("echo") {
        Ok(_) => panic!("second unlimited stream must be refused before encoding"),
        Err(error) => error,
    };
    assert_semantic(refusal, ErrorCode::CallCapacityExceeded, "operation slot");
    let receipt = first.close_with_timeout(Some(WAIT));
    assert!(receipt.completed);
    assert_semantic(
        receipt
            .terminal
            .unwrap()
            .expect_err("first stream cancelled"),
        ErrorCode::CallCancelled,
        "consumer",
    );
    observe_until(|| {
        fixture
            .server
            .call_execution_snapshot()
            .unwrap()
            .used_operations
            == 0
    });
    let mut recovered = fixture.stream(1, 0);
    drop(assert_item(&mut recovered, 1));
    assert!(recovered.next_item().unwrap().is_none());
    let terminal = recovered.close_with_timeout(Some(WAIT));
    assert!(terminal.completed);
    assert!(terminal.terminal.unwrap().is_ok());
}

#[test]
fn shutdown_cleans_a_budget_waiter_but_receipt_waits_for_its_current_item() {
    let fixture = Fixture::new(
        false,
        2,
        ServiceConcurrencyMode::ReadParallel,
        None,
        Some(8),
    );
    let mut stream = fixture.stream(255, 0);
    let first = assert_item(&mut stream, 1);
    let second = assert_item(&mut stream, 2);
    observe_until(|| {
        fixture.probe.steps.load(Ordering::SeqCst) == 3
            && fixture.server.payload_buffer_stats().unwrap().used_bytes == 8
    });
    let receipt = fixture
        .server
        .begin_session_retirement(
            RuntimeShutdownTarget::Host(&fixture.host),
            Instant::now() + WAIT,
        )
        .unwrap();
    assert_eq!(fixture.finish_events.recv_timeout(WAIT).unwrap(), 0);
    assert_eq!(fixture.probe.finishes.load(Ordering::SeqCst), 1);
    assert!(
        !receipt.observe_until(Instant::now()).1,
        "cleanup finished but the current item is still waiting for B"
    );
    drop(first);
    let third = assert_item(&mut stream, 3);
    assert_semantic(
        expect_stream_error(&mut stream),
        ErrorCode::CallCancelled,
        "shutdown",
    );
    assert_eq!(fixture.probe.steps.load(Ordering::SeqCst), 3);
    drop(second);
    drop(third);
    let (outcome, completed) = receipt.observe_until(Instant::now() + WAIT);
    assert!(completed, "{outcome:?}");
    assert!(outcome.runtime_barrier_error.is_none(), "{outcome:?}");
}

#[test]
fn deadline_only_ends_waiting_and_the_original_stream_can_be_cancelled_later() {
    let fixture = Fixture::new(
        false,
        2,
        ServiceConcurrencyMode::ReadParallel,
        Some(1),
        None,
    );
    let timed = fixture
        .client
        .with_call_options(CallOptions::with_timeout(CallTimeout::After(
            Duration::from_secs(1),
        )));
    let mut stream = timed
        .begin_stream("echo")
        .unwrap()
        .encode_vec(vec![2, 3])
        .unwrap()
        .stream()
        .unwrap();
    assert_eq!(fixture.step_events.recv_timeout(WAIT).unwrap(), (3, 0));
    assert_semantic(
        expect_stream_error(&mut stream),
        ErrorCode::CallDeadlineExceeded,
        "deadline",
    );
    assert_eq!(fixture.probe.finishes.load(Ordering::SeqCst), 0);
    fixture.probe.step_gate.release();
    assert_eq!(fixture.encoded_events.recv_timeout(WAIT).unwrap(), (3, 1));
    assert_eq!(fixture.probe.finishes.load(Ordering::SeqCst), 0);
    assert_eq!(
        fixture
            .server
            .call_execution_snapshot()
            .unwrap()
            .used_operations,
        1
    );
    let receipt = stream.close_with_timeout(Some(WAIT));
    assert!(receipt.completed);
    assert_semantic(
        receipt
            .terminal
            .unwrap()
            .expect_err("explicit cancellation"),
        ErrorCode::CallCancelled,
        "consumer",
    );
    assert_eq!(fixture.probe.finishes.load(Ordering::SeqCst), 1);
}

#[test]
fn http_deadline_keeps_original_post_for_later_close() {
    let fixture = Fixture::with_relay(
        false,
        2,
        ServiceConcurrencyMode::ReadParallel,
        Some(1),
        None,
        true,
    );
    let client = fixture
        .server
        .connect(
            fixture.client.expected_route().clone(),
            Connect::ExplicitRelay {
                relay_url: fixture.server.relay_anchor_address_override().unwrap(),
            },
        )
        .unwrap();
    let timed = client.with_call_options(CallOptions::with_timeout(CallTimeout::After(
        Duration::from_secs(1),
    )));
    let mut stream = timed
        .begin_stream("echo")
        .unwrap()
        .encode_vec(vec![2, 3])
        .unwrap()
        .stream()
        .unwrap();
    assert_eq!(fixture.step_events.recv_timeout(WAIT).unwrap(), (3, 0));
    assert_semantic(
        expect_stream_error(&mut stream),
        ErrorCode::CallDeadlineExceeded,
        "deadline",
    );
    assert_eq!(fixture.probe.finishes.load(Ordering::SeqCst), 0);
    let pending = stream.close_with_timeout(Some(Duration::ZERO));
    assert!(!pending.completed);
    fixture.probe.step_gate.release();
    let receipt = stream.close_with_timeout(Some(WAIT));
    assert!(receipt.completed);
    assert_semantic(
        receipt
            .terminal
            .unwrap()
            .expect_err("explicit cancellation"),
        ErrorCode::CallCancelled,
        "consumer",
    );
    assert_eq!(fixture.probe.finishes.load(Ordering::SeqCst), 1);
}

#[test]
fn http_opening_cancellation_refuses_business_post() {
    let fixture = Fixture::with_relay(
        false,
        2,
        ServiceConcurrencyMode::ReadParallel,
        None,
        None,
        true,
    );
    let client = fixture
        .server
        .connect(
            fixture.client.expected_route().clone(),
            Connect::ExplicitRelay {
                relay_url: fixture.server.relay_anchor_address_override().unwrap(),
            },
        )
        .unwrap();
    let cancellation = c2_ipc::IpcStreamCancellation::new();
    cancellation.cancel();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = runtime.block_on(
        client
            .begin_stream("echo")
            .unwrap()
            .encode_vec(vec![1, 0])
            .unwrap()
            .stream_record_async(cancellation),
    );
    let error = match result {
        Ok(_) => panic!("canceled opening unexpectedly dispatched"),
        Err(error) => error,
    };
    assert_semantic(error, ErrorCode::CallCancelled, "before dispatch");
    assert_eq!(fixture.probe.opens.load(Ordering::SeqCst), 0);
}

#[test]
fn http_and_verified_relay_local_ipc_streams_share_core_facade() {
    let fixture = Fixture::with_relay(
        false,
        2,
        ServiceConcurrencyMode::ReadParallel,
        None,
        None,
        true,
    );
    let expected = fixture.client.expected_route().clone();
    let http = fixture
        .server
        .connect(
            expected.clone(),
            Connect::ExplicitRelay {
                relay_url: fixture.server.relay_anchor_address_override().unwrap(),
            },
        )
        .unwrap();
    let mut stream = http
        .begin_stream("echo")
        .unwrap()
        .encode_vec(vec![1, 0])
        .unwrap()
        .stream()
        .unwrap();
    drop(assert_item(&mut stream, 1));
    assert!(stream.next_item().unwrap().is_none());
    let receipt = stream.close();
    assert!(receipt.terminal.unwrap().is_ok());
    assert_eq!(receipt.complete_items, Some(1));
    assert_eq!(http.call_owned("ping", &[]).unwrap(), b"pong");
    let local = fixture
        .server
        .connect(expected.clone(), Connect::RelayAware)
        .unwrap();
    assert_eq!(
        local.observed_path(),
        c2_core::ObservedPath::RelayAwareLocalIpc
    );
    let mut stream = local
        .begin_stream("echo")
        .unwrap()
        .encode_vec(vec![1, 0])
        .unwrap()
        .stream()
        .unwrap();
    drop(assert_item(&mut stream, 1));
    assert!(stream.next_item().unwrap().is_none());
    assert!(stream.close().terminal.unwrap().is_ok());

    let unavailable = Runtime::new(RuntimeOptions {
        server_id: Some(unique("unavailable-anchor-client")),
        relay_anchor_address: Some("http://127.0.0.1:9".into()),
        use_process_relay_anchor: false,
        ..Default::default()
    })
    .unwrap();
    unavailable
        .inherit_local_endpoint_selection(&fixture.server)
        .unwrap();
    let direct = unavailable
        .connect(
            expected,
            Connect::DirectIpc {
                address: fixture.host.local_endpoint().address().into(),
            },
        )
        .unwrap();
    let mut stream = direct
        .begin_stream("echo")
        .unwrap()
        .encode_vec(vec![1, 0])
        .unwrap()
        .stream()
        .unwrap();
    drop(assert_item(&mut stream, 1));
    assert!(stream.next_item().unwrap().is_none());
    assert!(stream.close().terminal.unwrap().is_ok());
}

#[test]
fn explicit_http_zero_one_multiple_and_resource_errors_preserve_counts() {
    let fixture = Fixture::with_relay(
        false,
        2,
        ServiceConcurrencyMode::ReadParallel,
        None,
        None,
        true,
    );
    let client = fixture
        .server
        .connect(
            fixture.client.expected_route().clone(),
            Connect::ExplicitRelay {
                relay_url: fixture.server.relay_anchor_address_override().unwrap(),
            },
        )
        .unwrap();
    for count in [0, 1, 3] {
        let mut stream = client
            .begin_stream("echo")
            .unwrap()
            .encode_vec(vec![count, 0])
            .unwrap()
            .stream()
            .unwrap();
        for index in 1..=count {
            drop(assert_item(&mut stream, index));
        }
        assert!(stream.next_item().unwrap().is_none());
        let receipt = stream.close_with_timeout(Some(WAIT));
        assert!(receipt.completed);
        assert!(receipt.terminal.unwrap().is_ok());
        assert_eq!(receipt.complete_items, Some(count as u64));
    }
    for (mode, reason) in [
        (1, "step failed after complete items"),
        (2, "finish failed after complete items"),
    ] {
        let mut stream = client
            .begin_stream("echo")
            .unwrap()
            .encode_vec(vec![1, mode])
            .unwrap()
            .stream()
            .unwrap();
        drop(assert_item(&mut stream, 1));
        assert_semantic(
            expect_stream_error(&mut stream),
            ErrorCode::ResourceFunctionExecuting,
            reason,
        );
        let receipt = stream.close_with_timeout(Some(WAIT));
        assert!(receipt.completed);
        assert_eq!(receipt.complete_items, Some(1));
        assert_semantic(
            receipt.terminal.unwrap().expect_err("original failure"),
            ErrorCode::ResourceFunctionExecuting,
            reason,
        );
    }
    assert_eq!(fixture.probe.finishes.load(Ordering::SeqCst), 5);
}

#[test]
fn explicit_http_multiple_holds_release_b_for_next_item() {
    let fixture = Fixture::with_relay(
        false,
        2,
        ServiceConcurrencyMode::ReadParallel,
        None,
        Some(8),
        true,
    );
    let client = fixture
        .server
        .connect(
            fixture.client.expected_route().clone(),
            Connect::ExplicitRelay {
                relay_url: fixture.server.relay_anchor_address_override().unwrap(),
            },
        )
        .unwrap();
    let mut stream = client
        .begin_stream("echo")
        .unwrap()
        .encode_vec(vec![3, 0])
        .unwrap()
        .stream()
        .unwrap();
    let first = assert_item(&mut stream, 1);
    let second = assert_item(&mut stream, 2);
    observe_until(|| fixture.server.payload_buffer_stats().unwrap().used_bytes == 8);
    let (started, started_rx) = mpsc::channel();
    let (returned, returned_rx) = mpsc::channel();
    let waiting = std::thread::spawn(move || {
        started.send(()).unwrap();
        let third = stream.next_item();
        returned.send(third).unwrap();
        stream
    });
    started_rx.recv_timeout(WAIT).unwrap();
    assert!(
        returned_rx.try_recv().is_err(),
        "third item bypassed full B"
    );
    drop(first);
    let third = returned_rx.recv_timeout(WAIT).unwrap().unwrap().unwrap();
    assert_eq!(third.bytes(), &[3; 4]);
    drop(second);
    drop(third);
    let stream = waiting.join().unwrap();
    assert!(stream.next_item().unwrap().is_none());
    let receipt = stream.close_with_timeout(Some(WAIT));
    assert!(receipt.completed);
    assert_eq!(receipt.complete_items, Some(3));
    assert!(receipt.terminal.unwrap().is_ok());
    observe_until(|| fixture.server.payload_buffer_stats().unwrap().used_bytes == 0);
}

#[test]
fn explicit_http_close_keeps_cancelled_outcome_when_small_b_is_full() {
    let fixture = Fixture::with_relay(
        false,
        2,
        ServiceConcurrencyMode::ReadParallel,
        None,
        Some(8),
        true,
    );
    let client = fixture
        .server
        .connect(
            fixture.client.expected_route().clone(),
            Connect::ExplicitRelay {
                relay_url: fixture.server.relay_anchor_address_override().unwrap(),
            },
        )
        .unwrap();
    let mut stream = client
        .begin_stream("echo")
        .unwrap()
        .encode_vec(vec![0, 6])
        .unwrap()
        .stream()
        .unwrap();
    let first = assert_item(&mut stream, 1);
    let second = assert_item(&mut stream, 2);
    for index in 0..=2 {
        assert_eq!(fixture.step_events.recv_timeout(WAIT).unwrap(), (6, index));
    }
    assert_eq!(fixture.server.payload_buffer_stats().unwrap().used_bytes, 8);
    assert!(!stream.close_with_timeout(Some(Duration::ZERO)).completed);
    fixture.probe.step_gate.release();
    let receipt = stream.close_with_timeout(Some(WAIT));
    assert!(receipt.completed);
    assert_eq!(fixture.probe.finishes.load(Ordering::SeqCst), 1);
    assert_semantic(
        receipt.terminal.unwrap().expect_err("consumer cancelled"),
        ErrorCode::CallCancelled,
        "consumer",
    );
    // Each four-byte item is encoded in one range. HTTP/TCP buffering can
    // let the resource run ahead while cancellation reaches the relay;
    // compare the terminal with actual complete encodings, not a step bound.
    let encoded = fixture.encoded_events.try_iter().collect::<Vec<_>>();
    for (index, event) in encoded.iter().enumerate() {
        assert_eq!(*event, (6, index + 1));
    }
    assert_eq!(receipt.complete_items, Some(encoded.len() as u64));
    assert_eq!(first.bytes(), &[1; 4]);
    assert_eq!(second.bytes(), &[2; 4]);
}

#[test]
fn relay_shutdown_delivers_running_http_item_then_true_cancelled_terminal() {
    let mut fixture = Fixture::with_relay(
        false,
        2,
        ServiceConcurrencyMode::ReadParallel,
        None,
        None,
        true,
    );
    let client = fixture
        .server
        .connect(
            fixture.client.expected_route().clone(),
            Connect::ExplicitRelay {
                relay_url: fixture.server.relay_anchor_address_override().unwrap(),
            },
        )
        .unwrap();
    let mut stream = client
        .begin_stream("echo")
        .unwrap()
        .encode_vec(vec![3, 3])
        .unwrap()
        .stream()
        .unwrap();
    assert_eq!(fixture.step_events.recv_timeout(WAIT).unwrap(), (3, 0));
    let mut relay = fixture._relay.take().unwrap();
    let (stopped_tx, stopped_rx) = mpsc::channel();
    let stopping = std::thread::spawn(move || {
        stopped_tx.send(relay.stop()).unwrap();
    });
    // The relay keeps this existing HTTP response alive while its forwarding
    // admission gate closes. An independent ping observes that gate before the
    // gated resource step is released.
    observe_until(|| match client.call_owned("ping", &[]) {
        Ok(_) => false,
        Err(Error::Semantic(error)) if error.code == ErrorCode::CallCapacityExceeded => true,
        Err(error) => panic!("unexpected admission observation: {error}"),
    });
    assert!(
        stopped_rx.try_recv().is_err(),
        "relay stopped before running step ended"
    );
    fixture.probe.step_gate.release();
    drop(assert_item(&mut stream, 1));
    assert_semantic(
        expect_stream_error(&mut stream),
        ErrorCode::CallCancelled,
        "relay shutdown",
    );
    let receipt = stream.close_with_timeout(Some(WAIT));
    assert!(receipt.completed);
    assert_eq!(receipt.complete_items, Some(1));
    assert_semantic(
        receipt.terminal.unwrap().expect_err("relay shutdown"),
        ErrorCode::CallCancelled,
        "relay shutdown",
    );
    assert!(stopped_rx.recv_timeout(WAIT).unwrap().is_ok());
    stopping.join().unwrap();
}

#[test]
fn async_native_client_projection_shares_the_original_stream_and_unary_driver() {
    let fixture = Fixture::new(true, 2, ServiceConcurrencyMode::ReadParallel, None, None);
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let mut prepared = fixture.client.begin_stream("echo").unwrap();
        prepared.charge_input_async(2).await.unwrap();
        let stream = prepared
            .encode_vec(vec![3, 0])
            .unwrap()
            .stream_record_async(c2_ipc::IpcStreamCancellation::new())
            .await
            .unwrap();
        let unary = fixture
            .client
            .begin_call("ping")
            .unwrap()
            .encode_vec(vec![])
            .unwrap()
            .call_held_record_async()
            .await
            .unwrap();
        assert_eq!(unary.bytes(), b"pong");
        for expected in 1..=3 {
            assert_eq!(
                stream.next_item_async().await.unwrap().unwrap().bytes(),
                &[expected; 4]
            );
        }
        assert!(stream.next_item_async().await.unwrap().is_none());
        let receipt = stream.close_with_timeout_async(Some(Duration::ZERO)).await;
        assert!(receipt.completed);
        assert!(receipt.terminal.unwrap().is_ok());
        assert_eq!(receipt.complete_items, Some(3));
    });
}
