//! Real IPC 16 server-stream regressions. Each test owns its endpoint namespace.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use c2_config::LocalEndpointContext;
use c2_contract::{ExpectedRouteContract, ValidatedContractDescriptor};
use c2_error::ErrorCode;
use c2_ipc::{ClientIpcConfig, IpcClient, IpcResponseStream, RouteBinding, StreamTerminal};
use c2_mem::retention::PayloadBufferBudget;
use c2_server::{
    ConcurrencyMode, CrmCallback, CrmError, CrmStream, CrmStreamCallback, CrmStreamItem,
    ResponseMeta, ResponsePool, RouteBuildSpec, SchedulerLimits, Server, ServerIpcConfig,
};
use c2_wire::rpc::CallStatus;

const WAIT: Duration = Duration::from_secs(5);
const DESCRIPTOR: &[u8] =
    include_bytes!("../../../foundation/c2-contract/tests/fixtures/server-stream.contract.json");

fn expected_contract() -> ExpectedRouteContract {
    let descriptor = ValidatedContractDescriptor::from_json(DESCRIPTOR).unwrap();
    ExpectedRouteContract {
        route_name: "portable".into(),
        crm_ns: descriptor.crm_namespace().into(),
        crm_name: descriptor.crm_name().into(),
        crm_ver: descriptor.crm_version().into(),
        abi_hash: descriptor.abi_hash().into(),
        signature_hash: descriptor.signature_hash().into(),
    }
}

#[derive(Default)]
struct Observed {
    opened: AtomicUsize,
    steps: AtomicUsize,
    finished: AtomicUsize,
    step_changed: tokio::sync::Notify,
    step_entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release_step: Mutex<Option<mpsc::Receiver<()>>>,
    unary_entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release_unary: Mutex<Option<mpsc::Receiver<()>>>,
    finish_entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release_finish: Mutex<Option<mpsc::Receiver<()>>>,
}

struct ScriptedCallback(Arc<Observed>);

struct ScriptedStream {
    observed: Arc<Observed>,
    produced: usize,
    item_count: usize,
    fail_step: bool,
    fail_finish: bool,
    item_bytes: usize,
}

impl CrmCallback for ScriptedCallback {
    fn invoke(
        &self,
        _: &str,
        method_idx: u16,
        _: Vec<u8>,
        _: Arc<ResponsePool>,
    ) -> Result<ResponseMeta, CrmError> {
        assert_eq!(method_idx, 0);
        if let Some(entered) = self.0.unary_entered.lock().unwrap().take() {
            entered.send(()).unwrap();
        }
        if let Some(release) = self.0.release_unary.lock().unwrap().take() {
            release.recv().unwrap();
        }
        Ok(ResponseMeta::Inline(b"pong".to_vec()))
    }
}

impl CrmStreamCallback for ScriptedCallback {
    fn open_stream(
        &self,
        _: &str,
        method_idx: u16,
        request: Vec<u8>,
        _: Arc<ResponsePool>,
    ) -> Result<Box<dyn CrmStream>, CrmError> {
        assert_eq!(method_idx, 1);
        self.0.opened.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(ScriptedStream {
            observed: self.0.clone(),
            item_count: request.first().copied().unwrap_or_default() as usize,
            fail_step: request.get(1) == Some(&1),
            fail_finish: request.get(1) == Some(&2),
            item_bytes: request.get(2).copied().map_or(8, usize::from),
            produced: 0,
        }))
    }
}

impl CrmStream for ScriptedStream {
    fn step(&mut self) -> Result<Option<Box<dyn CrmStreamItem>>, CrmError> {
        self.observed.steps.fetch_add(1, Ordering::SeqCst);
        self.observed.step_changed.notify_waiters();
        if let Some(entered) = self.observed.step_entered.lock().unwrap().take() {
            entered.send(()).unwrap();
        }
        if let Some(release) = self.observed.release_step.lock().unwrap().take() {
            release.recv().unwrap();
        }
        if self.produced == self.item_count {
            return if self.fail_step {
                Err(CrmError::InternalError("scripted step failure".into()))
            } else {
                Ok(None)
            };
        }
        let byte = self.produced as u8;
        self.produced += 1;
        Ok(Some(Box::new(vec![byte; self.item_bytes])))
    }

