//! Handwritten IPC 18 whole-frame fixtures for the public RPC encoders.
//! Keep the expected headers and bodies independent of the production codec.

use crate::buddy::BuddyPayload;
use crate::frame::decode_frame;
use crate::rpc::{
    CallMode, CallStatus, CancelCause, MAX_INLINE_CALL_END_ERROR_BYTES, RouteCallIdentity,
    RpcRecordKind, ValuePart, ValueStorage, WireLagPolicy, WireSubscribeOptions, WireTopicStart,
    bounded_call_end_error_bytes, decode_rpc_record, encode_call_end_error_frame,
    encode_call_end_frame, encode_cancel_frame, encode_credit_frame, encode_item_begin_frame,
    encode_item_data_frame, encode_open_frame, encode_open_subscribe_draft_frame,
    encode_open_subscribe_frame, encode_ready_frame, encode_subscribe_item_begin_frame,
    encode_value_continuation_frame,
};
use c2_config::CallMetadata;
use c2_error::{C2Error, ErrorCode};

const REQUEST_ID: u64 = 0x0807_0605_0403_0201;
const RID_BYTES: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
const OPEN_UNARY: &[u8] = b"\x01\x01r\x01n\x01G\x01v\x01a\x01s\x00\x34\x12\x00";
const OPEN_STREAM: &[u8] = b"\x01\x01r\x01n\x01G\x01v\x01a\x01s\x01\x34\x12\x00";
const OPEN_SUBSCRIBE: &[u8] = b"\x01\x01r\x01n\x01G\x01v\x01a\x01s\x02\x02t1\x03\x00";
const SHM_BYTES: [u8; 23] = [
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 2, 0, 0, 0, 1, 11, 12, 13, 14, 15, 16, 17, 18,
];
const ERROR: &[u8] =
    br#"C2E1{"version":1,"code":718,"name":"CallCancelled","message":"cancelled","details":{}}"#;

fn identity() -> RouteCallIdentity<&'static str> {
    RouteCallIdentity {
        route_name: "r",
        crm_ns: "n",
        crm_name: "G",
        crm_ver: "v",
        abi_hash: "a",
        signature_hash: "s",
    }
}

fn shm() -> BuddyPayload {
    BuddyPayload {
        seg_idx: 0x0201,
        generation: 0x0605_0403,
        offset: 0x0a09_0807,
        data_size: 2,
        is_dedicated: true,
        loan_id: 0x1211_100f_0e0d_0c0b,
    }
}

// The length prefix is an explicit fixture argument, not computed from the
// body or obtained by calling another encoder.
fn frame(length: [u8; 4], flags: [u8; 4], body: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&length);
    bytes.extend_from_slice(&RID_BYTES);
    bytes.extend_from_slice(&flags);
    bytes.extend_from_slice(body);
    bytes
}

#[test]
fn open_modes_and_storage_match_whole_frames() {
    for (mode, prefix) in [
        (CallMode::Unary, OPEN_UNARY),
        (CallMode::ServerStream, OPEN_STREAM),
    ] {
        let mut inline_body = prefix.to_vec();
        inline_body.extend_from_slice(b"xy");
        assert_eq!(
            encode_open_frame(
                REQUEST_ID,
                &identity(),
                0x1234,
                mode,
                &CallMetadata::empty(),
                ValuePart::whole(ValueStorage::Inline(b"xy")),
            )
            .unwrap(),
            frame([0x1f, 0, 0, 0], [0, 0x10, 0, 0], &inline_body),
        );

        let mut shm_body = prefix.to_vec();
        shm_body.extend_from_slice(&SHM_BYTES);
        assert_eq!(
            encode_open_frame(
                REQUEST_ID,
                &identity(),
                0x1234,
                mode,
                &CallMetadata::empty(),
                ValuePart::whole(ValueStorage::Shm(shm())),
            )
            .unwrap(),
            frame([0x34, 0, 0, 0], [0x40, 0x10, 0, 0], &shm_body),
        );
    }
}

#[test]
fn open_chunks_have_only_first_chunk_metadata() {
    let mut first_body = OPEN_UNARY.to_vec();
    first_body.extend_from_slice(b"\x05\x00\x00\x00\x00\x00\x00\x00ab");
    assert_eq!(
        encode_open_frame(
            REQUEST_ID,
            &identity(),
            0x1234,
            CallMode::Unary,
            &CallMetadata::empty(),
            ValuePart::first(5, ValueStorage::Inline(b"ab")),
        )
        .unwrap(),
        frame([0x27, 0, 0, 0], [0, 0x32, 0, 0], &first_body),
    );
    assert_eq!(
        encode_value_continuation_frame(
            REQUEST_ID,
            RpcRecordKind::Open,
            ValueStorage::Inline(b"cde"),
        )
        .unwrap(),
        frame([0x10, 0, 0, 0], [0, 0x12, 0, 0], b"\x01cde"),
    );
}

