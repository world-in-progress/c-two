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
        assert!(client.has_request_pool());
        assert_eq!(client.config.shm_threshold, cfg.shm_threshold);

        // A disabled buddy pool is still a real pool: it keeps dedicated SHM
        // requests and the wire prefix alive, only the buddy tiers are off.
        let mut disabled = cfg;
        disabled.base.pool_enabled = false;
        let client = IpcClient::with_config("ipc://configured-no-pool", disabled);
        let pool = client
            .request_pool()
            .expect("policy-disabled pool stays live");
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
        use c2_wire::chunk::ReassemblyBacking;

        let payload = b"reassembled response".repeat(256);
        for logical_len in [payload.len(), 4096, 1, 0] {
            let pool = empty_reassembly_pool('h');
            let mut backing =
                ReassemblyBacking::admit(Arc::clone(&pool), 1, payload.len()).unwrap();
            backing.write_at(0, &payload).unwrap();
            backing.trim_to(logical_len).unwrap();
            let mut lease =
                ResponseLease::new(ResponseData::Handle(backing), Arc::new(Mutex::new(None)));

            assert_eq!(lease.copy_bytes().unwrap(), payload[..logical_len]);
            assert_eq!(pool.read().stats().alloc_count, 1);
            lease.release().unwrap();
            assert_eq!(pool.read().stats().alloc_count, 0);
        }
    }

    #[test]
    fn file_spill_response_lease_preserves_trim_and_release_state() {
        use c2_wire::chunk::ReassemblyBacking;

        let payload = b"file-backed response".repeat(256);
        for logical_len in [payload.len(), 1, 0] {
            let mut config = pool_config();
            config.spill_threshold = 0.0;
            let pool = Arc::new(RwLock::new(MemPool::new_with_prefix(
                config,
                unique_prefix('f'),
            )));
            let mut backing =
                ReassemblyBacking::admit(Arc::clone(&pool), 1, payload.len()).unwrap();
            assert!(backing.is_file_spill());
            backing.write_at(0, &payload).unwrap();
            backing.trim_to(logical_len).unwrap();
            #[cfg(windows)]
            let path = backing.file_spill_path().unwrap();
            let mut lease =
                ResponseLease::new(ResponseData::Handle(backing), Arc::new(Mutex::new(None)));
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
        use c2_wire::chunk::ReassemblyBacking;

        let pool = empty_reassembly_pool('c');
        let mut handle = ReassemblyBacking::admit(Arc::clone(&pool), 1, 16).unwrap();
        handle.write_at(0, b"combined failure").unwrap();
        let lease = ResponseLease::new(ResponseData::Handle(handle), Arc::new(Mutex::new(None)));
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
    use c2_mem::{MemPool, MemoryBudget, PoolAllocation, PoolConfig};
    use c2_server::{
        AccessLevel, ConcurrencyMode, CrmCallback, CrmError, RequestData, RequestLease,
        ResponseMeta, RouteBuildSpec, SchedulerLimits, Server, ServerIpcConfig,
    };
    use parking_lot::{Mutex, RwLock};
    use tokio::time::timeout;

    use crate::client::{
        ClientIpcConfig, DispatchPermitSeam, FrameWriteSeam, IpcClient, IpcError, RequestBlock,
    };
    use crate::pool::ClientPool;
    use crate::response::{ResponseData, ResponseLease};

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
            let bytes = request
                .into_owned_bytes()
                .map_err(CrmError::InternalError)?;
            Ok(ResponseMeta::Inline(bytes))
        }
    }

    fn echo_callback() -> (Arc<Echo>, Arc<Mutex<Vec<&'static str>>>) {
        let seen_kinds = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(Echo {
                seen_kinds: Arc::clone(&seen_kinds),
            }),
            seen_kinds,
        )
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
                    held.push(pool.alloc(self.hog_size).map_err(CrmError::InternalError)?);
                }
            }
            let bytes = request
                .into_owned_bytes()
                .map_err(CrmError::InternalError)?;
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
        server
            .wait_until_ready(Duration::from_secs(5))
            .await
            .unwrap();
        Arc::clone(&server)
    }

    async fn stop_server(server: &Arc<Server>) {
        timeout(
            Duration::from_secs(5),
            server.shutdown_and_wait(Duration::from_secs(5)),
        )
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
            .request_pool()
            .expect("config-owned client pool")
            .lock()
            .segment_count()
    }

    async fn echo_roundtrip(client: &IpcClient, route_name: &str, payload: &[u8]) -> ResponseData {
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
            .into_bytes_with_pool(&client.server_pool_arc().clone())
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
        assert!(client.request_pool().unwrap().lock().config().buddy_enabled);

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
        let _retire_guard = crate::client::dedicated_retire_test_control::production_guard();
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
        assert!(!client.request_pool().unwrap().lock().config().buddy_enabled);
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
        let mut backing = match response {
            ResponseData::Handle(backing) => backing,
            other => panic!("expected chunked Handle response, got {other:?}"),
        };
        let reassembly = Arc::clone(client.require_chunk_registry().pool());
        assert!(!reassembly.read().config().buddy_enabled);
        assert!(backing.is_dedicated());
        assert_eq!(backing.copy_bytes().unwrap(), payload);
        backing.release().unwrap();

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
        assert!(
            !*is_dedicated,
            "buddy-enabled pools must serve buddy replies"
        );
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
    async fn wait_until(secs: u64, cond: impl FnMut() -> bool) {
        wait_until_observed(secs, "condition", cond, || String::new()).await;
    }

    /// Poll every 25 ms until `cond` holds, panicking after `secs` with the
    /// caller's observation of the state that was still wrong.
    ///
    /// A bare "condition not reached within 5s" cannot distinguish "the
    /// fixture never granted the concurrency this scenario asks for" from "the
    /// production path lost a backing, a permit, or a signal". The waits that
    /// carry the cancellation/retirement proof therefore report what they
    /// actually saw instead of only how long they waited.
    async fn wait_until_observed(
        secs: u64,
        what: &str,
        mut cond: impl FnMut() -> bool,
        mut observe: impl FnMut() -> String,
    ) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(secs);
        while !cond() {
            if tokio::time::Instant::now() >= deadline {
                panic!(
                    "condition not reached within {secs}s: {what} | observed {}",
                    observe()
                );
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }

    /// Declared server execution concurrency for the cancellation scenarios.
    ///
    /// Both scenarios hand a request to the server and only cancel the client's
    /// reply wait, so every earlier callback is still holding an execution slot
    /// when the next request arrives. The default
    /// `max_execution_workers` follows `available_parallelism()` (clamped to
    /// `4..=64`), which makes the number of slots a property of the machine:
    /// a 4-vCPU CI runner starves the fifth simultaneous callback. The fixture
    /// requirement is therefore declared here, where the scenario can be read,
    /// instead of being inherited from the host CPU count.
    fn cancellation_server_config(
        base: BaseIpcConfig,
        shm_threshold: u64,
        concurrent_callbacks: usize,
    ) -> ServerIpcConfig {
        ServerIpcConfig {
            max_execution_workers: u32::try_from(concurrent_callbacks)
                .expect("fixture concurrency fits a worker count"),
            ..server_config(base, shm_threshold)
        }
    }

    /// One-line observation of the state the cancellation scenarios wait on.
    ///
    /// Deliberately mixes three different scopes so a timeout is diagnosable:
    /// `callbacks_entered` is the server's proof that the request arrived,
    /// `pending`/`connected` are caller-side cancellation evidence, the
    /// `dedicated_*` fields are pool-local backing evidence, and the retire
    /// counters are process-wide (a parallel test can own permits this scenario
    /// never granted).
    fn cancellation_state(
        label: &str,
        callbacks_entered: usize,
        pending: usize,
        connected: bool,
        pool: &Arc<Mutex<MemPool>>,
        budget: &MemoryBudget,
    ) -> String {
        let stats = pool.lock().stats();
        let (jobs, permits, workers) = crate::client::dedicated_retire_test_control::snapshot();
        format!(
            "{label}: callbacks_entered={callbacks_entered} pending={pending} \
             connected={connected} dedicated_segments={} dedicated_active={} \
             dedicated_pending_free_bytes={} retire_jobs={jobs} retire_permits={permits} \
             retire_workers={workers} shm_used_bytes={}",
            stats.dedicated_segments,
            stats.dedicated_active_count,
            stats.dedicated_pending_free_bytes,
            budget.snapshot().shm.used_bytes,
        )
    }

    /// Retention permits still outstanding when an exclusive retire test
    /// starts.
    ///
    /// The retire executor is process-wide and a test that already finished can
    /// leave a backing retained until its peer reads it or the crash timeout
    /// fires; the exclusive test lock only excludes *concurrent* tests. Exact
    /// assertions therefore size themselves against this observed baseline
    /// instead of assuming a quiescent executor.
    async fn retire_permit_baseline() -> usize {
        for _ in 0..20 {
            if crate::client::dedicated_retire_test_control::snapshot().1 == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        crate::client::dedicated_retire_test_control::snapshot().1
    }

    /// The public release may arrive after the dispatch CAS while its permit
    /// is being installed. It must wait for that installation, then retain the
    /// real backing under the same bounded permit until peer read_done.
    #[test]
    fn dedicated_release_at_dispatch_permit_handoff_keeps_capacity_and_charge() {
        use crate::client::dedicated_retire_test_control as control;
        let _retire_lock = control::exclusive_lock();
        let baseline = control::snapshot().1;
        let _capacity = control::capacity_override(baseline + 1);
        let config = PoolConfig {
            segment_size: 64 * 1024,
            min_block_size: 4096,
            max_segments: 2,
            max_dedicated_segments: 2,
            dedicated_crash_timeout_secs: 5.0,
            spill_threshold: 1.0,
            buddy_enabled: false,
            ..PoolConfig::default()
        };
        let budget = MemoryBudget::new(1 << 20, 1 << 20, 1 << 20);
        let pool = Arc::new(Mutex::new(MemPool::new_with_prefix_and_budget(
            config.clone(),
            format!(
                "/cc3handoff{:08x}{:08x}",
                std::process::id(),
                ADDR_COUNTER.fetch_add(1, Ordering::Relaxed)
            ),
            budget.clone(),
        )));
        let alloc = pool.lock().alloc(4096).expect("real dedicated allocation");
        assert!(alloc.is_dedicated);
        let prefix = pool.lock().prefix().to_string();
        let name = pool
            .lock()
            .dedicated_name(alloc.seg_idx)
            .expect("dedicated backing name")
            .to_string();
        let mut peer = MemPool::open_peer(config, prefix);
        peer.open_dedicated_at(alloc.seg_idx, &name, 4096)
            .expect("peer opens the request backing");
        let charged = budget.snapshot().shm.used_bytes;
        assert!(charged > 0);

        let block = Arc::new(RequestBlock::new(Arc::clone(&pool), alloc));
        let (permit, returned) = control::reserve_permit();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (resume_tx, resume_rx) = std::sync::mpsc::channel();
        block.set_dispatch_seam_for_test(DispatchPermitSeam {
            entered: entered_tx,
            resume: resume_rx,
        });
        let dispatch_block = Arc::clone(&block);
        let dispatch = std::thread::spawn(move || dispatch_block.try_dispatch(Some(permit)));
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("dispatch crossed CAS before permit install");

        let release_block = Arc::clone(&block);
        let (attempt_tx, attempt_rx) = std::sync::mpsc::channel();
        let (released_tx, released_rx) = std::sync::mpsc::channel();
        let release = std::thread::spawn(move || {
            attempt_tx.send(()).unwrap();
            let result = release_block.release();
            released_tx.send(()).unwrap();
            result
        });
        attempt_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("public release entered the race");
        let premature_release = released_rx.recv_timeout(Duration::from_millis(100));
        resume_tx.send(()).unwrap();
        let request = dispatch.join().unwrap().expect("dispatch succeeds once");
        release
            .join()
            .unwrap()
            .expect("public release settles once");
        assert!(
            matches!(
                premature_release,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ),
            "release must wait until dispatch has installed its permit"
        );
        drop(request);
        drop(block);

        assert!(!returned.load(Ordering::Acquire));
        let outstanding = control::snapshot().1;
        assert!(outstanding > 0, "retained backing has a counted permit");
        control::set_capacity_override(outstanding);
        assert!(
            !control::can_reserve(),
            "one retained backing fills capacity"
        );
        assert!(pool.lock().dedicated_awaiting_retirement(&alloc));
        assert_eq!(budget.snapshot().shm.used_bytes, charged);

        peer.free_at(alloc.seg_idx, alloc.generation, alloc.offset, 4096, true)
            .expect("peer signals read_done");
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !returned.load(Ordering::Acquire) || budget.snapshot().shm.used_bytes != 0 {
            assert!(std::time::Instant::now() < deadline, "retirement timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
        control::set_capacity_override(0);
        assert!(control::can_reserve(), "retirement returns the permit");
    }

    /// A single retained job still counts against its exact pool after a worker
    /// takes it off the shared queue and until the peer's `read_done` returns
    /// its permit.
    ///
    /// Host-review regression: `pop_front` removes the job from `state.jobs`
    /// before `dedicated_retire_backing` runs, so a queue-only scoped witness
    /// observed zero during that window and a "slots returned" wait could pass
    /// while the backing was still retained unread. The in-flight window is
    /// pinned deterministically here: the job is proven taken through the
    /// executor's own popped-job observation, and the backing cannot retire
    /// because the peer has not read it and the crash timeout is 30 s away.
    #[test]
    fn pool_scoped_retention_slot_stays_visible_while_the_job_is_in_flight() {
        use crate::client::dedicated_retire_test_control as control;
        let _retire_lock = control::exclusive_lock();
        let config = PoolConfig {
            segment_size: 64 * 1024,
            min_block_size: 4096,
            max_segments: 2,
            max_dedicated_segments: 2,
            // Far beyond every wait bound in this test: without the peer's
            // read_done the backing stays retained, which is what makes the
            // in-flight assertion deterministic instead of a race with
            // retirement.
            dedicated_crash_timeout_secs: 30.0,
            spill_threshold: 1.0,
            buddy_enabled: false,
            ..PoolConfig::default()
        };
        let budget = MemoryBudget::new(1 << 20, 1 << 20, 1 << 20);
        let pool = Arc::new(Mutex::new(MemPool::new_with_prefix_and_budget(
            config.clone(),
            format!(
                "/cc3inflight{:08x}{:08x}",
                std::process::id(),
                ADDR_COUNTER.fetch_add(1, Ordering::Relaxed)
            ),
            budget.clone(),
        )));
        let alloc = pool.lock().alloc(4096).expect("real dedicated allocation");
        assert!(alloc.is_dedicated);
        let prefix = pool.lock().prefix().to_string();
        let name = pool
            .lock()
            .dedicated_name(alloc.seg_idx)
            .expect("dedicated backing name")
            .to_string();
        let mut peer = MemPool::open_peer(config, prefix);
        peer.open_dedicated_at(alloc.seg_idx, &name, 4096)
            .expect("peer opens the request backing");
        let charged = budget.snapshot().shm.used_bytes;
        assert!(charged > 0, "the retained backing must be charged");

        // Hand one already-freed backing to the shared executor through the
        // production queue path. `returned` flips inside `permit.release()`.
        let returned = control::submit_retention_for_test(Arc::clone(&pool), alloc);

        // Deterministic "taken by the worker": a non-zero popped-job count
        // proves the job left the queue, and retirement is impossible before
        // read_done.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while control::in_flight_jobs_for_pool(&pool) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the shared retire worker must take the single queued job"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            control::retention_jobs_for_pool(&pool),
            1,
            "a popped, unread retained backing must still count against its pool"
        );
        assert!(pool.lock().dedicated_awaiting_retirement(&alloc));
        assert_eq!(budget.snapshot().shm.used_bytes, charged);
        assert!(!returned.load(Ordering::Acquire));

        peer.free_at(alloc.seg_idx, alloc.generation, alloc.offset, 4096, true)
            .expect("peer signals read_done");
        // Sample the returned flag and scoped count under the executor lock.
        // Independent reads allow the worker to return the permit and clear
        // the count between them, producing a false failure. If this snapshot
        // sees false + zero, the count really vanished before permit return.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            let (retained, permit_returned) =
                control::retention_and_returned_for_pool(&pool, &returned);
            if permit_returned {
                break;
            }
            assert!(
                retained >= 1,
                "the scoped count must stay non-zero until the permit returns"
            );
            assert!(std::time::Instant::now() < deadline, "retirement timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while control::retention_jobs_for_pool(&pool) != 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the scoped count must return to zero once the permit is back"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(control::in_flight_jobs_for_pool(&pool), 0);
        assert!(!pool.lock().dedicated_awaiting_retirement(&alloc));
        assert_eq!(
            budget.snapshot().shm.used_bytes,
            0,
            "the retired backing's charge must return"
        );
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
                    held.push(pool.alloc(self.hog_size).map_err(CrmError::InternalError)?);
                }
            }
            let bytes = request
                .into_owned_bytes()
                .map_err(CrmError::InternalError)?;
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
                assert!(
                    msg.contains("pool_prewarm_segments"),
                    "unexpected error: {msg}"
                );
                assert!(msg.contains("injected"), "unexpected error: {msg}");
            }
            other => panic!("expected Pool policy rejection before I/O, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn disabled_injected_pool_still_sends_dedicated_data() {
        let _retire_guard = crate::client::dedicated_retire_test_control::production_guard();
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
        let mut client = IpcClient::with_pool(server.ipc_address(), Arc::clone(&injected), config);
        client.connect().await.unwrap();

        let payload: Vec<u8> = (0..64 * 1024u32).map(|i| (i % 251) as u8).collect();
        let response = echo_roundtrip(&client, "disabled_injected", &payload).await;

        assert_eq!(*seen_kinds.lock(), vec!["shm_dedicated"]);
        let ResponseData::Shm { is_dedicated, .. } = &response else {
            panic!("expected SHM response, got {response:?}");
        };
        assert!(
            *is_dedicated,
            "disabled injected pool must serve dedicated SHM"
        );
        assert_eq!(response_bytes(&client, response), payload);

        // The externally owned pool was never mutated: same policy, no buddy
        // mappings, and the settled dedicated request retired promptly — the
        // response proves the peer set `read_done`, so the owner entry is
        // already reclaimed here rather than lingering until a later GC.
        {
            let pool = injected.lock();
            assert!(!pool.config().buddy_enabled);
            assert_eq!(pool.segment_count(), 0);
            assert_eq!(pool.stats().dedicated_segments, 0);
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
        let server =
            start_echo_server("client_gc", server_config(base.clone(), 1024), callback).await;

        let mut client =
            IpcClient::with_config(server.ipc_address(), fast_client_config(base, 1024));
        client.connect().await.unwrap();
        assert!(
            client.maintenance.lock().is_some(),
            "connected client must own exactly one maintenance task"
        );

        // Plant a stale assembly that will expire: the maintenance sweep must
        // abort it and release its reassembly-pool bytes.
        client
            .require_chunk_registry()
            .insert(42_42, 7, 2, 4096)
            .expect("stale assembly insert");
        assert_eq!(client.require_chunk_registry().active_count(), 1);

        // One call: buddy request (client pool segment 0, generation 1) and a
        // chunked reply reassembled into the client's reassembly pool.
        let payload: Vec<u8> = (0..32 * 1024u32).map(|i| (i % 241) as u8).collect();
        let response = echo_roundtrip(&client, "client_gc", &payload).await;
        assert_eq!(seen_kinds.lock().len(), 1);
        assert_eq!(&seen_kinds.lock()[0], &"shm_buddy");
        let mut backing = match response {
            ResponseData::Handle(backing) => backing,
            other => panic!("expected chunked Handle reply, got {other:?}"),
        };
        let reassembly = Arc::clone(client.require_chunk_registry().pool());
        assert_eq!(backing.copy_bytes().unwrap(), payload);
        let request_gen = client.request_pool().unwrap().lock().segment_generation(0);
        assert_eq!(request_gen, Some(1));
        assert_eq!(reassembly.read().segment_generation(0), Some(1));
        backing.release().unwrap();

        // Maintenance (not a new allocation) drives both owner pools back to
        // zero mappings and sweeps the expired stale assembly.
        wait_until(5, || {
            own_pool_segment_count(&client) == 0
                && reassembly.read().segment_count() == 0
                && client.require_chunk_registry().active_count() == 0
        })
        .await;
        assert_eq!(
            client.request_pool().unwrap().lock().segment_generation(0),
            None,
            "retired slot must not report a live generation"
        );

        // A fresh large call lazily recreates both segments with incremented
        // generations while the connection stays open.
        let response = echo_roundtrip(&client, "client_gc", &payload).await;
        let mut backing = match response {
            ResponseData::Handle(backing) => backing,
            other => panic!("expected chunked Handle reply, got {other:?}"),
        };
        assert_eq!(backing.copy_bytes().unwrap(), payload);
        backing.release().unwrap();
        assert_eq!(
            client.request_pool().unwrap().lock().segment_generation(0),
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
        let server =
            start_echo_server("gc_lifecycle", server_config(base.clone(), 4096), callback).await;

        // Explicit close: the task is stopped and removed from the state.
        let mut client =
            IpcClient::with_config(server.ipc_address(), fast_client_config(base.clone(), 4096));
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
        let client_b = IpcClient::with_config(server.ipc_address(), fast_client_config(base, 4096));
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
        let _retire_guard = crate::client::dedicated_retire_test_control::production_guard();
        let (callback, seen_kinds) = echo_callback();
        // Dedicated-only connection (buddy disabled) with fast maintenance
        // cadence on both sides.
        let base = BaseIpcConfig {
            pool_enabled: false,
            chunk_size: 1 << 20,
            chunk_gc_interval_secs: 0.05,
            ..small_base(64 * 1024, 2)
        };
        let server =
            start_echo_server("dedicated_gc", server_config(base.clone(), 1024), callback).await;

        let mut client =
            IpcClient::with_config(server.ipc_address(), fast_client_config(base, 1024));
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
                    .request_pool()
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
        wait_until(5, || {
            server.response_pool_arc().read().stats().alloc_count == 0
        })
        .await;
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
                let payload: Vec<u8> = (0..32 * 1024u32).map(|j| ((i + j) % 241) as u8).collect();
                let response = timeout(
                    Duration::from_secs(10),
                    client.call_bound(&binding, "echo", &payload),
                )
                .await
                .expect("call must not hang")
                .unwrap();
                let bytes = response
                    .into_bytes_with_pool(&client.server_pool_arc().clone())
                    .unwrap();
                assert_eq!(bytes, payload, "request {i} must echo back");
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }

        assert_eq!(
            seen_kinds
                .lock()
                .iter()
                .filter(|k| **k == "shm_buddy")
                .count(),
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
        let pool_arc = client.request_pool().unwrap();
        wait_until(5, || pool_arc.lock().stats().alloc_count == 0).await;

        Arc::clone(&client).close_shared().await;
        drop(client);
        stop_server(&server).await;
    }

    // ── Shared transport memory context and read-only snapshots ──────────

    static SEGMENT_PROBE_GEN: AtomicU64 = AtomicU64::new(0);

    /// Measure the exact mapped cost of one buddy segment for `cfg` so a test
    /// can cap a shared domain at exactly one segment without hard-coding the
    /// allocator's header/bitmap layout.
    fn measured_one_segment_charge(cfg: &ClientIpcConfig) -> u64 {
        let probed = c2_mem::MemoryBudget::new(u64::MAX, u64::MAX, u64::MAX);
        let prefix = format!(
            "/cc3p{:08x}{:08x}",
            std::process::id(),
            SEGMENT_PROBE_GEN.fetch_add(1, Ordering::Relaxed)
        );
        let mut pool = MemPool::new_with_prefix_and_budget(
            cfg.base.primary_pool_config(&cfg.pool_tuning()),
            prefix,
            probed.clone(),
        );
        pool.ensure_buddy_segments(1).expect("probe segment");
        let charge = probed.snapshot().shm.used_bytes;
        assert!(charge > 0, "a mapped buddy segment must be charged");
        charge
    }

    /// Route-bound echo call through a pooled (synchronous) client, returning
    /// owned bytes after the response lease releases its exact pool.
    fn sync_echo_roundtrip(
        client: &crate::SyncClient,
        route_name: &str,
        payload: &[u8],
    ) -> Vec<u8> {
        let binding = client
            .acquire_route(&expected_contract(route_name))
            .unwrap();
        let response = client
            .call_bound_phased(&binding, "echo", payload)
            .expect("sync echo call");
        client
            .lease_response(response)
            .into_owned_bytes()
            .expect("materialize echoed response")
    }

    /// A standalone client shares one budget between its request pool and its
    /// reassembly pool: a request charge and a chunk-reassembly charge land in
    /// the same accounting context.
    #[tokio::test]
    async fn standalone_client_request_and_reassembly_share_one_budget() {
        let (callback, _seen) = echo_callback();
        let base = small_base(64 * 1024, 4);
        // Deny every server-side backing tier so the echo reply must use the
        // chunked path; the client's reassembly pool then really allocates.
        let mut server_cfg = server_config(base.clone(), 1024);
        server_cfg.base.shm_backing_budget_bytes = 0;
        server_cfg.base.file_backing_budget_bytes = 0;
        server_cfg.base.chunk_size = 8 * 1024;
        let server = start_echo_server("budget_share_standalone", server_cfg, callback).await;

        let mut client = IpcClient::with_config(server.ipc_address(), client_config(base, 1024));
        client.connect().await.unwrap();
        let request_pool = client.request_pool().unwrap();
        let reassembly_pool = Arc::clone(client.require_chunk_registry().pool());
        assert_eq!(
            request_pool.lock().budget().unwrap().snapshot(),
            reassembly_pool.read().budget().unwrap().snapshot(),
            "request and reassembly pools must charge one context"
        );

        let payload: Vec<u8> = (0..32 * 1024u32).map(|i| (i % 251) as u8).collect();
        let response = echo_roundtrip(&client, "budget_share_standalone", &payload).await;
        assert_eq!(response_bytes(&client, response), payload);
        let charged = request_pool.lock().budget().unwrap().snapshot();
        assert!(
            charged.shm.used_bytes > 0,
            "buddy request backing must be charged to the shared context"
        );
        assert_eq!(
            charged,
            reassembly_pool.read().budget().unwrap().snapshot(),
            "reassembly work must be visible in the same context"
        );

        client.close().await;
        stop_server(&server).await;
    }

    /// Two distinct outgoing connections in one client cache charge one
    /// finite budget and cannot double its cap: the first connection maps the
    /// single admitted segment, the second is denied and falls back to the
    /// checked chunked path while the shared accounting stays at one segment.
    ///
    /// This is a synchronous test: the pooled client embeds a blocking
    /// runtime handle, so its calls must not run on a `#[tokio::test]` worker
    /// thread. The servers run on a separate local runtime instead.
    #[test]
    fn cached_connections_share_one_finite_budget_and_cannot_double_the_cap() {
        let server_rt = tokio::runtime::Runtime::new().expect("server runtime");
        let (callback, _seen) = echo_callback();
        let callback: Arc<dyn CrmCallback> = callback;
        let base = small_base(64 * 1024, 4);
        let server_a = server_rt.block_on(start_echo_server(
            "budget_two_a",
            server_config(base.clone(), 1024),
            Arc::clone(&callback),
        ));
        let server_b = server_rt.block_on(start_echo_server(
            "budget_two_b",
            server_config(base.clone(), 1024),
            callback,
        ));

        let mut cfg = client_config(base, 1024);
        cfg.base.shm_backing_budget_bytes = measured_one_segment_charge(&cfg);
        cfg.base.file_backing_budget_bytes = 0;

        let cache = Arc::new(ClientPool::new(Duration::from_secs(30)));
        let client_a = cache.acquire(server_a.ipc_address(), Some(&cfg)).unwrap();
        let client_b = cache.acquire(server_b.ipc_address(), Some(&cfg)).unwrap();
        let pool_a = client_a.request_pool().unwrap();
        let pool_b = client_b.request_pool().unwrap();
        assert_eq!(
            pool_a.lock().budget().unwrap().snapshot(),
            pool_b.lock().budget().unwrap().snapshot(),
            "both cached connections must charge the cache's one domain context"
        );
        assert_eq!(pool_a.lock().segment_count(), 0);
        assert_eq!(pool_b.lock().segment_count(), 0);

        // Connection A consumes the one-segment cap with a real allocation.
        let payload: Vec<u8> = (0..32 * 1024u32).map(|i| (i % 241) as u8).collect();
        assert_eq!(
            sync_echo_roundtrip(&client_a, "budget_two_a", &payload),
            payload
        );
        assert_eq!(pool_a.lock().segment_count(), 1);
        let after_a = pool_a.lock().budget().unwrap().snapshot();
        assert!(after_a.shm.used_bytes > 0);

        // Connection B cannot map a second segment under the same cap. Its
        // buddy attempt is rejected, the call succeeds through the checked
        // chunked fallback, and no additional shm backing is admitted.
        assert_eq!(
            sync_echo_roundtrip(&client_b, "budget_two_b", &payload),
            payload
        );
        assert_eq!(
            pool_b.lock().segment_count(),
            0,
            "a second connection must not map backing beyond the shared cap"
        );
        let after_b = pool_b.lock().budget().unwrap().snapshot();
        assert_eq!(
            after_b.shm.used_bytes, after_a.shm.used_bytes,
            "two connections must not double the shared cap"
        );
        assert!(
            after_b.shm.rejected_allocations > after_a.shm.rejected_allocations,
            "the denied second connection must be accounted as a rejected reservation"
        );
        assert!(
            after_b.shm.used_bytes <= cfg.base.shm_backing_budget_bytes,
            "shared usage must never exceed the configured cap"
        );
        assert_eq!(
            cache.memory_budget_snapshot().unwrap().budget,
            after_b,
            "the cache snapshot must report the same accounting"
        );

        cache.close_all(Duration::from_secs(5));
        server_rt.block_on(stop_server(&server_a));
        server_rt.block_on(stop_server(&server_b));
    }

    /// An injected owner pool whose budget limits diverge from the config is
    /// rejected before any connection I/O, and the pool is not mutated.
    #[tokio::test]
    async fn injected_pool_with_divergent_budget_is_rejected_before_io() {
        let cfg = client_config(small_base(64 * 1024, 4), 1024);
        let injected_budget = c2_mem::MemoryBudget::new(1, 2, 3);
        let pool = Arc::new(Mutex::new(MemPool::new_with_prefix_and_budget(
            cfg.base.primary_pool_config(&cfg.pool_tuning()),
            "/cc3i00000000000001".to_string(),
            injected_budget.clone(),
        )));
        let mut client = IpcClient::with_pool(
            "ipc://budget_mismatch_never_connects",
            Arc::clone(&pool),
            cfg,
        );
        let error = client
            .connect()
            .await
            .expect_err("divergent budget limits must reject before I/O");
        assert!(matches!(error, IpcError::Pool(_)), "unexpected: {error:?}");
        assert!(error.to_string().contains("do not match"), "{error}");
        assert_eq!(
            injected_budget.snapshot().shm.limit_bytes,
            1,
            "rejection must not mutate the injected budget"
        );
    }

    /// An injected peer pool carries no owner-creation budget: it is rejected
    /// instead of silently pairing the client with a freshly invented context.
    #[tokio::test]
    async fn injected_peer_pool_without_budget_is_rejected_before_io() {
        let cfg = client_config(small_base(64 * 1024, 4), 1024);
        let peer = MemPool::open_peer(
            c2_mem::config::PoolConfig::default(),
            "/cc3peer00000000001".to_string(),
        );
        let mut client = IpcClient::with_pool(
            "ipc://peer_pool_without_budget_never_connects",
            Arc::new(Mutex::new(peer)),
            cfg,
        );
        let error = client
            .connect()
            .await
            .expect_err("a peer pool without an owner budget must reject");
        assert!(matches!(error, IpcError::Pool(_)), "unexpected: {error:?}");
        assert!(
            error
                .to_string()
                .contains("no owner-creation memory budget"),
            "{error}"
        );
    }

    /// Zero SHM budget still serves inline replies, and chunk/file reassembly
    /// remains available when the file cell admits it.
    #[tokio::test]
    async fn zero_shm_budget_still_serves_inline_and_file_chunk_reassembly() {
        let (callback, _seen) = echo_callback();
        let base = small_base(64 * 1024, 4);
        let mut server_cfg = server_config(base.clone(), 1024);
        server_cfg.base.shm_backing_budget_bytes = 0;
        server_cfg.base.file_backing_budget_bytes = 0;
        server_cfg.base.chunk_size = 8 * 1024;
        let server = start_echo_server("budget_zero_shm", server_cfg, callback).await;

        let mut cfg = client_config(base, 1024);
        cfg.base.shm_backing_budget_bytes = 0;
        cfg.base.file_backing_budget_bytes = 2 * 1024 * 1024 * 1024;
        let mut client = IpcClient::with_config(server.ipc_address(), cfg);
        client.connect().await.unwrap();
        let client = Arc::new(client);

        // Inline path: no owner backing is needed at all.
        let response = echo_roundtrip(&client, "budget_zero_shm", b"inline").await;
        assert_eq!(response_bytes(&client, response), b"inline");

        // Chunked path: buddy/dedicated are denied by the zero shm budget, so
        // reassembly must spill to the admitted file tier. Keep the exact
        // carrier across a confirmed close while the client Arc remains live.
        let payload: Vec<u8> = (0..32 * 1024u32).map(|i| (i % 239) as u8).collect();
        let response = echo_roundtrip(&client, "budget_zero_shm", &payload).await;
        let ResponseData::Handle(backing) = &response else {
            panic!("expected file-backed chunked response, got {response:?}");
        };
        assert!(backing.is_file_spill());
        let budget = client
            .require_chunk_registry()
            .pool()
            .read()
            .budget()
            .unwrap()
            .clone();
        let lease = ResponseLease::new(response, Arc::clone(client.server_pool_arc()));
        assert_eq!(lease.copy_bytes().unwrap(), payload);
        let held_budget = budget.snapshot();
        assert_eq!(
            held_budget.shm.used_bytes, 0,
            "zero shm budget admits no mapping"
        );
        assert!(
            held_budget.file.used_bytes > 0,
            "file-backed reassembly must be charged to the file cell"
        );
        assert!(held_budget.reassembly.used_bytes > 0);

        assert!(
            client.close_shared_bounded(Duration::from_secs(5)).await,
            "the client close must confirm before owner detachment"
        );
        assert_eq!(lease.copy_bytes().unwrap(), payload);
        assert!(budget.snapshot().file.used_bytes > 0);
        assert!(budget.snapshot().reassembly.used_bytes > 0);
        drop(lease);
        assert_eq!(budget.snapshot().file.used_bytes, 0);
        assert_eq!(budget.snapshot().reassembly.used_bytes, 0);

        drop(client);
        stop_server(&server).await;
    }

    /// Exhausting the eligible client budgets produces a correlated failure
    /// for the affected call while ping and a bounded close stay usable.
    #[tokio::test]
    async fn exhausted_client_budgets_fail_correlated_while_ping_and_close_stay_usable() {
        let (callback, _seen) = echo_callback();
        let base = small_base(64 * 1024, 4);
        let mut server_cfg = server_config(base.clone(), 1024);
        server_cfg.base.shm_backing_budget_bytes = 0;
        server_cfg.base.file_backing_budget_bytes = 0;
        server_cfg.base.chunk_size = 8 * 1024;
        let server = start_echo_server("budget_exhausted", server_cfg, callback).await;

        let mut cfg = client_config(base, 1024);
        cfg.base.pool_enabled = false;
        cfg.base.shm_backing_budget_bytes = 0;
        cfg.base.file_backing_budget_bytes = 0;
        let mut client = IpcClient::with_config(server.ipc_address(), cfg);
        client.connect().await.unwrap();
        let binding = client
            .acquire_route(&expected_contract("budget_exhausted"))
            .await
            .unwrap();

        let payload: Vec<u8> = (0..32 * 1024u32).map(|i| (i % 233) as u8).collect();
        let error = timeout(
            Duration::from_secs(10),
            client.call_bound(&binding, "echo", &payload),
        )
        .await
        .expect("an exhausted budget must complete the call, not hang it")
        .expect_err("chunked reception must fail when no budget cell admits it");
        let message = error.to_string();
        assert!(
            message.contains("budget") && message.contains("rejected"),
            "failure must carry the budget rejection: {message}"
        );

        // Control traffic is unaffected by data-budget exhaustion. The probe
        // runs on a blocking worker: a `#[tokio::test]` runtime is
        // single-threaded, so blocking its only worker would starve the
        // server's accept loop this probe needs.
        let address = server.ipc_address().to_string();
        let ping_ok =
            tokio::task::spawn_blocking(move || crate::ping(&address, Duration::from_secs(5)))
                .await
                .expect("ping worker must not panic")
                .expect("ping probe must not fail");
        assert!(
            ping_ok,
            "ping must stay usable after data-budget exhaustion"
        );
        assert!(client.close_shared_bounded(Duration::from_secs(5)).await);
        stop_server(&server).await;
    }

    /// Independent client caches keep independent domains: they can carry
    /// distinct limits, and draining one leaves the other's connection
    /// callable on its own untouched budget.
    #[test]
    fn independent_client_caches_keep_distinct_domains_after_one_drains() {
        let server_rt = tokio::runtime::Runtime::new().expect("server runtime");
        let (callback_a, _seen_a) = echo_callback();
        let (callback_b, _seen_b) = echo_callback();
        let base = small_base(64 * 1024, 4);
        let server_a = server_rt.block_on(start_echo_server(
            "budget_independent_a",
            server_config(base.clone(), 1024),
            callback_a,
        ));
        let server_b = server_rt.block_on(start_echo_server(
            "budget_independent_b",
            server_config(base.clone(), 1024),
            callback_b,
        ));

        let mut cfg_a = client_config(base.clone(), 1024);
        cfg_a.base.shm_backing_budget_bytes = 1_234_567;
        let cfg_b = client_config(base, 1024);
        let default_shm = c2_config::BaseIpcConfig::default().shm_backing_budget_bytes;

        let cache_a = Arc::new(ClientPool::new(Duration::from_secs(30)));
        let cache_b = Arc::new(ClientPool::new(Duration::from_secs(30)));
        let client_a = cache_a
            .acquire(server_a.ipc_address(), Some(&cfg_a))
            .unwrap();
        let client_b = cache_b
            .acquire(server_b.ipc_address(), Some(&cfg_b))
            .unwrap();
        assert_eq!(
            cache_a
                .memory_budget_snapshot()
                .unwrap()
                .limits
                .shm_backing_budget_bytes,
            1_234_567
        );
        assert_eq!(
            cache_b
                .memory_budget_snapshot()
                .unwrap()
                .limits
                .shm_backing_budget_bytes,
            default_shm
        );

        let payload: Vec<u8> = (0..32 * 1024u32).map(|i| (i % 223) as u8).collect();
        assert_eq!(
            sync_echo_roundtrip(&client_a, "budget_independent_a", &payload),
            payload
        );
        assert_eq!(
            sync_echo_roundtrip(&client_b, "budget_independent_b", &payload),
            payload
        );

        // Draining cache A must not reset or close cache B's domain.
        assert!(
            cache_a
                .close_all(Duration::from_secs(5))
                .unconfirmed
                .is_empty()
        );
        assert_eq!(
            sync_echo_roundtrip(&client_b, "budget_independent_b", &payload),
            payload
        );
        assert_eq!(
            cache_b
                .memory_budget_snapshot()
                .unwrap()
                .limits
                .shm_backing_budget_bytes,
            default_shm
        );

        drop(client_a);
        drop(client_b);
        server_rt.block_on(stop_server(&server_a));
        server_rt.block_on(stop_server(&server_b));
    }

    /// A chunked reply retained by a response lease keeps its reassembly
    /// charge visible across a cache drain — shutdown must never reset usage —
    /// and the charge returns once every owner of the backing is gone.
    /// This is a synchronous test for the same runtime-handle reason as
    /// [`cached_connections_share_one_finite_budget_and_cannot_double_the_cap`].
    #[test]
    fn held_chunked_charge_survives_cache_drain_and_returns_after_release() {
        let server_rt = tokio::runtime::Runtime::new().expect("server runtime");
        let (callback, _seen) = echo_callback();
        let base = small_base(64 * 1024, 4);
        let mut server_cfg = server_config(base.clone(), 1024);
        server_cfg.base.shm_backing_budget_bytes = 0;
        server_cfg.base.file_backing_budget_bytes = 0;
        server_cfg.base.chunk_size = 8 * 1024;
        let server = server_rt.block_on(start_echo_server(
            "budget_held_charge",
            server_cfg,
            callback,
        ));

        let cfg = client_config(base, 1024);
        let cache = Arc::new(ClientPool::new(Duration::from_secs(30)));
        let client = cache.acquire(server.ipc_address(), Some(&cfg)).unwrap();
        let binding = client
            .acquire_route(&expected_contract("budget_held_charge"))
            .unwrap();
        let payload: Vec<u8> = (0..32 * 1024u32).map(|i| (i % 227) as u8).collect();
        let response = client
            .call_bound_phased(&binding, "echo", &payload)
            .expect("chunked echo call");
        let lease = client.lease_response(response);
        let held = cache.memory_budget_snapshot().unwrap().budget;
        assert!(
            held.shm.used_bytes > 0,
            "retained chunked backing must be charged"
        );
        assert!(
            held.reassembly.used_bytes > 0,
            "the carrier must retain the full reassembly reservation"
        );

        // Draining the cache closes the connection but must not reset the
        // held charge or free the retained backing. The idle request pool is
        // detached by the confirmed close, so the remaining charge is the
        // reassembly backing retained by the lease.
        let report = cache.close_all(Duration::from_secs(5));
        assert!(report.unconfirmed.is_empty(), "{report:?}");
        assert!(
            client.request_pool().is_none(),
            "the idle request pool must be detached by the drain"
        );
        assert!(
            cache
                .memory_budget_snapshot()
                .unwrap()
                .budget
                .shm
                .used_bytes
                > 0,
            "shutdown must not reset charges retained by held data"
        );
        assert!(
            cache
                .memory_budget_snapshot()
                .unwrap()
                .budget
                .reassembly
                .used_bytes
                > 0,
            "the held reassembly reservation must survive the drain"
        );
        assert_eq!(
            lease.copy_bytes().unwrap(),
            payload,
            "held data must stay valid across the drain"
        );

        // Releasing the carrier returns both charges while the application
        // still holds its closed client Arc. The new lazy registry must never
        // be used to free coordinates from the old backing.
        drop(lease);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline && {
            let budget = cache.memory_budget_snapshot().unwrap().budget;
            budget.shm.used_bytes != 0 || budget.reassembly.used_bytes != 0
        } {
            std::thread::sleep(Duration::from_millis(20));
        }
        let released = cache.memory_budget_snapshot().unwrap().budget;
        assert_eq!(released.shm.used_bytes, 0, "old backing must be reclaimed");
        assert_eq!(
            released.reassembly.used_bytes, 0,
            "released carrier must return its full reservation"
        );
        drop(binding);
        drop(client);
        server_rt.block_on(stop_server(&server));
    }

    /// A confirmed close detaches idle owner pools so a closed-but-retained
    /// client stops pinning unused mappings and their charge; a reconnect
    /// rebuilds fresh incarnations on the same frozen budget.
    #[tokio::test]
    async fn confirmed_close_detaches_idle_owner_pools_and_reconnect_rebuilds() {
        let (callback, _seen) = echo_callback();
        let base = small_base(64 * 1024, 4);
        let server = start_echo_server(
            "budget_detach_reconnect",
            server_config(base.clone(), 1024),
            callback,
        )
        .await;

        let cfg = client_config(base, 1024);
        let budget = c2_mem::MemoryBudget::from_limits(&cfg.memory_budget_limits());
        let mut client = IpcClient::with_shared_budget(server.ipc_address(), cfg, budget.clone());
        client.connect().await.unwrap();
        let first_prefix = client
            .request_pool()
            .expect("connected request pool")
            .lock()
            .prefix()
            .to_string();
        let first_registry = client
            .chunk_registry_arc()
            .expect("connected reassembly registry");

        let payload: Vec<u8> = (0..32 * 1024u32).map(|i| (i % 229) as u8).collect();
        let response = echo_roundtrip(&client, "budget_detach_reconnect", &payload).await;
        assert_eq!(response_bytes(&client, response), payload);
        assert!(
            budget.snapshot().shm.used_bytes > 0,
            "real request backing must be charged before close"
        );

        client.close().await;
        assert!(
            client.request_pool().is_none(),
            "a confirmed close must detach the idle request pool"
        );
        let fresh_registry = client
            .chunk_registry_arc()
            .expect("a fresh lazy registry incarnation remains");
        assert!(
            !Arc::ptr_eq(&fresh_registry, &first_registry),
            "a confirmed close must detach the idle reassembly registry"
        );
        assert_eq!(
            budget.snapshot().shm.used_bytes,
            0,
            "detaching an idle pool must return its backing charge"
        );

        // Reconnect rebuilds fresh pool incarnations on the same budget.
        client.connect().await.unwrap();
        let second_pool = client.request_pool().expect("reconnect recreates the pool");
        assert_ne!(
            second_pool.lock().prefix(),
            first_prefix,
            "reconnect must create a fresh pool incarnation"
        );
        assert!(client.chunk_registry_arc().is_some());
        let response = echo_roundtrip(&client, "budget_detach_reconnect", &payload).await;
        assert_eq!(response_bytes(&client, response), payload);
        assert!(budget.snapshot().shm.used_bytes > 0);

        client.close().await;
        stop_server(&server).await;
    }

    // ── Cancellation-safe dispatched request ownership ────────────────────

    /// Echo callback that deterministically parks the server after dispatch:
    /// it signals `started` when invoked, blocks on `release`, then consumes
    /// the request and echoes it. `finished` fires after the request lease
    /// was released, so dedicated `read_done` is observable. The observed
    /// request transports are recorded so tests can prove exactly how many
    /// dispatches reached the server.
    struct StallingEcho {
        seen_kinds: Arc<Mutex<Vec<&'static str>>>,
        started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
        finished: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    }

    impl CrmCallback for StallingEcho {
        fn invoke(
            &self,
            _route_name: &str,
            _method_idx: u16,
            request: RequestData,
            _response_pool: Arc<RwLock<MemPool>>,
        ) -> Result<ResponseMeta, CrmError> {
            self.seen_kinds.lock().push(request_kind(&request));
            if let Some(started) = self.started.lock().take() {
                let _ = started.send(());
            }
            let _ = self.release.lock().recv_timeout(Duration::from_secs(10));
            let request = RequestLease::new(request);
            let bytes = request
                .into_owned_bytes()
                .map_err(CrmError::InternalError)?;
            if let Some(finished) = self.finished.lock().take() {
                let _ = finished.send(());
            }
            Ok(ResponseMeta::Inline(bytes))
        }
    }

    #[allow(clippy::type_complexity)]
    fn stalling_echo() -> (
        Arc<StallingEcho>,
        Arc<Mutex<Vec<&'static str>>>,
        tokio::sync::oneshot::Receiver<()>,
        std::sync::mpsc::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    ) {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = tokio::sync::oneshot::channel();
        let seen_kinds = Arc::new(Mutex::new(Vec::new()));
        (
            Arc::new(StallingEcho {
                seen_kinds: Arc::clone(&seen_kinds),
                started: Mutex::new(Some(started_tx)),
                release: Mutex::new(release_rx),
                finished: Mutex::new(Some(finished_tx)),
            }),
            seen_kinds,
            started_rx,
            release_tx,
            finished_rx,
        )
    }

    /// Buddy request whose caller is cancelled after dispatch: the server
    /// owns the cross-process free, so the client must keep the allocation
    /// charged (never free it locally), let the server's free land exactly
    /// once, and never touch a replacement pool installed meanwhile.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_buddy_call_after_dispatch_keeps_the_peer_owner_free() {
        let (callback, _seen_kinds, started_rx, release_tx, finished_rx) = stalling_echo();
        let callback: Arc<dyn CrmCallback> = callback;
        let server = start_echo_server(
            "cancel_buddy_dispatch",
            server_config(small_base(64 * 1024, 2), 1024),
            callback,
        )
        .await;
        let mut client = IpcClient::with_config(
            server.ipc_address(),
            client_config(small_base(64 * 1024, 2), 1024),
        );
        client.connect().await.unwrap();
        let old_pool = client.request_pool().expect("transport-owned request pool");
        let binding = client
            .acquire_route(&expected_contract("cancel_buddy_dispatch"))
            .await
            .unwrap();
        let client = Arc::new(client);

        let payload = vec![7u8; 8192];
        let task = {
            let client = Arc::clone(&client);
            let binding = binding.clone();
            tokio::spawn(async move { client.call_bound(&binding, "echo", &payload).await })
        };
        timeout(Duration::from_secs(5), started_rx)
            .await
            .expect("callback must start (frame dispatched)")
            .unwrap();

        // Caller cancellation after dispatch, before any response.
        task.abort();
        let join_error = task.await.expect_err("the aborted call must be cancelled");
        assert!(join_error.is_cancelled());

        assert_eq!(
            old_pool.lock().stats().alloc_count,
            1,
            "a dispatched buddy allocation belongs to the server's free; the cancelled caller must keep it charged"
        );

        // Install a replacement pool incarnation exactly as a reconnect
        // would, with a live canary: every later release must address only
        // the old owner and never the replacement.
        let (fresh_config, fresh_budget) = {
            let pool = old_pool.lock();
            (
                pool.config().clone(),
                pool.budget()
                    .cloned()
                    .expect("owner pool carries the domain budget"),
            )
        };
        let fresh_pool = Arc::new(parking_lot::Mutex::new(
            MemPool::new_with_prefix_and_budget(
                fresh_config,
                format!(
                    "/cc3t5{:08x}{:08x}",
                    std::process::id(),
                    ADDR_COUNTER.fetch_add(1, Ordering::Relaxed)
                ),
                fresh_budget,
            ),
        ));
        client.replace_request_pool_for_test(Some(Arc::clone(&fresh_pool)));
        let _canary = fresh_pool.lock().alloc(64).expect("fresh-pool canary");
        assert_eq!(fresh_pool.lock().stats().alloc_count, 1);

        // Let the server finish: it frees the buddy block cross-process
        // exactly once and replies into a pending entry whose caller is gone.
        release_tx.send(()).expect("release callback");
        timeout(Duration::from_secs(5), finished_rx)
            .await
            .expect("callback must finish")
            .unwrap();
        wait_until(5, || old_pool.lock().stats().alloc_count == 0).await;

        // The shared allocator stayed consistent: a probe alloc/free on the
        // old owner must still round-trip after the server's cross-process
        // free (a second free of the same block would have corrupted the
        // shared free list).
        let probe = old_pool
            .lock()
            .alloc(8192)
            .expect("the old owner's allocator must stay consistent");
        old_pool
            .lock()
            .free(&probe)
            .expect("probe free must round-trip on the old owner");
        assert_eq!(
            fresh_pool.lock().stats().alloc_count,
            1,
            "the replacement pool's live canary must never be freed by stale coordinates"
        );

        client.close_shared().await;
        stop_server(&server).await;
    }

    /// Dedicated request whose caller is cancelled after dispatch: the owner
    /// release must settle (the cross-process `read_done` protocol keeps the
    /// mapping alive for the still-reading server), and after the server
    /// finishes, GC retires the backing and returns the shared-domain charge.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_dedicated_call_settles_its_owner_release() {
        let _retire_guard = crate::client::dedicated_retire_test_control::production_guard();
        let base = BaseIpcConfig {
            pool_enabled: false,
            chunk_size: 1 << 20,
            ..small_base(64 * 1024, 2)
        };
        let (callback, _seen_kinds, started_rx, release_tx, finished_rx) = stalling_echo();
        let callback: Arc<dyn CrmCallback> = callback;
        let server = start_echo_server(
            "cancel_dedicated",
            server_config(base.clone(), 1024),
            callback,
        )
        .await;
        let mut client = IpcClient::with_config(server.ipc_address(), client_config(base, 1024));
        client.connect().await.unwrap();
        let pool = client
            .request_pool()
            .expect("dedicated-capable request pool");
        let budget = pool
            .lock()
            .budget()
            .cloned()
            .expect("owner pool carries the domain budget");
        let binding = client
            .acquire_route(&expected_contract("cancel_dedicated"))
            .await
            .unwrap();
        let client = Arc::new(client);

        let payload = vec![7u8; 48 * 1024];
        let task = {
            let client = Arc::clone(&client);
            let binding = binding.clone();
            tokio::spawn(async move { client.call_bound(&binding, "echo", &payload).await })
        };
        timeout(Duration::from_secs(5), started_rx)
            .await
            .expect("callback must start (frame dispatched)")
            .unwrap();
        task.abort();
        let join_error = task.await.expect_err("the aborted call must be cancelled");
        assert!(join_error.is_cancelled());

        assert_eq!(
            pool.lock().stats().dedicated_active_count,
            0,
            "cancelling a dispatched dedicated call must settle the owner release exactly once"
        );
        assert_eq!(pool.lock().stats().alloc_count, 0);

        // The mapping stays for the stalled server (read_done not yet set):
        // the charge must remain until the peer signals completion.
        assert!(
            budget.snapshot().shm.used_bytes > 0,
            "the dedicated backing must stay charged while the peer may still read it"
        );

        release_tx.send(()).expect("release callback");
        timeout(Duration::from_secs(5), finished_rx)
            .await
            .expect("callback must finish (read_done now set)")
            .unwrap();
        pool.lock().gc_dedicated();
        assert_eq!(
            pool.lock().stats().dedicated_segments,
            0,
            "read_done must let GC retire the settled dedicated backing"
        );
        assert_eq!(
            budget.snapshot().shm.used_bytes,
            0,
            "retiring the backing must return the shared-domain charge"
        );

        client.close_shared().await;
        stop_server(&server).await;
    }

    /// Dedicated request whose block is owned by an external caller (the
    /// prealloc FFI shape): cancelling the call future leaves the block
    /// alive, so the confirmed-close drain must settle the owner release
    /// through the pending entry's release authority.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_settles_dispatched_dedicated_request_for_an_external_block() {
        let _retire_guard = crate::client::dedicated_retire_test_control::production_guard();
        let base = BaseIpcConfig {
            pool_enabled: false,
            chunk_size: 1 << 20,
            ..small_base(64 * 1024, 2)
        };
        let (callback, _seen_kinds, started_rx, release_tx, finished_rx) = stalling_echo();
        let callback: Arc<dyn CrmCallback> = callback;
        let server = start_echo_server(
            "close_settle_dedicated",
            server_config(base.clone(), 1024),
            callback,
        )
        .await;
        let mut client = IpcClient::with_config(server.ipc_address(), client_config(base, 1024));
        client.connect().await.unwrap();
        let pool = client
            .request_pool()
            .expect("dedicated-capable request pool");
        let budget = pool
            .lock()
            .budget()
            .cloned()
            .expect("owner pool carries the domain budget");
        let binding = client
            .acquire_route(&expected_contract("close_settle_dedicated"))
            .await
            .unwrap();
        let (method_idx, identity, _) = binding.call_target_for("echo").unwrap();

        let payload = vec![9u8; 48 * 1024];
        let block = client
            .try_alloc_request_block(payload.len())
            .expect("allocation attempt")
            .expect("a request pool is selected");
        block.write_at(0, &payload).expect("fill the block");

        let mut started_rx = started_rx;
        tokio::select! {
            started = &mut started_rx => {
                started.expect("callback must start (frame dispatched)");
            }
            result = client.call_with_prealloc(&identity, method_idx, &block, payload.len()) => {
                panic!("the call must not complete while the callback stalls: {result:?}");
            }
        }
        // The caller's future is gone: the send guard already handed the
        // dispatched dedicated release to the bounded retire path (freed_at
        // marked; the mapping stays for the still-reading peer). The still
        // alive external block must not double-settle.
        assert_eq!(
            pool.lock().stats().dedicated_active_count,
            0,
            "the cancelled reply wait settles the dispatched dedicated owner release"
        );
        let _ = block.release();
        assert_eq!(
            pool.lock().stats().dedicated_active_count,
            0,
            "the still-alive external block must not double-settle"
        );

        assert!(
            client.close_shared_bounded(Duration::from_secs(5)).await,
            "the close barrier must confirm while the callback stalls"
        );
        assert_eq!(
            pool.lock().stats().dedicated_active_count,
            0,
            "the confirmed-close drain must settle the owner release for a caller that is gone"
        );
        // The still-alive external block must not double-release.
        let _ = block.release();
        assert_eq!(
            pool.lock().stats().dedicated_active_count,
            0,
            "an explicit release after the drain must be an exact-once no-op"
        );

        release_tx.send(()).expect("release callback");
        timeout(Duration::from_secs(5), finished_rx)
            .await
            .expect("callback must finish (read_done now set)")
            .unwrap();
        pool.lock().gc_dedicated();
        assert_eq!(
            budget.snapshot().shm.used_bytes,
            0,
            "retiring the backing must return the shared-domain charge"
        );
        drop(client);
        stop_server(&server).await;
    }

    // ── Linearizable dispatch state and physical dedicated retirement ─────

    /// Raw handshake-only peer: completes the client handshake and hands the
    /// accepted stream back to the test, so tests can observe exactly which
    /// bytes the client wrote after connecting.
    async fn raw_handshake_peer(
        label: &str,
    ) -> (
        String,
        tokio::sync::oneshot::Receiver<c2_local::LocalStream>,
    ) {
        let address = unique_address(label);
        let endpoint = c2_local::LocalEndpoint::from_address(&address).expect("test endpoint");
        let mut listener = c2_local::LocalListener::bind(&endpoint).expect("test listener");
        let (stream_tx, stream_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let mut stream = listener.accept().await.expect("accept client");
            let mut len_buf = [0u8; 4];
            use tokio::io::AsyncReadExt as _;
            stream
                .read_exact(&mut len_buf)
                .await
                .expect("handshake len");
            let mut body = vec![0u8; u32::from_le_bytes(len_buf) as usize];
            stream.read_exact(&mut body).await.expect("handshake body");
            let identity = c2_wire::handshake::ServerIdentity {
                server_id: "raw-peer-server".into(),
                server_instance_id: "raw-peer-instance".into(),
            };
            let handshake = c2_wire::handshake::encode_server_handshake(
                &[],
                c2_wire::handshake::CAP_CALL_V2
                    | c2_wire::handshake::CAP_METHOD_IDX
                    | c2_wire::handshake::CAP_CHUNKED,
                &[],
                "",
                &identity,
            )
            .expect("server handshake");
            let frame = c2_wire::frame::encode_frame(
                0,
                c2_wire::flags::FLAG_HANDSHAKE | c2_wire::flags::FLAG_RESPONSE,
                &handshake,
            );
            stream.write_all(&frame).await.expect("send handshake");
            let _ = stream_tx.send(stream);
        });
        (address, stream_rx)
    }

    fn raw_peer_identity() -> c2_wire::control::RouteCallIdentity {
        c2_wire::control::RouteCallIdentity {
            route_name: "grid".into(),
            route_uid: "grid-route-uid-raw".into(),
            observed_route_revision: 1,
            crm_ns: "test.grid".into(),
            crm_name: "Grid".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: ABI_HASH.into(),
            signature_hash: SIG_HASH.into(),
        }
    }

    /// A released block must never be sent: `pool_free` followed by a call
    /// through the public prealloc surface must fail before any byte is
    /// written, leave exactly one free through the owning pool, and never
    /// reach the server.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn released_request_block_is_never_sent() {
        let (callback, seen_kinds) = echo_callback();
        let callback: Arc<dyn CrmCallback> = callback;
        let server = start_echo_server(
            "released_block",
            server_config(small_base(64 * 1024, 2), 1024),
            callback,
        )
        .await;
        let mut client = IpcClient::with_config(
            server.ipc_address(),
            client_config(small_base(64 * 1024, 2), 1024),
        );
        client.connect().await.unwrap();
        let pool = client.request_pool().expect("transport-owned request pool");
        let binding = client
            .acquire_route(&expected_contract("released_block"))
            .await
            .unwrap();
        let (method_idx, identity, _) = binding.call_target_for("echo").unwrap();

        let payload = vec![5u8; 8192];
        let block = client
            .try_alloc_request_block(payload.len())
            .expect("allocation attempt")
            .expect("a request pool is selected");
        block.write_at(0, &payload).expect("fill the block");
        // A live canary in the same pool: any misdirected or repeated free
        // would have to corrupt it or the allocator state.
        let _canary = pool.lock().alloc(64).expect("pool canary");
        assert_eq!(pool.lock().stats().alloc_count, 2);

        // The public release path (SyncClient::pool_free equivalent): the
        // armed block is freed through its exact owner.
        let _ = block.release();
        assert_eq!(
            pool.lock().stats().alloc_count,
            1,
            "releasing the armed block must free exactly its own allocation"
        );

        let err = timeout(
            Duration::from_secs(5),
            client.call_with_prealloc(&identity, method_idx, &block, payload.len()),
        )
        .await
        .expect("a released block must be rejected promptly")
        .expect_err("a released block must never be sent");
        assert!(
            matches!(err, IpcError::Pool(ref message) if message.contains("released")),
            "unexpected error: {err:?}"
        );
        assert!(
            seen_kinds.lock().is_empty(),
            "a released block must never reach server dispatch"
        );
        assert_eq!(
            pool.lock().stats().alloc_count,
            1,
            "no second free and no canary corruption: only the canary remains"
        );

        client.close_shared().await;
        stop_server(&server).await;
    }

    /// One block carries at most one in-flight frame: a second call while
    /// the first is dispatched must be rejected before any write, and the
    /// server must observe exactly one dispatch.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_block_never_carries_two_in_flight_frames() {
        let (callback, seen_kinds, started_rx, release_tx, finished_rx) = stalling_echo();
        let callback: Arc<dyn CrmCallback> = callback;
        let server = start_echo_server(
            "double_dispatch",
            server_config(small_base(64 * 1024, 2), 1024),
            callback,
        )
        .await;
        let mut client = IpcClient::with_config(
            server.ipc_address(),
            client_config(small_base(64 * 1024, 2), 1024),
        );
        client.connect().await.unwrap();
        let binding = client
            .acquire_route(&expected_contract("double_dispatch"))
            .await
            .unwrap();
        let (method_idx, identity, _) = binding.call_target_for("echo").unwrap();

        let payload = vec![6u8; 8192];
        let block = client
            .try_alloc_request_block(payload.len())
            .expect("allocation attempt")
            .expect("a request pool is selected");
        block.write_at(0, &payload).expect("fill the block");

        // Drive the first call to the stalling server: its frame was
        // dispatched and the call parks waiting for the response.
        let first = client.call_with_prealloc(&identity, method_idx, &block, payload.len());
        tokio::pin!(first);
        let mut started_rx = started_rx;
        tokio::select! {
            _ = &mut started_rx => {}
            result = &mut first => panic!("the first call must stall: {result:?}"),
        }
        assert_eq!(*seen_kinds.lock(), vec!["shm_buddy"]);

        let err = timeout(
            Duration::from_secs(5),
            client.call_with_prealloc(&identity, method_idx, &block, payload.len()),
        )
        .await
        .expect("a re-dispatch must be rejected promptly")
        .expect_err("an already-dispatched block must never carry a second frame");
        assert!(
            matches!(err, IpcError::Pool(ref message) if message.contains("dispatched")),
            "unexpected error: {err:?}"
        );
        assert_eq!(
            seen_kinds.lock().len(),
            1,
            "the server must observe exactly one dispatch for one block"
        );

        // The first call still completes normally after the stall.
        release_tx.send(()).expect("release callback");
        timeout(Duration::from_secs(5), finished_rx)
            .await
            .expect("callback must finish")
            .unwrap();
        let response = timeout(Duration::from_secs(5), &mut first)
            .await
            .expect("first call must complete")
            .expect("first call must succeed");
        assert_eq!(response_bytes(&client, response), payload);
        assert_eq!(seen_kinds.lock().len(), 1);

        client.close_shared().await;
        stop_server(&server).await;
    }

    /// Release racing the dispatch seam: the writer lock is held so the call
    /// parks before the seam, the block is released (armed free through its
    /// owner), and after the writer unblocks the call must fail without the
    /// peer receiving a single frame byte.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn release_racing_dispatch_never_sends_freed_coordinates() {
        let (address, peer_rx) = raw_handshake_peer("release_race_dispatch").await;
        let mut client =
            IpcClient::with_config(&address, client_config(small_base(64 * 1024, 2), 1024));
        client.connect().await.expect("connect");
        let mut peer = peer_rx.await.expect("peer stream");
        let pool = client.request_pool().expect("transport-owned request pool");

        let payload = vec![3u8; 8192];
        let block = client
            .try_alloc_request_block(payload.len())
            .expect("allocation attempt")
            .expect("a request pool is selected");
        block.write_at(0, &payload).expect("fill the block");

        // Deterministically occupy the writer slot so the call parks at the
        // writer-lock await, before the dispatch seam.
        let writer_slot = client.writer_slot_for_test();
        let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
        let (unhold_tx, unhold_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _guard = writer_slot.lock().await;
            let _ = held_tx.send(());
            let _ = unhold_rx.await;
        });
        held_rx.await.expect("writer slot must be held");

        let identity = raw_peer_identity();
        let call = client.call_with_prealloc(&identity, 0, &block, payload.len());
        tokio::pin!(call);
        use futures_util::FutureExt;
        assert!(
            call.as_mut().now_or_never().is_none(),
            "the call must park waiting for the writer lock"
        );
        assert_eq!(pool.lock().stats().alloc_count, 1);

        // Release wins the race (armed free through the exact owner).
        let _ = block.release();
        assert_eq!(
            pool.lock().stats().alloc_count,
            0,
            "the armed release must free through the owning pool"
        );

        let _ = unhold_tx.send(());
        match timeout(Duration::from_secs(2), &mut call).await {
            Ok(Err(err)) => assert!(
                matches!(err, IpcError::Pool(ref message) if message.contains("released")),
                "unexpected error: {err:?}"
            ),
            Ok(Ok(response)) => panic!("a released block must not be sent: {response:?}"),
            Err(_) => panic!(
                "a freed block was sent: the call is waiting for a response to a frame it must \
                 never have written"
            ),
        }

        // The peer must not have received a single frame byte.
        use tokio::io::AsyncReadExt;
        let mut probe = [0u8; 16];
        let read = timeout(Duration::from_millis(300), peer.read(&mut probe)).await;
        let silent = !matches!(read, Ok(Ok(count)) if count > 0);
        assert!(
            silent,
            "the peer must receive no bytes for a released block"
        );

        client.close_shared().await;
    }

    /// Physical dedicated lifecycle: after the caller, block, and the whole
    /// client are gone while the server callback still holds the unread
    /// request, the backing must stay mapped and charged (no external pool
    /// or block Arc may be needed), the server must still read the data, and
    /// only then may the charge return.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dedicated_backing_survives_client_drop_until_peer_read_done() {
        let _retire_guard = crate::client::dedicated_retire_test_control::production_guard();
        let base = BaseIpcConfig {
            pool_enabled: false,
            chunk_size: 1 << 20,
            ..small_base(64 * 1024, 2)
        };
        let (callback, _seen_kinds, started_rx, release_tx, finished_rx) = stalling_echo();
        let callback: Arc<dyn CrmCallback> = callback;
        let server = start_echo_server(
            "dedicated_survive_drop",
            server_config(base.clone(), 1024),
            callback,
        )
        .await;
        let mut client = IpcClient::with_config(server.ipc_address(), client_config(base, 1024));
        client.connect().await.unwrap();
        // Keep only the domain budget (an accounting handle, not backing
        // retention); the pool Arc is dropped immediately.
        let budget = client
            .request_pool()
            .expect("dedicated-capable request pool")
            .lock()
            .budget()
            .cloned()
            .expect("owner pool carries the domain budget");
        let binding = client
            .acquire_route(&expected_contract("dedicated_survive_drop"))
            .await
            .unwrap();
        let client = Arc::new(client);

        let payload = vec![11u8; 48 * 1024];
        let task = {
            let client = Arc::clone(&client);
            let binding = binding.clone();
            tokio::spawn(async move { client.call_bound(&binding, "echo", &payload).await })
        };
        timeout(Duration::from_secs(5), started_rx)
            .await
            .expect("callback must start (frame dispatched)")
            .unwrap();
        task.abort();
        assert!(task.await.expect_err("aborted").is_cancelled());

        // Confirmed close and full client drop with no external pool or
        // block reference left in the test.
        assert!(
            client.close_shared_bounded(Duration::from_secs(5)).await,
            "the close barrier must confirm while the callback stalls"
        );
        match Arc::try_unwrap(client) {
            Ok(dropped) => drop(dropped),
            Err(_) => panic!("no other client owners may retain the client"),
        }

        assert!(
            budget.snapshot().shm.used_bytes > 0,
            "the settled dedicated backing must stay mapped and charged while the peer still reads"
        );

        // The server finishes: its read through the peer mapping must still
        // see the data (read_done is set by that same release), and only
        // then may the bounded retainer return the charge.
        release_tx.send(()).expect("release callback");
        timeout(Duration::from_secs(5), finished_rx)
            .await
            .expect("the server must still read the request data after client drop")
            .unwrap();
        wait_until(5, || budget.snapshot().shm.used_bytes == 0).await;

        stop_server(&server).await;
    }

    /// Bounded crash retirement: when the peer never reads, the settled
    /// dedicated backing stays charged until the pool's configured crash
    /// timeout retires it — never leaked forever, never dropped early.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dedicated_backing_retires_within_bounded_crash_timeout() {
        let _retire_guard = crate::client::dedicated_retire_test_control::production_guard();
        let base = BaseIpcConfig {
            pool_enabled: false,
            chunk_size: 1 << 20,
            ..small_base(64 * 1024, 2)
        };
        let (callback, _seen_kinds, started_rx, release_tx, finished_rx) = stalling_echo();
        let callback: Arc<dyn CrmCallback> = callback;
        let server = start_echo_server(
            "dedicated_crash_retire",
            server_config(base.clone(), 1024),
            callback,
        )
        .await;
        // Build the client around a short-crash-timeout, buddy-disabled
        // injected pool exactly as a deployment would configure it, and drop
        // the test's pool reference right away: only production ownership
        // may keep the backing alive from here on.
        let cfg = client_config(base, 1024);
        let budget = c2_mem::MemoryBudget::from_limits(&cfg.memory_budget_limits());
        let mut pool_config = cfg.base.primary_pool_config(&cfg.pool_tuning());
        pool_config.dedicated_crash_timeout_secs = 0.25;
        let pool = Arc::new(Mutex::new(MemPool::new_with_prefix_and_budget(
            pool_config,
            format!(
                "/cc3crash{:08x}{:08x}",
                std::process::id(),
                ADDR_COUNTER.fetch_add(1, Ordering::Relaxed)
            ),
            budget.clone(),
        )));
        let mut client = IpcClient::with_pool(server.ipc_address(), Arc::clone(&pool), cfg);
        drop(pool);
        client.connect().await.unwrap();

        let binding = client
            .acquire_route(&expected_contract("dedicated_crash_retire"))
            .await
            .unwrap();
        let client = Arc::new(client);

        let payload = vec![11u8; 48 * 1024];
        let task = {
            let client = Arc::clone(&client);
            let binding = binding.clone();
            tokio::spawn(async move { client.call_bound(&binding, "echo", &payload).await })
        };
        timeout(Duration::from_secs(5), started_rx)
            .await
            .expect("callback must start (frame dispatched)")
            .unwrap();
        task.abort();
        assert!(task.await.expect_err("aborted").is_cancelled());
        assert!(
            client.close_shared_bounded(Duration::from_secs(5)).await,
            "the close barrier must confirm while the callback stalls"
        );
        drop(client);

        assert!(
            budget.snapshot().shm.used_bytes > 0,
            "the settled dedicated backing must stay charged while the peer has not read"
        );
        // The peer never reads before this point, so only the crash-timeout
        // policy can retire the backing — bounded, then refunded.
        wait_until(5, || budget.snapshot().shm.used_bytes == 0).await;

        release_tx.send(()).expect("release callback");
        timeout(Duration::from_secs(5), finished_rx)
            .await
            .expect("callback must finish for shutdown")
            .unwrap();
        stop_server(&server).await;
    }

    // ── Fast replies, partial writes, and per-call pending cleanup ────────

    /// A fast reply must settle an externally borrowed dedicated block even
    /// when the caller vanishes inside the write: the pending entry carries
    /// the release authority from the moment bytes could flow, so neither a
    /// response that beats the caller nor a mid-write cancellation can
    /// strand the owner release.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fast_reply_settles_external_dedicated_block_despite_cancellation() {
        let _retire_guard = crate::client::dedicated_retire_test_control::production_guard();
        let base = BaseIpcConfig {
            pool_enabled: false,
            chunk_size: 1 << 20,
            ..small_base(64 * 1024, 2)
        };
        let (callback, seen_kinds) = echo_callback();
        let callback: Arc<dyn CrmCallback> = callback;
        let server = start_echo_server(
            "fast_reply_settle",
            server_config(base.clone(), 1024),
            callback,
        )
        .await;
        let mut client = IpcClient::with_config(server.ipc_address(), client_config(base, 1024));
        client.connect().await.unwrap();
        let pool = client
            .request_pool()
            .expect("dedicated-capable request pool");
        let binding = client
            .acquire_route(&expected_contract("fast_reply_settle"))
            .await
            .unwrap();
        let (method_idx, identity, _) = binding.call_target_for("echo").unwrap();

        let payload = vec![13u8; 48 * 1024];
        let block = client
            .try_alloc_request_block(payload.len())
            .expect("allocation attempt")
            .expect("a request pool is selected");
        block.write_at(0, &payload).expect("fill the block");

        // Seam: write the complete frame, then park inside the write. The
        // instant server dispatches and replies while the caller is parked.
        let (prefix_tx, prefix_rx) = tokio::sync::oneshot::channel::<()>();
        let (_park_hold, park_release) = tokio::sync::oneshot::channel::<()>();
        client.set_frame_write_seam_for_test(Some(FrameWriteSeam {
            prefix_bytes: usize::MAX,
            prefix_written: prefix_tx,
            release: park_release,
        }));

        let mut call =
            Box::pin(client.call_with_prealloc(&identity, method_idx, &block, payload.len()));
        let mut prefix_rx = prefix_rx;
        tokio::select! {
            _ = &mut prefix_rx => {}
            result = &mut call => panic!("the call must park inside the write seam: {result:?}"),
        }
        // The full frame landed; wait for the instant server to dispatch.
        wait_until(5, || !seen_kinds.lock().is_empty()).await;

        // Caller cancellation while parked inside the write; the external
        // block stays alive, so only the armed pending entry can settle it.
        drop(call);

        wait_until(5, || pool.lock().stats().dedicated_active_count == 0).await;
        assert_eq!(
            seen_kinds.lock().len(),
            1,
            "exactly one dispatch happened for one block"
        );
        // The still-alive external block must not double-settle.
        let _ = block.release();
        assert_eq!(pool.lock().stats().dedicated_active_count, 0);

        client.close_shared().await;
        stop_server(&server).await;
    }

    /// Cancelling a partial frame write must poison the stream: the peer has
    /// really received a prefix (twelve bytes of a frame header), no later
    /// call may append to the corrupt stream, the pending entry drains with
    /// a correct settle, and the buddy block stays conservatively charged
    /// until pool destruction returns it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn partial_frame_write_cancellation_poisons_the_stream() {
        let (address, peer_rx) = raw_handshake_peer("partial_write_poison").await;
        let base = BaseIpcConfig {
            // One backing fits; two simultaneous 64 KiB buddy backings do
            // not. Reacquire must use this same finite budget, not a reset.
            shm_backing_budget_bytes: 128 * 1024,
            ..small_base(64 * 1024, 2)
        };
        let cfg = client_config(base.clone(), 1024);
        let mut client = IpcClient::with_config(&address, cfg.clone());
        client.connect().await.expect("connect");
        let mut peer = peer_rx.await.expect("peer stream");
        let pool = client.request_pool().expect("transport-owned request pool");
        let budget = pool
            .lock()
            .budget()
            .cloned()
            .expect("owner pool carries the domain budget");
        let first_prefix = pool.lock().prefix().to_string();

        let payload = vec![7u8; 8192];
        let block = client
            .try_alloc_request_block(payload.len())
            .expect("allocation attempt")
            .expect("a request pool is selected");
        block.write_at(0, &payload).expect("fill the block");

        // Seam: write only a twelve-byte header prefix, then park mid-frame.
        let (prefix_tx, prefix_rx) = tokio::sync::oneshot::channel::<()>();
        let (_park_hold, park_release) = tokio::sync::oneshot::channel::<()>();
        client.set_frame_write_seam_for_test(Some(FrameWriteSeam {
            prefix_bytes: 12,
            prefix_written: prefix_tx,
            release: park_release,
        }));

        let identity = raw_peer_identity();
        let mut call = Box::pin(client.call_with_prealloc(&identity, 0, &block, payload.len()));
        let mut prefix_rx = prefix_rx;
        tokio::select! {
            _ = &mut prefix_rx => {}
            result = &mut call => panic!("the call must park mid-frame: {result:?}"),
        }

        // Proof that bytes really landed: the peer reads exactly the
        // twelve-byte prefix of the frame header.
        use tokio::io::AsyncReadExt as _;
        let mut prefix_buf = [0u8; 12];
        timeout(Duration::from_secs(3), peer.read_exact(&mut prefix_buf))
            .await
            .expect("the written prefix must be readable by the peer")
            .expect("prefix read");

        // Caller cancellation after the partial write.
        drop(call);

        // A follow-up call must fail instead of appending to the partial
        // frame on the same stream.
        let err = timeout(
            Duration::from_secs(3),
            client.call_inline(&identity, 0, b"follow-up"),
        )
        .await
        .expect("the follow-up call must terminate")
        .expect_err("no call may append to a corrupt stream");
        assert!(
            matches!(err, IpcError::Io(_) | IpcError::Closed),
            "unexpected follow-up error: {err:?}"
        );

        // The peer receives nothing beyond the prefix.
        let mut probe = [0u8; 8];
        let read = timeout(Duration::from_millis(300), peer.read(&mut probe)).await;
        let silent = !matches!(read, Ok(Ok(count)) if count > 0);
        assert!(silent, "the poisoned stream must carry no further bytes");

        // The pending entry drains with a correct settle.
        wait_until(2, || client.pending_len_for_test() == 0).await;
        // A dispatched buddy block is never freed locally: it stays charged
        // until pool destruction returns it.
        assert_eq!(
            pool.lock().stats().alloc_count,
            1,
            "the buddy block stays conservatively charged for the peer"
        );
        assert!(budget.snapshot().cell(c2_mem::BudgetKind::Shm).used_bytes > 0);
        let retained_bytes = budget.snapshot().shm.used_bytes;
        assert!(client.close_shared_bounded(Duration::from_secs(2)).await);
        assert!(client.request_pool().is_none());
        assert_eq!(
            pool.lock().stats().alloc_count,
            1,
            "close must not locally free a possibly published buddy block"
        );
        drop(pool);
        assert_eq!(
            budget.snapshot().shm.used_bytes,
            retained_bytes,
            "the real external RequestBlock keeps its pool and budget alive"
        );
        drop(block);
        assert_eq!(
            budget.snapshot().cell(c2_mem::BudgetKind::Shm).used_bytes,
            0,
            "the closed client must not pin an orphaned buddy backing"
        );

        // Keep the closed client alive while a fresh connection uses exactly
        // the same frozen budget to perform a real buddy request.
        let (callback, seen_kinds) = echo_callback();
        let server = start_echo_server(
            "partial_write_reacquire",
            server_config(base, 1024),
            callback,
        )
        .await;
        let mut reacquired =
            IpcClient::with_shared_budget(server.ipc_address(), cfg, budget.clone());
        reacquired.connect().await.expect("same-budget reconnect");
        assert_ne!(
            reacquired.request_pool().unwrap().lock().prefix(),
            first_prefix,
            "the old buddy coordinates must not be reused in a new incarnation"
        );
        let response = echo_roundtrip(&reacquired, "partial_write_reacquire", &payload).await;
        assert_eq!(response_bytes(&reacquired, response), payload);
        assert_eq!(*seen_kinds.lock(), vec!["shm_buddy"]);
        assert!(reacquired.close_shared_bounded(Duration::from_secs(2)).await);
        drop(client);
        assert_eq!(budget.snapshot().shm.used_bytes, 0);
        stop_server(&server).await;
    }

    /// Cancelling a call while it waits for the writer lock must remove its
    /// pending entry immediately: nothing was publishable, so a healthy
    /// connection must not carry per-call residue.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_call_waiting_for_the_writer_leaves_no_pending_entry() {
        let cfg = client_config(small_base(64 * 1024, 2), 1024);
        let client = IpcClient::with_config("ipc://pending_entry_cleanup", cfg);
        let pool = client.request_pool().expect("transport-owned request pool");

        let writer_slot = client.writer_slot_for_test();
        let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
        let (unhold_tx, unhold_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            let _guard = writer_slot.lock().await;
            let _ = held_tx.send(());
            let _ = unhold_rx.await;
        });
        held_rx.await.expect("writer slot must be held");

        let payload = vec![7u8; 8192];
        let block = client
            .try_alloc_request_block(payload.len())
            .expect("allocation attempt")
            .expect("a request pool is selected");
        block.write_at(0, &payload).expect("fill the block");

        let identity = raw_peer_identity();
        let mut call = Box::pin(client.call_with_prealloc(&identity, 0, &block, payload.len()));
        use futures_util::FutureExt as _;
        assert!(
            call.as_mut().now_or_never().is_none(),
            "the call must park waiting for the writer lock"
        );
        assert_eq!(
            client.pending_len_for_test(),
            1,
            "the in-flight call owns exactly one pending entry"
        );

        // Caller cancellation: the send guard removes the entry at once.
        drop(call);
        assert_eq!(
            client.pending_len_for_test(),
            0,
            "cancellation before the write must remove the pending entry immediately"
        );
        assert_eq!(pool.lock().stats().alloc_count, 1, "the block is armed");
        let _ = block.release();
        assert_eq!(pool.lock().stats().alloc_count, 0);

        let _ = unhold_tx.send(());
        client.close_shared().await;
    }

    // ── Reply-wait cancellation, late replies, and bounded retire capacity ─

    /// Echo callback that records dispatches (multi-shot) and stalls every
    /// invoke until released, for batch-cancellation coverage.
    struct CountingStall {
        seen_kinds: Arc<Mutex<Vec<&'static str>>>,
        release: Mutex<std::sync::mpsc::Receiver<()>>,
        finished: std::sync::atomic::AtomicUsize,
    }

    impl CrmCallback for CountingStall {
        fn invoke(
            &self,
            _route_name: &str,
            _method_idx: u16,
            request: RequestData,
            _response_pool: Arc<RwLock<MemPool>>,
        ) -> Result<ResponseMeta, CrmError> {
            self.seen_kinds.lock().push(request_kind(&request));
            let _ = self.release.lock().recv_timeout(Duration::from_secs(10));
            let request = RequestLease::new(request);
            let bytes = request
                .into_owned_bytes()
                .map_err(CrmError::InternalError)?;
            self.finished
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(ResponseMeta::Inline(bytes))
        }
    }

    /// Cancelling a call while it waits for a delayed reply on an otherwise
    /// healthy connection must clean up its pending waiter, hand the
    /// dedicated release authority to the retire path (charge stays until
    /// read_done), and let the late reply release its response backing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_reply_wait_cleans_pending_and_hands_off_dedicated() {
        let _retire_guard = crate::client::dedicated_retire_test_control::production_guard();
        let base = BaseIpcConfig {
            pool_enabled: false,
            chunk_size: 1 << 20,
            ..small_base(64 * 1024, 2)
        };
        let (callback, _seen_kinds, started_rx, release_tx, finished_rx) = stalling_echo();
        let callback: Arc<dyn CrmCallback> = callback;
        // Small server shm_threshold so the (late) reply uses SHM backing we
        // can observe on the server's response pool.
        let server = start_echo_server(
            "reply_wait_cancel",
            server_config(base.clone(), 64),
            callback,
        )
        .await;
        let mut client = IpcClient::with_config(server.ipc_address(), client_config(base, 1024));
        client.connect().await.unwrap();
        let budget = client
            .request_pool()
            .expect("dedicated-capable request pool")
            .lock()
            .budget()
            .cloned()
            .expect("owner pool carries the domain budget");
        let binding = client
            .acquire_route(&expected_contract("reply_wait_cancel"))
            .await
            .unwrap();
        let client = Arc::new(client);

        let payload = vec![11u8; 48 * 1024];
        let task = {
            let client = Arc::clone(&client);
            let binding = binding.clone();
            tokio::spawn(async move { client.call_bound(&binding, "echo", &payload).await })
        };
        timeout(Duration::from_secs(5), started_rx)
            .await
            .expect("callback must start (frame dispatched)")
            .unwrap();
        assert_eq!(client.pending_len_for_test(), 1);

        // Cancel while the call waits for the delayed reply: the connection
        // itself stays healthy.
        task.abort();
        assert!(task.await.expect_err("aborted").is_cancelled());
        assert!(
            client.is_connected(),
            "cancelling a reply wait must not tear down the healthy connection"
        );
        assert_eq!(
            client.pending_len_for_test(),
            0,
            "cancelling a reply wait must remove the pending waiter immediately"
        );
        // The dedicated owner release was handed off: freed_at marked and
        // the backing retained (not yet refunded) while the peer still reads.
        assert!(
            budget.snapshot().shm.used_bytes > 0,
            "the handed-off dedicated backing must stay charged until read_done"
        );

        // Let the late reply land: its response backing must be released
        // even though the waiter is gone.
        release_tx.send(()).expect("release callback");
        timeout(Duration::from_secs(5), finished_rx)
            .await
            .expect("callback must finish")
            .unwrap();
        wait_until(5, || budget.snapshot().shm.used_bytes == 0).await;
        wait_until(5, || {
            server.response_pool_arc().read().stats().alloc_count == 0
        })
        .await;
        client.close_shared().await;
        stop_server(&server).await;
    }

    /// Cancelling the reply wait must clean the pending waiter on every CRM
    /// sender: inline, buddy/prealloc, and chunked.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_reply_wait_cleans_pending_across_senders() {
        let _retire_guard = crate::client::dedicated_retire_test_control::production_guard();
        let (callback, seen_kinds, release_tx) = {
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let seen_kinds = Arc::new(Mutex::new(Vec::new()));
            (
                Arc::new(CountingStall {
                    seen_kinds: Arc::clone(&seen_kinds),
                    release: Mutex::new(release_rx),
                    finished: std::sync::atomic::AtomicUsize::new(0),
                }),
                seen_kinds,
                release_tx,
            )
        };
        let callback: Arc<dyn CrmCallback> = callback;
        // Three client policies, one connection each: a high threshold with a
        // small chunk_size gives the inline (4 B) and chunked (48 KiB >
        // chunk_size, under the threshold) senders; a buddy-disabled
        // low-threshold client gives the prealloc/dedicated sender (4 KiB).
        // One in-flight request per connection keeps every cancellation in the
        // reply wait without depending on cross-request reassembly ordering.
        let chunked_base = BaseIpcConfig {
            chunk_size: 16 * 1024,
            ..small_base(64 * 1024, 2)
        };
        let dedicated_base = BaseIpcConfig {
            pool_enabled: false,
            chunk_size: 16 * 1024,
            ..small_base(64 * 1024, 2)
        };
        let server = start_echo_server(
            "sender_cancel",
            server_config(chunked_base.clone(), 64),
            callback,
        )
        .await;
        let expected = expected_contract("sender_cancel");

        let mut inline_client = IpcClient::with_config(
            server.ipc_address(),
            client_config(chunked_base.clone(), 1 << 20),
        );
        inline_client.connect().await.unwrap();
        let inline_binding = inline_client.acquire_route(&expected).await.unwrap();
        let inline_client = Arc::new(inline_client);
        let mut chunked_client =
            IpcClient::with_config(server.ipc_address(), client_config(chunked_base, 1 << 20));
        chunked_client.connect().await.unwrap();
        let chunked_binding = chunked_client.acquire_route(&expected).await.unwrap();
        let chunked_client = Arc::new(chunked_client);
        let mut dedicated_client =
            IpcClient::with_config(server.ipc_address(), client_config(dedicated_base, 8));
        dedicated_client.connect().await.unwrap();
        let dedicated_binding = dedicated_client.acquire_route(&expected).await.unwrap();
        let dedicated_client = Arc::new(dedicated_client);

        let mut kinds = Vec::new();
        for (label, client, binding, size) in [
            ("inline", &inline_client, &inline_binding, 4usize),
            (
                "chunked_handle",
                &chunked_client,
                &chunked_binding,
                48 * 1024,
            ),
            ("shm_dedicated", &dedicated_client, &dedicated_binding, 4096),
        ] {
            let dispatched = seen_kinds.lock().len();
            let task = {
                let client = Arc::clone(client);
                let binding = binding.clone();
                let payload = vec![7u8; size];
                tokio::spawn(async move { client.call_bound(&binding, "echo", &payload).await })
            };
            // The stalled callback has not read the request, so the frame was
            // fully sent and the caller is parked in the reply wait.
            wait_until(5, || seen_kinds.lock().len() > dispatched).await;
            assert_eq!(
                seen_kinds.lock().last().copied(),
                Some(label),
                "the {label} sender must be the transport under test"
            );
            task.abort();
            assert!(
                task.await.expect_err("aborted").is_cancelled(),
                "the {label} caller must be cancelled in its reply wait"
            );
            assert_eq!(
                client.pending_len_for_test(),
                0,
                "{label}: cancelling a reply wait must remove the pending waiter immediately"
            );
            assert!(
                client.is_connected(),
                "{label}: cancelling a reply wait must not tear down the healthy connection"
            );
            kinds.push(label);
        }
        assert_eq!(
            kinds,
            vec!["inline", "chunked_handle", "shm_dedicated"],
            "all three CRM senders must be covered"
        );

        // Wake each stalled invoke so the server can shut down cleanly.
        for _ in 0..3 {
            let _ = release_tx.send(());
        }
        inline_client.close_shared().await;
        chunked_client.close_shared().await;
        dedicated_client.close_shared().await;
        stop_server(&server).await;
    }

    /// Retire-worker creation failure must fail the dedicated call closed
    /// before publication: no bytes written, no dispatch, the block stays
    /// armed — and the failure is recoverable once workers are available
    /// again.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retire_spawn_failure_fails_closed_before_publication() {
        let _retire_lock = crate::client::dedicated_retire_test_control::exclusive_lock();
        let (callback, seen_kinds) = echo_callback();
        let callback: Arc<dyn CrmCallback> = callback;
        let base = BaseIpcConfig {
            pool_enabled: false,
            chunk_size: 1 << 20,
            ..small_base(64 * 1024, 2)
        };
        let server = start_echo_server(
            "retire_spawn_fail",
            server_config(base.clone(), 64),
            callback,
        )
        .await;
        let mut client = IpcClient::with_config(server.ipc_address(), client_config(base, 8));
        client.connect().await.unwrap();
        let pool = client
            .request_pool()
            .expect("dedicated-capable request pool");
        let binding = client
            .acquire_route(&expected_contract("retire_spawn_fail"))
            .await
            .unwrap();
        let (method_idx, identity, _) = binding.call_target_for("echo").unwrap();

        let payload = vec![9u8; 4096];
        let block = client
            .try_alloc_request_block(payload.len())
            .expect("allocation attempt")
            .expect("a request pool is selected");
        block.write_at(0, &payload).expect("fill the block");
        assert!(block.is_dedicated());

        let spawn_failure = crate::client::dedicated_retire_test_control::fail_worker_spawn();
        let err = timeout(
            Duration::from_secs(5),
            client.call_with_prealloc(&identity, method_idx, &block, payload.len()),
        )
        .await
        .expect("the call must fail promptly")
        .expect_err("a dedicated publication must fail without a retire worker");
        assert!(
            matches!(err, IpcError::Pool(ref message) if message.contains("retire worker unavailable")),
            "unexpected error: {err:?}"
        );
        assert!(
            seen_kinds.lock().is_empty(),
            "the unpublished block must never reach dispatch"
        );
        assert_eq!(
            pool.lock().stats().alloc_count,
            0,
            "the unpublished failure released the armed block cleanly (no leak)"
        );

        // Recoverable: a fresh publication succeeds once workers exist.
        drop(spawn_failure);
        let block = client
            .try_alloc_request_block(payload.len())
            .expect("allocation attempt")
            .expect("a request pool is selected");
        block.write_at(0, &payload).expect("fill the block");
        let response = timeout(
            Duration::from_secs(5),
            client.call_with_prealloc(&identity, method_idx, &block, payload.len()),
        )
        .await
        .expect("retry must complete")
        .expect("retry must succeed after worker recovery");
        assert_eq!(response_bytes(&client, response), payload);
        assert_eq!(seen_kinds.lock().len(), 1);

        client.close_shared().await;
        stop_server(&server).await;
    }

    /// Capacity saturation is a pre-publication rejection: the first
    /// retained backing keeps its charge (never dropped or refunded), the
    /// next dedicated publication fails with an explicit capacity error,
    /// and once the retained backing retires through read_done the permit
    /// returns and the same call succeeds.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retire_capacity_saturation_retains_and_recovers() {
        let _retire_lock = crate::client::dedicated_retire_test_control::exclusive_lock();
        let (callback, seen_kinds, release_tx) = {
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let seen_kinds = Arc::new(Mutex::new(Vec::new()));
            (
                Arc::new(CountingStall {
                    seen_kinds: Arc::clone(&seen_kinds),
                    release: Mutex::new(release_rx),
                    finished: std::sync::atomic::AtomicUsize::new(0),
                }),
                seen_kinds,
                release_tx,
            )
        };
        let callback: Arc<dyn CrmCallback> = callback;
        let base = BaseIpcConfig {
            pool_enabled: false,
            chunk_size: 1 << 20,
            ..small_base(64 * 1024, 2)
        };
        let server =
            start_echo_server("retire_capacity", server_config(base.clone(), 64), callback).await;
        let mut client = IpcClient::with_config(server.ipc_address(), client_config(base, 8));
        client.connect().await.unwrap();
        let budget = client
            .request_pool()
            .expect("dedicated-capable request pool")
            .lock()
            .budget()
            .cloned()
            .expect("owner pool carries the domain budget");
        let binding = client
            .acquire_route(&expected_contract("retire_capacity"))
            .await
            .unwrap();
        let (method_idx, identity, _) = binding.call_target_for("echo").unwrap();
        let client = Arc::new(client);

        // A previously finished test may still own a permit for a backing its
        // peer has not read; size the override one slot above that baseline.
        let baseline = retire_permit_baseline().await;
        let _capacity =
            crate::client::dedicated_retire_test_control::capacity_override(baseline + 1);
        let client_pool = client
            .request_pool()
            .expect("dedicated-capable request pool");

        // First dedicated call: dispatched, then cancelled at the reply wait
        // — its backing is retained through the bounded retire queue.
        let first = {
            let client = Arc::clone(&client);
            let binding = binding.clone();
            let payload = vec![5u8; 4096];
            tokio::spawn(async move { client.call_bound(&binding, "echo", &payload).await })
        };
        wait_until(5, || !seen_kinds.lock().is_empty()).await;
        first.abort();
        assert!(first.await.expect_err("aborted").is_cancelled());
        // Pool-local evidence: this call's freed-but-retained dedicated entry.
        wait_until(5, || client_pool.lock().stats().dedicated_segments >= 1).await;
        assert!(
            budget.snapshot().shm.used_bytes > 0,
            "the saturated-retained backing must stay charged"
        );

        // Second dedicated publication is rejected before any byte: the block
        // stays armed and no dispatch happens. The override is re-measured per
        // attempt because a permit owned by a test that already finished can
        // retire at any moment; the admission check is exact whenever the
        // grant count does not change between measurement and admission.
        let second_block = client
            .try_alloc_request_block(4096)
            .expect("allocation attempt")
            .expect("a request pool is selected");
        let mut rejection = None;
        for attempt in 0..5 {
            crate::client::dedicated_retire_test_control::set_capacity_override(
                crate::client::dedicated_retire_test_control::snapshot().1,
            );
            match timeout(
                Duration::from_secs(5),
                client.call_with_prealloc(&identity, method_idx, &second_block, 4096),
            )
            .await
            {
                Ok(Err(err)) => {
                    rejection = Some(err);
                    break;
                }
                Ok(Ok(_)) => panic!(
                    "attempt {attempt}: a dedicated publication must be rejected while the \
                     retention queue is full"
                ),
                Err(_) => panic!("attempt {attempt}: the saturated call must fail promptly"),
            }
        }
        let err = rejection.expect("saturation must reject the publication");
        assert!(
            matches!(err, IpcError::Pool(ref message) if message.contains("retire capacity exhausted")),
            "unexpected error: {err:?}"
        );
        assert_eq!(seen_kinds.lock().len(), 1, "no second dispatch happened");

        // Retire the retained backing through read_done; the permit returns
        // and a fresh publication then succeeds.
        let _ = release_tx.send(());
        wait_until(5, || {
            crate::client::dedicated_retire_test_control::snapshot().1 <= baseline
        })
        .await;
        let retry_block = client
            .try_alloc_request_block(4096)
            .expect("allocation attempt")
            .expect("a request pool is selected");
        retry_block
            .write_at(0, &[5u8; 4096][..])
            .expect("fill the block");
        // The retry's stalled callback reads the next queued release signal.
        let _ = release_tx.send(());
        let response = timeout(
            Duration::from_secs(5),
            client.call_with_prealloc(&identity, method_idx, &retry_block, 4096),
        )
        .await
        .expect("retry must complete")
        .expect("retry must succeed after retirement");
        assert_eq!(response_bytes(&client, response).len(), 4096);

        client.close_shared().await;
        stop_server(&server).await;
    }

    /// Batch cancellation must keep retire workers bounded and eventually
    /// reclaim every retained backing.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retire_workers_stay_bounded_under_batch_cancellation() {
        let _retire_lock = crate::client::dedicated_retire_test_control::exclusive_lock();
        let (callback, seen_kinds, release_tx) = {
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let seen_kinds = Arc::new(Mutex::new(Vec::new()));
            (
                Arc::new(CountingStall {
                    seen_kinds: Arc::clone(&seen_kinds),
                    release: Mutex::new(release_rx),
                    finished: std::sync::atomic::AtomicUsize::new(0),
                }),
                seen_kinds,
                release_tx,
            )
        };
        let callback: Arc<dyn CrmCallback> = callback;
        let base = BaseIpcConfig {
            pool_enabled: false,
            chunk_size: 1 << 20,
            ..small_base(64 * 1024, 2)
        };
        // The scenario needs `BATCH` callbacks stalled at the same time: the
        // `64` below is the server's shm_threshold, not a worker count.
        const BATCH: usize = 4;
        let server = start_echo_server(
            "retire_batch_bound",
            cancellation_server_config(base.clone(), 64, BATCH),
            callback,
        )
        .await;
        let mut client = IpcClient::with_config(server.ipc_address(), client_config(base, 8));
        client.connect().await.unwrap();
        let budget = client
            .request_pool()
            .expect("dedicated-capable request pool")
            .lock()
            .budget()
            .cloned()
            .expect("owner pool carries the domain budget");
        let binding = client
            .acquire_route(&expected_contract("retire_batch_bound"))
            .await
            .unwrap();
        let client = Arc::new(client);
        let client_pool = client
            .request_pool()
            .expect("dedicated-capable request pool");
        let call_errors = Arc::new(Mutex::new(Vec::<String>::new()));
        let tasks: Vec<_> = (0..BATCH)
            .map(|_| {
                let client = Arc::clone(&client);
                let binding = binding.clone();
                let payload = vec![5u8; 4096];
                let call_errors = Arc::clone(&call_errors);
                tokio::spawn(async move {
                    let result = client.call_bound(&binding, "echo", &payload).await;
                    if let Err(error) = &result {
                        call_errors.lock().push(format!("{error:?}"));
                    }
                    result
                })
            })
            .collect();
        wait_until_observed(
            5,
            "every batch callback must be dispatched to the server",
            || seen_kinds.lock().len() >= BATCH,
            || {
                format!(
                    "{} call_errors={:?}",
                    cancellation_state(
                        "batch dispatch",
                        seen_kinds.lock().len(),
                        client.pending_len_for_test(),
                        client.is_connected(),
                        &client_pool,
                        &budget,
                    ),
                    call_errors.lock().clone(),
                )
            },
        )
        .await;

        for task in tasks {
            task.abort();
            let _ = task.await;
        }
        // Every cancelled call retained its backing (pool-local evidence: the
        // freed-but-unread dedicated entries are still mapped); the shared
        // executor serves them with a bounded worker set.
        wait_until_observed(
            5,
            "every cancelled batch call must still retain its dedicated backing",
            || client_pool.lock().stats().dedicated_segments >= BATCH,
            || {
                cancellation_state(
                    "batch retention",
                    seen_kinds.lock().len(),
                    client.pending_len_for_test(),
                    client.is_connected(),
                    &client_pool,
                    &budget,
                )
            },
        )
        .await;
        let (_, permits, workers) = crate::client::dedicated_retire_test_control::snapshot();
        assert!(permits >= BATCH, "retained permits: {permits}");
        assert!(
            workers <= 2,
            "retire workers must stay within the thread limit, got {workers}"
        );
        assert!(budget.snapshot().shm.used_bytes > 0);

        // read_done for all: every backing unmaps and every permit returns.
        for _ in 0..BATCH {
            let _ = release_tx.send(());
        }
        wait_until_observed(
            5,
            "every released batch backing must unmap after read_done",
            || client_pool.lock().stats().dedicated_segments == 0,
            || {
                cancellation_state(
                    "batch read_done",
                    seen_kinds.lock().len(),
                    client.pending_len_for_test(),
                    client.is_connected(),
                    &client_pool,
                    &budget,
                )
            },
        )
        .await;
        wait_until_observed(
            5,
            "every batch retention slot must return to its pool",
            || {
                crate::client::dedicated_retire_test_control::retention_jobs_for_pool(&client_pool)
                    == 0
            },
            || {
                let (jobs, permits, workers) =
                    crate::client::dedicated_retire_test_control::snapshot();
                format!(
                    "batch slots: pool_jobs={} pool_in_flight={} executor_jobs={jobs} \
                     executor_permits={permits} workers={workers}",
                    crate::client::dedicated_retire_test_control::retention_jobs_for_pool(
                        &client_pool
                    ),
                    crate::client::dedicated_retire_test_control::in_flight_jobs_for_pool(
                        &client_pool
                    ),
                )
            },
        )
        .await;
        wait_until_observed(
            5,
            "the batch memory budget must be fully returned",
            || budget.snapshot().shm.used_bytes == 0,
            || {
                cancellation_state(
                    "batch budget",
                    seen_kinds.lock().len(),
                    client.pending_len_for_test(),
                    client.is_connected(),
                    &client_pool,
                    &budget,
                )
            },
        )
        .await;

        client.close_shared().await;
        stop_server(&server).await;
    }

    /// Repeated reply-wait cancellations on one healthy connection must not
    /// accumulate pending waiters, and every handed-off dedicated backing must
    /// still retire once the peer reads — nothing retained, nothing stranded.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repeated_reply_wait_cancellation_never_accumulates_pending() {
        let _retire_guard = crate::client::dedicated_retire_test_control::production_guard();
        let (callback, seen_kinds, release_tx) = {
            let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
            let seen_kinds = Arc::new(Mutex::new(Vec::new()));
            (
                Arc::new(CountingStall {
                    seen_kinds: Arc::clone(&seen_kinds),
                    release: Mutex::new(release_rx),
                    finished: std::sync::atomic::AtomicUsize::new(0),
                }),
                seen_kinds,
                release_tx,
            )
        };
        let callback: Arc<dyn CrmCallback> = callback;
        let base = BaseIpcConfig {
            pool_enabled: false,
            chunk_size: 1 << 20,
            ..small_base(64 * 1024, 2)
        };
        // One execution slot per round: the earlier rounds' callbacks are still
        // stalled when the next request arrives (the `64` below is the server's
        // shm_threshold, not a worker count).
        const ROUNDS: usize = 6;
        let server = start_echo_server(
            "reply_wait_repeat",
            cancellation_server_config(base.clone(), 64, ROUNDS),
            callback,
        )
        .await;
        let mut client = IpcClient::with_config(server.ipc_address(), client_config(base, 8));
        client.connect().await.unwrap();
        let client_pool = client
            .request_pool()
            .expect("dedicated-capable request pool");
        let budget = client_pool
            .lock()
            .budget()
            .cloned()
            .expect("owner pool carries the domain budget");
        let binding = client
            .acquire_route(&expected_contract("reply_wait_repeat"))
            .await
            .unwrap();
        let client = Arc::new(client);

        for round in 0..ROUNDS {
            // A call that returns before its callback enters failed instead of
            // waiting; the diagnostic below reports that error verbatim.
            let call_error = Arc::new(Mutex::new(None::<String>));
            let task = {
                let client = Arc::clone(&client);
                let binding = binding.clone();
                let payload = vec![13u8; 4096];
                let call_error = Arc::clone(&call_error);
                tokio::spawn(async move {
                    let result = client.call_bound(&binding, "echo", &payload).await;
                    if let Err(error) = &result {
                        *call_error.lock() = Some(format!("{error:?}"));
                    }
                    result
                })
            };
            wait_until_observed(
                5,
                &format!("round {round} must reach the stalled callback"),
                || seen_kinds.lock().len() > round,
                || {
                    format!(
                        "{} call_outcome={}",
                        cancellation_state(
                            &format!("round {round} call_finished={}", task.is_finished()),
                            seen_kinds.lock().len(),
                            client.pending_len_for_test(),
                            client.is_connected(),
                            &client_pool,
                            &budget,
                        ),
                        match call_error.lock().as_deref() {
                            Some(error) => format!("failed with {error}"),
                            None => "still waiting".to_string(),
                        },
                    )
                },
            )
            .await;
            // The callback is stalled and has not read the request, so the
            // cancellation below happens in the reply wait of a call whose
            // frame the server already dispatched.
            task.abort();
            assert!(task.await.expect_err("aborted").is_cancelled());
            assert_eq!(
                client.pending_len_for_test(),
                0,
                "round {round} must leave no pending waiter on the healthy connection"
            );
            assert!(
                client.is_connected(),
                "round {round} must not tear down the healthy connection"
            );
        }

        // Every cancelled call handed its dedicated backing to the bounded
        // retire queue; all of them are still mapped and charged because the
        // peer has not read yet. The evidence is pool-local: the process-wide
        // permit count also carries permits owned by other, parallel tests.
        wait_until_observed(
            5,
            "every cancelled round must retain its dedicated backing",
            || client_pool.lock().stats().dedicated_segments >= ROUNDS,
            || {
                cancellation_state(
                    "round retention",
                    seen_kinds.lock().len(),
                    client.pending_len_for_test(),
                    client.is_connected(),
                    &client_pool,
                    &budget,
                )
            },
        )
        .await;
        let workers = crate::client::dedicated_retire_test_control::snapshot().2;
        assert!(workers <= 2, "retire workers stay bounded, got {workers}");
        assert!(budget.snapshot().shm.used_bytes > 0);

        // Unblock every stalled callback: each one reads its request and the
        // handed-off backings retire through read_done.
        for _ in 0..ROUNDS {
            let _ = release_tx.send(());
        }
        wait_until_observed(
            5,
            "every released round backing must unmap after read_done",
            || client_pool.lock().stats().dedicated_segments == 0,
            || {
                cancellation_state(
                    "round read_done",
                    seen_kinds.lock().len(),
                    client.pending_len_for_test(),
                    client.is_connected(),
                    &client_pool,
                    &budget,
                )
            },
        )
        .await;
        wait_until_observed(
            5,
            "every round retention slot must return to its pool",
            || {
                crate::client::dedicated_retire_test_control::retention_jobs_for_pool(&client_pool)
                    == 0
            },
            || {
                let (jobs, permits, workers) =
                    crate::client::dedicated_retire_test_control::snapshot();
                format!(
                    "round slots: pool_jobs={} pool_in_flight={} executor_jobs={jobs} \
                     executor_permits={permits} workers={workers}",
                    crate::client::dedicated_retire_test_control::retention_jobs_for_pool(
                        &client_pool
                    ),
                    crate::client::dedicated_retire_test_control::in_flight_jobs_for_pool(
                        &client_pool
                    ),
                )
            },
        )
        .await;
        wait_until_observed(
            5,
            "the round memory budget must be fully returned",
            || budget.snapshot().shm.used_bytes == 0,
            || {
                cancellation_state(
                    "round budget",
                    seen_kinds.lock().len(),
                    client.pending_len_for_test(),
                    client.is_connected(),
                    &client_pool,
                    &budget,
                )
            },
        )
        .await;

        // The connection is still fully usable after the cancellation storm.
        let roundtrip = vec![21u8; 4096];
        let task = {
            let client = Arc::clone(&client);
            let binding = binding.clone();
            let payload = roundtrip.clone();
            tokio::spawn(async move { client.call_bound(&binding, "echo", &payload).await })
        };
        let _ = release_tx.send(());
        let response = timeout(Duration::from_secs(5), task)
            .await
            .expect("the post-cancellation call must complete")
            .expect("call task must not panic")
            .expect("the healthy connection must still serve calls");
        assert_eq!(response_bytes(&client, response), roundtrip);
        assert_eq!(client.pending_len_for_test(), 0);

        client.close_shared().await;
        stop_server(&server).await;
    }
}

