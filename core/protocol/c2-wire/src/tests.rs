//! Unit tests for c2-wire codec.
//!
//! Tests verify round-trip encoding/decoding and canonical cross-language
//! wire compatibility fixtures.

mod frame_tests {
    use crate::flags::*;
    use crate::frame::*;

    #[test]
    fn encode_decode_roundtrip() {
        let payload = b"hello world";
        let encoded = encode_frame(42, FLAG_BUDDY | FLAG_CALL_V2, payload);
        assert_eq!(encoded.len(), HEADER_SIZE + payload.len());

        let (hdr, decoded_payload) = decode_frame(&encoded).unwrap();
        assert_eq!(hdr.request_id, 42);
        assert_eq!(hdr.flags, FLAG_BUDDY | FLAG_CALL_V2);
        assert_eq!(decoded_payload, payload);
        assert!(hdr.is_buddy());
        assert!(hdr.is_call_v2());
        assert!(!hdr.is_response());
    }

    #[test]
    fn encode_decode_empty_payload() {
        let encoded = encode_frame(0, 0, &[]);
        let (hdr, payload) = decode_frame(&encoded).unwrap();
        assert_eq!(hdr.request_id, 0);
        assert_eq!(hdr.flags, 0);
        assert!(payload.is_empty());
        assert_eq!(hdr.total_len, 12); // 8B rid + 4B flags
    }

    #[test]
    fn decode_total_len_basic() {
        let buf = 42u32.to_le_bytes();
        let (total_len, rest) = decode_total_len(&buf).unwrap();
        assert_eq!(total_len, 42);
        assert!(rest.is_empty());
    }

    #[test]
    fn decode_truncated() {
        let encoded = encode_frame(1, 0, b"data");
        // Truncate the frame
        let result = decode_frame(&encoded[..10]);
        assert!(result.is_err());
    }

    #[test]
    fn header_predicates() {
        let hdr = FrameHeader {
            total_len: 12,
            request_id: 1,
            flags: FLAG_RESPONSE | FLAG_REPLY_V2 | FLAG_BUDDY,
        };
        assert!(hdr.is_response());
        assert!(hdr.is_reply_v2());
        assert!(hdr.is_buddy());
        assert!(!hdr.is_call_v2());
        assert!(!hdr.is_handshake());
        assert!(!hdr.is_ctrl());
    }

    #[test]
    fn total_len_matches_canonical_frame_format() {
        // Canonical frame header layout: `<IQI` → 16 bytes
        // total_len = 12 + payload_len
        // Frame = [4B total_len][8B rid][4B flags][payload]
        let payload = b"test";
        let encoded = encode_frame(100, FLAG_CALL_V2, payload);

        // Check total_len value
        let total_len = u32::from_le_bytes([encoded[0], encoded[1], encoded[2], encoded[3]]);
        assert_eq!(total_len, 12 + 4); // 8 + 4 + payload_len

        // Check request_id
        let rid = u64::from_le_bytes([
            encoded[4],
            encoded[5],
            encoded[6],
            encoded[7],
            encoded[8],
            encoded[9],
            encoded[10],
            encoded[11],
        ]);
        assert_eq!(rid, 100);

        // Check flags
        let flags = u32::from_le_bytes([encoded[12], encoded[13], encoded[14], encoded[15]]);
        assert_eq!(flags, FLAG_CALL_V2);
    }
}

mod buddy_tests {
    use crate::buddy::*;
    use crate::frame::DecodeError;

    #[test]
    fn roundtrip() {
        let bp = BuddyPayload {
            seg_idx: 3,
            generation: 7,
            offset: 65536,
            data_size: 1024,
            is_dedicated: false,
        };
        let encoded = encode_buddy_payload(&bp);
        assert_eq!(encoded.len(), BUDDY_PAYLOAD_SIZE);

        let (decoded, consumed) = decode_buddy_payload(&encoded).unwrap();
        assert_eq!(consumed, BUDDY_PAYLOAD_SIZE);
        assert_eq!(decoded, bp);
    }

    #[test]
    fn dedicated_flag() {
        let bp = BuddyPayload {
            seg_idx: 0,
            generation: 0,
            offset: 0,
            data_size: 256,
            is_dedicated: true,
        };
        let encoded = encode_buddy_payload(&bp);
        assert_eq!(encoded[14], BUDDY_FLAG_DEDICATED);

        let (decoded, _) = decode_buddy_payload(&encoded).unwrap();
        assert!(decoded.is_dedicated);
        assert_eq!(decoded.generation, 0);
    }

    #[test]
    fn canonical_buddy_payload_layout() {
        // `<HIII B`: segment, generation, offset, size, flags. Distinct bytes
        // make field-order and truncation mistakes visible in this wire golden.
        assert_eq!(BUDDY_PAYLOAD_SIZE, 15);

        let bp = BuddyPayload {
            seg_idx: 0x0201,
            generation: 0x0605_0403,
            offset: 0x0a09_0807,
            data_size: 0x0e0d_0c0b,
            is_dedicated: false,
        };
        let encoded = encode_buddy_payload(&bp);
        assert_eq!(encoded, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 0]);
        assert_eq!(decode_buddy_payload(&encoded).unwrap(), (bp, 15));
    }

    #[test]
    fn generation_preserves_nonzero_u32_boundaries() {
        for generation in [1, u32::MAX] {
            let bp = BuddyPayload {
                seg_idx: u16::MAX,
                generation,
                offset: u32::MAX,
                data_size: u32::MAX,
                is_dedicated: false,
            };
            assert_eq!(
                decode_buddy_payload(&encode_buddy_payload(&bp)).unwrap(),
                (bp, 15)
            );
        }
    }

    #[test]
    fn decoder_rejects_zero_buddy_generation() {
        // Valid 15-byte shape, but only dedicated allocations may use generation 0.
        let bytes = [0_u8; 15];
        assert!(matches!(
            decode_buddy_payload(&bytes),
            Err(DecodeError::InvalidValue {
                field: "backing generation",
                value: 0
            })
        ));
    }

    #[test]
    fn decoder_rejects_nonzero_dedicated_generation() {
        for generation in [1_u32, u32::MAX] {
            let mut bytes = [0_u8; 15];
            bytes[2..6].copy_from_slice(&generation.to_le_bytes());
            bytes[14] = BUDDY_FLAG_DEDICATED;
            assert!(matches!(
                decode_buddy_payload(&bytes),
                Err(DecodeError::InvalidValue { field: "backing generation", value })
                    if value == u64::from(generation)
            ));
        }
    }

    #[test]
    fn decoder_rejects_unknown_flags() {
        for flags in [0x02_u8, 0x03, 0x80, 0xff] {
            let mut bytes = [0_u8; 15];
            bytes[2] = 1;
            bytes[14] = flags;
            assert!(matches!(
                decode_buddy_payload(&bytes),
                Err(DecodeError::InvalidValue { field: "buddy flags", value })
                    if value == u64::from(flags)
            ));
        }
    }
}

