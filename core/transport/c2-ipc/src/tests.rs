//! c2-ipc unit tests.

/// Common first exchange for manual peers that exercise ordinary IPC traffic.
pub(crate) async fn accept_version_preamble(stream: &mut c2_local::LocalStream) {
    use c2_wire::handshake::{VersionPreambleKind, encode_version_preamble};
    use tokio::io::AsyncReadExt as _;

    let mut hello = [0u8; 25];
    stream.read_exact(&mut hello).await.expect("version Hello");
    assert_eq!(hello, encode_version_preamble(VersionPreambleKind::Hello));
    stream
        .write_all(&encode_version_preamble(VersionPreambleKind::Accepted))
        .await
        .expect("version Accepted");
}

pub(crate) async fn close_listener(listener: c2_local::LocalListener) {
    let outcome = tokio::task::spawn_blocking(move || listener.close())
        .await
        .unwrap();
    assert!(
        matches!(
            outcome,
            c2_local::EndpointReapResult::Reaped
                | c2_local::EndpointReapResult::AlreadyAbsent
                | c2_local::EndpointReapResult::NotApplicable
        ),
        "listener cleanup failed: {outcome:?}"
    );
}

#[cfg(test)]
mod client_tests {
    use c2_wire::buddy::{BUDDY_PAYLOAD_SIZE, decode_buddy_payload};
    use c2_wire::chunk::header::{CHUNK_HEADER_SIZE, decode_chunk_header};
    use c2_wire::control::*;
    use c2_wire::flags;
    use c2_wire::frame;

    use crate::client::{
        ClientIpcConfig, IpcClient, RequestTransportKind, choose_request_transport,
        request_chunk_count,
    };

    const ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SIG_HASH: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    fn call_identity(route_name: &str) -> RouteCallIdentity {
        RouteCallIdentity {
            route_name: route_name.into(),
            route_uid: format!("{route_name}-route-uid-0001"),

            crm_ns: "test.grid".into(),
            crm_name: "Grid".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: ABI_HASH.into(),
            signature_hash: SIG_HASH.into(),
        }
    }

    #[test]
    fn encode_v2_inline_call() {
        // Verify that an inline v2 call frame has the expected layout:
        // [16B header (flags=FLAG_CALL_V2)] [call_control] [data]
        let ctrl = encode_call_control(&call_identity("grid"), 0);
        let data = b"hello";
        let mut payload = Vec::new();
        payload.extend_from_slice(&ctrl);
        payload.extend_from_slice(data);
        let frame_bytes = frame::encode_frame(42, flags::FLAG_CALL_V2, &payload);

        let (hdr, frame_payload) = frame::decode_frame(&frame_bytes).unwrap();
        assert_eq!(hdr.request_id, 42);
        assert!(hdr.is_call_v2());
        assert!(!hdr.is_buddy());

        let (decoded_ctrl, consumed) = decode_call_control(frame_payload, 0).unwrap();
        assert_eq!(decoded_ctrl.identity.route_name, "grid");
        assert_eq!(decoded_ctrl.method_idx, 0);

        let inline_data = &frame_payload[consumed..];
        assert_eq!(inline_data, b"hello");
    }

    #[test]
    fn encode_v2_inline_reply() {
        // [16B header (flags=RESPONSE|REPLY_V2)] [1B status=OK] [data]
        let ctrl = encode_reply_control(&ReplyControl::Success);
        let data = b"result";
        let mut payload = Vec::new();
        payload.extend_from_slice(&ctrl);
        payload.extend_from_slice(data);
        let frame_bytes =
            frame::encode_frame(42, flags::FLAG_RESPONSE | flags::FLAG_REPLY_V2, &payload);

        let (hdr, frame_payload) = frame::decode_frame(&frame_bytes).unwrap();
        assert!(hdr.is_response());
        assert!(hdr.is_reply_v2());
        assert!(!hdr.is_buddy());

        let (ctrl, consumed) = decode_reply_control(frame_payload, 0).unwrap();
        assert_eq!(ctrl, ReplyControl::Success);
        assert_eq!(&frame_payload[consumed..], b"result");
    }

