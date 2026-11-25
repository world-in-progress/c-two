use super::*;
use crate::tests::EMPTY_METADATA;
use c2_wire::rpc::{WireCallOutcome, WireQueryCallResult, encode_query_call_result_frame};

#[tokio::test]
async fn topic_admission_and_query_match_reordered_results_on_the_subscription_connection() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let leader = CallMetadata::from_entries([("subscriber", "leader")]).unwrap();
    let follower = CallMetadata::from_entries([("subscriber", "follower")]).unwrap();
    let lookup = CallMetadata::from_entries([("caller", "lookup")]).unwrap();
    let peer = tokio::spawn({
        let leader = leader.clone();
        let follower = follower.clone();
        let lookup = lookup.clone();
        async move {
            let mut socket = accept_with_capabilities(&mut listener, CAP_ADMISSION_HOOK).await;
            let (open, body) = read_frame(&mut socket).await;
            assert!(matches!(
                decode_rpc_record(&open, &body, None).unwrap(),
                RpcRecord::OpenSubscribe { metadata, .. } if metadata == leader
            ));
            let subscription_id = open.request_id;
            socket
                .write_all(&rpc::encode_ready_frame(subscription_id, 18, 17).unwrap())
                .await
                .unwrap();

            let (admit, body) = read_frame(&mut socket).await;
            assert!(admit.request_id > subscription_id);
            assert!(matches!(
                decode_rpc_record(&admit, &body, None).unwrap(),
                RpcRecord::AdmitTopic { subscription_stream_id, metadata }
                    if subscription_stream_id == subscription_id && metadata == follower
            ));
            let (query, body) = read_frame(&mut socket).await;
            assert!(query.request_id > admit.request_id);
            assert!(matches!(
                decode_rpc_record(&query, &body, None).unwrap(),
                RpcRecord::QueryCall { metadata, .. } if metadata == lookup
            ));
            reply_to_query(&mut socket, query.request_id, WireCallOutcome::unknown()).await;
            socket
                .write_all(
                    &rpc::encode_admit_topic_result_frame(
                        admit.request_id,
                        &rpc::WireAdmitTopicResult::Allowed { head_seq: 23 },
                    )
                    .unwrap(),
                )
                .await
                .unwrap();

            let (denied, body) = read_frame(&mut socket).await;
            assert!(matches!(
                decode_rpc_record(&denied, &body, None).unwrap(),
                RpcRecord::AdmitTopic { subscription_stream_id, .. }
                    if subscription_stream_id == subscription_id
            ));
            socket
                .write_all(
                    &rpc::encode_admit_topic_result_frame(
                        denied.request_id,
                        &rpc::WireAdmitTopicResult::Error(C2Error::new(
                            ErrorCode::AdmissionDenied,
                            "private refusal",
                        )),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();

            let (cancel, body) = read_frame(&mut socket).await;
            assert_eq!(cancel.request_id, subscription_id);
            assert!(matches!(
                decode_rpc_record(&cancel, &body, None).unwrap(),
                RpcRecord::Cancel {
                    cause: rpc::CancelCause::Consumer
                }
            ));
            socket
                .write_all(
                    &rpc::encode_call_end_error_frame(
                        subscription_id,
                        CallMode::Subscribe,
                        CallStatus::Failed,
                        0,
                        &C2Error::new(ErrorCode::CallCancelled, "subscription cancelled"),
                    )
                    .unwrap(),
                )
                .await
                .unwrap();
            expect_disconnect_or_eof(&mut socket).await;
            crate::tests::close_listener(listener).await;
        }
    });
    let mut client = endpoint.client();
    assert!(!client.server_has_admission_hook());
    client.connect().await.unwrap();
    assert!(client.server_has_admission_hook());
    client.route_directory.write().insert_table(
        "grid".into(),
        MethodTable::from_entries(&[], identity(), 1024),
    );
    let binding = client.bind_cached_route(&contract()).unwrap();
    let topic = timeout(
        WAIT,
        client.forward_subscribe_bound(
            &binding,
            "state",
            rpc::WireSubscribeOptions::default(),
            &leader,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(topic.requires_admission());
    assert_eq!(topic.start_seq(), 18);
    assert_eq!(topic.head_seq(), 17);

    let synchronizer = topic.synchronizer();
    let permits: Vec<_> =
        std::iter::from_fn(|| client.budget.reserve_control_operation().ok()).collect();
    assert!(!permits.is_empty());
    let saturated = synchronizer
        .admit_subscriber(&follower, None)
        .await
        .unwrap_err();
    assert_eq!(saturated.phase(), crate::TransportPhase::PreDispatch);
    assert!(matches!(
        saturated.source_error(),
        IpcError::LocalCallRejected(error) if error.code == ErrorCode::CallCapacityExceeded
    ));
    drop(permits);

    let (admitted, queried) = tokio::join!(
        synchronizer.admit_subscriber(&follower, std::time::Instant::now().checked_add(WAIT)),
        client.query_call(&binding, "id", WAIT, &lookup),
    );
    assert_eq!(admitted.unwrap(), 23);
    assert_eq!(queried.unwrap(), WireCallOutcome::unknown());
    let denied = timeout(WAIT, synchronizer.admit_subscriber(&follower, None))
        .await
        .unwrap()
        .unwrap_err();
    assert_eq!(denied.phase(), crate::TransportPhase::PreDispatch);
    assert!(matches!(
        denied.source_error(),
        IpcError::CallRejected(error) if error.code == ErrorCode::AdmissionDenied
    ));
    topic
        .cancellation()
        .cancel(rpc::CancelCause::Consumer)
        .unwrap();
    let closed = synchronizer
        .admit_subscriber(&follower, None)
        .await
        .unwrap_err();
    assert!(matches!(
        closed.source_error(),
        IpcError::LocalCallRejected(error) if error.code == ErrorCode::ResourceClosed
    ));
    timeout(WAIT, topic.completion()).await.unwrap().unwrap();
    drop(topic);
    assert!(client.close_shared_bounded(WAIT).await);
    peer.await.unwrap();
}

fn one_call_limits() -> DomainLimits {
    DomainLimits {
        max_outstanding_calls: 1,
        ..DomainLimits::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn outcome_query_bypasses_saturated_client_and_server() {
    struct GatedEcho {
        entered: StdMutex<Option<oneshot::Sender<()>>>,
        release: StdMutex<std::sync::mpsc::Receiver<()>>,
    }
    impl CrmCallback for GatedEcho {
        fn invoke(
            &self,
            _: &str,
            _: u16,
            data: Vec<u8>,
            _: Arc<c2_server::ResponsePool>,
        ) -> Result<ResponseMeta, CrmError> {
            self.entered.lock().take().unwrap().send(()).unwrap();
            self.release.lock().recv_timeout(WAIT * 2).unwrap();
            Ok(ResponseMeta::Inline(data))
        }
    }

    let endpoint = Endpoint::new();
    let (entered, ready) = oneshot::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let (server, runner) = start_with_server_limits(
        &endpoint,
        Arc::new(GatedEcho {
            entered: StdMutex::new(Some(entered)),
            release: StdMutex::new(gate),
        }),
        ServerIpcConfig::default(),
        one_call_limits(),
    )
    .await;
    let budget = DomainBudget::from_limits(&one_call_limits());
    let mut client =
        IpcClient::with_endpoint_and_budget(endpoint.endpoint.clone(), budget.clone()).unwrap();
    client.connect().await.unwrap();
    let binding = client.acquire_route(&contract()).await.unwrap();
    let client = Arc::new(client);
    let call = tokio::spawn({
        let client = client.clone();
        let binding = binding.clone();
        // The client's Runtime owns the business permit for the IPC call.
        let slot = budget.reserve_operation().unwrap();
        async move {
            let _slot = slot;
            client
                .call_keyed_bound(&binding, "echo", "blocked", b"result", &EMPTY_METADATA)
                .await
        }
    });
    timeout(WAIT, ready).await.unwrap().unwrap();
    assert!(budget.reserve_operation().is_err());
    assert!(server.domain_budget().reserve_operation().is_err());
    let outcome = client
        .query_call(&binding, "blocked", WAIT, &EMPTY_METADATA)
        .await;
    release.send(()).unwrap();
    assert_eq!(outcome.unwrap(), WireCallOutcome::in_flight("echo".into()));
    assert_eq!(
        call.await.unwrap().unwrap().into_owned_bytes().unwrap(),
        b"result"
    );
    assert_eq!(
        client
            .query_call(&binding, "blocked", WAIT, &EMPTY_METADATA)
            .await
            .unwrap(),
        WireCallOutcome::succeeded("echo".into(), true, None),
    );
    assert_eq!(
        client
            .recover_call_bound(&binding, "echo", "blocked", &EMPTY_METADATA)
            .await
            .unwrap()
            .into_owned_bytes()
            .unwrap(),
        b"result",
    );
    let miss = client
        .recover_call_bound(&binding, "echo", "missing", &EMPTY_METADATA)
        .await
        .unwrap_err();
    assert!(
        matches!(miss, IpcError::CallRejected(error) if error.code == ErrorCode::CallOutcomeUnknown)
    );
    assert!(client.close_shared_bounded(WAIT).await);
    stop(server, runner).await;
}

async fn reply_to_query(stream: &mut LocalStream, rid: u64, outcome: WireCallOutcome) {
    stream
        .write_all(
            &encode_query_call_result_frame(rid, &WireQueryCallResult::Outcome(outcome)).unwrap(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn outcome_query_gets_writer_id_in_queue_order() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        for expected_id in [1, 2] {
            let (header, payload) = read_frame(&mut stream).await;
            assert_eq!(header.request_id, expected_id);
            assert_eq!(payload[0], RpcRecordKind::Open as u8);
            reply(&mut stream, header.request_id, b"ok").await;
        }
        let (header, payload) = read_frame(&mut stream).await;
        assert_eq!(header.request_id, 3);
        assert!(
            matches!(decode_rpc_record(&header, &payload, None).unwrap(), RpcRecord::QueryCall { call_id, .. } if call_id == "lookup")
        );
        reply_to_query(&mut stream, header.request_id, WireCallOutcome::unknown()).await;
        expect_disconnect_or_eof(&mut stream).await;
        crate::tests::close_listener(listener).await;
    });
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let conn = client.driver.read().clone().unwrap();
    let (entered, paused) = oneshot::channel();
    let (resume, resumed) = oneshot::channel();
    *conn.pause.lock() = Some((entered, resumed));
    let identity = identity();
    let (first, second, query) = {
        let first = client.call_inline(&identity, 0, b"first", &EMPTY_METADATA);
        tokio::pin!(first);
        assert!(futures_util::poll!(&mut first).is_pending());
        timeout(WAIT, paused).await.unwrap().unwrap();
        let second = client.call_inline(&identity, 0, b"second", &EMPTY_METADATA);
        tokio::pin!(second);
        assert!(futures_util::poll!(&mut second).is_pending());
        let query = client.query_call_identity(&identity, "lookup", WAIT, &EMPTY_METADATA);
        tokio::pin!(query);
        assert!(futures_util::poll!(&mut query).is_pending());
        resume.send(()).unwrap();
        timeout(WAIT, async { tokio::join!(first, second, query) })
            .await
            .unwrap()
    };
    first.unwrap();
    second.unwrap();
    assert_eq!(query.unwrap(), WireCallOutcome::unknown());
    assert!(client.close_shared_bounded(WAIT).await);
    peer.await.unwrap();
}

#[tokio::test]
async fn outcome_query_cancellation_releases_slot_and_ignores_late_reply() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let (sent, observed) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let (first, _) = read_frame(&mut stream).await;
        sent.send(()).unwrap();
        let (second, _) = read_frame(&mut stream).await;
        assert!(second.request_id > first.request_id);
        reply_to_query(
            &mut stream,
            first.request_id,
            WireCallOutcome::in_flight("stale".into()),
        )
        .await;
        reply_to_query(&mut stream, second.request_id, WireCallOutcome::unknown()).await;
        expect_disconnect_or_eof(&mut stream).await;
        crate::tests::close_listener(listener).await;
    });
    let mut client = IpcClient::with_endpoint_and_budget(
        endpoint.endpoint.clone(),
        DomainBudget::from_limits(&one_call_limits()),
    )
    .unwrap();
    client.connect().await.unwrap();
    let conn = client.driver.read().clone().unwrap();
    let identity = identity();
    {
        let query = client.query_call_identity(&identity, "first", WAIT, &EMPTY_METADATA);
        tokio::pin!(query);
        assert!(futures_util::poll!(&mut query).is_pending());
        timeout(WAIT, observed).await.unwrap().unwrap();
        let rejected = client
            .query_call_identity(&identity, "over-capacity", WAIT, &EMPTY_METADATA)
            .await
            .unwrap_err();
        assert!(
            matches!(rejected, IpcError::LocalCallRejected(error) if error.code == ErrorCode::CallCapacityExceeded)
        );
    }
    assert_eq!(conn.pending_control_requests_for_test(), 0);
    assert_eq!(
        client
            .query_call_identity(&identity, "second", WAIT, &EMPTY_METADATA)
            .await
            .unwrap(),
        WireCallOutcome::unknown()
    );
    assert!(client.close_shared_bounded(WAIT).await);
    peer.await.unwrap();
}

#[tokio::test]
async fn outcome_query_deadline_covers_the_writer_queue() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let (header, payload) = read_frame(&mut stream).await;
        assert_eq!(payload[0], RpcRecordKind::Open as u8);
        reply(&mut stream, header.request_id, b"ok").await;
        expect_disconnect_or_eof(&mut stream).await;
        crate::tests::close_listener(listener).await;
    });
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let conn = client.driver.read().clone().unwrap();
    let (entered, paused) = oneshot::channel();
    let (resume, resumed) = oneshot::channel();
    *conn.pause.lock() = Some((entered, resumed));
    let identity = identity();
    let first = client.call_inline(&identity, 0, b"first", &EMPTY_METADATA);
    tokio::pin!(first);
    assert!(futures_util::poll!(&mut first).is_pending());
    timeout(WAIT, paused).await.unwrap().unwrap();
    let error = timeout(
        WAIT,
        client.query_call_identity(
            &identity,
            "queued",
            Duration::from_millis(20),
            &EMPTY_METADATA,
        ),
    )
    .await;
    resume.send(()).unwrap();
    assert!(
        matches!(error.unwrap().unwrap_err(), IpcError::Io(error) if error.kind() == std::io::ErrorKind::TimedOut)
    );
    assert_eq!(conn.pending_control_requests_for_test(), 0);
    timeout(WAIT, first).await.unwrap().unwrap();
    assert!(client.close_shared_bounded(WAIT).await);
    peer.await.unwrap();
}

