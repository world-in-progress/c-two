use super::*;

#[tokio::test]
async fn incomplete_call_end_waits_for_peer_end_while_other_calls_finish() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let (first_seen, first_received) = oneshot::channel();
    let (end_peer, peer_can_end) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let (incomplete, _) = read_frame(&mut stream).await;
        first_seen.send(()).unwrap();
        let (running, _) = read_frame(&mut stream).await;
        let (queued, _) = read_frame(&mut stream).await;
        stream
            .write_all(&reply_chunk(incomplete.request_id, 2, 0, &[42; 4096]))
            .await
            .unwrap();
        reply(&mut stream, running.request_id, b"running result").await;
        reply(&mut stream, queued.request_id, b"queued result").await;
        // No second CALL_END for the incomplete value. The other responses are
        // complete records, so their outcomes are known before this real EOF.
        peer_can_end.await.unwrap();
        drop(stream);
        crate::tests::close_listener(listener).await;
    });
    let mut cfg = config();
    cfg.shm_threshold = u64::MAX;
    let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
    client.connect().await.unwrap();
    let client = Arc::new(client);
    let incomplete = tokio::spawn({
        let client = client.clone();
        async move { client.call_inline(&identity(), 0, b"incomplete").await }
    });
    first_received.await.unwrap();
    let call_identity = identity();
    let (running, queued) = timeout(WAIT, async {
        tokio::join!(
            client.call_inline(&call_identity, 0, b"running"),
            client.call_inline(&call_identity, 0, b"queued"),
        )
    })
    .await
    .unwrap();
    assert_eq!(
        running.unwrap().into_owned_bytes().unwrap(),
        b"running result"
    );
    assert_eq!(
        queued.unwrap().into_owned_bytes().unwrap(),
        b"queued result"
    );
    assert!(!incomplete.is_finished());
    assert_eq!(client.pending_len_for_test(), 1);
    end_peer.send(()).unwrap();
    let error = timeout(WAIT, incomplete)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert_eq!(error.phase(), crate::TransportPhase::DispatchUncertain);
    assert!(!error.is_retry_safe());
    assert_eq!(client.pending_len_for_test(), 0);
    assert!(!client.is_connected());
    peer.await.unwrap();
}

#[tokio::test]
async fn rejected_reply_discards_late_chunks_and_preserves_other_calls() {
    for count in [2] {
        let endpoint = Endpoint::new();
        let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
        let (first_seen, first_received) = oneshot::channel();
        let (send_unknown, unknown_requested) = oneshot::channel();
        let peer = tokio::spawn(async move {
            let mut stream = accept(&mut listener).await;
            let (rejected, _) = read_frame(&mut stream).await;
            first_seen.send(()).unwrap();
            let (other, _) = read_frame(&mut stream).await;
            let data = vec![7; 8192 / count as usize];
            stream
                .write_all(&reply_chunk(rejected.request_id, count, 0, &data))
                .await
                .unwrap();
            reply(&mut stream, other.request_id, b"other").await;
            for index in 1..count {
                stream
                    .write_all(&reply_chunk(rejected.request_id, count, index, &data))
                    .await
                    .unwrap();
            }
            let (after, _) = read_frame(&mut stream).await;
            reply(&mut stream, after.request_id, b"after").await;
            unknown_requested.await.unwrap();
            stream
                .write_all(&reply_chunk(rejected.request_id, count, 1, &data))
                .await
                .unwrap();
            let (tail, _) = read_frame(&mut stream).await;
            reply(&mut stream, tail.request_id, b"tail").await;
            expect_disconnect_or_eof(&mut stream).await;
            crate::tests::close_listener(listener).await;
        });
        let mut cfg = config();
        cfg.base.live_reassembly_budget_bytes = 0;
        let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
        client.connect().await.unwrap();
        let connection = client.driver.read().clone().unwrap();
        let client = Arc::new(client);
        let rejected = tokio::spawn({
            let client = client.clone();
            async move { client.call_inline(&identity(), 0, b"reject").await }
        });
        first_received.await.unwrap();
        let other_identity = identity();
        let other = client.call_inline(&other_identity, 0, b"other");
        let (rejected, other) = timeout(WAIT, async { tokio::join!(rejected, other) })
            .await
            .unwrap();
        let error = rejected.unwrap().unwrap_err();
        assert!(
            matches!(error.source_error(), IpcError::ResponseFailure(source)
            if matches!(source.as_ref(), IpcError::Chunk(ChunkError::Capacity(_))))
        );
        assert_eq!(error.phase(), crate::TransportPhase::DispatchUncertain);
        assert_eq!(other.unwrap().into_owned_bytes().unwrap(), b"other");
        assert!(client.is_connected());
        assert!(Arc::ptr_eq(
            client.driver.read().as_ref().unwrap(),
            &connection
        ));
        assert_eq!(
            client
                .call_inline(&identity(), 0, b"after")
                .await
                .unwrap()
                .into_owned_bytes()
                .unwrap(),
            b"after"
        );
        assert_eq!(client.pending_len_for_test(), 0);
        assert_eq!(client.memory_budget.snapshot().reassembly.used_bytes, 0);
        send_unknown.send(()).unwrap();
        assert_eq!(
            client
                .call_inline(&identity(), 0, b"tail")
                .await
                .unwrap()
                .into_owned_bytes()
                .unwrap(),
            b"tail"
        );
        assert!(client.is_connected());
        assert!(client.close_shared_bounded(WAIT).await);
        timeout(WAIT, peer).await.unwrap().unwrap();
    }
}

