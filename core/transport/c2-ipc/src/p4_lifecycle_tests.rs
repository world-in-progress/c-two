use super::*;

// An unpolled receive task owns its cleanup fence. B retains the actual stream,
// while an unproved end must leave L allocated in its original owner pool.
#[tokio::test]
async fn dropped_receive_task_preserves_first_cause_and_waits_only_for_remote_reads() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let (stream, peer) = tokio::join!(
        LocalStream::connect(&endpoint.endpoint, WAIT),
        listener.accept()
    );
    let stream = stream.unwrap();
    let mut peer = peer.unwrap();
    let source = c2_local::ConnectionEventSource::new();
    let events = source.subscribe();
    let publisher = source.open_connection(c2_local::ConnectionSide::Client);
    let observation = publisher.observation();
    let end = EndState::new(publisher);
    let retirement = PhysicalRetirement::new(stream.retain_for_remote_read(), end);
    let abort = stream.abort_handle();
    let (reader, writer) = stream.into_split();
    let pending = Arc::new(StdMutex::new(HashMap::new()));
    let conn = ClientConnection::spawn(writer, pending.clone(), abort, retirement.clone());
    let (_loan, pool, budget) = publish_file_loan(&conn, endpoint._root.path());
    let remote = conn
        .core
        .admit_remote_loan(991, Box::new(retirement.clone()))
        .unwrap();
    let registry = IpcClient::build_chunk_registry(&config(), &budget).unwrap();
    let peer_pool = Arc::new(RwLock::new(ServerPoolState::from_pool_for_test(
        MemPool::open_peer(c2_mem::PoolConfig::default(), "/p4unused".into()).unwrap(),
    )));
    let receiving = tokio::spawn(recv_loop(
        reader,
        pending,
        peer_pool,
        conn.clone(),
        registry,
        991,
        false,
        retirement,
    ));
    conn.end(
        ConnectionEndReason::ProtocolError,
        "original_protocol_fault",
    );
    receiving.abort();
    assert!(receiving.await.unwrap_err().is_cancelled());
    assert!(observation.snapshot().is_none());
    assert!(
        timeout(Duration::from_millis(20), peer.read(&mut [0]))
            .await
            .is_err(),
        "the admitted read retains the physical stream"
    );
    assert!(budget.snapshot().file.used_bytes > 0);
    remote.finish();
    let closed = observation.wait_closed(Some(WAIT)).await.unwrap();
    assert_eq!(closed.reason(), ConnectionEndReason::ProtocolError);
    assert_eq!(closed.operation(), "original_protocol_fault");
    assert!(!closed.is_peer_end());
    assert_eq!(
        timeout(WAIT, peer.read(&mut [0])).await.unwrap().unwrap(),
        0
    );
    assert!(!conn.core.try_close(), "unproved end must preserve L");
    assert_eq!(budget.snapshot().file.used_bytes, 4096);
    assert!(matches!(
        events.recv().await.unwrap(),
        Some(c2_local::ConnectionEvent::Opened(_))
    ));
    assert!(
        matches!(events.recv().await.unwrap(), Some(c2_local::ConnectionEvent::Closed(record)) if record == closed)
    );
    conn.end(ConnectionEndReason::LocalClose, "later_close");
    assert!(
        timeout(Duration::from_millis(20), events.recv())
            .await
            .is_err()
    );
    drop(conn);
    // This file-backed fixture proves the live loan book's wiring. The real
    // SHM test below separately proves owner-pool retention after client Drop.
    drop(pool);
    assert_eq!(budget.snapshot().file.used_bytes, 0);
    crate::tests::close_listener(listener).await;
}