    #[test]
    fn encode_v2_error_reply() {
        let err = br#"C2E1{"version":1,"code":3,"name":"ResourceFunctionExecuting","message":"test error","details":{}}"#.to_vec();
        let ctrl = encode_reply_control(&ReplyControl::Error(err.clone()));
        let frame_bytes =
            frame::encode_frame(42, flags::FLAG_RESPONSE | flags::FLAG_REPLY_V2, &ctrl);

        let (hdr, frame_payload) = frame::decode_frame(&frame_bytes).unwrap();
        assert!(hdr.is_response());
        assert!(hdr.is_reply_v2());

        let (decoded, _) = decode_reply_control(frame_payload, 0).unwrap();
        assert_eq!(decoded, ReplyControl::Error(err));
    }

    // ── New tests for buddy + chunked transport ─────────────────────────

    #[test]
    fn test_ipc_config_defaults() {
        let cfg = ClientIpcConfig::default();
        assert_eq!(cfg.shm_threshold, 4096);
        assert_eq!(cfg.chunk_size, 131072);
    }

    #[test]
    fn test_buddy_frame_encoding() {
        // Build a buddy call frame as call_buddy would:
        // payload = [23B buddy_payload][call_control]
        // flags   = FLAG_CALL_V2 | FLAG_BUDDY
        use c2_wire::buddy::{BuddyPayload, encode_buddy_payload};

        let bp = BuddyPayload {
            seg_idx: 0,
            generation: 1,
            offset: 4096,
            data_size: 8192,
            is_dedicated: false,
            loan_id: 0x0807_0605_0403_0201,
        };
        let buddy_bytes = encode_buddy_payload(&bp);
        assert_eq!(buddy_bytes.len(), BUDDY_PAYLOAD_SIZE);

        let ctrl = encode_call_control(&call_identity("grid"), 3);
        let mut payload = Vec::new();
        payload.extend_from_slice(&buddy_bytes);
        payload.extend_from_slice(&ctrl);

        let frame_flags = flags::FLAG_CALL_V2 | flags::FLAG_BUDDY;
        let frame_bytes = frame::encode_frame(99, frame_flags, &payload);

        // Decode and verify structure.
        let (hdr, frame_payload) = frame::decode_frame(&frame_bytes).unwrap();
        assert_eq!(hdr.request_id, 99);
        assert!(hdr.is_call_v2());
        assert!(hdr.is_buddy());
        assert!(!flags::is_chunked(hdr.flags));

        // Decode buddy payload.
        let (decoded_bp, bp_consumed) = decode_buddy_payload(frame_payload).unwrap();
        assert_eq!(decoded_bp.seg_idx, 0);
        assert_eq!(decoded_bp.generation, 1);
        assert_eq!(decoded_bp.offset, 4096);
        assert_eq!(decoded_bp.data_size, 8192);
        assert!(!decoded_bp.is_dedicated);
        assert_eq!(decoded_bp.loan_id, bp.loan_id);
        assert_eq!(bp_consumed, BUDDY_PAYLOAD_SIZE);

        // Decode call control after buddy payload.
        let (decoded_ctrl, _) = decode_call_control(frame_payload, BUDDY_PAYLOAD_SIZE).unwrap();
        assert_eq!(decoded_ctrl.identity.route_name, "grid");
        assert_eq!(decoded_ctrl.method_idx, 3);
    }

