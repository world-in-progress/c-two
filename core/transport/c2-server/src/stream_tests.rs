use super::*;
use crate::dispatcher::CrmStream;
use crate::test_support::isolated_pair;
use c2_contract::ValidatedContractDescriptor;
use c2_error::ErrorCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use tokio::sync::oneshot;

const STREAM_DESCRIPTOR: &[u8] =
    include_bytes!("../../../foundation/c2-contract/tests/fixtures/server-stream.contract.json");

struct CounterStream {
    next: u8,
    total: u8,
    failure: u8,
    wide_capacity: bool,
    observed: Arc<Observed>,
}

#[derive(Default)]
struct Observed {
    steps: AtomicUsize,
    finished: AtomicUsize,
    item_bytes: AtomicUsize,
    step_entered: std::sync::Mutex<Option<oneshot::Sender<()>>>,
    release_step: std::sync::Mutex<Option<mpsc::Receiver<()>>>,
    unary_entered: std::sync::Mutex<Option<oneshot::Sender<()>>>,
    release_unary: std::sync::Mutex<Option<mpsc::Receiver<()>>>,
}

impl CrmStream for CounterStream {
    fn step(&mut self) -> Result<Option<ResponseMeta>, CrmError> {
        self.observed.steps.fetch_add(1, Ordering::SeqCst);
        if self.next == self.total {
            return if self.failure == 1 {
                Err(CrmError::InternalError("step failed".into()))
            } else {
                Ok(None)
            };
        }
        if let Some(entered) = self.observed.step_entered.lock().unwrap().take() {
            let _ = entered.send(());
        }
        if let Some(release) = self.observed.release_step.lock().unwrap().take() {
            release.recv().unwrap();
        }
        if self.failure == 3 {
            return Err(CrmError::InternalError("running step failed".into()));
        }
        let value = self.next;
        self.next += 1;
        let data = if self.wide_capacity {
            let mut data = Vec::with_capacity(64);
            data.extend_from_slice(&[value; 4]);
            data
        } else {
            vec![value; self.observed.item_bytes.load(Ordering::SeqCst).max(4)]
        };
        Ok(Some(ResponseMeta::Inline(data)))
    }

    fn finish(self: Box<Self>) -> Result<(), CrmError> {
        self.observed.finished.fetch_add(1, Ordering::SeqCst);
        if self.failure == 2 {
            Err(CrmError::InternalError("finish failed".into()))
        } else {
            Ok(())
        }
    }
}

#[test]
fn peer_end_prevents_a_step_already_queued_to_the_blocking_pool() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let observed = Arc::new(Observed::default());
            let mut harness = Harness::start(observed.clone(), ServerIpcConfig::default(), 0).await;
            harness.open(1, &[2]).await;
            assert!(matches!(
                read_record(&mut harness.peer, CallMode::ServerStream).await,
                SeenRecord::Begin(4)
            ));
            harness
                .peer
                .write_all(&rpc::encode_credit_frame(1, 4, 0).unwrap())
                .await
                .unwrap();
            assert!(matches!(
                read_record(&mut harness.peer, CallMode::ServerStream).await,
                SeenRecord::Data(_)
            ));

            let (entered, waiting) = oneshot::channel();
            let (release, resume) = mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                entered.send(()).unwrap();
                resume.recv().unwrap();
            });
            waiting.await.unwrap();
            harness
                .peer
                .write_all(&rpc::encode_credit_frame(1, 0, 1).unwrap())
                .await
                .unwrap();
            let conn = harness
                .server
                .active_connection(harness.server.active_connection_ids()[0])
                .unwrap();
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let snapshot = harness
                        .server
                        .route_scheduler_snapshot("portable")
                        .await
                        .unwrap();
                    if snapshot.active_workers == 1
                        && conn
                            .core
                            .stream_state(1)
                            .is_some_and(|state| state.producing)
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            drop(harness.peer);
            tokio::time::timeout(Duration::from_secs(5), conn.wait_work_cancelled())
                .await
                .unwrap();
            release.send(()).unwrap();
            blocker.await.unwrap();
            tokio::time::timeout(Duration::from_secs(5), harness.handler)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                observed.steps.load(Ordering::SeqCst),
                1,
                "the queued step never entered the resource"
            );
            assert_eq!(observed.finished.load(Ordering::SeqCst), 1);
        });
}

