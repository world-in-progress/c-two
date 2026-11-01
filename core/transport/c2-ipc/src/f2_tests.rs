use super::*;
use c2_wire::chunk::header::encode_reply_chunk_meta;
use c2_wire::release::{ReleaseBatch, decode_release_frame, encode_release_frame};

#[derive(Clone, Copy, Debug)]
enum ReplyFault {
    Geometry(u64, u32, usize),
    ShortMetadata,
    ChangedMetadata,
    DuplicateFirst,
    DuplicateLater,
    Oversized,
    LocalOpen,
}

fn chunk(rid: u64, size: u64, count: u32, index: u32, data: &[u8]) -> Vec<u8> {
    frame::encode_frame_parts(
        rid,
        flags::FLAG_RESPONSE | flags::FLAG_REPLY_V2 | flags::FLAG_CHUNKED,
        &[&encode_reply_chunk_meta(size, count, index), data],
    )
}

// Both requests really cross the OS stream before the broken reply arrives.
// The peer withholds the second reply until after the first caller failed.
async fn isolated_reply_failure(fault: ReplyFault) {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let (seen, request_seen) = oneshot::channel();
    let (ready, assembly_ready) = oneshot::channel();
    let (break_reply, break_requested) = oneshot::channel();
    let (finish_other, other_requested) = oneshot::channel();
    let partial = !matches!(fault, ReplyFault::Geometry(..) | ReplyFault::LocalOpen);
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let (target, _) = read_frame(&mut stream).await;
        seen.send(()).unwrap();
        let (other, _) = read_frame(&mut stream).await;
        if partial {
            stream
                .write_all(&chunk(target.request_id, 1536, 3, 0, &[1; 512]))
                .await
                .unwrap();
        }
        ready.send(()).unwrap();
        break_requested.await.unwrap();
        let bad = match fault {
            ReplyFault::Geometry(size, count, len) => {
                chunk(target.request_id, size, count, 0, &vec![1; len])
            }
            ReplyFault::ShortMetadata => frame::encode_frame(
                target.request_id,
                flags::FLAG_RESPONSE | flags::FLAG_REPLY_V2 | flags::FLAG_CHUNKED,
                &[0; 8],
            ),
            ReplyFault::ChangedMetadata => chunk(target.request_id, 2048, 4, 1, &[2; 512]),
            ReplyFault::DuplicateFirst => chunk(target.request_id, 1536, 3, 0, &[2; 512]),
            ReplyFault::DuplicateLater => {
                stream
                    .write_all(&chunk(target.request_id, 1536, 3, 1, &[2; 512]))
                    .await
                    .unwrap();
                chunk(target.request_id, 1536, 3, 1, &[3; 512])
            }
            ReplyFault::Oversized => chunk(target.request_id, 1536, 3, 1, &[2; 1024]),
            ReplyFault::LocalOpen => frame::encode_frame_parts(
                target.request_id,
                flags::FLAG_RESPONSE | flags::FLAG_REPLY_V2 | flags::FLAG_BUDDY,
                &[
                    &encode_buddy_payload(&BuddyPayload {
                        seg_idx: 0,
                        generation: 1,
                        offset: 0,
                        data_size: 512,
                        is_dedicated: false,
                        loan_id: 77,
                    }),
                    &[0],
                ],
            ),
        };
        stream.write_all(&bad).await.unwrap();
        other_requested.await.unwrap();
        if partial {
            for index in 0..3 {
                stream
                    .write_all(&chunk(other.request_id, 1536, 3, index, &[9; 512]))
                    .await
                    .unwrap();
            }
        } else {
            reply(&mut stream, other.request_id, b"other completes").await;
        }
        // The failed descriptor's release precedes the subsequent business
        // call. It must be queued once even when the local open failed.
        let mut released = Vec::new();
        let ping = loop {
            let (header, payload) = read_frame(&mut stream).await;
            if header.is_ctrl() {
                released.extend_from_slice(
                    decode_release_frame(&header, &payload, false, true)
                        .unwrap()
                        .ids(),
                );
            } else {
                break header;
            }
        };
        assert_eq!(
            released,
            if matches!(fault, ReplyFault::LocalOpen) {
                vec![77]
            } else {
                vec![]
            }
        );
        reply(&mut stream, ping.request_id, b"pong").await;
        expect_disconnect_or_eof(&mut stream).await;
        crate::tests::close_listener(listener).await;
    });
    let mut cfg = config();
    cfg.base.shm_backing_budget_bytes = 0;
    if matches!(fault, ReplyFault::Geometry(..)) {
        cfg.base.live_reassembly_budget_bytes = 0;
    } else if partial {
        cfg.base.live_reassembly_budget_bytes = 1536 + 3;
    }
    let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
    client.connect().await.unwrap();
    if matches!(fault, ReplyFault::LocalOpen) {
        client
            .server_pool
            .lock()
            .as_ref()
            .unwrap()
            .write()
            .open_failure = Some("injected EMFILE/ENOMEM opening peer mapping".into());
    }
    let connection = client.driver.read().clone().unwrap();
    let client = Arc::new(client);
    let target = tokio::spawn({
        let client = client.clone();
        async move { client.call_inline(&identity(), 0, b"target").await }
    });
    request_seen.await.unwrap();
    let other = tokio::spawn({
        let client = client.clone();
        async move { client.call_inline(&identity(), 0, b"other").await }
    });
    assembly_ready.await.unwrap();
    if partial {
        eventually(|| client.memory_budget.snapshot().reassembly.used_bytes == 1536 + 3).await;
    }
    assert_eq!(client.pending_len_for_test(), 2);
    break_reply.send(()).unwrap();
    let error = timeout(WAIT, target).await.unwrap().unwrap().unwrap_err();
    assert_eq!(
        error.phase(),
        crate::TransportPhase::DispatchUncertain,
        "{fault:?}: {error}"
    );
    match error.source_error() {
        IpcError::ResponseFailure(source) => match (fault, source.as_ref()) {
            (ReplyFault::LocalOpen, IpcError::Shm(message)) => {
                assert!(message.contains("injected EMFILE/ENOMEM"))
            }
            (_, IpcError::Chunk(ChunkError::Protocol(_))) => {}
            _ => panic!("unexpected correlated error for {fault:?}: {error}"),
        },
        _ => panic!("missing response phase for {fault:?}: {error}"),
    }
    assert!(client.is_connected(), "{fault:?}");
    assert_eq!(
        client.pending_len_for_test(),
        1,
        "the other sent call is still waiting"
    );
    assert!(!other.is_finished());
    assert_eq!(client.memory_budget.snapshot().reassembly.used_bytes, 0);
    assert_eq!(
        client
            .memory_budget
            .snapshot()
            .reassembly
            .rejected_allocations,
        0
    );
    finish_other.send(()).unwrap();
    let expected = if partial {
        vec![9; 1536]
    } else {
        b"other completes".to_vec()
    };
    assert_eq!(
        timeout(WAIT, other)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_owned_bytes()
            .unwrap(),
        expected
    );
    assert_eq!(
        client
            .call_inline(&identity(), 0, b"ping")
            .await
            .unwrap()
            .into_owned_bytes()
            .unwrap(),
        b"pong"
    );
    assert!(Arc::ptr_eq(
        client.driver.read().as_ref().unwrap(),
        &connection
    ));
    assert_eq!(client.pending_len_for_test(), 0);
    assert_eq!(client.memory_budget.snapshot().reassembly.used_bytes, 0);
    assert!(client.close_shared_bounded(WAIT).await);
    timeout(WAIT, peer).await.unwrap().unwrap();
}

