//! Chunk header codec (4 bytes).
//!
//! Used for chunked transfer of large payloads that exceed a single buddy
//! block. Each chunk carries a header identifying its position in the
//! sequence:
//!
//! ```text
//! [2B chunk_idx LE][2B total_chunks LE]
//! ```

use crate::frame::DecodeError;

/// Size of the chunk header in bytes.
pub const CHUNK_HEADER_SIZE: usize = 4;

/// Encode chunk header: `[2B chunk_idx LE][2B total_chunks LE]`.
#[inline]
pub fn encode_chunk_header(chunk_idx: u16, total_chunks: u16) -> [u8; CHUNK_HEADER_SIZE] {
    let mut buf = [0u8; CHUNK_HEADER_SIZE];
    buf[0..2].copy_from_slice(&chunk_idx.to_le_bytes());
    buf[2..4].copy_from_slice(&total_chunks.to_le_bytes());
    buf
}

/// Decode chunk header from `buf[offset..]`.
///
/// Returns `(chunk_idx, total_chunks, bytes_consumed)`.
pub fn decode_chunk_header(buf: &[u8], offset: usize) -> Result<(u16, u16, usize), DecodeError> {
    let remaining = buf.len().saturating_sub(offset);
    if remaining < CHUNK_HEADER_SIZE {
        return Err(DecodeError::BufferTooShort {
            need: CHUNK_HEADER_SIZE,
            have: remaining,
        });
    }
    let chunk_idx = u16::from_le_bytes([buf[offset], buf[offset + 1]]);
    let total_chunks = u16::from_le_bytes([buf[offset + 2], buf[offset + 3]]);
    Ok((chunk_idx, total_chunks, CHUNK_HEADER_SIZE))
}

// ── Reply chunk meta (extended format for chunked responses) ────────────

/// Size of the reply chunk meta header in bytes.
pub const REPLY_CHUNK_META_SIZE: usize = 16;

/// Encode reply chunk metadata: total_size(8) + total_chunks(4) + chunk_idx(4).
pub fn encode_reply_chunk_meta(
    total_size: u64,
    total_chunks: u32,
    chunk_idx: u32,
) -> [u8; REPLY_CHUNK_META_SIZE] {
    let mut buf = [0u8; REPLY_CHUNK_META_SIZE];
    buf[0..8].copy_from_slice(&total_size.to_le_bytes());
    buf[8..12].copy_from_slice(&total_chunks.to_le_bytes());
    buf[12..16].copy_from_slice(&chunk_idx.to_le_bytes());
    buf
}

/// Decode reply chunk metadata from `buf` at `offset`.
///
/// Returns `(total_size, total_chunks, chunk_idx, consumed)`.
pub fn decode_reply_chunk_meta(
    buf: &[u8],
    offset: usize,
) -> Result<(u64, u32, u32, usize), DecodeError> {
    let remaining = buf.len().saturating_sub(offset);
    if remaining < REPLY_CHUNK_META_SIZE {
        return Err(DecodeError::BufferTooShort {
            need: REPLY_CHUNK_META_SIZE,
            have: remaining,
        });
    }
    let total_size = u64::from_le_bytes(buf[offset..offset + 8].try_into().unwrap());
    let total_chunks = u32::from_le_bytes(buf[offset + 8..offset + 12].try_into().unwrap());
    let chunk_idx = u32::from_le_bytes(buf[offset + 12..offset + 16].try_into().unwrap());
    Ok((total_size, total_chunks, chunk_idx, REPLY_CHUNK_META_SIZE))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(feature = "std"))]
    use alloc::vec;

    #[test]
    fn round_trip() {
        let encoded = encode_chunk_header(42, 100);
        let (idx, total, consumed) = decode_chunk_header(&encoded, 0).unwrap();
        assert_eq!(idx, 42);
        assert_eq!(total, 100);
        assert_eq!(consumed, CHUNK_HEADER_SIZE);
    }

    #[test]
    fn decode_with_offset() {
        let mut buf = vec![0xFF; 8];
        let hdr = encode_chunk_header(3, 7);
        buf[2..6].copy_from_slice(&hdr);
        let (idx, total, _) = decode_chunk_header(&buf, 2).unwrap();
        assert_eq!(idx, 3);
        assert_eq!(total, 7);
    }

    #[test]
    fn buffer_too_short() {
        assert!(decode_chunk_header(&[0, 1], 0).is_err());
    }

    #[test]
    fn reply_chunk_meta_round_trip() {
        let encoded = encode_reply_chunk_meta(1_000_000_000, 8000, 42);
        let (total_size, total_chunks, chunk_idx, consumed) =
            decode_reply_chunk_meta(&encoded, 0).unwrap();
        assert_eq!(total_size, 1_000_000_000);
        assert_eq!(total_chunks, 8000);
        assert_eq!(chunk_idx, 42);
        assert_eq!(consumed, REPLY_CHUNK_META_SIZE);
    }

    #[test]
    fn reply_chunk_meta_with_offset() {
        let meta = encode_reply_chunk_meta(500, 3, 1);
        let mut buf = vec![0xFF, 0xFF];
        buf.extend_from_slice(&meta);
        let (total_size, total_chunks, chunk_idx, _) =
            decode_reply_chunk_meta(&buf, 2).unwrap();
        assert_eq!(total_size, 500);
        assert_eq!(total_chunks, 3);
        assert_eq!(chunk_idx, 1);
    }

    #[test]
    fn reply_chunk_meta_buffer_too_short() {
        assert!(decode_reply_chunk_meta(&[0; 10], 0).is_err());
    }
}