struct CounterCallback(Arc<Observed>);

impl CrmCallback for CounterCallback {
    fn invoke(
        &self,
        _: &str,
        _: u16,
        _: Vec<u8>,
        _: Arc<crate::ResponsePool>,
    ) -> Result<ResponseMeta, CrmError> {
        if let Some(entered) = self.0.unary_entered.lock().unwrap().take() {
            let _ = entered.send(());
        }
        if let Some(release) = self.0.release_unary.lock().unwrap().take() {
            release.recv().unwrap();
        }
        Ok(ResponseMeta::Inline(b"pong".to_vec()))
    }
}

impl CrmStreamCallback for CounterCallback {
    fn open_stream(
        &self,
        _: &str,
        _: u16,
        request: Vec<u8>,
        _: Arc<crate::ResponsePool>,
    ) -> Result<Box<dyn CrmStream>, CrmError> {
        Ok(Box::new(CounterStream {
            next: 0,
            total: request[0],
            failure: request.get(1).copied().unwrap_or(0),
            wide_capacity: request.get(2) == Some(&1),
            observed: self.0.clone(),
        }))
    }
}

#[derive(Debug)]
enum SeenRecord {
    Begin(u64),
    Data(Vec<u8>),
    ShmData(BuddyPayload),
    End(CallStatus, Option<u64>, Option<ErrorCode>),
}

async fn read_record(peer: &mut LocalStream, mode: CallMode) -> SeenRecord {
    let mut len = [0; 4];
    tokio::time::timeout(Duration::from_secs(5), peer.read_exact(&mut len))
        .await
        .unwrap()
        .unwrap();
    let total_len = u32::from_le_bytes(len);
    let mut body = vec![0; total_len as usize];
    tokio::time::timeout(Duration::from_secs(5), peer.read_exact(&mut body))
        .await
        .unwrap()
        .unwrap();
    let mut frame_bytes = len.to_vec();
    frame_bytes.extend_from_slice(&body);
    let (header, payload) = frame::decode_frame(&frame_bytes).unwrap();
    let record = rpc::decode_rpc_record(&header, payload, Some(mode))
        .unwrap_or_else(|error| panic!("record {header:?} {payload:?}: {error:?}"));
    match record {
        RpcRecord::ItemBegin { total_bytes } => SeenRecord::Begin(total_bytes),
        RpcRecord::ItemData {
            value:
                ValuePart {
                    storage: ValueStorage::Inline(bytes),
                    ..
                },
        } => SeenRecord::Data(bytes.to_vec()),
        RpcRecord::ItemData {
            value:
                ValuePart {
                    storage: ValueStorage::Shm(descriptor),
                    ..
                },
        } => SeenRecord::ShmData(descriptor),
        RpcRecord::CallEnd {
            status,
            complete_items,
            error,
            ..
        } => SeenRecord::End(status, complete_items, error.map(|error| error.code)),
        other => panic!("unexpected record: {other:?}"),
    }
}

struct Harness {
    server: Arc<Server>,
    peer: LocalStream,
    handler: tokio::task::JoinHandle<()>,
    identity: RouteCallIdentity,
}

impl Harness {
    async fn start(observed: Arc<Observed>, config: ServerIpcConfig, capabilities: u16) -> Self {
        Self::start_with_budget(observed, config, capabilities, None).await
    }

