use super::*;
use c2_wire::release::{ReleaseBatch, encode_release_frame};
use std::thread;
use std::time::Instant;
use tokio::io::AsyncReadExt;

fn registered_server(
    observed: Arc<Observed>,
    config: ServerIpcConfig,
) -> (Arc<Server>, RouteCallIdentity) {
    let descriptor = ValidatedContractDescriptor::from_json(STREAM_DESCRIPTOR).unwrap();
    let server = Arc::new(
        Server::new_with_endpoint(
            c2_config::LocalEndpointContext::default_for_platform()
                .unwrap()
                .endpoint(&format!(
                    "ipc://stream-executor-{}",
                    uuid::Uuid::new_v4().simple()
                ))
                .unwrap(),
            config,
        )
        .unwrap(),
    );
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
        crm_ns: built.crm_ns().to_string(),
        crm_name: built.crm_name().to_string(),
        crm_ver: built.crm_ver().to_string(),
        abi_hash: built.abi_hash().to_string(),
        signature_hash: built.signature_hash().to_string(),
    };
    let reservation = futures_executor::block_on(server.reserve_route(built)).unwrap();
    futures_executor::block_on(server.commit_reserved_route(reservation)).unwrap();
    (server, identity)
}

enum ShutdownPoint {
    BeforeFirstStep,
    BeforeEncoding,
    Orderly,
}