#[test]
fn open_drafts_and_streaming_prefixes_keep_one_reserved_frame() {
    use crate::rpc::{
        ChunkPosition, encode_open_continuation_draft_frame, encode_open_continuation_draft_prefix,
        encode_open_draft_frame, encode_open_inline_draft_prefix,
    };

    for (chunk, length, flags) in [
        (ChunkPosition::Whole, [0x1f, 0, 0, 0], [0, 0x10, 0, 0]),
        (
            ChunkPosition::First { total_bytes: 5 },
            [0x27, 0, 0, 0],
            [0, 0x32, 0, 0],
        ),
    ] {
        let mut body = OPEN_UNARY.to_vec();
        if matches!(chunk, ChunkPosition::First { .. }) {
            body.extend_from_slice(b"\x05\x00\x00\x00\x00\x00\x00\x00");
        }
        body.extend_from_slice(b"ab");
        let mut expected = frame(length, flags, &body);
        expected[4..12].fill(0);
        let part = ValuePart {
            chunk,
            storage: ValueStorage::Inline(b"ab"),
        };
        assert_eq!(
            encode_open_draft_frame(
                &identity(),
                0x1234,
                CallMode::Unary,
                &CallMetadata::empty(),
                part
            )
            .unwrap(),
            expected
        );

        let mut prefix = encode_open_inline_draft_prefix(
            &identity(),
            0x1234,
            CallMode::Unary,
            &CallMetadata::empty(),
            chunk,
            2,
        )
        .unwrap();
        let allocation = prefix.as_ptr();
        prefix.extend_from_slice(b"a");
        prefix.extend_from_slice(b"b");
        assert_eq!(prefix.as_ptr(), allocation);
        assert_eq!(prefix, expected);
    }

    let mut expected = frame([0x10, 0, 0, 0], [0, 0x12, 0, 0], b"\x01cde");
    expected[4..12].fill(0);
    assert_eq!(
        encode_open_continuation_draft_frame(ValueStorage::Inline(b"cde")).unwrap(),
        expected
    );
    let mut prefix = encode_open_continuation_draft_prefix(3).unwrap();
    let allocation = prefix.as_ptr();
    prefix.extend_from_slice(b"cde");
    assert_eq!(prefix.as_ptr(), allocation);
    assert_eq!(prefix, expected);
}

#[test]
fn inline_draft_prefixes_reject_unrepresentable_lengths_before_allocation() {
    use crate::rpc::{
        ChunkPosition, EncodeError, encode_open_continuation_draft_prefix,
        encode_open_inline_draft_prefix,
    };
    for (chunk, bytes) in [
        (ChunkPosition::Whole, usize::MAX),
        (
            ChunkPosition::First {
                total_bytes: u64::MAX,
            },
            usize::MAX - 1,
        ),
    ] {
        assert!(matches!(
            encode_open_inline_draft_prefix(
                &identity(),
                0,
                CallMode::Unary,
                &CallMetadata::empty(),
                chunk,
                bytes
            ),
            Err(EncodeError::FieldTooLong {
                field: "RPC record",
                ..
            }),
        ));
    }
    assert!(matches!(
        encode_open_continuation_draft_prefix(usize::MAX),
        Err(EncodeError::FieldTooLong {
            field: "RPC record",
            ..
        }),
    ));
}

