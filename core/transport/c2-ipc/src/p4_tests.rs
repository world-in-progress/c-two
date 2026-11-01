use super::*;
use c2_config::LocalEndpointContext;
use c2_local::LocalListener;
use c2_server::{
    ConcurrencyMode, CrmCallback, CrmError, ResponseMeta, RouteBuildSpec, SchedulerLimits, Server,
    ServerIpcConfig,
};
use c2_wire::control::encode_reply_control;
use c2_wire::handshake::{ServerIdentity, encode_server_handshake};
use std::time::Duration;
use tokio::time::timeout;

const WAIT: Duration = Duration::from_secs(5);

struct Endpoint {
    _root: tempfile::TempDir,
    endpoint: LocalEndpoint,
}
impl Endpoint {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("c2p4")
            .tempdir_in(std::env::temp_dir())
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
                "ipc://p4-{}",
                CLIENT_CONN_COUNTER.fetch_add(1, Ordering::Relaxed)
            ))
            .unwrap();
        Self {
            _root: root,
            endpoint,
        }
    }
    fn client(&self) -> IpcClient {
        IpcClient::with_endpoint(self.endpoint.clone(), config()).unwrap()
    }
}
fn config() -> ClientIpcConfig {
    ClientIpcConfig {
        base: c2_config::BaseIpcConfig {
            pool_segment_size: 64 * 1024,
            max_pool_segments: 2,
            chunk_size: 4096,
            ..Default::default()
        },
        shm_threshold: 1,
        ..Default::default()
    }
}
fn identity() -> RouteCallIdentity {
    RouteCallIdentity {
        route_name: "grid".into(),
        route_uid: "grid-uid".into(),
        crm_ns: "test.p4".into(),
        crm_name: "Grid".into(),
        crm_ver: "0.1.0".into(),
        abi_hash: "01".repeat(32),
        signature_hash: "02".repeat(32),
    }
}
fn contract() -> c2_contract::ExpectedRouteContract {
    let id = identity();
    c2_contract::ExpectedRouteContract {
        route_name: id.route_name,
        crm_ns: id.crm_ns,
        crm_name: id.crm_name,
        crm_ver: id.crm_ver,
        abi_hash: id.abi_hash,
        signature_hash: id.signature_hash,
    }
}
async fn read_frame(stream: &mut LocalStream) -> (FrameHeader, Vec<u8>) {
    let mut header = [0; HEADER_SIZE];
    timeout(WAIT, stream.read_exact(&mut header))
        .await
        .unwrap()
        .unwrap();
    let (len, body) = frame::decode_total_len(&header).unwrap();
    let (header, _) = frame::decode_frame_body(body, len).unwrap();
    let mut payload = vec![0; header.payload_len()];
    timeout(WAIT, stream.read_exact(&mut payload))
        .await
        .unwrap()
        .unwrap();
    (header, payload)
}
async fn accept(listener: &mut LocalListener) -> LocalStream {
    let mut stream = timeout(WAIT, listener.accept()).await.unwrap().unwrap();
    crate::tests::accept_version_preamble(&mut stream).await;
    let (_, payload) = read_frame(&mut stream).await;
    let hs = decode_handshake(&payload, false).unwrap();
    let payload = encode_server_handshake(
        hs.capability_flags,
        &[] as &[RouteInfo],
        "/p4peer",
        &ServerIdentity {
            server_id: "p4-server".to_string(),
            server_instance_id: "p4-instance".to_string(),
        },
    )
    .unwrap();
    stream
        .write_all(&frame::encode_frame(
            0,
            flags::FLAG_HANDSHAKE | flags::FLAG_RESPONSE,
            &payload,
        ))
        .await
        .unwrap();
    stream
}
async fn reply(stream: &mut LocalStream, rid: u64, data: &[u8]) {
    let ctrl = encode_reply_control(&ReplyControl::Success);
    stream
        .write_all(&frame::encode_frame_parts(
            rid,
            flags::FLAG_RESPONSE | flags::FLAG_REPLY_V2,
            &[&ctrl, data],
        ))
        .await
        .unwrap();
}
async fn eventually(mut check: impl FnMut() -> bool) {
    timeout(WAIT, async {
        while !check() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn response_does_not_release_a_request_loan_and_peer_end_does() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let (close, closed) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let (hdr, payload) = read_frame(&mut stream).await;
        assert!(hdr.is_buddy());
        assert_ne!(decode_buddy_payload(&payload).unwrap().0.loan_id, 0);
        reply(&mut stream, hdr.request_id, b"ok").await;
        closed.await.unwrap();
        drop(stream);
        crate::tests::close_listener(listener).await;
    });
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let pool = client.request_pool().unwrap();
    pool.write()
        .ensure_buddy_segments(1)
        .expect("this test requires real request SHM backing");
    assert_eq!(
        client
            .call_buddy(&identity(), 0, &[7; 8193], None)
            .await
            .unwrap()
            .into_owned_bytes()
            .unwrap(),
        b"ok"
    );
    assert_eq!(
        pool.read().stats().alloc_count,
        1,
        "a reply is not release authority"
    );
    close.send(()).unwrap();
    peer.await.unwrap();
    eventually(|| pool.read().stats().alloc_count == 0).await;
    assert!(client.close_shared_bounded(WAIT).await);
}

#[tokio::test]
async fn high_request_bits_are_preserved_through_pending_and_delivery() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let (received, first) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let (low, _) = read_frame(&mut stream).await;
        assert_eq!(low.request_id, 7);
        received.send(()).unwrap();
        let (high, _) = read_frame(&mut stream).await;
        assert_eq!(high.request_id, (1 << 32) | 7);
        reply(&mut stream, high.request_id, b"high").await;
        reply(&mut stream, low.request_id, b"low").await;
        let _ = stream.read(&mut [0]).await;
        crate::tests::close_listener(listener).await;
    });
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    client.rid_counter.store(6, Ordering::Relaxed);
    let client = Arc::new(client);
    let low = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .call_inline(&identity(), 0, b"low")
                .await
                .unwrap()
                .into_owned_bytes()
                .unwrap()
        }
    });
    first.await.unwrap();
    client.rid_counter.store((1 << 32) | 6, Ordering::Relaxed);
    let high = client
        .call_inline(&identity(), 0, b"high")
        .await
        .unwrap()
        .into_owned_bytes()
        .unwrap();
    assert_eq!(high, b"high");
    assert_eq!(low.await.unwrap(), b"low");
    assert!(client.close_shared_bounded(WAIT).await);
    peer.await.unwrap();
}