mod control_tests {
    use crate::control::*;

    const ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SIG_HASH: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    fn call_identity(route_name: &str) -> RouteCallIdentity {
        RouteCallIdentity {
            route_name: route_name.into(),
            route_uid: format!("{route_name}-uid-0001"),
            observed_route_revision: 1,
            crm_ns: "test.grid".into(),
            crm_name: "Grid".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: ABI_HASH.into(),
            signature_hash: SIG_HASH.into(),
        }
    }

    #[test]
    fn call_control_roundtrip() {
        let identity = call_identity("grid");
        let encoded = encode_call_control(&identity, 42).unwrap();
        let (decoded, consumed) = decode_call_control(&encoded, 0).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded.identity, identity);
        assert_eq!(decoded.method_idx, 42);
    }

    #[test]
    fn call_control_roundtrip_preserves_route_identity_and_contract() {
        let identity = RouteCallIdentity {
            route_name: "grid".into(),
            route_uid: "route-uid-0001".into(),
            observed_route_revision: 17,
            crm_ns: "test.grid".into(),
            crm_name: "Grid".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            signature_hash: "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
                .into(),
        };

        let encoded = encode_call_control(&identity, 42).unwrap();
        let (decoded, consumed) = decode_call_control(&encoded, 0).unwrap();

        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded.identity, identity);
        assert_eq!(decoded.method_idx, 42);
    }

    #[test]
    fn call_control_rejects_empty_route_name() {
        let mut identity = call_identity("grid");
        identity.route_name.clear();
        assert!(encode_call_control(&identity, 0).is_err());
        assert!(decode_call_control(&[0, 0, 0], 0).is_err());
    }

    #[test]
    fn call_control_with_offset() {
        let mut buf = vec![0xAA, 0xBB]; // prefix
        let identity = call_identity("net");
        buf.extend_from_slice(&encode_call_control(&identity, 7).unwrap());
        let (decoded, consumed) = decode_call_control(&buf, 2).unwrap();
        assert_eq!(decoded.identity, identity);
        assert_eq!(decoded.method_idx, 7);
        assert_eq!(consumed, buf.len() - 2);
    }

    #[test]
    fn encode_call_control_into_rejects_short_buffer_without_panic() {
        let identity = call_identity("grid");
        let len = encoded_call_control_len(&identity).unwrap();
        let mut buf = vec![0u8; len - 1];

        let err = encode_call_control_into(&mut buf, 0, &identity, 0)
            .expect_err("short call-control buffer must be reported");

        assert!(err.to_string().contains("buffer is too short"), "{err}");
    }

    #[test]
    fn reply_control_success_roundtrip() {
        let encoded = try_encode_reply_control(&ReplyControl::Success).unwrap();
        assert_eq!(encoded, &[STATUS_SUCCESS]);
        let (decoded, consumed) = decode_reply_control(&encoded, 0).unwrap();
        assert_eq!(consumed, 1);
        assert_eq!(decoded, ReplyControl::Success);
    }

    #[test]
    fn reply_control_error_roundtrip() {
        let err = br#"C2E1{"version":1,"code":3,"name":"ResourceFunctionExecuting","message":"test error","details":{}}"#.to_vec();
        let encoded = try_encode_reply_control(&ReplyControl::Error(err.clone())).unwrap();
        let (decoded, consumed) = decode_reply_control(&encoded, 0).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, ReplyControl::Error(err));
    }

    #[test]
    fn reply_control_error_empty_data() {
        let encoded = try_encode_reply_control(&ReplyControl::Error(vec![])).unwrap();
        // status=1, error_len=0 → [0x01, 0x00, 0x00, 0x00, 0x00]
        assert_eq!(encoded.len(), 5);
        let (decoded, consumed) = decode_reply_control(&encoded, 0).unwrap();
        assert_eq!(consumed, 5);
        assert_eq!(decoded, ReplyControl::Error(vec![]));
    }

    #[test]
    fn reply_control_route_not_found_roundtrip() {
        let encoded =
            try_encode_reply_control(&ReplyControl::RouteNotFound("grid".into())).unwrap();
        assert_eq!(encoded[0], STATUS_ROUTE_NOT_FOUND);
        let (decoded, consumed) = decode_reply_control(&encoded, 0).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded, ReplyControl::RouteNotFound("grid".into()));
    }

    #[test]
    fn reply_control_route_not_found_rejects_invalid_route_text() {
        let err = try_encode_reply_control(&ReplyControl::RouteNotFound("grid\0hidden".into()))
            .expect_err("invalid route text must not be encoded");
        assert!(err.to_string().contains("route_name"), "{err}");
    }

    #[test]
    fn reply_control_route_not_found_rejects_truncated_name() {
        let mut encoded = vec![STATUS_ROUTE_NOT_FOUND];
        encoded.extend_from_slice(&8u32.to_le_bytes());
        encoded.extend_from_slice(b"grid");

        assert!(decode_reply_control(&encoded, 0).is_err());
    }

    #[test]
    fn reply_control_invalid_status() {
        let buf = [0xFF];
        let result = decode_reply_control(&buf, 0);
        assert!(result.is_err());
    }

    #[test]
    fn canonical_call_control_fixture_matches() {
        let identity = call_identity("grid");
        let mut expected = Vec::new();
        expected.extend_from_slice(b"\x04grid");
        expected.extend_from_slice(b"\x0dgrid-uid-0001");
        expected.extend_from_slice(&1u64.to_le_bytes());
        expected.extend_from_slice(b"\x09test.grid");
        expected.extend_from_slice(b"\x04Grid");
        expected.extend_from_slice(b"\x050.1.0");
        expected.push(64);
        expected.extend_from_slice(ABI_HASH.as_bytes());
        expected.push(64);
        expected.extend_from_slice(SIG_HASH.as_bytes());
        expected.extend_from_slice(&5u16.to_le_bytes());

        let encoded = encode_call_control(&identity, 5).unwrap();
        assert_eq!(encoded, expected);
    }

    #[test]
    fn call_control_rejects_route_name_longer_than_one_byte_length() {
        let long_name = "x".repeat(c2_contract::MAX_WIRE_TEXT_BYTES + 1);
        let mut identity = call_identity("grid");
        identity.route_name = long_name;
        let err = encode_call_control(&identity, 0).unwrap_err();
        assert!(err.to_string().contains("route_name"));
    }
}