fn cached_binding(client: &IpcClient) -> RouteBinding {
    client.route_directory.write().insert_table(
        "grid".into(),
        MethodTable::from_entries(
            &[MethodEntry {
                name: "echo".into(),
                index: 0,
            }],
            identity(),
            u64::MAX,
        ),
    );
    client.bind_cached_route(&contract()).unwrap()
}

#[tokio::test]
async fn writer_assigns_monotonic_ids_after_input_preparation() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let (source_polled, source_waiting) = oneshot::channel();
    let (resume_source, source_resumed) = oneshot::channel();
    let (b_seen, b_received) = oneshot::channel();
    let (control_seen, control_received) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let (b, body) = read_frame(&mut stream).await;
        assert_eq!(b.flags, flags::FLAG_RPC_RECORD);
        assert!(matches!(
            decode_rpc_record(&b, &body, None).unwrap(),
            RpcRecord::Open {
                value: ValuePart {
                    storage: ValueStorage::Inline(b"B"),
                    ..
                },
                ..
            }
        ));
        reply(&mut stream, b.request_id, b"b done").await;
        b_seen.send(b.request_id).unwrap();

        let (control, body) = read_frame(&mut stream).await;
        assert_eq!(control.flags, flags::FLAG_CTRL);
        assert_eq!(body, b"order control");
        stream
            .write_all(&frame::encode_frame(
                control.request_id,
                flags::FLAG_CTRL | flags::FLAG_RESPONSE,
                b"control done",
            ))
            .await
            .unwrap();
        control_seen.send(control.request_id).unwrap();

        let (a, body) = read_frame(&mut stream).await;
        assert_eq!(a.flags, flags::FLAG_RPC_RECORD);
        assert!(matches!(
            decode_rpc_record(&a, &body, None).unwrap(),
            RpcRecord::Open {
                value: ValuePart {
                    storage: ValueStorage::Inline(b"A"),
                    ..
                },
                ..
            }
        ));
        assert!(b.request_id < control.request_id && control.request_id < a.request_id);
        reply(&mut stream, a.request_id, b"a done").await;
        expect_disconnect_or_eof(&mut stream).await;
        crate::tests::close_listener(listener).await;
        (b.request_id, control.request_id, a.request_id)
    });

    let mut cfg = config();
    cfg.shm_threshold = 1024;
    cfg.base.chunk_size = 1024;
    let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
    client.connect().await.unwrap();
    let binding = cached_binding(&client);
    let client = Arc::new(client);
    let a = tokio::spawn({
        let client = client.clone();
        async move {
            let source = futures_util::stream::once(async move {
                source_polled.send(()).unwrap();
                source_resumed.await.unwrap();
                Ok::<Vec<u8>, std::io::Error>(b"A".to_vec())
            });
            client
                .call_bound_sized_stream_phased(&binding, "echo", 1, source, None)
                .await
        }
    });
    timeout(WAIT, source_waiting).await.unwrap().unwrap();
    let b = tokio::spawn({
        let client = client.clone();
        async move { client.call_inline(&identity(), 0, b"B").await }
    });
    let b_id = timeout(WAIT, b_received).await.unwrap().unwrap();
    assert_eq!(
        b.await.unwrap().unwrap().into_owned_bytes().unwrap(),
        b"b done"
    );
    let control = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .send_control_inline(b"order control".to_vec(), "ordering")
                .await
        }
    });
    let control_id = timeout(WAIT, control_received).await.unwrap().unwrap();
    assert_eq!(control.await.unwrap().unwrap(), b"control done");
    resume_source.send(()).unwrap();
    assert_eq!(
        a.await.unwrap().unwrap().into_owned_bytes().unwrap(),
        b"a done"
    );
    assert!(client.close_shared_bounded(WAIT).await);
    let (observed_b, observed_control, observed_a) = peer.await.unwrap();
    assert_eq!((observed_b, observed_control), (b_id, control_id));
    assert!(observed_b < observed_control && observed_control < observed_a);
}

struct CountEcho(AtomicU64);
impl CrmCallback for CountEcho {
    fn invoke(
        &self,
        _: &str,
        _: u16,
        data: Vec<u8>,
        _: Arc<c2_server::ResponsePool>,
    ) -> Result<ResponseMeta, CrmError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(ResponseMeta::Inline(data))
    }
}

