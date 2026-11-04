use super::*;

#[tokio::test]
async fn close_preserves_started_chunked_upload_and_reply() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let (first_sent, first_seen) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let (first, payload) = read_frame(&mut stream).await;
        assert_eq!(
            first.flags,
            flags::FLAG_RPC_RECORD | flags::FLAG_FRAGMENTED | flags::FLAG_FIRST_FRAGMENT
        );
        assert!(matches!(
            decode_rpc_record(&first, &payload, None).unwrap(),
            RpcRecord::Open { .. }
        ));
        first_sent.send(()).unwrap();
        for expected in 1..3 {
            let (next, payload) = read_frame(&mut stream).await;
            assert_eq!(next.request_id, first.request_id);
            assert_eq!(next.flags, flags::FLAG_RPC_RECORD | flags::FLAG_FRAGMENTED);
            assert!(
                matches!(decode_rpc_record(&next, &payload, None).unwrap(), RpcRecord::OpenContinuation { value: ValuePart { storage: ValueStorage::Inline(bytes), .. } } if bytes.iter().all(|byte| *byte == expected as u8 + 1))
            );
        }
        reply(&mut stream, first.request_id, b"all chunks executed").await;
        expect_disconnect_or_eof(&mut stream).await;
        crate::tests::close_listener(listener).await;
    });
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let client = Arc::new(client);
    let (source, receiver) = tokio::sync::mpsc::channel::<Vec<u8>>(2);
    let chunks = futures_util::stream::unfold(receiver, |mut receiver| async {
        receiver
            .recv()
            .await
            .map(|bytes| (Ok::<_, std::io::Error>(bytes), receiver))
    });
    let caller = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .call_chunked_stream_phased(&identity(), 0, 3 * 4096, chunks)
                .await
        }
    });
    source.send(vec![1; 4096]).await.unwrap();
    first_seen.await.unwrap();
    assert!(!client.close_shared_bounded(Duration::from_millis(20)).await);
    assert!(!caller.is_finished());
    let error = client
        .call_inline(&identity(), 0, b"new")
        .await
        .unwrap_err();
    assert_eq!(error.phase(), crate::TransportPhase::PreDispatch);
    source.send(vec![2; 4096]).await.unwrap();
    source.send(vec![3; 4096]).await.unwrap();
    drop(source);
    assert_eq!(
        timeout(WAIT, caller)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_owned_bytes()
            .unwrap(),
        b"all chunks executed"
    );
    assert!(client.close_shared_bounded(WAIT).await);
    peer.await.unwrap();
}

#[tokio::test]
async fn deactivation_wakes_calls_waiting_for_full_writer_queue() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let (first, _) = read_frame(&mut stream).await;
        reply(&mut stream, first.request_id, b"only dispatched call").await;
        expect_disconnect_or_eof(&mut stream).await;
        crate::tests::close_listener(listener).await;
    });
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let conn = client.driver.read().clone().unwrap();
    let client = Arc::new(client);
    let (entered, paused) = oneshot::channel();
    let (resume, resumed) = oneshot::channel();
    *conn.pause.lock() = Some((entered, resumed));
    let first = tokio::spawn({
        let client = client.clone();
        async move { client.call_inline(&identity(), 0, b"first").await }
    });
    paused.await.unwrap();
    let id = identity();
    let mut waiting = (0..66)
        .map(|_| Box::pin(client.call_inline(&id, 0, b"queued")))
        .collect::<Vec<_>>();
    for call in &mut waiting {
        assert!(futures_util::poll!(call.as_mut()).is_pending());
    }
    assert_eq!(conn.queue_capacity_for_test(), 0);
    assert_eq!(client.pending_len_for_test(), 67);
    client.deactivate();
    for call in waiting {
        let error = timeout(WAIT, call)
            .await
            .expect("queue admission must wake on deactivation")
            .unwrap_err();
        assert_eq!(error.phase(), crate::TransportPhase::PreDispatch);
    }
    assert!(!first.is_finished());
    assert_eq!(client.pending_len_for_test(), 1);
    resume.send(()).unwrap();
    assert_eq!(
        timeout(WAIT, first)
            .await
            .unwrap()
            .unwrap()
            .unwrap()
            .into_owned_bytes()
            .unwrap(),
        b"only dispatched call"
    );
    assert!(client.close_shared_bounded(WAIT).await);
    peer.await.unwrap();
}

#[tokio::test]
async fn physical_end_releases_peer_mappings_with_retained_client() {
    let endpoint = Endpoint::new();
    let (server, runner) = start(
        &endpoint,
        Arc::new(Echo),
        ServerIpcConfig {
            shm_threshold: 1,
            ..Default::default()
        },
    )
    .await;
    let mut cfg = config();
    cfg.pool_decay_seconds = 3600.0;
    let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
    client.connect().await.unwrap();
    let binding = client.acquire_route(&contract()).await.unwrap();
    let peer = client.server_pool.lock().clone().unwrap();
    let held = client
        .call_bound(&binding, "echo", &[9; 8193])
        .await
        .unwrap()
        .into_owned_bytes()
        .unwrap();
    assert!(peer.read().cache.as_ref().unwrap().mapped_segments() > 0);
    let observation = client.connection_observation().unwrap();
    client.deactivate();
    assert!(observation.wait_closed(Some(WAIT)).await.is_some());
    assert!(
        peer.read().cache.is_none(),
        "physical close must drop the cache before reporting completion, even without close_shared_bounded"
    );
    assert!(
        client.server_pool.lock().is_some(),
        "business metadata is still retained"
    );
    assert_eq!(held, vec![9; 8193]);
    stop(server, runner).await;
}