/// Chunked-response admission against the client's canonical reassembly
/// budget: rejections are correlated to the pending caller, control frames
/// keep progressing while the budget is fully consumed, and held backings
/// stay charged until release.
#[cfg(test)]
mod chunk_reply_admission_tests {
    use std::collections::HashMap;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use c2_local::LocalStream;
    use c2_mem::{MemPool, MemoryBudget, PoolConfig};
    use c2_wire::chunk::{ChunkConfig, ChunkRegistry, encode_reply_chunk_meta};
    use c2_wire::flags;
    use c2_wire::frame;
    use tokio::io::AsyncReadExt as _;
    use tokio::sync::oneshot;

    use crate::IpcError;
    use crate::client::{ChunkError, PendingResponse, recv_loop};
    use crate::response::ResponseData;

    const SIG_PING: u8 = 0x01;
    const SIG_PONG: u8 = 0x02;

    fn pool_config() -> PoolConfig {
        PoolConfig {
            segment_size: 64 * 1024,
            min_block_size: 4096,
            max_segments: 2,
            max_dedicated_segments: 2,
            dedicated_crash_timeout_secs: 0.0,
            buddy_idle_decay_secs: 0.0,
            spill_threshold: 1.0,
            spill_dir: std::env::temp_dir().join("c_two_reply_admission_test_spill"),
            ..PoolConfig::default()
        }
    }

