//! Unit tests for c2-wire codec.
//!
//! Tests verify round-trip encoding/decoding and canonical cross-language
//! wire compatibility fixtures.

mod owner_control_tests {
    use crate::owner_control::*;

    // Handwritten bytes, never produced by the codec under test. The address
    // occupies four UTF-8 bytes; the distinct byte ranges expose field swaps.
    const IDENTITY: &[u8] = b"\x04\x00a\xe7\x95\x8c\x01\x00s\x01\x00i\
        \x01\x02\x03\x04\x05\x06\x07\x08\
        \x10\x11\x12\x13\x14\x15\x16\x17\
        \x18\x19\x1a\x1b\x1c\x1d\x1e\x1f\
        \x20\x21\x22\x23\x24\x25\x26\x27\
        \x28\x29\x2a\x2b\x2c\x2d\x2e\x2f";
    const SECRET: &[u8] = b"\x30\x31\x32\x33\x34\x35\x36\x37\
        \x38\x39\x3a\x3b\x3c\x3d\x3e\x3f\
        \x40\x41\x42\x43\x44\x45\x46\x47\
        \x48\x49\x4a\x4b\x4c\x4d\x4e\x4f";
    const SUCCESSOR: &[u8] = b"\x50\x51\x52\x53\x54\x55\x56\x57\
        \x58\x59\x5a\x5b\x5c\x5d\x5e\x5f\
        \x60\x61\x62\x63\x64\x65\x66\x67\
        \x68\x69\x6a\x6b\x6c\x6d\x6e\x6f";

    fn ticket() -> OwnerTicketWire {
        OwnerTicketWire {
            address: "a界".into(),
            server_id: "s".into(),
            server_instance_id: "i".into(),
            epoch: 0x0807_0605_0403_0201,
            context_binding: std::array::from_fn(|i| 0x10 + i as u8),
            secret: std::array::from_fn(|i| 0x30 + i as u8),
        }
    }

    fn status(phase: OwnerPhaseWire) -> OwnerStatusWire {
        let ticket = ticket();
        OwnerStatusWire {
            address: ticket.address,
            server_id: ticket.server_id,
            server_instance_id: ticket.server_instance_id,
            epoch: ticket.epoch,
            context_binding: ticket.context_binding,
            phase,
        }
    }

    fn request_cases() -> Vec<(OwnerRequest, Vec<u8>)> {
        vec![
            (OwnerRequest::Hello, vec![0x16, 1, 1]),
            (OwnerRequest::GetRecoveryTicket, vec![0x16, 1, 2]),
            (OwnerRequest::RevokeRecovery, vec![0x16, 1, 3]),
            (
                OwnerRequest::Takeover {
                    ticket: ticket(),
                    successor_secret: std::array::from_fn(|i| 0x50 + i as u8),
                },
                [b"\x16\x01\x04".as_slice(), IDENTITY, SECRET, SUCCESSOR].concat(),
            ),
            (OwnerRequest::RequestShutdown, vec![0x16, 1, 5]),
        ]
    }

    fn response_cases() -> Vec<(OwnerResponse, Vec<u8>)> {
        vec![
            (
                OwnerResponse::Status(status(OwnerPhaseWire::OwnerMissing)),
                [b"\x16\x01\x01".as_slice(), IDENTITY, b"\x02"].concat(),
            ),
            (
                OwnerResponse::RecoveryTicket(ticket()),
                [b"\x16\x01\x02".as_slice(), IDENTITY, SECRET].concat(),
            ),
            (OwnerResponse::Revoked, vec![0x16, 1, 3]),
            (
                OwnerResponse::TakenOver(status(OwnerPhaseWire::Armed)),
                [b"\x16\x01\x04".as_slice(), IDENTITY, b"\x01"].concat(),
            ),
            (OwnerResponse::ShutdownInitiated, vec![0x16, 1, 5]),
            (
                OwnerResponse::Rejected(OwnerRejection::InvalidGrant),
                vec![0x16, 1, 255, 5],
            ),
        ]
    }

    #[test]
    fn all_request_operations_match_handwritten_goldens() {
        for (request, golden) in request_cases() {
            assert_eq!(encode_owner_request(&request).unwrap(), golden);
            assert_eq!(decode_owner_request(&golden).unwrap(), request);
        }
    }

    #[test]
    fn all_response_operations_match_handwritten_goldens() {
        for (response, golden) in response_cases() {
            assert_eq!(encode_owner_response(&response).unwrap(), golden);
            assert_eq!(decode_owner_response(&golden).unwrap(), response);
        }
    }

