//! IPC 16 release records. Shape, direction and phase are checked at admission.

use c2_config::IPC_RELEASE_BATCH_CAPACITY;

use crate::flags::{FLAG_RESPONSE, FLAG_RPC_RECORD};
use crate::frame::{DecodeError, FrameHeader};
use crate::rpc::EncodeError;
use crate::rpc::RpcRecordKind;

/// One nonempty, bounded release payload. IDs are settled in order by the
/// connection core; a malformed ID must not undo preceding settlements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseBatch {
    ids: [u64; IPC_RELEASE_BATCH_CAPACITY],
    count: usize,
}

impl ReleaseBatch {
    pub fn new(ids: &[u64]) -> Result<Self, EncodeError> {
        if ids.is_empty() {
            return Err(EncodeError::InvalidText {
                field: "release count",
                reason: "must be at least one".into(),
            });
        }
        if ids.len() > IPC_RELEASE_BATCH_CAPACITY {
            return Err(EncodeError::FieldTooLong {
                field: "release count",
                max: IPC_RELEASE_BATCH_CAPACITY,
                actual: ids.len(),
            });
        }
        let mut batch = Self {
            ids: [0; IPC_RELEASE_BATCH_CAPACITY],
            count: ids.len(),
        };
        batch.ids[..ids.len()].copy_from_slice(ids);
        Ok(batch)
    }

    pub fn ids(&self) -> &[u64] {
        &self.ids[..self.count]
    }
}

/// Encode one complete frame, retaining the ordinary 16-byte envelope.
pub fn encode_release_frame(batch: &ReleaseBatch, response: bool) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(18 + 8 * batch.count);
    append_release_frame(&mut bytes, batch.ids(), response);
    bytes
}

/// Append a producer-validated batch into caller-reserved storage.
pub(crate) fn append_release_frame(bytes: &mut Vec<u8>, ids: &[u64], response: bool) {
    bytes.extend_from_slice(&(14 + 8 * ids.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&0u64.to_le_bytes());
    bytes.extend_from_slice(
        &(FLAG_RPC_RECORD | if response { FLAG_RESPONSE } else { 0 }).to_le_bytes(),
    );
    bytes.extend_from_slice(&[RpcRecordKind::Release as u8, ids.len() as u8]);
    for id in ids {
        bytes.extend_from_slice(&id.to_le_bytes());
    }
}

/// Validate an incoming release frame at its connection's receive boundary.
/// `response` is the expected peer direction, not a value read from the frame.
pub fn decode_release_frame(
    header: &FrameHeader,
    payload: &[u8],
    response: bool,
    handshake_done: bool,
) -> Result<ReleaseBatch, DecodeError> {
    if !handshake_done {
        return Err(DecodeError::InvalidValue {
            field: "release before handshake",
            value: 0,
        });
    }
    if header.request_id != 0 {
        return Err(DecodeError::InvalidValue {
            field: "release request id",
            value: header.request_id,
        });
    }
    let expected_flags = FLAG_RPC_RECORD | if response { FLAG_RESPONSE } else { 0 };
    if header.flags != expected_flags {
        return Err(DecodeError::InvalidValue {
            field: "release flags",
            value: header.flags.into(),
        });
    }
    if payload.len() < 2 {
        return Err(DecodeError::BufferTooShort {
            need: 2,
            have: payload.len(),
        });
    }
    if payload[0] != RpcRecordKind::Release as u8 {
        return Err(DecodeError::InvalidValue {
            field: "release type",
            value: payload[0].into(),
        });
    }
    let count = usize::from(payload[1]);
    if !(1..=IPC_RELEASE_BATCH_CAPACITY).contains(&count) {
        return Err(DecodeError::InvalidValue {
            field: "release count",
            value: count as u64,
        });
    }
    let expected = 2 + 8 * count;
    if payload.len() != expected || u64::from(header.total_len) != 12 + expected as u64 {
        return Err(DecodeError::InvalidValue {
            field: "release length",
            value: header.total_len.into(),
        });
    }
    let mut batch = ReleaseBatch {
        ids: [0; IPC_RELEASE_BATCH_CAPACITY],
        count,
    };
    for (id, bytes) in batch.ids.iter_mut().zip(payload[2..].chunks_exact(8)) {
        *id = u64::from_le_bytes(bytes.try_into().unwrap());
    }
    Ok(batch)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flags::{FLAG_BUDDY, FLAG_HANDSHAKE};
    use crate::frame::decode_frame;

    #[test]
    fn handwritten_minimum_and_full_batch_frames() {
        let batch = ReleaseBatch::new(&[0x0807_0605_0403_0201]).unwrap();
        let bytes = encode_release_frame(&batch, false);
        assert_eq!(
            bytes,
            [
                22, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 16, 0, 0, 7, 1, 1, 2, 3, 4, 5, 6, 7, 8
            ]
        );
        let ids: Vec<_> = (1..=64).collect();
        let batch = ReleaseBatch::new(&ids).unwrap();
        for response in [false, true] {
            let bytes = encode_release_frame(&batch, response);
            assert_eq!(bytes.len(), 530);
            let (header, payload) = decode_frame(&bytes).unwrap();
            assert_eq!(header.total_len, 526);
            assert_eq!(
                decode_release_frame(&header, payload, response, true).unwrap(),
                batch
            );
        }
    }

    #[test]
    fn boundary_rejects_type_direction_phase_count_and_inexact_lengths() {
        let bytes = encode_release_frame(&ReleaseBatch::new(&[1]).unwrap(), false);
        let (header, payload) = decode_frame(&bytes).unwrap();
        assert!(decode_release_frame(&header, payload, false, false).is_err());
        assert!(decode_release_frame(&header, payload, true, true).is_err());
        for flags in [
            0,
            FLAG_RPC_RECORD | FLAG_HANDSHAKE,
            FLAG_RPC_RECORD | FLAG_BUDDY,
        ] {
            assert!(
                decode_release_frame(&FrameHeader { flags, ..header }, payload, false, true)
                    .is_err()
            );
        }
        assert!(
            decode_release_frame(
                &FrameHeader {
                    request_id: 1,
                    ..header
                },
                payload,
                false,
                true
            )
            .is_err()
        );
        assert!(
            decode_release_frame(
                &FrameHeader {
                    total_len: 23,
                    ..header
                },
                payload,
                false,
                true
            )
            .is_err()
        );
        for count in [0, 65, 255] {
            let mut bad = payload.to_vec();
            bad[1] = count;
            assert!(decode_release_frame(&header, &bad, false, true).is_err());
        }
        let mut bad = payload.to_vec();
        bad[0] = 0x16;
        assert!(decode_release_frame(&header, &bad, false, true).is_err());
        for len in 0..payload.len() {
            assert!(decode_release_frame(&header, &payload[..len], false, true).is_err());
        }
        let mut bad = payload.to_vec();
        bad.push(0);
        assert!(decode_release_frame(&header, &bad, false, true).is_err());
        assert!(ReleaseBatch::new(&[]).is_err());
        assert!(ReleaseBatch::new(&[1; 65]).is_err());
    }
}