#[tokio::test]
async fn encoded_invalid_reply_geometry_is_protocol_and_ping_stays_usable() {
    for (size, count, len) in [
        (128, 0, 64),
        (0, 1, 1),
        (0, 2, 64),
        (128, 2, 0),
        (128, 1, 0),
        (128, 1, 129),
    ] {
        isolated_reply_failure(ReplyFault::Geometry(size, count, len)).await;
    }
}

#[tokio::test]
async fn malformed_later_chunk_metadata_aborts_assembly_and_keeps_connection_usable() {
    for fault in [ReplyFault::ShortMetadata, ReplyFault::ChangedMetadata] {
        isolated_reply_failure(fault).await;
    }
}

#[tokio::test]
async fn duplicate_later_chunk_aborts_assembly_and_releases_budget_once() {
    isolated_reply_failure(ReplyFault::DuplicateLater).await;
}

#[tokio::test]
async fn duplicate_first_chunk_aborts_existing_assembly_and_releases_budget_once() {
    isolated_reply_failure(ReplyFault::DuplicateFirst).await;
}

#[tokio::test]
async fn oversized_later_chunk_aborts_assembly_and_releases_budget_once() {
    isolated_reply_failure(ReplyFault::Oversized).await;
}

#[tokio::test]
async fn local_peer_mapping_open_failure_releases_descriptor_and_preserves_other_calls() {
    isolated_reply_failure(ReplyFault::LocalOpen).await;
}

