use c2_config::CallMetadata;
use c2_error::{C2Error, ErrorCode};
use c2_wire::buddy::BuddyPayload;
use c2_wire::flags::{FLAG_CHUNKED, FLAG_FIRST_CHUNK, FLAG_RPC_RECORD};
use c2_wire::frame::{DecodeError, decode_frame};
use c2_wire::rpc::{
    CallMode, CallStatus, CancelCause, ChunkPosition, MAX_QUERY_CALL_ERROR_BYTES,
    RouteCallIdentity, RpcRecord, ValuePart, ValueStorage, WireCallOutcome, WireCallOutcomeStatus,
    WireLagPolicy, WireQueryCallResult, WireResultUnavailableReason, WireSubscribeOptions,
    WireTopicStart, decode_rpc_record, decode_rpc_record_with_carrier, encode_call_end_frame,
    encode_cancel_frame, encode_credit_frame, encode_item_begin_frame, encode_item_data_frame,
    encode_open_frame, encode_open_keyed_frame, encode_open_recover_frame,
    encode_open_subscribe_frame, encode_query_call_frame, encode_query_call_result_frame,
    encode_ready_frame, encode_subscribe_item_begin_frame, encode_sync_ack_frame,
    encode_sync_frame,
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
        &CallMetadata::empty(),
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
    let open = encode_open_frame(
        2,
        &identity(),
        1,
        CallMode::Unary,
        &CallMetadata::empty(),
        part,
    )
    .unwrap();
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
        &CallMetadata::empty(),
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
        &CallMetadata::empty(),
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
    let metadata = CallMetadata::from_entries([("trace", "call"), ("empty", "")]).unwrap();
    for mode in [CallMode::Unary, CallMode::ServerStream] {
        for value in [
            ValuePart::whole(ValueStorage::Inline(b"x")),
            ValuePart::first(2, ValueStorage::Inline(b"x")),
        ] {
            let wire = encode_open_frame(1, &identity(), 3, mode, &metadata, value).unwrap();
            let (header, payload) = decode_frame(&wire).unwrap();
            let (_, _, _, call_id, decoded_metadata, metadata_len) =
                c2_wire::rpc::decode_open_metadata(payload).unwrap();
            assert_eq!(call_id, None);
            assert_eq!(decoded_metadata, metadata);
            assert_eq!(
                metadata_len,
                c2_wire::rpc::encoded_open_metadata_len(&identity(), &metadata).unwrap()
            );
            let metadata_start = metadata_len - c2_wire::rpc::encoded_call_metadata_len(&metadata);
            assert_eq!(
                &payload[metadata_start - 3..metadata_start],
                &[mode as u8, 3, 0]
            );
            assert!(matches!(decode_rpc_record(&header, payload, None).unwrap(),
                RpcRecord::Open { method_idx: 3, mode: actual, call_id: None, metadata: ref actual_metadata, value: actual_value, .. }
                if actual == mode && actual_value == value && *actual_metadata == metadata));
        }
    }
}

