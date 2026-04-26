//! 16-byte frame header codec.
//!
//! Wire layout (all little-endian):
//!
//! ```text
//! [4B total_len][8B request_id][4B flags][payload ...]
//! ```
//!
//! `total_len` = 12 + payload_len (covers request_id + flags + payload).
//! The 4-byte `total_len` prefix itself is **not** included in the value.

use crate::flags;

/// Size of the frame header in bytes: 4 (total_len) + 8 (rid) + 4 (flags).
pub const HEADER_SIZE: usize = 16;

/// Decoded frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Total length of (request_id + flags + payload). Does **not** include
    /// the 4 bytes used to encode `total_len` itself.
    pub total_len: u32,
    /// Per-connection request identifier (unique per client).
    pub request_id: u64,
    /// Bit flags (see [`flags`]).
    pub flags: u32,
}

impl FrameHeader {
    /// Expected number of bytes following the 4-byte `total_len` prefix.
    #[inline]
    pub const fn body_len(&self) -> usize {
        self.total_len as usize
    }

    /// Payload length (total_len minus the 12 fixed body bytes).
    #[inline]
    pub const fn payload_len(&self) -> usize {
        if self.total_len >= 12 {
            (self.total_len - 12) as usize
        } else {
            0
        }
    }

    #[inline]
    pub const fn is_response(&self) -> bool {
        flags::is_response(self.flags)
    }

    #[inline]
    pub const fn is_buddy(&self) -> bool {
        flags::is_buddy(self.flags)
    }

    #[inline]
    pub const fn is_call_v2(&self) -> bool {
        flags::is_call_v2(self.flags)
    }

    #[inline]
    pub const fn is_reply_v2(&self) -> bool {
        flags::is_reply_v2(self.flags)
    }

    #[inline]
    pub const fn is_handshake(&self) -> bool {
        flags::is_handshake(self.flags)
    }

    #[inline]
    pub const fn is_ctrl(&self) -> bool {
        flags::is_ctrl(self.flags)
    }

    #[inline]
    pub const fn is_signal(&self) -> bool {
        flags::is_signal(self.flags)
    }
}

// ── Encoding ─────────────────────────────────────────────────────────────

/// Encode a complete frame: header + payload.
///
/// Returns a `Vec<u8>` of length `HEADER_SIZE + payload.len()`.
pub fn encode_frame(request_id: u64, frame_flags: u32, payload: &[u8]) -> Vec<u8> {
    let payload_len = payload.len();
    let total_len = (12 + payload_len) as u32;
    let mut buf = Vec::with_capacity(HEADER_SIZE + payload_len);
    buf.extend_from_slice(&total_len.to_le_bytes());
    buf.extend_from_slice(&request_id.to_le_bytes());
    buf.extend_from_slice(&frame_flags.to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

// ── Decoding ─────────────────────────────────────────────────────────────

/// Error returned when frame data is malformed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// Buffer is shorter than the minimum header size.
    BufferTooShort { need: usize, have: usize },
    /// A length field references more data than available.
    Truncated { field: &'static str, need: usize, have: usize },
    /// An invalid value was encountered (e.g. unknown status byte).
    InvalidValue { field: &'static str, value: u64 },
    /// UTF-8 decoding failed.
    Utf8Error,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BufferTooShort { need, have } => {
                write!(f, "buffer too short: need {need}, have {have}")
            }
            Self::Truncated { field, need, have } => {
                write!(f, "{field}: need {need} bytes, have {have}")
            }
            Self::InvalidValue { field, value } => {
                write!(f, "{field}: invalid value {value}")
            }
            Self::Utf8Error => write!(f, "UTF-8 decode error"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Decode the 4-byte `total_len` prefix from the start of a buffer.
///
/// Returns `(total_len_value, &rest)` where `rest` starts immediately after
/// the 4-byte prefix.
#[inline]
pub fn decode_total_len(buf: &[u8]) -> Result<(u32, &[u8]), DecodeError> {
    if buf.len() < 4 {
        return Err(DecodeError::BufferTooShort { need: 4, have: buf.len() });
    }
    let total_len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    Ok((total_len, &buf[4..]))
}

/// Decode a frame header from a 12-byte body slice (after the 4-byte total_len).
///
/// Returns `(header, payload_slice)`.
pub fn decode_frame_body(body: &[u8], total_len: u32) -> Result<(FrameHeader, &[u8]), DecodeError> {
    if body.len() < 12 {
        return Err(DecodeError::BufferTooShort { need: 12, have: body.len() });
    }
    let request_id = u64::from_le_bytes([
        body[0], body[1], body[2], body[3],
        body[4], body[5], body[6], body[7],
    ]);
    let frame_flags = u32::from_le_bytes([body[8], body[9], body[10], body[11]]);
    let payload = &body[12..];
    Ok((
        FrameHeader {
            total_len,
            request_id,
            flags: frame_flags,
        },
        payload,
    ))
}

/// Decode a complete frame from a buffer starting with the 4-byte total_len.
///
/// Returns `(header, payload_slice)`.
pub fn decode_frame(buf: &[u8]) -> Result<(FrameHeader, &[u8]), DecodeError> {
    let (total_len, body) = decode_total_len(buf)?;
    let expected = total_len as usize;
    if body.len() < expected {
        return Err(DecodeError::Truncated {
            field: "frame body",
            need: expected,
            have: body.len(),
        });
    }
    decode_frame_body(&body[..expected], total_len)
}