    #[test]
    fn test_buddy_frame_dedicated_segment() {
        use c2_wire::buddy::{BuddyPayload, encode_buddy_payload};

        let bp = BuddyPayload {
            seg_idx: 5,
            generation: 1,
            offset: 0,
            data_size: 1_000_000,
            is_dedicated: true,
            loan_id: 17,
        };
        let buddy_bytes = encode_buddy_payload(&bp);

        let ctrl = encode_call_control(&call_identity("net"), 1);

        let mut payload = Vec::new();
        payload.extend_from_slice(&buddy_bytes);
        payload.extend_from_slice(&ctrl);

        let frame_bytes = frame::encode_frame(7, flags::FLAG_CALL_V2 | flags::FLAG_BUDDY, &payload);

        let (hdr, frame_payload) = frame::decode_frame(&frame_bytes).unwrap();
        assert!(hdr.is_buddy());
        let (decoded_bp, _) = decode_buddy_payload(frame_payload).unwrap();
        assert_eq!(decoded_bp.seg_idx, 5);
        assert_eq!(decoded_bp.generation, 1);
        assert!(decoded_bp.is_dedicated);
        assert_eq!(decoded_bp.data_size, 1_000_000);
    }

    #[test]
    fn test_chunked_frame_encoding() {
        use c2_wire::chunk::header::encode_chunk_header;

        let ctrl = encode_call_control(&call_identity("grid"), 2);
        let data = [0xABu8; 128];
        for (idx, count, last) in [(0, 1, true), (0, 3, false), (1, 3, false), (2, 3, true)] {
            let chunk_header = encode_chunk_header(idx, count);
            let frame_flags = flags::FLAG_CALL_V2
                | flags::FLAG_CHUNKED
                | if last { flags::FLAG_CHUNK_LAST } else { 0 };
            let encoded = frame::encode_frame_parts(
                42,
                frame_flags,
                &[&chunk_header, if idx == 0 { &ctrl } else { &[] }, &data],
            );
            let (header, payload) = frame::decode_frame(&encoded).unwrap();
            assert_eq!(header.request_id, 42);
            assert!(header.is_call_v2());
            assert!(flags::is_chunked(header.flags));
            assert_eq!(flags::is_chunk_last(header.flags), last);
            let (actual_idx, actual_count, offset) = decode_chunk_header(payload, 0).unwrap();
            assert_eq!(
                (actual_idx, actual_count, offset),
                (idx, count, CHUNK_HEADER_SIZE)
            );
            let control_len = if idx == 0 {
                let (decoded, consumed) = decode_call_control(payload, offset).unwrap();
                assert_eq!(decoded.identity.route_name, "grid");
                assert_eq!(decoded.method_idx, 2);
                consumed
            } else {
                0
            };
            assert_eq!(&payload[offset + control_len..], &data);
        }
    }

    #[test]
    fn canonical_call_transport_selection_is_testable() {
        // Verify ClientIpcConfig thresholds determine the transport path used by
        // route-bound IPC calls. This must stay as a pure selector so relay
        // behavior is not proven only by comments or a live UDS integration test.
        let cfg = ClientIpcConfig {
            shm_threshold: 100,
            base: c2_config::BaseIpcConfig {
                chunk_size: 500,
                ..c2_config::BaseIpcConfig::default()
            },
            ..ClientIpcConfig::default()
        };

        // `shm_available` means pool present AND a negotiated CAP_SHM_REQUEST
        // direction; without negotiation the selector never picks SHM even
        // for bodies the pool could carry.
        assert_eq!(
            choose_request_transport(&cfg, false, 50),
            RequestTransportKind::Inline
        );
        assert_eq!(
            choose_request_transport(&cfg, true, 50),
            RequestTransportKind::Inline
        );

        assert_eq!(
            choose_request_transport(&cfg, true, 200),
            RequestTransportKind::Buddy
        );
        assert_eq!(
            choose_request_transport(&cfg, false, 200),
            RequestTransportKind::Inline
        );

        assert_eq!(
            choose_request_transport(&cfg, true, 600),
            RequestTransportKind::Buddy
        );
        assert_eq!(
            choose_request_transport(&cfg, false, 600),
            RequestTransportKind::Chunked
        );
        assert_eq!(
            choose_request_transport(&cfg, true, u32::MAX as usize + 1),
            RequestTransportKind::Chunked,
            "buddy request metadata cannot represent payload lengths above u32::MAX"
        );

        // Verify chunk count calculation.
        let chunk_size = cfg.chunk_size as usize;
        let total_chunks = request_chunk_count(600, chunk_size).unwrap();
        assert_eq!(total_chunks, 2); // 600 / 500 = 1.2 -> 2 chunks
        assert!(request_chunk_count(usize::from(u16::MAX) * chunk_size + 1, chunk_size).is_err());
    }