#[tokio::test]
async fn control_violations_deactivate_but_sent_inline_and_chunked_calls_finish() {
    // Zero, unknown, duplicate-in-batch and previously released IDs. A real
    // file-backed loan proves partial settlement without requiring SHM here.
    for fault in 0..5 {
        let endpoint = Endpoint::new();
        let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
        let (loan_tx, loan_rx) = oneshot::channel();
        let (seen, requests_seen) = oneshot::channel();
        let (finish, finish_requested) = oneshot::channel();
        let peer = tokio::spawn(async move {
            let mut stream = accept(&mut listener).await;
            let (first, _) = read_frame(&mut stream).await;
            let (second, _) = read_frame(&mut stream).await;
            let loan = loan_rx.await.unwrap();
            if fault == 3 {
                stream
                    .write_all(&encode_release_frame(
                        &ReleaseBatch::new(&[loan]).unwrap(),
                        true,
                    ))
                    .await
                    .unwrap();
            }
            let ids = match fault {
                0 => vec![0],
                1 => vec![loan + 1],
                2 => vec![loan, loan],
                _ => vec![loan],
            };
            let invalid = if fault == 4 {
                frame::encode_frame(0, flags::FLAG_SIGNAL, &[0xff])
            } else {
                encode_release_frame(&ReleaseBatch::new(&ids).unwrap(), true)
            };
            stream.write_all(&invalid).await.unwrap();
            seen.send(()).unwrap();
            finish_requested.await.unwrap();
            reply(&mut stream, first.request_id, b"first").await;
            stream
                .write_all(&chunk(second.request_id, 1024, 2, 0, &[2; 512]))
                .await
                .unwrap();
            stream
                .write_all(&chunk(second.request_id, 1024, 2, 1, &[3; 512]))
                .await
                .unwrap();
            // For the first two cases the original published loan remains
            // live through both replies and is settled only by this record.
            if fault < 2 || fault == 4 {
                stream
                    .write_all(&encode_release_frame(
                        &ReleaseBatch::new(&[loan]).unwrap(),
                        true,
                    ))
                    .await
                    .unwrap();
            }
            expect_disconnect_or_eof(&mut stream).await;
            crate::tests::close_listener(listener).await;
        });
        let mut client = endpoint.client();
        client.connect().await.unwrap();
        let conn = client.driver.read().clone().unwrap();
        let (loan, _pool, budget) = publish_file_loan(&conn, endpoint._root.path());
        loan_tx.send(loan).unwrap();
        let client = Arc::new(client);
        let first = tokio::spawn({
            let client = client.clone();
            async move { client.call_inline(&identity(), 0, b"first").await }
        });
        let second = tokio::spawn({
            let client = client.clone();
            async move { client.call_inline(&identity(), 0, b"second").await }
        });
        requests_seen.await.unwrap();
        eventually(|| !client.is_connected()).await;
        assert_eq!(
            client.pending_len_for_test(),
            2,
            "a release violation must preserve sent waiters"
        );
        assert!(!first.is_finished() && !second.is_finished());
        assert_eq!(
            budget.snapshot().file.used_bytes > 0,
            fault < 2 || fault == 4
        );
        let rejected = client
            .call_inline(&identity(), 0, b"new")
            .await
            .unwrap_err();
        assert_eq!(rejected.phase(), crate::TransportPhase::PreDispatch);
        finish.send(()).unwrap();
        let (first, second) = timeout(WAIT, async { tokio::join!(first, second) })
            .await
            .unwrap();
        let mut replies = vec![
            first.unwrap().unwrap().into_owned_bytes().unwrap(),
            second.unwrap().unwrap().into_owned_bytes().unwrap(),
        ];
        replies.sort_by_key(Vec::len);
        assert_eq!(replies[0], b"first");
        assert_eq!(replies[1], [&[2; 512][..], &[3; 512][..]].concat());
        eventually(|| budget.snapshot().file.used_bytes == 0).await;
        assert!(client.close_shared_bounded(WAIT).await);
        timeout(WAIT, peer).await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn peer_views_retire_after_the_last_carrier_and_idle_gc() {
    for dedicated in [false, true] {
        let endpoint = Endpoint::new();
        let (server, runner) = start(
            &endpoint,
            Arc::new(Echo),
            ServerIpcConfig {
                shm_threshold: 1,
                base: c2_config::BaseIpcConfig {
                    pool_enabled: !dedicated,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await;
        let mut cfg = config();
        cfg.pool_decay_seconds = 1.0;
        cfg.base.chunk_gc_interval_secs = 0.005;
        let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
        client.connect().await.unwrap();
        let peer = client.server_pool.lock().clone().unwrap();
        let binding = client.acquire_route(&contract()).await.unwrap();
        let held = client
            .call_bound(&binding, "echo", &[9; 8193])
            .await
            .unwrap()
            .into_owned_bytes()
            .unwrap();
        let stats = server.response_pool_arc().read().stats();
        assert!(
            if dedicated {
                stats.dedicated_allocs > 0
            } else {
                stats.buddy_expanded_allocs + stats.buddy_reused_allocs > 0
            },
            "real SHM response required: {stats:?}"
        );
        assert_eq!(
            peer.read().cache.mapped_segments(),
            if dedicated { 0 } else { 1 }
        );
        eventually(|| peer.read().cache.mapped_segments() == 0).await;
        assert!(client.is_connected());
        assert_eq!(held, vec![9; 8193]);
        assert_eq!(
            client
                .call_bound(&binding, "echo", &[8; 8193])
                .await
                .unwrap()
                .into_owned_bytes()
                .unwrap(),
            vec![8; 8193],
            "a retired peer view can be reopened on the same physical connection"
        );
        assert_eq!(held, vec![9; 8193]);
        assert!(client.close_shared_bounded(WAIT).await);
        stop(server, runner).await;
    }
}

#[tokio::test]
async fn subnanosecond_gc_interval_keeps_the_five_millisecond_floor() {
    let endpoint = Endpoint::new();
    let (server, runner) = start(&endpoint, Arc::new(Echo), ServerIpcConfig::default()).await;
    let mut cfg = config();
    cfg.base.chunk_gc_interval_secs = 1e-12; // Duration rounds to zero.
    cfg.pool_decay_seconds = 0.0;
    let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
    client.connect().await.unwrap();
    let pool = client.request_pool().unwrap();
    pool.write().ensure_buddy_segments(1).unwrap();
    eventually(|| pool.read().segment_count() == 0).await;
    assert!(client.is_connected());
    assert!(client.close_shared_bounded(WAIT).await);
    stop(server, runner).await;
}

#[tokio::test]
async fn caller_connect_deadline_can_exceed_the_default_handshake_guard() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let peer = tokio::spawn(async move {
        let mut stream = listener.accept().await.unwrap();
        let mut hello = [0; 25];
        stream.read_exact(&mut hello).await.unwrap();
        // A longer explicit caller budget must override the old five-second
        // guard, while covering the preamble and metadata as one attempt.
        tokio::time::sleep(DEFAULT_CONNECT_TIMEOUT + Duration::from_millis(250)).await;
        stream
            .write_all(&c2_wire::handshake::encode_version_preamble(
                VersionPreambleKind::Accepted,
            ))
            .await
            .unwrap();
        read_frame(&mut stream).await;
        let hs = encode_server_handshake(
            0,
            &[] as &[RouteInfo],
            "/f2handshake",
            &ServerIdentity {
                server_id: "f2-peer".to_string(),
                server_instance_id: "f2-instance".to_string(),
            },
        )
        .unwrap();
        stream
            .write_all(&frame::encode_frame(
                0,
                flags::FLAG_HANDSHAKE | flags::FLAG_RESPONSE,
                &hs,
            ))
            .await
            .unwrap();
        expect_disconnect_or_eof(&mut stream).await;
        crate::tests::close_listener(listener).await;
    });
    let mut client = endpoint.client();
    let deadline = ConnectDeadline::start(
        c2_config::ConnectOptions::new().with_timeout(Duration::from_secs(10)),
    )
    .unwrap();
    client.connect_with_deadline(deadline).await.unwrap();
    assert!(client.is_connected());
    assert!(client.close_shared_bounded(WAIT).await);
    timeout(WAIT, peer).await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_write_timeout_preserves_a_sent_calls_readable_reply() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let (seen, request_seen) = oneshot::channel();
    let (prefix, prefix_written) = oneshot::channel();
    let (other_reply, reply_requested) = oneshot::channel();
    let (drain, drain_requested) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let (other, _) = read_frame(&mut stream).await;
        seen.send(()).unwrap();
        let mut header = [0; HEADER_SIZE];
        stream.read_exact(&mut header[..8]).await.unwrap();
        prefix.send(()).unwrap();
        reply_requested.await.unwrap();
        reply(&mut stream, other.request_id, b"other survives").await;
        drain_requested.await.unwrap();
        stream.read_exact(&mut header[8..]).await.unwrap();
        let (length, body) = frame::decode_total_len(&header).unwrap();
        let (header, _) = frame::decode_frame_body(body, length).unwrap();
        let mut payload = vec![0; header.payload_len()];
        stream.read_exact(&mut payload).await.unwrap();
        let (_, consumed) = c2_wire::control::decode_call_control(&payload, 0).unwrap();
        assert_eq!(&payload[consumed..], vec![7; 8 * 1024 * 1024]);
        expect_disconnect_or_eof(&mut stream).await;
        crate::tests::close_listener(listener).await;
    });
    let mut cfg = config();
    cfg.base.write_timeout_secs = 0.05;
    let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
    client.connect().await.unwrap();
    let client = Arc::new(client);
    let other = tokio::spawn({
        let client = client.clone();
        async move { client.call_inline(&identity(), 0, b"other").await }
    });
    request_seen.await.unwrap();
    let target = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .call_inline(&identity(), 0, &vec![7; 8 * 1024 * 1024])
                .await
        }
    });
    prefix_written.await.unwrap();
    let error = timeout(Duration::from_secs(2), target)
        .await
        .expect("configured 50ms write timeout, not the 30s default")
        .unwrap()
        .unwrap_err();
    assert_eq!(error.phase(), crate::TransportPhase::DispatchUncertain);
    assert!(
        matches!(error.source_error(), IpcError::Io(error) if error.kind() == std::io::ErrorKind::TimedOut)
    );
    assert!(!client.is_connected());
    assert_eq!(client.pending_len_for_test(), 1);
    assert!(!other.is_finished());
    other_reply.send(()).unwrap();
    assert_eq!(
        timeout(WAIT, other)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_owned_bytes()
            .unwrap(),
        b"other survives"
    );
    drain.send(()).unwrap();
    assert!(client.close_shared_bounded(WAIT).await);
    timeout(WAIT, peer).await.unwrap().unwrap();
}