#[tokio::test]
async fn exhausted_request_ids_deactivate_without_reconnect_or_any_frame() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        assert_eq!(
            timeout(WAIT, stream.read(&mut [0])).await.unwrap().unwrap(),
            0
        );
        crate::tests::close_listener(listener).await;
    });
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let id = client.connection_observation().unwrap().connection_id();
    client.rid_counter.store(u64::MAX, Ordering::Relaxed);
    let error = client
        .call_inline(&identity(), 0, b"unpublished")
        .await
        .unwrap_err();
    assert_eq!(error.phase(), crate::TransportPhase::PreDispatch);
    assert!(
        matches!(error.source_error(), IpcError::Protocol(error) if error.contains("request ID space exhausted"))
    );
    assert!(!client.is_connected());
    let error = client
        .call_inline(&identity(), 0, b"again")
        .await
        .unwrap_err();
    assert_eq!(error.phase(), crate::TransportPhase::PreDispatch);
    assert!(matches!(error.source_error(), IpcError::Closed));
    assert_eq!(client.connection_observation().unwrap().connection_id(), id);
    assert_eq!(client.pending_len_for_test(), 0);
    peer.await.unwrap();
}

#[tokio::test]
async fn cancellation_before_writer_publication_releases_ordinary_and_stream_preparation() {
    // Adapt the original R13 experiment (89b20a0f): park the sole writer,
    // poll the next prepared send to Pending, cancel it, inspect the real pool.
    for streaming in [false, true] {
        let endpoint = Endpoint::new();
        let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
        let peer = tokio::spawn(async move {
            let mut stream = accept(&mut listener).await;
            let (hdr, _) = read_frame(&mut stream).await;
            reply(&mut stream, hdr.request_id, b"first").await;
            assert_eq!(
                timeout(WAIT, stream.read(&mut [0])).await.unwrap().unwrap(),
                0
            );
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
        let pool = client.request_pool().unwrap();
        pool.write()
            .ensure_buddy_segments(1)
            .expect("this test requires real request SHM backing");
        pool.write()
            .ensure_buddy_segments(1)
            .expect("this test requires real request SHM backing");
        if streaming {
            let block = client.try_alloc_request_block(8193).unwrap().unwrap();
            let id = identity();
            let mut call = Box::pin(client.call_buddy_stream(
                &id,
                0,
                block,
                8193,
                futures_util::stream::iter([Ok::<_, std::io::Error>(vec![3; 8193])]),
            ));
            assert!(futures_util::poll!(call.as_mut()).is_pending());
            assert_eq!(pool.read().stats().alloc_count, 1);
            drop(call);
        } else {
            let id = identity();
            let mut call = Box::pin(client.call_buddy(&id, 0, &[3; 8193], None));
            assert!(futures_util::poll!(call.as_mut()).is_pending());
            assert_eq!(pool.read().stats().alloc_count, 1);
            drop(call);
        }
        assert_eq!(pool.read().stats().alloc_count, 0);
        assert!(
            !conn.core.is_deactivated(),
            "unpublished cancellation has no effect on the earlier frame"
        );
        resume.send(()).unwrap();
        assert_eq!(first.await.unwrap().into_owned_bytes().unwrap(), b"first");
        assert!(client.close_shared_bounded(WAIT).await);
        peer.await.unwrap();
    }
}

struct Echo;
impl CrmCallback for Echo {
    fn invoke(
        &self,
        _: &str,
        _: u16,
        data: Vec<u8>,
        _: Arc<c2_server::ResponsePool>,
    ) -> Result<ResponseMeta, CrmError> {
        Ok(ResponseMeta::Inline(data))
    }
}
async fn start(
    endpoint: &Endpoint,
    callback: Arc<dyn CrmCallback>,
    mut config: ServerIpcConfig,
) -> (Arc<Server>, tokio::task::JoinHandle<()>) {
    config.base.pool_segment_size = 64 * 1024;
    config.base.max_pool_segments = 2;
    let server = Arc::new(Server::new_with_endpoint(endpoint.endpoint.clone(), config).unwrap());
    let id = contract();
    let built = server
        .build_route(
            RouteBuildSpec {
                name: id.route_name,
                crm_ns: id.crm_ns,
                crm_name: id.crm_name,
                crm_ver: id.crm_ver,
                abi_hash: id.abi_hash,
                signature_hash: id.signature_hash,
                method_names: vec!["echo".into()],
                access_map: HashMap::new(),
                concurrency_mode: ConcurrencyMode::Parallel,
                limits: SchedulerLimits::default(),
            },
            callback,
        )
        .unwrap();
    let reservation = server.reserve_route(built).await.unwrap();
    server.commit_reserved_route(reservation).await.unwrap();
    let task = tokio::spawn({
        let server = server.clone();
        async move {
            server.run().await.unwrap();
        }
    });
    server.wait_until_ready(WAIT).await.unwrap();
    (server, task)
}
async fn stop(server: Arc<Server>, task: tokio::task::JoinHandle<()>) {
    server.shutdown_and_wait(WAIT).await.unwrap();
    timeout(WAIT, task).await.unwrap().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn real_server_releases_request_before_slow_callback() {
    struct Slow {
        entered: StdMutex<Option<oneshot::Sender<()>>>,
        gate: (std::sync::Mutex<bool>, std::sync::Condvar),
    }
    impl CrmCallback for Slow {
        fn invoke(
            &self,
            _: &str,
            _: u16,
            data: Vec<u8>,
            _: Arc<c2_server::ResponsePool>,
        ) -> Result<ResponseMeta, CrmError> {
            self.entered.lock().take().unwrap().send(()).unwrap();
            let mut gate = self.gate.0.lock().unwrap();
            while !*gate {
                gate = self.gate.1.wait(gate).unwrap();
            }
            Ok(ResponseMeta::Inline(data))
        }
    }
    let endpoint = Endpoint::new();
    let (entered, ready) = oneshot::channel();
    let slow = Arc::new(Slow {
        entered: StdMutex::new(Some(entered)),
        gate: (std::sync::Mutex::new(false), std::sync::Condvar::new()),
    });
    let (server, runner) = start(
        &endpoint,
        slow.clone(),
        ServerIpcConfig {
            shm_threshold: 1,
            ..Default::default()
        },
    )
    .await;
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let binding = client.acquire_route(&contract()).await.unwrap();
    let pool = client.request_pool().unwrap();
    pool.write()
        .ensure_buddy_segments(1)
        .expect("this test requires real request SHM backing");
    let client = Arc::new(client);
    let call = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .call_bound(&binding, "echo", &[8; 8193])
                .await
                .unwrap()
        }
    });
    timeout(WAIT, ready).await.unwrap().unwrap();
    let released = timeout(WAIT, async {
        while pool.read().stats().alloc_count != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await;
    *slow.gate.0.lock().unwrap() = true;
    slow.gate.1.notify_all();
    released.expect("client must receive the request release while callback is still blocked");
    assert_eq!(
        call.await.unwrap().into_owned_bytes().unwrap(),
        vec![8; 8193]
    );
    assert!(client.close_shared_bounded(WAIT).await);
    stop(server, runner).await;
}

#[tokio::test]
async fn real_response_is_released_before_local_copy_is_held_and_close() {
    for dedicated in [false, true] {
        let endpoint = Endpoint::new();
        let mut server_config = ServerIpcConfig {
            shm_threshold: 1,
            ..Default::default()
        };
        server_config.base.pool_enabled = !dedicated;
        let (server, runner) = start(&endpoint, Arc::new(Echo), server_config).await;
        let mut client_config = config();
        client_config.base.pool_enabled = !dedicated;
        let mut client =
            IpcClient::with_endpoint(endpoint.endpoint.clone(), client_config).unwrap();
        client.connect().await.unwrap();
        let binding = client.acquire_route(&contract()).await.unwrap();
        let response = client
            .call_bound(&binding, "echo", &[5; 8193])
            .await
            .unwrap();
        assert!(
            matches!(response, ResponseData::Inline(_)),
            "no transport lease escapes a call"
        );
        let held_copy = response.into_owned_bytes().unwrap();
        let stats = server.response_pool_arc().read().stats();
        assert!(
            stats.buddy_expanded_allocs + stats.buddy_reused_allocs + stats.dedicated_allocs > 0,
            "this test requires an actual server SHM response: {stats:?}"
        );
        eventually(|| server.response_pool_arc().read().stats().alloc_count == 0).await;
        assert!(client.close_shared_bounded(WAIT).await);
        stop(server, runner).await;
        assert_eq!(held_copy, vec![5; 8193]);
    }
}

#[tokio::test]
async fn real_chunk_fallback_returns_reassembly_budget_before_delivery() {
    let endpoint = Endpoint::new();
    let mut server_config = ServerIpcConfig {
        shm_threshold: 1,
        ..Default::default()
    };
    server_config.base.shm_backing_budget_bytes = 0;
    server_config.base.chunk_size = 4096;
    let (server, runner) = start(&endpoint, Arc::new(Echo), server_config).await;
    let mut config = config();
    config.base.shm_backing_budget_bytes = 0;
    let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), config).unwrap();
    client.connect().await.unwrap();
    let binding = client.acquire_route(&contract()).await.unwrap();
    let data = vec![9; 8193];
    assert_eq!(
        client
            .call_bound(&binding, "echo", &data)
            .await
            .unwrap()
            .into_owned_bytes()
            .unwrap(),
        data
    );
    assert_eq!(client.memory_budget.snapshot().reassembly.used_bytes, 0);
    assert_eq!(client.memory_budget.snapshot().shm.used_bytes, 0);
    assert!(client.close_shared_bounded(WAIT).await);
    stop(server, runner).await;
}