mod route_catalog_control_tests {
    use crate::route_catalog_control::*;
    use c2_error::{C2Error, ErrorCode};

    const ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SIG_HASH: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    fn contract(route_name: &str) -> RouteContractWire {
        RouteContractWire {
            route_name: route_name.into(),
            crm_ns: "test.grid".into(),
            crm_name: "Grid".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: ABI_HASH.into(),
            signature_hash: SIG_HASH.into(),
        }
    }

    fn record(route_name: &str, revision: u64) -> RouteRecordWire {
        RouteRecordWire {
            route_name: route_name.into(),
            route_uid: format!("{route_name}-uid-0001"),
            route_revision: revision,
            catalog_revision: revision,
            owner_server_id: "server-grid".into(),
            owner_server_instance_id: "instance-grid-0001".into(),
            owner_epoch: 1,
            contract: contract(route_name),
            methods: vec![
                RouteMethodWire {
                    name: "step".into(),
                    index: 0,
                },
                RouteMethodWire {
                    name: "query".into(),
                    index: 1,
                },
            ],
            max_payload_size: 1024,
            state: RouteStateWire::Ready,
            state_reason: Some(RouteStateReasonWire::RegisterCommitted),
            lease_deadline_ms: None,
        }
    }

    #[test]
    fn route_list_request_matches_canonical_fixture() {
        let request = RouteListRequest {
            selector: RouteSelector::All,
            min_revision: None,
        };
        let encoded = encode_route_list_request(&request).unwrap();
        let expected = b"\x0e{\"selector\":{\"type\":\"all\"},\"min_revision\":null}".to_vec();

        assert_eq!(encoded, expected);
        assert_eq!(decode_route_list_request(&expected).unwrap(), request);
    }

    #[test]
    fn route_lookup_stale_response_round_trips_current_record() {
        let current = record("grid", 3);
        let response = RouteLookupResponse::Stale {
            current: current.clone(),
        };
        let encoded = encode_route_lookup_response(&response).unwrap();
        let decoded = decode_route_lookup_response(&encoded).unwrap();

        assert_eq!(decoded, response);
        match decoded {
            RouteLookupResponse::Stale {
                current: decoded_current,
            } => {
                assert_eq!(decoded_current.route_uid, "grid-uid-0001");
                assert_eq!(decoded_current.route_revision, 3);
                assert_eq!(decoded_current.contract.abi_hash, ABI_HASH);
            }
            other => panic!("expected stale response, got {other:?}"),
        }
    }

    #[test]
    fn route_watch_compacted_event_is_history_boundary_not_removal() {
        let event = RouteWatchEvent::Compacted {
            compacted_revision: 7,
            current_revision: 11,
        };
        let encoded = encode_route_watch_event(&event).unwrap();
        let expected =
            b"\x13{\"event\":\"compacted\",\"compacted_revision\":7,\"current_revision\":11}"
                .to_vec();

        assert_eq!(encoded, expected);
        assert_eq!(decode_route_watch_event(&expected).unwrap(), event);
    }

    #[test]
    fn route_nack_carries_registered_c2_error_envelope() {
        let error =
            C2Error::new(ErrorCode::RouteCatalogCompacted, "watch history compacted").envelope();
        let nack = RouteNack {
            nonce: 55,
            rejected_revision: 9,
            error,
        };
        let encoded = encode_route_nack(&nack).unwrap();
        let decoded = decode_route_nack(&encoded).unwrap();

        assert_eq!(decoded, nack);
        assert_eq!(decoded.error.name, "RouteCatalogCompacted");
        assert_eq!(decoded.error.code, 711);
    }

    #[test]
    fn route_lookup_request_requires_complete_observed_token() {
        let request = RouteLookupRequest {
            expected: contract("grid"),
            observed_route_uid: Some("grid-uid-0001".into()),
            observed_route_revision: None,
        };

        let err = encode_route_lookup_request(&request)
            .expect_err("observed route token must include uid and revision together");

        assert!(err.contains("observed route token"), "{err}");
    }