#[tokio::test]
async fn failed_and_expired_handshakes_publish_one_open_and_the_original_close() {
    for expire in [false, true] {
        let endpoint = Endpoint::new();
        let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
        let peer = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            let mut hello = [0; 25];
            stream.read_exact(&mut hello).await.unwrap();
            if !expire {
                stream.write_all(&[0; 25]).await.unwrap();
            }
            assert_eq!(
                timeout(WAIT, stream.read(&mut [0])).await.unwrap().unwrap(),
                0
            );
            crate::tests::close_listener(listener).await;
        });
        let source = c2_local::ConnectionEventSource::new();
        let events = source.subscribe();
        let mut client = endpoint.client().with_connection_event_source(source);
        let deadline = ConnectDeadline::start(
            c2_config::ConnectOptions::new().with_timeout(Duration::from_millis(100)),
        )
        .unwrap();
        let error = client.connect_with_deadline(deadline).await.unwrap_err();
        let observation = match events.recv().await.unwrap().unwrap() {
            c2_local::ConnectionEvent::Opened(observation) => observation,
            event => panic!("expected physical admission: {event:?}"),
        };
        let closed = observation.wait_closed(Some(WAIT)).await.unwrap();
        assert_eq!(
            closed.reason(),
            if expire {
                ConnectionEndReason::LocalClose
            } else {
                ConnectionEndReason::ProtocolError
            },
            "{error}"
        );
        assert_eq!(
            closed.operation(),
            if expire {
                "connect_deadline"
            } else {
                "handshake_preamble"
            }
        );
        assert!(client.close_shared_bounded(WAIT).await);
        assert!(
            matches!(events.recv().await.unwrap(), Some(c2_local::ConnectionEvent::Closed(record)) if record == closed)
        );
        assert!(
            timeout(Duration::from_millis(20), events.recv())
                .await
                .is_err()
        );
        peer.await.unwrap();
    }
}

// The peer stops consuming in the middle of a genuinely blocked write. Close
// observes its deadline without aborting that frame or claiming completion.
#[tokio::test]
async fn bounded_close_is_honest_while_peer_stops_reading_a_started_frame() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let (entered, started) = oneshot::channel();
    let (resume, resumed) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let mut header = [0; HEADER_SIZE];
        stream.read_exact(&mut header).await.unwrap();
        let (len, body) = frame::decode_total_len(&header).unwrap();
        let (header, _) = frame::decode_frame_body(body, len).unwrap();
        assert!(header.payload_len() > 8 * 1024 * 1024);
        entered.send(()).unwrap();
        resumed.await.unwrap();
        let mut payload = vec![0; header.payload_len()];
        stream.read_exact(&mut payload).await.unwrap();
        let (_, control_len) = c2_wire::control::decode_call_control(&payload, 0).unwrap();
        assert!(payload[control_len..].iter().all(|byte| *byte == 0x5b));
        assert_eq!(
            timeout(WAIT, stream.read(&mut [0])).await.unwrap().unwrap(),
            0
        );
        crate::tests::close_listener(listener).await;
    });
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let client = Arc::new(client);
    let caller = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .call_inline(&identity(), 0, &vec![0x5b; 16 * 1024 * 1024])
                .await
        }
    });
    started.await.unwrap();
    assert!(!client.close_shared_bounded(Duration::from_millis(20)).await);
    assert!(
        client
            .connection_observation()
            .unwrap()
            .snapshot()
            .is_none()
    );
    let error = caller.await.unwrap().unwrap_err();
    assert_eq!(error.phase(), crate::TransportPhase::DispatchUncertain);
    resume.send(()).unwrap();
    assert!(client.close_shared_bounded(WAIT).await);
    peer.await.unwrap();
}

#[tokio::test]
async fn idle_client_gc_refunds_real_buddy_segments_and_recreates_a_new_generation() {
    let endpoint = Endpoint::new();
    let (server, runner) = start(&endpoint, Arc::new(Echo), ServerIpcConfig::default()).await;
    let mut cfg = config();
    cfg.base.max_pool_segments = 1;
    cfg.base.chunk_gc_interval_secs = 0.01;
    cfg.pool_decay_seconds = 0.01;
    let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
    client.connect().await.unwrap();
    let binding = client.acquire_route(&contract()).await.unwrap();
    let pool = client.request_pool().unwrap();
    pool.write()
        .ensure_buddy_segments(1)
        .expect("real request SHM is required");
    for expected_generation in 1..=2 {
        let block = client.try_alloc_request_block(8193).unwrap().unwrap();
        assert!(!block.allocation().is_dedicated);
        assert_eq!(block.allocation().generation, expected_generation);
        block.write_at(0, &[9; 8193]).unwrap();
        assert_eq!(
            client
                .call_with_prealloc(&binding.table.call_identity(), 0, &block)
                .await
                .unwrap()
                .into_owned_bytes()
                .unwrap(),
            vec![9; 8193]
        );
        drop(block);
        eventually(|| pool.read().segment_count() == 0).await;
        assert_eq!(client.memory_budget.snapshot().shm.used_bytes, 0);
        assert!(client.is_connected());
    }
    assert!(client.close_shared_bounded(WAIT).await);
    stop(server, runner).await;
}