    #[test]
    fn with_config_owns_request_pool_when_pool_enabled() {
        let cfg = ClientIpcConfig {
            shm_threshold: 100,
            base: c2_config::BaseIpcConfig {
                pool_enabled: true,
                pool_segment_size: 65_536,
                max_pool_segments: 2,
                ..c2_config::BaseIpcConfig::default()
            },
            ..ClientIpcConfig::default()
        };
        let client = IpcClient::with_config("ipc://configured", cfg.clone()).unwrap();
        assert!(client.has_request_pool());
        assert_eq!(client.config.shm_threshold, cfg.shm_threshold);

        // A disabled buddy pool is still a real pool: it keeps dedicated SHM
        // requests and the wire prefix alive, only the buddy tiers are off.
        let mut disabled = cfg;
        disabled.base.pool_enabled = false;
        let client = IpcClient::with_config("ipc://configured-no-pool", disabled).unwrap();
        let pool = client
            .request_pool()
            .expect("policy-disabled pool stays live");
        let pool = pool.read();
        assert!(!pool.config().buddy_enabled);
        assert_eq!(pool.segment_count(), 0);
        assert_eq!(pool.segment_count(), 0);
    }

    // ── SyncClient tests ──────────────────────────────────────────────

    #[test]
    fn test_sync_client_global_runtime() {
        // get_or_create_runtime() must return the same runtime on every call.
        let rt1 = crate::sync_client::tests::runtime_ptr();
        let rt2 = crate::sync_client::tests::runtime_ptr();
        assert_eq!(rt1, rt2, "global runtime should be the same instance");
    }
}

#[cfg(test)]
mod endpoint_context_tests {
    use crate::{ClientIpcConfig, ClientPool, IpcClient};
    use c2_config::{LocalEndpointContext, LocalEndpointNamespace};
    use c2_server::{Server, ServerIpcConfig};
    use std::time::Duration;

    #[test]
    fn platform_default_and_explicit_constructors_keep_the_same_snapshot() {
        let context = LocalEndpointContext::default_for_platform().unwrap();
        let endpoint = context.endpoint("ipc://context_api").unwrap();
        let client =
            IpcClient::with_config(endpoint.address(), ClientIpcConfig::default()).unwrap();
        let configured =
            IpcClient::with_config(endpoint.address(), ClientIpcConfig::default()).unwrap();
        let explicit =
            IpcClient::with_endpoint(endpoint.clone(), ClientIpcConfig::default()).unwrap();
        assert_eq!(client.local_endpoint().unwrap(), &endpoint);
        assert_eq!(configured.local_endpoint().unwrap(), &endpoint);
        assert_eq!(explicit.local_endpoint().unwrap(), &endpoint);
        let server =
            Server::new_with_endpoint(endpoint.clone(), ServerIpcConfig::default()).unwrap();
        let default_server = Server::new(endpoint.address(), ServerIpcConfig::default()).unwrap();
        let identified = Server::new_with_identity_and_endpoint(
            endpoint.clone(),
            ServerIpcConfig::default(),
            server.identity().clone(),
        )
        .unwrap();
        assert_eq!(server.local_endpoint(), &endpoint);
        assert_eq!(default_server.local_endpoint(), &endpoint);
        assert_eq!(identified.local_endpoint(), &endpoint);
        assert_eq!(identified.identity(), server.identity());
        let pool = ClientPool::with_endpoint_context(Duration::ZERO, context.clone());
        let default_pool = ClientPool::new(Duration::ZERO);
        assert_eq!(pool.endpoint_context().unwrap(), &context);
        assert_eq!(default_pool.endpoint_context().unwrap(), &context);
        #[cfg(windows)]
        {
            assert_eq!(
                context.platform_kind(),
                LocalEndpointNamespace::WindowsNamedPipe
            );
            assert!(context.unix_root().is_none());
            assert!(context.windows_user_scope_id().is_some());
        }
        #[cfg(unix)]
        assert_eq!(
            context.platform_kind(),
            LocalEndpointNamespace::UnixFilesystem
        );
    }