    #[test]
    fn phase_and_rejection_discriminants_match_handwritten_goldens() {
        for (byte, phase) in [
            (1, OwnerPhaseWire::Armed),
            (2, OwnerPhaseWire::OwnerMissing),
            (3, OwnerPhaseWire::Draining),
            (4, OwnerPhaseWire::Finished),
            (5, OwnerPhaseWire::WatcherIoError),
        ] {
            for op in [1, 4] {
                let golden = [&[0x16, 1, op], IDENTITY, &[byte]].concat();
                let response = if op == 1 {
                    OwnerResponse::Status(status(phase))
                } else {
                    OwnerResponse::TakenOver(status(phase))
                };
                assert_eq!(encode_owner_response(&response).unwrap(), golden);
                assert_eq!(decode_owner_response(&golden).unwrap(), response);
            }
        }
        for (byte, reason) in [
            (1, OwnerRejection::Malformed),
            (2, OwnerRejection::UnsupportedVersion),
            (3, OwnerRejection::IdentityMismatch),
            (4, OwnerRejection::OwnerPresent),
            (5, OwnerRejection::InvalidGrant),
            (6, OwnerRejection::GraceExpired),
            (7, OwnerRejection::ShuttingDown),
            (8, OwnerRejection::Unavailable),
        ] {
            let golden = [0x16, 1, 255, byte];
            let response = OwnerResponse::Rejected(reason);
            assert_eq!(encode_owner_response(&response).unwrap(), golden);
            assert_eq!(decode_owner_response(&golden).unwrap(), response);
        }
    }

    #[test]
    fn every_truncated_or_extended_golden_is_rejected() {
        for (_, mut golden) in request_cases() {
            for len in 0..golden.len() {
                assert!(decode_owner_request(&golden[..len]).is_err(), "{len}");
            }
            golden.push(0);
            assert_eq!(
                decode_owner_request(&golden),
                Err(OwnerWireError::Malformed)
            );
        }
        for (_, mut golden) in response_cases() {
            for len in 0..golden.len() {
                assert!(decode_owner_response(&golden[..len]).is_err(), "{len}");
            }
            golden.push(0);
            assert_eq!(
                decode_owner_response(&golden),
                Err(OwnerWireError::Malformed)
            );
        }
    }

    #[test]
    fn unknown_tags_versions_operations_and_enums_are_rejected() {
        for byte in 0..=255 {
            if byte != 0x16 {
                assert_eq!(
                    decode_owner_request(&[byte, 1, 1]),
                    Err(OwnerWireError::Malformed)
                );
                assert_eq!(
                    decode_owner_response(&[byte, 1, 3]),
                    Err(OwnerWireError::Malformed)
                );
            }
            if byte != 1 {
                assert_eq!(
                    decode_owner_request(&[0x16, byte, 1]),
                    Err(OwnerWireError::UnsupportedVersion)
                );
                assert_eq!(
                    decode_owner_response(&[0x16, byte, 3]),
                    Err(OwnerWireError::UnsupportedVersion)
                );
            }
            if !(1..=5).contains(&byte) {
                assert_eq!(
                    decode_owner_request(&[0x16, 1, byte]),
                    Err(OwnerWireError::Malformed)
                );
                if byte != 255 {
                    assert_eq!(
                        decode_owner_response(&[0x16, 1, byte]),
                        Err(OwnerWireError::Malformed)
                    );
                }
                for op in [1, 4] {
                    let payload = [&[0x16, 1, op], IDENTITY, &[byte]].concat();
                    assert_eq!(
                        decode_owner_response(&payload),
                        Err(OwnerWireError::Malformed)
                    );
                }
            }
            if !(1..=8).contains(&byte) {
                assert_eq!(
                    decode_owner_response(&[0x16, 1, 255, byte]),
                    Err(OwnerWireError::Malformed)
                );
            }
        }
        // Correct tag but no version is a format error, not a version error.
        assert_eq!(
            decode_owner_request(&[0x16]),
            Err(OwnerWireError::Malformed)
        );
        assert_eq!(
            decode_owner_response(&[0x16]),
            Err(OwnerWireError::Malformed)
        );
    }