    fn unique_prefix(label: &str) -> String {
        static NEXT_POOL: AtomicUsize = AtomicUsize::new(0);
        let sequence = NEXT_POOL.fetch_add(1, Ordering::Relaxed);
        format!("/c2qa{:06x}{:04x}{}", std::process::id(), sequence, label)
            .chars()
            .take(24)
            .collect()
    }

    fn budgeted_registry(reassembly_limit: u64, label: &str) -> (Arc<ChunkRegistry>, MemoryBudget) {
        let budget = MemoryBudget::new(1 << 20, 1 << 20, reassembly_limit);
        let pool = Arc::new(parking_lot::RwLock::new(
            MemPool::new_with_prefix_and_budget(
                pool_config(),
                unique_prefix(label),
                budget.clone(),
            ),
        ));
        (
            Arc::new(ChunkRegistry::new(pool, ChunkConfig::default())),
            budget,
        )
    }

    fn chunked_reply_frame(
        rid: u64,
        total_size: u64,
        total_chunks: u32,
        idx: u32,
        data: &[u8],
    ) -> Vec<u8> {
        let mut payload = encode_reply_chunk_meta(total_size, total_chunks, idx).to_vec();
        payload.extend_from_slice(data);
        frame::encode_frame(
            rid,
            flags::FLAG_RESPONSE | flags::FLAG_REPLY_V2 | flags::FLAG_CHUNKED,
            &payload,
        )
    }