#[tokio::test]
async fn cancelling_a_started_shm_write_keeps_the_driver_and_never_replays() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let (hdr, payload) = read_frame(&mut stream).await;
        assert!(hdr.is_buddy());
        let loan = decode_buddy_payload(&payload).unwrap().0.loan_id;
        let release = c2_wire::release::encode_release_frame(
            &c2_wire::release::ReleaseBatch::new(&[loan]).unwrap(),
            true,
        );
        stream.write_all(&release).await.unwrap();
        assert_eq!(
            timeout(WAIT, stream.read(&mut [0])).await.unwrap().unwrap(),
            0,
            "exactly one complete request was written"
        );
        crate::tests::close_listener(listener).await;
    });
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let pool = client.request_pool().unwrap();
    pool.write()
        .ensure_buddy_segments(1)
        .expect("this test requires real request SHM backing");
    let conn = client.driver.read().clone().unwrap();
    let client = Arc::new(client);
    let (entered, paused) = oneshot::channel();
    let (resume, resumed) = oneshot::channel();
    *conn.pause.lock() = Some((entered, resumed));
    let caller = tokio::spawn({
        let client = client.clone();
        async move { client.call_buddy(&identity(), 0, &[7; 8193], None).await }
    });
    paused.await.unwrap();
    assert_eq!(pool.read().stats().alloc_count, 1);
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    assert!(!conn.core.is_deactivated());
    assert!(
        client
            .connection_observation()
            .unwrap()
            .snapshot()
            .is_none(),
        "a started frame still owns the physical stream"
    );
    assert_eq!(pool.read().stats().alloc_count, 1);
    resume.send(()).unwrap();
    eventually(|| pool.read().stats().alloc_count == 0).await;
    assert!(client.is_connected());
    assert!(client.close_shared_bounded(WAIT).await);
    timeout(WAIT, peer).await.unwrap().unwrap();
    assert_eq!(pool.read().stats().alloc_count, 0);
}