    #[test]
    fn invalid_utf8_and_string_lengths_are_rejected_in_each_field() {
        for (op, body, is_request) in [
            (4, [IDENTITY, SECRET, SUCCESSOR].concat(), true),
            (2, [IDENTITY, SECRET].concat(), false),
            (1, [IDENTITY, b"\x01"].concat(), false),
            (4, [IDENTITY, b"\x02"].concat(), false),
        ] {
            let golden = [&[0x16, 1, op], body.as_slice()].concat();
            for (length_offset, text_offset) in [(3, 5), (9, 11), (12, 14)] {
                let mut invalid_utf8 = golden.clone();
                invalid_utf8[text_offset] = 0xff;
                let mut overlong = golden.clone();
                overlong[length_offset..length_offset + 2].copy_from_slice(&[0xff, 0xff]);
                for payload in [invalid_utf8, overlong] {
                    if is_request {
                        assert_eq!(
                            decode_owner_request(&payload),
                            Err(OwnerWireError::Malformed)
                        );
                    } else {
                        assert_eq!(
                            decode_owner_response(&payload),
                            Err(OwnerWireError::Malformed)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn complete_payload_limit_is_enforced_for_both_decoders() {
        assert_eq!(MAX_OWNER_PAYLOAD_SIZE, 4096);
        for op in [1, 2, 3, 4, 5, 255] {
            let mut oversized = vec![0; 4097];
            oversized[..3].copy_from_slice(&[0x16, 1, op]);
            assert_eq!(
                decode_owner_request(&oversized),
                Err(OwnerWireError::PayloadTooLarge)
            );
            assert_eq!(
                decode_owner_response(&oversized),
                Err(OwnerWireError::PayloadTooLarge)
            );
        }
    }

    #[test]
    fn encoding_limits_include_header_and_all_fixed_fields() {
        // Empty text fields leave 81 bytes for a ticket response, 113 for a
        // takeover request, and 50 for a status response (including headers).
        for (op, fixed_size) in [(2, 81), (4, 113), (1, 50)] {
            let address_len = 4096 - fixed_size;
            let mut ticket = OwnerTicketWire {
                address: "a".repeat(address_len),
                server_id: String::new(),
                server_instance_id: String::new(),
                epoch: 0,
                context_binding: [0; 32],
                secret: [0; 32],
            };
            let mut golden = vec![0x16, 1, op];
            golden.extend_from_slice(&(address_len as u16).to_le_bytes());
            golden.extend_from_slice(&vec![b'a'; address_len]);
            golden.extend_from_slice(&[0; 4 + 8 + 32]);
            if op == 1 {
                golden.push(1);
            } else {
                golden.extend_from_slice(&[0; 32]);
                if op == 4 {
                    golden.extend_from_slice(&[0; 32]);
                }
            }
            assert_eq!(golden.len(), 4096);
            if op == 4 {
                let request = OwnerRequest::Takeover {
                    ticket: ticket.clone(),
                    successor_secret: [0; 32],
                };
                assert_eq!(encode_owner_request(&request).unwrap(), golden);
                assert_eq!(decode_owner_request(&golden).unwrap(), request);
                ticket.address.push('a');
                assert_eq!(
                    encode_owner_request(&OwnerRequest::Takeover {
                        ticket,
                        successor_secret: [0; 32]
                    }),
                    Err(OwnerWireError::PayloadTooLarge)
                );
            } else if op == 2 {
                let response = OwnerResponse::RecoveryTicket(ticket.clone());
                assert_eq!(encode_owner_response(&response).unwrap(), golden);
                assert_eq!(decode_owner_response(&golden).unwrap(), response);
                ticket.address.push('a');
                assert_eq!(
                    encode_owner_response(&OwnerResponse::RecoveryTicket(ticket)),
                    Err(OwnerWireError::PayloadTooLarge)
                );
            } else {
                let mut status = OwnerStatusWire {
                    address: ticket.address,
                    server_id: ticket.server_id,
                    server_instance_id: ticket.server_instance_id,
                    epoch: 0,
                    context_binding: [0; 32],
                    phase: OwnerPhaseWire::Armed,
                };
                let response = OwnerResponse::Status(status.clone());
                assert_eq!(encode_owner_response(&response).unwrap(), golden);
                assert_eq!(decode_owner_response(&golden).unwrap(), response);
                status.address.push('a');
                assert_eq!(
                    encode_owner_response(&OwnerResponse::Status(status.clone())),
                    Err(OwnerWireError::PayloadTooLarge)
                );
                assert_eq!(
                    encode_owner_response(&OwnerResponse::TakenOver(status)),
                    Err(OwnerWireError::PayloadTooLarge)
                );
            }
        }
    }

    #[test]
    fn excessive_text_lengths_are_rejected_without_payload_diagnostics() {
        for len in [4096, u16::MAX as usize, u16::MAX as usize + 1] {
            for field in 0..3 {
                let mut ticket = ticket();
                let text = "s".repeat(len);
                match field {
                    0 => ticket.address = text,
                    1 => ticket.server_id = text,
                    _ => ticket.server_instance_id = text,
                }
                let error = encode_owner_response(&OwnerResponse::RecoveryTicket(ticket.clone()))
                    .unwrap_err();
                assert_eq!(error, OwnerWireError::PayloadTooLarge);
                assert_eq!(
                    format!("{error}"),
                    "owner control payload exceeds size limit"
                );
                assert_eq!(format!("{error:?}"), "PayloadTooLarge");
                assert_eq!(
                    encode_owner_request(&OwnerRequest::Takeover {
                        ticket,
                        successor_secret: [0; 32]
                    }),
                    Err(error)
                );
            }
        }
        assert_eq!(
            OwnerWireError::Malformed.to_string(),
            "malformed owner control payload"
        );
        assert_eq!(
            OwnerWireError::UnsupportedVersion.to_string(),
            "unsupported owner control version"
        );
    }

    #[test]
    fn codec_leaves_business_identity_validation_to_core() {
        let ticket = OwnerTicketWire {
            address: "not an IPC address\0界".into(),
            server_id: String::new(),
            server_instance_id: " / \\ ".into(),
            epoch: u64::MAX,
            context_binding: [0xff; 32],
            secret: [0; 32],
        };
        let response = OwnerResponse::RecoveryTicket(ticket.clone());
        assert_eq!(
            decode_owner_response(&encode_owner_response(&response).unwrap()).unwrap(),
            response
        );
        let request = OwnerRequest::Takeover {
            ticket,
            successor_secret: [0; 32],
        };
        assert_eq!(
            decode_owner_request(&encode_owner_request(&request).unwrap()).unwrap(),
            request
        );
    }

    #[test]
    fn debug_redacts_ticket_and_successor_secrets_in_nested_values() {
        let first = ticket();
        let mut second = first.clone();
        second.secret = [0xa7; 32];
        for pretty in [false, true] {
            let debug = |value: &dyn std::fmt::Debug| {
                if pretty {
                    format!("{value:#?}")
                } else {
                    format!("{value:?}")
                }
            };
            assert_eq!(debug(&first), debug(&second));
            assert!(debug(&first).contains("[REDACTED]"));
            assert_eq!(
                debug(&OwnerResponse::RecoveryTicket(first.clone())),
                debug(&OwnerResponse::RecoveryTicket(second.clone()))
            );
            let before = OwnerRequest::Takeover {
                ticket: first.clone(),
                successor_secret: [0xb9; 32],
            };
            let after = OwnerRequest::Takeover {
                ticket: second.clone(),
                successor_secret: [0xc8; 32],
            };
            assert_eq!(debug(&before), debug(&after));
            assert_eq!(debug(&before).matches("[REDACTED]").count(), 2);
        }
    }
}

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
            allocation_id: 0x0807_0605_0403_0201,
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
            allocation_id: 0,
        };
        let encoded = encode_buddy_payload(&bp);
        assert_eq!(encoded[14], BUDDY_FLAG_DEDICATED);

        let (decoded, _) = decode_buddy_payload(&encoded).unwrap();
        assert!(decoded.is_dedicated);
        assert_eq!(decoded.generation, 0);
    }

    #[test]
    fn canonical_buddy_payload_layout() {
        // `<HIII B Q`: segment, generation, offset, size, flags, allocation id. Distinct bytes
        // make field-order and truncation mistakes visible in this wire golden.
        assert_eq!(BUDDY_PAYLOAD_SIZE, 23);

        let bp = BuddyPayload {
            seg_idx: 0x0201,
            generation: 0x0605_0403,
            offset: 0x0a09_0807,
            data_size: 0x0e0d_0c0b,
            is_dedicated: false,
            allocation_id: 0x1615_1413_1211_100f,
        };
        let encoded = encode_buddy_payload(&bp);
        assert_eq!(
            encoded,
            [
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 0, 15, 16, 17, 18, 19, 20, 21, 22
            ]
        );
        assert_eq!(decode_buddy_payload(&encoded).unwrap(), (bp, 23));
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
                allocation_id: u64::MAX,
            };
            assert_eq!(
                decode_buddy_payload(&encode_buddy_payload(&bp)).unwrap(),
                (bp, 23)
            );
        }
    }

    #[test]
    fn decoder_rejects_zero_buddy_generation() {
        // Valid 23-byte shape, but only dedicated allocations may use generation 0.
        let bytes = [0_u8; 23];
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
            let mut bytes = [0_u8; 23];
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
            let mut bytes = [0_u8; 23];
            bytes[2] = 1;
            bytes[14] = flags;
            assert!(matches!(
                decode_buddy_payload(&bytes),
                Err(DecodeError::InvalidValue { field: "buddy flags", value })
                    if value == u64::from(flags)
            ));
        }
    }