    async fn start_with_budget(
        observed: Arc<Observed>,
        config: ServerIpcConfig,
        capabilities: u16,
        budget: Option<u64>,
    ) -> Self {
        let descriptor = ValidatedContractDescriptor::from_json(STREAM_DESCRIPTOR).unwrap();
        let mut server = Server::new_with_endpoint(
            c2_config::LocalEndpointContext::default_for_platform()
                .unwrap()
                .endpoint(&format!(
                    "ipc://stream-test-{}",
                    uuid::Uuid::new_v4().simple()
                ))
                .unwrap(),
            config,
        )
        .unwrap();
        if let Some(bytes) = budget {
            server.stream_payload_budget = PayloadBufferBudget::new(bytes);
        }
        let server = Arc::new(server);
        let built = server
            .build_stream_route(
                RouteBuildSpec {
                    name: "portable".into(),
                    crm_ns: descriptor.crm_namespace().into(),
                    crm_name: descriptor.crm_name().into(),
                    crm_ver: descriptor.crm_version().into(),
                    abi_hash: descriptor.abi_hash().into(),
                    signature_hash: descriptor.signature_hash().into(),
                    method_names: descriptor
                        .methods()
                        .iter()
                        .map(|method| method.name().into())
                        .collect(),
                    access_map: HashMap::new(),
                    concurrency_mode: crate::scheduler::ConcurrencyMode::Exclusive,
                    limits: SchedulerLimits::default(),
                },
                Arc::new(CounterCallback(observed)),
                &descriptor,
            )
            .unwrap();
        let identity = RouteCallIdentity {
            route_name: built.name().to_string(),
            route_uid: built.route_uid().to_string(),
            crm_ns: built.crm_ns().to_string(),
            crm_name: built.crm_name().to_string(),
            crm_ver: built.crm_ver().to_string(),
            abi_hash: built.abi_hash().to_string(),
            signature_hash: built.signature_hash().to_string(),
        };
        let reservation = server.reserve_route(built).await.unwrap();
        server.commit_reserved_route(reservation).await.unwrap();
        let (mut peer, stream) = isolated_pair().await;
        let handler = tokio::spawn(handle_connection(server.clone(), stream));
        let ack = super::tests::handshake_test_client_with(&mut peer, capabilities, "").await;
        assert_eq!(ack.capability_flags & capabilities, capabilities);
        Self {
            server,
            peer,
            handler,
            identity,
        }
    }

    async fn open(&mut self, id: u64, input: &[u8]) {
        self.peer
            .write_all(
                &rpc::encode_open_frame(
                    id,
                    &self.identity,
                    1,
                    ValuePart::whole(ValueStorage::Inline(input)),
                )
                .unwrap(),
            )
            .await
            .unwrap();
    }

    async fn stop(self) {
        drop(self.peer);
        self.handler.await.unwrap();
    }
}

#[test]
fn stream_registration_uses_descriptor_access_for_read_parallel() {
    let descriptor = ValidatedContractDescriptor::from_json(STREAM_DESCRIPTOR).unwrap();
    let server = Server::new_with_endpoint(
        c2_config::LocalEndpointContext::default_for_platform()
            .unwrap()
            .endpoint(&format!(
                "ipc://stream-access-{}",
                uuid::Uuid::new_v4().simple()
            ))
            .unwrap(),
        ServerIpcConfig::default(),
    )
    .unwrap();
    let make_spec = || RouteBuildSpec {
        name: "portable".into(),
        crm_ns: descriptor.crm_namespace().into(),
        crm_name: descriptor.crm_name().into(),
        crm_ver: descriptor.crm_version().into(),
        abi_hash: descriptor.abi_hash().into(),
        signature_hash: descriptor.signature_hash().into(),
        method_names: descriptor
            .methods()
            .iter()
            .map(|method| method.name().into())
            .collect(),
        access_map: HashMap::new(),
        concurrency_mode: crate::scheduler::ConcurrencyMode::ReadParallel,
        limits: SchedulerLimits::default(),
    };
    let mut wrong = make_spec();
    wrong
        .access_map
        .insert(1, crate::scheduler::AccessLevel::Read);
    assert!(
        matches!(server.build_stream_route(wrong, Arc::new(CounterCallback(Arc::new(Observed::default()))), &descriptor),
        Err(ServerError::Protocol(message)) if message.contains("access map"))
    );
    let built = server
        .build_stream_route(
            make_spec(),
            Arc::new(CounterCallback(Arc::new(Observed::default()))),
            &descriptor,
        )
        .unwrap();
    assert_eq!(
        built.access_map_snapshot().get(&0),
        Some(&crate::scheduler::AccessLevel::Read)
    );
    assert_eq!(
        built.access_map_snapshot().get(&1),
        Some(&crate::scheduler::AccessLevel::Write)
    );
}