    fn finish(self: Box<Self>) -> Result<(), CrmError> {
        self.observed.finished.fetch_add(1, Ordering::SeqCst);
        if let Some(entered) = self.observed.finish_entered.lock().unwrap().take() {
            entered.send(()).unwrap();
        }
        if let Some(release) = self.observed.release_finish.lock().unwrap().take() {
            release.recv().unwrap();
        }
        if self.fail_finish {
            Err(CrmError::InternalError("scripted cleanup failure".into()))
        } else {
            Ok(())
        }
    }
}

struct Harness {
    _root: Option<tempfile::TempDir>,
    server: Arc<Server>,
    run: tokio::task::JoinHandle<Result<(), c2_server::ServerError>>,
    client: Arc<IpcClient>,
    binding: RouteBinding,
    observed: Arc<Observed>,
}

impl Harness {
    async fn start(max_execution_workers: u32) -> Self {
        Self::start_with_config(ServerIpcConfig {
            max_execution_workers,
            ..ServerIpcConfig::default()
        })
        .await
    }

    async fn start_with_config(config: ServerIpcConfig) -> Self {
        let descriptor = ValidatedContractDescriptor::from_json(DESCRIPTOR).unwrap();
        static NEXT_ADDRESS: AtomicUsize = AtomicUsize::new(0);
        let address = format!(
            "ipc://stream-{}-{}",
            std::process::id(),
            NEXT_ADDRESS.fetch_add(1, Ordering::Relaxed)
        );
        #[cfg(unix)]
        let root = tempfile::Builder::new()
            .prefix("c2s-")
            .tempdir_in("/tmp")
            .unwrap();
        #[cfg(unix)]
        let context = LocalEndpointContext::with_unix_root(root.path()).unwrap();
        #[cfg(windows)]
        let context = LocalEndpointContext::default_for_platform().unwrap();
        let endpoint = context.endpoint(&address).unwrap();
        let server = Arc::new(Server::new_with_endpoint(endpoint.clone(), config).unwrap());
        let observed = Arc::new(Observed::default());
        let built = server
            .build_stream_route(
                RouteBuildSpec {
                    name: "portable".into(),
                    crm_ns: descriptor.crm_namespace().into(),
                    crm_name: descriptor.crm_name().into(),
                    crm_ver: descriptor.crm_version().into(),
                    abi_hash: descriptor.abi_hash().into(),
                    signature_hash: descriptor.signature_hash().into(),
                    method_names: vec!["ping".into(), "echo".into()],
                    access_map: HashMap::new(),
                    concurrency_mode: ConcurrencyMode::Parallel,
                    limits: SchedulerLimits::default(),
                },
                Arc::new(ScriptedCallback(observed.clone())),
                &descriptor,
            )
            .unwrap();
        let reservation = server.reserve_route(built).await.unwrap();
        server.commit_reserved_route(reservation).await.unwrap();
        server.begin_start_attempt().unwrap();
        let run = tokio::spawn({
            let server = server.clone();
            async move { server.run().await }
        });
        server.wait_until_ready(WAIT).await.unwrap();
        let mut client = IpcClient::with_endpoint(endpoint, ClientIpcConfig::default()).unwrap();
        client.connect().await.unwrap();
        let binding = client.acquire_route(&expected_contract()).await.unwrap();
        let client = Arc::new(client);
        Self {
            _root: {
                #[cfg(unix)]
                {
                    Some(root)
                }
                #[cfg(windows)]
                {
                    None
                }
            },
            server,
            run,
            client,
            binding,
            observed,
        }
    }

    async fn stop(self) {
        self.client.close_shared().await;
        self.server.shutdown_and_wait(WAIT).await.unwrap();
        self.run.await.unwrap().unwrap();
    }

    async fn unary(&self) {
        let reply = self
            .client
            .call_bound(&self.binding, "ping", b"")
            .await
            .unwrap();
        assert_eq!(reply.into_owned_bytes().unwrap(), b"pong");
    }
}

async fn wait_for_steps(observed: &Observed, expected: usize) {
    tokio::time::timeout(WAIT, async {
        loop {
            let changed = observed.step_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if observed.steps.load(Ordering::SeqCst) >= expected {
                return;
            }
            changed.await;
        }
    })
    .await
    .expect("resource steps did not advance");
}