    #[test]
    fn route_list_response_rejects_duplicate_routes() {
        let response = RouteListResponse {
            catalog_revision: 2,
            min_watch_revision: 1,
            routes: vec![record("grid", 1), record("grid", 2)],
        };

        let err = encode_route_list_response(&response)
            .expect_err("duplicate route names must be rejected");

        assert!(err.contains("duplicate route_name"), "{err}");
    }

    #[test]
    fn route_record_rejects_contract_route_name_mismatch() {
        let mut current = record("grid", 1);
        current.contract.route_name = "other-grid".into();
        let response = RouteLookupResponse::Ready { current };

        let err = encode_route_lookup_response(&response)
            .expect_err("record contract route_name must match route_name");

        assert!(err.contains("contract route_name"), "{err}");
    }

    #[test]
    fn route_nack_rejects_error_code_name_mismatch() {
        let mut error =
            C2Error::new(ErrorCode::RouteCatalogCompacted, "watch history compacted").envelope();
        error.name = "ResourceUnavailable".into();
        let nack = RouteNack {
            nonce: 55,
            rejected_revision: 9,
            error,
        };

        let err = encode_route_nack(&nack).expect_err("error envelope code/name mismatch fails");

        assert!(err.contains("name mismatch"), "{err}");
    }
}

mod handshake_tests {
    use crate::handshake::*;

    const ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SIG_HASH: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    fn test_identity() -> ServerIdentity {
        ServerIdentity {
            server_id: "test-server".to_string(),
            server_instance_id: "test-instance".to_string(),
        }
    }

    fn route(name: &str, methods: &[&str]) -> RouteInfo {
        RouteInfo {
            name: name.into(),
            route_uid: format!("{name}-route-uid-0001"),
            route_revision: 1,
            crm_ns: "test.grid".into(),
            crm_name: "Grid".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: ABI_HASH.into(),
            signature_hash: SIG_HASH.into(),
            max_payload_size: 1024,
            methods: methods
                .iter()
                .enumerate()
                .map(|(index, method)| MethodEntry {
                    name: (*method).into(),
                    index: index as u16,
                })
                .collect(),
        }
    }

    fn route_hash(name: &str) -> RouteInfo {
        RouteInfo {
            name: name.into(),
            route_uid: format!("{name}-route-uid-0001"),
            route_revision: 1,
            crm_ns: "test.grid".into(),
            crm_name: "Grid".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: ABI_HASH.into(),
            signature_hash: SIG_HASH.into(),
            max_payload_size: 1024,
            methods: vec![MethodEntry {
                name: "get".into(),
                index: 0,
            }],
        }
    }

    #[test]
    fn route_hash_fields_roundtrip_through_server_handshake() {
        let routes = vec![route_hash("grid")];
        let encoded = encode_server_handshake(&[], CAP_CALL_V2, &routes, "", &test_identity())
            .expect("hash-bearing route encodes");
        let decoded = decode_handshake(&encoded).expect("hash-bearing route decodes");

        assert_eq!(decoded.routes[0].abi_hash, ABI_HASH);
        assert_eq!(decoded.routes[0].signature_hash, SIG_HASH);
    }

    #[test]
    fn route_hash_encode_rejects_malformed_hashes() {
        let mut route = route_hash("grid");
        route.abi_hash = "ABCDEF".into();
        let err = encode_server_handshake(&[], CAP_CALL_V2, &[route], "", &test_identity())
            .expect_err("uppercase or short ABI hash must fail");
        assert!(err.to_string().contains("abi_hash"));

        let mut route = route_hash("grid");
        route.signature_hash = "not-a-sha256".into();
        let err = encode_server_handshake(&[], CAP_CALL_V2, &[route], "", &test_identity())
            .expect_err("malformed signature hash must fail");
        assert!(err.to_string().contains("signature_hash"));
    }

    #[test]
    fn route_hash_decode_rejects_malformed_hashes() {
        let route = route_hash("grid");
        let mut encoded = encode_server_handshake(&[], CAP_CALL_V2, &[route], "", &test_identity())
            .expect("valid handshake encodes before byte mutation");

        let hash_pos = encoded
            .windows(ABI_HASH.len())
            .position(|window| window == ABI_HASH.as_bytes())
            .expect("encoded ABI hash should be present");
        encoded[hash_pos] = b'X';

        let err = decode_handshake(&encoded).expect_err("decode must reject malformed ABI hash");
        assert!(err.to_string().contains("abi_hash"));
    }

    #[test]
    fn backing_generation_bumps_handshake_version() {
        assert_eq!(HANDSHAKE_VERSION, 11);

        let client = encode_client_handshake(&[], CAP_CALL_V2, "")
            .expect("current client handshake encodes");
        let server = encode_server_handshake(
            &[("seg0".into(), 4096)],
            CAP_CALL_V2,
            &[route_hash("grid")],
            "test-prefix",
            &test_identity(),
        )
        .expect("current server handshake encodes");
        for version in [9_u8, 10] {
            for mut encoded in [client.clone(), server.clone()] {
                encoded[0] = version;
                assert!(matches!(
                    decode_handshake(&encoded),
                    Err(crate::frame::DecodeError::InvalidValue { field: "handshake version", value })
                        if value == u64::from(version)
                ));
            }
        }
    }

    #[test]
    fn crm_tag_validator_accepts_normal_contract_identity() {
        c2_contract::validate_crm_tag("test.grid", "Grid", "0.1.0").unwrap();
    }