#[tokio::test]
async fn r13_prepared_reply_drop_before_publication_releases_buddy_and_dedicated() {
    struct Discard;
    impl CrmCallback for Discard {
        fn invoke(
            &self,
            _: &str,
            _: u16,
            _: Vec<u8>,
            pool: Arc<c2_server::ResponsePool>,
        ) -> Result<ResponseMeta, CrmError> {
            // The original R13 cancellation point was after preparation and
            // before writer publication. The new move-only preparer owns Drop
            // at that point; the server writer's internals are outside P4.
            for size in [8193, 65537] {
                let prepared =
                    c2_server::response::try_prepare_shm_response(&pool, 1, size, |bytes| {
                        bytes.fill(1);
                        Ok(())
                    })
                    .unwrap()
                    .unwrap();
                assert_eq!(pool.read().stats().alloc_count, 1);
                drop(prepared);
                assert_eq!(pool.read().stats().alloc_count, 0);
            }
            Ok(ResponseMeta::Inline(vec![1]))
        }
    }
    let endpoint = Endpoint::new();
    let (server, runner) = start(&endpoint, Arc::new(Discard), ServerIpcConfig::default()).await;
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let binding = client.acquire_route(&contract()).await.unwrap();
    assert_eq!(
        timeout(WAIT, client.call_bound(&binding, "echo", &[]))
            .await
            .expect("prepared reply must complete")
            .unwrap()
            .into_owned_bytes()
            .unwrap(),
        [1]
    );
    assert!(client.close_shared_bounded(WAIT).await);
    stop(server, runner).await;
}

// Retained route, configuration and call-policy regression coverage.
#[test]
fn reassembly_pool_unique_prefixes() {
    let cfg = ClientIpcConfig::default();
    let budget = c2_mem::MemoryBudget::from_limits(&cfg.memory_budget_limits());
    let r1 = IpcClient::build_chunk_registry(&cfg, &budget).unwrap();
    let r2 = IpcClient::build_chunk_registry(&cfg, &budget).unwrap();
    let r3 = IpcClient::build_chunk_registry(&cfg, &budget).unwrap();
    let prefix1 = r1.pool().read().prefix().to_string();
    let prefix2 = r2.pool().read().prefix().to_string();
    let prefix3 = r3.pool().read().prefix().to_string();
    assert_ne!(prefix1, prefix2);
    assert_ne!(prefix2, prefix3);
    assert_ne!(prefix1, prefix3);
    assert!(prefix1.starts_with("/cc3a"), "unexpected prefix: {prefix1}");
    assert!(
        prefix1.len() <= c2_contract::MAX_WIRE_TEXT_BYTES,
        "prefix exceeds handshake text limit: {}",
        prefix1.len()
    );
}

#[test]
fn client_projects_server_identity() {
    let identity = c2_wire::handshake::ServerIdentity {
        server_id: "identity-server".to_string(),
        server_instance_id: "identity-instance".to_string(),
    };
    let mut client =
        IpcClient::with_config("ipc://identity_projection", ClientIpcConfig::default()).unwrap();
    client.server_identity = Some(identity.clone());

    assert_eq!(client.server_identity(), Some(&identity));
    assert_eq!(client.server_id(), Some("identity-server"));
    assert_eq!(client.server_instance_id(), Some("identity-instance"));
}

#[test]
fn route_binding_keeps_acquired_token_after_directory_update() {
    const ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SIG_HASH: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
    let client =
        IpcClient::with_config("ipc://route_binding_projection", ClientIpcConfig::default())
            .unwrap();
    client.route_directory.write().insert_table(
        "grid".to_string(),
        MethodTable::from_entries(
            &[MethodEntry {
                name: "ping".to_string(),
                index: 0,
            }],
            RouteCallIdentity {
                route_name: "grid".to_string(),
                route_uid: "grid-route-uid-0001".to_string(),
                crm_ns: "test.grid".to_string(),
                crm_name: "Grid".to_string(),
                crm_ver: "0.1.0".to_string(),
                abi_hash: ABI_HASH.to_string(),
                signature_hash: SIG_HASH.to_string(),
            },
            1024,
        ),
    );
    let expected = c2_contract::ExpectedRouteContract {
        route_name: "grid".to_string(),
        crm_ns: "test.grid".to_string(),
        crm_name: "Grid".to_string(),
        crm_ver: "0.1.0".to_string(),
        abi_hash: ABI_HASH.to_string(),
        signature_hash: SIG_HASH.to_string(),
    };
    let binding = client
        .bind_cached_route(&expected)
        .expect("initial route should bind");

    client.route_directory.write().insert_table(
        "grid".to_string(),
        MethodTable::from_entries(
            &[MethodEntry {
                name: "ping".to_string(),
                index: 0,
            }],
            RouteCallIdentity {
                route_name: "grid".to_string(),
                route_uid: "grid-route-uid-0002".to_string(),
                crm_ns: "test.grid".to_string(),
                crm_name: "Grid".to_string(),
                crm_ver: "0.1.0".to_string(),
                abi_hash: ABI_HASH.to_string(),
                signature_hash: SIG_HASH.to_string(),
            },
            1024,
        ),
    );

    let (_, identity, _) = binding
        .call_target_for("ping")
        .expect("bound method should still exist");
    assert_eq!(identity.route_uid, "grid-route-uid-0001");
    let current_binding = client
        .bind_cached_route(&expected)
        .expect("directory should expose current route");
    assert_eq!(current_binding.route_uid(), "grid-route-uid-0002");
}

#[test]
fn exact_token_binding_rejects_same_name_replacement() {
    const ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SIG_HASH: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
    let client = IpcClient::with_config(
        "ipc://route_binding_stale_projection",
        ClientIpcConfig::default(),
    )
    .unwrap();
    client.route_directory.write().insert_table(
        "grid".to_string(),
        MethodTable::from_entries(
            &[MethodEntry {
                name: "ping".to_string(),
                index: 0,
            }],
            RouteCallIdentity {
                route_name: "grid".to_string(),
                route_uid: "grid-route-uid-0002".to_string(),
                crm_ns: "test.grid".to_string(),
                crm_name: "Grid".to_string(),
                crm_ver: "0.1.0".to_string(),
                abi_hash: ABI_HASH.to_string(),
                signature_hash: SIG_HASH.to_string(),
            },
            1024,
        ),
    );
    let expected = c2_contract::ExpectedRouteContract {
        route_name: "grid".to_string(),
        crm_ns: "test.grid".to_string(),
        crm_name: "Grid".to_string(),
        crm_ver: "0.1.0".to_string(),
        abi_hash: ABI_HASH.to_string(),
        signature_hash: SIG_HASH.to_string(),
    };

    let err = client
        .bind_cached_route_token(&expected, "grid-route-uid-0001")
        .expect_err("exact token binding must reject replacement route");

    assert!(
        matches!(
            &err,
            IpcError::RouteStale {
                route_name,
                current_route_uid,
            } if route_name == "grid"
                && current_route_uid == "grid-route-uid-0002"
        ),
        "unexpected error: {err:?}"
    );
}

