//! Readable stream failures stay local to the original call and its owners.
use super::*;
use crate::StreamTerminal;
use c2_error::{C2Error, ErrorCode};
use c2_mem::retention::PayloadBufferBudget;
use c2_wire::rpc::{encode_credit_frame, encode_item_begin_frame, encode_item_data_frame};

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
    assert!(response.recv().await.unwrap().is_none());
    peer.await.unwrap();
}