    fn signal_frame(rid: u64, signal: u8) -> Vec<u8> {
        frame::encode_frame(rid, flags::FLAG_SIGNAL, &[signal])
    }

    /// Drive one `recv_loop` over a real local-transport pair with a probe
    /// read half for replies the loop writes (pongs).
    struct DrivenRecvLoop {
        pending: Arc<parking_lot::Mutex<HashMap<u32, PendingResponse>>>,
        probe_read: c2_local::LocalReadHalf,
        probe_write: c2_local::LocalWriteHalf,
        _handle: tokio::task::JoinHandle<()>,
    }

    async fn drive_recv_loop(registry: Arc<ChunkRegistry>) -> DrivenRecvLoop {
        let (client, server) = LocalStream::pair().await.unwrap();
        let (client_read, client_write) = client.into_split();
        let (probe_read, probe_write) = server.into_split();
        let pending: Arc<parking_lot::Mutex<HashMap<u32, PendingResponse>>> =
            Arc::new(parking_lot::Mutex::new(HashMap::new()));
        let loop_pending = pending.clone();
        let handle = tokio::spawn(async move {
            recv_loop(
                client_read,
                loop_pending,
                Arc::new(parking_lot::Mutex::new(None::<crate::ServerPoolState>)),
                Arc::new(tokio::sync::Mutex::new(Some(client_write))),
                registry,
                7,
            )
            .await
        });
        DrivenRecvLoop {
            pending,
            probe_read,
            probe_write,
            _handle: handle,
        }
    }