#[tokio::test]
async fn published_shm_loan_and_budget_survive_cancellation_close_and_client_drop() {
    for dedicated in [false, true] {
        for peer_eof in [false, true] {
            let endpoint = Endpoint::new();
            let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
            let (entered, published) = oneshot::channel();
            let (finish, finished) = oneshot::channel();
            let peer = tokio::spawn(async move {
                let mut stream = accept(&mut listener).await;
                let (header, bytes) = read_frame(&mut stream).await;
                assert!(header.is_buddy());
                let descriptor = decode_buddy_payload(&bytes).unwrap().0;
                assert_eq!(descriptor.is_dedicated, dedicated);
                entered.send(()).unwrap();
                finished.await.unwrap();
                if !peer_eof {
                    stream
                        .write_all(&c2_wire::release::encode_release_frame(
                            &c2_wire::release::ReleaseBatch::new(&[descriptor.loan_id]).unwrap(),
                            true,
                        ))
                        .await
                        .unwrap();
                    assert_eq!(
                        timeout(WAIT, stream.read(&mut [0])).await.unwrap().unwrap(),
                        0
                    );
                }
                drop(stream);
                crate::tests::close_listener(listener).await;
            });
            let mut cfg = config();
            cfg.base.pool_enabled = !dedicated;
            let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
            client.connect().await.unwrap();
            let pool = client.request_pool().unwrap();
            if !dedicated {
                pool.write()
                    .ensure_buddy_segments(1)
                    .expect("real request SHM is required");
            }
            let block = client
                .try_alloc_request_block(8193)
                .unwrap()
                .expect("real SHM allocation is required");
            assert_eq!(block.allocation().is_dedicated, dedicated);
            block.write_at(0, &[7; 8193]).unwrap();
            let budget = client.memory_budget.clone();
            let observation = client.connection_observation().unwrap();
            let client = Arc::new(client);
            let caller = tokio::spawn({
                let client = client.clone();
                async move { client.call_with_prealloc(&identity(), 0, &block).await }
            });
            published.await.unwrap();
            caller.abort();
            assert!(caller.await.unwrap_err().is_cancelled());
            assert!(client.is_connected());
            assert!(!client.close_shared_bounded(Duration::from_millis(20)).await);
            drop(client);
            assert_eq!(pool.read().stats().alloc_count, 1);
            assert!(budget.snapshot().shm.used_bytes > 0);
            assert!(observation.snapshot().is_none());
            finish.send(()).unwrap();
            observation.wait_closed(Some(WAIT)).await.unwrap();
            eventually(|| pool.read().stats().alloc_count == 0).await;
            if dedicated {
                assert_eq!(budget.snapshot().shm.used_bytes, 0);
            }
            peer.await.unwrap();
        }
    }
}