#[tokio::test]
async fn writer_rejection_preserves_error_and_connection_for_zero_inline_and_chunks() {
    let endpoint = Endpoint::new();
    let calls = Arc::new(CountEcho(AtomicU64::new(0)));
    let (server, runner) = start(&endpoint, calls.clone(), ServerIpcConfig::default()).await;
    let mut cfg = config();
    cfg.base.shm_backing_budget_bytes = 0;
    let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
    client.connect().await.unwrap();
    let binding = client.acquire_route(&contract()).await.unwrap();
    let expected =
        C2Error::new(ErrorCode::CallDeadlineExceeded, "writer rejected scope").with_details(
            std::collections::BTreeMap::from([("call_id".into(), "p4-41".into())]),
        );
    for data in [&[][..], &b"inline"[..], &vec![7; 8193][..]] {
        let grants = Arc::new(AtomicU64::new(0));
        let control = IpcCallControl::new({
            let expected = expected.clone();
            let grants = grants.clone();
            move || {
                grants.fetch_add(1, Ordering::SeqCst);
                Err(expected.clone())
            }
        })
        .with_before_prepare(|| Ok(()));
        let error = client
            .call_bound_controlled_phased(&binding, "echo", data, &control)
            .await
            .unwrap_err();
        assert_eq!(error.phase(), crate::TransportPhase::PreDispatch);
        assert!(
            matches!(error.into_source(), IpcError::LocalCallRejected(actual) if actual == expected)
        );
        assert_eq!(grants.load(Ordering::SeqCst), 1);
        assert_eq!(client.pending_len_for_test(), 0);
        assert_eq!(calls.0.load(Ordering::SeqCst), 0);
        assert!(client.is_connected());
    }
    assert_eq!(
        client
            .call_bound(&binding, "echo", b"healthy")
            .await
            .unwrap()
            .into_owned_bytes()
            .unwrap(),
        b"healthy"
    );
    assert_eq!(calls.0.load(Ordering::SeqCst), 1);
    assert!(client.close_shared_bounded(WAIT).await);
    stop(server, runner).await;
}

#[tokio::test]
async fn deactivation_distinguishes_unpublished_calls_from_started_dispatch() {
    for (controlled, data) in [false, true].into_iter().flat_map(|controlled| {
        [Vec::new(), b"queued".to_vec(), vec![3; 8193]]
            .into_iter()
            .map(move |data| (controlled, data))
    }) {
        let endpoint = Endpoint::new();
        let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
        let peer = tokio::spawn(async move {
            let mut stream = accept(&mut listener).await;
            let (request, payload) = read_frame(&mut stream).await;
            assert!(!request.is_buddy());
            assert!(matches!(
                decode_rpc_record(&request, &payload, None).unwrap(),
                RpcRecord::Open {
                    value: ValuePart {
                        storage: ValueStorage::Inline(b"first"),
                        ..
                    },
                    ..
                }
            ));
            reply(&mut stream, request.request_id, b"first").await;
            expect_disconnect_or_eof(&mut stream).await;
            crate::tests::close_listener(listener).await;
        });
        let mut cfg = config();
        cfg.base.shm_backing_budget_bytes = 0;
        let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
        client.connect().await.unwrap();
        let binding = cached_binding(&client);
        let client = Arc::new(client);
        let conn = client.driver.read().clone().unwrap();
        let (entered, paused) = oneshot::channel();
        let (resume, resumed) = oneshot::channel();
        *conn.pause.lock() = Some((entered, resumed));
        let first = tokio::spawn({
            let client = client.clone();
            let binding = binding.clone();
            async move { client.call_bound_phased(&binding, "echo", b"first").await }
        });
        paused.await.unwrap();
        let control = IpcCallControl::new(|| Ok(()));
        let queued = async {
            if controlled {
                client
                    .call_bound_controlled_phased(&binding, "echo", &data, &control)
                    .await
            } else {
                client.call_bound_phased(&binding, "echo", &data).await
            }
        };
        let mut queued = Box::pin(queued);
        assert!(futures_util::poll!(queued.as_mut()).is_pending());
        assert_eq!(client.pending_len_for_test(), 2);
        conn.end(
            ConnectionEndReason::LocalClose,
            "test_deactivate_before_dispatch",
        );
        assert_eq!(
            client.pending_len_for_test(),
            1,
            "unpolled queued waiter is removed on deactivation"
        );
        let queued_error = queued.await.unwrap_err();
        assert!(!first.is_finished());
        resume.send(()).unwrap();
        assert_eq!(
            timeout(WAIT, first)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .into_owned_bytes()
                .unwrap(),
            b"first"
        );
        assert!(client.close_shared_bounded(WAIT).await);
        peer.await.unwrap();
        assert!(matches!(queued_error.source_error(), IpcError::Closed));
        assert_eq!(queued_error.phase(), crate::TransportPhase::PreDispatch);
        assert_eq!(client.pending_len_for_test(), 0);
    }
}