#[tokio::test]
async fn keyed_unary_falls_back_to_chunks_when_shm_allocation_is_refused() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let input_len = CHUNK_SIZE as usize * 2 + 7;
    let call_id = "Z".repeat(128);
    let peer = tokio::spawn({
        let call_id = call_id.clone();
        async move {
            let mut stream = accept(&mut listener).await;
            let (first, payload) = read_frame(&mut stream).await;
            let record = decode_rpc_record(&first, &payload, None).unwrap();
            assert!(matches!(record, RpcRecord::Open {
                mode: CallMode::KeyedUnary,
                call_id: Some(actual),
                value: ValuePart {
                    chunk: rpc::ChunkPosition::First { total_bytes },
                    storage: ValueStorage::Inline(bytes),
                },
                ..
            } if actual == call_id && total_bytes == input_len as u64
                && bytes.len() == CHUNK_SIZE as usize && bytes.iter().all(|byte| *byte == 42)));
            for expected_size in [CHUNK_SIZE as usize, 7] {
                let (next, payload) = read_frame(&mut stream).await;
                assert_eq!(next.request_id, first.request_id);
                let record = decode_rpc_record(&next, &payload, None).unwrap();
                assert!(matches!(record, RpcRecord::OpenContinuation {
                    value: ValuePart { storage: ValueStorage::Inline(bytes), .. },
                } if bytes.len() == expected_size && bytes.iter().all(|byte| *byte == 42)));
            }
            reply(&mut stream, first.request_id, b"ok").await;
            expect_disconnect_or_eof(&mut stream).await;
            crate::tests::close_listener(listener).await;
        }
    });
    let mut client = endpoint.client_without_memory();
    client.connect().await.unwrap();
    assert!(client.shm_request_available());
    let response = timeout(
        WAIT,
        client.call_resolved_target(
            0,
            identity(),
            DEFAULT_MAX_PAYLOAD_SIZE,
            &vec![42; input_len],
            Some(&call_id),
            None,
            &EMPTY_METADATA,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.into_owned_bytes().unwrap(), b"ok");
    assert!(client.close_shared_bounded(WAIT).await);
    peer.await.unwrap();
}