// Prepared but unpublished blocks keep all 17 original segments occupied.
// Real c2-server must accept each generation/slot, including index 16.
#[tokio::test]
async fn prepared_request_owners_reach_peer_segment_index_16_and_remain_budgeted_after_close() {
    let endpoint = Endpoint::new();
    let (server, runner) = start(&endpoint, Arc::new(Echo), ServerIpcConfig::default()).await;
    let mut cfg = config();
    cfg.base.max_pool_segments = 20;
    let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
    client.connect().await.unwrap();
    let binding = client.acquire_route(&contract()).await.unwrap();
    let pool = client.request_pool().unwrap();
    pool.write()
        .ensure_buddy_segments(17)
        .expect("17 real request SHM segments are required");
    let mut blocks = Vec::new();
    for index in 0..17 {
        let block = client.try_alloc_request_block(64 * 1024).unwrap().unwrap();
        assert_eq!(block.allocation().seg_idx, index);
        block.write_at(0, &vec![index as u8; 64 * 1024]).unwrap();
        blocks.push(block);
    }
    let last = blocks.pop().unwrap();
    for (index, block) in blocks.into_iter().enumerate() {
        let bytes = client
            .call_with_prealloc(&binding.table.call_identity(), 0, &block)
            .await
            .unwrap()
            .into_owned_bytes()
            .unwrap();
        assert_eq!(bytes, vec![index as u8; 64 * 1024]);
    }
    let bytes = client
        .call_with_prealloc(&binding.table.call_identity(), 0, &last)
        .await
        .unwrap()
        .into_owned_bytes()
        .unwrap();
    assert_eq!(bytes, vec![16; 64 * 1024]);
    drop(last);
    let external = client.try_alloc_request_block(64 * 1024).unwrap().unwrap();
    let budget = client.memory_budget.clone();
    assert!(client.close_shared_bounded(WAIT).await);
    drop(client);
    assert!(budget.snapshot().shm.used_bytes > 0);
    assert_eq!(pool.read().stats().alloc_count, 1);
    drop(external);
    assert_eq!(pool.read().stats().alloc_count, 0);
    drop(pool);
    assert_eq!(budget.snapshot().shm.used_bytes, 0);
    stop(server, runner).await;
}

#[tokio::test]
async fn owned_response_copies_reach_peer_segment_index_16_without_held_transport_leases() {
    let endpoint = Endpoint::new();
    let pool = Arc::new(RwLock::new(
        MemPool::new_with_prefix(
            c2_mem::PoolConfig {
                segment_size: 8192,
                max_segments: 20,
                ..Default::default()
            },
            format!(
                "/p4slot{}",
                CLIENT_CONN_COUNTER.fetch_add(1, Ordering::Relaxed)
            ),
        )
        .unwrap(),
    ));
    pool.write()
        .ensure_buddy_segments(17)
        .expect("17 real response SHM segments are required");
    let mut handles = Vec::new();
    for index in 0..17 {
        let mut owner = pool.write();
        let mut handle = owner
            .try_alloc_shm(8192)
            .expect("real response SHM is required");
        assert_eq!(handle.shm_allocation().unwrap().seg_idx, index);
        owner.handle_slice_mut(&mut handle).fill(index as u8);
        handles.push(handle);
    }
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let peer_pool = pool.clone();
    let peer = tokio::spawn(async move {
        let core = c2_wire::connection::ConnectionCore::new(
            c2_wire::connection::ConnectionSide::Server,
            || {},
        );
        core.finish_handshake().unwrap();
        let mut stream = listener.accept().await.unwrap();
        crate::tests::accept_version_preamble(&mut stream).await;
        let (_, metadata) = read_frame(&mut stream).await;
        let hs = decode_handshake(&metadata, false).unwrap();
        let prefix = peer_pool.read().prefix().to_string();
        let response = encode_server_handshake(
            hs.capability_flags,
            &[] as &[RouteInfo],
            &prefix,
            &ServerIdentity {
                server_id: "response-slots".to_string(),
                server_instance_id: "instance".to_string(),
            },
        )
        .unwrap();
        stream
            .write_all(&frame::encode_frame(
                0,
                flags::FLAG_HANDSHAKE | flags::FLAG_RESPONSE,
                &response,
            ))
            .await
            .unwrap();
        for handle in handles {
            let request = loop {
                let (header, bytes) = read_frame(&mut stream).await;
                if header.is_ctrl() {
                    let release = core.receive_release(&header, &bytes);
                    assert!(release.error.is_none());
                    drop(release.reclaims);
                } else {
                    break header;
                }
            };
            let allocation = handle.shm_allocation().unwrap();
            let loan = core.prepare_loan(peer_pool.clone(), handle).unwrap();
            let descriptor = encode_buddy_payload(&BuddyPayload {
                seg_idx: allocation.seg_idx as u16,
                generation: allocation.generation,
                offset: allocation.offset,
                data_size: 8192,
                is_dedicated: false,
                loan_id: loan.id(),
            });
            let mut batch = core
                .begin_write(WriteRequest::Business(vec![loan]))
                .unwrap()
                .unwrap();
            batch.begin_io().unwrap();
            stream
                .write_all(&frame::encode_frame_parts(
                    request.request_id,
                    flags::FLAG_RESPONSE | flags::FLAG_REPLY_V2 | flags::FLAG_BUDDY,
                    &[&descriptor, &[0]],
                ))
                .await
                .unwrap();
            batch.frame_completed();
            batch.flush_completed();
        }
        while peer_pool.read().stats().alloc_count > 0 {
            let (header, bytes) = read_frame(&mut stream).await;
            assert!(header.is_ctrl(), "one response per call; never replay");
            let release = core.receive_release(&header, &bytes);
            assert!(release.error.is_none());
            drop(release.reclaims);
        }
        drop(stream);
        crate::tests::close_listener(listener).await;
    });
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let mut held_copies = Vec::new();
    for index in 0..17 {
        let response = client
            .call_inline(&identity(), 0, b"response slot")
            .await
            .unwrap();
        assert!(matches!(response, ResponseData::Inline(_)));
        let bytes = response.into_owned_bytes().unwrap();
        assert_eq!(bytes, vec![index; 8192]);
        held_copies.push(bytes);
    }
    peer.await.unwrap();
    assert_eq!(pool.read().stats().alloc_count, 0);
    assert!(client.close_shared_bounded(WAIT).await);
    assert_eq!(held_copies.len(), 17);
}