    #[test]
    fn crm_tag_validator_rejects_malformed_fields() {
        let too_long = "x".repeat(c2_contract::MAX_WIRE_TEXT_BYTES + 1);
        for (crm_ns, crm_name, crm_ver, needle) in [
            ("", "Grid", "0.1.0", "cannot be empty"),
            ("test.grid", "", "0.1.0", "cannot be empty"),
            ("test.grid", "Grid", "", "cannot be empty"),
            (
                " test.grid",
                "Grid",
                "0.1.0",
                "leading or trailing whitespace",
            ),
            ("test.grid", "Grid\nInjected", "0.1.0", "control characters"),
            ("test/grid", "Grid", "0.1.0", "path or tag separators"),
            ("test.grid", "Bad\\Grid", "0.1.0", "path or tag separators"),
            ("test.grid", too_long.as_str(), "0.1.0", "cannot exceed"),
        ] {
            let err = c2_contract::validate_crm_tag(crm_ns, crm_name, crm_ver)
                .expect_err("malformed CrmTag field must fail");
            let err = err.to_string();
            assert!(err.contains(needle), "expected {needle:?}, got {err:?}");
        }
    }

    #[test]
    fn server_handshake_encode_rejects_invalid_crm_tag_fields() {
        let mut route = route("grid", &["get"]);
        route.crm_name = "Grid\0Injected".into();

        let err = encode_server_handshake(&[], CAP_CALL_V2, &[route], "", &test_identity())
            .expect_err("invalid CrmTag must fail during encode");

        assert!(err.to_string().contains("crm name"));
        assert!(err.to_string().contains("control characters"));
    }

    #[test]
    fn server_handshake_decode_rejects_invalid_crm_tag_fields() {
        let route = RouteInfo {
            name: "grid".into(),
            route_uid: "grid-route-uid-0001".into(),
            route_revision: 1,
            crm_ns: "test.grid".into(),
            crm_name: "Grid".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: ABI_HASH.into(),
            signature_hash: SIG_HASH.into(),
            max_payload_size: 1024,
            methods: vec![MethodEntry {
                name: "get".into(),
                index: 0,
            }],
        };
        let identity = test_identity();
        let mut encoded = encode_server_handshake(&[], CAP_CALL_V2, &[route], "", &identity)
            .expect("valid handshake encodes before byte-level mutation");

        let grid_pos = encoded
            .windows("Grid".len())
            .position(|window| window == b"Grid")
            .expect("encoded CRM name should be present");
        encoded[grid_pos + 1] = b'\n';

        let err = decode_handshake(&encoded).expect_err("decode must reject malformed CrmTag");
        assert!(err.to_string().contains("crm name"));
        assert!(err.to_string().contains("control characters"));
    }

    #[test]
    fn client_handshake_roundtrip() {
        let segments = vec![
            ("seg0".into(), 268_435_456u32),
            ("seg1".into(), 268_435_456u32),
        ];
        let encoded = encode_client_handshake(&segments, CAP_CALL_V2 | CAP_METHOD_IDX, "").unwrap();
        let decoded = decode_handshake(&encoded).unwrap();

        assert_eq!(decoded.prefix, "");
        assert_eq!(decoded.segments.len(), 2);
        assert_eq!(decoded.segments[0].0, "seg0");
        assert_eq!(decoded.segments[0].1, 268_435_456);
        assert_eq!(decoded.capability_flags, CAP_CALL_V2 | CAP_METHOD_IDX);
        assert_eq!(decoded.server_identity, None);
        assert!(decoded.routes.is_empty());
    }

    #[test]
    fn server_handshake_roundtrip_includes_server_identity() {
        let routes = vec![RouteInfo {
            name: "grid".to_string(),
            route_uid: "grid-route-uid-0001".to_string(),
            route_revision: 1,
            crm_ns: "test.grid".to_string(),
            crm_name: "Grid".to_string(),
            crm_ver: "1.2.3".to_string(),
            abi_hash: ABI_HASH.to_string(),
            signature_hash: SIG_HASH.to_string(),
            max_payload_size: 1024,
            methods: vec![MethodEntry {
                name: "ping".to_string(),
                index: 0,
            }],
        }];
        let identity = ServerIdentity {
            server_id: "grid-server".to_string(),
            server_instance_id: "inst-001".to_string(),
        };

        let encoded = encode_server_handshake(
            &[],
            CAP_CALL_V2 | CAP_METHOD_IDX,
            &routes,
            "/cc3rtest",
            &identity,
        )
        .expect("server handshake encodes");
        let decoded = decode_handshake(&encoded).expect("server handshake decodes");

        assert_eq!(decoded.server_identity.as_ref(), Some(&identity));
        assert_eq!(decoded.routes, routes);
        assert_eq!(decoded.routes[0].crm_ns, "test.grid");
        assert_eq!(decoded.routes[0].crm_name, "Grid");
        assert_eq!(decoded.routes[0].crm_ver, "1.2.3");
    }

    #[test]
    fn server_handshake_rejects_trailing_bytes() {
        let mut encoded = encode_server_handshake(
            &[],
            CAP_CALL_V2 | CAP_METHOD_IDX,
            &[],
            "/cc3rtest",
            &test_identity(),
        )
        .expect("server handshake encodes");
        encoded.push(0xff);

        let err = decode_handshake(&encoded).expect_err("trailing byte must be rejected");
        assert!(err.to_string().contains("trailing bytes"));
    }

    #[test]
    fn client_handshake_has_no_server_identity() {
        let encoded = encode_client_handshake(&[], CAP_CALL_V2, "/cc3ctest")
            .expect("client handshake encodes");
        let decoded = decode_handshake(&encoded).expect("client handshake decodes");

        assert_eq!(decoded.server_identity, None);
        assert!(decoded.routes.is_empty());
    }