#[tokio::test]
async fn deactivation_before_shm_dispatch_reports_pre_dispatch_and_releases_its_owner() {
    for dedicated in [false, true] {
        let endpoint = Endpoint::new();
        let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
        let peer = tokio::spawn(async move {
            let mut stream = accept(&mut listener).await;
            let (first, _) = read_frame(&mut stream).await;
            reply(&mut stream, first.request_id, b"first").await;
            expect_disconnect_or_eof(&mut stream).await;
            crate::tests::close_listener(listener).await;
        });
        let mut cfg = config();
        cfg.base.pool_enabled = !dedicated;
        let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
        client.connect().await.unwrap();
        let binding = cached_binding(&client);
        let pool = client.request_pool().unwrap();
        if !dedicated {
            pool.write()
                .ensure_buddy_segments(1)
                .expect("real request SHM is required");
        }
        let client = Arc::new(client);
        let conn = client.driver.read().clone().unwrap();
        let (entered, paused) = oneshot::channel();
        let (resume, resumed) = oneshot::channel();
        *conn.pause.lock() = Some((entered, resumed));
        let first = tokio::spawn({
            let client = client.clone();
            async move { client.call_inline(&identity(), 0, b"first").await }
        });
        paused.await.unwrap();
        let data = vec![3; 8193];
        let mut queued = Box::pin(client.call_bound_phased(&binding, "echo", &data));
        assert!(futures_util::poll!(queued.as_mut()).is_pending());
        assert_eq!(
            pool.read().stats().alloc_count,
            1,
            "the queued request must own actual SHM"
        );
        conn.end(
            ConnectionEndReason::LocalClose,
            "test_deactivate_before_shm_dispatch",
        );
        let error = queued.await.unwrap_err();
        assert_eq!(pool.read().stats().alloc_count, 0);
        assert!(!first.is_finished());
        resume.send(()).unwrap();
        assert_eq!(
            timeout(WAIT, first)
                .await
                .unwrap()
                .unwrap()
                .unwrap()
                .into_owned_bytes()
                .unwrap(),
            b"first"
        );
        assert!(client.close_shared_bounded(WAIT).await);
        peer.await.unwrap();
        assert!(matches!(error.source_error(), IpcError::Closed));
        assert_eq!(error.phase(), crate::TransportPhase::PreDispatch);
        assert_eq!(client.pending_len_for_test(), 0);
    }
}

#[tokio::test]
async fn writer_rejection_after_real_buddy_or_dedicated_preparation_releases_block() {
    for dedicated in [false, true] {
        let endpoint = Endpoint::new();
        let calls = Arc::new(CountEcho(AtomicU64::new(0)));
        let (server, runner) = start(&endpoint, calls.clone(), ServerIpcConfig::default()).await;
        let mut cfg = config();
        cfg.base.pool_enabled = !dedicated;
        let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
        client.connect().await.unwrap();
        let binding = client.acquire_route(&contract()).await.unwrap();
        let pool = client.request_pool().unwrap();
        if !dedicated {
            pool.write()
                .ensure_buddy_segments(1)
                .expect("real request SHM is required");
        }
        let saw_allocation = Arc::new(AtomicBool::new(false));
        let expected = C2Error::new(ErrorCode::CallDeadlineExceeded, "reject prepared SHM");
        let control = IpcCallControl::new({
            let pool = pool.clone();
            let saw_allocation = saw_allocation.clone();
            let expected = expected.clone();
            move || {
                let stats = pool.read().stats();
                assert_eq!(
                    stats.alloc_count, 1,
                    "writer must see a real SHM allocation: {stats:?}"
                );
                if dedicated {
                    assert!(
                        stats.dedicated_allocs > 0,
                        "dedicated allocation is required: {stats:?}"
                    );
                } else {
                    assert!(
                        stats.buddy_expanded_allocs + stats.buddy_reused_allocs > 0,
                        "buddy allocation is required: {stats:?}"
                    );
                }
                saw_allocation.store(true, Ordering::SeqCst);
                Err(expected.clone())
            }
        });
        let error = client
            .call_bound_controlled_phased(&binding, "echo", &[3; 8193], &control)
            .await
            .unwrap_err();
        assert_eq!(error.phase(), crate::TransportPhase::PreDispatch);
        assert!(
            matches!(error.into_source(), IpcError::LocalCallRejected(actual) if actual == expected)
        );
        assert!(
            saw_allocation.load(Ordering::SeqCst),
            "SHM transport must reach the writer"
        );
        assert_eq!(pool.read().stats().alloc_count, 0);
        assert_eq!(client.pending_len_for_test(), 0);
        assert_eq!(calls.0.load(Ordering::SeqCst), 0);
        assert_eq!(
            client
                .call_bound(&binding, "echo", b"ok")
                .await
                .unwrap()
                .into_owned_bytes()
                .unwrap(),
            b"ok"
        );
        assert_eq!(calls.0.load(Ordering::SeqCst), 1);
        assert!(client.close_shared_bounded(WAIT).await);
        stop(server, runner).await;
    }
}