#[tokio::test]
async fn stream_items_require_storage_credit_and_terminal_counts_complete_items() {
    let mut harness =
        Harness::start(Arc::new(Observed::default()), ServerIpcConfig::default(), 0).await;

    for (id, count) in [(1, 0_u8), (2, 1), (3, 3)] {
        harness.open(id, &[count]).await;
        for index in 0..count {
            assert!(matches!(
                read_record(&mut harness.peer, CallMode::ServerStream).await,
                SeenRecord::Begin(4)
            ));
            harness
                .peer
                .write_all(&rpc::encode_credit_frame(id, 4, 0).unwrap())
                .await
                .unwrap();
            match read_record(&mut harness.peer, CallMode::ServerStream).await {
                SeenRecord::Data(data) => assert_eq!(data, [index; 4]),
                other => panic!("expected ITEM_DATA, got {other:?}"),
            }
            harness
                .peer
                .write_all(&rpc::encode_credit_frame(id, 0, 1).unwrap())
                .await
                .unwrap();
        }
        match read_record(&mut harness.peer, CallMode::ServerStream).await {
            SeenRecord::End(CallStatus::Success, Some(actual), None) => {
                assert_eq!(actual, count as u64)
            }
            other => panic!("expected terminal, got {other:?}"),
        }
    }
    harness
        .peer
        .write_all(&rpc::encode_cancel_frame(3).unwrap())
        .await
        .unwrap();
    harness
        .peer
        .write_all(
            &rpc::encode_open_frame(
                4,
                &harness.identity,
                0,
                ValuePart::whole(ValueStorage::Inline(&[])),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::Unary).await,
        SeenRecord::End(CallStatus::Success, None, None)
    ));
    harness.stop().await;
}

#[tokio::test]
async fn paused_stream_keeps_call_slot_but_releases_worker_for_unary() {
    let observed = Arc::new(Observed::default());
    let mut harness = Harness::start(
        observed.clone(),
        ServerIpcConfig {
            max_execution_workers: 1,
            ..ServerIpcConfig::default()
        },
        0,
    )
    .await;
    harness.open(1, &[2]).await;
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Begin(4)
    ));
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(1, 4, 0).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Data(_)
    ));
    let snapshot = harness
        .server
        .route_scheduler_snapshot("portable")
        .await
        .unwrap();
    assert_eq!((snapshot.pending, snapshot.active_workers), (1, 0));
    harness
        .peer
        .write_all(
            &rpc::encode_open_frame(
                2,
                &harness.identity,
                0,
                ValuePart::whole(ValueStorage::Inline(&[])),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::Unary).await,
        SeenRecord::End(CallStatus::Success, None, None)
    ));
    harness
        .peer
        .write_all(&rpc::encode_cancel_frame(1).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::End(CallStatus::Failed, Some(1), Some(ErrorCode::CallCancelled))
    ));
    assert_eq!(observed.finished.load(Ordering::SeqCst), 1);
    harness.stop().await;
}

