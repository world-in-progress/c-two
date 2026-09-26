//! c2-ipc unit tests.

#[cfg(test)]
mod client_tests {
    use c2_wire::buddy::{BUDDY_PAYLOAD_SIZE, decode_buddy_payload};
    use c2_wire::chunk::{CHUNK_HEADER_SIZE, decode_chunk_header};
    use c2_wire::control::*;
    use c2_wire::flags;
    use c2_wire::frame;
    use c2_wire::handshake::*;

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
            observed_route_revision: 1,
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
        let ctrl = encode_call_control(&call_identity("grid"), 0).unwrap();
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
        let ctrl = try_encode_reply_control(&ReplyControl::Success).unwrap();
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
        let ctrl = try_encode_reply_control(&ReplyControl::Error(err.clone())).unwrap();
        let frame_bytes =
            frame::encode_frame(42, flags::FLAG_RESPONSE | flags::FLAG_REPLY_V2, &ctrl);

        let (hdr, frame_payload) = frame::decode_frame(&frame_bytes).unwrap();
        assert!(hdr.is_response());
        assert!(hdr.is_reply_v2());

        let (decoded, _) = decode_reply_control(frame_payload, 0).unwrap();
        assert_eq!(decoded, ReplyControl::Error(err));
    }

    #[test]
    fn handshake_client_message() {
        // Client sends: [version][segments][cap_flags]
        let segments = vec![("seg0".into(), 268_435_456u32)];
        let encoded = encode_client_handshake(&segments, CAP_CALL_V2 | CAP_METHOD_IDX, "").unwrap();
        let frame_bytes = frame::encode_frame(0, flags::FLAG_HANDSHAKE, &encoded);

        let (hdr, payload) = frame::decode_frame(&frame_bytes).unwrap();
        assert!(hdr.is_handshake());
        assert_eq!(hdr.request_id, 0);

        let hs = decode_handshake(payload).unwrap();
        assert_eq!(hs.segments.len(), 1);
        assert_eq!(hs.capability_flags, CAP_CALL_V2 | CAP_METHOD_IDX);
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
        // payload = [15B buddy_payload][call_control]
        // flags   = FLAG_CALL_V2 | FLAG_BUDDY
        use c2_wire::buddy::{BuddyPayload, encode_buddy_payload};

        let bp = BuddyPayload {
            seg_idx: 0,
            generation: 1,
            offset: 4096,
            data_size: 8192,
            is_dedicated: false,
        };
        let buddy_bytes = encode_buddy_payload(&bp);
        assert_eq!(buddy_bytes.len(), BUDDY_PAYLOAD_SIZE);
        assert_eq!(buddy_bytes, [0, 0, 1, 0, 0, 0, 0, 16, 0, 0, 0, 32, 0, 0, 0]);

        let ctrl = encode_call_control(&call_identity("grid"), 3).unwrap();
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
            generation: 0,
            offset: 0,
            data_size: 1_000_000,
            is_dedicated: true,
        };
        let buddy_bytes = encode_buddy_payload(&bp);
        assert_eq!(
            buddy_bytes,
            [5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x40, 0x42, 0x0f, 0, 1]
        );
        let ctrl = encode_call_control(&call_identity("net"), 1).unwrap();

        let mut payload = Vec::new();
        payload.extend_from_slice(&buddy_bytes);
        payload.extend_from_slice(&ctrl);

        let frame_bytes = frame::encode_frame(7, flags::FLAG_CALL_V2 | flags::FLAG_BUDDY, &payload);

        let (hdr, frame_payload) = frame::decode_frame(&frame_bytes).unwrap();
        assert!(hdr.is_buddy());
        let (decoded_bp, _) = decode_buddy_payload(frame_payload).unwrap();
        assert_eq!(decoded_bp.seg_idx, 5);
        assert_eq!(decoded_bp.generation, 0);
        assert!(decoded_bp.is_dedicated);
        assert_eq!(decoded_bp.data_size, 1_000_000);
    }

    #[test]
    fn test_chunked_frame_encoding() {
        // Simulate a 3-chunk transfer.
        use c2_wire::chunk::encode_chunk_header;

        let route = "grid";
        let method_idx: u16 = 2;
        let total_chunks: u16 = 3;
        let request_id: u64 = 42;

        let ctrl = encode_call_control(&call_identity(route), method_idx).unwrap();
        let chunk_data = b"chunk_payload_data";

        // ── Chunk 0: [chunk_header][call_control][data] ──
        let chunk_hdr_0 = encode_chunk_header(0, total_chunks);
        let mut payload_0 = Vec::new();
        payload_0.extend_from_slice(&chunk_hdr_0);
        payload_0.extend_from_slice(&ctrl);
        payload_0.extend_from_slice(chunk_data);

        let flags_0 = flags::FLAG_CALL_V2 | flags::FLAG_CHUNKED;
        let frame_0 = frame::encode_frame(request_id, flags_0, &payload_0);

        let (hdr_0, fp_0) = frame::decode_frame(&frame_0).unwrap();
        assert_eq!(hdr_0.request_id, request_id);
        assert!(hdr_0.is_call_v2());
        assert!(flags::is_chunked(hdr_0.flags));
        assert!(!flags::is_chunk_last(hdr_0.flags));

        // Decode chunk header.
        let (chunk_idx, total, ch_consumed) = decode_chunk_header(fp_0, 0).unwrap();
        assert_eq!(chunk_idx, 0);
        assert_eq!(total, 3);
        assert_eq!(ch_consumed, CHUNK_HEADER_SIZE);

        // Decode call control (present on chunk 0).
        let (decoded_ctrl, ctrl_consumed) = decode_call_control(fp_0, CHUNK_HEADER_SIZE).unwrap();
        assert_eq!(decoded_ctrl.identity.route_name, route);
        assert_eq!(decoded_ctrl.method_idx, method_idx);

        // Remaining is chunk data.
        let data_start = CHUNK_HEADER_SIZE + ctrl_consumed;
        assert_eq!(&fp_0[data_start..], chunk_data);

        // ── Chunk 1: [chunk_header][data] (no call_control) ──
        let chunk_hdr_1 = encode_chunk_header(1, total_chunks);
        let mut payload_1 = Vec::new();
        payload_1.extend_from_slice(&chunk_hdr_1);
        payload_1.extend_from_slice(chunk_data);

        let flags_1 = flags::FLAG_CALL_V2 | flags::FLAG_CHUNKED;
        let frame_1 = frame::encode_frame(request_id, flags_1, &payload_1);

        let (hdr_1, fp_1) = frame::decode_frame(&frame_1).unwrap();
        assert!(flags::is_chunked(hdr_1.flags));
        assert!(!flags::is_chunk_last(hdr_1.flags));
        let (ci_1, tc_1, _) = decode_chunk_header(fp_1, 0).unwrap();
        assert_eq!(ci_1, 1);
        assert_eq!(tc_1, 3);
        assert_eq!(&fp_1[CHUNK_HEADER_SIZE..], chunk_data);

        // ── Chunk 2 (last): [chunk_header][data] + FLAG_CHUNK_LAST ──
        let chunk_hdr_2 = encode_chunk_header(2, total_chunks);
        let mut payload_2 = Vec::new();
        payload_2.extend_from_slice(&chunk_hdr_2);
        payload_2.extend_from_slice(chunk_data);

        let flags_2 = flags::FLAG_CALL_V2 | flags::FLAG_CHUNKED | flags::FLAG_CHUNK_LAST;
        let frame_2 = frame::encode_frame(request_id, flags_2, &payload_2);

        let (hdr_2, fp_2) = frame::decode_frame(&frame_2).unwrap();
        assert!(flags::is_chunked(hdr_2.flags));
        assert!(flags::is_chunk_last(hdr_2.flags));
        let (ci_2, tc_2, _) = decode_chunk_header(fp_2, 0).unwrap();
        assert_eq!(ci_2, 2);
        assert_eq!(tc_2, 3);
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
    fn call_full_is_not_a_public_production_api() {
        let client_source = include_str!("client.rs");
        let client_production = client_source
            .split("#[cfg(test)]")
            .next()
            .expect("client.rs must contain a production section");
        assert!(
            !client_production.contains("pub async fn call_full("),
            "IpcClient must expose one canonical semantic call API"
        );
        assert!(
            !client_production.contains("refresh_route_contract"),
            "IpcClient must not reintroduce name-only route contract refresh"
        );

        let sync_source = include_str!("sync_client.rs");
        let sync_production = sync_source
            .split("#[cfg(test)]")
            .next()
            .expect("sync_client.rs must contain a production section");
        assert!(
            !sync_production.contains(".call_full("),
            "SyncClient must delegate to canonical route-bound IPC calls"
        );
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
        let client = IpcClient::with_config("ipc://configured", cfg.clone());
        assert!(client.pool.is_some());
        assert_eq!(client.config.shm_threshold, cfg.shm_threshold);

        // A disabled buddy pool is still a real pool: it keeps dedicated SHM
        // requests and the wire prefix alive, only the buddy tiers are off.
        let mut disabled = cfg;
        disabled.base.pool_enabled = false;
        let client = IpcClient::with_config("ipc://configured-no-pool", disabled);
        let pool = client.pool.as_ref().expect("policy-disabled pool stays live");
        let mut pool = pool.lock();
        assert!(!pool.config().buddy_enabled);
        assert_eq!(pool.segment_count(), 0);
        assert!(pool.ensure_ready().is_ok());
        assert_eq!(pool.segment_count(), 0);
    }

    #[test]
    fn test_chunked_single_chunk() {
        // Edge case: data exactly at chunk_size boundary → 1 chunk.
        use c2_wire::chunk::encode_chunk_header;

        let chunk_hdr = encode_chunk_header(0, 1);
        let ctrl = encode_call_control(&call_identity("route"), 0).unwrap();
        let data = vec![0xABu8; 128];

        let mut payload = Vec::new();
        payload.extend_from_slice(&chunk_hdr);
        payload.extend_from_slice(&ctrl);
        payload.extend_from_slice(&data);

        let flags_last = flags::FLAG_CALL_V2 | flags::FLAG_CHUNKED | flags::FLAG_CHUNK_LAST;
        let frame_bytes = frame::encode_frame(1, flags_last, &payload);

        let (hdr, fp) = frame::decode_frame(&frame_bytes).unwrap();
        assert!(flags::is_chunked(hdr.flags));
        assert!(flags::is_chunk_last(hdr.flags));
        let (ci, tc, _) = decode_chunk_header(fp, 0).unwrap();
        assert_eq!(ci, 0);
        assert_eq!(tc, 1);
    }

    // ── SyncClient tests ──────────────────────────────────────────────

    #[test]
    fn test_sync_client_global_runtime() {
        // get_or_create_runtime() must return the same runtime on every call.
        let rt1 = crate::sync_client::tests::runtime_ptr();
        let rt2 = crate::sync_client::tests::runtime_ptr();
        assert_eq!(rt1, rt2, "global runtime should be the same instance");
    }

    #[test]
    fn test_ipc_config_propagation() {
        // Verify custom config flows through to IpcClient via with_pool.
        // We can't actually connect (no server), but construction must succeed
        // and the config should influence transport path selection.
        let cfg = ClientIpcConfig {
            shm_threshold: 512,
            base: c2_config::BaseIpcConfig {
                chunk_size: 2048,
                ..c2_config::BaseIpcConfig::default()
            },
            ..ClientIpcConfig::default()
        };

        // IpcClient::new uses default config.
        let c1 = crate::client::IpcClient::new("ipc://test_prop_1");
        assert!(!c1.is_connected());

        // IpcClient::with_pool uses custom config.
        let pool = std::sync::Arc::new(parking_lot::Mutex::new(c2_mem::MemPool::new(
            c2_mem::PoolConfig::default(),
        )));
        let c2 = crate::client::IpcClient::with_pool("ipc://test_prop_2", pool, cfg);
        assert!(!c2.is_connected());
    }

    #[test]
    fn test_handshake_with_pool_segments() {
        // Verify that a handshake with pool segments is encoded correctly.
        let segments = vec![
            ("seg_a".into(), 256 * 1024 * 1024u32),
            ("seg_b".into(), 256 * 1024 * 1024u32),
        ];
        let cap = CAP_CALL_V2 | CAP_METHOD_IDX | CAP_CHUNKED;
        let encoded = encode_client_handshake(&segments, cap, "test_prefix").unwrap();
        let frame_bytes = frame::encode_frame(0, flags::FLAG_HANDSHAKE, &encoded);

        let (hdr, payload) = frame::decode_frame(&frame_bytes).unwrap();
        assert!(hdr.is_handshake());

        let hs = decode_handshake(payload).unwrap();
        assert_eq!(hs.segments.len(), 2);
        assert_eq!(hs.segments[0].0, "seg_a");
        assert_eq!(hs.segments[1].0, "seg_b");
        assert_eq!(hs.capability_flags, cap);
        assert_eq!(hs.prefix, "test_prefix");
    }
}