    #[test]
    fn decoder_rejects_missing_or_wrong_allocation_id() {
        let mut buddy = [0_u8; BUDDY_PAYLOAD_SIZE];
        buddy[2] = 1;
        assert!(matches!(
            decode_buddy_payload(&buddy),
            Err(DecodeError::InvalidValue {
                field: "allocation id",
                value: 0
            })
        ));

        let mut dedicated = [0_u8; BUDDY_PAYLOAD_SIZE];
        dedicated[14] = BUDDY_FLAG_DEDICATED;
        dedicated[15] = 1;
        assert!(matches!(
            decode_buddy_payload(&dedicated),
            Err(DecodeError::InvalidValue {
                field: "allocation id",
                value: 1
            })
        ));

        assert!(matches!(
            decode_buddy_payload(&buddy[..15]),
            Err(DecodeError::BufferTooShort { need: 23, have: 15 })
        ));
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
    fn call_control_roundtrip_preserves_route_identity_and_contract() {
        let mut identity = call_identity("grid");
        identity.route_uid = "route-uid-0001".into();
        identity.observed_route_revision = 17;

        let encoded = encode_call_control(&identity, 42).unwrap();
        let (decoded, consumed) = decode_call_control(&encoded, 0).unwrap();

        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded.identity, identity);
        assert_eq!(decoded.method_idx, 42);
    }

