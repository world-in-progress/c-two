use c2_wire::buddy::BuddyPayload;
use c2_wire::flags::{FLAG_FIRST_FRAGMENT, FLAG_FRAGMENTED, FLAG_RPC_RECORD};
use c2_wire::frame::{DecodeError, decode_frame};
use c2_wire::rpc::{
    CallMode, CallStatus, FragmentPosition, RouteCallIdentity, RpcRecord, ValuePart, ValueStorage,
    decode_rpc_record, decode_rpc_record_with_carrier, encode_call_end_frame, encode_cancel_frame,
    encode_credit_frame, encode_item_begin_frame, encode_item_data_frame, encode_open_frame,
};

fn identity() -> RouteCallIdentity<&'static str> {
    RouteCallIdentity {
        route_name: "grid",
        route_uid: "uid",
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
                fragment: FragmentPosition::Whole,
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
fn fragment_first_is_strictly_less_than_total_and_zero_loan_is_typed() {
    let part = ValuePart::first(3, ValueStorage::Inline(b"x"));
    let open = encode_open_frame(2, &identity(), 1, part).unwrap();
    let (header, payload) = decode_frame(&open).unwrap();
    assert_eq!(
        header.flags,
        FLAG_RPC_RECORD | FLAG_FRAGMENTED | FLAG_FIRST_FRAGMENT
    );
    assert!(matches!(
        decode_rpc_record(&header, payload, None).unwrap(),
        RpcRecord::Open {
            value: ValuePart {
                fragment: FragmentPosition::First { total_bytes: 3 },
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
            complete_items: Some(0),
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
    assert!(decode_rpc_record(&header, payload, None).is_err());
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
        RpcRecord::ItemBegin { total_bytes: 12 }
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

    let cancel = encode_cancel_frame(5).unwrap();
    let (header, payload) = decode_frame(&cancel).unwrap();
    assert_eq!(payload, [5]);
    assert!(matches!(
        decode_rpc_record(&header, payload, None).unwrap(),
        RpcRecord::Cancel
    ));
    let legacy_flag_header = c2_wire::frame::FrameHeader {
        flags: header.flags | 0x0400,
        ..header
    };
    assert!(decode_rpc_record(&legacy_flag_header, payload, None).is_err());
}