#[test]
fn call_end_statuses_and_stream_counts_match_whole_frames() {
    assert_eq!(
        encode_call_end_frame(
            REQUEST_ID,
            CallMode::Unary,
            CallStatus::Success,
            0,
            ValuePart::whole(ValueStorage::Inline(b"ok")),
        )
        .unwrap(),
        frame([0x10, 0, 0, 0], [2, 0x10, 0, 0], b"\x06\x00ok"),
    );
    assert_eq!(
        encode_call_end_frame(
            REQUEST_ID,
            CallMode::ServerStream,
            CallStatus::Success,
            0x0807_0605_0403_0201,
            ValuePart::whole(ValueStorage::Inline(b"")),
        )
        .unwrap(),
        frame(
            [0x16, 0, 0, 0],
            [2, 0x10, 0, 0],
            b"\x06\x00\x01\x02\x03\x04\x05\x06\x07\x08",
        ),
    );
    let mut failed_body = b"\x06\x01\x03\x00\x00\x00\x00\x00\x00\x00".to_vec();
    failed_body.extend_from_slice(ERROR);
    assert_eq!(
        encode_call_end_frame(
            REQUEST_ID,
            CallMode::ServerStream,
            CallStatus::Failed,
            3,
            ValuePart::whole(ValueStorage::Inline(ERROR)),
        )
        .unwrap(),
        frame([0x6c, 0, 0, 0], [2, 0x10, 0, 0], &failed_body),
    );
    let mut not_executed_body = b"\x06\x02".to_vec();
    not_executed_body.extend_from_slice(ERROR);
    assert_eq!(
        encode_call_end_frame(
            REQUEST_ID,
            CallMode::ServerStream,
            CallStatus::NotExecuted,
            0,
            ValuePart::whole(ValueStorage::Inline(ERROR)),
        )
        .unwrap(),
        frame([0x64, 0, 0, 0], [2, 0x10, 0, 0], &not_executed_body),
    );
}

#[test]
fn call_end_chunks_have_only_first_chunk_metadata() {
    assert_eq!(
        encode_call_end_frame(
            REQUEST_ID,
            CallMode::Unary,
            CallStatus::Success,
            0,
            ValuePart::first(5, ValueStorage::Inline(b"ab")),
        )
        .unwrap(),
        frame(
            [0x18, 0, 0, 0],
            [2, 0x32, 0, 0],
            b"\x06\x00\x05\x00\x00\x00\x00\x00\x00\x00ab",
        ),
    );
    assert_eq!(
        encode_value_continuation_frame(
            REQUEST_ID,
            RpcRecordKind::CallEnd,
            ValueStorage::Inline(b"cde"),
        )
        .unwrap(),
        frame([0x10, 0, 0, 0], [2, 0x12, 0, 0], b"\x06cde"),
    );
}

#[test]
fn item_begin_and_item_data_match_whole_frames() {
    assert_eq!(
        encode_item_begin_frame(REQUEST_ID, 0x0807_0605_0403_0201).unwrap(),
        frame(
            [0x15, 0, 0, 0],
            [2, 0x10, 0, 0],
            b"\x02\x01\x02\x03\x04\x05\x06\x07\x08",
        ),
    );
    assert_eq!(
        encode_item_data_frame(REQUEST_ID, ValueStorage::Inline(b"xy")).unwrap(),
        frame([0x0f, 0, 0, 0], [2, 0x10, 0, 0], b"\x03xy"),
    );
    let mut shm_body = vec![3];
    shm_body.extend_from_slice(&SHM_BYTES);
    assert_eq!(
        encode_item_data_frame(REQUEST_ID, ValueStorage::Shm(shm())).unwrap(),
        frame([0x24, 0, 0, 0], [0x42, 0x10, 0, 0], &shm_body),
    );
}

#[test]
fn subscribe_open_ready_and_item_begin_match_whole_frames() {
    let options = WireSubscribeOptions {
        start: WireTopicStart::Next,
        on_lag: WireLagPolicy::Error,
    };
    assert_eq!(options.to_byte(), 0x03);
    assert_eq!(
        encode_open_subscribe_frame(
            REQUEST_ID,
            &identity(),
            "t1",
            options,
            &CallMetadata::empty()
        )
        .unwrap(),
        frame([0x1f, 0, 0, 0], [0, 0x10, 0, 0], OPEN_SUBSCRIBE),
    );
    let mut expected_draft = frame([0x1f, 0, 0, 0], [0, 0x10, 0, 0], OPEN_SUBSCRIBE);
    expected_draft[4..12].fill(0);
    assert_eq!(
        encode_open_subscribe_draft_frame(&identity(), "t1", options, &CallMetadata::empty())
            .unwrap(),
        expected_draft,
    );
    assert_eq!(
        encode_ready_frame(REQUEST_ID, 0x0807_0605_0403_0201, 0x1817_1615_1413_1211).unwrap(),
        frame(
            [0x1d, 0, 0, 0],
            [2, 0x10, 0, 0],
            b"\x08\x01\x02\x03\x04\x05\x06\x07\x08\x11\x12\x13\x14\x15\x16\x17\x18",
        ),
    );
    assert_eq!(
        encode_subscribe_item_begin_frame(REQUEST_ID, 0x0102_0304_0506_0708, 0x1112_1314_1516_1718)
            .unwrap(),
        frame(
            [0x1d, 0, 0, 0],
            [2, 0x10, 0, 0],
            b"\x02\x08\x07\x06\x05\x04\x03\x02\x01\x18\x17\x16\x15\x14\x13\x12\x11",
        ),
    );
}