#[test]
fn unknown_open_mode_and_cancel_cause_use_invalid_value() {
    for value in [5u8, 255] {
        let mut wire = encode_open_frame(
            1,
            &identity(),
            0,
            CallMode::Unary,
            &CallMetadata::empty(),
            ValuePart::whole(ValueStorage::Inline(b"")),
        )
        .unwrap();
        let mode_offset = wire.len() - 4;
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
fn invalid_keyed_open_call_id_preserves_shm_carrier_and_unknown_mode_returns_none() {
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
        // Structurally intact KeyedUnary OPEN with invalid call_id characters
        // proves exact carrier offset so the SHM loan is released once.
        let mut keyed =
            encode_open_keyed_frame(1, &identity(), 0, "valid-id", &CallMetadata::empty(), part)
                .unwrap();
        let (_, _, _, _, _, metadata_len) =
            c2_wire::rpc::decode_open_metadata(&keyed[16..]).unwrap();
        // Corrupt one character of call_id to '/' (forbidden).
        keyed[16 + metadata_len - 2] = b'/';
        let (header, payload) = decode_frame(&keyed).unwrap();
        let failure = decode_rpc_record_with_carrier(&header, payload, None).unwrap_err();
        assert!(matches!(
            failure.error,
            DecodeError::InvalidText {
                field: "call_id",
                ..
            }
        ));
        assert_eq!(failure.carrier, Some(descriptor));

        // Unknown CallMode has unprovable carrier offset in IPC 18 -> None
        // (connection retires instead of guessing offset).
        let mut unknown = encode_open_frame(
            1,
            &identity(),
            0,
            CallMode::Unary,
            &CallMetadata::empty(),
            part,
        )
        .unwrap();
        let (_, _, _, _, _, metadata_len) =
            c2_wire::rpc::decode_open_metadata(&unknown[16..]).unwrap();
        let mode_offset = 16 + metadata_len - 4;
        unknown[mode_offset] = 5;
        let (header, payload) = decode_frame(&unknown).unwrap();
        let failure = decode_rpc_record_with_carrier(&header, payload, None).unwrap_err();
        assert_eq!(
            failure.error,
            DecodeError::InvalidValue {
                field: "call mode",
                value: 5
            }
        );
        assert_eq!(failure.carrier, None);
    }
}

#[test]
fn subscribe_open_ready_item_begin_and_call_end_roundtrip_and_reject_invalid() {
    let options = WireSubscribeOptions {
        start: WireTopicStart::Next,
        on_lag: WireLagPolicy::Error,
    };
    let metadata = CallMetadata::from_entries([("trace", "subscribe")]).unwrap();
    let open = encode_open_subscribe_frame(7, &identity(), "state", options, &metadata).unwrap();
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
            metadata: metadata.clone(),
        }
    );

    // Reserved option bits and trailing bytes are rejected.
    let mut bad_opts = open.clone();
    let options_index = bad_opts.len() - c2_wire::rpc::encoded_call_metadata_len(&metadata) - 1;
    bad_opts[options_index] = 0x04;
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
            &CallMetadata::empty(),
            ValuePart::whole(ValueStorage::Inline(b""))
        )
        .is_err()
    );

    // READY round-trips and rejects start_seq = 0.
    let ready = encode_ready_frame(7, 42, 41).unwrap();
    let (h, p) = decode_frame(&ready).unwrap();
    assert_eq!(
        decode_rpc_record(&h, p, Some(CallMode::Subscribe)).unwrap(),
        RpcRecord::Ready {
            start_seq: 42,
            head_seq: 41
        }
    );
    assert!(encode_ready_frame(7, 0, 41).is_err());

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