fn encoding_waits(shm: bool, shutdown: ShutdownPoint) {
    let stop_executor = matches!(shutdown, ShutdownPoint::BeforeEncoding);
    let peer_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let server_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let observed = Arc::new(Observed::default());
    let mut config = ServerIpcConfig::default();
    if shm {
        config.shm_threshold = 1;
    }
    let (server, identity) = registered_server(observed.clone(), config);
    let (server_stream, mut peer) = peer_runtime.block_on(isolated_pair());
    let (_, write_half) = server_stream.into_split();
    let conn = Arc::new(Connection::new(1));
    conn.core.finish_handshake().unwrap();
    conn.set_shm_response_capable(shm);
    let (writer, writer_task) = server_runtime.block_on(async {
        ConnectionWriter::spawn(
            write_half,
            conn.clone(),
            server.config.write_timeout(),
            server.control_reply_budget.clone(),
        )
    });
    *conn.rpc_writer.lock() = Some(Arc::downgrade(&writer));
    let call = conn.calls.lock().open(1, ServerCall::default()).unwrap();
    conn.core.stream_open(1).unwrap();
    let route = reserve_accepted_route_execution(&server, &identity, 1, CallMode::ServerStream)
        .unwrap_or_else(|_| panic!("stream route admission failed"));
    *call.data.stream.lock() = Some(StreamCallControl {
        route_uid: route.route.route_uid.clone(),
        cancellation: None,
    });
    assert!(call.queue());
    let admission = ChunkRouteAdmission {
        route: route.route,
        method_idx: 1,
        pending_permit: route.pending_permit,
        _pending_permit: server.try_acquire_pending_request().unwrap(),
        _flight: crate::connection::FlightGuard::new(&conn),
    };

    if matches!(shutdown, ShutdownPoint::BeforeFirstStep) {
        let handle = server_runtime.handle().clone();
        server_runtime.shutdown_background();
        let _entered = handle.enter();
        futures_executor::block_on(stream::dispatch_stream_call(
            &server,
            &conn,
            &writer,
            1,
            RequestData::Inline(vec![1]),
            admission,
        ));
        assert_eq!(observed.entries.load(Ordering::SeqCst), 0);
        assert_eq!(observed.finished.load(Ordering::SeqCst), 0);
        assert!(conn.calls.lock().get(1).is_none());
        assert!(conn.core.stream_state(1).is_none());
        assert_eq!(conn.inflight_count(), 0);
        let writer_end = peer_runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(5), writer_task).await })
            .unwrap();
        // The writer may finish on conn.end before Tokio cancels its task.
        if let Err(error) = writer_end {
            assert!(
                error.is_cancelled(),
                "writer panicked during teardown: {error}"
            );
        }
        let mut byte = [0];
        let read = peer_runtime
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(5), peer.read(&mut byte)).await
            })
            .unwrap()
            .unwrap();
        assert_eq!(
            read, 0,
            "peer expected EOF without a false NotExecuted CALL_END"
        );
        return;
    }

    // Poll the original dispatch future outside Tokio's task set. This lets
    // it observe its own encoding JoinHandle after the executor has stopped.
    let (encoding_tx, encoding_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    let runtime_handle = server_runtime.handle().clone();
    let dispatch_server = server.clone();
    let dispatch_conn = conn.clone();
    let dispatch_writer = writer.clone();
    let (dispatch_done_tx, dispatch_done_rx) = mpsc::channel();
    let dispatch = thread::spawn(move || {
        let _entered = runtime_handle.enter();
        if stop_executor {
            stream::pause_next_encoding(encoding_tx, resume_rx);
        } else {
            stream::observe_next_encoding(encoding_tx);
        }
        futures_executor::block_on(stream::dispatch_stream_call(
            &dispatch_server,
            &dispatch_conn,
            &dispatch_writer,
            1,
            RequestData::Inline(vec![1]),
            admission,
        ));
        dispatch_done_tx.send(()).unwrap();
    });
    assert!(matches!(
        peer_runtime.block_on(read_record(&mut peer, CallMode::ServerStream)),
        SeenRecord::Begin(4)
    ));

    if stop_executor {
        conn.core.stream_credit(1, 4, 0).unwrap();
        conn.writer_wake.notify_waiters();
        encoding_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(observed.encoded_ranges.lock().unwrap().is_empty());
        assert_eq!(observed.steps.load(Ordering::SeqCst), 1);

        // The observed dispatch future pauses before submitting encoding.
        // Submitting through its entered Handle after real runtime teardown
        // yields a cancelled JoinHandle without any test-injected abort.
        server_runtime.shutdown_background();
        resume_tx.send(()).unwrap();
        let dispatch_completed = dispatch_done_rx
            .recv_timeout(Duration::from_secs(5))
            .is_ok();
        assert!(
            dispatch.join().is_ok(),
            "dispatch panicked on cancelled encoding"
        );
        assert!(
            dispatch_completed,
            "dispatch did not settle after executor teardown"
        );
        let writer_end = peer_runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(5), writer_task).await })
            .expect("writer task did not exit after executor shutdown");
        // The writer may finish on conn.end before Tokio cancels its task.
        if let Err(error) = writer_end {
            assert!(
                error.is_cancelled(),
                "writer panicked during teardown: {error}"
            );
        }
        let mut byte = [0];
        let read = peer_runtime
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(5), peer.read(&mut byte)).await
            })
            .unwrap()
            .unwrap();
        assert_eq!(read, 0, "peer expected EOF without ITEM_DATA or CALL_END");
        assert!(
            conn.is_deactivated(),
            "executor teardown did not end the connection"
        );
        assert!(observed.encoded_ranges.lock().unwrap().is_empty());
        assert_eq!(observed.finished.load(Ordering::SeqCst), 0);
        assert!(conn.calls.lock().get(1).is_none());
        assert!(conn.core.stream_state(1).is_none());
        assert_eq!(
            server.pending_requests.available_permits(),
            server.config.max_pending_requests as usize
        );
        return;
    }

    let (blocker_entered_tx, blocker_entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let blocker = server_runtime.handle().spawn_blocking(move || {
        blocker_entered_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    blocker_entered_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    conn.core.stream_credit(1, 4, 0).unwrap();
    conn.writer_wake.notify_waiters();
    encoding_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(observed.encoded_ranges.lock().unwrap().is_empty());
    assert_eq!(observed.steps.load(Ordering::SeqCst), 1);

    // The live executor must still finish the current item and cleanup
    // after host shutdown intent arrives during queued encoding.
    submit_stream_cancel(&conn, 1, &call, StreamCancelCause::Shutdown);
    release_tx.send(()).unwrap();
    match peer_runtime.block_on(read_record(&mut peer, CallMode::ServerStream)) {
        SeenRecord::Data(bytes) if !shm => assert_eq!(bytes, [0; 4]),
        SeenRecord::ShmData(loan) if shm => {
            assert_eq!(loan.data_size, 4);
            let release = encode_release_frame(&ReleaseBatch::new(&[loan.loan_id]).unwrap(), false);
            let (header, payload) = frame::decode_frame(&release).unwrap();
            let outcome = conn.core.receive_release(&header, payload);
            assert!(outcome.error.is_none(), "SHM loan release failed");
            outcome.reclaims.release();
        }
        other => panic!("expected current item after shutdown intent, got {other:?}"),
    }
    let (id, flags, payload) =
        peer_runtime.block_on(super::super::tests::read_reply_frame(&mut peer));
    let header = frame::FrameHeader {
        total_len: 12 + payload.len() as u32,
        request_id: id,
        flags,
    };
    match rpc::decode_rpc_record(&header, &payload, Some(CallMode::ServerStream)).unwrap() {
        RpcRecord::CallEnd {
            status: CallStatus::Failed,
            complete_items: Some(1),
            error: Some(error),
            ..
        } => {
            assert_eq!(error.code, ErrorCode::CallCancelled);
            assert!(error.message.contains("Runtime shutdown"));
        }
        other => panic!("expected Runtime shutdown terminal, got {other:?}"),
    }
    dispatch_done_rx
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    dispatch.join().unwrap();
    assert_eq!(observed.finished.load(Ordering::SeqCst), 1);
    assert_eq!(observed.steps.load(Ordering::SeqCst), 1);
    assert!(conn.calls.lock().get(1).is_none());
    drop(peer);
    drop(writer);
    server_runtime.shutdown_background();
    assert!(blocker.is_finished());
}

#[test]
fn runtime_teardown_before_inline_encoding_submission_ends_without_secondary_panic() {
    encoding_waits(false, ShutdownPoint::BeforeEncoding);
}

#[test]
fn runtime_teardown_before_shm_encoding_submission_ends_without_secondary_panic() {
    encoding_waits(true, ShutdownPoint::BeforeEncoding);
}

#[test]
fn host_shutdown_during_queued_encoding_delivers_item_then_cancelled_terminal() {
    encoding_waits(false, ShutdownPoint::Orderly);
}

#[test]
fn host_shutdown_during_queued_shm_encoding_delivers_item_then_cancelled_terminal() {
    encoding_waits(true, ShutdownPoint::Orderly);
}

#[test]
fn runtime_teardown_before_first_step_retires_the_unexecuted_stream() {
    encoding_waits(false, ShutdownPoint::BeforeFirstStep);
}

#[test]
fn cleanup_keeps_connection_busy_after_dispatch_executor_stops() {
    let server_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let observer_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let observed = Arc::new(Observed::default());
    let mut harness = server_runtime.block_on(Harness::start(
        observed.clone(),
        ServerIpcConfig::default(),
        0,
    ));
    server_runtime.block_on(async {
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
        assert!(matches!(
            read_record(&mut harness.peer, CallMode::ServerStream).await,
            SeenRecord::Data(data) if data == [0; 4]
        ));
    });
    let conn = harness
        .server
        .active_connection(harness.server.active_connection_ids()[0])
        .unwrap();

    // CounterStream::finish records entry before locking finish_entered.
    // Holding this lock keeps its actual callback body on the blocking worker
    // while shutdown cancels the async dispatch task that used to own flight.
    let finish_gate = observed.finish_entered.lock().unwrap();
    server_runtime.block_on(async {
        harness
            .peer
            .write_all(&rpc::encode_cancel_frame(1, rpc::CancelCause::Consumer).unwrap())
            .await
            .unwrap();
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while observed.finished.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        thread::yield_now();
    }
    assert_eq!(observed.finished.load(Ordering::SeqCst), 1);

    server_runtime.shutdown_background();
    assert!(
        observer_runtime
            .block_on(async {
                tokio::time::timeout(Duration::from_millis(100), conn.wait_idle()).await
            })
            .is_err(),
        "connection drained while its cleanup callback was still running"
    );
    drop(finish_gate);
    observer_runtime
        .block_on(async { tokio::time::timeout(Duration::from_secs(5), conn.wait_idle()).await })
        .unwrap();
    assert_eq!(conn.inflight_count(), 0);
}