#[test]
fn credit_and_both_cancel_causes_match_whole_frames() {
    assert_eq!(
        encode_credit_frame(REQUEST_ID, 0x0807_0605_0403_0201, 0x1817_1615_1413_1211).unwrap(),
        frame(
            [0x1d, 0, 0, 0],
            [0, 0x10, 0, 0],
            b"\x04\x01\x02\x03\x04\x05\x06\x07\x08\x11\x12\x13\x14\x15\x16\x17\x18",
        ),
    );
    for (cause, byte) in [(CancelCause::Consumer, 0), (CancelCause::Lifecycle, 1)] {
        assert_eq!(
            encode_cancel_frame(REQUEST_ID, cause).unwrap(),
            frame([0x0e, 0, 0, 0], [0, 0x10, 0, 0], &[5, byte]),
        );
    }
}

#[test]
fn bounded_call_end_error_returns_small_errors_byte_identical() {
    let error = C2Error::new(ErrorCode::CallCancelled, "cancelled");
    assert_eq!(
        bounded_call_end_error_bytes(&error),
        error.to_wire_bytes(),
        "a fitting error must never be re-encoded"
    );
    let detailed = C2Error::new(ErrorCode::RouteConflict, "duplicate online route").with_details(
        std::collections::BTreeMap::from([
            ("route".to_string(), "grid".to_string()),
            ("cause_kind".to_string(), "TransportIo".to_string()),
        ]),
    );
    assert_eq!(
        bounded_call_end_error_bytes(&detailed),
        detailed.to_wire_bytes()
    );
}

#[test]
fn oversized_call_end_error_is_truncated_into_the_frame_budget() {
    let error = C2Error::new(ErrorCode::CallCancelled, "x".repeat(2 * 1024 * 1024 + 7))
        .with_details(std::collections::BTreeMap::from([
            ("big".to_string(), "y".repeat(64 * 1024)),
            ("small".to_string(), "kept".to_string()),
        ]));
    let bytes = bounded_call_end_error_bytes(&error);
    assert!(bytes.len() <= MAX_INLINE_CALL_END_ERROR_BYTES);
    let decoded = C2Error::from_wire_bytes(&bytes).unwrap().unwrap();
    assert_eq!(decoded.code, ErrorCode::CallCancelled);
    assert_eq!(decoded.code.name(), error.code.name());
    assert_eq!(
        decoded.details.get("truncated").map(String::as_str),
        Some("true")
    );

    let frame = encode_call_end_error_frame(
        REQUEST_ID,
        CallMode::ServerStream,
        CallStatus::Failed,
        12,
        &error,
    )
    .unwrap();
    let (header, payload) = decode_frame(&frame).unwrap();
    assert!(
        u64::from(header.total_len) <= c2_config::MAX_RPC_FRAME_BYTES,
        "total_len {} exceeds the frame budget",
        header.total_len
    );
    // The bounded frame decodes through the production record codec.
    let record = decode_rpc_record(&header, payload, Some(CallMode::ServerStream)).unwrap();
    let crate::rpc::RpcRecord::CallEnd {
        status,
        complete_items,
        error: decoded_error,
        ..
    } = record
    else {
        panic!("expected CALL_END");
    };
    assert_eq!(status, CallStatus::Failed);
    assert_eq!(complete_items, Some(12));
    let decoded_error = decoded_error.expect("error must decode");
    assert_eq!(decoded_error.code, ErrorCode::CallCancelled);
    assert_eq!(
        decoded_error.details.get("truncated").map(String::as_str),
        Some("true")
    );
    // The decoded message is valid UTF-8/JSON by construction; the marker
    // proves the truncation happened through the bounded path.
    assert!(decoded_error.message.ends_with("...[truncated]"));
}