#[tokio::test]
async fn keyed_unary_can_reassemble_a_reply_larger_than_the_request() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let (header, payload) = read_frame(&mut stream).await;
        let record = decode_rpc_record(&header, &payload, None).unwrap();
        assert!(matches!(
            record,
            RpcRecord::Open {
                mode: CallMode::KeyedUnary,
                ..
            }
        ));
        for index in 0..2 {
            stream
                .write_all(&reply_chunk(header.request_id, 2, index, &[42; 4096]))
                .await
                .unwrap();
        }
        expect_disconnect_or_eof(&mut stream).await;
        crate::tests::close_listener(listener).await;
    });
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let response = timeout(
        WAIT,
        client.call_resolved_target(
            0,
            identity(),
            DEFAULT_MAX_PAYLOAD_SIZE,
            b"x",
            Some("expand"),
            None,
            &EMPTY_METADATA,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.into_owned_bytes().unwrap(), vec![42; 8192]);
    assert!(client.close_shared_bounded(WAIT).await);
    peer.await.unwrap();
}

#[tokio::test]
async fn keyed_forwarding_preserves_the_call_id_across_sliced_input() {
    let endpoint = Endpoint::new();
    let (server, runner) = start(&endpoint, Arc::new(Echo), ServerIpcConfig::default()).await;
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let binding = client.acquire_route(&contract()).await.unwrap();
    let call_id = "L".repeat(128);
    let input = b"0123456789abcdefghijklmnop";
    let source = futures_util::stream::iter(input.chunks(5).map(Ok::<_, std::io::Error>));
    let reservation = client.domain_budget().try_reserve_memory(7).unwrap();
    let result = timeout(
        WAIT,
        client.forward_keyed_unary_bound_sized_phased(
            &binding,
            "echo",
            Some(&call_id),
            input.len() as u64,
            source,
            reservation,
            &EMPTY_METADATA,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(result.into_owned_bytes().unwrap(), input);
    let recovered = client
        .recover_call_bound(&binding, "echo", &call_id, &EMPTY_METADATA)
        .await
        .unwrap();
    assert_eq!(recovered.into_owned_bytes().unwrap(), input);
    assert!(client.close_shared_bounded(WAIT).await);
    stop(server, runner).await;
}

#[tokio::test]
async fn outcome_query_deadline_does_not_interrupt_a_partial_frame() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let (first, payload) = read_frame(&mut stream).await;
        assert!(matches!(
            decode_rpc_record(&first, &payload, None).unwrap(),
            RpcRecord::QueryCall { call_id, .. } if call_id == "timed-out"
        ));
        reply_to_query(
            &mut stream,
            first.request_id,
            WireCallOutcome::in_flight("stale".into()),
        )
        .await;
        let (second, payload) = read_frame(&mut stream).await;
        assert!(second.request_id > first.request_id);
        assert!(matches!(
            decode_rpc_record(&second, &payload, None).unwrap(),
            RpcRecord::QueryCall { call_id, .. } if call_id == "following"
        ));
        reply_to_query(&mut stream, second.request_id, WireCallOutcome::unknown()).await;
        expect_disconnect_or_eof(&mut stream).await;
        crate::tests::close_listener(listener).await;
    });
    let mut client = IpcClient::with_endpoint_and_budget(
        endpoint.endpoint.clone(),
        DomainBudget::from_limits(&one_call_limits()),
    )
    .unwrap();
    client.connect().await.unwrap();
    let conn = client.driver.read().clone().unwrap();
    let (entered, paused) = oneshot::channel();
    let (resume, resumed) = oneshot::channel();
    client.set_write_prefix_pause_for_test(Some((entered, resumed)));
    let identity = identity();
    let query = client.query_call_identity(
        &identity,
        "timed-out",
        Duration::from_millis(100),
        &EMPTY_METADATA,
    );
    tokio::pin!(query);
    assert!(futures_util::poll!(&mut query).is_pending());
    timeout(WAIT, paused).await.unwrap().unwrap();
    let error = timeout(WAIT, query).await.unwrap().unwrap_err();
    assert!(matches!(error, IpcError::Io(error) if error.kind() == std::io::ErrorKind::TimedOut));
    assert_eq!(conn.pending_control_requests_for_test(), 0);
    resume.send(()).unwrap();
    assert_eq!(
        client
            .query_call_identity(&identity, "following", WAIT, &EMPTY_METADATA)
            .await
            .unwrap(),
        WireCallOutcome::unknown()
    );
    assert!(client.close_shared_bounded(WAIT).await);
    peer.await.unwrap();
}

