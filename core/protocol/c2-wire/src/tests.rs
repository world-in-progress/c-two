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
        let golden = b"\x17\0\0\0\x2a\0\0\0\0\0\0\0\xc0\0\0\0hello world";
        assert_eq!(encode_frame(42, FLAG_BUDDY | FLAG_CALL_V2, payload), golden);
        for parts in [
            &[payload.as_slice()][..],
            &[b"hello ", b"world"],
            &[b"", b"hello ", b"", b"world", b""],
        ] {
            let encoded = encode_frame_parts(42, FLAG_BUDDY | FLAG_CALL_V2, parts);
            assert_eq!(encoded, golden);
            let (hdr, decoded_payload) = decode_frame(&encoded).unwrap();
            assert_eq!(hdr.request_id, 42);
            assert_eq!(hdr.flags, FLAG_BUDDY | FLAG_CALL_V2);
            assert_eq!(decoded_payload, payload);
            assert!(hdr.is_buddy());
            assert!(hdr.is_call_v2());
            assert!(!hdr.is_response());
        }
    }

    #[test]
    fn encode_decode_empty_payload() {
        let encoded = encode_frame(0, 0, &[]);
        let (hdr, payload) = decode_frame(&encoded).unwrap();
        assert_eq!(hdr.request_id, 0);
        assert_eq!(hdr.flags, 0);
        assert!(payload.is_empty());
        assert_eq!(hdr.total_len, 12); // 8B rid + 4B flags
        for parts in [&[][..], &[b"".as_slice()], &[b"".as_slice(), b""]] {
            assert_eq!(
                encode_frame_parts(0, 0, parts),
                b"\x0c\0\0\0\0\0\0\0\0\0\0\0\0\0\0\0"
            );
        }
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

mod ipc15_tests {
    use crate::buddy::*;
    use crate::control::*;
    use crate::flags::*;
    use crate::frame::*;
    use crate::handshake::*;
    use crate::msg_type::MsgType;
    use crate::route_catalog_control::*;

    const ABI: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
    const SIG: &str = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";

    fn identity(route_name: &str) -> RouteCallIdentity<&str> {
        RouteCallIdentity {
            route_name,
            route_uid: "uid",
            crm_ns: "ns",
            crm_name: "Grid",
            crm_ver: "0.1.0",
            abi_hash: ABI,
            signature_hash: SIG,
        }
    }

    fn route() -> RouteInfo<&'static str> {
        RouteInfo {
            name: "grid",
            route_uid: "uid",
            crm_ns: "ns",
            crm_name: "Grid",
            crm_ver: "0.1.0",
            abi_hash: ABI,
            signature_hash: SIG,
            max_payload_size: 1024,
            methods: vec![MethodEntry {
                name: "echo",
                index: 7,
            }],
        }
    }

    fn server() -> ServerIdentity<&'static str> {
        ServerIdentity {
            server_id: "srv",
            server_instance_id: "inst",
        }
    }

    fn call_golden() -> Vec<u8> {
        concat!(
            "\x04grid\x03uid\x02ns\x04Grid\x050.1.0\x40",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "\x40abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            "\x07\x00"
        )
        .as_bytes()
        .to_vec()
    }

    fn server_golden() -> Vec<u8> {
        concat!(
            "\x01p\x18\x00\x03srv\x04inst\x01\x00",
            "\x04grid\x03uid\x02ns\x04Grid\x050.1.0\x40",
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "\x40abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
            "\x00\x04\x00\x00\x00\x00\x00\x00\x01\x00\x04echo\x07\x00"
        )
        .as_bytes()
        .to_vec()
    }

    #[test]
    fn request_numbers_keep_all_u64_bits() {
        for id in [1, 1 + (1 << 32), u64::MAX] {
            let frame = encode_frame(id, FLAG_CALL_V2, b"body");
            assert_eq!(decode_frame(&frame).unwrap().0.request_id, id);
        }
    }

    #[test]
    fn descriptor_has_handwritten_little_endian_23_byte_layout() {
        let expected = [
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 0, 15, 16, 17, 18, 19, 20, 21, 22,
        ];
        let descriptor = BuddyPayload {
            seg_idx: 0x0201,
            generation: 0x0605_0403,
            offset: 0x0a09_0807,
            data_size: 0x0e0d_0c0b,
            is_dedicated: false,
            loan_id: 0x1615_1413_1211_100f,
        };
        assert_eq!(encode_buddy_payload(&descriptor), expected);
        assert_eq!(decode_buddy_payload(&expected).unwrap(), (descriptor, 23));
        let dedicated = BuddyPayload {
            is_dedicated: true,
            ..descriptor
        };
        let mut expected = expected;
        expected[14] = 1;
        assert_eq!(encode_buddy_payload(&dedicated), expected);
        assert_eq!(decode_buddy_payload(&expected).unwrap(), (dedicated, 23));
    }

    #[test]
    fn descriptor_rejects_zero_identity_and_unrecognized_kind() {
        let descriptor = BuddyPayload {
            seg_idx: u16::MAX,
            generation: u32::MAX,
            offset: u32::MAX,
            data_size: u32::MAX,
            is_dedicated: false,
            loan_id: u64::MAX,
        };
        for dedicated in [false, true] {
            let good = encode_buddy_payload(&BuddyPayload {
                is_dedicated: dedicated,
                ..descriptor
            });
            assert!(decode_buddy_payload(&good).is_ok());
            for len in 0..23 {
                assert!(decode_buddy_payload(&good[..len]).is_err());
            }
            let mut bad = good;
            bad[2..6].fill(0);
            assert!(decode_buddy_payload(&bad).is_err());
            let mut bad = good;
            bad[15..23].fill(0);
            assert!(decode_buddy_payload(&bad).is_err());
            for kind in 2..=u8::MAX {
                let mut bad = good;
                bad[14] = kind;
                assert!(decode_buddy_payload(&bad).is_err());
            }
        }
    }

    #[test]
    fn borrowed_call_identity_encodes_without_revision() {
        let bytes = encode_call_control(&identity("grid"), 7);
        assert_eq!(bytes, call_golden());
        let (decoded, consumed) = decode_call_control(&call_golden(), 0).unwrap();
        assert_eq!(consumed, bytes.len());
        assert_eq!(decoded.identity.route_name, "grid");
        assert_eq!(decoded.identity.route_uid, "uid");
        assert_eq!(decoded.identity.abi_hash, ABI);
        assert_eq!(decoded.identity.signature_hash, SIG);
        assert_eq!(decoded.method_idx, 7);
    }

    #[test]
    fn call_control_writes_only_inside_checked_output_range() {
        let value = identity("grid");
        let size = encoded_call_control_len(&value);
        let mut out = vec![0xaa; size + 4];
        assert_eq!(
            encode_call_control_into(&mut out, 2, &value, 7).unwrap(),
            size
        );
        assert_eq!(&out[2..size + 2], &call_golden());
        assert_eq!(&out[..2], &[0xaa; 2]);
        assert_eq!(&out[size + 2..], &[0xaa; 2]);
        assert_eq!(decode_call_control(&out, 2).unwrap().0.method_idx, 7);
        for offset in [0, 1, usize::MAX] {
            let mut out = vec![0xaa; size - 1];
            assert!(encode_call_control_into(&mut out, offset, &value, 7).is_err());
            assert!(out.iter().all(|byte| *byte == 0xaa));
        }
    }

    #[test]
    fn call_boundary_rejects_truncation_bad_contract_and_legacy_revision() {
        let bytes = call_golden();
        for len in 0..bytes.len() {
            assert!(decode_call_control(&bytes[..len], 0).is_err());
        }
        assert!(decode_call_control(&bytes, usize::MAX).is_err());
        for (field, text) in [
            ("route", "grid\0hidden"),
            ("uid", "bad/uid"),
            ("crm", "bad/crm"),
            ("hash", "short"),
        ] {
            let mut value = identity("grid");
            match field {
                "route" => value.route_name = text,
                "uid" => value.route_uid = text,
                "crm" => value.crm_name = text,
                _ => value.abi_hash = text,
            }
            assert!(
                decode_call_control(&encode_call_control(&value, 7), 0).is_err(),
                "{field}"
            );
        }
        let mut old = bytes;
        old.splice(9..9, 1u64.to_le_bytes());
        assert!(decode_call_control(&old, 0).is_err());
    }

    #[test]
    fn reply_control_preserves_status_and_registered_error_bytes() {
        let error = br#"C2E1{"version":1,"code":713,"name":"ProtocolViolation","message":"bad","details":{}}"#.to_vec();
        for reply in [
            ReplyControl::Success,
            ReplyControl::Error(error),
            ReplyControl::Error(vec![]),
            ReplyControl::RouteNotFound("grid".into()),
        ] {
            let bytes = encode_reply_control(&reply);
            assert_eq!(
                decode_reply_control(&bytes, 0).unwrap(),
                (reply, bytes.len())
            );
            for len in 0..bytes.len() {
                assert!(decode_reply_control(&bytes[..len], 0).is_err());
            }
            assert!(decode_reply_control(&bytes, usize::MAX).is_err());
        }
        assert_eq!(encode_reply_control(&ReplyControl::Success), [0]);
        assert_eq!(
            encode_reply_control(&ReplyControl::Error(b"abc".to_vec())),
            b"\x01\x03\0\0\0abc"
        );
        assert!(decode_reply_control(&[255], 0).is_err());
        assert!(
            decode_reply_control(
                &encode_reply_control(&ReplyControl::RouteNotFound("bad\0route".into())),
                0
            )
            .is_err()
        );
    }

    #[test]
    fn permanent_preamble_stays_25_bytes_and_advertises_15() {
        for (kind, flags) in [
            (VersionPreambleKind::Hello, 4),
            (VersionPreambleKind::Accepted, 6),
            (VersionPreambleKind::VersionMismatch, 6),
        ] {
            let mut golden = [0; 25];
            golden[0] = 21;
            golden[12] = flags;
            golden[16..20].copy_from_slice(b"C2IP");
            golden[20] = kind as u8;
            golden[21] = 15;
            assert_eq!(encode_version_preamble(kind), golden);
            validate_version_preamble_header(golden[..16].try_into().unwrap(), flags == 6).unwrap();
            assert_eq!(
                decode_version_preamble_payload(golden[16..].try_into().unwrap()).unwrap(),
                (kind, 15)
            );
        }
    }

    #[test]
    fn preamble_checks_fixed_envelope_before_metadata() {
        let good = encode_version_preamble(VersionPreambleKind::Hello);
        for byte in [0, 4, 12] {
            let mut bad = good;
            bad[byte] ^= 1;
            assert!(
                validate_version_preamble_header(bad[..16].try_into().unwrap(), false).is_err()
            );
        }
        for byte in [16, 20, 21] {
            let mut bad = good;
            bad[byte] = 0;
            assert!(decode_version_preamble_payload(bad[16..].try_into().unwrap()).is_err());
        }
        // The permanent codec must read other nonzero versions so drivers can
        // produce a typed VersionMismatch; accepting equality is their fence.
        let mut old = good;
        old[21..].copy_from_slice(&14u32.to_le_bytes());
        assert_eq!(
            decode_version_preamble_payload(old[16..].try_into().unwrap())
                .unwrap()
                .1,
            14
        );
    }

    #[test]
    fn client_metadata_has_only_prefix_and_real_shm_directions() {
        for caps in [
            0,
            CAP_SHM_REQUEST,
            CAP_SHM_RESPONSE,
            CAP_SHM_REQUEST | CAP_SHM_RESPONSE,
        ] {
            let bytes = encode_client_handshake(caps, "p").unwrap();
            assert_eq!(bytes, [1, b'p', caps as u8, 0]);
            let decoded = decode_handshake(&bytes, false).unwrap();
            assert_eq!(decoded.prefix, "p");
            assert_eq!(decoded.capability_flags, caps);
            assert!(decoded.server_identity.is_none());
        }
        assert_eq!(
            decode_handshake(b"\0\0\0", false).unwrap().capability_flags,
            0
        );
        for caps in [1, 2, 4, 0xffff] {
            assert!(decode_handshake(&encode_client_handshake(caps, "p").unwrap(), false).is_err());
        }
    }

    #[test]
    fn server_metadata_matches_handwritten_ipc15_golden() {
        let bytes = encode_server_handshake(0x18, &[route()], "p", &server()).unwrap();
        assert_eq!(bytes, server_golden());
        let decoded = decode_handshake(&server_golden(), true).unwrap();
        assert_eq!(decoded.routes.len(), 1);
        let current = &decoded.routes[0];
        assert_eq!(current.name, "grid");
        assert_eq!(current.route_uid, "uid");
        assert_eq!(current.max_payload_size, 1024);
        assert_eq!(
            current.methods,
            [MethodEntry {
                name: "echo".into(),
                index: 7
            }]
        );
        assert_eq!(decoded.server_identity.unwrap().server_instance_id, "inst");
    }

    #[test]
    fn metadata_rejects_wrong_direction_truncation_and_superseded_fields() {
        let server = server_golden();
        for len in 0..server.len() {
            assert!(decode_handshake(&server[..len], true).is_err(), "{len}");
        }
        let client = [1, b'p', 0x18, 0];
        assert!(decode_handshake(&client, true).is_err());
        assert!(decode_handshake(&server, false).is_err());
        for bytes in [
            b"\0\0\0\x18\0".to_vec(),
            b"\0\x01\0\0\0\0\x10\x04seg0\x18\0".to_vec(),
            [server.as_slice(), &[0]].concat(),
        ] {
            assert!(decode_handshake(&bytes, false).is_err());
            assert!(decode_handshake(&bytes, true).is_err());
        }
        let mut old = server;
        let contract_offset = old.windows(3).position(|bytes| bytes == b"\x02ns").unwrap();
        old.splice(contract_offset..contract_offset, 1u64.to_le_bytes());
        assert!(decode_handshake(&old, true).is_err());
    }

    #[test]
    fn metadata_validates_contract_payload_limit_and_count_on_receive() {
        for tag in 0..5 {
            let mut value = route();
            match tag {
                0 => value.crm_ns = "bad/ns",
                1 => value.crm_name = "bad/name",
                2 => value.abi_hash = "short",
                3 => value.signature_hash = "short",
                _ => value.max_payload_size = 0,
            }
            let bytes = encode_server_handshake(0, &[value], "p", &server()).unwrap();
            assert!(decode_handshake(&bytes, true).is_err(), "{tag}");
        }
        let values = vec![route(); MAX_ROUTES + 1];
        assert!(
            decode_handshake(
                &encode_server_handshake(0, &values, "p", &server()).unwrap(),
                true
            )
            .is_err()
        );
        let mut value = route();
        value.methods = vec![
            MethodEntry {
                name: "echo",
                index: 0
            };
            MAX_METHODS + 1
        ];
        assert!(
            decode_handshake(
                &encode_server_handshake(0, &[value], "p", &server()).unwrap(),
                true
            )
            .is_err()
        );
        assert!(encode_client_handshake(0, &"x".repeat(256)).is_err());
    }

    fn record() -> RouteRecordWire {
        RouteRecordWire {
            route_name: "grid".into(),
            route_uid: "uid".into(),
            owner_server_id: "srv".into(),
            owner_server_instance_id: "inst".into(),
            contract: RouteContractWire {
                route_name: "grid".into(),
                crm_ns: "ns".into(),
                crm_name: "Grid".into(),
                crm_ver: "0.1.0".into(),
                abi_hash: ABI.into(),
                signature_hash: SIG.into(),
            },
            methods: vec![RouteMethodWire {
                name: "echo".into(),
                index: 7,
            }],
            max_payload_size: 1024,
            state: RouteStateWire::Ready,
            state_reason: None,
        }
    }

    #[test]
    fn list_lookup_and_nack_roundtrip_without_revision_or_nonce() {
        let request = RouteListRequest {
            selector: RouteSelector::All,
        };
        assert_eq!(
            encode_route_list_request(&request).unwrap(),
            b"\x0e{\"selector\":{\"type\":\"all\"}}"
        );
        assert_eq!(
            decode_route_list_request(&encode_route_list_request(&request).unwrap()).unwrap(),
            request
        );
        let response = RouteListResponse {
            routes: vec![record()],
        };
        assert_eq!(
            decode_route_list_response(&encode_route_list_response(&response).unwrap()).unwrap(),
            response
        );
        let request = RouteLookupRequest {
            expected: record().contract,
            observed_route_uid: Some("uid".into()),
        };
        assert_eq!(
            decode_route_lookup_request(&encode_route_lookup_request(&request).unwrap()).unwrap(),
            request
        );
        let response = RouteLookupResponse::Ready { current: record() };
        assert_eq!(
            decode_route_lookup_response(&encode_route_lookup_response(&response).unwrap())
                .unwrap(),
            response
        );
        let nack = RouteNack {
            error: c2_error::C2ErrorEnvelope {
                version: c2_error::ERROR_WIRE_VERSION,
                code: 713,
                name: "ProtocolViolation".into(),
                message: "bad".into(),
                details: Default::default(),
            },
        };
        assert_eq!(
            decode_route_nack(&encode_route_nack(&nack).unwrap()).unwrap(),
            nack
        );
    }

    #[test]
    fn json_boundary_rejects_superseded_fields_at_every_nested_level() {
        let good = RouteLookupResponse::Ready { current: record() };
        for key in [
            "route_revision",
            "catalog_revision",
            "owner_epoch",
            "lease_deadline_ms",
        ] {
            let mut value = serde_json::to_value(&good).unwrap();
            value["current"][key] = 1.into();
            let mut bytes = vec![0x11];
            bytes.extend_from_slice(&serde_json::to_vec(&value).unwrap());
            assert!(decode_route_lookup_response(&bytes).is_err(), "{key}");
        }
        let request = RouteLookupRequest {
            expected: record().contract,
            observed_route_uid: None,
        };
        let mut value = serde_json::to_value(request).unwrap();
        value["observed_route_revision"] = 1.into();
        let mut bytes = vec![0x10];
        bytes.extend_from_slice(&serde_json::to_vec(&value).unwrap());
        assert!(decode_route_lookup_request(&bytes).is_err());
        assert!(
            decode_route_list_request(b"\x0e{\"selector\":{\"type\":\"all\"},\"min_revision\":1}")
                .is_err()
        );
        assert!(
            decode_route_list_response(
                b"\x0f{\"routes\":[],\"catalog_revision\":1,\"min_watch_revision\":1}"
            )
            .is_err()
        );
        for tag in [0x0c, 0x0d, 0x12, 0x13, 0x14, 0x16] {
            assert!(MsgType::from_byte(tag).is_none());
        }
    }

    #[test]
    fn directory_receive_validation_rejects_duplicate_and_mismatched_records() {
        let response = RouteListResponse {
            routes: vec![record(), record()],
        };
        assert!(
            decode_route_list_response(&encode_route_list_response(&response).unwrap()).is_err()
        );
        let mut current = record();
        current.contract.route_name = "other".into();
        let response = RouteLookupResponse::Ready { current };
        assert!(
            decode_route_lookup_response(&encode_route_lookup_response(&response).unwrap())
                .is_err()
        );
    }
}