#[test]
fn multibyte_call_end_error_truncation_preserves_char_boundaries() {
    let error = C2Error::new(ErrorCode::CallCancelled, "失".repeat(600_000));
    let bytes = bounded_call_end_error_bytes(&error);
    assert!(bytes.len() <= MAX_INLINE_CALL_END_ERROR_BYTES);
    let decoded = C2Error::from_wire_bytes(&bytes).unwrap().unwrap();
    assert_eq!(decoded.code, ErrorCode::CallCancelled);
    assert_eq!(decoded.code.name(), error.code.name());
    // Decoding yields a valid String, so every cut landed on a char boundary.
    assert!(decoded.message.ends_with("...[truncated]"));
    assert!(decoded.message.len() < 3 * 600_000);
    assert_eq!(
        decoded.details.get("truncated").map(String::as_str),
        Some("true")
    );
}

#[test]
fn keyed_open_recover_and_query_place_metadata_after_call_id() {
    use crate::rpc::{
        ChunkPosition, encode_open_keyed_draft_frame, encode_open_keyed_frame,
        encode_open_keyed_inline_draft_prefix, encode_open_recover_draft_frame,
        encode_open_recover_frame, encode_query_call_draft_frame, encode_query_call_frame,
    };
    let metadata = CallMetadata::from_entries([("z", ""), ("a", "x y")]).unwrap();
    let keyed = frame(
        [0x2d, 0, 0, 0],
        [0, 0x10, 0, 0],
        b"\x01\x01r\x01n\x01G\x01v\x01a\x01s\x03\x34\x12\x02id\x02\x01a\x03\x00x y\x01z\x00\x00xy",
    );
    let value = ValuePart::whole(ValueStorage::Inline(b"xy"));
    assert_eq!(
        encode_open_keyed_frame(REQUEST_ID, &identity(), 0x1234, "id", &metadata, value).unwrap(),
        keyed
    );
    let mut keyed_draft = keyed;
    keyed_draft[4..12].fill(0);
    assert_eq!(
        encode_open_keyed_draft_frame(&identity(), 0x1234, "id", &metadata, value).unwrap(),
        keyed_draft
    );
    let mut prefix = encode_open_keyed_inline_draft_prefix(
        &identity(),
        0x1234,
        "id",
        &metadata,
        ChunkPosition::Whole,
        2,
    )
    .unwrap();
    let allocation = prefix.as_ptr();
    prefix.extend_from_slice(b"xy");
    assert_eq!(prefix.as_ptr(), allocation);
    assert_eq!(prefix, keyed_draft);

    for (wire, draft, mut expected) in [
        (
            encode_open_recover_frame(REQUEST_ID, &identity(), 0x1234, "id", &metadata).unwrap(),
            encode_open_recover_draft_frame(&identity(), 0x1234, "id", &metadata).unwrap(),
            frame(
                [0x2b, 0, 0, 0],
                [0, 0x10, 0, 0],
                b"\x01\x01r\x01n\x01G\x01v\x01a\x01s\x04\x34\x12\x02id\x02\x01a\x03\x00x y\x01z\x00\x00",
            ),
        ),
        (
            encode_query_call_frame(REQUEST_ID, &identity(), "id", &metadata).unwrap(),
            encode_query_call_draft_frame(&identity(), "id", &metadata).unwrap(),
            frame(
                [0x28, 0, 0, 0],
                [0, 0x10, 0, 0],
                b"\x0b\x01r\x01n\x01G\x01v\x01a\x01s\x02id\x02\x01a\x03\x00x y\x01z\x00\x00",
            ),
        ),
    ] {
        assert_eq!(wire, expected);
        expected[4..12].fill(0);
        assert_eq!(draft, expected);
    }
}

#[test]
fn topic_admission_records_match_whole_frames() {
    use crate::rpc::{
        WireAdmitTopicResult, encode_admit_topic_frame, encode_admit_topic_result_frame,
    };

    let metadata = CallMetadata::from_entries([("k", "")]).unwrap();
    assert_eq!(
        encode_admit_topic_frame(REQUEST_ID, 0x1817_1615_1413_1211, &metadata).unwrap(),
        frame(
            [0x1a, 0, 0, 0],
            [0, 0x10, 0, 0],
            b"\x0d\x11\x12\x13\x14\x15\x16\x17\x18\x01\x01k\x00\x00",
        ),
    );
    assert_eq!(
        encode_admit_topic_result_frame(
            REQUEST_ID,
            &WireAdmitTopicResult::Allowed {
                head_seq: 0x1817_1615_1413_1211
            },
        )
        .unwrap(),
        frame(
            [0x16, 0, 0, 0],
            [2, 0x10, 0, 0],
            b"\x0e\x00\x11\x12\x13\x14\x15\x16\x17\x18",
        ),
    );
}
