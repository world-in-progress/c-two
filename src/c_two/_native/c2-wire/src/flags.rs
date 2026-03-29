//! Frame flag bit definitions.
//!
//! Matches Python `ipc_protocol.py` and `protocol.py` flag constants.
//! All flags occupy the 4-byte LE `flags` field of the frame header.

/// Payload references a per-request SharedMemory segment (legacy).
pub const FLAG_SHM: u32 = 1 << 0;

/// Server→client direction marker.
pub const FLAG_RESPONSE: u32 = 1 << 1;

/// Handshake message (pool SHM exchange or capability negotiation).
pub const FLAG_HANDSHAKE: u32 = 1 << 2;

/// Payload references pre-allocated pool SharedMemory (legacy).
pub const FLAG_POOL: u32 = 1 << 3;

/// Control message (segment announce, consumed signal).
pub const FLAG_CTRL: u32 = 1 << 4;

/// Reserved for disk spillover.
pub const FLAG_DISK_SPILL: u32 = 1 << 5;

/// Payload references a buddy-allocated SHM block.
pub const FLAG_BUDDY: u32 = 1 << 6;

/// V2 call frame — carries control-plane routing in the frame payload.
pub const FLAG_CALL_V2: u32 = 1 << 7;

/// V2 reply frame — carries control-plane status in the frame payload.
pub const FLAG_REPLY_V2: u32 = 1 << 8;

// ── Convenience predicates ───────────────────────────────────────────────

#[inline]
pub const fn is_response(flags: u32) -> bool {
    flags & FLAG_RESPONSE != 0
}

#[inline]
pub const fn is_handshake(flags: u32) -> bool {
    flags & FLAG_HANDSHAKE != 0
}

#[inline]
pub const fn is_ctrl(flags: u32) -> bool {
    flags & FLAG_CTRL != 0
}

#[inline]
pub const fn is_buddy(flags: u32) -> bool {
    flags & FLAG_BUDDY != 0
}

#[inline]
pub const fn is_call_v2(flags: u32) -> bool {
    flags & FLAG_CALL_V2 != 0
}

#[inline]
pub const fn is_reply_v2(flags: u32) -> bool {
    flags & FLAG_REPLY_V2 != 0
}