#[tokio::test]
async fn registered_stream_continues_under_original_route_after_unregister_starts() {
    let mut harness =
        Harness::start(Arc::new(Observed::default()), ServerIpcConfig::default(), 0).await;
    harness.open(1, &[2]).await;
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Begin(4)
    ));
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(1, 4, 0).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Data(_)
    ));
    let server = harness.server.clone();
    let unregister = tokio::spawn(async move { server.unregister_route("portable").await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !harness
            .server
            .route_scheduler_snapshot("portable")
            .await
            .unwrap()
            .closed
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(1, 0, 1).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Begin(4)
    ));
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(1, 4, 0).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Data(_)
    ));
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(1, 0, 1).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::End(CallStatus::Success, Some(2), None)
    ));
    assert!(unregister.await.unwrap());
    harness.stop().await;
}

#[tokio::test]
async fn active_stream_continues_after_host_business_admission_closes() {
    let mut harness =
        Harness::start(Arc::new(Observed::default()), ServerIpcConfig::default(), 0).await;
    harness.open(1, &[2]).await;
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Begin(4)
    ));
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(1, 4, 0).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Data(_)
    ));
    assert_eq!(
        harness.server.close_business_admission("maintenance").await,
        1
    );
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(1, 0, 1).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Begin(4)
    ));
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(1, 4, 0).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Data(_)
    ));
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(1, 0, 1).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::End(CallStatus::Success, Some(2), None)
    ));
    harness.stop().await;
}

#[tokio::test]
async fn host_close_rejects_stream_queued_before_first_resource_entry() {
    let observed = Arc::new(Observed::default());
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *observed.unary_entered.lock().unwrap() = Some(entered_tx);
    *observed.release_unary.lock().unwrap() = Some(release_rx);
    let mut harness = Harness::start(observed.clone(), ServerIpcConfig::default(), 0).await;
    harness
        .peer
        .write_all(
            &rpc::encode_open_frame(
                1,
                &harness.identity,
                0,
                ValuePart::whole(ValueStorage::Inline(&[])),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    entered_rx.await.unwrap();
    harness.open(2, &[1]).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while harness
            .server
            .route_scheduler_snapshot("portable")
            .await
            .unwrap()
            .pending
            != 2
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        harness.server.close_business_admission("maintenance").await,
        1
    );
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::End(
            CallStatus::NotExecuted,
            None,
            Some(ErrorCode::ResourceClosed)
        )
    ));
    assert_eq!(observed.finished.load(Ordering::SeqCst), 0);
    release_tx.send(()).unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::Unary).await,
        SeenRecord::End(CallStatus::Success, None, None)
    ));
    harness.stop().await;
}

#[tokio::test]
async fn cancellation_before_resource_entry_is_not_executed() {
    let observed = Arc::new(Observed::default());
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *observed.unary_entered.lock().unwrap() = Some(entered_tx);
    *observed.release_unary.lock().unwrap() = Some(release_rx);
    let mut harness = Harness::start(
        observed.clone(),
        ServerIpcConfig {
            max_execution_workers: 1,
            ..ServerIpcConfig::default()
        },
        0,
    )
    .await;
    harness
        .peer
        .write_all(
            &rpc::encode_open_frame(
                1,
                &harness.identity,
                0,
                ValuePart::whole(ValueStorage::Inline(&[])),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    entered_rx.await.unwrap();
    harness.open(2, &[1]).await;
    harness
        .peer
        .write_all(&rpc::encode_cancel_frame(2).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::End(
            CallStatus::NotExecuted,
            None,
            Some(ErrorCode::CallCancelled)
        )
    ));
    assert_eq!(observed.finished.load(Ordering::SeqCst), 0);
    release_tx.send(()).unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::Unary).await,
        SeenRecord::End(CallStatus::Success, None, None)
    ));
    harness.stop().await;
}

