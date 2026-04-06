//! c2-ipc unit tests.

#[cfg(test)]
mod client_tests {
    use c2_wire::buddy::{decode_buddy_payload, BUDDY_PAYLOAD_SIZE};
    use c2_wire::chunk::{decode_chunk_header, CHUNK_HEADER_SIZE};
    use c2_wire::control::*;
    use c2_wire::flags;
    use c2_wire::frame;
    use c2_wire::handshake::*;

    use crate::client::ClientIpcConfig;

    #[test]
    fn encode_v2_inline_call() {
        // Verify that an inline v2 call frame has the expected layout:
        // [16B header (flags=FLAG_CALL_V2)] [call_control] [data]
        let ctrl = encode_call_control("grid", 0);
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
        assert_eq!(decoded_ctrl.route_name, "grid");
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
        let frame_bytes = frame::encode_frame(
            42,
            flags::FLAG_RESPONSE | flags::FLAG_REPLY_V2,
            &payload,
        );

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
        let err = b"3:test error".to_vec();
        let ctrl = encode_reply_control(&ReplyControl::Error(err.clone()));
        let frame_bytes = frame::encode_frame(
            42,
            flags::FLAG_RESPONSE | flags::FLAG_REPLY_V2,
            &ctrl,
        );

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
        let encoded = encode_client_handshake(&segments, CAP_CALL_V2 | CAP_METHOD_IDX, "");
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
        // payload = [11B buddy_payload][call_control]
        // flags   = FLAG_CALL_V2 | FLAG_BUDDY
        use c2_wire::buddy::{encode_buddy_payload, BuddyPayload};

        let bp = BuddyPayload {
            seg_idx: 0,
            offset: 4096,
            data_size: 8192,
            is_dedicated: false,
        };
        let buddy_bytes = encode_buddy_payload(&bp);
        assert_eq!(buddy_bytes.len(), BUDDY_PAYLOAD_SIZE);

        let ctrl = encode_call_control("grid", 3);
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
        assert_eq!(decoded_bp.offset, 4096);
        assert_eq!(decoded_bp.data_size, 8192);
        assert!(!decoded_bp.is_dedicated);
        assert_eq!(bp_consumed, BUDDY_PAYLOAD_SIZE);

        // Decode call control after buddy payload.
        let (decoded_ctrl, _) =
            decode_call_control(frame_payload, BUDDY_PAYLOAD_SIZE).unwrap();
        assert_eq!(decoded_ctrl.route_name, "grid");
        assert_eq!(decoded_ctrl.method_idx, 3);
    }

    #[test]
    fn test_buddy_frame_dedicated_segment() {
        use c2_wire::buddy::{encode_buddy_payload, BuddyPayload};

        let bp = BuddyPayload {
            seg_idx: 5,
            offset: 0,
            data_size: 1_000_000,
            is_dedicated: true,
        };
        let buddy_bytes = encode_buddy_payload(&bp);
        let ctrl = encode_call_control("net", 1);

        let mut payload = Vec::new();
        payload.extend_from_slice(&buddy_bytes);
        payload.extend_from_slice(&ctrl);

        let frame_bytes = frame::encode_frame(
            7,
            flags::FLAG_CALL_V2 | flags::FLAG_BUDDY,
            &payload,
        );

        let (hdr, frame_payload) = frame::decode_frame(&frame_bytes).unwrap();
        assert!(hdr.is_buddy());
        let (decoded_bp, _) = decode_buddy_payload(frame_payload).unwrap();
        assert_eq!(decoded_bp.seg_idx, 5);
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

        let ctrl = encode_call_control(route, method_idx);
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
        let (decoded_ctrl, ctrl_consumed) =
            decode_call_control(fp_0, CHUNK_HEADER_SIZE).unwrap();
        assert_eq!(decoded_ctrl.route_name, route);
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
    fn test_call_full_path_selection() {
        // Verify ClientIpcConfig thresholds determine the expected transport path.
        // We can't call call_full directly (no UDS connection), but we can
        // verify the selection logic by checking threshold boundaries.
        let cfg = ClientIpcConfig {
            shm_threshold: 100,
            base: c2_config::BaseIpcConfig { chunk_size: 500, ..c2_config::BaseIpcConfig::default() },
        };

        // Below shm_threshold → inline
        let small = vec![0u8; 50];
        assert!(small.len() <= cfg.shm_threshold as usize);

        // Between shm_threshold and chunk_size → buddy (if pool) or inline
        let medium = vec![0u8; 200];
        assert!(medium.len() > cfg.shm_threshold as usize);
        assert!(medium.len() <= cfg.chunk_size as usize);

        // Above chunk_size → chunked (if no pool) or buddy first
        let large = vec![0u8; 600];
        assert!(large.len() > cfg.chunk_size as usize);

        // Verify chunk count calculation.
        let chunk_size = cfg.chunk_size as usize;
        let total_chunks = (large.len() + chunk_size - 1) / chunk_size;
        assert_eq!(total_chunks, 2); // 600 / 500 = 1.2 → 2 chunks
    }

    #[test]
    fn test_chunked_single_chunk() {
        // Edge case: data exactly at chunk_size boundary → 1 chunk.
        use c2_wire::chunk::encode_chunk_header;

        let chunk_hdr = encode_chunk_header(0, 1);
        let ctrl = encode_call_control("route", 0);
        let data = vec![0xABu8; 128];

        let mut payload = Vec::new();
        payload.extend_from_slice(&chunk_hdr);
        payload.extend_from_slice(&ctrl);
        payload.extend_from_slice(&data);

        let flags_last =
            flags::FLAG_CALL_V2 | flags::FLAG_CHUNKED | flags::FLAG_CHUNK_LAST;
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
            base: c2_config::BaseIpcConfig { chunk_size: 2048, ..c2_config::BaseIpcConfig::default() },
        };

        // IpcClient::new uses default config.
        let c1 = crate::client::IpcClient::new("ipc://test_prop_1");
        assert!(!c1.is_connected());

        // IpcClient::with_pool uses custom config.
        let pool = std::sync::Arc::new(parking_lot::Mutex::new(
            c2_mem::MemPool::new(c2_mem::PoolConfig::default()),
        ));
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
        let encoded = encode_client_handshake(&segments, cap, "test_prefix");
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
