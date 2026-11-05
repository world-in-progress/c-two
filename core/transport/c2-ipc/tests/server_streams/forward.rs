use super::*;
use c2_ipc::ForwardRecord;
use c2_wire::rpc::CancelCause;
use futures_util::FutureExt;
use futures_util::StreamExt;
use futures_util::stream;

struct FailingWriteCallback;

struct FailingWriteStream {
    yielded: u8,
}

struct FailingWriteItem;

impl CrmCallback for FailingWriteCallback {
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

impl CrmStreamCallback for FailingWriteCallback {
    fn open_stream(
        &self,
        _: &str,
        _: u16,
        _: Vec<u8>,
        _: Arc<ResponsePool>,
    ) -> Result<Box<dyn CrmStream>, CrmError> {
        Ok(Box::new(FailingWriteStream { yielded: 0 }))
    }
}

impl CrmStream for FailingWriteStream {
    fn step(&mut self) -> Result<Option<Box<dyn CrmStreamItem>>, CrmError> {
        let item: Box<dyn CrmStreamItem> = match self.yielded {
            0 => Box::new(vec![7; 8]),
            1 => Box::new(FailingWriteItem),
            _ => return Ok(None),
        };
        self.yielded += 1;
        Ok(Some(item))
    }

    fn finish(self: Box<Self>) -> Result<(), CrmError> {
        Ok(())
    }
}

impl CrmStreamItem for FailingWriteItem {
    fn len(&self) -> usize {
        8
    }