#[tokio::test]
async fn expired_scope_after_first_chunk_does_not_recheck_dispatch_and_invokes_once() {
    let endpoint = Endpoint::new();
    let calls = Arc::new(CountEcho(AtomicU64::new(0)));
    let (server, runner) = start(&endpoint, calls.clone(), ServerIpcConfig::default()).await;
    let mut cfg = config();
    cfg.base.shm_backing_budget_bytes = 0;
    let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
    client.connect().await.unwrap();
    let binding = client.acquire_route(&contract()).await.unwrap();
    let expired = Arc::new(AtomicBool::new(false));
    let (method_idx, acquired_identity, _) = binding.call_target_for("echo").unwrap();
    let checks = Arc::new(AtomicU64::new(0));
    let control = IpcCallControl::new({
        let expired = expired.clone();
        let checks = checks.clone();
        move || {
            checks.fetch_add(1, Ordering::SeqCst);
            if expired.load(Ordering::SeqCst) {
                Err(C2Error::new(
                    ErrorCode::CallDeadlineExceeded,
                    "scope expired",
                ))
            } else {
                Ok(())
            }
        }
    });
    let (first_written, first_done) = oneshot::channel();
    let (more, remaining) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    let stream = futures_util::stream::unfold(
        (0u8, Some(first_written), remaining),
        |(step, signal, mut remaining)| {
            async move {
                if step == 0 {
                    Some((
                        Ok::<_, std::io::Error>(vec![1; 4096]),
                        (1, signal, remaining),
                    ))
                } else {
                    // The producer polls the next source item only after the first
                    // frame's write receipt, so this is a real post-publication gate.
                    if let Some(signal) = signal {
                        let _ = signal.send(());
                    }
                    remaining
                        .recv()
                        .await
                        .map(|bytes| (Ok(bytes), (step + 1, None, remaining)))
                }
            }
        },
    );
    let call = tokio::spawn({
        let client = Arc::new(client);
        let binding = binding.clone();
        let control = control.clone();
        async move {
            let result = client
                .call_chunked_stream_controlled(
                    &acquired_identity,
                    method_idx,
                    8193,
                    stream,
                    Some(control),
                )
                .await;
            (client, binding, result)
        }
    });
    timeout(WAIT, first_done).await.unwrap().unwrap();
    expired.store(true, Ordering::SeqCst);
    more.send(vec![2; 4096]).unwrap();
    more.send(vec![3]).unwrap();
    drop(more);
    let (client, binding, result) = timeout(WAIT, call).await.unwrap().unwrap();
    let expected = [vec![1; 4096], vec![2; 4096], vec![3]].concat();
    assert_eq!(result.unwrap().into_owned_bytes().unwrap(), expected);
    assert_eq!(checks.load(Ordering::SeqCst), 1);
    assert_eq!(calls.0.load(Ordering::SeqCst), 1);
    assert_eq!(client.pending_len_for_test(), 0);
    assert_eq!(
        client
            .call_bound(&binding, "echo", b"next")
            .await
            .unwrap()
            .into_owned_bytes()
            .unwrap(),
        b"next"
    );
    assert!(client.close_shared_bounded(WAIT).await);
    stop(server, runner).await;
}

#[tokio::test]
async fn source_failures_before_publication_leave_unrelated_calls_healthy() {
    for source_error in [true, false] {
        let endpoint = Endpoint::new();
        let calls = Arc::new(CountEcho(AtomicU64::new(0)));
        let (server, runner) = start(&endpoint, calls.clone(), ServerIpcConfig::default()).await;
        let mut cfg = config();
        cfg.base.shm_backing_budget_bytes = 0;
        let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
        client.connect().await.unwrap();
        let binding = client.acquire_route(&contract()).await.unwrap();
        let items: Vec<Result<Vec<u8>, std::io::Error>> = if source_error {
            vec![Err(std::io::Error::other("body source failed"))]
        } else {
            vec![Ok(vec![4; 17])]
        };
        let error = client
            .call_bound_sized_stream_phased(
                &binding,
                "echo",
                8193,
                futures_util::stream::iter(items),
                None,
            )
            .await
            .unwrap_err();
        assert_eq!(error.phase(), crate::TransportPhase::PreDispatch);
        assert!(matches!(
            error.into_source(),
            IpcError::Io(_) | IpcError::Config(_)
        ));
        assert_eq!(client.pending_len_for_test(), 0);
        assert_eq!(calls.0.load(Ordering::SeqCst), 0);
        assert_eq!(
            client
                .call_bound(&binding, "echo", b"other")
                .await
                .unwrap()
                .into_owned_bytes()
                .unwrap(),
            b"other"
        );
        assert_eq!(calls.0.load(Ordering::SeqCst), 1);
        assert!(client.close_shared_bounded(WAIT).await);
        stop(server, runner).await;
    }
}

#[tokio::test]
async fn source_error_after_first_chunk_is_uncertain_and_records_request_source() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let (first_frame, first_seen) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let (header, _) = read_frame(&mut stream).await;
        assert_ne!(header.flags & flags::FLAG_CHUNKED, 0);
        first_frame.send(()).unwrap();
        assert_eq!(
            timeout(WAIT, stream.read(&mut [0])).await.unwrap().unwrap(),
            0,
            "no replay or second request"
        );
        crate::tests::close_listener(listener).await;
    });
    let mut cfg = config();
    cfg.base.shm_backing_budget_bytes = 0;
    let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
    client.connect().await.unwrap();
    let binding = cached_binding(&client);
    let observation = client.connection_observation().unwrap();
    let (gate, opened) = oneshot::channel::<()>();
    let source = futures_util::stream::unfold((0u8, Some(opened)), |(step, gate)| async move {
        if step == 0 {
            Some((Ok::<_, std::io::Error>(vec![1; 4096]), (1, gate)))
        } else if step == 1 {
            gate.unwrap().await.unwrap();
            Some((Err(std::io::Error::other("late source failure")), (2, None)))
        } else {
            None
        }
    });
    let call = tokio::spawn(async move {
        let error = client
            .call_bound_sized_stream_phased(&binding, "echo", 8193, source, None)
            .await
            .unwrap_err();
        (client, error)
    });
    first_seen.await.unwrap();
    gate.send(()).unwrap();
    let (client, error) = timeout(WAIT, call).await.unwrap().unwrap();
    assert_eq!(error.phase(), crate::TransportPhase::DispatchUncertain);
    assert!(matches!(error.into_source(), IpcError::Io(_)));
    assert!(!client.is_connected());
    peer.await.unwrap();
    eventually(|| observation.snapshot().is_some()).await;
    let closed = observation.snapshot().unwrap();
    assert_eq!(closed.reason(), c2_local::ConnectionEndReason::IoError);
    assert_eq!(closed.operation(), "request_source");
    assert_eq!(client.pending_len_for_test(), 0);
}