#[test]
fn client_validates_route_crm_contract_from_handshake_metadata() {
    const ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SIG_HASH: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
    let client =
        IpcClient::with_config("ipc://contract_projection", ClientIpcConfig::default()).unwrap();
    let mut methods = HashMap::new();
    methods.insert("ping".to_string(), 0);
    client.route_directory.write().insert_table(
        "grid".to_string(),
        MethodTable {
            route_name: "grid".to_string(),
            route_uid: "grid-route-uid-0001".to_string(),
            crm_ns: "test.grid".to_string(),
            crm_name: "Grid".to_string(),
            crm_ver: "0.1.0".to_string(),
            abi_hash: ABI_HASH.to_string(),
            signature_hash: SIG_HASH.to_string(),
            max_payload_size: 1024,
            name_to_idx: methods,
        },
    );
    let expected = c2_contract::ExpectedRouteContract {
        route_name: "grid".to_string(),
        crm_ns: "test.grid".to_string(),
        crm_name: "Grid".to_string(),
        crm_ver: "0.1.0".to_string(),
        abi_hash: ABI_HASH.to_string(),
        signature_hash: SIG_HASH.to_string(),
    };

    client
        .validate_route_contract(&expected)
        .expect("matching CRM contract should be accepted");

    let mut mismatched_name = expected.clone();
    mismatched_name.crm_name = "OtherGrid".to_string();
    let err = client
        .validate_route_contract(&mismatched_name)
        .expect_err("mismatched CRM name should be rejected");
    assert!(
        err.to_string().contains("CRM contract mismatch"),
        "unexpected error: {err}"
    );

    let mut mismatched_hash = expected;
    mismatched_hash.signature_hash =
        "1111111111111111111111111111111111111111111111111111111111111111".to_string();
    let err = client
        .validate_route_contract(&mismatched_hash)
        .expect_err("mismatched signature hash should be rejected");
    assert!(
        err.to_string().contains("signature_hash"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn client_rejects_request_over_route_payload_limit_before_transport() {
    const ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SIG_HASH: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
    let client = IpcClient::with_config("ipc://payload_limit", ClientIpcConfig::default()).unwrap();
    let mut methods = HashMap::new();
    methods.insert("ping".to_string(), 0);
    client.route_directory.write().insert_table(
        "grid".to_string(),
        MethodTable {
            route_name: "grid".to_string(),
            route_uid: "grid-route-uid-0001".to_string(),
            crm_ns: "test.grid".to_string(),
            crm_name: "Grid".to_string(),
            crm_ver: "0.1.0".to_string(),
            abi_hash: ABI_HASH.to_string(),
            signature_hash: SIG_HASH.to_string(),
            max_payload_size: 4,
            name_to_idx: methods,
        },
    );

    let binding = client
        .bind_cached_route(&c2_contract::ExpectedRouteContract {
            route_name: "grid".to_string(),
            crm_ns: "test.grid".to_string(),
            crm_name: "Grid".to_string(),
            crm_ver: "0.1.0".to_string(),
            abi_hash: ABI_HASH.to_string(),
            signature_hash: SIG_HASH.to_string(),
        })
        .expect("cached test route should bind");

    let err = client
        .call_bound(&binding, "ping", b"12345")
        .await
        .expect_err("oversized bound call should be rejected before writer access");
    assert!(
        matches!(err, IpcError::Config(_)),
        "unexpected error: {err:?}"
    );
    assert!(err.to_string().contains("max_payload_size"));

    let stream = futures_util::stream::once(async {
        panic!("oversized sized stream should not be polled");
        #[allow(unreachable_code)]
        Ok::<&'static [u8], std::io::Error>(b"12345")
    });
    let err = client
        .call_bound_sized_stream(&binding, "ping", 5, stream)
        .await
        .expect_err("oversized streaming call should be rejected before body polling");
    assert!(
        matches!(err, IpcError::Config(_)),
        "unexpected error: {err:?}"
    );
    assert!(err.to_string().contains("max_payload_size"));
}

#[tokio::test]
async fn client_rejects_path_like_ipc_region_before_connecting() {
    for address in [
        "ipc://../escape",
        "ipc://bad/name",
        "ipc://bad\\name",
        "ipc://.",
        "ipc://..",
        "ipc:// leading",
        "ipc://trailing ",
        "ipc://bad\nname",
        "tcp://not-ipc",
    ] {
        let mut client = IpcClient::with_config(address, ClientIpcConfig::default()).unwrap();
        let error = client
            .connect()
            .await
            .expect_err("invalid IPC address should not attempt UDS connect");
        assert!(
            matches!(error, IpcError::Config(_)),
            "expected config error for {address:?}, got {error:?}"
        );
    }
}

fn cancellation_client(label: &str) -> (IpcClient, RouteBinding, Arc<RwLock<MemPool>>) {
    const ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SIG_HASH: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
    let cfg = ClientIpcConfig {
        shm_threshold: 10,
        base: c2_config::BaseIpcConfig {
            pool_segment_size: 65_536,
            max_pool_segments: 1,
            ..c2_config::BaseIpcConfig::default()
        },
        ..ClientIpcConfig::default()
    };
    let client = IpcClient::with_config(&format!("ipc://{label}"), cfg).unwrap();
    client.negotiated_shm_request.store(true, Ordering::Release);
    let mut methods = HashMap::new();
    methods.insert("ping".to_string(), 0);
    client.route_directory.write().insert_table(
        "grid".to_string(),
        MethodTable {
            route_name: "grid".to_string(),
            route_uid: "grid-route-uid-0001".to_string(),
            crm_ns: "test.grid".to_string(),
            crm_name: "Grid".to_string(),
            crm_ver: "0.1.0".to_string(),
            abi_hash: ABI_HASH.to_string(),
            signature_hash: SIG_HASH.to_string(),
            max_payload_size: 1024,
            name_to_idx: methods,
        },
    );
    let binding = client
        .bind_cached_route(&c2_contract::ExpectedRouteContract {
            route_name: "grid".to_string(),
            crm_ns: "test.grid".to_string(),
            crm_name: "Grid".to_string(),
            crm_ver: "0.1.0".to_string(),
            abi_hash: ABI_HASH.to_string(),
            signature_hash: SIG_HASH.to_string(),
        })
        .expect("cached test route should bind");
    let pool = client.request_pool().expect("transport-owned request pool");
    (client, binding, pool)
}

#[tokio::test]
async fn controlled_prepare_rejection_is_pure_and_preserves_canonical_error() {
    let (client, binding, pool) = cancellation_client("controlled_pure_prepare");
    let expected = C2Error::new(ErrorCode::CallDeadlineExceeded, "fake deadline").with_details(
        std::collections::BTreeMap::from([("call_id".into(), "pure-17".into())]),
    );
    for reject_at in [1, 2] {
        let checks = Arc::new(AtomicU64::new(0));
        let control = IpcCallControl::new(|| panic!("no dispatch permission before preparation"))
            .with_before_prepare({
                let checks = Arc::clone(&checks);
                let expected = expected.clone();
                move || {
                    if checks.fetch_add(1, Ordering::SeqCst) + 1 == reject_at {
                        Err(expected.clone())
                    } else {
                        Ok(())
                    }
                }
            });
        // Empty inline preparation needs neither OS backing nor a writer.
        let error = client
            .call_bound_controlled_phased(&binding, "ping", &[], &control)
            .await
            .unwrap_err();
        assert_eq!(error.phase(), crate::TransportPhase::PreDispatch);
        assert!(
            matches!(error.into_source(), IpcError::LocalCallRejected(error) if error == expected)
        );
        assert_eq!(checks.load(Ordering::SeqCst), reject_at);
        assert_eq!(client.pending_len_for_test(), 0);
        assert_eq!(pool.read().stats().alloc_count, 0);
        assert_eq!(pool.read().segment_count(), 0);
    }
}

#[tokio::test]
async fn controlled_method_contract_and_payload_limits_precede_preparation() {
    let (client, binding, pool) = cancellation_client("controlled_pure_limits");
    let control = IpcCallControl::new(|| panic!("invalid request must never dispatch"))
        .with_before_prepare(|| panic!("invalid request must never prepare"));
    let error = client
        .call_bound_controlled_phased(&binding, "missing", &[], &control)
        .await
        .unwrap_err();
    assert_eq!(error.phase(), crate::TransportPhase::PreDispatch);
    assert!(matches!(
        error.into_source(),
        IpcError::MethodNotFound { .. }
    ));
    let error = client
        .call_bound_controlled_phased(&binding, "ping", &[1; 1025], &control)
        .await
        .unwrap_err();
    assert_eq!(error.phase(), crate::TransportPhase::PreDispatch);
    assert!(matches!(error.into_source(), IpcError::Config(_)));
    let mut wrong_contract = c2_contract::ExpectedRouteContract {
        route_name: binding.route_name().into(),
        crm_ns: "test.grid".into(),
        crm_name: "Grid".into(),
        crm_ver: "0.1.0".into(),
        abi_hash: binding.table.abi_hash.clone(),
        signature_hash: binding.table.signature_hash.clone(),
    };
    wrong_contract.crm_ver = "0.2.0".into();
    assert!(matches!(
        client.bind_cached_route(&wrong_contract),
        Err(IpcError::ContractMismatch(_))
    ));
    assert_eq!(client.pending_len_for_test(), 0);
    assert_eq!(pool.read().segment_count(), 0);
}

#[tokio::test]
async fn started_inline_cancellation_finishes_the_original_frame_once() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let (header, payload) = read_frame(&mut stream).await;
        assert!(header.is_call_v2());
        let (_, consumed) = c2_wire::control::decode_call_control(&payload, 0).unwrap();
        assert_eq!(&payload[consumed..], b"one side effect");
        assert_eq!(
            timeout(WAIT, stream.read(&mut [0])).await.unwrap().unwrap(),
            0
        );
        crate::tests::close_listener(listener).await;
    });
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let conn = client.driver.read().clone().unwrap();
    let (entered, paused) = oneshot::channel();
    let (resume, resumed) = oneshot::channel();
    *conn.pause.lock() = Some((entered, resumed));
    let client = Arc::new(client);
    let caller = tokio::spawn({
        let client = client.clone();
        async move { client.call_inline(&identity(), 0, b"one side effect").await }
    });
    paused.await.unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    assert!(!conn.core.is_deactivated());
    assert!(
        client
            .connection_observation()
            .unwrap()
            .snapshot()
            .is_none()
    );
    resume.send(()).unwrap();
    assert!(client.close_shared_bounded(WAIT).await);
    timeout(WAIT, peer).await.unwrap().unwrap();
}