    fn write_range(&self, _: usize, _: &mut [u8]) -> Result<(), CrmError> {
        Err(CrmError::InternalError("scripted write failure".into()))
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn item_write_range_failure_preserves_failed_call_end() {
    let fixture = Harness::start_with_config(ServerIpcConfig {
        shm_threshold: 1024,
        ..ServerIpcConfig::default()
    })
    .await;
    let descriptor = ValidatedContractDescriptor::from_json(DESCRIPTOR).unwrap();
    let built = fixture
        .server
        .build_stream_route(
            RouteBuildSpec {
                name: "failing-write".into(),
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
            Arc::new(FailingWriteCallback),
            &descriptor,
        )
        .unwrap();
    let reservation = fixture.server.reserve_route(built).await.unwrap();
    fixture
        .server
        .commit_reserved_route(reservation)
        .await
        .unwrap();
    let mut expected = expected_contract();
    expected.route_name = "failing-write".into();
    let binding = fixture.client.acquire_route(&expected).await.unwrap();
    let budget = PayloadBufferBudget::new(9);
    let mut response = fixture
        .client
        .forward_stream_bound_sized_phased(
            &binding,
            "echo",
            1,
            stream::iter([Ok::<_, std::io::Error>(vec![2])]),
            budget.try_reserve(1).unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(WAIT, response.next_record())
            .await
            .unwrap()
            .unwrap(),
        ForwardRecord::ItemBegin { total_bytes: 8 }
    ));
    let first = tokio::time::timeout(WAIT, response.read_chunk(budget.try_reserve(8).unwrap(), 8))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first.as_ref(), &[7; 8]);
    drop(first);
    assert!(matches!(
        tokio::time::timeout(WAIT, response.next_record())
            .await
            .unwrap()
            .unwrap(),
        ForwardRecord::ItemBegin { total_bytes: 8 }
    ));
    let failed_item =
        tokio::time::timeout(WAIT, response.read_chunk(budget.try_reserve(8).unwrap(), 8))
            .await
            .unwrap();
    assert!(failed_item.is_ok(), "CALL_END is the failed item's outcome");
    assert!(
        failed_item.unwrap().is_none(),
        "incomplete item has no chunk"
    );
    let terminal = tokio::time::timeout(WAIT, response.next_record())
        .await
        .unwrap()
        .unwrap();
    let ForwardRecord::CallEnd(terminal) = terminal else {
        panic!("expected failed CALL_END")
    };
    assert_eq!(terminal.status, CallStatus::Failed);
    assert_eq!(terminal.complete_items, 1);
    let error = terminal.error.expect("resource failure must be preserved");
    assert_eq!(error.code, ErrorCode::ResourceFunctionExecuting);
    assert_eq!(error.message, "scripted write failure");
    let completed = response.completion().await.unwrap();
    assert_eq!(completed.complete_items, 1);
    assert_eq!(
        completed.error.unwrap().code,
        ErrorCode::ResourceFunctionExecuting
    );
    assert_eq!(budget.snapshot().used_bytes, 0);
    fixture.unary().await;
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn whole_shm_item_larger_than_copy_window_releases_after_last_chunk() {
    let fixture = Harness::start_with_config(ServerIpcConfig {
        shm_threshold: 1,
        ..ServerIpcConfig::default()
    })
    .await;
    let budget = PayloadBufferBudget::new(9);
    let send = budget.try_reserve(1).unwrap();
    let mut response = fixture
        .client
        .forward_stream_bound_sized_phased(
            &fixture.binding,
            "echo",
            3,
            stream::iter([Ok::<_, std::io::Error>(vec![1, 0, 64])]),
            send,
        )
        .await
        .unwrap();
    let first = tokio::time::timeout(WAIT, response.next_record())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        first,
        ForwardRecord::ItemBegin { total_bytes: 64 }
    ));
    fixture.unary().await; // Shared connection remains readable while B is held by the caller.
    let mut output = Vec::new();
    for _ in 0..8 {
        let chunk =
            tokio::time::timeout(WAIT, response.read_chunk(budget.try_reserve(8).unwrap(), 8))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        output.extend_from_slice(chunk.as_ref());
        drop(chunk);
    }
    assert_eq!(output, vec![0; 64]);
    let end = tokio::time::timeout(WAIT, response.next_record())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        end,
        ForwardRecord::CallEnd(StreamTerminal {
            status: CallStatus::Success,
            complete_items: 1,
            ..
        })
    ));
    assert_eq!(response.completion().await.unwrap().complete_items, 1);
    assert_eq!(budget.snapshot().used_bytes, 0);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn consumer_cancel_mid_item_keeps_shared_unary_alive() {
    let fixture = Harness::start_with_config(ServerIpcConfig {
        shm_threshold: 1,
        ..ServerIpcConfig::default()
    })
    .await;
    let budget = PayloadBufferBudget::new(9);
    let mut response = fixture
        .client
        .forward_stream_bound_sized_phased(
            &fixture.binding,
            "echo",
            3,
            stream::iter([Ok::<_, std::io::Error>(vec![2, 0, 64])]),
            budget.try_reserve(1).unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(WAIT, response.next_record())
            .await
            .unwrap()
            .unwrap(),
        ForwardRecord::ItemBegin { total_bytes: 64 }
    ));
    let part = tokio::time::timeout(WAIT, response.read_chunk(budget.try_reserve(8).unwrap(), 8))
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(part.as_ref(), &[0; 8]);
    response
        .cancellation()
        .cancel(CancelCause::Consumer)
        .unwrap();
    fixture.unary().await;
    let end = tokio::time::timeout(WAIT, response.completion())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(end.status, CallStatus::Failed);
    assert_eq!(end.error.unwrap().code, ErrorCode::CallCancelled);
    drop(part);
    assert_eq!(budget.snapshot().used_bytes, 0);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unary_cursor_reads_a_carrier_in_charged_ranges() {
    let fixture = Harness::start_with_config(ServerIpcConfig {
        shm_threshold: 1,
        ..ServerIpcConfig::default()
    })
    .await;
    let budget = PayloadBufferBudget::new(4);
    let response = fixture
        .client
        .forward_unary_bound_sized_phased(
            &fixture.binding,
            "ping",
            1,
            stream::iter([Ok::<_, std::io::Error>(vec![7])]),
            budget.try_reserve(1).unwrap(),
        )
        .await
        .unwrap();
    let mut cursor = response.into_cursor();
    assert_eq!(cursor.len(), 4);
    let first = cursor
        .read_chunk(budget.try_reserve(2).unwrap(), 2)
        .unwrap()
        .unwrap();
    assert_eq!(first.as_ref(), b"po");
    drop(first);
    let second = cursor
        .read_chunk(budget.try_reserve(2).unwrap(), 2)
        .unwrap()
        .unwrap();
    assert_eq!(second.as_ref(), b"ng");
    drop(second);
    assert_eq!(cursor.remaining(), 0);
    assert_eq!(budget.snapshot().used_bytes, 0);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn inline_item_consumes_each_real_credit_window() {
    let fixture = Harness::start_with_config(ServerIpcConfig {
        shm_threshold: 1024,
        ..ServerIpcConfig::default()
    })
    .await;
    let budget = PayloadBufferBudget::new(9);
    let mut response = fixture
        .client
        .forward_stream_bound_sized_phased(
            &fixture.binding,
            "echo",
            3,
            stream::iter([Ok::<_, std::io::Error>(vec![1, 0, 32])]),
            budget.try_reserve(1).unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(WAIT, response.next_record())
            .await
            .unwrap()
            .unwrap(),
        ForwardRecord::ItemBegin { total_bytes: 32 }
    ));
    let mut output = Vec::new();
    for _ in 0..4 {
        let chunk =
            tokio::time::timeout(WAIT, response.read_chunk(budget.try_reserve(8).unwrap(), 8))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        output.extend_from_slice(chunk.as_ref());
        drop(chunk);
    }
    assert_eq!(output, vec![0; 32]);
    assert!(matches!(
        tokio::time::timeout(WAIT, response.next_record())
            .await
            .unwrap()
            .unwrap(),
        ForwardRecord::CallEnd(StreamTerminal {
            status: CallStatus::Success,
            complete_items: 1,
            ..
        })
    ));
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_cancel_keeps_current_item_storage_and_cleanup_count() {
    let fixture = Harness::start_with_config(ServerIpcConfig {
        shm_threshold: 1,
        ..ServerIpcConfig::default()
    })
    .await;
    let budget = PayloadBufferBudget::new(9);
    let mut response = fixture
        .client
        .forward_stream_bound_sized_phased(
            &fixture.binding,
            "echo",
            3,
            stream::iter([Ok::<_, std::io::Error>(vec![2, 0, 32])]),
            budget.try_reserve(1).unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(WAIT, response.next_record())
            .await
            .unwrap()
            .unwrap(),
        ForwardRecord::ItemBegin { total_bytes: 32 }
    ));
    response
        .cancellation()
        .cancel(CancelCause::Lifecycle)
        .unwrap();
    let mut output = Vec::new();
    for _ in 0..4 {
        let chunk =
            tokio::time::timeout(WAIT, response.read_chunk(budget.try_reserve(8).unwrap(), 8))
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        output.extend_from_slice(chunk.as_ref());
        drop(chunk);
    }
    assert_eq!(output, vec![0; 32]);
    let terminal = tokio::time::timeout(WAIT, response.next_record())
        .await
        .unwrap()
        .unwrap();
    let ForwardRecord::CallEnd(terminal) = terminal else {
        panic!("expected cleanup terminal")
    };
    assert_eq!(terminal.complete_items, 1);
    assert_eq!(terminal.status, CallStatus::Failed);
    assert_eq!(terminal.error.unwrap().code, ErrorCode::CallCancelled);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn partial_upload_failure_cancels_only_its_stream() {
    let fixture = Harness::start(2).await;
    let budget = PayloadBufferBudget::new(1);
    let mut response = fixture
        .client
        .forward_stream_bound_sized_phased(
            &fixture.binding,
            "echo",
            3,
            stream::iter([
                Ok(vec![1]),
                Err(std::io::Error::other("source broke after OPEN")),
            ]),
            budget.try_reserve(1).unwrap(),
        )
        .await
        .unwrap();
    let error = response
        .wait_uploaded()
        .await
        .expect_err("partial upload must fail before response headers");
    assert!(matches!(
        error.phase(),
        c2_ipc::TransportPhase::PreDispatch | c2_ipc::TransportPhase::DispatchUncertain
    ));
    fixture.unary().await;
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn consumer_escalates_lifecycle_cancel_without_blocking_other_calls() {
    let fixture = Harness::start_with_config(ServerIpcConfig {
        shm_threshold: 1024,
        ..ServerIpcConfig::default()
    })
    .await;
    let budget = PayloadBufferBudget::new(9);
    let mut response = fixture
        .client
        .forward_stream_bound_sized_phased(
            &fixture.binding,
            "echo",
            3,
            stream::iter([Ok::<_, std::io::Error>(vec![2, 0, 32])]),
            budget.try_reserve(1).unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(WAIT, response.next_record())
            .await
            .unwrap()
            .unwrap(),
        ForwardRecord::ItemBegin { total_bytes: 32 }
    ));
    let cancellation = response.cancellation();
    cancellation.cancel(CancelCause::Lifecycle).unwrap();
    cancellation.cancel(CancelCause::Consumer).unwrap();
    fixture.unary().await;
    let terminal = tokio::time::timeout(WAIT, response.completion())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(terminal.status, CallStatus::Failed);
    assert_eq!(terminal.error.unwrap().code, ErrorCode::CallCancelled);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn consumer_cancel_settles_call_end_deferred_behind_shm_copy() {
    let fixture = Harness::start_with_config(ServerIpcConfig {
        shm_threshold: 1,
        ..ServerIpcConfig::default()
    })
    .await;
    let budget = PayloadBufferBudget::new(9);
    let mut response = fixture
        .client
        .forward_stream_bound_sized_phased(
            &fixture.binding,
            "echo",
            3,
            stream::iter([Ok::<_, std::io::Error>(vec![1, 0, 64])]),
            budget.try_reserve(1).unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(WAIT, response.next_record())
            .await
            .unwrap()
            .unwrap(),
        ForwardRecord::ItemBegin { total_bytes: 64 }
    ));
    let cancellation = response.cancellation();
    cancellation.cancel(CancelCause::Lifecycle).unwrap();
    let mut read = Box::pin(response.read_chunk(budget.try_reserve(8).unwrap(), 8));
    assert!(read.as_mut().now_or_never().is_none());
    tokio::time::timeout(WAIT, cancellation.wait_deferred_terminal_for_test())
        .await
        .unwrap();
    cancellation.cancel(CancelCause::Consumer).unwrap();
    drop(read);
    let terminal = tokio::time::timeout(WAIT, response.completion())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(terminal.complete_items, 1);
    assert_eq!(terminal.status, CallStatus::Failed);
    assert_eq!(terminal.error.unwrap().code, ErrorCode::CallCancelled);
    assert_eq!(budget.snapshot().used_bytes, 0);
    fixture.unary().await;
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_before_item_header_suppresses_queued_begin() {
    let fixture = Harness::start(2).await;
    let budget = PayloadBufferBudget::new(1);
    let mut response = fixture
        .client
        .forward_stream_bound_sized_phased(
            &fixture.binding,
            "echo",
            3,
            stream::iter([Ok::<_, std::io::Error>(vec![1, 0, 8])]),
            budget.try_reserve(1).unwrap(),
        )
        .await
        .unwrap();
    let cancellation = response.cancellation();
    tokio::time::timeout(WAIT, cancellation.wait_queued_begin_for_test())
        .await
        .unwrap();
    cancellation.cancel(CancelCause::Consumer).unwrap();
    let record = tokio::time::timeout(WAIT, response.next_record())
        .await
        .unwrap()
        .unwrap();
    let ForwardRecord::CallEnd(terminal) = record else {
        panic!("cancelled item was announced")
    };
    assert_eq!(terminal.status, CallStatus::Failed);
    assert_eq!(terminal.error.unwrap().code, ErrorCode::CallCancelled);
    fixture.unary().await;
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn begin_arriving_after_consumer_cancel_is_never_announced() {
    let fixture = Harness::start(2).await;
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = mpsc::channel();
    *fixture.observed.step_entered.lock().unwrap() = Some(entered_tx);
    *fixture.observed.release_step.lock().unwrap() = Some(release_rx);
    let budget = PayloadBufferBudget::new(1);
    let mut response = fixture
        .client
        .forward_stream_bound_sized_phased(
            &fixture.binding,
            "echo",
            3,
            stream::iter([Ok::<_, std::io::Error>(vec![1, 0, 8])]),
            budget.try_reserve(1).unwrap(),
        )
        .await
        .unwrap();
    response.wait_uploaded().await.unwrap();
    wait_for_signal(entered_rx).await;
    response
        .cancellation()
        .cancel(CancelCause::Consumer)
        .unwrap();
    release_tx.send(()).unwrap();
    let record = tokio::time::timeout(WAIT, response.next_record())
        .await
        .unwrap()
        .unwrap();
    let ForwardRecord::CallEnd(terminal) = record else {
        panic!("late cancelled item was announced")
    };
    assert_eq!(terminal.status, CallStatus::Failed);
    fixture.unary().await;
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn early_not_executed_stops_a_stalled_request_source() {
    let fixture = Harness::start(2).await;
    assert!(fixture.server.unregister_route("portable").await);
    let budget = PayloadBufferBudget::new(1);
    let source = stream::once(async { Ok::<_, std::io::Error>(vec![1]) })
        .chain(stream::pending::<Result<Vec<u8>, std::io::Error>>());
    let mut response = tokio::time::timeout(
        WAIT,
        fixture.client.forward_stream_bound_sized_phased(
            &fixture.binding,
            "echo",
            3,
            source,
            budget.try_reserve(1).unwrap(),
        ),
    )
    .await
    .expect("stream handle must be ready before upload")
    .unwrap();
    let error = tokio::time::timeout(WAIT, response.wait_uploaded())
        .await
        .expect("early refusal must wake stalled upload")
        .expect_err("retired route must refuse opening");
    assert_eq!(error.phase(), c2_ipc::TransportPhase::PreDispatch);
    assert_eq!(fixture.client.pending_len_for_test(), 0);
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lifecycle_cancel_before_open_wakes_stalled_upload() {
    let fixture = Harness::start(2).await;
    let budget = PayloadBufferBudget::new(1);
    let mut response = fixture
        .client
        .forward_stream_bound_sized_phased(
            &fixture.binding,
            "echo",
            3,
            stream::pending::<Result<Vec<u8>, std::io::Error>>(),
            budget.try_reserve(1).unwrap(),
        )
        .await
        .unwrap();
    response
        .cancellation()
        .cancel(CancelCause::Lifecycle)
        .unwrap();
    let error = tokio::time::timeout(WAIT, response.wait_uploaded())
        .await
        .expect("cancellation must wake request source")
        .expect_err("cancelled pre-OPEN upload must not commit headers");
    assert_eq!(error.phase(), c2_ipc::TransportPhase::PreDispatch);
    assert_eq!(budget.snapshot().used_bytes, 0);
    fixture.unary().await;
    fixture.stop().await;
}

// The request block must exist while the source is incomplete, with no OPEN
// published. Checking the original pool distinguishes SHM preparation from
// chunked forwarding, which can otherwise return the same application result.
async fn check_unpublished_forward_shm(unary: bool, cancel: bool) {
    let fixture = Harness::start(2).await;
    let budget = PayloadBufferBudget::new(1);
    let (polled_tx, polled_rx) = tokio::sync::oneshot::channel();
    let (source_tx, source_rx) = tokio::sync::oneshot::channel::<()>();
    let source =
        stream::iter([Ok::<_, std::io::Error>(vec![0; 4096])]).chain(stream::once(async move {
            polled_tx.send(()).unwrap();
            let _ = source_rx.await;
            Err(std::io::Error::other("forward source failed before OPEN"))
        }));
    let mut response = None;
    let mut unary_task = None;
    if unary {
        let client = fixture.client.clone();
        let binding = fixture.binding.clone();
        let send = budget.try_reserve(1).unwrap();
        unary_task = Some(tokio::spawn(async move {
            client
                .forward_unary_bound_sized_phased(&binding, "ping", 8192, source, send)
                .await
        }));
    } else {
        response = Some(
            fixture
                .client
                .forward_stream_bound_sized_phased(
                    &fixture.binding,
                    "echo",
                    8192,
                    source,
                    budget.try_reserve(1).unwrap(),
                )
                .await
                .unwrap(),
        );
    }
    wait_for_signal(polled_rx).await;
    // SAFETY: observation only; the test never changes or frees pool storage.
    let pool = unsafe { fixture.client.request_pool_for_test() }.unwrap();
    assert_eq!(
        pool.read().stats().alloc_count,
        1,
        "partial input must fill an unpublished SHM request block"
    );
    assert_eq!(fixture.observed.opened.load(Ordering::SeqCst), 0);
    fixture.unary().await;
    if cancel {
        response
            .as_ref()
            .unwrap()
            .cancellation()
            .cancel(CancelCause::Lifecycle)
            .unwrap();
    } else {
        source_tx.send(()).unwrap();
    }
    let error = if let Some(task) = unary_task {
        tokio::time::timeout(WAIT, task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err()
    } else {
        tokio::time::timeout(WAIT, response.as_mut().unwrap().wait_uploaded())
            .await
            .expect("pre-OPEN cancellation must wake the stalled source")
            .unwrap_err()
    };
    assert_eq!(error.phase(), c2_ipc::TransportPhase::PreDispatch);
    assert_eq!(pool.read().stats().alloc_count, 0);
    assert_eq!(budget.snapshot().used_bytes, 0);
    assert_eq!(fixture.client.pending_len_for_test(), 0);
    fixture.unary().await;
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unary_forward_source_error_releases_unpublished_request_shm() {
    check_unpublished_forward_shm(true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stream_forward_source_error_releases_unpublished_request_shm() {
    check_unpublished_forward_shm(false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stream_forward_cancel_releases_unpublished_request_shm() {
    check_unpublished_forward_shm(false, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forward_request_shm_checks_source_eof_and_declared_length_before_open() {
    let fixture = Harness::start(2).await;
    let budget = PayloadBufferBudget::new(1);
    for unary in [false, true] {
        for actual in [8191, 8193] {
            let source = stream::iter([Ok::<_, std::io::Error>(vec![0; actual])]);
            let error = if unary {
                fixture
                    .client
                    .forward_unary_bound_sized_phased(
                        &fixture.binding,
                        "ping",
                        8192,
                        source,
                        budget.try_reserve(1).unwrap(),
                    )
                    .await
                    .unwrap_err()
            } else {
                fixture
                    .client
                    .forward_stream_bound_sized_phased(
                        &fixture.binding,
                        "echo",
                        8192,
                        source,
                        budget.try_reserve(1).unwrap(),
                    )
                    .await
                    .unwrap()
                    .wait_uploaded()
                    .await
                    .unwrap_err()
            };
            assert_eq!(error.phase(), c2_ipc::TransportPhase::PreDispatch);
            // SAFETY: this handle is used only to inspect allocation counts.
            let pool = unsafe { fixture.client.request_pool_for_test() }.unwrap();
            assert_eq!(pool.read().stats().alloc_count, 0);
            assert_eq!(fixture.client.pending_len_for_test(), 0);
            assert_eq!(fixture.observed.opened.load(Ordering::SeqCst), 0);
            assert_eq!(budget.snapshot().used_bytes, 0);
            fixture.unary().await;
        }
    }
    // A full-size first chunk still cannot publish before its source reaches
    // EOF: a later source error must remain a pre-dispatch failure.
    let error = fixture
        .client
        .forward_unary_bound_sized_phased(
            &fixture.binding,
            "ping",
            8192,
            stream::iter([
                Ok(vec![0; 8192]),
                Err(std::io::Error::other("error after all bytes")),
            ]),
            budget.try_reserve(1).unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(error.phase(), c2_ipc::TransportPhase::PreDispatch);
    fixture.unary().await;
    fixture.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forward_request_shm_publishes_after_complete_source() {
    let fixture = Harness::start(2).await;
    let budget = PayloadBufferBudget::new(1);
    let response = fixture
        .client
        .forward_unary_bound_sized_phased(
            &fixture.binding,
            "ping",
            8192,
            stream::iter([Ok::<_, std::io::Error>(vec![0; 4096]), Ok(vec![0; 4096])]),
            budget.try_reserve(1).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.into_owned_bytes().unwrap(), b"pong");
    let mut response = fixture
        .client
        .forward_stream_bound_sized_phased(
            &fixture.binding,
            "echo",
            8192,
            stream::iter([Ok::<_, std::io::Error>(vec![0; 4096]), Ok(vec![0; 4096])]),
            budget.try_reserve(1).unwrap(),
        )
        .await
        .unwrap();
    response.wait_uploaded().await.unwrap();
    assert!(matches!(
        response.next_record().await.unwrap(),
        ForwardRecord::CallEnd(StreamTerminal {
            status: CallStatus::Success,
            complete_items: 0,
            ..
        })
    ));
    assert_eq!(fixture.observed.opened.load(Ordering::SeqCst), 1);
    assert_eq!(budget.snapshot().used_bytes, 0);
    fixture.unary().await;
    fixture.stop().await;
}

#[tokio::test]
async fn queued_forward_frame_keeps_its_charge_after_deactivation() {
    // On this current-thread executor the upload driver runs from source EOF
    // to the queued submit wait before the EOF notification wakes this test.
    let fixture = Harness::start(2).await;
    let (paused_tx, paused_rx) = tokio::sync::oneshot::channel();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    fixture
        .client
        .set_write_pause_for_test(Some((paused_tx, resume_rx)));
    let first = tokio::spawn({
        let client = fixture.client.clone();
        let binding = fixture.binding.clone();
        async move { client.call_bound(&binding, "ping", b"").await }
    });
    wait_for_signal(paused_rx).await;
    let budget = PayloadBufferBudget::new(1);
    let (eof_tx, eof_rx) = tokio::sync::oneshot::channel();
    let mut eof_tx = Some(eof_tx);
    let mut remaining = Some(vec![7]);
    let source = stream::poll_fn(move |_| {
        if let Some(bytes) = remaining.take() {
            std::task::Poll::Ready(Some(Ok::<_, std::io::Error>(bytes)))
        } else {
            eof_tx.take().unwrap().send(()).unwrap();
            std::task::Poll::Ready(None)
        }
    });
    let queued = tokio::spawn({
        let client = fixture.client.clone();
        let binding = fixture.binding.clone();
        let send = budget.try_reserve(1).unwrap();
        async move {
            client
                .forward_unary_bound_sized_phased(&binding, "ping", 1, source, send)
                .await
        }
    });
    wait_for_signal(eof_rx).await;
    fixture.client.deactivate();
    let error = tokio::time::timeout(WAIT, queued)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.phase(), c2_ipc::TransportPhase::PreDispatch);
    assert_eq!(
        budget.snapshot().used_bytes,
        1,
        "queued frame bytes still own their charge after the submit waiter ends"
    );
    resume_tx.send(()).unwrap();
    let response = tokio::time::timeout(WAIT, first)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(response.into_owned_bytes().unwrap(), b"pong");
    tokio::time::timeout(WAIT, async {
        while budget.snapshot().used_bytes != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    fixture.stop().await;
}