#[tokio::test]
async fn admitted_response_error_never_enters_the_buddy_request_fallback() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let (request, bytes) = read_frame(&mut stream).await;
        assert!(request.is_buddy());
        let request_loan = decode_buddy_payload(&bytes).unwrap().0.loan_id;
        let invalid = encode_buddy_payload(&BuddyPayload {
            seg_idx: 0,
            generation: 1,
            offset: 0,
            data_size: 8193,
            is_dedicated: false,
            loan_id: 787,
        });
        stream
            .write_all(&frame::encode_frame_parts(
                request.request_id,
                flags::FLAG_RESPONSE | flags::FLAG_REPLY_V2 | flags::FLAG_BUDDY,
                &[&invalid, &[0]],
            ))
            .await
            .unwrap();
        stream
            .write_all(&c2_wire::release::encode_release_frame(
                &c2_wire::release::ReleaseBatch::new(&[request_loan]).unwrap(),
                true,
            ))
            .await
            .unwrap();
        loop {
            let mut header = [0; HEADER_SIZE];
            match timeout(WAIT, stream.read_exact(&mut header)).await.unwrap() {
                Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
                result => {
                    result.unwrap();
                }
            }
            let (length, body) = frame::decode_total_len(&header).unwrap();
            let (header, _) = frame::decode_frame_body(body, length).unwrap();
            assert!(
                header.is_ctrl(),
                "response admission error must never replay the call as chunks"
            );
            let mut payload = vec![0; header.payload_len()];
            stream.read_exact(&mut payload).await.unwrap();
        }
        crate::tests::close_listener(listener).await;
    });
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let pool = client.request_pool().unwrap();
    pool.write()
        .ensure_buddy_segments(1)
        .expect("real request SHM is required");
    let error = client
        .call_buddy(&identity(), 0, &[8; 8193], None)
        .await
        .unwrap_err();
    assert!(matches!(error.source_error(), IpcError::ResponseFailure(_)));
    assert_eq!(error.phase(), crate::TransportPhase::DispatchUncertain);
    assert!(client.close_shared_bounded(WAIT).await);
    assert_eq!(pool.read().stats().alloc_count, 0);
    peer.await.unwrap();
}