#[tokio::test]
async fn unnegotiated_shm_reply_is_rejected_before_mapping_or_loan_admission() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let peer = tokio::spawn(async move {
        let mut stream = listener.accept().await.unwrap();
        crate::tests::accept_version_preamble(&mut stream).await;
        let _ = read_frame(&mut stream).await;
        let hs = encode_server_handshake(
            0,
            &[] as &[RouteInfo],
            "",
            &ServerIdentity {
                server_id: "no-shm".to_string(),
                server_instance_id: "instance".to_string(),
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
        let (call, _) = read_frame(&mut stream).await;
        let descriptor = encode_buddy_payload(&BuddyPayload {
            seg_idx: 0,
            generation: 1,
            offset: 0,
            data_size: 4096,
            is_dedicated: false,
            loan_id: 17,
        });
        let control = encode_reply_control(&ReplyControl::Success);
        stream
            .write_all(&frame::encode_frame_parts(
                call.request_id,
                flags::FLAG_RESPONSE | flags::FLAG_REPLY_V2 | flags::FLAG_BUDDY,
                &[&descriptor, &control],
            ))
            .await
            .unwrap();
        assert_eq!(
            timeout(WAIT, stream.read(&mut [0])).await.unwrap().unwrap(),
            0
        );
        crate::tests::close_listener(listener).await;
    });
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let error = client.call_inline(&identity(), 0, b"x").await.unwrap_err();
    assert_eq!(error.phase(), crate::TransportPhase::DispatchUncertain);
    assert!(
        matches!(error.source_error(), IpcError::ResponseFailure(error) if error.to_string().contains("not negotiated"))
    );
    assert_eq!(
        client
            .server_pool
            .lock()
            .as_ref()
            .unwrap()
            .read()
            .pool
            .segment_count(),
        0
    );
    assert!(client.close_shared_bounded(WAIT).await);
    peer.await.unwrap();
}

// Group: cancellation after first publication retains the whole chunk send,
// and an unrelated call keeps progressing on the same physical connection.
#[tokio::test]
async fn cancelled_chunk_sender_finishes_its_stream_and_preserves_other_calls() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let (first_sent, first_seen) = oneshot::channel();
    let (complete, completed) = oneshot::channel();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let (first, body) = read_frame(&mut stream).await;
        assert!(flags::is_chunked(first.flags));
        let (index, total, _) = c2_wire::chunk::header::decode_chunk_header(&body, 0).unwrap();
        assert_eq!((index, total), (0, 3));
        first_sent.send(()).unwrap();
        let (other, _) = read_frame(&mut stream).await;
        assert_ne!(other.request_id, first.request_id);
        reply(&mut stream, other.request_id, b"other").await;
        for expected in 1..3 {
            let (header, body) = read_frame(&mut stream).await;
            assert_eq!(header.request_id, first.request_id);
            assert_eq!(
                c2_wire::chunk::header::decode_chunk_header(&body, 0)
                    .unwrap()
                    .0,
                expected
            );
        }
        reply(&mut stream, first.request_id, b"cancelled result").await;
        complete.send(()).unwrap();
        assert_eq!(
            timeout(WAIT, stream.read(&mut [0])).await.unwrap().unwrap(),
            0
        );
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
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    assert!(client.is_connected());
    assert_eq!(
        client
            .call_inline(&identity(), 0, b"other")
            .await
            .unwrap()
            .into_owned_bytes()
            .unwrap(),
        b"other"
    );
    source.send(vec![2; 8192]).await.unwrap();
    drop(source);
    completed.await.unwrap();
    assert!(client.is_connected());
    assert_eq!(client.pending_len_for_test(), 0);
    assert!(client.close_shared_bounded(WAIT).await);
    peer.await.unwrap();
}