    fn register_pending(
        driven: &DrivenRecvLoop,
        rid: u32,
    ) -> oneshot::Receiver<Result<ResponseData, IpcError>> {
        let (tx, rx) = oneshot::channel();
        driven
            .pending
            .lock()
            .insert(rid, PendingResponse::unary(tx));
        rx
    }

    async fn read_exact_frame(probe: &mut c2_local::LocalReadHalf, len: usize) -> Vec<u8> {
        let mut buf = vec![0u8; len];
        tokio::time::timeout(Duration::from_secs(5), probe.read_exact(&mut buf))
            .await
            .expect("frame within timeout")
            .expect("probe read");
        buf
    }

    fn reassembly_used(budget: &MemoryBudget) -> u64 {
        budget
            .snapshot()
            .cell(c2_mem::budget::BudgetKind::Reassembly)
            .used_bytes
    }

    /// Wait (bounded) until the reassembly cell reports exactly `expected`
    /// live bytes, so the test observes recv_loop state rather than racing it.
    async fn wait_for_used_bytes(budget: &MemoryBudget, expected: u64) {
        for _ in 0..500 {
            if reassembly_used(budget) == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "reassembly cell did not reach {expected} bytes, observed {}",
            reassembly_used(budget)
        );
    }

    /// Well-encoded metadata can still describe invalid geometry. It must
    /// remain a protocol failure even when the reassembly budget is zero.
    #[tokio::test]
    async fn encoded_invalid_reply_geometry_is_protocol_and_ping_stays_usable() {
        let (registry, budget) = budgeted_registry(0, "g");
        let mut driven = drive_recv_loop(Arc::clone(&registry)).await;
        for (offset, (size, chunks, data_len)) in [
            (128, 0, 64),
            (0, 1, 1),
            (0, 2, 64),
            (128, 2, 0),
            (128, 1, 0),
            (128, 1, 129),
        ]
        .into_iter()
        .enumerate()
        {
            let rid = 100 + offset as u32;
            let receiver = register_pending(&driven, rid);
            driven
                .probe_write
                .write_all(&chunked_reply_frame(
                    u64::from(rid),
                    size,
                    chunks,
                    0,
                    &vec![0x42; data_len],
                ))
                .await
                .unwrap();
            let error = tokio::time::timeout(Duration::from_secs(5), receiver)
                .await
                .expect("invalid geometry completes its RID")
                .unwrap()
                .unwrap_err();
            assert!(
                matches!(error, IpcError::Chunk(ChunkError::Protocol(_))),
                "{error:?}"
            );
            assert!(!driven.pending.lock().contains_key(&rid));
            assert!(!registry.contains(7, rid as u64));
            assert_eq!(budget.snapshot().reassembly.rejected_allocations, 0);
            assert_eq!(reassembly_used(&budget), 0);
        }
        let ping = signal_frame(199, SIG_PING);
        driven.probe_write.write_all(&ping).await.unwrap();
        let pong = frame::encode_frame(199, flags::FLAG_RESPONSE | flags::FLAG_SIGNAL, &[SIG_PONG]);
        assert_eq!(
            read_exact_frame(&mut driven.probe_read, pong.len()).await,
            pong
        );
        // A different RID gets the actual capacity result, rather than a
        // sticky protocol rejection or a failed connection.
        for (offset, (chunks, len)) in [(2, 64), (1, 128)].into_iter().enumerate() {
            let rid = 200 + offset as u32;
            let receiver = register_pending(&driven, rid);
            driven
                .probe_write
                .write_all(&chunked_reply_frame(
                    u64::from(rid),
                    128,
                    chunks,
                    0,
                    &vec![0x33; len],
                ))
                .await
                .unwrap();
            let error = tokio::time::timeout(Duration::from_secs(5), receiver)
                .await
                .expect("capacity failure completes unrelated RID")
                .unwrap()
                .unwrap_err();
            assert!(matches!(error, IpcError::Chunk(ChunkError::Capacity(_))));
            assert!(!driven.pending.lock().contains_key(&rid));
            assert!(!registry.contains(7, u64::from(rid)));
            assert_eq!(
                budget.snapshot().reassembly.rejected_allocations,
                offset as u64 + 1
            );
        }
        assert_eq!(reassembly_used(&budget), 0);
        assert_eq!(registry.active_count(), 0);
        driven._handle.abort();
    }