#[tokio::test]
async fn disabled_buddy_chunked_request_reassembles_into_real_dedicated_shm() {
    let endpoint = Endpoint::new();
    let mut server_config = ServerIpcConfig {
        shm_threshold: 64 * 1024,
        ..Default::default()
    };
    server_config.base.pool_enabled = false;
    let (server, runner) = start(&endpoint, Arc::new(Echo), server_config).await;
    let mut cfg = config();
    cfg.base.pool_enabled = false;
    let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
    client.connect().await.unwrap();
    let binding = client.acquire_route(&contract()).await.unwrap();
    let bytes = client
        .call_chunked(&binding.table.call_identity(), 0, &[8; 8193], None)
        .await
        .unwrap()
        .into_owned_bytes()
        .unwrap();
    assert_eq!(bytes, vec![8; 8193]);
    let snapshot = server.memory_budget_snapshot().budget;
    assert!(
        snapshot.shm.peak_bytes >= 8193,
        "real dedicated reassembly is required: {snapshot:?}"
    );
    assert_eq!(snapshot.reassembly.used_bytes, 0);
    assert_eq!(snapshot.shm.used_bytes, 0);
    assert_eq!(
        snapshot.file.peak_bytes, 0,
        "real dedicated reassembly is required; file fallback is not SHM evidence: {snapshot:?}"
    );
    assert_eq!(
        server.response_pool_arc().read().stats().dedicated_allocs,
        0
    );
    assert!(client.close_shared_bounded(WAIT).await);
    stop(server, runner).await;
}

#[tokio::test]
async fn disabled_buddy_response_exhaustion_falls_back_to_dedicated_chunk_reassembly() {
    let endpoint = Endpoint::new();
    let mut server_config = ServerIpcConfig {
        shm_threshold: 1,
        ..Default::default()
    };
    server_config.base.pool_enabled = false;
    server_config.base.chunk_size = 4096;
    let (server, runner) = start(&endpoint, Arc::new(Echo), server_config).await;
    let response_pool = server.response_pool_arc();
    let mut held_preparations = Vec::new();
    for _ in 0..4 {
        let prepared =
            c2_server::response::try_prepare_shm_response(&response_pool, 1, 8193, |bytes| {
                bytes.fill(1);
                Ok(())
            })
            .unwrap()
            .expect("real dedicated response SHM is required");
        held_preparations.push(prepared);
    }
    assert_eq!(response_pool.read().stats().dedicated_active_count, 4);
    let mut cfg = config();
    cfg.base.pool_enabled = false;
    let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
    client.connect().await.unwrap();
    let binding = client.acquire_route(&contract()).await.unwrap();
    assert_eq!(
        client
            .call_inline(&binding.table.call_identity(), 0, &[3; 8193])
            .await
            .unwrap()
            .into_owned_bytes()
            .unwrap(),
        vec![3; 8193]
    );
    let registry = client.chunk_registry.lock().clone().unwrap();
    let stats = registry.pool().read().stats();
    assert!(
        stats.dedicated_allocs > 0,
        "reply must reassemble in dedicated SHM: {stats:?}"
    );
    assert_eq!(stats.alloc_count, 0);
    assert_eq!(client.memory_budget.snapshot().reassembly.used_bytes, 0);
    assert_eq!(response_pool.read().stats().dedicated_active_count, 4);
    assert!(client.close_shared_bounded(WAIT).await);
    drop(held_preparations);
    assert_eq!(response_pool.read().stats().alloc_count, 0);
    stop(server, runner).await;
}

#[tokio::test]
async fn real_lazy_startup_sends_one_byte_without_mapping_buddy_backing() {
    let endpoint = Endpoint::new();
    let (server, runner) = start(&endpoint, Arc::new(Echo), ServerIpcConfig::default()).await;
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let binding = client.acquire_route(&contract()).await.unwrap();
    assert_eq!(client.request_pool().unwrap().read().segment_count(), 0);
    assert_eq!(
        client
            .call_bound(&binding, "echo", b"x")
            .await
            .unwrap()
            .into_owned_bytes()
            .unwrap(),
        b"x"
    );
    assert_eq!(client.request_pool().unwrap().read().segment_count(), 0);
    assert_eq!(server.response_pool_arc().read().segment_count(), 0);
    assert_eq!(client.memory_budget.snapshot().shm.used_bytes, 0);
    assert_eq!(server.memory_budget_snapshot().budget.shm.used_bytes, 0);
    assert!(client.close_shared_bounded(WAIT).await);
    stop(server, runner).await;
}