#[test]
fn keyed_unary_recover_query_call_and_result_roundtrip_and_reject_invalid() {
    let metadata = CallMetadata::from_entries([("token", "keyed one"), ("trace", "a")]).unwrap();
    // KeyedUnary whole + chunked first + metadata len matches exact payload offset.
    for part in [
        ValuePart::whole(ValueStorage::Inline(b"payload")),
        ValuePart::first(12, ValueStorage::Inline(b"payl")),
    ] {
        let wire = encode_open_keyed_frame(9, &identity(), 4, "op_01-AZ", &metadata, part).unwrap();
        let (h, p) = decode_frame(&wire).unwrap();
        let (_, midx, mode, cid, decoded_metadata, meta_len) =
            c2_wire::rpc::decode_open_metadata(p).unwrap();
        assert_eq!(midx, 4);
        assert_eq!(mode, CallMode::KeyedUnary);
        assert_eq!(cid.as_deref(), Some("op_01-AZ"));
        assert_eq!(decoded_metadata, metadata);
        assert_eq!(
            meta_len,
            c2_wire::rpc::encoded_keyed_open_metadata_len(&identity(), "op_01-AZ", &metadata)
                .unwrap()
        );
        assert!(matches!(
            decode_rpc_record(&h, p, None).unwrap(),
            RpcRecord::Open {
                method_idx: 4,
                mode: CallMode::KeyedUnary,
                call_id: Some(ref id),
                metadata: ref actual_metadata,
                value,
                ..
            } if id == "op_01-AZ" && value == part && *actual_metadata == metadata
        ));
    }
    assert!(
        encode_open_keyed_frame(
            9,
            &identity(),
            4,
            "",
            &metadata,
            ValuePart::whole(ValueStorage::Inline(b""))
        )
        .is_err()
    );
    assert!(
        encode_open_keyed_frame(
            9,
            &identity(),
            4,
            "bad/id",
            &metadata,
            ValuePart::whole(ValueStorage::Inline(b""))
        )
        .is_err()
    );

    // Recover OPEN carries no payload and rejects trailing bytes or storage flags.
    let rec = encode_open_recover_frame(11, &identity(), 4, "op_01-AZ", &metadata).unwrap();
    let (h, p) = decode_frame(&rec).unwrap();
    assert!(matches!(
        decode_rpc_record(&h, p, None).unwrap(),
        RpcRecord::OpenRecover {
            method_idx: 4,
            call_id: ref id,
            metadata: ref actual_metadata,
            ..
        } if id == "op_01-AZ" && *actual_metadata == metadata
    ));
    let mut rec_trailing = rec.clone();
    rec_trailing.push(0xAA);
    let new_len = (rec_trailing.len() - 4) as u32;
    rec_trailing[..4].copy_from_slice(&new_len.to_le_bytes());
    let (h, p) = decode_frame(&rec_trailing).unwrap();
    assert!(decode_rpc_record(&h, p, None).is_err());

    // CALL_END works with unary response semantics for KeyedUnary and Recover.
    for mode in [CallMode::KeyedUnary, CallMode::Recover] {
        let ok = encode_call_end_frame(
            11,
            mode,
            CallStatus::Success,
            0,
            ValuePart::whole(ValueStorage::Inline(b"ok")),
        )
        .unwrap();
        let (h, p) = decode_frame(&ok).unwrap();
        assert!(matches!(
            decode_rpc_record(&h, p, Some(mode)).unwrap(),
            RpcRecord::CallEnd {
                status: CallStatus::Success,
                complete_items: None,
                value,
                error: None,
            } if value == ValuePart::whole(ValueStorage::Inline(b"ok"))
        ));
    }

    // QueryCall round-trip and direction/RID validation.
    let q = encode_query_call_frame(15, &identity(), "op_01-AZ", &metadata).unwrap();
    let (h, p) = decode_frame(&q).unwrap();
    assert!(matches!(
        decode_rpc_record(&h, p, None).unwrap(),
        RpcRecord::QueryCall { call_id: ref id, metadata: ref actual_metadata, .. }
            if id == "op_01-AZ" && *actual_metadata == metadata
    ));
    assert!(encode_query_call_frame(0, &identity(), "op_01-AZ", &metadata).is_err());

    // QueryCallResult round-trips every valid outcome variant + error truncation.
    for res in [
        WireQueryCallResult::Outcome(WireCallOutcome::unknown()),
        WireQueryCallResult::Outcome(WireCallOutcome::in_flight("step".into())),
        WireQueryCallResult::Outcome(WireCallOutcome::succeeded("step".into(), true, None)),
        WireQueryCallResult::Outcome(WireCallOutcome::succeeded(
            "step".into(),
            false,
            Some(WireResultUnavailableReason::NotRetained),
        )),
        WireQueryCallResult::Outcome(WireCallOutcome::succeeded(
            "step".into(),
            false,
            Some(WireResultUnavailableReason::Evicted),
        )),
        WireQueryCallResult::Outcome(WireCallOutcome::failed(
            "step".into(),
            ErrorCode::ResourceFunctionExecuting,
            true,
        )),
        WireQueryCallResult::Outcome(WireCallOutcome::failed(
            "step".into(),
            ErrorCode::ResourceFunctionExecuting,
            false,
        )),
        WireQueryCallResult::Error(C2Error::new(
            ErrorCode::ResourceNotFound,
            "missing route",
        )),
    ] {
        let wire = encode_query_call_result_frame(15, &res).unwrap();
        let (h, p) = decode_frame(&wire).unwrap();
        assert_eq!(
            decode_rpc_record(&h, p, None).unwrap(),
            RpcRecord::QueryCallResult(res)
        );
    }

    // Oversized error in QueryCallResult truncates cleanly within MAX_QUERY_CALL_ERROR_BYTES.
    let huge = C2Error::new(ErrorCode::ProtocolViolation, "x".repeat(20_000));
    let wire = encode_query_call_result_frame(16, &WireQueryCallResult::Error(huge)).unwrap();
    let (h, p) = decode_frame(&wire).unwrap();
    assert!(p.len() <= MAX_QUERY_CALL_ERROR_BYTES + 2);
    assert!(matches!(
        decode_rpc_record(&h, p, None).unwrap(),
        RpcRecord::QueryCallResult(WireQueryCallResult::Error(err))
            if err.code == ErrorCode::ProtocolViolation
                && err.details.get("truncated").map(String::as_str) == Some("true")
    ));

    // Invalid WireCallOutcome combinations are rejected at encode boundary.
    assert!(
        encode_query_call_result_frame(
            17,
            &WireQueryCallResult::Outcome(WireCallOutcome {
                status: WireCallOutcomeStatus::Succeeded,
                method_name: Some("step".into()),
                result_available: false,
                result_unavailable_reason: None,
                error_code: None,
                error_details_available: false,
            })
        )
        .is_err()
    );
}