#[tokio::test]
async fn untrusted_frame_boundary_fails_all_sent_calls_and_waits_for_peer_end() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let (seen, request_seen) = oneshot::channel();
    let (exit, exit_requested) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        read_frame(&mut stream).await;
        seen.send(()).unwrap();
        read_frame(&mut stream).await;
        let mut invalid = [0; HEADER_SIZE];
        invalid[..4].copy_from_slice(&4u32.to_le_bytes());
        stream.write_all(&invalid).await.unwrap();
        exit_requested.await.unwrap();
        drop(stream);
        crate::tests::close_listener(listener).await;
    });
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let conn = client.driver.read().clone().unwrap();
    let (_, _pool, budget) = publish_file_loan(&conn, endpoint._root.path());
    let client = Arc::new(client);
    let first = tokio::spawn({
        let client = client.clone();
        async move { client.call_inline(&identity(), 0, b"first").await }
    });
    request_seen.await.unwrap();
    let second_identity = identity();
    let second = client.call_inline(&second_identity, 0, b"second");
    let (first, second) = timeout(WAIT, async { tokio::join!(first, second) })
        .await
        .unwrap();
    for error in [first.unwrap().unwrap_err(), second.unwrap_err()] {
        assert_eq!(error.phase(), crate::TransportPhase::DispatchUncertain);
    }
    assert!(!client.is_connected());
    assert_eq!(client.pending_len_for_test(), 0);
    assert!(
        budget.snapshot().file.used_bytes > 0,
        "a malformed frame proves no peer end"
    );
    exit.send(()).unwrap();
    eventually(|| budget.snapshot().file.used_bytes == 0).await;
    assert!(client.close_shared_bounded(WAIT).await);
    timeout(WAIT, peer).await.unwrap().unwrap();
}

