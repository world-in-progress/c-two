use super::*;
use crate::tests::download_slice;
use crate::tests::memory_budget;
use c2_config::LocalEndpointContext;
use c2_contract::ValidatedContractDescriptor;
use c2_server::{
    ConcurrencyMode, CrmCallback, CrmError, CrmStream, CrmStreamCallback, CrmStreamItem,
    ResponseMeta, ResponsePool, RouteBuildSpec, SchedulerLimits, Server, ServerIpcConfig,
};
use futures_util::FutureExt;
use std::time::Duration;

const ITEM_SIZE: usize = 32 * 1024;

const WAIT: Duration = Duration::from_secs(5);

struct Callback;
struct Items(bool);
impl CrmCallback for Callback {
    fn invoke(
        &self,
        _: &str,
        _: u16,
        _: Vec<u8>,
        _: Arc<ResponsePool>,
    ) -> Result<ResponseMeta, CrmError> {
        Ok(ResponseMeta::Inline(b"pong".to_vec()))
    }
}
impl CrmStreamCallback for Callback {
    fn open_stream(
        &self,
        _: &str,
        _: u16,
        _: Vec<u8>,
        _: Arc<ResponsePool>,
    ) -> Result<Box<dyn CrmStream>, CrmError> {
        Ok(Box::new(Items(false)))
    }
}
impl CrmStream for Items {
    fn step(&mut self) -> Result<Option<Box<dyn CrmStreamItem>>, CrmError> {
        if std::mem::replace(&mut self.0, true) {
            Ok(None)
        } else {
            Ok(Some(Box::new(vec![7_u8; ITEM_SIZE])))
        }
    }
    fn finish(self: Box<Self>) -> Result<(), CrmError> {
        Ok(())
    }
}