#[tokio::test]
async fn outcome_open_honors_the_first_frame_guard_on_every_request_carrier() {
    let endpoint = Endpoint::new();
    let (server, runner) = start(&endpoint, Arc::new(Echo), ServerIpcConfig::default()).await;
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let binding = client.acquire_route(&contract()).await.unwrap();
    let refused = IpcCallControl::new(|| {
        Err(C2Error::new(
            ErrorCode::CallDeadlineExceeded,
            "first frame refused",
        ))
    });
    let assert_refused = |error: crate::IpcCallError| {
        assert_eq!(error.phase(), crate::TransportPhase::PreDispatch);
        assert!(matches!(
            error.into_source(),
            IpcError::LocalCallRejected(error) if error.code == ErrorCode::CallDeadlineExceeded
        ));
    };

    let inline = client
        .call_bound_owned_with_key_controlled_phased(
            &binding,
            "echo",
            b"inline".to_vec(),
            Some("inline"),
            &refused,
            &EMPTY_METADATA,
        )
        .await
        .unwrap_err();
    assert_refused(inline);
    let slab = client
        .try_alloc_request_slab(CHUNK_SIZE as usize)
        .unwrap()
        .unwrap();
    let shm = client
        .call_bound_slab_with_key_controlled_phased(
            &binding,
            "echo",
            slab,
            Some("shm"),
            &refused,
            &EMPTY_METADATA,
        )
        .await
        .unwrap_err();
    assert_refused(shm);
    let recover = client
        .recover_call_bound_controlled_phased(&binding, "echo", "absent", &refused, &EMPTY_METADATA)
        .await
        .unwrap_err();
    assert_refused(recover);

    let mut chunk_client = endpoint.client_without_memory();
    chunk_client.connect().await.unwrap();
    let chunk_binding = chunk_client.acquire_route(&contract()).await.unwrap();
    let chunked = chunk_client
        .call_bound_owned_with_key_controlled_phased(
            &chunk_binding,
            "echo",
            vec![0; CHUNK_SIZE as usize + 1],
            Some("chunk"),
            &refused,
            &EMPTY_METADATA,
        )
        .await
        .unwrap_err();
    assert_refused(chunked);

    for call_id in ["inline", "shm", "chunk"] {
        assert_eq!(
            client
                .query_call(&binding, call_id, WAIT, &EMPTY_METADATA)
                .await
                .unwrap(),
            WireCallOutcome::unknown()
        );
    }
    assert_eq!(
        client
            .call_bound(&binding, "echo", b"healthy", &EMPTY_METADATA)
            .await
            .unwrap()
            .into_owned_bytes()
            .unwrap(),
        b"healthy"
    );
    assert!(chunk_client.close_shared_bounded(WAIT).await);
    assert!(client.close_shared_bounded(WAIT).await);
    stop(server, runner).await;
}