async fn wait_for_signal(signal: tokio::sync::oneshot::Receiver<()>) {
    tokio::time::timeout(WAIT, signal)
        .await
        .expect("test callback did not reach its barrier")
        .expect("test callback dropped its barrier");
}

async fn observed_terminal(stream: &IpcResponseStream) -> StreamTerminal {
    tokio::time::timeout(WAIT, stream.completion())
        .await
        .expect("stream did not settle")
        .expect("stream did not return a terminal")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zero_one_and_multiple_items_have_exact_terminal_counts() {
    let fixture = Harness::start(2).await;
    for count in [0_u8, 1, 4] {
        let mut stream = fixture
            .client
            .stream_bound(
                &fixture.binding,
                "echo",
                &[count],
                PayloadBufferBudget::new(1024),
            )
            .await
            .unwrap();
        for expected in 0..count {
            let item = stream.recv().await.unwrap().expect("complete item");
            assert_eq!(item.as_ref(), &[expected; 8]);
        }
        assert!(stream.recv().await.unwrap().is_none());
        let terminal = observed_terminal(&stream).await;
        assert_eq!(terminal.status, CallStatus::Success);
        assert_eq!(terminal.complete_items, count as u64);
        assert!(terminal.error.is_none());
    }
    assert_eq!(fixture.observed.opened.load(Ordering::SeqCst), 3);
    assert_eq!(fixture.observed.finished.load(Ordering::SeqCst), 3);
    fixture.unary().await;
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn zero_length_items_need_no_storage_credit_but_keep_demand_and_counts() {
    let fixture = Harness::start(1).await;
    let budget = PayloadBufferBudget::new(0);
    let mut stream = fixture
        .client
        .stream_bound(&fixture.binding, "echo", &[3, 0, 0], budget.clone())
        .await
        .unwrap();
    for _ in 0..3 {
        let item = tokio::time::timeout(WAIT, stream.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(item.as_ref().is_empty());
    }
    assert!(
        tokio::time::timeout(WAIT, stream.recv())
            .await
            .unwrap()
            .unwrap()
            .is_none()
    );
    assert_eq!(observed_terminal(&stream).await.complete_items, 3);
    assert_eq!(budget.snapshot().used_bytes, 0);
    fixture.unary().await;
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resource_and_cleanup_errors_follow_only_complete_items() {
    let fixture = Harness::start(2).await;
    for failure in [1_u8, 2] {
        let mut stream = fixture
            .client
            .stream_bound(
                &fixture.binding,
                "echo",
                &[2, failure],
                PayloadBufferBudget::new(1024),
            )
            .await
            .unwrap();
        for expected in 0..2_u8 {
            let item = stream
                .recv()
                .await
                .unwrap()
                .expect("preceding complete item");
            assert_eq!(item.as_ref(), &[expected; 8]);
        }
        let error = stream
            .recv()
            .await
            .expect_err("resource or cleanup failure follows complete items");
        assert!(format!("{error:?}").contains("CrmError"), "{error:?}");
        let terminal = observed_terminal(&stream).await;
        assert_eq!(terminal.status, CallStatus::Failed);
        assert_eq!(terminal.complete_items, 2);
        assert!(terminal.error.is_some());
    }
    assert_eq!(fixture.observed.finished.load(Ordering::SeqCst), 2);
    fixture.unary().await;
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn waiting_resource_step_does_not_hold_an_execution_worker() {
    let fixture = Harness::start(1).await;
    let mut streams = Vec::new();
    for _ in 0..4 {
        streams.push(
            fixture
                .client
                .stream_bound(
                    &fixture.binding,
                    "echo",
                    &[10],
                    PayloadBufferBudget::new(1024),
                )
                .await
                .unwrap(),
        );
    }
    for stream in &mut streams {
        assert_eq!(stream.recv().await.unwrap().unwrap().as_ref(), &[0; 8]);
    }
    wait_for_steps(&fixture.observed, 12).await;
    fixture.unary().await;
    let snapshot = tokio::time::timeout(WAIT, async {
        loop {
            let snapshot = fixture
                .server
                .route_scheduler_snapshot("portable")
                .await
                .unwrap();
            if snapshot.active_workers == 0 {
                break snapshot;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("yielded streams kept route workers active");
    assert_eq!(snapshot.pending, 4);
    assert_eq!(snapshot.active_workers, 0);
    for stream in streams {
        stream.cancel().unwrap();
        let terminal = observed_terminal(&stream).await;
        assert_eq!(terminal.status, CallStatus::Failed);
        assert_eq!(terminal.error.unwrap().code, ErrorCode::CallCancelled);
    }
    assert_eq!(fixture.observed.finished.load(Ordering::SeqCst), 4);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_during_running_step_waits_for_step_and_cleanup_once() {
    let fixture = Harness::start(1).await;
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *fixture.observed.step_entered.lock().unwrap() = Some(entered_tx);
    *fixture.observed.release_step.lock().unwrap() = Some(release_rx);
    let stream = fixture
        .client
        .stream_bound(
            &fixture.binding,
            "echo",
            &[10],
            PayloadBufferBudget::new(1024),
        )
        .await
        .unwrap();
    wait_for_signal(entered_rx).await;
    stream.cancel().unwrap();
    stream.cancel().unwrap();
    fixture
        .client
        .lookup_route(&expected_contract())
        .await
        .unwrap();
    assert_eq!(fixture.observed.finished.load(Ordering::SeqCst), 0);
    release_tx.send(()).unwrap();
    let terminal = observed_terminal(&stream).await;
    assert_eq!(terminal.status, CallStatus::Failed);
    assert_eq!(terminal.error.unwrap().code, ErrorCode::CallCancelled);
    assert_eq!(fixture.observed.steps.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.observed.finished.load(Ordering::SeqCst), 1);
    fixture.unary().await;
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_delivery_grants_one_step_ahead_without_waiting_for_recv() {
    let fixture = Harness::start(1).await;
    let mut stream = fixture
        .client
        .stream_bound(
            &fixture.binding,
            "echo",
            &[6],
            PayloadBufferBudget::new(1024),
        )
        .await
        .unwrap();
    wait_for_steps(&fixture.observed, 2).await;
    assert_eq!(fixture.observed.steps.load(Ordering::SeqCst), 2);
    assert_eq!(
        fixture
            .server
            .route_scheduler_snapshot("portable")
            .await
            .unwrap()
            .active_workers,
        0
    );
    fixture.unary().await;
    assert_eq!(fixture.observed.steps.load(Ordering::SeqCst), 2);

    let first = stream.recv().await.unwrap().unwrap();
    assert_eq!(first.as_ref(), &[0; 8]);
    wait_for_steps(&fixture.observed, 3).await;
    assert_eq!(fixture.observed.steps.load(Ordering::SeqCst), 3);
    stream.cancel().unwrap();
    let terminal = observed_terminal(&stream).await;
    assert_eq!(terminal.status, CallStatus::Failed);
    assert_eq!(terminal.error.unwrap().code, ErrorCode::CallCancelled);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn budget_wait_preserves_other_calls_and_resumes_after_a_local_copy_drops() {
    let fixture = Harness::start(1).await;
    let budget = PayloadBufferBudget::new(16);
    let mut stream = fixture
        .client
        .stream_bound(&fixture.binding, "echo", &[4], budget.clone())
        .await
        .unwrap();
    let first = stream.recv().await.unwrap().unwrap();
    let second = stream.recv().await.unwrap().unwrap();
    assert_eq!(first.as_ref(), &[0; 8]);
    assert_eq!(second.as_ref(), &[1; 8]);
    wait_for_steps(&fixture.observed, 3).await;
    assert_eq!(budget.snapshot().used_bytes, 16);
    fixture.unary().await;
    assert_eq!(
        fixture
            .server
            .route_scheduler_snapshot("portable")
            .await
            .unwrap()
            .active_workers,
        0
    );

    drop(first);
    let third = tokio::time::timeout(WAIT, stream.recv())
        .await
        .expect("budget release must wake the waiting item")
        .unwrap()
        .unwrap();
    assert_eq!(third.as_ref(), &[2; 8]);
    stream.cancel().unwrap();
    let terminal = observed_terminal(&stream).await;
    assert_eq!(terminal.status, CallStatus::Failed);
    assert_eq!(terminal.error.unwrap().code, ErrorCode::CallCancelled);
    drop(second);
    drop(third);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_while_second_item_waits_for_budget_does_not_wait_for_the_held_copy() {
    let fixture = Harness::start(1).await;
    let budget = PayloadBufferBudget::new(8);
    let mut stream = fixture
        .client
        .stream_bound(&fixture.binding, "echo", &[4], budget.clone())
        .await
        .unwrap();
    let first = stream.recv().await.unwrap().unwrap();
    assert_eq!(first.as_ref(), &[0; 8]);
    wait_for_steps(&fixture.observed, 2).await;
    assert_eq!(budget.snapshot().used_bytes, 8);
    stream.cancel().unwrap();
    let terminal = tokio::time::timeout(WAIT, stream.completion())
        .await
        .expect("cancel cleanup must bypass the full payload budget")
        .unwrap();
    assert_eq!(terminal.status, CallStatus::Failed);
    assert_eq!(terminal.error.unwrap().code, ErrorCode::CallCancelled);
    assert_eq!(first.as_ref(), &[0; 8]);
    fixture.unary().await;
    drop(first);
    assert_eq!(budget.snapshot().used_bytes, 0);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn item_larger_than_entire_budget_fails_locally_and_keeps_call_until_cleanup() {
    let fixture = Harness::start(1).await;
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *fixture.observed.finish_entered.lock().unwrap() = Some(entered_tx);
    *fixture.observed.release_finish.lock().unwrap() = Some(release_rx);
    let mut stream = tokio::time::timeout(
        WAIT,
        fixture.client.stream_bound(
            &fixture.binding,
            "echo",
            &[3, 0, 9],
            PayloadBufferBudget::new(8),
        ),
    )
    .await
    .expect("stream OPEN did not submit")
    .unwrap();
    let error = tokio::time::timeout(WAIT, stream.recv())
        .await
        .expect("oversize item did not fail locally")
        .expect_err("9-byte item cannot fit an 8-byte budget");
    assert!(
        format!("{error:?}").contains("CallCapacityExceeded"),
        "{error:?}"
    );
    wait_for_signal(entered_rx).await;
    assert_eq!(fixture.client.pending_len_for_test(), 1);
    release_tx.send(()).unwrap();
    let terminal = stream
        .completion()
        .await
        .expect("close observes the server cleanup terminal after local capacity failure");
    assert_eq!(terminal.status, CallStatus::Failed);
    assert_eq!(terminal.error.unwrap().code, ErrorCode::CallCancelled);
    assert!(format!("{:?}", stream.recv().await.unwrap_err()).contains("CallCapacityExceeded"));
    assert_eq!(fixture.client.pending_len_for_test(), 0);
    fixture.unary().await;
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_before_first_resource_step_is_not_executed() {
    let fixture = Harness::start(1).await;
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *fixture.observed.unary_entered.lock().unwrap() = Some(entered_tx);
    *fixture.observed.release_unary.lock().unwrap() = Some(release_rx);
    let client = fixture.client.clone();
    let binding = fixture.binding.clone();
    let unary = tokio::spawn(async move { client.call_bound(&binding, "ping", b"").await });
    wait_for_signal(entered_rx).await;
    let stream = fixture
        .client
        .stream_bound(
            &fixture.binding,
            "echo",
            &[3],
            PayloadBufferBudget::new(1024),
        )
        .await
        .unwrap();
    stream.cancel().unwrap();
    fixture
        .client
        .lookup_route(&expected_contract())
        .await
        .unwrap();
    release_tx.send(()).unwrap();
    unary.await.unwrap().unwrap();
    let terminal = observed_terminal(&stream).await;
    assert_eq!(terminal.status, CallStatus::NotExecuted);
    assert_eq!(terminal.complete_items, 0);
    assert_eq!(terminal.error.unwrap().code, ErrorCode::CallCancelled);
    assert_eq!(fixture.observed.opened.load(Ordering::SeqCst), 0);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn late_cancel_does_not_rewrite_successful_terminal() {
    let fixture = Harness::start(1).await;
    let stream = fixture
        .client
        .stream_bound(
            &fixture.binding,
            "echo",
            &[0],
            PayloadBufferBudget::new(1024),
        )
        .await
        .unwrap();
    let terminal = observed_terminal(&stream).await;
    assert_eq!(terminal.status, CallStatus::Success);
    assert_eq!(terminal.complete_items, 0);
    stream.cancel().unwrap();
    stream.cancel().unwrap();
    let after_cancel = observed_terminal(&stream).await;
    assert_eq!(after_cancel.status, CallStatus::Success);
    assert_eq!(after_cancel.complete_items, 0);
    assert!(after_cancel.error.is_none());
    assert_eq!(fixture.observed.finished.load(Ordering::SeqCst), 1);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removed_route_returns_decodable_not_executed_to_old_stream_binding() {
    let fixture = Harness::start(1).await;
    assert!(fixture.server.unregister_route("portable").await);
    let result = fixture
        .client
        .stream_bound(
            &fixture.binding,
            "echo",
            &[1],
            PayloadBufferBudget::new(1024),
        )
        .await;
    match result {
        Ok(stream) => {
            let terminal = observed_terminal(&stream).await;
            assert_eq!(terminal.status, CallStatus::NotExecuted);
            assert_eq!(terminal.complete_items, 0);
            assert_eq!(terminal.error.unwrap().code, ErrorCode::ResourceRemoved);
        }
        Err(error) => {
            assert!(error.is_retry_safe(), "{error:?}");
            assert!(
                format!("{error:?}").contains("ResourceRemoved"),
                "{error:?}"
            );
        }
    }
    assert_eq!(fixture.observed.opened.load(Ordering::SeqCst), 0);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn changed_contract_returns_decodable_not_executed_to_old_stream_binding() {
    let fixture = Harness::start(1).await;
    let descriptor = ValidatedContractDescriptor::from_json(DESCRIPTOR).unwrap();
    assert!(fixture.server.unregister_route("portable").await);
    let replacement = fixture
        .server
        .build_route(
            RouteBuildSpec {
                name: "portable".into(),
                crm_ns: descriptor.crm_namespace().into(),
                crm_name: descriptor.crm_name().into(),
                crm_ver: descriptor.crm_version().into(),
                abi_hash: descriptor.abi_hash().into(),
                signature_hash: "0".repeat(64),
                method_names: vec!["ping".into(), "echo".into()],
                access_map: HashMap::new(),
                concurrency_mode: ConcurrencyMode::Parallel,
                limits: SchedulerLimits::default(),
            },
            Arc::new(ScriptedCallback(fixture.observed.clone())),
        )
        .unwrap();
    let reservation = fixture.server.reserve_route(replacement).await.unwrap();
    fixture
        .server
        .commit_reserved_route(reservation)
        .await
        .unwrap();

    let result = fixture
        .client
        .stream_bound(
            &fixture.binding,
            "echo",
            &[1],
            PayloadBufferBudget::new(1024),
        )
        .await;
    match result {
        Ok(stream) => {
            let terminal = observed_terminal(&stream).await;
            assert_eq!(terminal.status, CallStatus::NotExecuted);
            assert_eq!(terminal.complete_items, 0);
            assert_eq!(terminal.error.unwrap().code, ErrorCode::ContractMismatch);
        }
        Err(error) => {
            assert!(error.is_retry_safe(), "{error:?}");
            assert!(
                format!("{error:?}").contains("ContractMismatch"),
                "{error:?}"
            );
        }
    }
    assert_eq!(fixture.observed.opened.load(Ordering::SeqCst), 0);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn deactivation_preserves_admitted_stream_and_local_copy_after_shm_receive() {
    let fixture = Harness::start_with_config(ServerIpcConfig {
        max_execution_workers: 1,
        shm_threshold: 1,
        ..ServerIpcConfig::default()
    })
    .await;
    let mut stream = fixture
        .client
        .stream_bound(
            &fixture.binding,
            "echo",
            &[3, 0, 64],
            PayloadBufferBudget::new(1024),
        )
        .await
        .unwrap();
    let first = stream.recv().await.unwrap().unwrap();
    assert_eq!(first.as_ref(), &[0; 64]);
    fixture.client.deactivate();
    assert_eq!(first.as_ref(), &[0; 64]);
    for expected in 1..3_u8 {
        let item = stream.recv().await.unwrap().unwrap();
        assert_eq!(item.as_ref(), &[expected; 64]);
    }
    assert!(stream.recv().await.unwrap().is_none());
    let terminal = observed_terminal(&stream).await;
    assert_eq!(terminal.status, CallStatus::Success);
    assert_eq!(terminal.complete_items, 3);
    assert_eq!(first.as_ref(), &[0; 64]);
    fixture.stop().await;
}