#[tokio::test]
async fn cancellation_during_incomplete_input_refuses_without_waiting_for_continuation() {
    let observed = Arc::new(Observed::default());
    let mut harness = Harness::start(observed.clone(), ServerIpcConfig::default(), 0).await;
    let first = rpc::encode_open_frame(
        1,
        &harness.identity,
        1,
        ValuePart::first(2, ValueStorage::Inline(&[1])),
    )
    .unwrap();
    harness.peer.write_all(&first).await.unwrap();
    harness
        .peer
        .write_all(&rpc::encode_cancel_frame(1).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::End(
            CallStatus::NotExecuted,
            None,
            Some(ErrorCode::CallCancelled)
        )
    ));
    assert_eq!(observed.finished.load(Ordering::SeqCst), 0);
    let late =
        rpc::encode_value_continuation_frame(1, RpcRecordKind::Open, ValueStorage::Inline(&[0]))
            .unwrap();
    harness.peer.write_all(&late).await.unwrap();
    harness
        .peer
        .write_all(
            &rpc::encode_open_frame(
                2,
                &harness.identity,
                0,
                ValuePart::whole(ValueStorage::Inline(&[])),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::Unary).await,
        SeenRecord::End(CallStatus::Success, None, None)
    ));
    harness.stop().await;
}

#[tokio::test]
async fn chunked_open_uses_original_stream_admission() {
    let mut harness =
        Harness::start(Arc::new(Observed::default()), ServerIpcConfig::default(), 0).await;
    harness
        .peer
        .write_all(
            &rpc::encode_open_frame(
                1,
                &harness.identity,
                1,
                ValuePart::first(2, ValueStorage::Inline(&[1])),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    harness
        .peer
        .write_all(
            &rpc::encode_value_continuation_frame(
                1,
                RpcRecordKind::Open,
                ValueStorage::Inline(&[0]),
            )
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Begin(4)
    ));
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(1, 4, 0).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Data(_)
    ));
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(1, 0, 1).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::End(CallStatus::Success, Some(1), None)
    ));
    harness.stop().await;
}

async fn cancellation_during_step(failure: u8, expected: ErrorCode) {
    let observed = Arc::new(Observed::default());
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *observed.step_entered.lock().unwrap() = Some(entered_tx);
    *observed.release_step.lock().unwrap() = Some(release_rx);
    let mut harness = Harness::start(observed.clone(), ServerIpcConfig::default(), 0).await;
    harness.open(1, &[2, failure]).await;
    entered_rx.await.unwrap();
    harness
        .peer
        .write_all(&rpc::encode_cancel_frame(1).unwrap())
        .await
        .unwrap();
    harness
        .peer
        .write_all(&rpc::encode_cancel_frame(1).unwrap())
        .await
        .unwrap();
    assert_eq!(observed.finished.load(Ordering::SeqCst), 0);
    let conn = harness
        .server
        .active_connection(harness.server.active_connection_ids()[0])
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while !conn
            .core
            .stream_state(1)
            .is_some_and(|state| state.cancelled)
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    release_tx.send(()).unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::End(CallStatus::Failed, Some(0), Some(actual)) if actual == expected
    ));
    assert_eq!(observed.finished.load(Ordering::SeqCst), 1);
    harness.stop().await;
}

#[tokio::test]
async fn cancellation_during_step_runs_cleanup_once() {
    cancellation_during_step(0, ErrorCode::CallCancelled).await;
}

#[tokio::test]
async fn cancellation_does_not_hide_the_running_steps_real_error() {
    cancellation_during_step(3, ErrorCode::ResourceFunctionExecuting).await;
}