#[tokio::test]
async fn queued_control_and_business_cancellation_remove_only_their_waiters() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let (first, _) = read_frame(&mut stream).await;
        reply(&mut stream, first.request_id, b"first").await;
        expect_disconnect_or_eof(&mut stream).await;
        crate::tests::close_listener(listener).await;
    });
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let client = Arc::new(client);
    let conn = client.driver.read().clone().unwrap();
    let (entered, paused) = oneshot::channel();
    let (resume, resumed) = oneshot::channel();
    *conn.pause.lock() = Some((entered, resumed));
    let first = tokio::spawn({
        let client = client.clone();
        async move { client.call_inline(&identity(), 0, b"first").await.unwrap() }
    });
    paused.await.unwrap();
    assert_eq!(client.pending_len_for_test(), 1);
    let queued_control = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .send_control_inline(b"queued control".to_vec(), "test")
                .await
        }
    });
    eventually(|| client.pending_len_for_test() == 2).await;
    queued_control.abort();
    assert!(queued_control.await.unwrap_err().is_cancelled());
    eventually(|| client.pending_len_for_test() == 1).await;
    let queued_business = tokio::spawn({
        let client = client.clone();
        async move { client.call_inline(&identity(), 0, b"queued business").await }
    });
    eventually(|| client.pending_len_for_test() == 2).await;
    queued_business.abort();
    assert!(queued_business.await.unwrap_err().is_cancelled());
    eventually(|| client.pending_len_for_test() == 1).await;
    assert!(!conn.core.is_deactivated());
    resume.send(()).unwrap();
    assert_eq!(first.await.unwrap().into_owned_bytes().unwrap(), b"first");
    assert_eq!(client.pending_len_for_test(), 0);
    assert!(client.close_shared_bounded(WAIT).await);
    peer.await.unwrap();
}

#[tokio::test]
async fn repeated_response_wait_cancellation_never_accumulates_pending_entries() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let (frames, mut seen) = tokio::sync::mpsc::unbounded_channel();
    let (reply_ready, mut may_reply) = tokio::sync::mpsc::unbounded_channel();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        for _ in 0..8 {
            let (header, _) = read_frame(&mut stream).await;
            frames.send(header.request_id).unwrap();
            may_reply.recv().await.unwrap();
            reply(&mut stream, header.request_id, b"late").await;
        }
        let (healthy, _) = read_frame(&mut stream).await;
        reply(&mut stream, healthy.request_id, b"healthy").await;
        expect_disconnect_or_eof(&mut stream).await;
        crate::tests::close_listener(listener).await;
    });
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let client = Arc::new(client);
    for index in 0..8 {
        let waiting = tokio::spawn({
            let client = client.clone();
            async move { client.call_inline(&identity(), 0, &[index]).await }
        });
        timeout(WAIT, seen.recv()).await.unwrap().unwrap();
        waiting.abort();
        assert!(waiting.await.unwrap_err().is_cancelled());
        assert_eq!(client.pending_len_for_test(), 1);
        reply_ready.send(()).unwrap();
        eventually(|| client.pending_len_for_test() == 0).await;
        assert!(client.is_connected());
    }
    assert_eq!(
        client
            .call_inline(&identity(), 0, b"healthy")
            .await
            .unwrap()
            .into_owned_bytes()
            .unwrap(),
        b"healthy"
    );
    assert_eq!(client.pending_len_for_test(), 0);
    assert!(client.close_shared_bounded(WAIT).await);
    peer.await.unwrap();
}

