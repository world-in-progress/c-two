//! Handwritten IPC 16 whole-frame fixtures for the public RPC encoders.
//! Keep the expected headers and bodies independent of the production codec.

use crate::buddy::BuddyPayload;
use crate::rpc::{
    CallMode, CallStatus, CancelCause, RouteCallIdentity, RpcRecordKind, ValuePart, ValueStorage,
    encode_call_end_frame, encode_cancel_frame, encode_credit_frame, encode_item_begin_frame,
    encode_item_data_frame, encode_open_frame, encode_value_continuation_frame,
};

const REQUEST_ID: u64 = 0x0807_0605_0403_0201;
const RID_BYTES: [u8; 8] = [1, 2, 3, 4, 5, 6, 7, 8];
const OPEN_UNARY: &[u8] = b"\x01\x01r\x01n\x01G\x01v\x01a\x01s\x34\x12\x00";
const OPEN_STREAM: &[u8] = b"\x01\x01r\x01n\x01G\x01v\x01a\x01s\x34\x12\x01";
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
                ValuePart::whole(ValueStorage::Inline(b"xy")),
            )
            .unwrap(),
            frame([0x1e, 0, 0, 0], [0, 0x10, 0, 0], &inline_body),
        );

        let mut shm_body = prefix.to_vec();
        shm_body.extend_from_slice(&SHM_BYTES);
        assert_eq!(
            encode_open_frame(
                REQUEST_ID,
                &identity(),
                0x1234,
                mode,
                ValuePart::whole(ValueStorage::Shm(shm())),
            )
            .unwrap(),
            frame([0x33, 0, 0, 0], [0x40, 0x10, 0, 0], &shm_body),
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
            ValuePart::first(5, ValueStorage::Inline(b"ab")),
        )
        .unwrap(),
        frame([0x26, 0, 0, 0], [0, 0x32, 0, 0], &first_body),
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
        (ChunkPosition::Whole, [0x1e, 0, 0, 0], [0, 0x10, 0, 0]),
        (
            ChunkPosition::First { total_bytes: 5 },
            [0x26, 0, 0, 0],
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
            encode_open_draft_frame(&identity(), 0x1234, CallMode::Unary, part).unwrap(),
            expected
        );

        let mut prefix =
            encode_open_inline_draft_prefix(&identity(), 0x1234, CallMode::Unary, chunk, 2)
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
            encode_open_inline_draft_prefix(&identity(), 0, CallMode::Unary, chunk, bytes),
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