#[cfg(test)]
mod response_lease_tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use c2_mem::{MemPool, PoolConfig};
    use parking_lot::{Mutex, RwLock};

    use crate::{ResponseData, ResponseLease, ServerPoolState};

    const SEGMENT_SIZE: usize = 64 * 1024;

    fn pool_config() -> PoolConfig {
        PoolConfig {
            segment_size: SEGMENT_SIZE,
            min_block_size: 4096,
            max_segments: 2,
            max_dedicated_segments: 2,
            dedicated_crash_timeout_secs: 5.0,
            buddy_idle_decay_secs: 1.0,
            spill_threshold: 1.0,
            spill_dir: std::env::temp_dir().join("c_two_response_lease_test_spill"),
            ..PoolConfig::default()
        }
    }

    fn unique_prefix(label: char) -> String {
        static NEXT_POOL: AtomicUsize = AtomicUsize::new(0);
        let sequence = NEXT_POOL.fetch_add(1, Ordering::Relaxed);
        format!("/c2lr{:08x}{sequence:04x}{label}", std::process::id())
            .chars()
            .take(24)
            .collect()
    }

    fn empty_reassembly_pool(label: char) -> Arc<RwLock<MemPool>> {
        Arc::new(RwLock::new(MemPool::new_with_prefix(
            pool_config(),
            unique_prefix(label),
        )))
    }

    #[test]
    fn inline_response_copy_and_release_are_independent_and_idempotent() {
        let mut lease = ResponseLease::new(
            ResponseData::Inline(b"inline response".to_vec()),
            Arc::new(Mutex::new(None)),
            empty_reassembly_pool('i'),
        );

        assert_eq!(lease.copy_bytes().unwrap(), b"inline response");
        assert!(!lease.is_released());
        lease.release().unwrap();
        lease.release().unwrap();
        assert!(lease.is_released());
    }

    #[test]
    fn buddy_and_dedicated_response_leases_copy_then_release_real_backings() {
        assert_shm_response(b"buddy response".repeat(128), false, 'b');
        assert_shm_response(vec![0xD2; SEGMENT_SIZE + 4096], true, 'd');
    }

    #[test]
    fn reassembly_handle_response_copies_then_releases_real_backing() {
        let payload = b"reassembled response".repeat(256);
        for logical_len in [payload.len(), 4096, 1, 0] {
            let pool = empty_reassembly_pool('h');
            let handle = {
                let mut pool = pool.write();
                let mut handle = pool.alloc_handle(payload.len()).unwrap();
                pool.handle_slice_mut(&mut handle).copy_from_slice(&payload);
                handle.set_len(logical_len);
                handle
            };
            let mut lease = ResponseLease::new(
                ResponseData::Handle(handle),
                Arc::new(Mutex::new(None)),
                Arc::clone(&pool),
            );

            assert_eq!(lease.copy_bytes().unwrap(), payload[..logical_len]);
            assert_eq!(pool.read().stats().alloc_count, 1);
            lease.release().unwrap();
            assert_eq!(pool.read().stats().alloc_count, 0);
        }
    }

    #[test]
    fn file_spill_response_lease_preserves_trim_and_release_state() {
        let payload = b"file-backed response".repeat(256);
        for logical_len in [payload.len(), 1, 0] {
            let mut config = pool_config();
            config.spill_threshold = 0.0;
            let mut owner = MemPool::new_with_prefix(config, unique_prefix('f'));
            let mut handle = owner.alloc_handle(payload.len()).unwrap();
            assert!(handle.is_file_spill());
            owner.handle_slice_mut(&mut handle).copy_from_slice(&payload);
            handle.set_len(logical_len);
            #[cfg(windows)]
            let path = match &handle {
                c2_mem::MemHandle::FileSpill { path, .. } => path.clone(),
                _ => unreachable!(),
            };
            let mut lease = ResponseLease::new(
                ResponseData::Handle(handle),
                Arc::new(Mutex::new(None)),
                Arc::new(RwLock::new(owner)),
            );
            #[cfg(windows)]
            assert!(path.exists());
            assert_eq!(lease.copy_bytes().unwrap(), payload[..logical_len]);
            lease.release().unwrap();
            lease.release().unwrap();
            assert!(lease.is_released());
            assert!(lease.copy_bytes().unwrap_err().contains("already released"));
            #[cfg(windows)]
            assert!(!path.exists());
        }
    }

    #[test]
    fn invalid_shm_span_fails_copy_and_drop_without_freeing_an_allocation() {
        let prefix = unique_prefix('x');
        let mut producer = MemPool::new_with_prefix(pool_config(), prefix.clone());
        let allocation = producer.alloc(4096).unwrap();
        let segment_capacity = producer
            .segment(allocation.seg_idx as usize)
            .unwrap()
            .allocator()
            .data_size();
        let reader = MemPool::open_peer(pool_config(), producer.prefix().to_string());
        let server_pool = Arc::new(Mutex::new(Some(ServerPoolState::from_pool_for_test(
            reader,
        ))));
        let lease = ResponseLease::new(
            ResponseData::Shm {
                seg_idx: u16::try_from(allocation.seg_idx).unwrap(),
                generation: allocation.generation,
                offset: u32::try_from(segment_capacity - 1).unwrap(),
                data_size: 2,
                is_dedicated: false,
            },
            server_pool,
            empty_reassembly_pool('r'),
        );

        assert!(
            lease
                .copy_bytes()
                .unwrap_err()
                .contains("outside buddy segment")
        );
        drop(lease);

        assert_eq!(producer.stats().alloc_count, 1);
        producer.free(&allocation).unwrap();
    }

    #[test]
    fn consuming_copy_retains_copy_and_release_failures() {
        let pool = empty_reassembly_pool('c');
        let handle = {
            let mut pool = pool.write();
            let mut handle = pool.alloc_handle(16).unwrap();
            pool.handle_slice_mut(&mut handle)
                .copy_from_slice(b"combined failure");
            handle
        };
        let lease = ResponseLease::new(
            ResponseData::Handle(handle),
            Arc::new(Mutex::new(None)),
            Arc::clone(&pool),
        );
        *pool.write() = MemPool::new_with_prefix(pool_config(), unique_prefix('z'));

        let error = lease.into_owned_bytes().unwrap_err();
        assert!(error.contains("response handle copy failed"), "{error}");
        assert!(error.contains("response handle release failed"), "{error}");
    }

    fn assert_shm_response(payload: Vec<u8>, expected_dedicated: bool, label: char) {
        let prefix = unique_prefix(label);
        let mut producer = MemPool::new_with_prefix(pool_config(), prefix.clone());
        let allocation = producer.alloc(payload.len()).unwrap();
        assert_eq!(allocation.is_dedicated, expected_dedicated);
        let pointer = producer.data_ptr(&allocation).unwrap();
        unsafe {
            std::ptr::copy_nonoverlapping(payload.as_ptr(), pointer, payload.len());
        }

        let reader = MemPool::open_peer(pool_config(), producer.prefix().to_string());
        let server_pool = Arc::new(Mutex::new(Some(ServerPoolState::from_pool_for_test(
            reader,
        ))));
        let mut lease = ResponseLease::new(
            ResponseData::Shm {
                seg_idx: u16::try_from(allocation.seg_idx).unwrap(),
                generation: allocation.generation,
                offset: allocation.offset,
                data_size: u32::try_from(payload.len()).unwrap(),
                is_dedicated: allocation.is_dedicated,
            },
            Arc::clone(&server_pool),
            empty_reassembly_pool(label.to_ascii_uppercase()),
        );

        assert_eq!(lease.copy_bytes().unwrap(), payload);
        lease.release().unwrap();
        assert_eq!(
            server_pool
                .lock()
                .as_ref()
                .unwrap()
                .pool
                .stats()
                .alloc_count,
            0
        );
    }
}

