use c2_error::{C2Error, ErrorCode};
use c2_wire::buddy::BuddyPayload;
use c2_wire::flags::{FLAG_CHUNKED, FLAG_FIRST_CHUNK, FLAG_RPC_RECORD};
use c2_wire::frame::{DecodeError, decode_frame};
use c2_wire::rpc::{
    CallMode, CallStatus, CancelCause, ChunkPosition, RouteCallIdentity, RpcRecord, ValuePart,
    ValueStorage, WireLagPolicy, WireSubscribeOptions, WireTopicStart, decode_rpc_record,
    decode_rpc_record_with_carrier, encode_call_end_frame, encode_cancel_frame,
    encode_credit_frame, encode_item_begin_frame, encode_item_data_frame, encode_open_frame,
    encode_open_subscribe_frame, encode_ready_frame, encode_subscribe_item_begin_frame,
    encode_sync_ack_frame, encode_sync_frame,
};

#[test]
fn subscription_sync_records_are_exact_and_directional() {
    let sync = encode_sync_frame(7).unwrap();
    let (header, payload) = decode_frame(&sync).unwrap();
    assert_eq!(payload, [9]);
    assert_eq!(
        decode_rpc_record(&header, payload, Some(CallMode::Subscribe)).unwrap(),
        RpcRecord::Sync
    );
    let ack = encode_sync_ack_frame(7, 0).unwrap();
    let (header, payload) = decode_frame(&ack).unwrap();
    assert_eq!(payload, [10, 0, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(
        decode_rpc_record(&header, payload, Some(CallMode::Subscribe)).unwrap(),
        RpcRecord::SyncAck { head_seq: 0 }
    );
    assert!(encode_sync_frame(0).is_err());
    assert!(encode_sync_ack_frame(0, 1).is_err());
    for bytes in [&sync, &ack] {
        let (header, payload) = decode_frame(bytes).unwrap();
        let mut wrong_direction = header;
        wrong_direction.flags ^= c2_wire::flags::FLAG_RESPONSE;
        assert!(decode_rpc_record(&wrong_direction, payload, Some(CallMode::Subscribe)).is_err());
        let mut wrong_storage = header;
        wrong_storage.flags |= c2_wire::flags::FLAG_BUDDY;
        assert!(decode_rpc_record(&wrong_storage, payload, Some(CallMode::Subscribe)).is_err());
        let mut wrong_length = header;
        wrong_length.total_len -= 1;
        assert!(
            decode_rpc_record(
                &wrong_length,
                &payload[..payload.len() - 1],
                Some(CallMode::Subscribe)
            )
            .is_err()
        );
    }
}

fn identity() -> RouteCallIdentity<&'static str> {
    RouteCallIdentity {
        route_name: "grid",
        crm_ns: "ns",
        crm_name: "Grid",
        crm_ver: "1",
        abi_hash: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        signature_hash: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
    }
}

#[test]
fn open_and_end_have_one_record_inside_the_sixteen_byte_envelope() {
    let open = encode_open_frame(
        9,
        &identity(),
        3,
        c2_wire::rpc::CallMode::Unary,
        ValuePart::whole(ValueStorage::Inline(b"x")),
    )
    .unwrap();
    let (header, payload) = decode_frame(&open).unwrap();
    assert_eq!(header.request_id, 9);
    assert_eq!(header.flags, FLAG_RPC_RECORD);
    assert_eq!(payload[0], 1);
    assert_eq!(payload[1], 4);
    assert_eq!(&payload[2..6], b"grid");
    assert!(matches!(
        decode_rpc_record(&header, payload, None).unwrap(),
        RpcRecord::Open {
            method_idx: 3,
            value: ValuePart {
                chunk: ChunkPosition::Whole,
                storage: ValueStorage::Inline(b"x"),
                ..
            },
            ..
        }
    ));

    let end = encode_call_end_frame(
        9,
        CallMode::Unary,
        CallStatus::Success,
        0,
        ValuePart::whole(ValueStorage::Inline(b"ok")),
    )
    .unwrap();
    let (header, payload) = decode_frame(&end).unwrap();
    assert_eq!(header.flags, FLAG_RPC_RECORD | 2);
    assert_eq!(payload, [6, 0, b'o', b'k']);
    assert!(matches!(
        decode_rpc_record(&header, payload, Some(CallMode::Unary)).unwrap(),
        RpcRecord::CallEnd {
            status: CallStatus::Success,
            complete_items: None,
            ..
        }
    ));
}

#[test]
fn chunk_first_is_strictly_less_than_total_and_zero_loan_is_typed() {
    let part = ValuePart::first(3, ValueStorage::Inline(b"x"));
    let open = encode_open_frame(2, &identity(), 1, c2_wire::rpc::CallMode::Unary, part).unwrap();
    let (header, payload) = decode_frame(&open).unwrap();
    assert_eq!(
        header.flags,
        FLAG_RPC_RECORD | FLAG_CHUNKED | FLAG_FIRST_CHUNK
    );
    assert!(matches!(
        decode_rpc_record(&header, payload, None).unwrap(),
        RpcRecord::Open {
            value: ValuePart {
                chunk: ChunkPosition::First { total_bytes: 3 },
                ..
            },
            ..
        }
    ));
    let mut bad = open.clone();
    let total_start = bad.len() - 9;
    bad[total_start..total_start + 8].copy_from_slice(&1u64.to_le_bytes());
    let (header, payload) = decode_frame(&bad).unwrap();
    assert!(decode_rpc_record(&header, payload, None).is_err());

    let descriptor = BuddyPayload {
        seg_idx: 1,
        generation: 1,
        offset: 0,
        data_size: 1,
        is_dedicated: false,
        loan_id: 0,
    };
    let wire = encode_open_frame(
        2,
        &identity(),
        1,
        c2_wire::rpc::CallMode::Unary,
        ValuePart::whole(ValueStorage::Shm(descriptor)),
    )
    .unwrap();
    let (header, payload) = decode_frame(&wire).unwrap();
    assert_eq!(
        decode_rpc_record(&header, payload, None).unwrap_err(),
        DecodeError::ZeroLoanId
    );
}

#[test]
fn canonical_errors_are_inline_and_mode_controls_complete_items() {
    let error =
        br#"C2E1{"version":1,"code":713,"name":"ProtocolViolation","message":"bad","details":{}}"#;
    let end = encode_call_end_frame(
        4,
        CallMode::ServerStream,
        CallStatus::NotExecuted,
        0,
        ValuePart::whole(ValueStorage::Inline(error)),
    )
    .unwrap();
    let (header, payload) = decode_frame(&end).unwrap();
    assert!(matches!(
        decode_rpc_record(&header, payload, Some(CallMode::ServerStream)).unwrap(),
        RpcRecord::CallEnd {
            complete_items: None,
            error: Some(err),
            ..
        } if err.message == "bad" && err.code as u16 == 713
    ));
    assert!(
        encode_call_end_frame(
            4,
            CallMode::Unary,
            CallStatus::Failed,
            0,
            ValuePart::whole(ValueStorage::Inline(b"bad"))
        )
        .is_err()
    );
    assert!(matches!(
        decode_rpc_record(&header, payload, None).unwrap(),
        RpcRecord::CallEnd {
            status: CallStatus::NotExecuted,
            complete_items: None,
            ..
        }
    ));
}

#[test]
fn call_end_status_selects_exact_metadata_layout() {
    for code in [ErrorCode::ResourceRemoved, ErrorCode::ContractMismatch] {
        let error = C2Error::new(code, "refused").to_wire_bytes();
        for mode in [CallMode::Unary, CallMode::ServerStream] {
            let wire = encode_call_end_frame(
                11,
                mode,
                CallStatus::NotExecuted,
                0,
                ValuePart::whole(ValueStorage::Inline(&error)),
            )
            .unwrap();
            let (header, payload) = decode_frame(&wire).unwrap();
            assert_eq!(payload, [&[6, 2][..], error.as_slice()].concat());
            for decode_mode in [None, Some(CallMode::Unary), Some(CallMode::ServerStream)] {
                let decoded = decode_rpc_record(&header, payload, decode_mode).unwrap();
                assert!(matches!(
                    decoded,
                    RpcRecord::CallEnd { status: CallStatus::NotExecuted, complete_items: None, error: Some(ref actual), .. }
                        if actual.code == code
                ));
            }
        }
    }

    let failed = C2Error::new(ErrorCode::ContractMismatch, "failed").to_wire_bytes();
    for (mode, status, count, value) in [
        (CallMode::Unary, CallStatus::Success, 0, b"ok".as_slice()),
        (CallMode::Unary, CallStatus::Failed, 0, failed.as_slice()),
        (
            CallMode::ServerStream,
            CallStatus::Success,
            7,
            b"".as_slice(),
        ),
        (
            CallMode::ServerStream,
            CallStatus::Failed,
            3,
            failed.as_slice(),
        ),
    ] {
        let wire = encode_call_end_frame(
            11,
            mode,
            status,
            count,
            ValuePart::whole(ValueStorage::Inline(value)),
        )
        .unwrap();
        let (header, payload) = decode_frame(&wire).unwrap();
        let expected_count = if mode == CallMode::ServerStream {
            Some(count)
        } else {
            None
        };
        let value_offset = if expected_count.is_some() { 10 } else { 2 };
        assert_eq!(&payload[value_offset..], value);
        if expected_count.is_some() {
            assert_eq!(&payload[2..10], &count.to_le_bytes());
        }
        assert!(matches!(
            decode_rpc_record(&header, payload, Some(mode)).unwrap(),
            RpcRecord::CallEnd { status: actual_status, complete_items, .. }
                if actual_status == status && complete_items == expected_count
        ));
        assert!(decode_rpc_record(&header, payload, None).is_err());
    }

    assert!(
        encode_call_end_frame(
            11,
            CallMode::Unary,
            CallStatus::Success,
            1,
            ValuePart::whole(ValueStorage::Inline(b"ok"))
        )
        .is_err()
    );
    assert!(
        encode_call_end_frame(
            11,
            CallMode::Unary,
            CallStatus::Failed,
            1,
            ValuePart::whole(ValueStorage::Inline(&failed))
        )
        .is_err()
    );
    assert!(
        encode_call_end_frame(
            11,
            CallMode::ServerStream,
            CallStatus::NotExecuted,
            1,
            ValuePart::whole(ValueStorage::Inline(&failed))
        )
        .is_err()
    );
}

#[test]
fn invalid_call_end_preserves_only_the_actual_shm_carrier() {
    let descriptor = BuddyPayload {
        seg_idx: 1,
        generation: 1,
        offset: 0,
        data_size: 1,
        is_dedicated: false,
        loan_id: 42,
    };
    for (mode, status, count) in [
        (None, CallStatus::NotExecuted, None),
        (Some(CallMode::ServerStream), CallStatus::NotExecuted, None),
        (Some(CallMode::ServerStream), CallStatus::Failed, Some(2u64)),
    ] {
        let mut payload = vec![6, status as u8];
        if let Some(count) = count {
            payload.extend_from_slice(&count.to_le_bytes());
        }
        payload.extend_from_slice(&c2_wire::buddy::encode_buddy_payload(&descriptor));
        let frame = c2_wire::frame::encode_frame(
            11,
            FLAG_RPC_RECORD | 2 | c2_wire::flags::FLAG_BUDDY,
            &payload,
        );
        let (header, payload) = decode_frame(&frame).unwrap();
        let failure = decode_rpc_record_with_carrier(&header, payload, mode).unwrap_err();
        assert_eq!(failure.carrier, Some(descriptor));
    }

    let error = C2Error::new(ErrorCode::ResourceRemoved, "refused").to_wire_bytes();
    let mut payload = vec![6, CallStatus::NotExecuted as u8];
    payload.extend_from_slice(&0u64.to_le_bytes());
    payload.extend_from_slice(&error);
    let frame = c2_wire::frame::encode_frame(11, FLAG_RPC_RECORD | 2, &payload);
    let (header, payload) = decode_frame(&frame).unwrap();
    assert_eq!(
        decode_rpc_record_with_carrier(&header, payload, Some(CallMode::ServerStream))
            .unwrap_err()
            .carrier,
        None
    );

    let mut payload = vec![6, CallStatus::NotExecuted as u8];
    payload.extend_from_slice(&0u64.to_le_bytes());
    payload.extend_from_slice(&c2_wire::buddy::encode_buddy_payload(&descriptor));
    let frame = c2_wire::frame::encode_frame(
        11,
        FLAG_RPC_RECORD | 2 | c2_wire::flags::FLAG_BUDDY,
        &payload,
    );
    let (header, payload) = decode_frame(&frame).unwrap();
    assert_eq!(
        decode_rpc_record_with_carrier(&header, payload, Some(CallMode::ServerStream))
            .unwrap_err()
            .carrier,
        None
    );
}

#[test]
fn invalid_open_identity_still_exposes_original_shm_carrier() {
    let descriptor = BuddyPayload {
        seg_idx: 1,
        generation: 1,
        offset: 0,
        data_size: 1,
        is_dedicated: false,
        loan_id: 42,
    };
    let mut wire = encode_open_frame(
        8,
        &identity(),
        1,
        c2_wire::rpc::CallMode::Unary,
        ValuePart::whole(ValueStorage::Shm(descriptor)),
    )
    .unwrap();
    wire[23] = b'/';
    let (header, payload) = decode_frame(&wire).unwrap();
    let failure = decode_rpc_record_with_carrier(&header, payload, None).unwrap_err();
    assert_eq!(failure.carrier, Some(descriptor));
}

#[test]
fn future_record_kinds_have_exact_shapes_and_reject_retired_flag_bit() {
    let begin = encode_item_begin_frame(5, 12).unwrap();
    let (header, payload) = decode_frame(&begin).unwrap();
    assert_eq!(payload, [2, 12, 0, 0, 0, 0, 0, 0, 0]);
    assert!(matches!(
        decode_rpc_record(&header, payload, None).unwrap(),
        RpcRecord::ItemBegin {
            total_bytes: 12,
            seq: None
        }
    ));

    let data = encode_item_data_frame(5, ValueStorage::Inline(b"ab")).unwrap();
    let (header, payload) = decode_frame(&data).unwrap();
    assert_eq!(payload, [3, b'a', b'b']);
    assert!(matches!(
        decode_rpc_record(&header, payload, None).unwrap(),
        RpcRecord::ItemData { .. }
    ));

    let credit = encode_credit_frame(5, 12, 1).unwrap();
    let (header, payload) = decode_frame(&credit).unwrap();
    assert_eq!(payload[0], 4);
    assert_eq!(&payload[1..9], &12u64.to_le_bytes());
    assert_eq!(&payload[9..17], &1u64.to_le_bytes());
    assert!(matches!(
        decode_rpc_record(&header, payload, None).unwrap(),
        RpcRecord::Credit {
            storage_bytes: 12,
            item_demand: 1
        }
    ));
    assert!(encode_credit_frame(5, 0, 0).is_err());

    let cancel = encode_cancel_frame(5, CancelCause::Consumer).unwrap();
    let (header, payload) = decode_frame(&cancel).unwrap();
    assert_eq!(payload, [5, 0]);
    assert!(matches!(
        decode_rpc_record(&header, payload, None).unwrap(),
        RpcRecord::Cancel {
            cause: CancelCause::Consumer
        }
    ));
    let lifecycle = encode_cancel_frame(5, CancelCause::Lifecycle).unwrap();
    let (lifecycle_header, lifecycle_payload) = decode_frame(&lifecycle).unwrap();
    assert_eq!(lifecycle_payload, [5, 1]);
    assert!(matches!(
        decode_rpc_record(&lifecycle_header, lifecycle_payload, None).unwrap(),
        RpcRecord::Cancel {
            cause: CancelCause::Lifecycle
        }
    ));
    assert!(decode_rpc_record(&header, &[5], None).is_err());
    assert!(decode_rpc_record(&header, &[5, 2], None).is_err());
    assert!(decode_rpc_record(&header, &[5, 0, 0], None).is_err());
    assert!(encode_cancel_frame(0, CancelCause::Consumer).is_err());
    let legacy_flag_header = c2_wire::frame::FrameHeader {
        flags: header.flags | 0x0400,
        ..header
    };
    assert!(decode_rpc_record(&legacy_flag_header, payload, None).is_err());
}

#[test]
fn open_roundtrips_call_mode_in_whole_and_first_chunk() {
    for mode in [CallMode::Unary, CallMode::ServerStream] {
        for value in [
            ValuePart::whole(ValueStorage::Inline(b"x")),
            ValuePart::first(2, ValueStorage::Inline(b"x")),
        ] {
            let wire = encode_open_frame(1, &identity(), 3, mode, value).unwrap();
            let (header, payload) = decode_frame(&wire).unwrap();
            let (_, _, _, metadata_len) = c2_wire::rpc::decode_open_metadata(payload).unwrap();
            assert_eq!(
                &payload[metadata_len - 3..metadata_len],
                &[mode as u8, 3, 0]
            );
            assert!(matches!(decode_rpc_record(&header, payload, None).unwrap(),
                RpcRecord::Open { method_idx: 3, mode: actual, value: actual_value, .. }
                if actual == mode && actual_value == value));
        }
    }
}

#[test]
fn unknown_open_mode_and_cancel_cause_use_invalid_value() {
    for value in [3u8, 255] {
        let mut wire = encode_open_frame(
            1,
            &identity(),
            0,
            CallMode::Unary,
            ValuePart::whole(ValueStorage::Inline(b"")),
        )
        .unwrap();
        let mode_offset = wire.len() - 3;
        wire[mode_offset] = value;
        let (header, payload) = decode_frame(&wire).unwrap();
        assert_eq!(
            decode_rpc_record(&header, payload, None).unwrap_err(),
            DecodeError::InvalidValue {
                field: "call mode",
                value: value.into()
            }
        );
        let mut cancel = encode_cancel_frame(1, CancelCause::Consumer).unwrap();
        *cancel.last_mut().unwrap() = value;
        let (header, payload) = decode_frame(&cancel).unwrap();
        assert_eq!(
            decode_rpc_record(&header, payload, None).unwrap_err(),
            DecodeError::InvalidValue {
                field: "CANCEL cause",
                value: value.into()
            }
        );
    }
}

#[test]
fn unknown_open_mode_preserves_whole_and_first_chunk_shm_carrier() {
    let descriptor = BuddyPayload {
        seg_idx: 1,
        generation: 1,
        offset: 0,
        data_size: 1,
        is_dedicated: false,
        loan_id: 42,
    };
    for part in [
        ValuePart::whole(ValueStorage::Shm(descriptor)),
        ValuePart::first(2, ValueStorage::Shm(descriptor)),
    ] {
        let mut wire = encode_open_frame(1, &identity(), 0, CallMode::Unary, part).unwrap();
        let (_, _, _, metadata_len) = c2_wire::rpc::decode_open_metadata(&wire[16..]).unwrap();
        let mode_offset = 16 + metadata_len - 3;
        wire[mode_offset] = 3;
        let (header, payload) = decode_frame(&wire).unwrap();
        let failure = decode_rpc_record_with_carrier(&header, payload, None).unwrap_err();
        assert_eq!(
            failure.error,
            DecodeError::InvalidValue {
                field: "call mode",
                value: 3
            }
        );
        assert_eq!(failure.carrier, Some(descriptor));
    }
}

#[test]
fn subscribe_open_ready_item_begin_and_call_end_roundtrip_and_reject_invalid() {
    let options = WireSubscribeOptions {
        start: WireTopicStart::Next,
        on_lag: WireLagPolicy::Error,
    };
    let open = encode_open_subscribe_frame(7, &identity(), "state", options).unwrap();
    let (header, payload) = decode_frame(&open).unwrap();
    assert_eq!(
        decode_rpc_record(&header, payload, None).unwrap(),
        RpcRecord::OpenSubscribe {
            identity: RouteCallIdentity {
                route_name: "grid".into(),
                crm_ns: "ns".into(),
                crm_name: "Grid".into(),
                crm_ver: "1".into(),
                abi_hash: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                signature_hash: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    .into(),
            },
            topic_name: "state".into(),
            options,
        }
    );

    // Reserved option bits and trailing bytes are rejected.
    let mut bad_opts = open.clone();
    *bad_opts.last_mut().unwrap() = 0x04;
    let (h, p) = decode_frame(&bad_opts).unwrap();
    assert_eq!(
        decode_rpc_record(&h, p, None).unwrap_err(),
        DecodeError::InvalidValue {
            field: "subscribe options",
            value: 4,
        }
    );
    assert!(
        encode_open_frame(
            7,
            &identity(),
            0,
            CallMode::Subscribe,
            ValuePart::whole(ValueStorage::Inline(b""))
        )
        .is_err()
    );

    // READY round-trips and rejects start_seq = 0.
    let ready = encode_ready_frame(7, 42).unwrap();
    let (h, p) = decode_frame(&ready).unwrap();
    assert_eq!(
        decode_rpc_record(&h, p, Some(CallMode::Subscribe)).unwrap(),
        RpcRecord::Ready { start_seq: 42 }
    );
    assert!(encode_ready_frame(7, 0).is_err());

    // Subscribe ITEM_BEGIN carries (seq, total_bytes) and rejects seq = 0 or wrong mode.
    let begin = encode_subscribe_item_begin_frame(7, 42, 1024).unwrap();
    let (h, p) = decode_frame(&begin).unwrap();
    assert_eq!(
        decode_rpc_record(&h, p, Some(CallMode::Subscribe)).unwrap(),
        RpcRecord::ItemBegin {
            total_bytes: 1024,
            seq: Some(42),
        }
    );
    assert!(decode_rpc_record(&h, p, Some(CallMode::ServerStream)).is_err());
    assert!(decode_rpc_record(&h, p, Some(CallMode::Unary)).is_err());
    assert!(encode_subscribe_item_begin_frame(7, 0, 1024).is_err());

    // Subscribe CALL_END rejects Success and accepts Failed (with complete_items) and NotExecuted.
    assert!(
        encode_call_end_frame(
            7,
            CallMode::Subscribe,
            CallStatus::Success,
            0,
            ValuePart::whole(ValueStorage::Inline(b""))
        )
        .is_err()
    );
    let lagged = C2Error::new(ErrorCode::SubscriptionLagged, "lagged").to_wire_bytes();
    let end = encode_call_end_frame(
        7,
        CallMode::Subscribe,
        CallStatus::Failed,
        9,
        ValuePart::whole(ValueStorage::Inline(&lagged)),
    )
    .unwrap();
    let (h, p) = decode_frame(&end).unwrap();
    assert!(matches!(
        decode_rpc_record(&h, p, Some(CallMode::Subscribe)).unwrap(),
        RpcRecord::CallEnd {
            status: CallStatus::Failed,
            complete_items: Some(9),
            error: Some(err),
            ..
        } if err.code == ErrorCode::SubscriptionLagged
    ));
}

#[test]
fn connection_core_subscribe_open_begins_items_without_resource_step() {
    use c2_wire::connection::{ConnectionCore, ConnectionSide};

    let server = ConnectionCore::new(ConnectionSide::Server, || {});
    server.finish_handshake().unwrap();
    server.subscribe_open(5).unwrap();
    assert!(server.stream_start_step(5).is_err());
    // Consumes initial demand = 1 without calling stream_start_step.
    server.stream_begin_item(5, 16).unwrap();
    assert_eq!(server.stream_state(5).unwrap().demand, 0);
    server.stream_credit(5, 16, 0).unwrap();
    assert_eq!(server.stream_data(5, 16).unwrap(), 0);
    server.stream_finish_item(5).unwrap();
    assert_eq!(server.stream_state(5).unwrap().complete_items, 1);
    // Next item requires demand credit first.
    assert!(server.stream_begin_item(5, 0).is_err());
    server.stream_credit(5, 0, 1).unwrap();
    server.stream_begin_item(5, 0).unwrap();
    assert_eq!(server.stream_state(5).unwrap().complete_items, 2);
}