#[derive(Clone, Copy, Debug)]
enum FramedFault {
    Signal,
    Direction,
    ReplyControl,
    UnnegotiatedShm,
    ZeroLoan,
    MissingMapping,
    ChunkGeometry,
    DuplicateChunk,
}

fn publish_file_loan(
    conn: &ClientConnection,
    directory: &std::path::Path,
) -> (u64, Arc<RwLock<MemPool>>, c2_mem::MemoryBudget) {
    let budget = c2_mem::MemoryBudget::new(0, 1024 * 1024, 1024 * 1024);
    let pool = Arc::new(RwLock::new(
        MemPool::new_with_prefix_and_budget(
            c2_mem::PoolConfig {
                spill_dir: directory.join("loan"),
                ..Default::default()
            },
            format!(
                "/p4file{}",
                CLIENT_CONN_COUNTER.fetch_add(1, Ordering::Relaxed)
            ),
            budget.clone(),
        )
        .unwrap(),
    ));
    let handle = pool.write().alloc_handle(4096).unwrap();
    assert!(handle.is_file_spill());
    let loan = conn.core.prepare_loan(pool.clone(), handle).unwrap();
    let id = loan.id();
    let mut batch = conn
        .core
        .begin_write(WriteRequest::Business(vec![loan]))
        .unwrap()
        .unwrap();
    batch.begin_io().unwrap();
    batch.frame_completed();
    batch.flush_completed();
    (id, pool, budget)
}
fn reply_chunk(rid: u64, count: u32, index: u32, data: &[u8]) -> Vec<u8> {
    let meta = c2_wire::chunk::header::encode_reply_chunk_meta(8192, count, index);
    frame::encode_frame_parts(
        rid,
        flags::FLAG_RESPONSE | flags::FLAG_REPLY_V2 | flags::FLAG_CHUNKED,
        &[&meta, data],
    )
}