#[test]
fn call_record_finish_replayed_competes_with_reject_before_execution() {
    use c2_wire::connection::calls::CallTable;

    let mut table = CallTable::default();
    let rec1 = table.open(1, ()).unwrap();
    assert_eq!(rec1.finish_replayed(CallStatus::Success), Some(CallStatus::Success));
    assert_eq!(rec1.reject_before_execution(), None);
    assert!(!rec1.enter_resource());

    let rec2 = table.open(2, ()).unwrap();
    assert!(rec2.queue());
    assert_eq!(rec2.reject_before_execution(), Some(CallStatus::NotExecuted));
    assert_eq!(rec2.finish_replayed(CallStatus::Success), None);
}

fn raw_metadata(
    entries: impl IntoIterator<Item = (impl AsRef<[u8]>, impl AsRef<[u8]>)>,
) -> Vec<u8> {
    let mut bytes = vec![0];
    for (key, value) in entries {
        let (key, value) = (key.as_ref(), value.as_ref());
        bytes[0] += 1;
        bytes.push(u8::try_from(key.len()).unwrap());
        bytes.extend_from_slice(key);
        bytes.extend_from_slice(&u16::try_from(value.len()).unwrap().to_le_bytes());
        bytes.extend_from_slice(value);
    }
    bytes
}