#[tokio::test]
async fn exhausted_reassembly_budget_returns_correlated_error_and_keeps_controls_usable() {
    let endpoint = Endpoint::new();
    let mut server_config = ServerIpcConfig::default();
    server_config.base.shm_backing_budget_bytes = 0;
    server_config.base.chunk_size = 4096;
    server_config.shm_threshold = 1;
    let (server, runner) = start(&endpoint, Arc::new(Echo), server_config).await;
    let mut cfg = config();
    cfg.base.shm_backing_budget_bytes = 0;
    cfg.base.live_reassembly_budget_bytes = 0;
    let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
    client.connect().await.unwrap();
    let binding = client.acquire_route(&contract()).await.unwrap();
    let connection = client.driver.read().clone().unwrap();
    let identity = binding.table.call_identity();
    let (rejected, other) = timeout(WAIT, async {
        tokio::join!(
            client.call_chunked(&identity, 0, &[3; 8193], None),
            client.call_bound(&binding, "echo", b"other")
        )
    })
    .await
    .unwrap();
    let error = rejected.unwrap_err();
    assert!(
        matches!(error.source_error(),
            IpcError::ResponseFailure(source) if matches!(source.as_ref(), IpcError::Chunk(ChunkError::Capacity(_)))
        ),
        "expected correlated response capacity error: {error:?}"
    );
    assert_eq!(error.phase(), crate::TransportPhase::DispatchUncertain);
    assert_eq!(other.unwrap().into_owned_bytes().unwrap(), b"other");
    assert_eq!(client.pending_len_for_test(), 0);
    assert!(client.is_connected());
    assert!(Arc::ptr_eq(
        client.driver.read().as_ref().unwrap(),
        &connection
    ));
    assert_eq!(client.memory_budget.snapshot().reassembly.used_bytes, 0);
    client.acquire_route(&contract()).await.unwrap();
    assert_eq!(
        client
            .call_bound(&binding, "echo", b"x")
            .await
            .unwrap()
            .into_owned_bytes()
            .unwrap(),
        b"x"
    );
    let ping_endpoint = endpoint.endpoint.clone();
    assert!(
        tokio::task::spawn_blocking(move || crate::control::ping_with_endpoint(
            &ping_endpoint,
            WAIT
        ))
        .await
        .unwrap()
        .unwrap()
    );
    assert!(client.close_shared_bounded(WAIT).await);
    stop(server, runner).await;
}

#[tokio::test]
async fn exhausted_request_reassembly_budget_returns_correlated_error_and_keeps_controls_usable() {
    let endpoint = Endpoint::new();
    let mut server_config = ServerIpcConfig::default();
    server_config.base.live_reassembly_budget_bytes = 0;
    let (server, runner) = start(&endpoint, Arc::new(Echo), server_config).await;
    let mut cfg = config();
    cfg.base.shm_backing_budget_bytes = 0;
    let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
    client.connect().await.unwrap();
    let binding = client.acquire_route(&contract()).await.unwrap();
    let error = timeout(
        WAIT,
        client.call_chunked(&binding.table.call_identity(), 0, &[3; 8193], None),
    )
    .await
    .unwrap()
    .unwrap_err();
    let IpcError::CrmError(bytes) = error.source_error() else {
        panic!("expected correlated budget error: {error:?}");
    };
    assert_eq!(
        C2Error::from_wire_bytes(bytes).unwrap().unwrap().code,
        ErrorCode::ResourceUnavailable
    );
    assert_eq!(client.pending_len_for_test(), 0);
    assert!(client.is_connected());
    client.acquire_route(&contract()).await.unwrap();
    assert_eq!(
        client
            .call_bound(&binding, "echo", b"x")
            .await
            .unwrap()
            .into_owned_bytes()
            .unwrap(),
        b"x"
    );
    let ping_endpoint = endpoint.endpoint.clone();
    assert!(
        tokio::task::spawn_blocking(move || crate::control::ping_with_endpoint(
            &ping_endpoint,
            WAIT
        ))
        .await
        .unwrap()
        .unwrap()
    );
    assert!(client.close_shared_bounded(WAIT).await);
    stop(server, runner).await;
}