    /// Malformed reply chunk metadata for a request with an in-flight assembly
    /// must complete exactly that pending caller with a useful error, release
    /// the charged assembly immediately (no GC or disconnect wait), and leave
    /// the connection usable for other replies and control frames.
    #[tokio::test]
    async fn malformed_later_chunk_metadata_aborts_assembly_and_keeps_connection_usable() {
        let (registry, budget) = budgeted_registry(1 << 20, "m");
        let mut driven = drive_recv_loop(Arc::clone(&registry)).await;

        // In-flight two-chunk assembly: 1024 bytes charged, caller waiting.
        let rx_first = register_pending(&driven, 11);
        driven
            .probe_write
            .write_all(&chunked_reply_frame(11, 1024, 2, 0, &[0x11u8; 512]))
            .await
            .unwrap();
        wait_for_used_bytes(&budget, 1024).await;
        assert!(registry.contains(7, 11), "assembly must be in flight");
        assert!(driven.pending.lock().contains_key(&11));

        // Malformed metadata (shorter than the 16-byte reply chunk meta) for
        // the same request id must not be ignored.
        let malformed = frame::encode_frame(
            11,
            flags::FLAG_RESPONSE | flags::FLAG_REPLY_V2 | flags::FLAG_CHUNKED,
            &[0u8; 8],
        );
        driven.probe_write.write_all(&malformed).await.unwrap();

        let error = tokio::time::timeout(Duration::from_secs(5), rx_first)
            .await
            .expect("malformed metadata must complete the correlated caller")
            .unwrap()
            .expect_err("malformed metadata must fail the call");
        match &error {
            IpcError::Chunk(ChunkError::Protocol(message)) => {
                assert!(message.contains("metadata"), "useful error: {message}");
            }
            other => panic!("expected chunk error, got {other:?}"),
        }
        // Immediate release: no GC sweep, no disconnect, no leftover caller.
        assert!(!driven.pending.lock().contains_key(&11));
        assert!(
            !registry.contains(7, 11),
            "aborted assembly must leave no registry entry"
        );
        assert_eq!(reassembly_used(&budget), 0);

        // Control frames and unrelated replies still work on this connection.
        let ping = signal_frame(21, SIG_PING);
        driven.probe_write.write_all(&ping).await.unwrap();
        let pong = frame::encode_frame(21, flags::FLAG_RESPONSE | flags::FLAG_SIGNAL, &[SIG_PONG]);
        assert_eq!(
            read_exact_frame(&mut driven.probe_read, pong.len()).await,
            pong
        );

        let rx_other = register_pending(&driven, 12);
        driven
            .probe_write
            .write_all(&chunked_reply_frame(12, 512, 1, 0, &[0x22u8; 512]))
            .await
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(5), rx_other)
            .await
            .expect("fresh reply within timeout")
            .unwrap()
            .unwrap();
        let ResponseData::Handle(mut backing) = response else {
            panic!("expected reassembled handle response");
        };
        assert_eq!(backing.copy_bytes().unwrap(), &[0x22u8; 512]);
        backing.release().unwrap();
        assert_eq!(reassembly_used(&budget), 0);