#[test]
fn metadata_codec_canonicalizes_empty_and_enforces_individual_and_combined_limits() {
    use c2_config::{
        MAX_CALL_METADATA_ENTRIES, MAX_CALL_METADATA_KEY_LEN, MAX_CALL_METADATA_TOTAL_BYTES,
        MAX_CALL_METADATA_VALUE_LEN,
    };
    use c2_wire::rpc::{
        MAX_WIRE_CALL_METADATA_BYTES, decode_call_metadata, encoded_call_metadata_len,
        write_call_metadata,
    };

    let mut bytes = Vec::new();
    write_call_metadata(&mut bytes, &CallMetadata::empty());
    assert_eq!(bytes, [0]);
    let mut pos = 0;
    assert_eq!(
        decode_call_metadata(&bytes, &mut pos).unwrap(),
        CallMetadata::empty()
    );
    assert_eq!(pos, 1);

    let max_metadata = CallMetadata::from_entries(
        (0..MAX_CALL_METADATA_ENTRIES)
            .rev()
            .map(|i| (format!("k{i:02}"), "v".repeat(253))),
    )
    .unwrap();
    assert_eq!(max_metadata.total_bytes(), MAX_CALL_METADATA_TOTAL_BYTES);
    assert_eq!(encoded_call_metadata_len(&max_metadata), 16_577);
    assert_eq!(
        encoded_call_metadata_len(&max_metadata),
        MAX_WIRE_CALL_METADATA_BYTES
    );
    bytes.clear();
    write_call_metadata(&mut bytes, &max_metadata);
    assert_eq!(bytes.len(), MAX_WIRE_CALL_METADATA_BYTES);
    assert_eq!(bytes[0] as usize, MAX_CALL_METADATA_ENTRIES);
    let mut pos = 0;
    assert_eq!(
        decode_call_metadata(&bytes, &mut pos).unwrap(),
        max_metadata
    );
    assert_eq!(pos, bytes.len());

    let key = "k".repeat(MAX_CALL_METADATA_KEY_LEN);
    let value = "v".repeat(MAX_CALL_METADATA_VALUE_LEN);
    let largest_entry = raw_metadata([(&key, &value)]);
    let mut pos = 0;
    let metadata = decode_call_metadata(&largest_entry, &mut pos).unwrap();
    assert_eq!(metadata.get(&key), Some(value.as_str()));
    assert_eq!(pos, largest_entry.len());

    let too_many = raw_metadata((0..=MAX_CALL_METADATA_ENTRIES).map(|i| (format!("k{i:02}"), "")));
    let too_large = raw_metadata((0..MAX_CALL_METADATA_ENTRIES).map(|i| {
        (
            format!("k{i:02}"),
            "v".repeat(if i == 0 { 254 } else { 253 }),
        )
    }));
    let invalid_segments = [
        too_many,
        too_large,
        raw_metadata([("a", "one"), ("a", "two")]),
        raw_metadata([("b", ""), ("a", "")]),
        raw_metadata([("A", "value")]),
        raw_metadata([("", "")]),
        raw_metadata([("key!", "")]),
        raw_metadata([("é", "")]),
        raw_metadata([("k", " start")]),
        raw_metadata([("k", "end ")]),
        raw_metadata([("k", "a\tb")]),
        raw_metadata([("k", "a\r\nb")]),
        raw_metadata([("k", "\x7f")]),
        raw_metadata([("k", "é")]),
        raw_metadata([(b"key".as_slice(), b"\xff".as_slice())]),
        raw_metadata([(b"\xff".as_slice(), b"".as_slice())]),
        raw_metadata([(format!("{key}k"), String::new())]),
        raw_metadata([(key, format!("{value}v"))]),
    ];
    for segment in invalid_segments {
        let mut bytes = vec![0xff];
        bytes.extend_from_slice(&segment);
        bytes.push(0xff);
        let mut pos = 1;
        let error = decode_call_metadata(&bytes, &mut pos).unwrap_err();
        assert!(error.is_call_metadata_validation(), "{error}");
        assert_eq!(pos, bytes.len() - 1);
    }
}

#[test]
fn invalid_open_metadata_still_exposes_the_original_shm_loan() {
    use c2_wire::rpc::decode_open_metadata;

    let descriptor = BuddyPayload {
        seg_idx: 1,
        generation: 1,
        offset: 7,
        data_size: 1,
        is_dedicated: false,
        loan_id: 43,
    };
    let empty = CallMetadata::empty();
    for value in [
        ValuePart::whole(ValueStorage::Shm(descriptor)),
        ValuePart::first(2, ValueStorage::Shm(descriptor)),
    ] {
        for wire in [
            encode_open_frame(1, &identity(), 3, CallMode::Unary, &empty, value).unwrap(),
            encode_open_frame(1, &identity(), 3, CallMode::ServerStream, &empty, value).unwrap(),
            encode_open_keyed_frame(1, &identity(), 3, "id", &empty, value).unwrap(),
        ] {
            let (header, payload) = decode_frame(&wire).unwrap();
            let (_, _, _, _, _, value_offset) = decode_open_metadata(payload).unwrap();
            for segment in [
                raw_metadata([("A", "value")]),
                raw_metadata([("a", "value ")]),
                raw_metadata([("a", "one"), ("a", "two")]),
                raw_metadata([("b", ""), ("a", "")]),
                raw_metadata((0..65).map(|i| (format!("k{i:02}"), ""))),
            ] {
                let malformed = [
                    &payload[..value_offset - 1],
                    &segment,
                    &payload[value_offset..],
                ]
                .concat();
                let mut header = header;
                header.total_len = 12 + malformed.len() as u32;
                let failure =
                    decode_rpc_record_with_carrier(&header, &malformed, None).unwrap_err();
                assert!(
                    failure.error.is_call_metadata_validation(),
                    "{}",
                    failure.error
                );
                assert_eq!(failure.carrier, Some(descriptor));

                let truncated = &malformed[..malformed.len() - 1];
                header.total_len -= 1;
                let failure = decode_rpc_record_with_carrier(&header, truncated, None).unwrap_err();
                assert!(
                    !failure.error.is_call_metadata_validation(),
                    "{}",
                    failure.error
                );
                assert_eq!(failure.carrier, None);
            }
        }
    }
}