#[tokio::test]
async fn resource_and_cleanup_errors_follow_complete_items() {
    let observed = Arc::new(Observed::default());
    let mut harness = Harness::start(observed.clone(), ServerIpcConfig::default(), 0).await;
    for (id, failure) in [(1, 1_u8), (2, 2_u8)] {
        harness.open(id, &[1, failure]).await;
        assert!(matches!(
            read_record(&mut harness.peer, CallMode::ServerStream).await,
            SeenRecord::Begin(4)
        ));
        harness
            .peer
            .write_all(&rpc::encode_credit_frame(id, 4, 0).unwrap())
            .await
            .unwrap();
        assert!(matches!(
            read_record(&mut harness.peer, CallMode::ServerStream).await,
            SeenRecord::Data(_)
        ));
        harness
            .peer
            .write_all(&rpc::encode_credit_frame(id, 0, 1).unwrap())
            .await
            .unwrap();
        assert!(matches!(
            read_record(&mut harness.peer, CallMode::ServerStream).await,
            SeenRecord::End(
                CallStatus::Failed,
                Some(1),
                Some(ErrorCode::ResourceFunctionExecuting)
            )
        ));
    }
    assert_eq!(observed.finished.load(Ordering::SeqCst), 2);
    harness.stop().await;
}

#[tokio::test]
async fn cleanup_error_takes_priority_over_cancellation() {
    let observed = Arc::new(Observed::default());
    let mut harness = Harness::start(observed.clone(), ServerIpcConfig::default(), 0).await;
    harness.open(1, &[2, 2]).await;
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Begin(4)
    ));
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(1, 4, 0).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Data(_)
    ));
    harness
        .peer
        .write_all(&rpc::encode_cancel_frame(1).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::End(
            CallStatus::Failed,
            Some(1),
            Some(ErrorCode::ResourceFunctionExecuting)
        )
    ));
    assert_eq!(observed.finished.load(Ordering::SeqCst), 1);
    harness.stop().await;
}

#[tokio::test]
async fn stream_item_can_use_real_shm_carrier() {
    let mut harness = Harness::start(
        Arc::new(Observed::default()),
        ServerIpcConfig {
            shm_threshold: 1,
            ..ServerIpcConfig::default()
        },
        CAP_SHM_RESPONSE,
    )
    .await;
    harness.open(1, &[1]).await;
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Begin(4)
    ));
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(1, 4, 0).unwrap())
        .await
        .unwrap();
    match read_record(&mut harness.peer, CallMode::ServerStream).await {
        SeenRecord::ShmData(descriptor) => assert_eq!(descriptor.data_size, 4),
        other => panic!("expected SHM ITEM_DATA, got {other:?}"),
    }
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(1, 0, 1).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::End(CallStatus::Success, Some(1), None)
    ));
    harness.stop().await;
}

#[tokio::test]
async fn own_transport_copy_capacity_error_is_failed_717_after_resource_entry() {
    let mut harness = Harness::start_with_budget(
        Arc::new(Observed::default()),
        ServerIpcConfig::default(),
        0,
        Some(3),
    )
    .await;
    harness.open(1, &[1]).await;
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::End(
            CallStatus::Failed,
            Some(0),
            Some(ErrorCode::CallCapacityExceeded)
        )
    ));
    harness.stop().await;
}

#[tokio::test]
async fn inline_item_can_use_the_entire_host_byte_budget() {
    let mut harness = Harness::start_with_budget(
        Arc::new(Observed::default()),
        ServerIpcConfig::default(),
        0,
        Some(4),
    )
    .await;
    harness.open(1, &[1]).await;
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Begin(4)
    ));
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(1, 4, 0).unwrap())
        .await
        .unwrap();
    assert!(
        matches!(read_record(&mut harness.peer, CallMode::ServerStream).await, SeenRecord::Data(data) if data == [0; 4])
    );
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(1, 0, 1).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::End(CallStatus::Success, Some(1), None)
    ));
    harness.stop().await;
}

