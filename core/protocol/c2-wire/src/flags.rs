//! Frame flag bit definitions.
//!
//! Defines the canonical frame flag constants shared by all SDKs.
//! All flags occupy the 4-byte LE `flags` field of the frame header.

/// Server→client direction marker.
pub const FLAG_RESPONSE: u32 = 1 << 1;

/// Handshake message (pool SHM exchange or capability negotiation).
pub const FLAG_HANDSHAKE: u32 = 1 << 2;

/// Control message.
pub const FLAG_CTRL: u32 = 1 << 4;

/// Payload references a buddy-allocated SHM block.
pub const FLAG_BUDDY: u32 = 1 << 6;

/// IPC 16 value is split across records.
pub const FLAG_FRAGMENTED: u32 = 1 << 9;

/// IPC 16 first fragment carries metadata and total value length.
pub const FLAG_FIRST_FRAGMENT: u32 = 1 << 13;

/// IPC 16 RPC record envelope.
pub const FLAG_RPC_RECORD: u32 = 1 << 12;

/// Frame carries a 1-byte signal (PING, PONG, SHUTDOWN, DISCONNECT, etc.).
pub const FLAG_SIGNAL: u32 = 1 << 11;

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
pub const fn is_signal(flags: u32) -> bool {
    flags & FLAG_SIGNAL != 0
}