#[tokio::test]
async fn zero_response_loan_deactivates_with_zero_shm_budget_and_preserves_other_sent_calls() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let (seen, request_seen) = oneshot::channel();
    let (finish, finish_requested) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let (target, _) = read_frame(&mut stream).await;
        seen.send(()).unwrap();
        let (other, _) = read_frame(&mut stream).await;
        let descriptor = encode_buddy_payload(&BuddyPayload {
            seg_idx: 0,
            generation: 1,
            offset: 0,
            data_size: 512,
            is_dedicated: false,
            loan_id: 0,
        });
        stream
            .write_all(&frame::encode_frame_parts(
                target.request_id,
                flags::FLAG_RESPONSE | flags::FLAG_REPLY_V2 | flags::FLAG_BUDDY,
                &[&descriptor, &[0]],
            ))
            .await
            .unwrap();
        finish_requested.await.unwrap();
        reply(&mut stream, other.request_id, b"other survives").await;
        expect_disconnect_or_eof(&mut stream).await;
        crate::tests::close_listener(listener).await;
    });
    let mut cfg = config();
    cfg.base.shm_backing_budget_bytes = 0;
    let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
    client.connect().await.unwrap();
    let client = Arc::new(client);
    let target = tokio::spawn({
        let client = client.clone();
        async move { client.call_inline(&identity(), 0, b"target").await }
    });
    request_seen.await.unwrap();
    let other = tokio::spawn({
        let client = client.clone();
        async move { client.call_inline(&identity(), 0, b"other").await }
    });
    let error = timeout(WAIT, target).await.unwrap().unwrap().unwrap_err();
    assert_eq!(error.phase(), crate::TransportPhase::DispatchUncertain);
    assert!(
        matches!(error.source_error(), IpcError::ResponseFailure(error)
        if matches!(error.as_ref(), IpcError::Decode(DecodeError::InvalidValue { field: "loan id", value: 0 })))
    );
    assert!(!client.is_connected());
    assert_eq!(client.pending_len_for_test(), 1);
    assert!(!other.is_finished());
    finish.send(()).unwrap();
    assert_eq!(
        timeout(WAIT, other)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_owned_bytes()
            .unwrap(),
        b"other survives"
    );
    assert!(client.close_shared_bounded(WAIT).await);
    timeout(WAIT, peer).await.unwrap().unwrap();
}

