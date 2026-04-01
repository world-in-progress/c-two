//! IPC message/signal type enum — single source of truth.
//!
//! Each variant maps to a 1-byte discriminant used as the inline frame body
//! for signal messages, or as a type tag in legacy v1 wire messages.

/// IPC message type (1-byte discriminant).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MsgType {
    Ping = 0x01,
    Pong = 0x02,
    CrmCall = 0x03,
    CrmReply = 0x04,
    ShutdownClient = 0x05,
    // 0x06 reserved (was SHUTDOWN_SERVER — never implemented)
    ShutdownAck = 0x07,
    Disconnect = 0x08,
    DisconnectAck = 0x09,
}

impl MsgType {
    /// Parse a single byte into a `MsgType`, returning `None` for unknown values.
    #[inline]
    pub const fn from_byte(b: u8) -> Option<Self> {
        match b {
            0x01 => Some(Self::Ping),
            0x02 => Some(Self::Pong),
            0x03 => Some(Self::CrmCall),
            0x04 => Some(Self::CrmReply),
            0x05 => Some(Self::ShutdownClient),
            0x07 => Some(Self::ShutdownAck),
            0x08 => Some(Self::Disconnect),
            0x09 => Some(Self::DisconnectAck),
            _ => None,
        }
    }

    /// Whether this type represents a signal (non-data frame).
    #[inline]
    pub const fn is_signal(self) -> bool {
        matches!(
            self,
            Self::Ping
                | Self::Pong
                | Self::ShutdownClient
                | Self::ShutdownAck
                | Self::Disconnect
                | Self::DisconnectAck
        )
    }

    /// The raw byte value of this type.
    #[inline]
    pub const fn as_byte(self) -> u8 {
        self as u8
    }
}

/// Size of a signal payload (always 1 byte).
pub const SIGNAL_SIZE: usize = 1;

// Pre-encoded signal bytes (matching Python msg_type.py constants).
pub const PING_BYTES: [u8; 1] = [MsgType::Ping as u8];
pub const PONG_BYTES: [u8; 1] = [MsgType::Pong as u8];
pub const SHUTDOWN_CLIENT_BYTES: [u8; 1] = [MsgType::ShutdownClient as u8];
pub const SHUTDOWN_ACK_BYTES: [u8; 1] = [MsgType::ShutdownAck as u8];
pub const DISCONNECT_BYTES: [u8; 1] = [MsgType::Disconnect as u8];
pub const DISCONNECT_ACK_BYTES: [u8; 1] = [MsgType::DisconnectAck as u8];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_all_variants() {
        for (byte, expected) in [
            (0x01, MsgType::Ping),
            (0x02, MsgType::Pong),
            (0x03, MsgType::CrmCall),
            (0x04, MsgType::CrmReply),
            (0x05, MsgType::ShutdownClient),
            (0x07, MsgType::ShutdownAck),
            (0x08, MsgType::Disconnect),
            (0x09, MsgType::DisconnectAck),
        ] {
            let parsed = MsgType::from_byte(byte).unwrap();
            assert_eq!(parsed, expected);
            assert_eq!(parsed.as_byte(), byte);
        }
    }

    #[test]
    fn unknown_byte_returns_none() {
        assert!(MsgType::from_byte(0x00).is_none());
        assert!(MsgType::from_byte(0x06).is_none());
        assert!(MsgType::from_byte(0xFF).is_none());
    }

    #[test]
    fn signal_classification() {
        assert!(MsgType::Ping.is_signal());
        assert!(MsgType::Pong.is_signal());
        assert!(MsgType::ShutdownClient.is_signal());
        assert!(MsgType::ShutdownAck.is_signal());
        assert!(MsgType::Disconnect.is_signal());
        assert!(MsgType::DisconnectAck.is_signal());
        assert!(!MsgType::CrmCall.is_signal());
        assert!(!MsgType::CrmReply.is_signal());
    }
}