#[test]
fn control_and_valueless_open_records_validate_structure_before_metadata_semantics() {
    use c2_wire::rpc::encode_admit_topic_frame;

    let empty = CallMetadata::empty();
    let segment = raw_metadata([("A", "first"), ("b", " trailing "), ("c", "last")]);
    let options = WireSubscribeOptions {
        start: WireTopicStart::Next,
        on_lag: WireLagPolicy::Error,
    };
    for wire in [
        encode_query_call_frame(15, &identity(), "id", &empty).unwrap(),
        encode_admit_topic_frame(15, 14, &empty).unwrap(),
        encode_open_recover_frame(15, &identity(), 3, "id", &empty).unwrap(),
        encode_open_subscribe_frame(15, &identity(), "state", options, &empty).unwrap(),
    ] {
        let (mut header, payload) = decode_frame(&wire).unwrap();
        let mut malformed = [&payload[..payload.len() - 1], &segment].concat();
        header.total_len = 12 + malformed.len() as u32;
        let error = decode_rpc_record(&header, &malformed, None).unwrap_err();
        assert!(error.is_call_metadata_validation(), "{error}");
        assert!(
            error.to_string().contains("key must contain only"),
            "{error}"
        );

        let truncated = &malformed[..malformed.len() - 1];
        header.total_len -= 1;
        let error = decode_rpc_record(&header, truncated, None).unwrap_err();
        assert!(!error.is_call_metadata_validation());
        assert!(
            matches!(
                error,
                DecodeError::Truncated {
                    field: "call_metadata value",
                    ..
                }
            ),
            "{error}"
        );

        malformed.push(0xff);
        header.total_len += 2;
        let error = decode_rpc_record(&header, &malformed, None).unwrap_err();
        assert!(!error.is_call_metadata_validation());
        assert!(
            matches!(error, DecodeError::InvalidValue { value: 1, .. }),
            "{error}"
        );
    }
}

#[test]
fn ready_requires_two_sequences_and_a_start_at_or_before_the_next_head() {
    for (start_seq, head_seq) in [(1, 0), (4, 5), (6, 5), (u64::MAX, u64::MAX)] {
        let wire = encode_ready_frame(1, start_seq, head_seq).unwrap();
        let (header, payload) = decode_frame(&wire).unwrap();
        assert_eq!(payload.len(), 17);
        assert_eq!(
            decode_rpc_record(&header, payload, Some(CallMode::Subscribe)).unwrap(),
            RpcRecord::Ready {
                start_seq,
                head_seq
            }
        );

        let mut shortened = header;
        shortened.total_len -= 8;
        assert!(decode_rpc_record(&shortened, &payload[..9], None).is_err());

        for invalid_start in [0, head_seq.saturating_add(2)] {
            if invalid_start == u64::MAX {
                continue;
            }
            assert!(encode_ready_frame(1, invalid_start, head_seq).is_err());
            let mut malformed = payload.to_vec();
            malformed[1..9].copy_from_slice(&invalid_start.to_le_bytes());
            assert!(decode_rpc_record(&header, &malformed, None).is_err());
        }
    }
    assert!(encode_ready_frame(0, 1, 1).is_err());
}

