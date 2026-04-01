//! IPC control message codec.
//!
//! Control messages are sent in frames with `FLAG_CTRL` set and `request_id=0`.
//! The first byte of the payload identifies the control message type.
//!
//! ## CTRL_SEGMENT_ANNOUNCE (0x01)
//!
//! ```text
//! [1B ctrl=0x01][1B direction][1B index][4B size LE][name UTF-8]
//! ```
//!
//! ## CTRL_CONSUMED (0x02)
//!
//! ```text
//! [1B ctrl=0x02][1B direction][1B index]
//! ```

#[cfg(not(feature = "std"))]
use alloc::{string::String, vec::Vec};

use crate::frame::DecodeError;

/// Announce a new pool segment to peer.
pub const CTRL_SEGMENT_ANNOUNCE: u8 = 0x01;
/// Signal that a segment has been consumed.
pub const CTRL_CONSUMED: u8 = 0x02;
/// Announce a buddy segment to peer.
pub const CTRL_BUDDY_ANNOUNCE: u8 = 0x03;
/// Notify peer of a freed buddy block.
pub const CTRL_BUDDY_FREE: u8 = 0x04;

/// Pool direction: client → server (request direction).
pub const POOL_DIR_OUTBOUND: u8 = 0;
/// Pool direction: server → client (response direction).
pub const POOL_DIR_RESPONSE: u8 = 1;

// ── CTRL_SEGMENT_ANNOUNCE ────────────────────────────────────────────────

/// Encode a `CTRL_SEGMENT_ANNOUNCE` message.
///
/// Format: `[1B ctrl=0x01][1B direction][1B index][4B size LE][name UTF-8]`
pub fn encode_ctrl_segment_announce(
    direction: u8,
    index: u8,
    size: u32,
    name: &str,
) -> Vec<u8> {
    let name_b = name.as_bytes();
    let mut buf = Vec::with_capacity(7 + name_b.len());
    buf.push(CTRL_SEGMENT_ANNOUNCE);
    buf.push(direction);
    buf.push(index);
    buf.extend_from_slice(&size.to_le_bytes());
    buf.extend_from_slice(name_b);
    buf
}

/// Decode a `CTRL_SEGMENT_ANNOUNCE` message.
///
/// Returns `(direction, segment_index, segment_size, shm_name)`.
pub fn decode_ctrl_segment_announce(
    payload: &[u8],
) -> Result<(u8, u8, u32, String), DecodeError> {
    if payload.len() < 7 {
        return Err(DecodeError::BufferTooShort {
            need: 7,
            have: payload.len(),
        });
    }
    let direction = payload[1];
    let index = payload[2];
    let size = u32::from_le_bytes([payload[3], payload[4], payload[5], payload[6]]);
    let name = core::str::from_utf8(&payload[7..])
        .map_err(|_| DecodeError::Utf8Error)?
        .into();
    Ok((direction, index, size, name))
}

// ── CTRL_CONSUMED ────────────────────────────────────────────────────────

/// Encode a `CTRL_CONSUMED` message.
///
/// Format: `[1B ctrl=0x02][1B direction][1B index]`
pub fn encode_ctrl_consumed(direction: u8, index: u8) -> [u8; 3] {
    [CTRL_CONSUMED, direction, index]
}

/// Decode a `CTRL_CONSUMED` message.
///
/// Returns `(direction, segment_index)`.
pub fn decode_ctrl_consumed(payload: &[u8]) -> Result<(u8, u8), DecodeError> {
    if payload.len() < 3 {
        return Err(DecodeError::BufferTooShort {
            need: 3,
            have: payload.len(),
        });
    }
    Ok((payload[1], payload[2]))
}

// ── CTRL_BUDDY_ANNOUNCE ──────────────────────────────────────────────────

/// Encode a `CTRL_BUDDY_ANNOUNCE` message for a new buddy segment.
///
/// Format: `[1B ctrl=0x03][2B seg_idx LE][4B size LE][name UTF-8]`
pub fn encode_ctrl_buddy_announce(seg_idx: u16, size: u32, name: &str) -> Vec<u8> {
    let name_b = name.as_bytes();
    let mut buf = Vec::with_capacity(7 + name_b.len());
    buf.push(CTRL_BUDDY_ANNOUNCE);
    buf.extend_from_slice(&seg_idx.to_le_bytes());
    buf.extend_from_slice(&size.to_le_bytes());
    buf.extend_from_slice(name_b);
    buf
}

/// Decode a `CTRL_BUDDY_ANNOUNCE` message.
///
/// Returns `(seg_idx, segment_size, shm_name)`.
pub fn decode_ctrl_buddy_announce(payload: &[u8]) -> Result<(u16, u32, String), DecodeError> {
    if payload.len() < 7 {
        return Err(DecodeError::BufferTooShort {
            need: 7,
            have: payload.len(),
        });
    }
    let seg_idx = u16::from_le_bytes([payload[1], payload[2]]);
    let size = u32::from_le_bytes([payload[3], payload[4], payload[5], payload[6]]);
    let name = core::str::from_utf8(&payload[7..])
        .map_err(|_| DecodeError::Utf8Error)?
        .into();
    Ok((seg_idx, size, name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_announce_round_trip() {
        let encoded = encode_ctrl_segment_announce(POOL_DIR_OUTBOUND, 2, 268_435_456, "cc_pool_0");
        let (dir, idx, size, name) = decode_ctrl_segment_announce(&encoded).unwrap();
        assert_eq!(dir, POOL_DIR_OUTBOUND);
        assert_eq!(idx, 2);
        assert_eq!(size, 268_435_456);
        assert_eq!(name, "cc_pool_0");
    }

    #[test]
    fn consumed_round_trip() {
        let encoded = encode_ctrl_consumed(POOL_DIR_RESPONSE, 3);
        let (dir, idx) = decode_ctrl_consumed(&encoded).unwrap();
        assert_eq!(dir, POOL_DIR_RESPONSE);
        assert_eq!(idx, 3);
    }

    #[test]
    fn buddy_announce_round_trip() {
        let encoded = encode_ctrl_buddy_announce(5, 1024, "buddy_seg_5");
        let (idx, size, name) = decode_ctrl_buddy_announce(&encoded).unwrap();
        assert_eq!(idx, 5);
        assert_eq!(size, 1024);
        assert_eq!(name, "buddy_seg_5");
    }
}