struct Harness {
    _root: tempfile::TempDir,
    server: Arc<Server>,
    runner: tokio::task::JoinHandle<()>,
    client: IpcClient,
    binding: RouteBinding,
    topics: Arc<c2_server::topic::RouteTopics>,
}
impl Harness {
    async fn start() -> Self {
        let root = tempfile::Builder::new()
            .prefix("c2fcopy")
            .tempdir()
            .unwrap();
        #[cfg(unix)]
        let context = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
            LocalEndpointContext::with_unix_root(root.path()).unwrap()
        };
        #[cfg(windows)]
        let context = LocalEndpointContext::default_for_platform().unwrap();
        let endpoint = context
            .endpoint(&format!(
                "ipc://fcopy-{}",
                CLIENT_CONN_COUNTER.fetch_add(1, Ordering::Relaxed)
            ))
            .unwrap();
        let descriptor = ValidatedContractDescriptor::from_json(include_bytes!(
            "../../../foundation/c2-contract/tests/fixtures/server-stream.contract.json"
        ))
        .unwrap();
        let expected = c2_contract::ExpectedRouteContract {
            route_name: "portable".into(),
            crm_ns: descriptor.crm_namespace().into(),
            crm_name: descriptor.crm_name().into(),
            crm_ver: descriptor.crm_version().into(),
            abi_hash: descriptor.abi_hash().into(),
            signature_hash: descriptor.signature_hash().into(),
        };
        let server = Arc::new(
            Server::new_with_endpoint(
                endpoint.clone(),
                ServerIpcConfig {
                    ..Default::default()
                },
            )
            .unwrap(),
        );
        let topics = server.create_route_topics(
            expected.clone(),
            &std::collections::BTreeMap::from([(
                "state".into(),
                c2_server::topic::TopicConfig { capacity: 2 },
            )]),
        );
        let built = server
            .build_stream_route(
                RouteBuildSpec {
                    name: expected.route_name.clone(),
                    crm_ns: expected.crm_ns.clone(),
                    crm_name: expected.crm_name.clone(),
                    crm_ver: expected.crm_ver.clone(),
                    abi_hash: expected.abi_hash.clone(),
                    signature_hash: expected.signature_hash.clone(),
                    method_names: vec!["ping".into(), "echo".into()],
                    access_map: HashMap::new(),
                    concurrency_mode: ConcurrencyMode::Parallel,
                    limits: SchedulerLimits::default(),
                },
                Arc::new(Callback),
                &descriptor,
            )
            .unwrap()
            .with_topics(topics.clone());
        let reservation = server.reserve_route(built).await.unwrap();
        server.commit_reserved_route(reservation).await.unwrap();
        server.begin_start_attempt().unwrap();
        let runner = tokio::spawn({
            let server = server.clone();
            async move { server.run().await.unwrap() }
        });
        server.wait_until_ready(WAIT).await.unwrap();
        let mut client = IpcClient::with_endpoint(endpoint).unwrap();
        client.connect().await.unwrap();
        let binding = client.acquire_route(&expected).await.unwrap();
        Self {
            _root: root,
            server,
            runner,
            client,
            binding,
            topics,
        }
    }
    async fn stream(&self) -> IpcForwardStream {
        let budget = memory_budget(1);
        let mut stream = self
            .client
            .forward_stream_bound_sized_phased(
                &self.binding,
                "echo",
                0,
                futures_util::stream::empty::<Result<Vec<u8>, std::io::Error>>(),
                budget.try_reserve_memory(1).unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            stream.next_record().await.unwrap(),
            ForwardRecord::ItemBegin { total_bytes } if total_bytes == ITEM_SIZE as u64
        ));
        stream
    }
    async fn stop(self) {
        self.client.close_shared().await;
        self.server.shutdown_and_wait(WAIT).await.unwrap();
        self.runner.await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forwarded_topic_uses_small_windows_and_reports_real_terminal() {
    let harness = Harness::start().await;
    let mut topic = tokio::time::timeout(
        WAIT,
        harness.client.forward_subscribe_bound(
            &harness.binding,
            "state",
            rpc::WireSubscribeOptions::default(),
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(topic.start_seq(), 1);
    assert_eq!(harness.topics.active_subscribers(), 1);
    fn assert_send<T: Send>(_: T) {}
    assert_send(topic.sync_head_seq());
    let publisher = harness.topics.publisher("state").unwrap();
    assert_eq!(
        tokio::time::timeout(WAIT, topic.sync_head_seq())
            .await
            .unwrap()
            .unwrap(),
        0
    );
    let payload = vec![37; ITEM_SIZE];
    assert_eq!(publisher.publish(payload.clone()).unwrap(), 1);
    let first = tokio::time::timeout(WAIT, topic.next_record())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(first, ForwardTopicRecord::ItemBegin { seq: 1, total_bytes } if total_bytes == ITEM_SIZE as u64)
    );
    // The source can publish again while this receiver is still on seq 1.
    // SYNC must return the source head, not the last observed IPC item.
    assert_eq!(publisher.publish(b"after release".to_vec()).unwrap(), 2);
    assert_eq!(
        tokio::time::timeout(WAIT, topic.sync_head_seq())
            .await
            .unwrap()
            .unwrap(),
        2
    );
    // The topic is larger than the relay's entire receive budget. Each window
    // is released before reserving the next one.
    let budget = memory_budget(1024);
    let mut result = Vec::new();
    while result.len() < payload.len() {
        let chunk =
            tokio::time::timeout(WAIT, topic.read_chunk(download_slice(&budget, 1024), 1024))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        result.extend_from_slice(chunk.as_ref());
        drop(chunk);
        if result.len() == 1024 {
            assert!(
                topic.stream.call.state.lock().carrier.is_some(),
                "expected an active SHM carrier"
            );
        }
    }
    assert_eq!(result, payload);
    assert!(topic.stream.call.state.lock().carrier.is_none());
    let second = tokio::time::timeout(WAIT, topic.next_record())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        second,
        ForwardTopicRecord::ItemBegin {
            seq: 2,
            total_bytes: 13
        }
    ));
    let chunk = tokio::time::timeout(WAIT, topic.read_chunk(download_slice(&budget, 13), 13))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(chunk.as_ref(), b"after release");
    drop(chunk);
    topic
        .cancellation()
        .cancel(rpc::CancelCause::Consumer)
        .unwrap();
    let terminal = tokio::time::timeout(WAIT, topic.completion())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(terminal.status, CallStatus::Failed);
    assert_eq!(terminal.complete_items, 2);
    assert_eq!(terminal.error.unwrap().code, ErrorCode::CallCancelled);
    tokio::time::timeout(WAIT, async {
        while harness.topics.active_subscribers() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    drop(topic);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forwarded_unknown_topic_preserves_pre_dispatch_refusal() {
    let harness = Harness::start().await;
    let error = match harness
        .client
        .forward_subscribe_bound(
            &harness.binding,
            "unknown",
            rpc::WireSubscribeOptions::default(),
        )
        .await
    {
        Ok(_) => panic!("unknown topic must fail"),
        Err(error) => error,
    };
    assert!(error.is_retry_safe());
    assert!(
        matches!(error.source_error(), IpcError::CallRejected(e) if e.code == ErrorCode::ProtocolViolation)
    );
    harness.stop().await;
}

#[test]
fn success_before_ready_is_a_protocol_error_without_panicking() {
    let terminal = stream::StreamTerminal {
        status: CallStatus::Success,
        complete_items: 0,
        error: None,
    };
    let error = topic_opening_terminal_error(&terminal);
    assert_eq!(error.phase(), crate::TransportPhase::DispatchUncertain);
    assert!(
        matches!(error.source_error(), IpcError::Protocol(message) if message.contains("before READY"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sync_waiters_observe_source_head_and_real_terminal() {
    let harness = Harness::start().await;
    let mut topic = harness
        .client
        .forward_subscribe_bound(
            &harness.binding,
            "state",
            rpc::WireSubscribeOptions::default(),
        )
        .await
        .unwrap();
    let sync = topic.synchronizer();
    let (a, b, c) = tokio::join!(
        sync.sync_head_seq(),
        sync.sync_head_seq(),
        sync.sync_head_seq()
    );
    assert_eq!((a.unwrap(), b.unwrap(), c.unwrap()), (0, 0, 0));
    assert_eq!(
        harness
            .topics
            .publisher("state")
            .unwrap()
            .publish(b"first".to_vec())
            .unwrap(),
        1
    );
    assert_eq!(sync.sync_head_seq().await.unwrap(), 1);
    assert!(matches!(
        topic.next_record().await.unwrap(),
        ForwardTopicRecord::ItemBegin {
            seq: 1,
            total_bytes: 5
        }
    ));
    harness
        .topics
        .close(c2_server::topic::TopicCloseCause::RouteUnregistered);
    let waiting = tokio::spawn(async move { sync.sync_head_seq().await });
    let budget = memory_budget(5);
    let chunk = topic
        .read_chunk(download_slice(&budget, 5), 5)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(chunk.as_ref(), b"first");
    drop(chunk);
    let error = tokio::time::timeout(WAIT, waiting)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    let c2 = match error.into_source() {
        IpcError::CrmError(bytes) => C2Error::from_wire_bytes(&bytes).unwrap().unwrap(),
        other => panic!("expected subscription terminal, got {other:?}"),
    };
    assert_eq!(c2.code, ErrorCode::CallCancelled);
    assert_eq!(
        c2.details.get("cause").map(String::as_str),
        Some("route_unregistered")
    );
    drop(topic);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn completion_observes_published_terminal_under_state_lock_contention() {
    let harness = Harness::start().await;
    let response = harness.stream().await;
    response
        .cancellation()
        .cancel(rpc::CancelCause::Consumer)
        .unwrap();
    tokio::time::timeout(WAIT, async {
        loop {
            let changed = response.call.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if response.call.state.lock().ended {
                break;
            }
            changed.await;
        }
    })
    .await
    .unwrap();

    // The real CALL_END has been published. Hold the same short lock that the
    // writer takes for has_control/take_control, without issuing another wake
    // when it is released. The barrier starts observation inside this interval.
    let state = response.call.state.lock();
    assert!(state.terminal.is_some());
    let entered = std::sync::Barrier::new(2);
    let (parked_tx, parked_rx) = std::sync::mpsc::channel();
    let runtime = tokio::runtime::Handle::current();
    let result = std::thread::scope(|scope| {
        let observer = scope.spawn(|| {
            runtime.block_on(async {
                let completion = response.completion();
                tokio::pin!(completion);
                entered.wait();
                tokio::time::timeout(
                    WAIT,
                    std::future::poll_fn(|cx| {
                        let result = completion.as_mut().poll(cx);
                        if result.is_pending() {
                            let _ = parked_tx.send(());
                        }
                        result
                    }),
                )
                .await
            })
        });
        entered.wait();
        // A try_lock waiter parks and releases this gate immediately. A lock
        // waiter blocks inside its poll, so bound the rendezvous before letting
        // it acquire the lock. No sleep schedules the failing interleaving.
        let _ = parked_rx.recv_timeout(Duration::from_millis(100));
        drop(state);
        observer.join().unwrap()
    });
    let terminal = result
        .expect("completion lost the terminal's last wake")
        .unwrap();
    assert_eq!(terminal.status, CallStatus::Failed);
    assert_eq!(terminal.error.unwrap().code, ErrorCode::CallCancelled);
    drop(response);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn consumer_cancel_between_shm_ready_and_copy_preparation_is_a_call_error() {
    let harness = Harness::start().await;
    let mut response = harness.stream().await;
    let call = response.call.clone();
    let budget = memory_budget(8);
    let mut read = Box::pin(response.read_chunk(download_slice(&budget, 8), 8));
    assert!(read.as_mut().now_or_never().is_none());
    tokio::time::timeout(WAIT, async {
        loop {
            let changed = call.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if call.state.lock().carrier.is_some() {
                break;
            }
            changed.await;
        }
    })
    .await
    .unwrap();
    call.cancel(rpc::CancelCause::Consumer).unwrap();
    assert!(read.await.is_err());
    assert_eq!(budget.snapshot().memory.used_bytes, 0);
    tokio::time::timeout(WAIT, response.completion())
        .await
        .unwrap()
        .unwrap();
    drop(response);
    harness.stop().await;
}

async fn terminal_during_owned_copy(consumer_first: bool) {
    let harness = Harness::start().await;
    let mut response = harness.stream().await;
    let budget = memory_budget(8);
    let first = response
        .read_chunk(download_slice(&budget, 8), 8)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.as_ref(), &[7; 8]);
    drop(first);
    let call = response.call.clone();
    let step = {
        let mut state = call.state.lock();
        let step = call
            .prepare_copy_locked(&mut state, download_slice(&budget, 8), 8)
            .unwrap();
        assert!(state.copying);
        assert!(state.carrier.is_none());
        step
    };
    // Hold the real move-only copy owner at the production ownership seam.
    // No injected gate or sleep is needed to keep this interval open.
    response
        .cancellation()
        .cancel(if consumer_first {
            rpc::CancelCause::Consumer
        } else {
            rpc::CancelCause::Lifecycle
        })
        .unwrap();
    tokio::time::timeout(
        WAIT,
        response.cancellation().wait_deferred_terminal_for_test(),
    )
    .await
    .unwrap();
    assert!(call.state.try_lock().is_some());
    assert!(response.completion().now_or_never().is_none());
    let id = call.state.lock().id.unwrap();
    assert!(call.conn.core.stream_state(id).is_some());
    let unary = tokio::time::timeout(
        WAIT,
        harness.client.call_bound(&harness.binding, "ping", b""),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(unary.into_owned_bytes().unwrap(), b"pong");
    // Cancellation after the reader's CALL_END must also retain this owner.
    response
        .cancellation()
        .cancel(rpc::CancelCause::Consumer)
        .unwrap();
    assert!(response.completion().now_or_never().is_none());
    let mut last_read = [0; 1];
    step.carrier
        .copy_range_into(step.offset, &mut last_read)
        .unwrap();
    assert_eq!(last_read, [7]);
    assert!(call.copy_carrier(step).is_err());
    assert_eq!(budget.snapshot().memory.used_bytes, 0);
    let terminal = response.completion().await.unwrap();
    assert_eq!(terminal.status, CallStatus::Failed);
    assert_eq!(terminal.complete_items, 1);
    assert_eq!(terminal.error.unwrap().code, ErrorCode::CallCancelled);
    assert!(call.conn.core.stream_state(id).is_none());
    drop(response);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn consumer_cancel_and_call_end_pass_owned_copy_without_finishing_its_read() {
    terminal_during_owned_copy(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn consumer_cancel_after_deferred_call_end_keeps_owned_copy_live() {
    terminal_during_owned_copy(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn physical_failure_waits_for_owned_shm_copy_to_end() {
    let harness = Harness::start().await;
    let mut response = harness.stream().await;
    let budget = memory_budget(8);
    drop(
        response
            .read_chunk(download_slice(&budget, 8), 8)
            .await
            .unwrap()
            .unwrap(),
    );
    let call = response.call.clone();
    let step = call
        .prepare_copy_locked(&mut call.state.lock(), download_slice(&budget, 8), 8)
        .unwrap();
    call.transport_failed(IpcError::Closed);
    assert!(response.completion().now_or_never().is_none());
    assert!(call.state.lock().deferred_failure.is_some());
    assert!(call.copy_carrier(step).is_err());
    assert_eq!(budget.snapshot().memory.used_bytes, 0);
    let error = response.completion().await.unwrap_err();
    assert_eq!(error.phase(), crate::TransportPhase::DispatchUncertain);
    drop(response);
    harness.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn successful_call_end_and_later_failure_wait_for_final_owned_copy() {
    let harness = Harness::start().await;
    let mut response = harness.stream().await;
    let budget = memory_budget((ITEM_SIZE - 8) as u64);
    drop(
        response
            .read_chunk(
                download_slice(&budget, (ITEM_SIZE - 8) as u64),
                ITEM_SIZE - 8,
            )
            .await
            .unwrap()
            .unwrap(),
    );
    let call = response.call.clone();
    let step = {
        let mut state = call.state.lock();
        let step = call
            .prepare_copy_locked(&mut state, download_slice(&budget, 8), 8)
            .unwrap();
        // Admit the next demand while the final range is still owned locally;
        // the peer can then report its real natural end through the reader.
        call.conn
            .core
            .stream_credit(state.id.unwrap(), 0, 1)
            .unwrap();
        state.pending_demand = 1;
        state.demand_outstanding = true;
        step
    };
    call.conn.wake.notify_one();
    tokio::time::timeout(
        WAIT,
        response.cancellation().wait_deferred_terminal_for_test(),
    )
    .await
    .unwrap();
    assert!(response.completion().now_or_never().is_none());
    call.transport_failed(IpcError::Closed);
    assert!(response.completion().now_or_never().is_none());
    let last = call.copy_carrier(step).unwrap();
    assert_eq!(last.as_ref(), &[7; 8]);
    let terminal = response.completion().await.unwrap();
    assert_eq!(terminal.status, CallStatus::Success);
    assert_eq!(terminal.complete_items, 1);
    drop(last);
    assert_eq!(budget.snapshot().memory.used_bytes, 0);
    drop(response);
    harness.stop().await;
}