    #[test]
    fn call_control_rejects_empty_route_name_before_buffer_capacity() {
        let mut identity = call_identity("grid");
        identity.route_name.clear();
        let error = encode_call_control(&identity, 0).unwrap_err();
        let mut buf = [0xAA; 1];
        assert_eq!(
            encode_call_control_into(&mut buf, usize::MAX, &identity, 0),
            Err(error)
        );
        assert_eq!(buf, [0xAA]);
        assert!(decode_call_control(&[0, 0, 0], 0).is_err());
    }

    #[test]
    fn call_control_with_offset() {
        let identity = call_identity("net");
        let encoded = encode_call_control(&identity, 7).unwrap();
        let len = encoded.len();
        let mut buf = vec![0xAA; len + 4];
        assert_eq!(encode_call_control_into(&mut buf, 2, &identity, 7), Ok(len));
        assert_eq!(&buf[2..2 + len], encoded);
        assert_eq!(&buf[..2], &[0xAA; 2]);
        assert_eq!(&buf[2 + len..], &[0xAA; 2]);
        let (decoded, consumed) = decode_call_control(&buf, 2).unwrap();
        assert_eq!(decoded.identity, identity);
        assert_eq!(decoded.method_idx, 7);
        assert_eq!(consumed, len);
    }

    #[test]
    fn encode_call_control_into_rejects_short_buffer_without_writing() {
        let identity = call_identity("grid");
        let len = encoded_call_control_len(&identity).unwrap();
        let mut buf = vec![0xAA; len - 1];
        for (offset, have) in [(0, len - 1), (1, len - 2), (usize::MAX, 0)] {
            assert_eq!(
                encode_call_control_into(&mut buf, offset, &identity, 0),
                Err(EncodeError::BufferTooShort {
                    field: "call_control",
                    need: len,
                    have
                }),
            );
            assert!(buf.iter().all(|&byte| byte == 0xAA));
        }
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
    fn call_control_rejects_route_name_longer_than_one_byte_length() {
        let long_name = "x".repeat(c2_contract::MAX_WIRE_TEXT_BYTES + 1);
        let mut identity = call_identity("grid");
        identity.route_name = long_name;
        let err = encode_call_control(&identity, 0).unwrap_err();
        assert!(err.to_string().contains("route_name"));
    }

    #[test]
    fn call_control_rejects_truncated_method_and_text() {
        let encoded = encode_call_control(&call_identity("grid"), 42).unwrap();
        assert_eq!(
            decode_call_control(&encoded, encoded.len() + 1),
            Err(crate::frame::DecodeError::BufferTooShort { need: 17, have: 0 })
        );
        let err = decode_call_control(&encoded[..encoded.len() - 1], 0).unwrap_err();
        assert_eq!(err.to_string(), "method_idx: need 2 bytes, have 1");

        // The signature hash is the final text field, so its declared length
        // cannot consume another field's bytes and mask the text bounds check.
        let mut malformed = encoded;
        let hash = SIG_HASH.as_bytes();
        let hash_start = malformed
            .windows(hash.len())
            .position(|w| w == hash)
            .unwrap();
        malformed[hash_start - 1] = u8::MAX;
        let err = decode_call_control(&malformed, 0).unwrap_err();
        assert!(
            err.to_string()
                .starts_with("signature_hash: need 255 bytes")
        );
    }

    #[test]
    fn call_control_accepts_maximum_route_name() {
        let mut identity = call_identity("grid");
        identity.route_name = "x".repeat(c2_contract::MAX_WIRE_TEXT_BYTES);
        let encoded = encode_call_control(&identity, 100).unwrap();
        let (decoded, consumed) = decode_call_control(&encoded, 0).unwrap();
        assert_eq!(consumed, encoded.len());
        assert_eq!(decoded.identity, identity);
        assert_eq!(decoded.method_idx, 100);
    }

    #[test]
    fn reply_control_error_rejects_truncated_header_body_and_offset() {
        let encoded = try_encode_reply_control(&ReplyControl::Error(b"hello".to_vec())).unwrap();
        let err = decode_reply_control(&encoded[..3], 0).unwrap_err();
        assert_eq!(
            err.to_string(),
            "reply control error_len: need 5 bytes, have 3"
        );
        let err = decode_reply_control(&encoded[..encoded.len() - 1], 0).unwrap_err();
        assert_eq!(
            err.to_string(),
            "reply control error_data: need 10 bytes, have 9"
        );
        assert_eq!(
            decode_reply_control(&encoded, encoded.len() + 1),
            Err(crate::frame::DecodeError::BufferTooShort { need: 1, have: 0 })
        );
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
    use crate::control::EncodeError;
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
    fn shm_direction_negotiation_bumps_handshake_version() {
        assert_eq!(HANDSHAKE_VERSION, 13);

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
        // Version 12 is the pre-negotiation handshake: it must be rejected in
        // both directions with no compatibility fallback.
        for version in [9_u8, 10, 11, 12] {
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
    fn shm_direction_capabilities_are_distinct_offer_bits() {
        assert_eq!(CAP_SHM_REQUEST, 1 << 3);
        assert_eq!(CAP_SHM_RESPONSE, 1 << 4);
        // The new bits must not collide with the preserved capabilities.
        assert_eq!(
            CAP_SHM_REQUEST & (CAP_CALL_V2 | CAP_METHOD_IDX | CAP_CHUNKED),
            0
        );
        assert_eq!(
            CAP_SHM_RESPONSE & (CAP_CALL_V2 | CAP_METHOD_IDX | CAP_CHUNKED),
            0
        );
        assert_ne!(CAP_SHM_REQUEST, CAP_SHM_RESPONSE);
    }

    #[test]
    fn shm_direction_capabilities_survive_handshake_roundtrip() {
        let cap = CAP_CALL_V2 | CAP_METHOD_IDX | CAP_CHUNKED | CAP_SHM_REQUEST | CAP_SHM_RESPONSE;

        let client = encode_client_handshake(&[], cap, "").unwrap();
        let decoded_client = decode_handshake(&client).unwrap();
        assert_eq!(decoded_client.capability_flags, cap);
        assert_eq!(
            decoded_client.capability_flags & CAP_SHM_REQUEST,
            CAP_SHM_REQUEST
        );
        assert_eq!(
            decoded_client.capability_flags & CAP_SHM_RESPONSE,
            CAP_SHM_RESPONSE
        );

        let server = encode_server_handshake(
            &[],
            cap,
            &[route_hash("grid")],
            "test-prefix",
            &test_identity(),
        )
        .unwrap();
        let decoded_server = decode_handshake(&server).unwrap();
        assert_eq!(decoded_server.capability_flags, cap);

        // Direction bits are independent: an ACK may keep one and drop the
        // other, which is how a one-direction-capable peer negotiates.
        let one_direction = CAP_CALL_V2 | CAP_METHOD_IDX | CAP_CHUNKED | CAP_SHM_RESPONSE;
        let server = encode_server_handshake(
            &[],
            one_direction,
            &[route_hash("grid")],
            "test-prefix",
            &test_identity(),
        )
        .unwrap();
        let decoded_server = decode_handshake(&server).unwrap();
        assert_eq!(decoded_server.capability_flags & CAP_SHM_REQUEST, 0);
        assert_eq!(
            decoded_server.capability_flags & CAP_SHM_RESPONSE,
            CAP_SHM_RESPONSE
        );
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
        let mut routes = vec![RouteInfo {
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

        routes[0].methods.pop();
        let encoded =
            encode_server_handshake(&[], CAP_CALL_V2, &routes, "", &test_identity()).unwrap();
        assert_eq!(
            decode_handshake(&encoded).unwrap().routes[0].methods.len(),
            MAX_METHODS
        );
    }

    #[test]
    fn handshake_segment_count_limit_is_symmetric() {
        let segments: Vec<_> = (0..MAX_SEGMENTS).map(|i| (format!("s{i}"), 256)).collect();
        let encoded = encode_client_handshake(&segments, CAP_CALL_V2, "").unwrap();
        assert_eq!(
            decode_handshake(&encoded).unwrap().segments.len(),
            MAX_SEGMENTS
        );

        let mut excessive = encoded.clone();
        excessive[2..4].copy_from_slice(&((MAX_SEGMENTS + 1) as u16).to_le_bytes());
        let err = decode_handshake(&excessive).unwrap_err();
        assert_eq!(err.to_string(), "segment count: invalid value 17");
        let mut too_many = segments;
        too_many.push(("extra".into(), 256));
        let err = encode_client_handshake(&too_many, CAP_CALL_V2, "").unwrap_err();
        assert!(matches!(
            err,
            EncodeError::FieldTooLong {
                field: "segment count",
                ..
            }
        ));
    }

    #[test]
    fn handshake_route_and_method_count_limits_are_symmetric() {
        let routes: Vec<_> = (0..MAX_ROUTES)
            .map(|i| route(&format!("r{i}"), &[]))
            .collect();
        let encoded =
            encode_server_handshake(&[], CAP_CALL_V2, &routes, "", &test_identity()).unwrap();
        assert_eq!(decode_handshake(&encoded).unwrap().routes.len(), MAX_ROUTES);
        let mut too_many = routes;
        too_many.push(route("extra", &[]));
        let err =
            encode_server_handshake(&[], CAP_CALL_V2, &too_many, "", &test_identity()).unwrap_err();
        assert!(matches!(
            err,
            EncodeError::FieldTooLong {
                field: "route count",
                ..
            }
        ));

        let mut empty =
            encode_server_handshake(&[], CAP_CALL_V2, &[], "", &test_identity()).unwrap();
        let count = empty.len() - 2;
        empty[count..].copy_from_slice(&((MAX_ROUTES + 1) as u16).to_le_bytes());
        let err = decode_handshake(&empty).unwrap_err();
        assert_eq!(err.to_string(), "route count: invalid value 65");

        let mut one = encode_server_handshake(
            &[],
            CAP_CALL_V2,
            &[route("grid", &[])],
            "",
            &test_identity(),
        )
        .unwrap();
        let count = one.len() - 2;
        one[count..].copy_from_slice(&((MAX_METHODS + 1) as u16).to_le_bytes());
        let err = decode_handshake(&one).unwrap_err();
        assert_eq!(err.to_string(), "method count: invalid value 257");
    }

    #[test]
    fn handshake_rejects_truncated_segment_and_route_fields() {
        let client = encode_client_handshake(&[("seg".into(), 256)], CAP_CALL_V2, "").unwrap();
        let err = decode_handshake(&client[..8]).unwrap_err();
        assert_eq!(err.to_string(), "segment entry: need 5 bytes, have 4");
        let err = decode_handshake(&client[..9]).unwrap_err();
        assert_eq!(err.to_string(), "segment name: need 3 bytes, have 0");
        let err = decode_handshake(&client[..client.len() - 1]).unwrap_err();
        assert_eq!(err.to_string(), "capability flags: need 2 bytes, have 1");

        let mut server =
            encode_server_handshake(&[], CAP_CALL_V2, &[], "", &test_identity()).unwrap();
        server.truncate(server.len() - 2); // replace route count with one incomplete route
        server.extend_from_slice(&1u16.to_le_bytes());
        server.extend_from_slice(&[4, b'g']);
        let err = decode_handshake(&server).unwrap_err();
        assert_eq!(err.to_string(), "route name: need 4 bytes, have 1");

        let method = encode_server_handshake(
            &[],
            CAP_CALL_V2,
            &[route("grid", &["get"])],
            "",
            &test_identity(),
        )
        .unwrap();
        let err = decode_handshake(&method[..method.len() - 1]).unwrap_err();
        assert_eq!(err.to_string(), "method index: need 2 bytes, have 1");
    }

    #[test]
    fn handshake_rejects_zero_max_payload_size_on_encode_and_decode() {
        let mut invalid = route("grid", &[]);
        invalid.max_payload_size = 0;
        let err = encode_server_handshake(&[], CAP_CALL_V2, &[invalid], "", &test_identity())
            .unwrap_err();
        assert!(matches!(
            err,
            EncodeError::InvalidText {
                field: "max_payload_size",
                ..
            }
        ));

        let mut encoded = encode_server_handshake(
            &[],
            CAP_CALL_V2,
            &[route("grid", &[])],
            "",
            &test_identity(),
        )
        .unwrap();
        let payload_start = encoded.len() - 10; // 8-byte maximum followed by 2-byte method count
        encoded[payload_start..payload_start + 8].fill(0);
        let err = decode_handshake(&encoded).unwrap_err();
        assert_eq!(err.to_string(), "max_payload_size: invalid value 0");
    }

    #[test]
    fn wrong_version() {
        let buf = [4, 0, 0]; // version 4
        let result = decode_handshake(&buf);
        assert!(result.is_err());
    }

    #[test]
    fn empty_handshake() {
        // Version 13, prefix_len=0, 0 segments, cap_flags=0
        let buf = [13, 0, 0, 0, 0, 0];
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
        let bytes = hex_to_bytes("0200070000000010000000020000000102030405060708");
        let (bp, consumed) = decode_buddy_payload(&bytes).unwrap();
        assert_eq!(bp.seg_idx, 2);
        assert_eq!(bp.generation, 7);
        assert_eq!(bp.offset, 4096);
        assert_eq!(bp.data_size, 512);
        assert!(!bp.is_dedicated);
        assert_eq!(bp.allocation_id, 0x0807_0605_0403_0201);
        assert_eq!(consumed, BUDDY_PAYLOAD_SIZE);
    }

    #[test]
    fn legacy_eleven_byte_buddy_fixture_is_rejected() {
        let bytes = hex_to_bytes("0200001000000002000000");
        assert!(matches!(
            decode_buddy_payload(&bytes),
            Err(DecodeError::BufferTooShort { need: 23, have: 11 })
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
        // v13: [0d][00 prefix_len][01 00 seg_count][00 00 00 10 size][04 seg0][03 00 caps]
        let bytes = hex_to_bytes("0d0001000000001004736567300300");
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
        // v13: client handshake prefix, server identity, then route table
        // with per-route full CRM tag and contract hashes.
        let bytes = hex_to_bytes(
            "0d00010000000008047372763003000b7365727665722d6772696409696e73742d677269640100046772696413677269642d726f7574652d7569642d30303031010000000000000009746573742e67726964044772696405302e312e3040303132333435363738396162636465663031323334353637383961626364656630313233343536373839616263646566303132333435363738396162636465664061626364656630313233343536373839616263646566303132333435363738396162636465663031323334353637383961626364656630313233343536373839000400000000000002000568656c6c6f0000036164640100",
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
            allocation_id: 0x0807_0605_0403_0201,
        };
        let encoded = encode_buddy_payload(&bp);
        let expected = hex_to_bytes("0200070000000010000000020000000102030405060708");
        assert_eq!(encoded.as_slice(), expected.as_slice());
    }

    #[test]
    fn rust_encode_matches_canonical_client_handshake_fixture() {
        let segments = vec![("seg0".into(), 268_435_456u32)];
        let encoded = encode_client_handshake(&segments, CAP_CALL_V2 | CAP_METHOD_IDX, "").unwrap();
        // v13: [0d][00 prefix_len][01 00 seg_count][00 00 00 10 size][04 name_len][seg0][03 00 caps]
        let expected = hex_to_bytes("0d0001000000001004736567300300");
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
        // v13: [0d][00 prefix_len] then segments/caps, identity, and route table
        // with per-route full CRM tag and contract hash metadata.
        let expected = hex_to_bytes(
            "0d00010000000008047372763003000b7365727665722d6772696409696e73742d677269640100046772696413677269642d726f7574652d7569642d30303031010000000000000009746573742e67726964044772696405302e312e3040303132333435363738396162636465663031323334353637383961626364656630313233343536373839616263646566303132333435363738396162636465664061626364656630313233343536373839616263646566303132333435363738396162636465663031323334353637383961626364656630313233343536373839000400000000000002000568656c6c6f0000036164640100",
        );
        assert_eq!(encoded, expected);
    }
}