    #[cfg(unix)]
    #[test]
    fn custom_context_is_frozen_across_client_close_and_empty_cache_epochs() {
        let a = LocalEndpointContext::with_unix_root(std::path::Path::new("/tmp/ctxa")).unwrap();
        let b = LocalEndpointContext::with_unix_root(std::path::Path::new("/tmp/ctxb")).unwrap();
        let endpoint = a.endpoint("ipc://same").unwrap();
        assert_ne!(endpoint, b.endpoint(endpoint.address()).unwrap());
        let config = ClientIpcConfig::default();
        let budget = c2_mem::MemoryBudget::from_limits(&config.memory_budget_limits());
        let mut client =
            IpcClient::with_endpoint_and_shared_budget(endpoint.clone(), config, budget).unwrap();
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(client.close());
        assert_eq!(client.local_endpoint().unwrap(), &endpoint);
        let server =
            Server::new_with_endpoint(endpoint.clone(), ServerIpcConfig::default()).unwrap();
        assert_eq!(server.local_endpoint(), &endpoint);
        let pool = ClientPool::with_endpoint_context(Duration::ZERO, a.clone());
        for _ in 0..2 {
            let report = pool.close_all(Duration::from_secs(1));
            assert!(report.error.is_none(), "{report:?}");
            assert!(report.unconfirmed.is_empty(), "{report:?}");
            assert_eq!(pool.endpoint_context().unwrap(), &a);
        }
        assert!(matches!(
            pool.acquire("http://invalid", None),
            Err(crate::IpcError::Config(_))
        ));
        assert_eq!(
            pool.endpoint_context().unwrap().namespace_id(),
            a.namespace_id()
        );
        assert_ne!(
            pool.endpoint_context().unwrap().namespace_id(),
            b.namespace_id()
        );
    }
}

#[cfg(all(test, unix))]
mod endpoint_context_transport_tests {
    use crate::{
        ClientPool, DirectIpcShutdownOutcome, SyncClient, ping_with_context, ping_with_endpoint,
        shutdown_with_context, shutdown_with_endpoint,
    };
    use c2_config::LocalEndpointContext;
    use c2_server::{
        ConcurrencyMode, CrmCallback, CrmError, ResponseMeta, RouteBuildSpec, SchedulerLimits,
        Server, ServerIpcConfig,
    };
    use std::collections::HashMap;
    use std::os::unix::fs::DirBuilderExt;
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::time::Duration;

    const WAIT: Duration = Duration::from_secs(5);
    const ADDRESS: &str = "ipc://same_context_route";