#[tokio::test]
async fn early_chunk_rejection_stops_at_the_frame_boundary_and_preserves_the_connection() {
    for partial_frame in [false, true] {
        let endpoint = Endpoint::new();
        let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
        let (seen, first_seen) = oneshot::channel();
        let peer = tokio::spawn(async move {
            let mut stream = accept(&mut listener).await;
            let (first, _) = read_frame(&mut stream).await;
            assert!(flags::is_chunked(first.flags));
            seen.send(()).unwrap();
            let mut header = [0; HEADER_SIZE];
            if partial_frame {
                stream.read_exact(&mut header[..8]).await.unwrap();
            }
            stream
                .write_all(&frame::encode_frame(
                    first.request_id,
                    flags::FLAG_RESPONSE | flags::FLAG_REPLY_V2,
                    &encode_reply_control(&ReplyControl::Error(b"early rejection".to_vec())),
                ))
                .await
                .unwrap();
            if partial_frame {
                stream.read_exact(&mut header[8..]).await.unwrap();
                let (length, body) = frame::decode_total_len(&header).unwrap();
                let (second, _) = frame::decode_frame_body(body, length).unwrap();
                assert_eq!(second.request_id, first.request_id);
                let mut payload = vec![0; second.payload_len()];
                stream.read_exact(&mut payload).await.unwrap();
                let (index, count, consumed) =
                    c2_wire::chunk::header::decode_chunk_header(&payload, 0).unwrap();
                assert_eq!((index, count), (1, 3));
                assert_eq!(&payload[consumed..], &[2; 4096]);
            }
            let (next, _) = read_frame(&mut stream).await;
            assert_ne!(
                next.request_id, first.request_id,
                "no later chunk may be written after the early reply"
            );
            assert!(!flags::is_chunked(next.flags));
            reply(&mut stream, next.request_id, b"same connection").await;
            expect_disconnect_or_eof(&mut stream).await;
            crate::tests::close_listener(listener).await;
        });
        let mut client = endpoint.client();
        client.connect().await.unwrap();
        let connection = client.driver.read().clone().unwrap();
        let client = Arc::new(client);
        let (body, source) = tokio::sync::mpsc::channel(3);
        body.send(vec![1; 4096]).await.unwrap();
        let source = futures_util::stream::unfold(source, |mut source| async move {
            source
                .recv()
                .await
                .map(|data| (Ok::<_, std::io::Error>(data), source))
        });
        let call = tokio::spawn({
            let client = client.clone();
            async move {
                client
                    .call_chunked_stream_phased(&identity(), 0, 3 * 4096, source)
                    .await
            }
        });
        first_seen.await.unwrap();
        if partial_frame {
            let (entered, paused) = oneshot::channel();
            let (resume, resumed) = oneshot::channel();
            client.set_write_prefix_pause_for_test(Some((entered, resumed)));
            body.send(vec![2; 4096]).await.unwrap();
            timeout(WAIT, paused).await.unwrap().unwrap();
            eventually(|| client.pending_len_for_test() == 0).await;
            assert!(
                !call.is_finished(),
                "finish the already-started frame before returning the server error"
            );
            resume.send(()).unwrap();
        }
        // The source stays open and never supplies the declared third chunk.
        // An early response must also wake a sender awaiting more source data.
        let error = timeout(WAIT, call).await.unwrap().unwrap().unwrap_err();
        assert_eq!(error.phase(), crate::TransportPhase::DispatchUncertain);
        assert!(
            matches!(error.source_error(), IpcError::CrmError(bytes) if bytes == b"early rejection")
        );
        assert!(client.is_connected());
        assert_eq!(
            client
                .call_inline(&identity(), 0, b"next")
                .await
                .unwrap()
                .into_owned_bytes()
                .unwrap(),
            b"same connection"
        );
        assert!(Arc::ptr_eq(
            client.driver.read().as_ref().unwrap(),
            &connection
        ));
        assert!(client.close_shared_bounded(WAIT).await);
        timeout(WAIT, peer).await.unwrap().unwrap();
    }
}