// Group: complete-frame protocol faults stop admission while the reader still
// settles the original loan book. No SHM allocation is claimed by this proof:
// the file handle isolates the driver's control/read and error-phase wiring.
#[tokio::test]
async fn framed_response_faults_remain_uncertain_and_still_settle_release_records() {
    for fault in [
        FramedFault::Signal,
        FramedFault::Direction,
        FramedFault::ReplyControl,
        FramedFault::UnnegotiatedShm,
        FramedFault::ZeroLoan,
        FramedFault::MissingMapping,
        FramedFault::ChunkGeometry,
        FramedFault::DuplicateChunk,
    ] {
        let endpoint = Endpoint::new();
        let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
        let (loan_sent, loan_received) = oneshot::channel();
        let peer = tokio::spawn(async move {
            let mut stream = listener.accept().await.unwrap();
            crate::tests::accept_version_preamble(&mut stream).await;
            let (_, metadata) = read_frame(&mut stream).await;
            let hs = decode_handshake(&metadata, false).unwrap();
            let caps = if matches!(fault, FramedFault::UnnegotiatedShm) {
                0
            } else {
                hs.capability_flags
            };
            let hs = encode_server_handshake(
                caps,
                &[] as &[RouteInfo],
                "/p4missing",
                &ServerIdentity {
                    server_id: "fault-peer".to_string(),
                    server_instance_id: "instance".to_string(),
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
            let (request, _) = read_frame(&mut stream).await;
            let bad = match fault {
                FramedFault::Signal => frame::encode_frame(0, flags::FLAG_SIGNAL, &[0xfe]),
                FramedFault::Direction => {
                    frame::encode_frame(request.request_id, flags::FLAG_CALL_V2, &[0])
                }
                FramedFault::ReplyControl => frame::encode_frame(
                    request.request_id,
                    flags::FLAG_RESPONSE | flags::FLAG_REPLY_V2,
                    &[0xff],
                ),
                FramedFault::ChunkGeometry => reply_chunk(request.request_id, 0, 0, b"x"),
                FramedFault::DuplicateChunk => {
                    let first = reply_chunk(request.request_id, 2, 0, &[1; 4096]);
                    stream.write_all(&first).await.unwrap();
                    first
                }
                _ => {
                    let descriptor = encode_buddy_payload(&BuddyPayload {
                        seg_idx: 0,
                        generation: 1,
                        offset: 0,
                        data_size: 4096,
                        is_dedicated: false,
                        loan_id: if matches!(fault, FramedFault::ZeroLoan) {
                            0
                        } else {
                            77
                        },
                    });
                    frame::encode_frame_parts(
                        request.request_id,
                        flags::FLAG_RESPONSE | flags::FLAG_REPLY_V2 | flags::FLAG_BUDDY,
                        &[&descriptor, &[0]],
                    )
                }
            };
            stream.write_all(&bad).await.unwrap();
            let loan = loan_received.await.unwrap();
            stream
                .write_all(&c2_wire::release::encode_release_frame(
                    &c2_wire::release::ReleaseBatch::new(&[loan]).unwrap(),
                    true,
                ))
                .await
                .unwrap();
            // Ignore any receiver release control, but never accept a replay.
            let mut byte = [0];
            loop {
                match timeout(WAIT, stream.read(&mut byte))
                    .await
                    .unwrap()
                    .unwrap()
                {
                    0 => break,
                    _ => {
                        let mut header = [0u8; HEADER_SIZE];
                        header[0] = byte[0];
                        stream.read_exact(&mut header[1..]).await.unwrap();
                        let (len, body) = frame::decode_total_len(&header).unwrap();
                        let (header, _) = frame::decode_frame_body(body, len).unwrap();
                        assert!(
                            header.is_ctrl(),
                            "response failure must not replay a business call"
                        );
                        let mut payload = vec![0; header.payload_len()];
                        stream.read_exact(&mut payload).await.unwrap();
                    }
                }
            }
            crate::tests::close_listener(listener).await;
        });
        let mut cfg = config();
        cfg.base.shm_backing_budget_bytes = 0;
        let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
        client.connect().await.unwrap();
        let conn = client.driver.read().clone().unwrap();
        let (loan, _pool, budget) = publish_file_loan(&conn, endpoint._root.path());
        let error = client
            .call_inline(&identity(), 0, b"execute once")
            .await
            .unwrap_err();
        assert_eq!(
            error.phase(),
            crate::TransportPhase::DispatchUncertain,
            "{fault:?}: {error}"
        );
        assert!(!client.is_connected());
        assert!(budget.snapshot().file.used_bytes > 0);
        loan_sent.send(loan).unwrap();
        eventually(|| budget.snapshot().file.used_bytes == 0).await;
        assert!(client.close_shared_bounded(WAIT).await);
        assert_eq!(client.memory_budget.snapshot().reassembly.used_bytes, 0);
        assert_eq!(
            client
                .connection_observation()
                .unwrap()
                .snapshot()
                .unwrap()
                .reason(),
            ConnectionEndReason::ProtocolError
        );
        peer.await.unwrap();
    }
}

#[tokio::test]
async fn materialization_failure_is_uncertain_and_refunds_local_backing_once() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        assert_eq!(
            timeout(WAIT, stream.read(&mut [0])).await.unwrap().unwrap(),
            0
        );
        crate::tests::close_listener(listener).await;
    });
    let mut client = endpoint.client();
    client.connect().await.unwrap();
    let budget = c2_mem::MemoryBudget::new(0, 1024 * 1024, 1024 * 1024);
    let pool = Arc::new(RwLock::new(
        MemPool::new_with_prefix_and_budget(
            c2_mem::PoolConfig {
                spill_dir: endpoint._root.path().join("materialize"),
                ..Default::default()
            },
            "/p4materialize".into(),
            budget.clone(),
        )
        .unwrap(),
    ));
    let mut backing = c2_wire::chunk::ReassemblyBacking::admit(pool, 1, 4096).unwrap();
    backing.release();
    let conn = client.driver.read().clone().unwrap();
    let error = owned_response(Ok(ResponseData::Handle(backing)), &conn).unwrap_err();
    assert!(matches!(error, IpcError::ResponseFailure(_)));
    assert_eq!(
        crate::sync_client::call_error_phase(&error),
        crate::TransportPhase::DispatchUncertain
    );
    assert_eq!(budget.snapshot().reassembly.used_bytes, 0);
    assert_eq!(budget.snapshot().file.used_bytes, 0);
    assert!(client.close_shared_bounded(WAIT).await);
    peer.await.unwrap();
}

// Group: periodic expiry owns local reassembly/idle pool reclamation, and its
// references end with the physical driver rather than a live client shell.
#[tokio::test]
async fn client_gc_expires_a_stalled_reply_and_stops_with_the_driver() {
    let endpoint = Endpoint::new();
    let mut listener = LocalListener::bind(&endpoint.endpoint).unwrap();
    let peer = tokio::spawn(async move {
        let mut stream = accept(&mut listener).await;
        let (request, _) = read_frame(&mut stream).await;
        stream
            .write_all(&reply_chunk(request.request_id, 2, 0, &[1; 4096]))
            .await
            .unwrap();
        assert_eq!(
            timeout(WAIT, stream.read(&mut [0])).await.unwrap().unwrap(),
            0
        );
        crate::tests::close_listener(listener).await;
    });
    let mut cfg = config();
    cfg.base.shm_backing_budget_bytes = 0;
    cfg.base.chunk_gc_interval_secs = 0.01;
    cfg.base.chunk_assembler_timeout_secs = 0.05;
    cfg.pool_decay_seconds = 0.01;
    let mut client = IpcClient::with_endpoint(endpoint.endpoint.clone(), cfg).unwrap();
    client.connect().await.unwrap();
    let registry = client.chunk_registry.lock().clone().unwrap();
    let weak = Arc::downgrade(registry.pool());
    drop(registry);
    let client = Arc::new(client);
    let caller = tokio::spawn({
        let client = client.clone();
        async move { client.call_inline(&identity(), 0, b"partial").await }
    });
    eventually(|| client.memory_budget.snapshot().reassembly.used_bytes > 0).await;
    eventually(|| client.memory_budget.snapshot().reassembly.used_bytes == 0).await;
    assert_eq!(client.memory_budget.snapshot().file.used_bytes, 0);
    caller.abort();
    let _ = caller.await;
    assert_eq!(client.pending_len_for_test(), 0);
    assert!(client.close_shared_bounded(WAIT).await);
    eventually(|| weak.upgrade().is_none()).await;
    peer.await.unwrap();
}

#[path = "p4_continuation_tests.rs"]
mod continuation;

#[path = "p4_lifecycle_tests.rs"]
mod lifecycle;