    #[test]
    fn server_handshake_roundtrip() {
        let segments = vec![("srv_seg0".into(), 134_217_728u32)];
        let routes = vec![
            RouteInfo {
                name: "grid".into(),
                route_uid: "grid-route-uid-0001".into(),
                route_revision: 1,
                crm_ns: "test.grid".into(),
                crm_name: "Grid".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: ABI_HASH.into(),
                signature_hash: SIG_HASH.into(),
                max_payload_size: 1024,
                methods: vec![
                    MethodEntry {
                        name: "hello".into(),
                        index: 0,
                    },
                    MethodEntry {
                        name: "subdivide_grids".into(),
                        index: 1,
                    },
                    MethodEntry {
                        name: "get_grid_infos".into(),
                        index: 2,
                    },
                ],
            },
            RouteInfo {
                name: "counter".into(),
                route_uid: "counter-route-uid-0001".into(),
                route_revision: 1,
                crm_ns: "test.counter".into(),
                crm_name: "Counter".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: ABI_HASH.into(),
                signature_hash: SIG_HASH.into(),
                max_payload_size: 1024,
                methods: vec![
                    MethodEntry {
                        name: "get".into(),
                        index: 0,
                    },
                    MethodEntry {
                        name: "increment".into(),
                        index: 1,
                    },
                ],
            },
        ];
        let identity = test_identity();
        let encoded =
            encode_server_handshake(&segments, CAP_CALL_V2, &routes, "", &identity).unwrap();
        let decoded = decode_handshake(&encoded).unwrap();

        assert_eq!(decoded.segments.len(), 1);
        assert_eq!(decoded.capability_flags, CAP_CALL_V2);
        assert_eq!(decoded.server_identity.as_ref(), Some(&identity));
        assert_eq!(decoded.routes.len(), 2);

        let grid = &decoded.routes[0];
        assert_eq!(grid.name, "grid");
        assert_eq!(grid.methods.len(), 3);
        assert_eq!(grid.methods[0].name, "hello");
        assert_eq!(grid.methods[0].index, 0);
        assert_eq!(grid.methods[2].name, "get_grid_infos");
        assert_eq!(grid.methods[2].index, 2);

        let counter = &decoded.routes[1];
        assert_eq!(counter.name, "counter");
        assert_eq!(counter.methods.len(), 2);
    }

    #[test]
    fn server_handshake_rejects_overlong_route_name() {
        let routes = vec![RouteInfo {
            name: "x".repeat(c2_contract::MAX_WIRE_TEXT_BYTES + 1),
            route_uid: "overlong-route-uid-0001".into(),
            route_revision: 1,
            crm_ns: "test.overlong".into(),
            crm_name: "Overlong".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: ABI_HASH.into(),
            signature_hash: SIG_HASH.into(),
            max_payload_size: 1024,
            methods: vec![],
        }];

        let err =
            encode_server_handshake(&[], CAP_CALL_V2, &routes, "", &test_identity()).unwrap_err();
        assert!(err.to_string().contains("route name"));
    }

    #[test]
    fn server_handshake_rejects_overlong_crm_metadata() {
        let routes = vec![RouteInfo {
            name: "grid".into(),
            route_uid: "grid-route-uid-0001".into(),
            route_revision: 1,
            crm_ns: "x".repeat(c2_contract::MAX_WIRE_TEXT_BYTES + 1),
            crm_name: "Grid".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: ABI_HASH.into(),
            signature_hash: SIG_HASH.into(),
            max_payload_size: 1024,
            methods: vec![],
        }];

        let err =
            encode_server_handshake(&[], CAP_CALL_V2, &routes, "", &test_identity()).unwrap_err();
        assert!(err.to_string().contains("crm namespace"));

        let routes = vec![RouteInfo {
            name: "grid".into(),
            route_uid: "grid-route-uid-0001".into(),
            route_revision: 1,
            crm_ns: "test.grid".into(),
            crm_name: "x".repeat(c2_contract::MAX_WIRE_TEXT_BYTES + 1),
            crm_ver: "0.1.0".into(),
            abi_hash: ABI_HASH.into(),
            signature_hash: SIG_HASH.into(),
            max_payload_size: 1024,
            methods: vec![],
        }];

        let err =
            encode_server_handshake(&[], CAP_CALL_V2, &routes, "", &test_identity()).unwrap_err();
        assert!(err.to_string().contains("crm name"));

        let routes = vec![RouteInfo {
            name: "grid".into(),
            route_uid: "grid-route-uid-0001".into(),
            route_revision: 1,
            crm_ns: "test.grid".into(),
            crm_name: "Grid".into(),
            crm_ver: "x".repeat(c2_contract::MAX_WIRE_TEXT_BYTES + 1),
            abi_hash: ABI_HASH.into(),
            signature_hash: SIG_HASH.into(),
            max_payload_size: 1024,
            methods: vec![],
        }];

        let err =
            encode_server_handshake(&[], CAP_CALL_V2, &routes, "", &test_identity()).unwrap_err();
        assert!(err.to_string().contains("crm version"));
    }

    #[test]
    fn server_handshake_rejects_too_many_methods() {
        let routes = vec![RouteInfo {
            name: "grid".into(),
            route_uid: "grid-route-uid-0001".into(),
            route_revision: 1,
            crm_ns: "test.grid".into(),
            crm_name: "Grid".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: ABI_HASH.into(),
            signature_hash: SIG_HASH.into(),
            max_payload_size: 1024,
            methods: (0..=MAX_METHODS)
                .map(|i| MethodEntry {
                    name: format!("m{i}"),
                    index: i as u16,
                })
                .collect(),
        }];

        let err =
            encode_server_handshake(&[], CAP_CALL_V2, &routes, "", &test_identity()).unwrap_err();
        assert!(err.to_string().contains("method count"));
    }

    #[test]
    fn wrong_version() {
        let buf = [4, 0, 0]; // version 4
        let result = decode_handshake(&buf);
        assert!(result.is_err());
    }

