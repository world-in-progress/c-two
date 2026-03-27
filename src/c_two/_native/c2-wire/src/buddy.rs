//! Buddy SHM payload codec (11 bytes).
//!
//! Encodes/decodes the pointer to a buddy-allocated SHM block:
//!
//! ```text
//! [2B seg_idx LE][4B offset LE][4B data_size LE][1B flags]
//! ```
//!
//! Matches Python `ipc_v3_protocol.BUDDY_PAYLOAD_STRUCT = struct.Struct('<HII B')`.

/// Size of the buddy payload header in bytes.
pub const BUDDY_PAYLOAD_SIZE: usize = 11;

/// Flag bit within the 1-byte buddy flags field: block is a dedicated
/// (fallback) allocation, not from the main pool.
pub const BUDDY_FLAG_DEDICATED: u8 = 1 << 0;

/// Decoded buddy payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BuddyPayload {
    /// Segment index within the buddy pool.
    pub seg_idx: u16,
    /// Byte offset within the segment.
    pub offset: u32,
    /// Actual data size at that offset.
    pub data_size: u32,
    /// Whether this is a dedicated (fallback) segment.
    pub is_dedicated: bool,
}

// ── Encoding ─────────────────────────────────────────────────────────────

/// Encode buddy payload into exactly [`BUDDY_PAYLOAD_SIZE`] bytes.
pub fn encode_buddy_payload(bp: &BuddyPayload) -> [u8; BUDDY_PAYLOAD_SIZE] {
    let mut buf = [0u8; BUDDY_PAYLOAD_SIZE];
    buf[0..2].copy_from_slice(&bp.seg_idx.to_le_bytes());
    buf[2..6].copy_from_slice(&bp.offset.to_le_bytes());
    buf[6..10].copy_from_slice(&bp.data_size.to_le_bytes());
    buf[10] = if bp.is_dedicated { BUDDY_FLAG_DEDICATED } else { 0 };
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
    let offset = u32::from_le_bytes([buf[2], buf[3], buf[4], buf[5]]);
    let data_size = u32::from_le_bytes([buf[6], buf[7], buf[8], buf[9]]);
    let flags = buf[10];
    Ok((
        BuddyPayload {
            seg_idx,
            offset,
            data_size,
            is_dedicated: flags & BUDDY_FLAG_DEDICATED != 0,
        },
        BUDDY_PAYLOAD_SIZE,
    ))
}