    // Short UUID-derived names keep the complete socket below macOS sun_path.
    // Only containers successfully created by this fixture are ever removed.
    struct Root(PathBuf);
    impl Root {
        fn new() -> Self {
            loop {
                let identity = Server::new("ipc://root_uuid", ServerIpcConfig::default()).unwrap();
                let path = PathBuf::from(format!("/tmp/c{}", &identity.server_instance_id()[..8]));
                match std::fs::DirBuilder::new().mode(0o700).create(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => panic!("create custom-root container: {error}"),
                }
            }
        }
        fn context(&self) -> LocalEndpointContext {
            LocalEndpointContext::with_unix_root(&self.0).unwrap()
        }
    }
    impl Drop for Root {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.0).expect("remove only fixture-owned container");
        }
    }

    struct Tag(u8);
    impl CrmCallback for Tag {
        fn invoke(
            &self,
            _: &str,
            _: u16,
            _: Vec<u8>,
            _: Arc<c2_server::ResponsePool>,
        ) -> Result<ResponseMeta, CrmError> {
            Ok(ResponseMeta::Inline(vec![self.0]))
        }
    }

    fn contract() -> c2_contract::ExpectedRouteContract {
        c2_contract::ExpectedRouteContract {
            route_name: "grid".into(),
            crm_ns: "test.context".into(),
            crm_name: "Grid".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                .into(),
        }
    }

    async fn register(server: &Server, tag: u8) {
        let expected = contract();
        let built = server
            .build_route(
                RouteBuildSpec {
                    name: expected.route_name,
                    crm_ns: expected.crm_ns,
                    crm_name: expected.crm_name,
                    crm_ver: expected.crm_ver,
                    abi_hash: expected.abi_hash,
                    signature_hash: expected.signature_hash,
                    method_names: vec!["tag".into()],
                    access_map: HashMap::new(),
                    concurrency_mode: ConcurrencyMode::ReadParallel,
                    limits: SchedulerLimits::default(),
                },
                Arc::new(Tag(tag)),
            )
            .unwrap();
        let reservation = server.reserve_route(built).await.unwrap();
        server.commit_reserved_route(reservation).await.unwrap();
    }

    fn check_tag(client: &SyncClient, expected: u8) {
        let binding = client.acquire_route(&contract()).unwrap();
        let response = client.call_bound(&binding, "tag", &[]).unwrap();
        assert_eq!(
            client.lease_response(response).into_owned_bytes().unwrap(),
            vec![expected]
        );
    }

    #[test]
    fn same_address_custom_roots_isolate_clients_cache_epochs_and_admin_control() {
        let root_a = Root::new();
        let root_b = Root::new();
        let a = root_a.context();
        let b = root_b.context();
        let endpoint_a = a.endpoint(ADDRESS).unwrap();
        let endpoint_b = b.endpoint(ADDRESS).unwrap();
        assert_ne!(endpoint_a.os_name(), endpoint_b.os_name());
        let rt = tokio::runtime::Runtime::new().unwrap();
        let server_a = Arc::new(
            Server::new_with_endpoint(endpoint_a.clone(), ServerIpcConfig::default()).unwrap(),
        );
        let server_b = Arc::new(
            Server::new_with_identity_and_endpoint(
                endpoint_b.clone(),
                ServerIpcConfig::default(),
                c2_server::ServerIdentity {
                    server_id: "root_b".into(),
                    server_instance_id: "root_b_instance".into(),
                },
            )
            .unwrap(),
        );
        let (run_a, run_b) = rt.block_on(async {
            register(&server_a, b'A').await;
            register(&server_b, b'B').await;
            let start = |server: Arc<Server>| {
                server.begin_start_attempt().unwrap();
                tokio::spawn(async move { server.run().await })
            };
            let runs = (start(server_a.clone()), start(server_b.clone()));
            // Native watch events prove bind readiness without polling or sleeps.
            server_a.wait_until_ready(WAIT).await.unwrap();
            server_b.wait_until_ready(WAIT).await.unwrap();
            runs
        });
        let direct_a = ClientPool::with_endpoint_context(WAIT, endpoint_a.context().clone())
            .acquire(endpoint_a.address(), None)
            .unwrap();
        let direct_b = ClientPool::with_endpoint_context(WAIT, endpoint_b.context().clone())
            .acquire(endpoint_b.address(), None)
            .unwrap();
        assert_eq!(direct_a.server_identity(), Some(server_a.identity()));
        assert_eq!(direct_b.server_identity(), Some(server_b.identity()));
        check_tag(&direct_a, b'A');
        check_tag(&direct_b, b'B');
        assert!(ping_with_context(ADDRESS, &a, WAIT).unwrap());
        assert!(ping_with_endpoint(&endpoint_b, WAIT).unwrap());

        let pool_a = ClientPool::with_endpoint_context(Duration::from_secs(30), a.clone());
        let pool_b = ClientPool::with_endpoint_context(Duration::from_secs(30), b.clone());
        let cached_a = pool_a.acquire(ADDRESS, None).unwrap();
        let cached_b = pool_b.acquire(ADDRESS, None).unwrap();
        let hit_a = pool_a.acquire(ADDRESS, None).unwrap();
        assert!(Arc::ptr_eq(&cached_a, &hit_a));
        assert_eq!(pool_a.refcount(ADDRESS), 2);
        assert_eq!(pool_b.refcount(ADDRESS), 1);
        assert!(!Arc::ptr_eq(&cached_a, &cached_b));
        check_tag(&cached_a, b'A');
        check_tag(&cached_b, b'B');
        assert!(pool_a.release_if_same(ADDRESS, &cached_a));
        assert_eq!(pool_a.refcount(ADDRESS), 1);
        let report = pool_a.close_all(WAIT);
        assert_eq!(report.detached, 1);
        assert!(report.error.is_none(), "{report:?}");
        assert!(report.unconfirmed.is_empty(), "{report:?}");
        assert!(!cached_a.is_connected());
        check_tag(&cached_b, b'B');
        assert_eq!(pool_b.refcount(ADDRESS), 1);
        assert_eq!(pool_a.endpoint_context().unwrap(), &a);
        let reopened_a = pool_a.acquire(ADDRESS, None).unwrap();
        assert!(!Arc::ptr_eq(&cached_a, &reopened_a));
        assert_eq!(reopened_a.local_endpoint(), &endpoint_a);
        assert_eq!(reopened_a.server_identity(), Some(server_a.identity()));
        check_tag(&reopened_a, b'A');
        check_tag(&cached_b, b'B');

        assert!(direct_a.close_shared(WAIT));
        assert!(direct_b.close_shared(WAIT));
        let report = pool_a.close_all(WAIT);
        assert!(
            report.error.is_none() && report.unconfirmed.is_empty(),
            "{report:?}"
        );
        let ack_a = shutdown_with_context(ADDRESS, &a, WAIT).unwrap();
        assert_eq!(ack_a, DirectIpcShutdownOutcome::Initiated);
        rt.block_on(async {
            server_a.wait_until_stopped(WAIT).await.unwrap();
            run_a.await.unwrap().unwrap();
        });
        assert_eq!(server_a.local_endpoint(), &endpoint_a);
        assert!(ping_with_endpoint(&endpoint_b, WAIT).unwrap());
        check_tag(&cached_b, b'B');
        let absent_a = shutdown_with_endpoint(&endpoint_a, WAIT).unwrap();
        assert_eq!(absent_a, DirectIpcShutdownOutcome::Absent);
        // Fence restart before readiness waiters, using the same Server endpoint.
        server_a.begin_start_attempt().unwrap();
        let restarted_a = rt.block_on(async {
            let server = server_a.clone();
            let runner = tokio::spawn(async move { server.run().await });
            server_a.wait_until_ready(WAIT).await.unwrap();
            runner
        });
        let restarted_client =
            ClientPool::with_endpoint_context(WAIT, endpoint_a.context().clone())
                .acquire(endpoint_a.address(), None)
                .unwrap();
        assert_eq!(
            restarted_client.server_identity(),
            Some(server_a.identity())
        );
        assert_eq!(restarted_client.local_endpoint(), &endpoint_a);
        assert!(restarted_client.close_shared(WAIT));
        check_tag(&cached_b, b'B');
        let ack_a = shutdown_with_endpoint(&endpoint_a, WAIT).unwrap();
        assert_eq!(ack_a, DirectIpcShutdownOutcome::Initiated);
        rt.block_on(async {
            server_a.wait_until_stopped(WAIT).await.unwrap();
            restarted_a.await.unwrap().unwrap();
        });
        let report = pool_b.close_all(WAIT);
        assert!(
            report.error.is_none() && report.unconfirmed.is_empty(),
            "{report:?}"
        );
        let ack_b = shutdown_with_endpoint(&endpoint_b, WAIT).unwrap();
        assert_eq!(ack_b, DirectIpcShutdownOutcome::Initiated);
        rt.block_on(async {
            server_b.wait_until_stopped(WAIT).await.unwrap();
            run_b.await.unwrap().unwrap();
        });
        assert_eq!(server_b.local_endpoint(), &endpoint_b);
    }
}