        driven._handle.abort();
    }

    /// A duplicated later chunk must abort the poisoned assembly, complete its
    /// caller and return the charge exactly once (a fresh assembly must be
    /// admitted immediately afterwards).
    #[tokio::test]
    async fn duplicate_later_chunk_aborts_assembly_and_releases_budget_once() {
        let (registry, budget) = budgeted_registry(1 << 20, "d");
        let mut driven = drive_recv_loop(Arc::clone(&registry)).await;

        let rx_duplicate = register_pending(&driven, 31);
        // Three 512-byte chunks: 1536 bytes charged.
        driven
            .probe_write
            .write_all(&chunked_reply_frame(31, 1536, 3, 0, &[0x31u8; 512]))
            .await
            .unwrap();
        wait_for_used_bytes(&budget, 1536).await;
        driven
            .probe_write
            .write_all(&chunked_reply_frame(31, 1536, 3, 1, &[0x32u8; 512]))
            .await
            .unwrap();
        // Same later chunk index again: rejected by the assembler.
        driven
            .probe_write
            .write_all(&chunked_reply_frame(31, 1536, 3, 1, &[0x33u8; 512]))
            .await
            .unwrap();

        let error = tokio::time::timeout(Duration::from_secs(5), rx_duplicate)
            .await
            .expect("duplicate chunk must complete the correlated caller")
            .unwrap()
            .expect_err("duplicate chunk must fail the call");
        match &error {
            IpcError::Chunk(ChunkError::Protocol(message)) => {
                assert!(message.contains("duplicate"), "useful error: {message}");
            }
            other => panic!("expected chunk error, got {other:?}"),
        }
        assert!(!registry.contains(7, 31));
        assert_eq!(reassembly_used(&budget), 0);

        // Exactly-once refund: a fresh assembly of the same size fits again.
        let rx_fresh = register_pending(&driven, 32);
        driven
            .probe_write
            .write_all(&chunked_reply_frame(32, 1024, 2, 0, &[0x34u8; 512]))
            .await
            .unwrap();
        wait_for_used_bytes(&budget, 1024).await;
        driven
            .probe_write
            .write_all(&chunked_reply_frame(32, 1024, 2, 1, &[0x35u8; 512]))
            .await
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(5), rx_fresh)
            .await
            .expect("fresh assembly within timeout")
            .unwrap()
            .unwrap();
        let ResponseData::Handle(mut backing) = response else {
            panic!("expected reassembled handle response");
        };
        assert_eq!(backing.len(), 1024);
        backing.release().unwrap();
        assert_eq!(reassembly_used(&budget), 0);

        driven._handle.abort();
    }

    /// A duplicated first chunk re-inserts over an in-flight assembly. The
    /// insert is rejected; the existing assembly must be released immediately
    /// (the caller is already failed) instead of staying charged until GC.
    #[tokio::test]
    async fn duplicate_first_chunk_aborts_existing_assembly_and_releases_budget_once() {
        let (registry, budget) = budgeted_registry(1024, "D");
        let mut driven = drive_recv_loop(Arc::clone(&registry)).await;

        let rx_duplicate = register_pending(&driven, 61);
        driven
            .probe_write
            .write_all(&chunked_reply_frame(61, 1024, 2, 0, &[0x61u8; 512]))
            .await
            .unwrap();
        wait_for_used_bytes(&budget, 1024).await;
        assert!(registry.contains(7, 61));
        // Second first-chunk frame for the same request id.
        driven
            .probe_write
            .write_all(&chunked_reply_frame(61, 1024, 2, 0, &[0x62u8; 512]))
            .await
            .unwrap();

        let error = tokio::time::timeout(Duration::from_secs(5), rx_duplicate)
            .await
            .expect("duplicate first chunk must complete the correlated caller")
            .unwrap()
            .expect_err("duplicate first chunk must fail the call");
        match &error {
            IpcError::Chunk(ChunkError::Protocol(message)) => {
                assert!(message.contains("duplicate"), "useful error: {message}");
            }
            other => panic!("expected chunk error, got {other:?}"),
        }
        assert!(
            !registry.contains(7, 61),
            "the abandoned assembly must be released, not stranded"
        );
        assert_eq!(reassembly_used(&budget), 0);

        // Duplicate rejection is decided before trying another reservation,
        // even when the existing assembly has filled the whole budget.
        assert_eq!(budget.snapshot().reassembly.rejected_allocations, 0);

        driven._handle.abort();
    }

    /// An oversized later chunk must abort the assembly, complete its caller
    /// and release the charge immediately.
    #[tokio::test]
    async fn oversized_later_chunk_aborts_assembly_and_releases_budget_once() {
        let (registry, budget) = budgeted_registry(1 << 20, "o");
        let mut driven = drive_recv_loop(Arc::clone(&registry)).await;

        let rx_oversized = register_pending(&driven, 41);
        // First chunk fixes chunk_size = 512 with three chunks total (1536).
        driven
            .probe_write
            .write_all(&chunked_reply_frame(41, 1536, 3, 0, &[0x41u8; 512]))
            .await
            .unwrap();
        wait_for_used_bytes(&budget, 1536).await;
        // 1024 bytes cannot fit the 512-byte chunk geometry.
        driven
            .probe_write
            .write_all(&chunked_reply_frame(41, 1536, 3, 1, &[0x42u8; 1024]))
            .await
            .unwrap();

        let error = tokio::time::timeout(Duration::from_secs(5), rx_oversized)
            .await
            .expect("oversized chunk must complete the correlated caller")
            .unwrap()
            .expect_err("oversized chunk must fail the call");
        match &error {
            IpcError::Chunk(ChunkError::Protocol(message)) => {
                assert!(
                    message.contains("exceeds chunk_size"),
                    "useful error: {message}"
                );
            }
            other => panic!("expected chunk error, got {other:?}"),
        }
        assert!(!registry.contains(7, 41));
        assert_eq!(reassembly_used(&budget), 0);

        // The connection and the budget remain usable for a fresh reply.
        let rx_fresh = register_pending(&driven, 42);
        driven
            .probe_write
            .write_all(&chunked_reply_frame(42, 1536, 3, 0, &[0x43u8; 512]))
            .await
            .unwrap();
        wait_for_used_bytes(&budget, 1536).await;
        assert!(registry.contains(7, 42));
        // Abort via the same release seam the caller-side lease uses.
        registry.abort(7, 42);
        assert_eq!(reassembly_used(&budget), 0);
        drop(rx_fresh);

        driven._handle.abort();
    }

    #[tokio::test]
    async fn admission_failure_is_correlated_and_ping_survives_exhausted_budget() {
        // Exactly one 1024-byte assembly fits.
        let (registry, budget) = budgeted_registry(1024, "a");
        let mut driven = drive_recv_loop(Arc::clone(&registry)).await;

        // (1) Fill the budget with one in-flight two-chunk assembly.
        let rx_fill = register_pending(&driven, 5);
        driven
            .probe_write
            .write_all(&chunked_reply_frame(5, 1024, 2, 0, &[0x55u8; 512]))
            .await
            .unwrap();
        // The pending entry must still be live (assembly incomplete).
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(driven.pending.lock().contains_key(&5));
        drop(rx_fill); // caller went away; the assembly stays in flight
        assert_eq!(
            budget
                .snapshot()
                .cell(c2_mem::budget::BudgetKind::Reassembly)
                .used_bytes,
            1024
        );

        // (2) A second, over-capacity chunked reply must produce a
        // correlated admission failure for its own pending caller, not a
        // hang or a warning.
        let rx_rejected = register_pending(&driven, 6);
        driven
            .probe_write
            .write_all(&chunked_reply_frame(6, 2048, 2, 0, &[0x66u8; 1024]))
            .await
            .unwrap();
        let rejection = tokio::time::timeout(Duration::from_secs(5), rx_rejected)
            .await
            .expect("admission failure correlated within timeout")
            .unwrap()
            .expect_err("over-capacity reply must be rejected");
        match &rejection {
            IpcError::Chunk(ChunkError::Capacity(message)) => {
                assert!(message.contains("'reassembly'"), "cell named: {message}");
                assert!(message.contains("2048"), "size named: {message}");
            }
            other => panic!("expected chunk admission error, got {other:?}"),
        }
        assert!(!driven.pending.lock().contains_key(&6));
        assert!(!registry.contains(7, 6));
        assert!(registry.contains(7, 5));
        // The other in-flight assembly keeps its own charge and identity.
        assert_eq!(reassembly_used(&budget), 1024);
        // The rejected reply left the budget untouched and no entry behind.
        assert_eq!(
            budget
                .snapshot()
                .cell(c2_mem::budget::BudgetKind::Reassembly)
                .rejected_allocations,
            1
        );

        // (3) Control frames keep progressing while the budget is fully
        // consumed: a ping probe still gets its pong.
        let ping = signal_frame(99, SIG_PING);
        let pong = frame::encode_frame(99, flags::FLAG_RESPONSE | flags::FLAG_SIGNAL, &[SIG_PONG]);
        driven.probe_write.write_all(&ping).await.unwrap();
        let observed = read_exact_frame(&mut driven.probe_read, pong.len()).await;
        assert_eq!(observed, pong);

        // (4) Complete the in-flight assembly: its pending caller receives
        // the owned backing with the right content, and the charge stays
        // held until that backing is released.
        let rx_fill = register_pending(&driven, 5);
        driven
            .probe_write
            .write_all(&chunked_reply_frame(5, 1024, 2, 1, &[0x77u8; 64]))
            .await
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(5), rx_fill)
            .await
            .expect("completion within timeout")
            .unwrap()
            .unwrap();
        let ResponseData::Handle(mut backing) = response else {
            panic!("expected reassembled handle response");
        };
        assert_eq!(backing.len(), 512 + 64);
        let payload = backing.copy_bytes().unwrap();
        assert_eq!(&payload[0..512], &[0x55u8; 512]);
        assert_eq!(&payload[512..576], &[0x77u8; 64]);
        // Trimmed but fully charged until release.
        assert_eq!(
            budget
                .snapshot()
                .cell(c2_mem::budget::BudgetKind::Reassembly)
                .used_bytes,
            1024
        );
        backing.release().unwrap();
        assert_eq!(
            budget
                .snapshot()
                .cell(c2_mem::budget::BudgetKind::Reassembly)
                .used_bytes,
            0
        );

        // (5) Freed budget admits a fresh reply again.
        let rx_again = register_pending(&driven, 8);
        driven
            .probe_write
            .write_all(&chunked_reply_frame(8, 512, 1, 0, &[0x88u8; 512]))
            .await
            .unwrap();
        let response = tokio::time::timeout(Duration::from_secs(5), rx_again)
            .await
            .expect("second completion within timeout")
            .unwrap()
            .unwrap();
        drop(response); // carrier drop releases storage and refunds.
        assert_eq!(reassembly_used(&budget), 0);
        assert_eq!(registry.active_count(), 0);

        driven._handle.abort();
    }
}