    #[test]
    fn empty_handshake() {
        // Version 11, prefix_len=0, 0 segments, cap_flags=0
        let buf = [11, 0, 0, 0, 0, 0];
        let decoded = decode_handshake(&buf).unwrap();
        assert_eq!(decoded.prefix, "");
        assert!(decoded.segments.is_empty());
        assert_eq!(decoded.capability_flags, 0);
        assert_eq!(decoded.server_identity, None);
        assert!(decoded.routes.is_empty());
    }
}

// ── Cross-language compatibility tests ───────────────────────────────────
// Canonical wire compatibility fixtures shared by all SDK bindings.

mod cross_lang_tests {
    use crate::buddy::*;
    use crate::control::*;
    use crate::frame::*;
    use crate::handshake::*;

    const ABI_HASH: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SIG_HASH: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    fn hex_to_bytes(hex: &str) -> Vec<u8> {
        (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect()
    }

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
    fn canonical_frame_fixture_decodes() {
        let bytes = hex_to_bytes("180000003930000000000000c2010000746573745f7061796c6f6164");
        let (hdr, payload) = decode_frame(&bytes).unwrap();
        assert_eq!(hdr.request_id, 12345);
        assert_eq!(hdr.flags, 0x1C2);
        assert_eq!(payload, b"test_payload");
    }

    #[test]
    fn canonical_call_control_fixture_decodes() {
        let identity = call_identity("hello");
        let bytes = encode_call_control(&identity, 7).unwrap();
        let (ctrl, consumed) = decode_call_control(&bytes, 0).unwrap();
        assert_eq!(ctrl.identity, identity);
        assert_eq!(ctrl.method_idx, 7);
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn legacy_short_call_control_fixture_is_rejected() {
        let bytes = hex_to_bytes("000000");
        let err = decode_call_control(&bytes, 0)
            .expect_err("legacy name-only call control is not a compatibility fixture");
        assert!(err.to_string().contains("too short"), "{err}");
    }

    #[test]
    fn canonical_reply_success_fixture_decodes() {
        let bytes = hex_to_bytes("00");
        let (ctrl, consumed) = decode_reply_control(&bytes, 0).unwrap();
        assert_eq!(ctrl, ReplyControl::Success);
        assert_eq!(consumed, 1);
    }

    #[test]
    fn canonical_reply_error_fixture_decodes() {
        let bytes = hex_to_bytes(
            "0161000000433245317b2276657273696f6e223a312c22636f6465223a332c226e616d65223a225265736f7572636546756e6374696f6e457865637574696e67222c226d657373616765223a2274657374206572726f72222c2264657461696c73223a7b7d7d",
        );
        let (ctrl, consumed) = decode_reply_control(&bytes, 0).unwrap();
        match ctrl {
            ReplyControl::Error(data) => {
                assert_eq!(
                    data,
                    br#"C2E1{"version":1,"code":3,"name":"ResourceFunctionExecuting","message":"test error","details":{}}"#
                );
            }
            _ => panic!("expected error"),
        }
        assert_eq!(consumed, bytes.len());
    }

    #[test]
    fn canonical_buddy_payload_fixture_decodes() {
        let bytes = hex_to_bytes("020007000000001000000002000000");
        let (bp, consumed) = decode_buddy_payload(&bytes).unwrap();
        assert_eq!(bp.seg_idx, 2);
        assert_eq!(bp.generation, 7);
        assert_eq!(bp.offset, 4096);
        assert_eq!(bp.data_size, 512);
        assert!(!bp.is_dedicated);
        assert_eq!(consumed, BUDDY_PAYLOAD_SIZE);
    }

    #[test]
    fn legacy_eleven_byte_buddy_fixture_is_rejected() {
        let bytes = hex_to_bytes("0200001000000002000000");
        assert!(matches!(
            decode_buddy_payload(&bytes),
            Err(DecodeError::BufferTooShort { need: 15, have: 11 })
        ));
    }

    #[test]
    fn legacy_v10_client_handshake_fixture_is_rejected() {
        let bytes = hex_to_bytes("0a0001000000001004736567300300");
        assert!(matches!(
            decode_handshake(&bytes),
            Err(DecodeError::InvalidValue {
                field: "handshake version",
                value: 10
            })
        ));
    }

    #[test]
    fn canonical_client_handshake_fixture_decodes() {
        // v11: [0b][00 prefix_len][01 00 seg_count][00 00 00 10 size][04 seg0][03 00 caps]
        let bytes = hex_to_bytes("0b0001000000001004736567300300");
        let hs = decode_handshake(&bytes).unwrap();
        assert_eq!(hs.prefix, "");
        assert_eq!(hs.segments.len(), 1);
        assert_eq!(hs.segments[0].0, "seg0");
        assert_eq!(hs.segments[0].1, 268_435_456);
        assert_eq!(hs.capability_flags, CAP_CALL_V2 | CAP_METHOD_IDX);
        assert_eq!(hs.server_identity, None);
        assert!(hs.routes.is_empty());
    }

    #[test]
    fn canonical_server_handshake_fixture_decodes() {
        // v11: client handshake prefix, server identity, then route table
        // with per-route full CRM tag and contract hashes.
        let bytes = hex_to_bytes(
            "0b00010000000008047372763003000b7365727665722d6772696409696e73742d677269640100046772696413677269642d726f7574652d7569642d30303031010000000000000009746573742e67726964044772696405302e312e3040303132333435363738396162636465663031323334353637383961626364656630313233343536373839616263646566303132333435363738396162636465664061626364656630313233343536373839616263646566303132333435363738396162636465663031323334353637383961626364656630313233343536373839000400000000000002000568656c6c6f0000036164640100",
        );
        let hs = decode_handshake(&bytes).unwrap();
        assert_eq!(hs.prefix, "");
        assert_eq!(hs.segments.len(), 1);
        assert_eq!(hs.segments[0].0, "srv0");
        assert_eq!(hs.segments[0].1, 134_217_728);
        assert_eq!(hs.capability_flags, CAP_CALL_V2 | CAP_METHOD_IDX);
        assert_eq!(
            hs.server_identity.as_ref(),
            Some(&ServerIdentity {
                server_id: "server-grid".into(),
                server_instance_id: "inst-grid".into(),
            })
        );
        assert_eq!(hs.routes.len(), 1);
        assert_eq!(hs.routes[0].name, "grid");
        assert_eq!(hs.routes[0].route_uid, "grid-route-uid-0001");
        assert_eq!(hs.routes[0].route_revision, 1);
        assert_eq!(hs.routes[0].crm_ns, "test.grid");
        assert_eq!(hs.routes[0].crm_name, "Grid");
        assert_eq!(hs.routes[0].crm_ver, "0.1.0");
        assert_eq!(hs.routes[0].abi_hash, ABI_HASH);
        assert_eq!(hs.routes[0].signature_hash, SIG_HASH);
        assert_eq!(hs.routes[0].methods.len(), 2);
        assert_eq!(hs.routes[0].methods[0].name, "hello");
        assert_eq!(hs.routes[0].methods[0].index, 0);
        assert_eq!(hs.routes[0].methods[1].name, "add");
        assert_eq!(hs.routes[0].methods[1].index, 1);
    }

    #[test]
    fn rust_encode_matches_canonical_call_control_fixture() {
        let identity = call_identity("hello");
        let encoded = encode_call_control(&identity, 7).unwrap();
        let mut expected = Vec::new();
        expected.extend_from_slice(b"\x05hello");
        expected.extend_from_slice(b"\x14hello-route-uid-0001");
        expected.extend_from_slice(&1u64.to_le_bytes());
        expected.extend_from_slice(b"\x09test.grid");
        expected.extend_from_slice(b"\x04Grid");
        expected.extend_from_slice(b"\x050.1.0");
        expected.push(64);
        expected.extend_from_slice(ABI_HASH.as_bytes());
        expected.push(64);
        expected.extend_from_slice(SIG_HASH.as_bytes());
        expected.extend_from_slice(&7u16.to_le_bytes());
        assert_eq!(encoded, expected);
    }

    #[test]
    fn rust_encode_matches_canonical_reply_success_fixture() {
        let encoded = try_encode_reply_control(&ReplyControl::Success).unwrap();
        assert_eq!(encoded, hex_to_bytes("00"));
    }

    #[test]
    fn rust_encode_matches_canonical_reply_error_fixture() {
        let err = br#"C2E1{"version":1,"code":3,"name":"ResourceFunctionExecuting","message":"test error","details":{}}"#.to_vec();
        let encoded = try_encode_reply_control(&ReplyControl::Error(err)).unwrap();
        let expected = hex_to_bytes(
            "0161000000433245317b2276657273696f6e223a312c22636f6465223a332c226e616d65223a225265736f7572636546756e6374696f6e457865637574696e67222c226d657373616765223a2274657374206572726f72222c2264657461696c73223a7b7d7d",
        );
        assert_eq!(encoded, expected);
    }

    #[test]
    fn rust_encode_matches_canonical_buddy_payload_fixture() {
        let bp = BuddyPayload {
            seg_idx: 2,
            generation: 7,
            offset: 4096,
            data_size: 512,
            is_dedicated: false,
        };
        let encoded = encode_buddy_payload(&bp);
        let expected = hex_to_bytes("020007000000001000000002000000");
        assert_eq!(encoded.as_slice(), expected.as_slice());
    }

    #[test]
    fn rust_encode_matches_canonical_client_handshake_fixture() {
        let segments = vec![("seg0".into(), 268_435_456u32)];
        let encoded = encode_client_handshake(&segments, CAP_CALL_V2 | CAP_METHOD_IDX, "").unwrap();
        // v11: [0b][00 prefix_len][01 00 seg_count][00 00 00 10 size][04 name_len][seg0][03 00 caps]
        let expected = hex_to_bytes("0b0001000000001004736567300300");
        assert_eq!(encoded, expected);
    }

    #[test]
    fn rust_encode_matches_canonical_server_handshake_fixture() {
        let segments = vec![("srv0".into(), 134_217_728u32)];
        let routes = vec![RouteInfo {
            name: "grid".into(),
            route_uid: "grid-route-uid-0001".into(),
            route_revision: 1,
            crm_ns: "test.grid".into(),
            crm_name: "Grid".into(),
            crm_ver: "0.1.0".into(),
            abi_hash: ABI_HASH.into(),
            signature_hash: SIG_HASH.into(),
            max_payload_size: 1024,
            methods: vec![
                MethodEntry {
                    name: "hello".into(),
                    index: 0,
                },
                MethodEntry {
                    name: "add".into(),
                    index: 1,
                },
            ],
        }];
        let identity = ServerIdentity {
            server_id: "server-grid".into(),
            server_instance_id: "inst-grid".into(),
        };
        let encoded = encode_server_handshake(
            &segments,
            CAP_CALL_V2 | CAP_METHOD_IDX,
            &routes,
            "",
            &identity,
        )
        .unwrap();
        // v11: [0b][00 prefix_len] then segments/caps, identity, and route table
        // with per-route full CRM tag and contract hash metadata.
        let expected = hex_to_bytes(
            "0b00010000000008047372763003000b7365727665722d6772696409696e73742d677269640100046772696413677269642d726f7574652d7569642d30303031010000000000000009746573742e67726964044772696405302e312e3040303132333435363738396162636465663031323334353637383961626364656630313233343536373839616263646566303132333435363738396162636465664061626364656630313233343536373839616263646566303132333435363738396162636465663031323334353637383961626364656630313233343536373839000400000000000002000568656c6c6f0000036164640100",
        );
        assert_eq!(encoded, expected);
    }
}