/// Phase 1A regression proofs: buddy policy enforcement against a real
/// `c2-server` over a real local stream — handshake, lazy-open, allocation,
/// and reply-transport selection all take their production code paths.
#[cfg(test)]
mod lazy_policy_roundtrip_tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use c2_config::BaseIpcConfig;
    use c2_mem::{MemPool, PoolAllocation};
    use c2_server::{
        AccessLevel, ConcurrencyMode, CrmCallback, CrmError, RequestData, RequestLease, ResponseMeta,
        RouteBuildSpec, SchedulerLimits, Server, ServerIpcConfig,
    };
    use parking_lot::{Mutex, RwLock};
    use tokio::time::timeout;

    use crate::client::{ClientIpcConfig, IpcClient, IpcError};
    use crate::response::ResponseData;

    const ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SIG_HASH: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    static ADDR_COUNTER: AtomicU64 = AtomicU64::new(1);

    fn unique_address(label: &str) -> String {
        let n = ADDR_COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("ipc://{label}_{}_{n}", std::process::id())
    }

    fn route_spec(route_name: &str) -> RouteBuildSpec {
        let mut access_map = HashMap::new();
        access_map.insert(0u16, AccessLevel::Write);
        RouteBuildSpec {
            name: route_name.into(),
            crm_ns: "test.grid".into(),
            crm_name: "Grid".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: ABI_HASH.into(),
            signature_hash: SIG_HASH.into(),
            method_names: vec!["echo".into()],
            access_map,
            concurrency_mode: ConcurrencyMode::Parallel,
            limits: SchedulerLimits::try_from_usize(None, None).unwrap(),
        }
    }

    fn expected_contract(route_name: &str) -> c2_contract::ExpectedRouteContract {
        c2_contract::ExpectedRouteContract {
            route_name: route_name.into(),
            crm_ns: "test.grid".into(),
            crm_name: "Grid".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: ABI_HASH.into(),
            signature_hash: SIG_HASH.into(),
        }
    }

    /// Which request transport the callback actually received.
    fn request_kind(request: &RequestData) -> &'static str {
        match request {
            RequestData::Inline(_) => "inline",
            RequestData::Shm { is_dedicated, .. } => {
                if *is_dedicated {
                    "shm_dedicated"
                } else {
                    "shm_buddy"
                }
            }
            RequestData::Handle { .. } => "chunked_handle",
        }
    }

    /// Echo callback: returns the exact request bytes as an owned inline
    /// response so the server's reply-transport selection (dedicated SHM vs
    /// chunked) is decided by config, exactly as in production dispatch. It
    /// also records the request transport it observed.
    struct Echo {
        seen_kinds: Arc<Mutex<Vec<&'static str>>>,
    }

    impl CrmCallback for Echo {
        fn invoke(
            &self,
            _route_name: &str,
            _method_idx: u16,
            request: RequestData,
            _response_pool: Arc<RwLock<MemPool>>,
        ) -> Result<ResponseMeta, CrmError> {
            self.seen_kinds.lock().push(request_kind(&request));
            let request = RequestLease::new(request);
            let bytes = request.into_owned_bytes().map_err(CrmError::InternalError)?;
            Ok(ResponseMeta::Inline(bytes))
        }
    }

    fn echo_callback() -> (Arc<Echo>, Arc<Mutex<Vec<&'static str>>>) {
        let seen_kinds = Arc::new(Mutex::new(Vec::new()));
        (Arc::new(Echo { seen_kinds: Arc::clone(&seen_kinds) }), seen_kinds)
    }

    /// Echo callback that first holds every dedicated response segment it can
    /// get. With buddy disabled, exhausting `max_dedicated_segments` forces
    /// the reply path to fall back to chunked transfer — proving the fallback
    /// chain stays intact when the buddy tiers are policy-disabled.
    struct DedicatedHoggingEcho {
        held: Arc<Mutex<Vec<PoolAllocation>>>,
        hog_count: usize,
        hog_size: usize,
    }

    impl CrmCallback for DedicatedHoggingEcho {
        fn invoke(
            &self,
            _route_name: &str,
            _method_idx: u16,
            request: RequestData,
            response_pool: Arc<RwLock<MemPool>>,
        ) -> Result<ResponseMeta, CrmError> {
            let request = RequestLease::new(request);
            {
                let mut pool = response_pool.write();
                let mut held = self.held.lock();
                while held.len() < self.hog_count {
                    held.push(
                        pool.alloc(self.hog_size)
                            .map_err(CrmError::InternalError)?,
                    );
                }
            }
            let bytes = request.into_owned_bytes().map_err(CrmError::InternalError)?;
            Ok(ResponseMeta::Inline(bytes))
        }
    }

    async fn start_echo_server(
        label: &str,
        config: ServerIpcConfig,
        callback: Arc<dyn CrmCallback>,
    ) -> Arc<Server> {
        let server = Arc::new(Server::new(&unique_address(label), config).unwrap());
        let built = server.build_route(route_spec(label), callback).unwrap();
        let reservation = server.reserve_route(built).await.unwrap();
        server.commit_reserved_route(reservation).await.unwrap();

        {
            let server = Arc::clone(&server);
            tokio::spawn(async move {
                let _ = server.run().await;
            });
        }
        server.wait_until_ready(Duration::from_secs(5)).await.unwrap();
        Arc::clone(&server)
    }

    async fn stop_server(server: &Arc<Server>) {
        timeout(Duration::from_secs(5), server.shutdown_and_wait(Duration::from_secs(5)))
            .await
            .expect("server must stop")
            .unwrap();
    }

    /// Base config with small, cheap mappings and a consistent memory budget.
    fn small_base(segment_size: u64, segments: u32) -> BaseIpcConfig {
        BaseIpcConfig {
            pool_segment_size: segment_size,
            max_pool_segments: segments,
            max_pool_memory: segment_size * u64::from(segments),
            ..BaseIpcConfig::default()
        }
    }

    fn server_config(base: BaseIpcConfig, shm_threshold: u64) -> ServerIpcConfig {
        ServerIpcConfig {
            base,
            shm_threshold,
            ..ServerIpcConfig::default()
        }
    }

    fn client_config(base: BaseIpcConfig, shm_threshold: u64) -> ClientIpcConfig {
        ClientIpcConfig {
            base,
            shm_threshold,
            ..ClientIpcConfig::default()
        }
    }

    fn own_pool_segment_count(client: &IpcClient) -> usize {
        client
            .pool
            .as_ref()
            .expect("config-owned client pool")
            .lock()
            .segment_count()
    }

    async fn echo_roundtrip(
        client: &IpcClient,
        route_name: &str,
        payload: &[u8],
    ) -> ResponseData {
        let binding = client
            .acquire_route(&expected_contract(route_name))
            .await
            .unwrap();
        timeout(
            Duration::from_secs(10),
            client.call_bound(&binding, "echo", payload),
        )
        .await
        .expect("echo call must not hang")
        .unwrap()
    }

    fn response_bytes(client: &IpcClient, response: ResponseData) -> Vec<u8> {
        response
            .into_bytes_with_pool(&client.server_pool_arc().clone(), &client.reassembly_pool_arc())
            .unwrap()
    }

    // ── Lazy startup: no buddy mappings without explicit prewarm ─────────

    #[tokio::test]
    async fn lazy_startup_maps_no_buddy_memory_and_serves_one_byte_ipc() {
        let (callback, seen_kinds) = echo_callback();
        let server = start_echo_server(
            "lazy_startup",
            server_config(small_base(64 * 1024, 2), 4096),
            callback,
        )
        .await;

        let mut client = IpcClient::with_config(
            server.ipc_address(),
            client_config(small_base(64 * 1024, 2), 4096),
        );
        client.connect().await.unwrap();

        // Default policy (prewarm 0): the handshake announced zero buddy
        // segments and neither side mapped any buddy memory.
        assert_eq!(own_pool_segment_count(&client), 0);
        assert_eq!(server.response_pool_arc().read().segment_count(), 0);
        assert!(client.pool.as_ref().unwrap().lock().config().buddy_enabled);

        // A one-byte inline round trip must not change that.
        let response = echo_roundtrip(&client, "lazy_startup", b"x").await;
        assert!(matches!(response, ResponseData::Inline(_)));
        assert_eq!(response_bytes(&client, response), b"x");
        assert_eq!(*seen_kinds.lock(), vec!["inline"]);
        assert_eq!(own_pool_segment_count(&client), 0);
        assert_eq!(server.response_pool_arc().read().segment_count(), 0);

        client.close().await;
        stop_server(&server).await;
    }

    // ── Disabled buddy: dedicated SHM still carries large payloads ───────

    #[tokio::test]
    async fn disabled_buddy_serves_dedicated_large_request_and_response() {
        let (callback, seen_kinds) = echo_callback();
        // chunk_size above the payload keeps the chunked fallback out of the
        // picture: dedicated SHM alone must carry both directions.
        let server = start_echo_server(
            "disabled_dedicated",
            server_config(
                BaseIpcConfig {
                    pool_enabled: false,
                    chunk_size: 1 << 20,
                    ..small_base(64 * 1024, 2)
                },
                1024,
            ),
            callback,
        )
        .await;

        let mut client = IpcClient::with_config(
            server.ipc_address(),
            client_config(
                BaseIpcConfig {
                    pool_enabled: false,
                    chunk_size: 1 << 20,
                    ..small_base(64 * 1024, 2)
                },
                1024,
            ),
        );
        client.connect().await.unwrap();

        let payload: Vec<u8> = (0..64 * 1024u32).map(|i| (i % 251) as u8).collect();
        let response = echo_roundtrip(&client, "disabled_dedicated", &payload).await;

        // The request reached the callback as dedicated SHM coordinates even
        // though the client's pool exists — buddy tiers are policy-disabled.
        assert_eq!(*seen_kinds.lock(), vec!["shm_dedicated"]);

        // The reply came back as dedicated SHM coordinates, not inline bytes:
        // with buddy disabled the response pool allocated dedicated segments.
        let ResponseData::Shm {
            is_dedicated,
            data_size,
            ..
        } = &response
        else {
            panic!("expected SHM response, got {response:?}");
        };
        assert!(*is_dedicated, "response must use dedicated SHM");
        assert_eq!(*data_size as usize, payload.len());
        assert_eq!(response_bytes(&client, response), payload);

        // No buddy segments were ever mapped on either side.
        assert!(!client.pool.as_ref().unwrap().lock().config().buddy_enabled);
        assert_eq!(own_pool_segment_count(&client), 0);
        let server_pool = server.response_pool_arc();
        let pool_guard = server_pool.read();
        assert!(!pool_guard.config().buddy_enabled);
        assert_eq!(pool_guard.segment_count(), 0);
        assert!(pool_guard.stats().dedicated_segments >= 1);
        drop(pool_guard);

        client.close().await;
        stop_server(&server).await;
    }

    // ── Disabled buddy: chunked receive fallback stays available ─────────

    #[tokio::test]
    async fn disabled_buddy_falls_back_to_chunked_reply_when_dedicated_exhausts() {
        let held: Arc<Mutex<Vec<PoolAllocation>>> = Arc::new(Mutex::new(Vec::new()));
        let callback = Arc::new(DedicatedHoggingEcho {
            held: Arc::clone(&held),
            // The response-role projection caps dedicated segments at 4.
            hog_count: 4,
            hog_size: 64 * 1024,
        });
        let server = start_echo_server(
            "disabled_chunked",
            server_config(
                BaseIpcConfig {
                    pool_enabled: false,
                    chunk_size: 8 * 1024,
                    ..small_base(64 * 1024, 2)
                },
                1024,
            ),
            callback,
        )
        .await;

        let mut client = IpcClient::with_config(
            server.ipc_address(),
            client_config(
                BaseIpcConfig {
                    pool_enabled: false,
                    // Request stays inline; this test isolates the reply path.
                    ..small_base(64 * 1024, 2)
                },
                1 << 20,
            ),
        );
        client.connect().await.unwrap();

        let payload: Vec<u8> = (0..64 * 1024u32).map(|i| (i % 241) as u8).collect();
        let response = echo_roundtrip(&client, "disabled_chunked", &payload).await;

        // Dedicated capacity was exhausted, so the reply fell back to chunked
        // transfer and the client reassembled it — into dedicated SHM, because
        // the client's reassembly pool follows the same buddy policy.
        let handle = match response {
            ResponseData::Handle(handle) => handle,
            other => panic!("expected chunked Handle response, got {other:?}"),
        };
        let reassembly = client.reassembly_pool_arc();
        assert!(!reassembly.read().config().buddy_enabled);
        assert_eq!(reassembly.read().handle_slice(&handle), payload);
        reassembly.write().release_handle(handle);

        // Release the hogged dedicated segments.
        {
            let response_pool = server.response_pool_arc();
            let mut pool = response_pool.write();
            for alloc in held.lock().drain(..) {
                pool.free(&alloc).unwrap();
            }
        }

        client.close().await;
        stop_server(&server).await;
    }

    #[tokio::test]
    async fn disabled_buddy_chunked_request_reassembles_into_dedicated_shm() {
        let (callback, seen_kinds) = echo_callback();
        let server = start_echo_server(
            "disabled_chunk_req",
            server_config(
                BaseIpcConfig {
                    pool_enabled: false,
                    ..small_base(64 * 1024, 2)
                },
                1 << 20,
            ),
            callback,
        )
        .await;

        let mut client = IpcClient::with_config(
            server.ipc_address(),
            client_config(
                BaseIpcConfig {
                    pool_enabled: false,
                    chunk_size: 4 * 1024,
                    ..small_base(64 * 1024, 2)
                },
                // 64 KiB stays below the SHM threshold but above chunk_size,
                // so the client sends the request as chunks.
                1 << 20,
            ),
        );
        client.connect().await.unwrap();

        let payload: Vec<u8> = (0..64 * 1024u32).map(|i| (i % 239) as u8).collect();
        let response = echo_roundtrip(&client, "disabled_chunk_req", &payload).await;

        // The chunked request was reassembled server-side into a MemHandle
        // backed by the reassembly pool's dedicated tier.
        assert_eq!(*seen_kinds.lock(), vec!["chunked_handle"]);
        assert!(matches!(response, ResponseData::Inline(_)));
        assert_eq!(response_bytes(&client, response), payload);

        client.close().await;
        stop_server(&server).await;
    }

    // ── Lazy creation after an empty handshake with unequal geometry ─────

    #[tokio::test]
    async fn lazy_buddy_created_after_empty_handshake_with_unequal_segment_sizes() {
        let (callback, seen_kinds) = echo_callback();
        let server = start_echo_server(
            "unequal_lazy",
            server_config(small_base(64 * 1024, 2), 1024),
            callback,
        )
        .await;

        // Deliberately different client/server segment geometry; neither side
        // may substitute its local default for the peer's real geometry.
        let mut client = IpcClient::with_config(
            server.ipc_address(),
            client_config(small_base(256 * 1024, 2), 1024),
        );
        client.connect().await.unwrap();

        assert_eq!(own_pool_segment_count(&client), 0);
        assert_eq!(server.response_pool_arc().read().segment_count(), 0);

        let payload: Vec<u8> = (0..32 * 1024u32).map(|i| (i % 253) as u8).collect();
        let response = echo_roundtrip(&client, "unequal_lazy", &payload).await;

        // The request crossed the 1 KiB threshold: the client lazily created
        // its first 256 KiB buddy segment after the handshake (announced to
        // nobody — the server lazy-opened it from the frame's coordinates
        // instead of trusting its own 64 KiB geometry).
        assert_eq!(*seen_kinds.lock(), vec!["shm_buddy"]);

        // The reply arrived as buddy SHM from the server's lazily created
        // 64 KiB response pool, which the client lazy-opened symmetrically.
        let ResponseData::Shm {
            is_dedicated,
            data_size,
            ..
        } = &response
        else {
            panic!("expected SHM response, got {response:?}");
        };
        assert!(!*is_dedicated, "buddy-enabled pools must serve buddy replies");
        assert_eq!(*data_size as usize, payload.len());
        assert_eq!(response_bytes(&client, response), payload);

        assert_eq!(own_pool_segment_count(&client), 1);
        let server_pool = server.response_pool_arc();
        let pool_guard = server_pool.read();
        assert_eq!(pool_guard.segment_count(), 1);
        assert_eq!(
            pool_guard.config().segment_size,
            64 * 1024,
            "server response pool must keep its own configured geometry"
        );
        drop(pool_guard);

        client.close().await;
        stop_server(&server).await;
    }

    // ── Shared helpers for the corrective (Host-review) tests ────────────

    /// Poll every 25 ms until `cond` holds, panicking after `secs`.
    async fn wait_until(secs: u64, mut cond: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        while !cond() {
            if tokio::time::Instant::now() >= deadline {
                panic!("condition not reached within {secs}s");
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Client config with a fast idle-decay window so maintenance-driven
    /// retirement is observable in bounded test time.
    fn fast_client_config(base: BaseIpcConfig, shm_threshold: u64) -> ClientIpcConfig {
        ClientIpcConfig {
            base,
            shm_threshold,
            pool_decay_seconds: 0.05,
            ..ClientIpcConfig::default()
        }
    }

    /// Echo callback that first holds `hog_count` allocations in the server's
    /// response pool. With buddy enabled and 64 KiB segments, 12 × 32 KiB
    /// fills 4 buddy segments (2 blocks each) plus the response role's 4
    /// dedicated segments, forcing the *next* reply through the chunked
    /// transport without disabling the buddy policy.
    struct PoolHoggingEcho {
        seen_kinds: Arc<Mutex<Vec<&'static str>>>,
        held: Arc<Mutex<Vec<PoolAllocation>>>,
        hog_count: usize,
        hog_size: usize,
    }

    impl CrmCallback for PoolHoggingEcho {
        fn invoke(
            &self,
            _route_name: &str,
            _method_idx: u16,
            request: RequestData,
            response_pool: Arc<RwLock<MemPool>>,
        ) -> Result<ResponseMeta, CrmError> {
            self.seen_kinds.lock().push(request_kind(&request));
            let request = RequestLease::new(request);
            {
                let mut pool = response_pool.write();
                let mut held = self.held.lock();
                while held.len() < self.hog_count {
                    held.push(
                        pool.alloc(self.hog_size)
                            .map_err(CrmError::InternalError)?,
                    );
                }
            }
            let bytes = request.into_owned_bytes().map_err(CrmError::InternalError)?;
            Ok(ResponseMeta::Inline(bytes))
        }
    }

    /// Echo callback that keeps every received `RequestData::Shm` alive, so
    /// the client's buddy request blocks stay allocated (no reuse) while the
    /// calls are in flight. Records the segment index each request used.
    struct RequestHoldingEcho {
        held: Arc<Mutex<Vec<RequestLease>>>,
        seen_seg_idx: Arc<Mutex<Vec<u16>>>,
        seen_kinds: Arc<Mutex<Vec<&'static str>>>,
    }

    impl CrmCallback for RequestHoldingEcho {
        fn invoke(
            &self,
            _route_name: &str,
            _method_idx: u16,
            request: RequestData,
            _response_pool: Arc<RwLock<MemPool>>,
        ) -> Result<ResponseMeta, CrmError> {
            self.seen_kinds.lock().push(request_kind(&request));
            if let RequestData::Shm { seg_idx, .. } = &request {
                self.seen_seg_idx.lock().push(*seg_idx);
            }
            let request = RequestLease::new(request);
            let bytes = request.copy_bytes().map_err(CrmError::InternalError)?;
            self.held.lock().push(request);
            Ok(ResponseMeta::Inline(bytes))
        }
    }

    // ── Injected pools cannot bypass the configured buddy policy ─────────

    #[tokio::test]
    async fn injected_buddy_enabled_pool_cannot_bypass_disabled_policy() {
        // No server is listening: the policy gate must reject before any
        // connection I/O, so the error is the policy error, not an I/O error.
        let mut client = IpcClient::with_pool(
            &unique_address("policy_mismatch"),
            Arc::new(Mutex::new(MemPool::new(c2_mem::PoolConfig::default()))),
            client_config(
                BaseIpcConfig {
                    pool_enabled: false,
                    ..small_base(64 * 1024, 2)
                },
                1024,
            ),
        );
        match client.connect().await {
            Err(IpcError::Pool(msg)) => {
                assert!(msg.contains("pool_enabled"), "unexpected error: {msg}");
                assert!(msg.contains("buddy enabled"), "unexpected error: {msg}");
            }
            other => panic!("expected Pool policy rejection before I/O, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn injected_pool_with_prewarm_config_is_rejected_before_io() {
        let mut client = IpcClient::with_pool(
            &unique_address("prewarm_injected"),
            Arc::new(Mutex::new(MemPool::new(c2_mem::PoolConfig::default()))),
            client_config(
                BaseIpcConfig {
                    pool_prewarm_segments: 1,
                    ..small_base(64 * 1024, 2)
                },
                1024,
            ),
        );
        match client.connect().await {
            Err(IpcError::Pool(msg)) => {
                assert!(msg.contains("pool_prewarm_segments"), "unexpected error: {msg}");
                assert!(msg.contains("injected"), "unexpected error: {msg}");
            }
            other => panic!("expected Pool policy rejection before I/O, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn disabled_injected_pool_still_sends_dedicated_data() {
        let (callback, seen_kinds) = echo_callback();
        let server = start_echo_server(
            "disabled_injected",
            server_config(
                BaseIpcConfig {
                    pool_enabled: false,
                    chunk_size: 1 << 20,
                    ..small_base(64 * 1024, 2)
                },
                1024,
            ),
            callback,
        )
        .await;

        let config = client_config(
            BaseIpcConfig {
                pool_enabled: false,
                chunk_size: 1 << 20,
                ..small_base(64 * 1024, 2)
            },
            1024,
        );
        // A policy-matching externally owned pool: buddy-disabled, built from
        // the same centralized projection the transport itself uses.
        let injected = Arc::new(Mutex::new(MemPool::new(
            config.base.primary_pool_config(&config.pool_tuning()),
        )));
        let mut client =
            IpcClient::with_pool(server.ipc_address(), Arc::clone(&injected), config);
        client.connect().await.unwrap();

        let payload: Vec<u8> = (0..64 * 1024u32).map(|i| (i % 251) as u8).collect();
        let response = echo_roundtrip(&client, "disabled_injected", &payload).await;

        assert_eq!(*seen_kinds.lock(), vec!["shm_dedicated"]);
        let ResponseData::Shm { is_dedicated, .. } = &response else {
            panic!("expected SHM response, got {response:?}");
        };
        assert!(*is_dedicated, "disabled injected pool must serve dedicated SHM");
        assert_eq!(response_bytes(&client, response), payload);

        // The externally owned pool was never mutated: same policy, no buddy
        // mappings, and it is still usable after the client closes.
        {
            let pool = injected.lock();
            assert!(!pool.config().buddy_enabled);
            assert_eq!(pool.segment_count(), 0);
            assert!(pool.stats().dedicated_segments >= 1);
        }
        client.close().await;
        assert_eq!(injected.lock().segment_count(), 0);
        stop_server(&server).await;
    }

    // ── Client maintenance: idle retirement, stale sweeps, liveness ──────

    #[tokio::test]
    async fn client_maintenance_retires_pools_to_zero_and_recreates_with_new_generation() {
        let held: Arc<Mutex<Vec<PoolAllocation>>> = Arc::new(Mutex::new(Vec::new()));
        let seen_kinds = Arc::new(Mutex::new(Vec::new()));
        let callback = Arc::new(PoolHoggingEcho {
            seen_kinds: Arc::clone(&seen_kinds),
            held: Arc::clone(&held),
            hog_count: 12,
            hog_size: 32 * 1024,
        });
        // Fast sweep/decay windows drive the maintenance task deterministically.
        let base = BaseIpcConfig {
            chunk_size: 8 * 1024,
            chunk_gc_interval_secs: 0.05,
            chunk_assembler_timeout_secs: 0.05,
            ..small_base(64 * 1024, 4)
        };
        let server = start_echo_server("client_gc", server_config(base.clone(), 1024), callback)
            .await;

        let mut client = IpcClient::with_config(
            server.ipc_address(),
            fast_client_config(base, 1024),
        );
        client.connect().await.unwrap();
        assert!(
            client.maintenance.lock().is_some(),
            "connected client must own exactly one maintenance task"
        );

        // Plant a stale assembly that will expire: the maintenance sweep must
        // abort it and release its reassembly-pool bytes.
        client
            .chunk_registry
            .insert(42_42, 7, 2, 4096)
            .expect("stale assembly insert");
        assert_eq!(client.chunk_registry.active_count(), 1);

        // One call: buddy request (client pool segment 0, generation 1) and a
        // chunked reply reassembled into the client's reassembly pool.
        let payload: Vec<u8> = (0..32 * 1024u32).map(|i| (i % 241) as u8).collect();
        let response = echo_roundtrip(&client, "client_gc", &payload).await;
        assert_eq!(seen_kinds.lock().len(), 1);
        assert_eq!(&seen_kinds.lock()[0], &"shm_buddy");
        let handle = match response {
            ResponseData::Handle(handle) => handle,
            other => panic!("expected chunked Handle reply, got {other:?}"),
        };
        let reassembly = client.reassembly_pool_arc();
        assert_eq!(reassembly.read().handle_slice(&handle), payload);
        let request_gen = client
            .pool
            .as_ref()
            .unwrap()
            .lock()
            .segment_generation(0);
        assert_eq!(request_gen, Some(1));
        assert_eq!(reassembly.read().segment_generation(0), Some(1));
        reassembly.write().release_handle(handle);

        // Maintenance (not a new allocation) drives both owner pools back to
        // zero mappings and sweeps the expired stale assembly.
        wait_until(5, || {
            own_pool_segment_count(&client) == 0
                && reassembly.read().segment_count() == 0
                && client.chunk_registry.active_count() == 0
        })
        .await;
        assert_eq!(
            client.pool.as_ref().unwrap().lock().segment_generation(0),
            None,
            "retired slot must not report a live generation"
        );

        // A fresh large call lazily recreates both segments with incremented
        // generations while the connection stays open.
        let response = echo_roundtrip(&client, "client_gc", &payload).await;
        let handle = match response {
            ResponseData::Handle(handle) => handle,
            other => panic!("expected chunked Handle reply, got {other:?}"),
        };
        assert_eq!(reassembly.read().handle_slice(&handle), payload);
        reassembly.write().release_handle(handle);
        assert_eq!(
            client.pool.as_ref().unwrap().lock().segment_generation(0),
            Some(2),
            "re-created slot must carry a fresh generation"
        );
        assert_eq!(reassembly.read().segment_generation(0), Some(2));

        // Release the hogged server-side allocations and shut down.
        {
            let response_pool = server.response_pool_arc();
            let mut pool = response_pool.write();
            for alloc in held.lock().drain(..) {
                pool.free(&alloc).unwrap();
            }
        }
        client.close().await;
        stop_server(&server).await;
    }

    #[tokio::test]
    async fn client_maintenance_stops_on_close_and_on_drop() {
        let (callback, _seen) = echo_callback();
        let base = BaseIpcConfig {
            chunk_gc_interval_secs: 0.05,
            ..small_base(64 * 1024, 2)
        };
        let server = start_echo_server("gc_lifecycle", server_config(base.clone(), 4096), callback)
            .await;

        // Explicit close: the task is stopped and removed from the state.
        let mut client = IpcClient::with_config(
            server.ipc_address(),
            fast_client_config(base.clone(), 4096),
        );
        client.connect().await.unwrap();
        let ticks = Arc::clone(&client.maintenance_ticks);
        wait_until(5, || ticks.load(Ordering::Relaxed) >= 2).await;
        client.close().await;
        assert!(client.maintenance.lock().is_none());
        tokio::time::sleep(Duration::from_millis(100)).await; // absorb an in-flight tick
        let after_close = ticks.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            ticks.load(Ordering::Relaxed),
            after_close,
            "maintenance must stop ticking after close()"
        );

        // Plain drop (no close): the weak liveness probe must terminate the
        // task on its next tick even though the recv loop keeps the registry
        // alive until the peer hangs up.
        let client_b = IpcClient::with_config(
            server.ipc_address(),
            fast_client_config(base, 4096),
        );
        let mut client_b = client_b;
        client_b.connect().await.unwrap();
        let ticks_b = Arc::clone(&client_b.maintenance_ticks);
        wait_until(5, || ticks_b.load(Ordering::Relaxed) >= 2).await;
        drop(client_b);
        tokio::time::sleep(Duration::from_millis(100)).await; // absorb an in-flight tick
        let after_drop = ticks_b.load(Ordering::Relaxed);
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            ticks_b.load(Ordering::Relaxed),
            after_drop,
            "maintenance must stop ticking after the client is dropped"
        );

        stop_server(&server).await;
    }

    #[tokio::test]
    async fn maintenance_releases_dedicated_read_done_mappings_without_further_calls() {
        let (callback, seen_kinds) = echo_callback();
        // Dedicated-only connection (buddy disabled) with fast maintenance
        // cadence on both sides.
        let base = BaseIpcConfig {
            pool_enabled: false,
            chunk_size: 1 << 20,
            chunk_gc_interval_secs: 0.05,
            ..small_base(64 * 1024, 2)
        };
        let server = start_echo_server("dedicated_gc", server_config(base.clone(), 1024), callback)
            .await;

        let mut client = IpcClient::with_config(
            server.ipc_address(),
            fast_client_config(base, 1024),
        );
        client.connect().await.unwrap();

        // One round trip: dedicated SHM request and dedicated SHM reply.
        let payload: Vec<u8> = (0..64 * 1024u32).map(|i| (i % 251) as u8).collect();
        let response = echo_roundtrip(&client, "dedicated_gc", &payload).await;
        assert_eq!(*seen_kinds.lock(), vec!["shm_dedicated"]);
        let ResponseData::Shm { is_dedicated, .. } = &response else {
            panic!("expected SHM response, got {response:?}");
        };
        assert!(*is_dedicated);

        // Before the client releases the reply, the server's creator-side
        // dedicated entry deterministically lingers: freed_at is set but
        // read_done is still 0, so no sweep (only the 5 s crash timeout,
        // longer than this whole test) can reclaim it yet.
        let server_pool = server.response_pool_arc();
        assert!(
            server_pool.read().stats().dedicated_segments >= 1,
            "dedicated response entry must exist before maintenance reclaims it"
        );

        // Reading and releasing the reply sets read_done on the backing. From
        // here no further call runs on either pool: only the periodic
        // maintenance sweeps (gc_dedicated) can observe read_done / freed_at
        // and unmap the creator-side entries on the server response pool and
        // the client's own request pool (whose entry survives the in-free
        // sweep because that sweep runs before its freed_at is recorded).
        assert_eq!(response_bytes(&client, response), payload);

        wait_until(5, || {
            server_pool.read().stats().dedicated_segments == 0
                && client
                    .pool
                    .as_ref()
                    .unwrap()
                    .lock()
                    .stats()
                    .dedicated_segments
                    == 0
        })
        .await;

        client.close().await;
        stop_server(&server).await;
    }

    // ── Canonical buddy segment bound: valid index 16 at both peers ──────

    #[tokio::test]
    async fn held_buddy_replies_reach_segment_index_16_on_client_peer_cache() {
        let (callback, _seen) = echo_callback();
        // 20 × 64 KiB server response segments; two 32 KiB buddy replies per
        // segment, so 34 held replies force legitimate use of seg_idx 16.
        let server = start_echo_server(
            "reply_idx16",
            server_config(small_base(64 * 1024, 20), 1024),
            callback,
        )
        .await;

        // Requests stay inline; only the reply path grows the server pool.
        let mut client = IpcClient::with_config(
            server.ipc_address(),
            client_config(small_base(64 * 1024, 20), 1 << 20),
        );
        client.connect().await.unwrap();

        struct HeldReply {
            seg_idx: u16,
            generation: u32,
            offset: u32,
            data_size: u32,
        }

        let mut held: Vec<HeldReply> = Vec::new();
        let mut max_seg_idx = 0u16;
        for i in 0..34u32 {
            let payload: Vec<u8> = (0..32 * 1024u32).map(|j| ((i + j) % 251) as u8).collect();
            let ResponseData::Shm {
                seg_idx,
                generation,
                offset,
                data_size,
                is_dedicated,
            } = echo_roundtrip(&client, "reply_idx16", &payload).await
            else {
                panic!("expected buddy SHM reply for request {i}");
            };
            assert!(!is_dedicated, "buddy-enabled pool must serve buddy replies");
            max_seg_idx = max_seg_idx.max(seg_idx);
            // Read correctness through the lazy client peer cache: this copy
            // exercises seg_idx 16, which a 16-entry cache would reject.
            let copied = {
                let mut server_pool = client.server_pool_arc().lock();
                let state = server_pool.as_mut().unwrap();
                state
                    .copy_response(seg_idx, generation, offset, data_size, false)
                    .unwrap_or_else(|e| panic!("copy of reply {i} failed: {e}"))
            };
            assert_eq!(copied, payload, "reply {i} bytes must round-trip");
            held.push(HeldReply {
                seg_idx,
                generation,
                offset,
                data_size,
            });
        }

        assert_eq!(max_seg_idx, 16, "34 held replies must reach buddy index 16");
        assert_eq!(server.response_pool_arc().read().segment_count(), 17);
        assert_eq!(
            client
                .server_pool_arc()
                .lock()
                .as_ref()
                .unwrap()
                .pool
                .segment_count(),
            17,
            "client peer cache must hold all lazily opened server segments"
        );

        // Free correctness: releasing every held reply frees the server's
        // buddy blocks cross-process.
        {
            let mut server_pool = client.server_pool_arc().lock();
            let state = server_pool.as_mut().unwrap();
            for reply in &held {
                state
                    .release_response(
                        reply.seg_idx,
                        reply.generation,
                        reply.offset,
                        reply.data_size,
                        false,
                    )
                    .unwrap_or_else(|e| panic!("release of reply failed: {e}"));
            }
        }
        wait_until(5, || server.response_pool_arc().read().stats().alloc_count == 0).await;
        drop(held);

        client.close().await;
        stop_server(&server).await;
    }

    #[tokio::test]
    async fn held_buddy_requests_reach_segment_index_16_on_server_peer_cache() {
        let held_requests = Arc::new(Mutex::new(Vec::new()));
        let seen_seg_idx = Arc::new(Mutex::new(Vec::new()));
        let seen_kinds = Arc::new(Mutex::new(Vec::new()));
        let callback = Arc::new(RequestHoldingEcho {
            held: Arc::clone(&held_requests),
            seen_seg_idx: Arc::clone(&seen_seg_idx),
            seen_kinds: Arc::clone(&seen_kinds),
        });
        let server = start_echo_server(
            "request_idx16",
            server_config(small_base(64 * 1024, 20), 1 << 20),
            callback,
        )
        .await;

        let mut client = IpcClient::with_config(
            server.ipc_address(),
            client_config(small_base(64 * 1024, 20), 1024),
        );
        client.connect().await.unwrap();
        let binding = client
            .acquire_route(&expected_contract("request_idx16"))
            .await
            .unwrap();

        // 34 concurrent SHM requests whose blocks the server holds: the
        // client pool grows to 17 segments (2 × 32 KiB per 64 KiB segment),
        // forcing the server's peer cache to lazy-open index 16.
        let client = Arc::new(client);
        let mut tasks = Vec::new();
        for i in 0..34u32 {
            let client = Arc::clone(&client);
            let binding = binding.clone();
            tasks.push(tokio::spawn(async move {
                let payload: Vec<u8> =
                    (0..32 * 1024u32).map(|j| ((i + j) % 241) as u8).collect();
                let response = timeout(
                    Duration::from_secs(10),
                    client.call_bound(&binding, "echo", &payload),
                )
                .await
                .expect("call must not hang")
                .unwrap();
                let bytes = response
                    .into_bytes_with_pool(&client.server_pool_arc().clone(), &client.reassembly_pool_arc())
                    .unwrap();
                assert_eq!(bytes, payload, "request {i} must echo back");
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }

        assert_eq!(
            seen_kinds.lock().iter().filter(|k| **k == "shm_buddy").count(),
            34,
            "every held request must have crossed as buddy SHM"
        );
        assert_eq!(
            seen_seg_idx.lock().iter().copied().max(),
            Some(16),
            "client request pool must reach buddy index 16"
        );
        assert_eq!(own_pool_segment_count(&client), 17);

        // Dropping the held requests frees the client's blocks cross-process.
        held_requests.lock().clear();
        let pool_arc = Arc::clone(client.pool.as_ref().unwrap());
        wait_until(5, || pool_arc.lock().stats().alloc_count == 0).await;

        Arc::clone(&client).close_shared().await;
        drop(client);
        stop_server(&server).await;
    }
}
