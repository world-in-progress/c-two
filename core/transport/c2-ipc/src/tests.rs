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
        };
        let client = IpcClient::with_config("ipc://configured", cfg.clone());
        assert!(client.pool.is_some());
        assert_eq!(client.config.shm_threshold, cfg.shm_threshold);

        let mut disabled = cfg;
        disabled.base.pool_enabled = false;
        let client = IpcClient::with_config("ipc://configured-no-pool", disabled);
        assert!(client.pool.is_none());
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
            SEGMENT_SIZE,
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
            SEGMENT_SIZE,
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