#[tokio::test]
async fn started_control_send_survives_caller_cancellation_and_keeps_connection() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let (control, payload) = read_frame(&mut stream).await;
        assert_ne!(control.flags & flags::FLAG_CTRL, 0);
        assert_eq!(payload, b"started control");
        stream
            .write_all(&frame::encode_frame(
                control.request_id,
                flags::FLAG_CTRL | flags::FLAG_RESPONSE,
                b"ack",
            ))
            .await
            .unwrap();
        let (business, _) = read_frame(&mut stream).await;
        reply(&mut stream, business.request_id, b"business").await;
        expect_disconnect_or_eof(&mut stream).await;
        crate::tests::close_listener(listener).await;
    });
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let conn = client.driver.read().clone().unwrap();
    let (entered, paused) = oneshot::channel();
    let (resume, resumed) = oneshot::channel();
    *conn.pause.lock() = Some((entered, resumed));
    let client = Arc::new(client);
    let control = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .send_control_inline(b"started control".to_vec(), "test")
                .await
        }
    });
    paused.await.unwrap();
    control.abort();
    assert!(control.await.unwrap_err().is_cancelled());
    assert!(!conn.core.is_deactivated());
    resume.send(()).unwrap();
    assert_eq!(
        client
            .call_inline(&identity(), 0, b"business")
            .await
            .unwrap()
            .into_owned_bytes()
            .unwrap(),
        b"business"
    );
    assert_eq!(client.pending_len_for_test(), 0);
    assert!(client.close_shared_bounded(WAIT).await);
    peer.await.unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn sync_client_controlled_async_call_uses_caller_runtime_without_nested_block_on() {
    let endpoint = Endpoint::new();
    let calls = Arc::new(CountEcho(AtomicU64::new(0)));
    let (server, runner) = start(&endpoint, calls.clone(), ServerIpcConfig::default()).await;
    #[cfg(unix)]
    let context = LocalEndpointContext::with_unix_root(endpoint._root.path()).unwrap();
    #[cfg(windows)]
    let context = LocalEndpointContext::default_for_platform().unwrap();
    let pool = Arc::new(crate::ClientPool::with_endpoint_context(WAIT, context));
    let address = endpoint.endpoint.address().to_string();
    let client = tokio::task::spawn_blocking({
        let pool = pool.clone();
        move || pool.acquire(&address, Some(&config())).unwrap()
    })
    .await
    .unwrap();
    let binding = tokio::task::spawn_blocking({
        let client = client.clone();
        move || client.acquire_route(&contract()).unwrap()
    })
    .await
    .unwrap();
    let grants = Arc::new(AtomicU64::new(0));
    let control = IpcCallControl::new({
        let grants = grants.clone();
        move || {
            grants.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    });
    let response = client
        .call_bound_controlled_async(&binding, "echo", b"async caller", &control)
        .await
        .unwrap();
    assert_eq!(response.into_owned_bytes().unwrap(), b"async caller");
    assert_eq!(grants.load(Ordering::SeqCst), 1);
    assert_eq!(calls.0.load(Ordering::SeqCst), 1);
    assert_eq!(client.pending_len_for_test(), 0);
    tokio::task::spawn_blocking(move || assert!(client.close_shared(WAIT)))
        .await
        .unwrap();
    stop(server, runner).await;
}

#[tokio::test]
async fn inline_stream_copy_reserves_before_polling_its_charged_source() {
    use c2_mem::retention::{BudgetedBuffer, PayloadBufferBudget};

    let endpoint = Endpoint::new();
    let calls = Arc::new(CountEcho(AtomicU64::new(0)));
    let (server, runner) = start(&endpoint, calls.clone(), ServerIpcConfig::default()).await;
    let mut cfg = config();
    cfg.shm_threshold = u64::MAX;
    let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
    client.connect().await.unwrap();
    let binding = client.acquire_route(&contract()).await.unwrap();
    let budget = PayloadBufferBudget::new(4);
    let source = BudgetedBuffer::new(vec![3; 4], budget.try_reserve(4).unwrap());
    let polls = Arc::new(AtomicU64::new(0));
    let polled = polls.clone();
    let source = futures_util::stream::iter([Ok::<_, std::io::Error>(source)]).inspect(move |_| {
        polled.fetch_add(1, Ordering::SeqCst);
    });
    let error = client
        .call_bound_sized_stream_phased(&binding, "echo", 4, source, Some(&budget))
        .await
        .unwrap_err();
    assert_eq!(error.phase(), crate::TransportPhase::PreDispatch);
    assert!(
        matches!(error.into_source(), IpcError::LocalCallRejected(error)
        if error.code == c2_error::ErrorCode::CallCapacityExceeded)
    );
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert_eq!(calls.0.load(Ordering::SeqCst), 0);
    assert_eq!(budget.snapshot().used_bytes, 0);
    assert_eq!(budget.snapshot().peak_bytes, 4);
    assert_eq!(budget.snapshot().rejected_allocations, 1);
    assert_eq!(budget.snapshot().rejected_bytes, 4);
    assert!(client.is_connected());
    assert_eq!(
        client
            .call_bound(&binding, "echo", b"healthy")
            .await
            .unwrap()
            .into_owned_bytes()
            .unwrap(),
        b"healthy"
    );
    assert!(client.close_shared_bounded(WAIT).await);
    stop(server, runner).await;
}

#[tokio::test]
async fn owned_input_survives_unpublished_shm_fallback_without_an_extra_copy() {
    use c2_mem::retention::{BudgetedBuffer, PayloadBufferBudget};

    let endpoint = Endpoint::new();
    let calls = Arc::new(CountEcho(AtomicU64::new(0)));
    let (server, runner) = start(&endpoint, calls.clone(), ServerIpcConfig::default()).await;
    let mut cfg = config();
    cfg.base.shm_backing_budget_bytes = 0;
    let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
    client.connect().await.unwrap();
    let binding = client.acquire_route(&contract()).await.unwrap();
    let budget = PayloadBufferBudget::new(8193);
    let input = BudgetedBuffer::new(vec![7; 8193], budget.try_reserve(8193).unwrap());
    let dispatches = Arc::new(AtomicU64::new(0));
    let counted = dispatches.clone();
    let control = IpcCallControl::new(move || {
        counted.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    let reply = client
        .call_bound_owned_controlled_phased(&binding, "echo", input, &control)
        .await
        .unwrap();
    assert_eq!(reply.into_owned_bytes().unwrap(), vec![7; 8193]);
    assert_eq!(dispatches.load(Ordering::SeqCst), 1);
    assert_eq!(calls.0.load(Ordering::SeqCst), 1);
    assert_eq!(budget.snapshot().used_bytes, 0);
    assert_eq!(budget.snapshot().peak_bytes, 8193);
    assert_eq!(budget.snapshot().rejected_allocations, 0);
    assert_eq!(client.memory_budget.snapshot().shm.used_bytes, 0);
    assert!(client.close_shared_bounded(WAIT).await);
    stop(server, runner).await;
}

async fn source_error_terminal_receipt_race(rejected: bool) {
    struct SourceError {
        taken: std::sync::Mutex<Option<oneshot::Sender<()>>>,
        resume: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
    }
    impl std::fmt::Display for SourceError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            if let Some(taken) = self.taken.lock().unwrap().take() {
                let _ = taken.send(());
            }
            let _ = self.resume.lock().unwrap().recv();
            f.write_str("upload source error already taken")
        }
    }
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let (take_error, error_taken) = oneshot::channel();
    let (resume_error, resumed_error) = std::sync::mpsc::channel();
    let (send_terminal, terminal_requested) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let (request, _) = read_frame(&mut stream).await;
        if !rejected {
            let (continuation, _) = read_frame(&mut stream).await;
            assert_eq!(continuation.request_id, request.request_id);
        }
        terminal_requested.await.unwrap();
        if rejected {
            let error =
                c2_error::C2Error::new(c2_error::ErrorCode::ResourceUnavailable, "not executed")
                    .to_wire_bytes();
            stream
                .write_all(
                    &encode_call_end_frame(
                        request.request_id,
                        CallMode::Unary,
                        CallStatus::NotExecuted,
                        0,
                        ValuePart::whole(ValueStorage::Inline(&error)),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
        } else {
            reply(&mut stream, request.request_id, b"known result").await;
        }
        // A control response on this physical reader proves the earlier terminal
        // was placed in its response channel before the upload can resume.
        let (barrier, _) = read_frame(&mut stream).await;
        assert_eq!(barrier.flags, flags::FLAG_CTRL);
        let payload = c2_wire::route_catalog_control::encode_route_list_response(
            &c2_wire::route_catalog_control::RouteListResponse { routes: vec![] },
        )
        .unwrap();
        stream
            .write_all(&frame::encode_frame(
                barrier.request_id,
                flags::FLAG_CTRL | flags::FLAG_RESPONSE,
                &payload,
            ))
            .await
            .unwrap();
        expect_disconnect_or_eof(&mut stream).await;
        crate::tests::close_listener(listener).await;
    });
    let mut cfg = config();
    cfg.base.shm_backing_budget_bytes = 0;
    let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
    client.connect().await.unwrap();
    let binding = cached_binding(&client);
    let client = Arc::new(client);
    let source_error = SourceError {
        taken: std::sync::Mutex::new(Some(take_error)),
        resume: std::sync::Mutex::new(resumed_error),
    };
    let sent_parts = if rejected { 1 } else { 2 };
    let source =
        futures_util::stream::unfold((0, Some(source_error)), move |(step, error)| async move {
            if step < sent_parts {
                Some((Ok(vec![1; 4096]), (step + 1, error)))
            } else {
                error.map(|error| (Err(error), (step + 1, None)))
            }
        });
    let call = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .call_bound_sized_stream_phased(&binding, "echo", 8192, source, None)
                .await
        }
    });
    error_taken.await.unwrap();
    send_terminal.send(()).unwrap();
    timeout(
        WAIT,
        client.list_routes_raw(c2_wire::route_catalog_control::RouteSelector::All),
    )
    .await
    .unwrap()
    .unwrap();
    resume_error.send(()).unwrap();
    let outcome = timeout(WAIT, call).await.unwrap().unwrap();
    if rejected {
        let error = outcome.unwrap_err();
        assert_eq!(error.phase(), crate::TransportPhase::PreDispatch);
        assert!(
            matches!(error.source_error(), IpcError::CallRejected(error) if error.code == c2_error::ErrorCode::ResourceUnavailable)
        );
    } else {
        assert_eq!(
            outcome.unwrap().into_owned_bytes().unwrap(),
            b"known result"
        );
    }
    assert!(client.close_shared_bounded(WAIT).await);
    peer.await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn received_not_executed_proof_overrides_an_upload_error_already_taken() {
    source_error_terminal_receipt_race(true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn received_success_overrides_an_upload_error_already_taken() {
    source_error_terminal_receipt_race(false).await;
}
