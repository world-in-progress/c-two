//! Readable stream failures stay local to the original call and its owners.
use super::*;
use crate::IpcStreamCancellation;
use crate::StreamTerminal;
use c2_error::{C2Error, ErrorCode};
use c2_mem::retention::PayloadBufferBudget;
use c2_wire::rpc::{
    encode_call_end_frame, encode_cancel_frame, encode_credit_frame, encode_item_begin_frame,
    encode_item_data_frame,
};

#[path = "stream_shm_tests.rs"]
mod shm;

async fn check_shm_item_copy(cancel_during_copy: bool) {
    use c2_contract::ValidatedContractDescriptor;
    use c2_server::{CrmStream, CrmStreamCallback, CrmStreamItem};

    struct OperationOwner(Arc<std::sync::atomic::AtomicBool>);
    impl Drop for OperationOwner {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    struct StreamCallback(std::sync::Mutex<Option<std::sync::mpsc::Receiver<()>>>);
    struct OneItem(std::sync::mpsc::Receiver<()>, bool);
    impl CrmCallback for StreamCallback {
        fn invoke(
            &self,
            _: &str,
            _: u16,
            _: Vec<u8>,
            _: Arc<c2_server::ResponsePool>,
        ) -> Result<ResponseMeta, CrmError> {
            Ok(ResponseMeta::Inline(b"pong".to_vec()))
        }
    }
    impl CrmStreamCallback for StreamCallback {
        fn open_stream(
            &self,
            _: &str,
            _: u16,
            _: Vec<u8>,
            _: Arc<c2_server::ResponsePool>,
        ) -> Result<Box<dyn CrmStream>, CrmError> {
            Ok(Box::new(OneItem(
                self.0.lock().unwrap().take().unwrap(),
                false,
            )))
        }
    }
    impl CrmStream for OneItem {
        fn step(&mut self) -> Result<Option<Box<dyn CrmStreamItem>>, CrmError> {
            if self.1 {
                return Ok(None);
            }
            self.0.recv().unwrap();
            self.1 = true;
            Ok(Some(Box::new(vec![7_u8; 64])))
        }
        fn finish(self: Box<Self>) -> Result<(), CrmError> {
            Ok(())
        }
    }

    let endpoint = Endpoint::new();
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
            endpoint.endpoint.clone(),
            ServerIpcConfig {
                shm_threshold: 1,
                ..ServerIpcConfig::default()
            },
        )
        .unwrap(),
    );
    let (step_tx, step_rx) = std::sync::mpsc::channel();
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
            Arc::new(StreamCallback(std::sync::Mutex::new(Some(step_rx)))),
            &descriptor,
        )
        .unwrap();
    let reservation = server.reserve_route(built).await.unwrap();
    server.commit_reserved_route(reservation).await.unwrap();
    server.begin_start_attempt().unwrap();
    let runner = tokio::spawn({
        let server = server.clone();
        async move { server.run().await.unwrap() }
    });
    server.wait_until_ready(WAIT).await.unwrap();

    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let binding = client.acquire_route(&expected).await.unwrap();
    let owner_dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut response = client
        .stream_bound_owned_controlled_phased(
            &binding,
            "echo",
            Vec::new(),
            PayloadBufferBudget::new(1024),
            Box::new(OperationOwner(owner_dropped.clone())),
            IpcStreamCancellation::new(),
            &IpcCallControl::new(|| Ok(())),
        )
        .await
        .unwrap();
    let (copy_enter_tx, copy_enter_rx) = std::sync::mpsc::channel();
    let (copy_release_tx, copy_release_rx) = std::sync::mpsc::channel();
    response
        .receive_call_for_test()
        .set_shm_copy_gate_for_test(copy_enter_tx, copy_release_rx);
    step_tx.send(()).unwrap();
    tokio::task::spawn_blocking(move || copy_enter_rx.recv_timeout(WAIT))
        .await
        .unwrap()
        .expect("the real SHM ITEM_DATA copy did not reach the test gate");
    assert!(!owner_dropped.load(Ordering::Acquire));

    if cancel_during_copy {
        response.cancel().unwrap();
        timeout(WAIT, async {
            loop {
                if client.pending.lock().is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("CANCEL cleanup terminal did not pass the active SHM copy");
        assert!(response.try_completion().is_none());
        assert!(!owner_dropped.load(Ordering::Acquire));
    }

    let call = response.receive_call_for_test();
    let (lock_done_tx, lock_done_rx) = std::sync::mpsc::channel();
    let lock_probe = std::thread::spawn(move || {
        drop(call.hold_state_for_test());
        lock_done_tx.send(()).unwrap();
    });
    let lock_result = tokio::task::spawn_blocking(move || lock_done_rx.recv_timeout(WAIT)).await;
    let unary_result = timeout(WAIT, client.call_bound(&binding, "ping", b"")).await;
    copy_release_tx.send(()).unwrap();
    lock_probe.join().unwrap();
    assert!(
        lock_result.unwrap().is_ok(),
        "the SHM copy held stream state"
    );
    let unary = unary_result
        .expect("SHM copy blocked the same socket reader")
        .unwrap();
    assert_eq!(unary.into_owned_bytes().unwrap(), b"pong");
    if cancel_during_copy {
        let terminal = response.completion().await.unwrap();
        assert_eq!(terminal.status, CallStatus::Failed);
        assert_eq!(terminal.complete_items, 1);
        assert_eq!(terminal.error.unwrap().code, ErrorCode::CallCancelled);
        assert!(response.recv().await.is_err());
    } else {
        assert_eq!(response.recv().await.unwrap().unwrap().as_ref(), &[7; 64]);
        assert!(response.recv().await.unwrap().is_none());
        assert_eq!(response.completion().await.unwrap().complete_items, 1);
    }
    assert!(owner_dropped.load(Ordering::Acquire));
    client.close_shared().await;
    server.shutdown_and_wait(WAIT).await.unwrap();
    runner.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_item_copy_does_not_hold_the_reader_or_stream_state() {
    check_shm_item_copy(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shm_copy_defers_cancel_terminal_until_the_carrier_is_released() {
    check_shm_item_copy(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancellation_and_zero_observation_do_not_wait_for_brief_receive_state_lock() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let peer = tokio::spawn(async move {
        let mut socket = accept(&mut listener).await;
        let (open, _) = read_frame(&mut socket).await;
        let (cancel, payload) = read_frame(&mut socket).await;
        assert_eq!(cancel.request_id, open.request_id);
        assert_eq!(
            payload,
            encode_cancel_frame(open.request_id, rpc::CancelCause::Consumer).unwrap()
                [HEADER_SIZE..]
        );
        let error = C2Error::new(ErrorCode::CallCancelled, "cancelled after copy").to_wire_bytes();
        socket
            .write_all(
                &encode_call_end_frame(
                    open.request_id,
                    CallMode::ServerStream,
                    CallStatus::Failed,
                    0,
                    ValuePart::whole(ValueStorage::Inline(&error)),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        drop(socket);
        crate::tests::close_listener(listener).await;
    });
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let response = client
        .stream_bound(&binding(), "items", b"", PayloadBufferBudget::new(8))
        .await
        .unwrap();
    let call = response.receive_call_for_test();
    let copy_lock = call.hold_state_for_test();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        response.cancel().unwrap();
        let pending = response.try_completion().is_none();
        done_tx.send(pending).unwrap();
        response
    });
    // Cancellation intent and the zero-budget observer must return even while
    // a short state operation holds the lock. Settlement follows CALL_END.
    let returned = done_rx.recv_timeout(WAIT);
    drop(copy_lock);
    let response = worker.join().unwrap();
    assert!(returned.unwrap());
    let terminal = timeout(WAIT, response.completion()).await.unwrap().unwrap();
    assert_eq!(terminal.error.unwrap().code, ErrorCode::CallCancelled);
    client.close_shared().await;
    peer.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn opening_abandonment_intent_attaches_before_request_publication() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let peer = tokio::spawn(async move {
        let mut socket = accept(&mut listener).await;
        let (open, _) = read_frame(&mut socket).await;
        let (cancel, payload) = read_frame(&mut socket).await;
        assert_eq!(cancel.request_id, open.request_id);
        assert_eq!(
            payload,
            encode_cancel_frame(open.request_id, rpc::CancelCause::Consumer).unwrap()
                [HEADER_SIZE..]
        );
        let error = C2Error::new(ErrorCode::CallCancelled, "opening waiter left").to_wire_bytes();
        socket
            .write_all(
                &encode_call_end_frame(
                    open.request_id,
                    CallMode::ServerStream,
                    CallStatus::Failed,
                    0,
                    ValuePart::whole(ValueStorage::Inline(&error)),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        drop(socket);
        crate::tests::close_listener(listener).await;
    });
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let cancellation = IpcStreamCancellation::new();
    cancellation.cancel();
    let control = IpcCallControl::new(|| Ok(()));
    let stream = client
        .stream_bound_owned_controlled_phased(
            &binding(),
            "items",
            Vec::new(),
            PayloadBufferBudget::new(8),
            Box::new(()),
            cancellation,
            &control,
        )
        .await
        .unwrap();
    let terminal = timeout(WAIT, stream.completion()).await.unwrap().unwrap();
    assert_eq!(terminal.error.unwrap().code, ErrorCode::CallCancelled);
    client.close_shared().await;
    peer.await.unwrap();
}

fn binding() -> RouteBinding {
    RouteBinding::from_table(MethodTable::from_entries(
        &[MethodEntry {
            name: "items".into(),
            index: 0,
        }],
        identity(),
        1024,
    ))
}

fn large_binding() -> RouteBinding {
    RouteBinding::from_table(MethodTable::from_entries(
        &[MethodEntry {
            name: "items".into(),
            index: 0,
        }],
        identity(),
        16 * 1024,
    ))
}

async fn grant(stream: &mut LocalStream, rid: u64, bytes: u64, demand: u64) {
    let (header, payload) = read_frame(stream).await;
    assert_eq!(header.request_id, rid);
    assert_eq!(
        payload,
        encode_credit_frame(rid, bytes, demand).unwrap()[HEADER_SIZE..]
    );
}

async fn send_item(stream: &mut LocalStream, rid: u64, bytes: &[u8]) {
    stream
        .write_all(&encode_item_begin_frame(rid, bytes.len() as u64).unwrap())
        .await
        .unwrap();
    grant(stream, rid, bytes.len() as u64, 0).await;
    // Unequal positive pieces deliberately differ from unary's fixed geometry.
    for part in [&bytes[..1], &bytes[1..]] {
        stream
            .write_all(&encode_item_data_frame(rid, ValueStorage::Inline(part)).unwrap())
            .await
            .unwrap();
    }
}

async fn bad_terminal(fault: u8) {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let (finish, finish_now) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let (open, _) = read_frame(&mut stream).await;
        if fault == 2 {
            stream
                .write_all(&encode_item_begin_frame(open.request_id, 3).unwrap())
                .await
                .unwrap();
            grant(&mut stream, open.request_id, 3, 0).await;
        } else {
            send_item(&mut stream, open.request_id, b"abc").await;
        }
        finish_now.await.unwrap();
        let end = if fault == 2 {
            let error = C2Error::new(
                ErrorCode::CallCancelled,
                "not executed after a resource item began",
            )
            .to_wire_bytes();
            encode_call_end_frame(
                open.request_id,
                CallMode::ServerStream,
                CallStatus::NotExecuted,
                0,
                ValuePart::whole(ValueStorage::Inline(&error)),
            )
            .unwrap()
        } else if fault == 1 {
            encode_call_end_frame(
                open.request_id,
                CallMode::ServerStream,
                CallStatus::Success,
                0,
                ValuePart::whole(ValueStorage::Inline(&[])),
            )
            .unwrap()
        } else {
            frame::encode_frame_parts(
                open.request_id,
                flags::FLAG_RPC_RECORD | flags::FLAG_RESPONSE,
                &[
                    &[RpcRecordKind::CallEnd as u8, CallStatus::Failed as u8],
                    &1u64.to_le_bytes(),
                    b"bad C2E1",
                ],
            )
        };
        stream.write_all(&end).await.unwrap();
        loop {
            let (header, payload) = read_frame(&mut stream).await;
            if payload.first() == Some(&(RpcRecordKind::Open as u8)) {
                // Retired stream records must be discarded by their original
                // ID, without deactivating this healthy following call.
                stream
                    .write_all(&encode_item_begin_frame(open.request_id, 3).unwrap())
                    .await
                    .unwrap();
                stream
                    .write_all(
                        &encode_item_data_frame(open.request_id, ValueStorage::Inline(b"old"))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                reply(&mut stream, header.request_id, b"healthy").await;
                break;
            }
            assert_eq!(header.request_id, open.request_id);
            assert!(matches!(payload.first(), Some(4 | 5)));
        }
        expect_disconnect_or_eof(&mut stream).await;
        crate::tests::close_listener(listener).await;
    });
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let mut response = client
        .stream_bound(&binding(), "items", b"", PayloadBufferBudget::new(16))
        .await
        .unwrap();
    if fault != 2 {
        let first = timeout(WAIT, response.recv())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(first.as_ref(), b"abc");
    }
    finish.send(()).unwrap();
    let error = timeout(WAIT, response.recv()).await.unwrap().unwrap_err();
    assert_eq!(error.phase(), crate::TransportPhase::DispatchUncertain);
    assert!(
        timeout(WAIT, response.completion())
            .await
            .expect("bad END must retire the original call")
            .is_err()
    );
    assert_eq!(client.pending_len_for_test(), 0);
    assert_eq!(
        client
            .call_inline(&identity(), 0, b"next")
            .await
            .unwrap()
            .into_owned_bytes()
            .unwrap(),
        b"healthy"
    );
    assert!(
        client.is_connected(),
        "late records must not stop new admission"
    );
    assert!(client.close_shared_bounded(WAIT).await);
    peer.await.unwrap();
}

#[tokio::test]
async fn mismatched_stream_end_count_retires_only_that_call() {
    bad_terminal(1).await;
}

#[tokio::test]
async fn undecodable_stream_end_retires_only_that_call() {
    bad_terminal(0).await;
}

async fn complete_item_before_end(peer_ends: bool) {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let (open, _) = read_frame(&mut stream).await;
        send_item(&mut stream, open.request_id, b"abc").await;
        grant(&mut stream, open.request_id, 0, 1).await;
        send_item(&mut stream, open.request_id, b"defg").await;
        if !peer_ends {
            let error =
                C2Error::new(ErrorCode::ResourceFunctionExecuting, "cleanup error").to_wire_bytes();
            stream
                .write_all(
                    &encode_call_end_frame(
                        open.request_id,
                        CallMode::ServerStream,
                        CallStatus::Failed,
                        2,
                        ValuePart::whole(ValueStorage::Inline(&error)),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            expect_disconnect_or_eof(&mut stream).await;
        }
        drop(stream);
        crate::tests::close_listener(listener).await;
    });
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let budget = PayloadBufferBudget::new(7);
    let mut response = client
        .stream_bound(&binding(), "items", b"", budget.clone())
        .await
        .unwrap();
    // Observe END/EOF while the first item still occupies the consumer slot.
    // The second complete item must survive until that slot becomes free.
    let terminal = timeout(WAIT, response.completion()).await.unwrap();
    if peer_ends {
        assert!(terminal.is_err());
    } else {
        assert_eq!(terminal.unwrap().complete_items, 2);
    }
    let first = timeout(WAIT, response.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let second = timeout(WAIT, response.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first.as_ref(), b"abc");
    assert_eq!(second.as_ref(), b"defg");
    assert!(timeout(WAIT, response.recv()).await.unwrap().is_err());
    drop((first, second));
    assert_eq!(budget.snapshot().used_bytes, 0);
    assert!(client.close_shared_bounded(WAIT).await);
    peer.await.unwrap();
}

#[tokio::test]
async fn complete_item_waiting_for_consumer_precedes_failed_end() {
    complete_item_before_end(false).await;
}

#[tokio::test]
async fn complete_item_waiting_for_consumer_survives_peer_eof() {
    complete_item_before_end(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancel_natural_end_and_peer_eof_share_one_stream_terminal() {
    // Block 9b3 requirement 1: cancellation, natural completion and real EOF
    // race on one barrier with one delivered item and one admitted half-item.
    check_cancel_natural_eof_race(false).await;
    // Also force the section 8 boundary: a terminal actually obtained before
    // EOF remains the same terminal after the physical peer end is observed.
    check_cancel_natural_eof_race(true).await;
}

async fn check_cancel_natural_eof_race(terminal_before_eof: bool) {
    use tokio::sync::{Barrier, Mutex};

    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let (ready_tx, ready_rx) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let mut socket = accept(&mut listener).await;
        let (open, _) = read_frame(&mut socket).await;
        let rid = open.request_id;
        send_item(&mut socket, rid, b"abc").await;
        grant(&mut socket, rid, 0, 1).await;
        socket
            .write_all(&encode_item_begin_frame(rid, 4).unwrap())
            .await
            .unwrap();
        grant(&mut socket, rid, 4, 0).await;
        socket
            .write_all(&encode_item_data_frame(rid, ValueStorage::Inline(b"xy")).unwrap())
            .await
            .unwrap();
        assert!(ready_tx.send((socket, rid, listener)).is_ok());
    });

    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let observation = client.connection_observation().unwrap();
    let mut response = client
        .stream_bound(&binding(), "items", b"", PayloadBufferBudget::new(16))
        .await
        .unwrap();
    let first = timeout(WAIT, response.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first.as_ref(), b"abc");
    let (socket, rid, listener) = ready_rx.await.unwrap();
    peer.await.unwrap();
    timeout(WAIT, async {
        loop {
            let state = client
                .driver
                .read()
                .as_ref()
                .unwrap()
                .core
                .stream_state(rid)
                .unwrap();
            if state.total_bytes == Some(4) && state.admitted_bytes == 2 {
                assert_eq!(state.complete_items, 1);
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the client did not admit the half item before the race");

    // The second item is only half complete when all three competitors start.
    // The peer's mutex makes END plus its final data one complete operation:
    // EOF can win before it or follow it, never forge a truncated END frame.
    let socket = Arc::new(Mutex::new(Some(socket)));
    let start = Arc::new(Barrier::new(4));
    let cancel_task = tokio::spawn({
        let start = start.clone();
        async move {
            start.wait().await;
            response.cancel().unwrap();
            response
        }
    });
    let natural_end = tokio::spawn({
        let start = start.clone();
        let socket = socket.clone();
        async move {
            start.wait().await;
            let mut guard = socket.lock().await;
            if let Some(socket) = guard.as_mut() {
                socket
                    .write_all(&encode_item_data_frame(rid, ValueStorage::Inline(b"zw")).unwrap())
                    .await
                    .unwrap();
                socket
                    .write_all(
                        &encode_call_end_frame(
                            rid,
                            CallMode::ServerStream,
                            CallStatus::Success,
                            2,
                            ValuePart::whole(ValueStorage::Inline(&[])),
                        )
                        .unwrap(),
                    )
                    .await
                    .unwrap();
            }
        }
    });
    let (allow_eof, end_observed) = oneshot::channel();
    let eof = tokio::spawn({
        let start = start.clone();
        let socket = socket.clone();
        async move {
            start.wait().await;
            if terminal_before_eof {
                end_observed.await.unwrap();
            }
            // Dropping the only peer LocalStream closes the real OS endpoint.
            drop(socket.lock().await.take());
        }
    });
    start.wait().await;
    let mut response = cancel_task.await.unwrap();
    let before_eof = if terminal_before_eof {
        let terminal = timeout(WAIT, response.completion()).await.unwrap().unwrap();
        assert_eq!(terminal.status, CallStatus::Success);
        assert_eq!(terminal.complete_items, 2);
        allow_eof.send(()).unwrap();
        Some(terminal)
    } else {
        None
    };
    natural_end.await.unwrap();
    eof.await.unwrap();
    observation
        .wait_closed(Some(WAIT))
        .await
        .expect("the client did not observe the physical peer end");

    // This peer only sends natural Success. EOF before a complete END yields
    // DispatchUncertain; cancellation alone cannot invent CallCancelled.
    // Neither outcome may revoke the already delivered "abc".
    let completion = timeout(WAIT, response.completion()).await.unwrap();
    if let Some(terminal) = before_eof {
        assert_eq!(completion.as_ref().unwrap(), &terminal);
    }
    match &completion {
        Ok(terminal) => {
            assert_eq!(terminal.status, CallStatus::Success);
            assert_eq!(terminal.complete_items, 2);
            assert!(terminal.error.is_none());
            assert_eq!(terminal, &response.completion().await.unwrap());
        }
        Err(error) => {
            assert_eq!(error.phase(), crate::TransportPhase::DispatchUncertain);
            assert_eq!(
                response.completion().await.unwrap_err().phase(),
                crate::TransportPhase::DispatchUncertain
            );
        }
    }
    assert_eq!(first.as_ref(), b"abc");
    let next = timeout(WAIT, response.recv()).await.unwrap();
    let final_read = match next {
        Ok(Some(item)) => {
            assert_eq!(item.as_ref(), b"xyzw");
            timeout(WAIT, response.recv()).await.unwrap()
        }
        terminal => terminal,
    };
    match final_read {
        Ok(None) => assert!(matches!(&completion, Ok(t) if t.status == CallStatus::Success)),
        Err(error) => {
            assert_eq!(error.phase(), crate::TransportPhase::DispatchUncertain);
            assert!(!matches!(&completion, Ok(t) if t.status == CallStatus::Success));
        }
        Ok(Some(item)) => panic!("unexpected extra item: {item:?}"),
    }
    drop(first);
    assert!(client.close_shared_bounded(WAIT).await);
    crate::tests::close_listener(listener).await;
}

#[tokio::test]
async fn not_executed_after_item_begin_is_never_retry_safe() {
    bad_terminal(2).await;
}

#[tokio::test]
async fn chunked_stream_open_prefers_early_not_executed_over_upload_failure() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let (first_seen, first_received) = oneshot::channel();
    let (send_terminal, terminal_requested) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let mut socket = accept(&mut listener).await;
        let (open, payload) = read_frame(&mut socket).await;
        assert_eq!(
            open.flags,
            flags::FLAG_RPC_RECORD | flags::FLAG_CHUNKED | flags::FLAG_FIRST_CHUNK
        );
        assert!(matches!(
            decode_rpc_record(&open, &payload, None).unwrap(),
            RpcRecord::Open {
                value: ValuePart {
                    chunk: ChunkPosition::First { total_bytes: 8192 },
                    ..
                },
                ..
            }
        ));
        first_seen.send(open.request_id).unwrap();
        terminal_requested.await.unwrap();
        let error = C2Error::new(ErrorCode::ResourceRemoved, "route removed").to_wire_bytes();
        socket
            .write_all(
                &encode_call_end_frame(
                    open.request_id,
                    CallMode::ServerStream,
                    CallStatus::NotExecuted,
                    0,
                    ValuePart::whole(ValueStorage::Inline(&error)),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        drop(socket);
        crate::tests::close_listener(listener).await;
    });

    let mut cfg = config();
    cfg.base.shm_backing_budget_bytes = 0;
    let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
    client.connect().await.unwrap();
    let client = Arc::new(client);
    let (first_paused, first_ready) = oneshot::channel();
    let (resume_first, first_resume) = oneshot::channel();
    client.set_write_pause_for_test(Some((first_paused, first_resume)));
    let caller = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .stream_bound(
                    &large_binding(),
                    "items",
                    &[7; 8192],
                    PayloadBufferBudget::new(8192),
                )
                .await
        }
    });
    first_ready.await.unwrap();
    let (second_paused, second_ready) = oneshot::channel();
    let (resume_second, second_resume) = oneshot::channel();
    client.set_write_pause_for_test(Some((second_paused, second_resume)));
    resume_first.send(()).unwrap();
    let rid = first_received.await.unwrap();
    second_ready.await.unwrap();
    let mut upload_done = client
        .pending
        .lock()
        .get(&rid)
        .and_then(|entry| entry.upload_stop.as_ref())
        .expect("stream upload owns its response notification")
        .subscribe();
    send_terminal.send(()).unwrap();
    // The existing upload notification follows its result-channel send, so
    // this barrier places the decoded refusal before the paused write resumes.
    timeout(WAIT, upload_done.changed())
        .await
        .expect("first-chunk refusal must reach the response reader")
        .unwrap();
    assert!(*upload_done.borrow());
    resume_second.send(()).unwrap();
    let result = timeout(WAIT, caller).await.unwrap().unwrap();
    let error = match result {
        Ok(_) => panic!("NotExecuted must be returned from stream_bound"),
        Err(error) => error,
    };
    assert_eq!(error.phase(), crate::TransportPhase::PreDispatch);
    assert!(matches!(
        error.source_error(),
        IpcError::CallRejected(rejection) if rejection.code == ErrorCode::ResourceRemoved
    ));
    peer.await.unwrap();
}

#[tokio::test]
async fn early_failed_end_keeps_complete_item_in_original_stream() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let (first_seen, first_received) = oneshot::channel();
    let (send_terminal, terminal_requested) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let mut socket = accept(&mut listener).await;
        let (open, _) = read_frame(&mut socket).await;
        assert_eq!(
            open.flags,
            flags::FLAG_RPC_RECORD | flags::FLAG_CHUNKED | flags::FLAG_FIRST_CHUNK
        );
        first_seen.send(open.request_id).unwrap();
        terminal_requested.await.unwrap();
        socket
            .write_all(&encode_item_begin_frame(open.request_id, 0).unwrap())
            .await
            .unwrap();
        let error = C2Error::new(ErrorCode::CallCancelled, "cleanup cancelled").to_wire_bytes();
        socket
            .write_all(
                &encode_call_end_frame(
                    open.request_id,
                    CallMode::ServerStream,
                    CallStatus::Failed,
                    1,
                    ValuePart::whole(ValueStorage::Inline(&error)),
                )
                .unwrap(),
            )
            .await
            .unwrap();
        drop(socket);
        crate::tests::close_listener(listener).await;
    });

    let mut cfg = config();
    cfg.base.shm_backing_budget_bytes = 0;
    let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
    client.connect().await.unwrap();
    let client = Arc::new(client);
    let (first_paused, first_ready) = oneshot::channel();
    let (resume_first, first_resume) = oneshot::channel();
    client.set_write_pause_for_test(Some((first_paused, first_resume)));
    let caller = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .stream_bound(
                    &large_binding(),
                    "items",
                    &[7; 8192],
                    PayloadBufferBudget::new(8192),
                )
                .await
        }
    });
    first_ready.await.unwrap();
    let (second_paused, second_ready) = oneshot::channel();
    let (resume_second, second_resume) = oneshot::channel();
    client.set_write_pause_for_test(Some((second_paused, second_resume)));
    resume_first.send(()).unwrap();
    let rid = first_received.await.unwrap();
    second_ready.await.unwrap();
    let mut upload_done = client
        .pending
        .lock()
        .get(&rid)
        .and_then(|entry| entry.upload_stop.as_ref())
        .expect("stream upload owns its response notification")
        .subscribe();
    send_terminal.send(()).unwrap();
    timeout(WAIT, upload_done.changed()).await.unwrap().unwrap();
    assert!(*upload_done.borrow());
    resume_second.send(()).unwrap();

    let mut response = timeout(WAIT, caller)
        .await
        .unwrap()
        .unwrap()
        .expect("Failed END retains the original stream and its queued item");
    let item = timeout(WAIT, response.recv())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(item.as_ref(), b"");
    let error = timeout(WAIT, response.recv()).await.unwrap().unwrap_err();
    assert_eq!(error.phase(), crate::TransportPhase::DispatchUncertain);
    assert!(matches!(
        response.completion().await,
        Ok(StreamTerminal { status: CallStatus::Failed, complete_items: 1, error: Some(reason) })
            if reason.code == ErrorCode::CallCancelled
    ));
    // A failed terminal stays a failure on repeated observations; only a
    // natural Success END can yield None.
    assert_eq!(
        response.recv().await.unwrap_err().phase(),
        crate::TransportPhase::DispatchUncertain
    );
    peer.await.unwrap();
}
