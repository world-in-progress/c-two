//! Buddy SHM payload codec (23 bytes).
//!
//! Encodes/decodes the pointer to a buddy-allocated SHM block:
//!
//! ```text
//! [2B slot LE][4B generation LE][4B offset LE][4B data_size LE][1B kind][8B loan_id LE]
//! ```
//!
//! This layout is part of the canonical wire protocol and is shared by all SDKs.

/// Size of the buddy payload header in bytes.
pub const BUDDY_PAYLOAD_SIZE: usize = 23;

/// Ordinary buddy allocation kind.
pub const SHM_KIND_BUDDY: u8 = 0;
/// Dedicated allocation kind.
pub const SHM_KIND_DEDICATED: u8 = 1;

/// Decoded buddy payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuddyPayload {
    /// Segment index within the buddy pool.
    pub seg_idx: u16,
    /// Concrete nonzero backing generation, for either kind.
    pub generation: u32,
    /// Byte offset within the segment.
    pub offset: u32,
    /// Actual data size at that offset.
    pub data_size: u32,
    /// Whether this is a dedicated (fallback) segment.
    pub is_dedicated: bool,
    /// Nonzero loan identity in the sending direction of this physical connection.
    pub loan_id: u64,
}

// ── Encoding ─────────────────────────────────────────────────────────────

/// Encode buddy payload into exactly [`BUDDY_PAYLOAD_SIZE`] bytes.
pub fn encode_buddy_payload(bp: &BuddyPayload) -> [u8; BUDDY_PAYLOAD_SIZE] {
    let mut buf = [0u8; BUDDY_PAYLOAD_SIZE];
    buf[0..2].copy_from_slice(&bp.seg_idx.to_le_bytes());
    buf[2..6].copy_from_slice(&bp.generation.to_le_bytes());
    buf[6..10].copy_from_slice(&bp.offset.to_le_bytes());
    buf[10..14].copy_from_slice(&bp.data_size.to_le_bytes());
    buf[14] = if bp.is_dedicated {
        SHM_KIND_DEDICATED
    } else {
        SHM_KIND_BUDDY
    };
    buf[15..23].copy_from_slice(&bp.loan_id.to_le_bytes());
    buf
}

// ── Decoding ─────────────────────────────────────────────────────────────

use crate::frame::DecodeError;

/// Decode buddy payload from a byte slice.
///
/// Returns `(payload, bytes_consumed)`.
pub fn decode_buddy_payload(buf: &[u8]) -> Result<(BuddyPayload, usize), DecodeError> {
    if buf.len() < BUDDY_PAYLOAD_SIZE {
        return Err(DecodeError::BufferTooShort {
            need: BUDDY_PAYLOAD_SIZE,
            have: buf.len(),
        });
    }
    let seg_idx = u16::from_le_bytes([buf[0], buf[1]]);
    let generation = u32::from_le_bytes([buf[2], buf[3], buf[4], buf[5]]);
    let offset = u32::from_le_bytes([buf[6], buf[7], buf[8], buf[9]]);
    let data_size = u32::from_le_bytes([buf[10], buf[11], buf[12], buf[13]]);
    let is_dedicated = match buf[14] {
        SHM_KIND_BUDDY => false,
        SHM_KIND_DEDICATED => true,
        kind => {
            return Err(DecodeError::InvalidValue {
                field: "SHM kind",
                value: kind as u64,
            });
        }
    };
    if generation == 0 {
        return Err(DecodeError::InvalidValue {
            field: "backing generation",
            value: generation as u64,
        });
    }
    let loan_id = u64::from_le_bytes(buf[15..23].try_into().unwrap());
    if loan_id == 0 {
        return Err(DecodeError::ZeroLoanId);
    }
    Ok((
        BuddyPayload {
            seg_idx,
            generation,
            offset,
            data_size,
            is_dedicated,
            loan_id,
        },
        BUDDY_PAYLOAD_SIZE,
    ))
}