#[tokio::test]
async fn inline_vec_capacity_is_charged_and_refunded_after_item_write() {
    let mut harness = Harness::start_with_budget(
        Arc::new(Observed::default()),
        ServerIpcConfig::default(),
        0,
        Some(64),
    )
    .await;
    harness.open(1, &[1, 0, 1]).await;
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Begin(4)
    ));
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(1, 4, 0).unwrap())
        .await
        .unwrap();
    assert!(
        matches!(read_record(&mut harness.peer, CallMode::ServerStream).await, SeenRecord::Data(data) if data == [0; 4])
    );
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(1, 0, 1).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::End(CallStatus::Success, Some(1), None)
    ));
    let budget = harness.server.stream_payload_budget.snapshot();
    assert_eq!(budget.peak_bytes, 64);
    assert_eq!(budget.used_bytes, 0);
    harness.stop().await;
}

#[tokio::test]
async fn deactivation_does_not_abandon_admitted_stream() {
    let mut harness =
        Harness::start(Arc::new(Observed::default()), ServerIpcConfig::default(), 0).await;
    harness.open(1, &[1]).await;
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Begin(4)
    ));
    let conn_id = harness.server.active_connection_ids()[0];
    let conn = harness.server.active_connection(conn_id).unwrap();
    conn.end(ConnectionEndReason::LocalClose, "test_deactivate_stream");
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(1, 4, 0).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Data(_)
    ));
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(1, 0, 1).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::End(CallStatus::Success, Some(1), None)
    ));
    drop(conn);
    harness.stop().await;
}

#[tokio::test]
async fn item_write_timeout_preserves_terminal_while_another_stream_keeps_connection_open() {
    const SIZE: usize = 8 * 1024 * 1024;
    let observed = Arc::new(Observed::default());
    observed.item_bytes.store(SIZE, Ordering::SeqCst);
    let mut config = ServerIpcConfig::default();
    config.base.write_timeout_secs = 0.01;
    config.base.chunk_size = SIZE as u64;
    config.max_frame_size = SIZE as u64 + 1024;
    let mut harness = Harness::start(observed.clone(), config, 0).await;
    harness.open(1, &[1]).await;
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Begin(bytes) if bytes == SIZE as u64
    ));
    harness.open(2, &[1]).await;
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Begin(bytes) if bytes == SIZE as u64
    ));
    let conn = harness
        .server
        .active_connection(harness.server.active_connection_ids()[0])
        .unwrap();
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(1, SIZE as u64, 0).unwrap())
        .await
        .unwrap();
    // No response reads until the real OS write has exceeded its configured
    // deadline. The other stream remains active without storage credit.
    tokio::time::timeout(Duration::from_secs(5), async {
        while !conn.is_deactivated() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!conn.is_work_cancelled());
    assert!(conn.core.stream_state(2).is_some());
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::Data(data) if data == vec![0; SIZE]
    ));
    harness
        .peer
        .write_all(&rpc::encode_credit_frame(1, 0, 1).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::End(CallStatus::Success, Some(1), None)
    ));
    assert_eq!(observed.finished.load(Ordering::SeqCst), 1);
    assert!(conn.core.stream_state(2).is_some());
    harness
        .peer
        .write_all(&rpc::encode_cancel_frame(2).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        read_record(&mut harness.peer, CallMode::ServerStream).await,
        SeenRecord::End(CallStatus::Failed, Some(0), Some(ErrorCode::CallCancelled))
    ));
    assert_eq!(observed.finished.load(Ordering::SeqCst), 2);
    harness.stop().await;
}

#[tokio::test]
async fn peer_end_waits_for_current_step_then_cleans_resource() {
    let observed = Arc::new(Observed::default());
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *observed.step_entered.lock().unwrap() = Some(entered_tx);
    *observed.release_step.lock().unwrap() = Some(release_rx);
    let mut harness = Harness::start(observed.clone(), ServerIpcConfig::default(), 0).await;
    harness.open(1, &[2]).await;
    entered_rx.await.unwrap();
    drop(harness.peer);
    assert_eq!(observed.finished.load(Ordering::SeqCst), 0);
    release_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), harness.handler)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(observed.finished.load(Ordering::SeqCst), 1);
}