#[test]
fn admit_topic_roundtrips_and_enforces_direction_request_id_and_storage() {
    use c2_wire::flags::{FLAG_BUDDY, FLAG_RESPONSE};
    use c2_wire::rpc::{
        WireAdmitTopicResult, encode_admit_topic_draft_frame, encode_admit_topic_frame,
        encode_admit_topic_result_frame,
    };

    let metadata = CallMetadata::from_entries([("token", "allowed")]).unwrap();
    let request = encode_admit_topic_frame(42, 7, &metadata).unwrap();
    let mut draft = encode_admit_topic_draft_frame(7, &metadata).unwrap();
    draft[4..12].copy_from_slice(&42u64.to_le_bytes());
    assert_eq!(draft, request);
    assert!(encode_admit_topic_frame(0, 7, &metadata).is_err());
    assert!(encode_admit_topic_frame(42, 0, &metadata).is_err());
    assert!(encode_admit_topic_draft_frame(0, &metadata).is_err());
    let (header, payload) = decode_frame(&request).unwrap();
    assert_eq!(
        decode_rpc_record(&header, payload, None).unwrap(),
        RpcRecord::AdmitTopic {
            subscription_stream_id: 7,
            metadata
        }
    );
    let mut zero_stream = payload.to_vec();
    zero_stream[1..9].fill(0);
    assert!(decode_rpc_record(&header, &zero_stream, None).is_err());

    let allowed = WireAdmitTopicResult::Allowed { head_seq: 0 };
    let denied = WireAdmitTopicResult::Error(C2Error::new(ErrorCode::AdmissionDenied, "denied"));
    let allowed_wire = encode_admit_topic_result_frame(42, &allowed).unwrap();
    let denied_wire = encode_admit_topic_result_frame(42, &denied).unwrap();
    for (bytes, expected) in [(&allowed_wire, allowed), (&denied_wire, denied)] {
        let (header, payload) = decode_frame(bytes).unwrap();
        assert_eq!(
            decode_rpc_record(&header, payload, None).unwrap(),
            RpcRecord::AdmitTopicResult(expected)
        );
    }
    assert!(
        encode_admit_topic_result_frame(0, &WireAdmitTopicResult::Allowed { head_seq: 0 }).is_err()
    );
    for bytes in [&request, &allowed_wire, &denied_wire] {
        let (header, payload) = decode_frame(bytes).unwrap();
        let mut bad = header;
        bad.flags ^= FLAG_RESPONSE;
        assert!(decode_rpc_record(&bad, payload, None).is_err());
        bad = header;
        bad.flags |= FLAG_BUDDY;
        assert!(decode_rpc_record(&bad, payload, None).is_err());
        bad = header;
        bad.flags |= FLAG_CHUNKED;
        assert!(decode_rpc_record(&bad, payload, None).is_err());
        bad = header;
        bad.request_id = 0;
        assert!(decode_rpc_record(&bad, payload, None).is_err());
    }

    let huge = C2Error::new(ErrorCode::AdmissionDenied, "x".repeat(20_000));
    let wire = encode_admit_topic_result_frame(42, &WireAdmitTopicResult::Error(huge)).unwrap();
    let (mut header, payload) = decode_frame(&wire).unwrap();
    assert!(payload.len() <= MAX_QUERY_CALL_ERROR_BYTES + 2);
    assert!(matches!(
        decode_rpc_record(&header, payload, None).unwrap(),
        RpcRecord::AdmitTopicResult(WireAdmitTopicResult::Error(error))
            if error.code == ErrorCode::AdmissionDenied
                && error.details.get("truncated").map(String::as_str) == Some("true")
    ));

    let oversized = [&[14, 1][..], &[0; MAX_QUERY_CALL_ERROR_BYTES + 1][..]].concat();
    for malformed in [
        &[14, 2][..],                            // Unknown tag.
        &[14, 0, 0][..],                         // Short allowed result.
        &[14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0][..], // Extra allowed byte.
        &[14, 1][..],                            // Empty error.
        b"\x0e\x01{}",
        &oversized,
    ] {
        header.total_len = 12 + malformed.len() as u32;
        assert!(decode_rpc_record(&header, malformed, None).is_err());
    }
}
