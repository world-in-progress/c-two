from __future__ import annotations
from enum import IntEnum, unique


@unique
class MsgType(IntEnum):
    """Compact 1-byte message type identifier for the C-Two wire protocol.

    Replaces string-encoded EventTag ('crm_call' = 16 bytes) with a single
    byte, and serves as the first byte of every wire message.
    """
    PING             = 0x01
    PONG             = 0x02
    CRM_CALL         = 0x03
    CRM_REPLY        = 0x04
    SHUTDOWN_CLIENT  = 0x05
    # 0x06 reserved (was SHUTDOWN_SERVER — never implemented)
    SHUTDOWN_ACK     = 0x07
    DISCONNECT       = 0x08  # graceful per-connection disconnect
    DISCONNECT_ACK   = 0x09


# Pre-encoded 1-byte signal payloads (used as inline frame body)
PING_BYTES = bytes([MsgType.PING])
PONG_BYTES = bytes([MsgType.PONG])
SHUTDOWN_CLIENT_BYTES = bytes([MsgType.SHUTDOWN_CLIENT])
SHUTDOWN_ACK_BYTES = bytes([MsgType.SHUTDOWN_ACK])
DISCONNECT_BYTES = bytes([MsgType.DISCONNECT])
DISCONNECT_ACK_BYTES = bytes([MsgType.DISCONNECT_ACK])
SIGNAL_SIZE = 1

_SIGNAL_TYPES = frozenset({
    MsgType.PING,
    MsgType.PONG,
    MsgType.SHUTDOWN_CLIENT,
    MsgType.SHUTDOWN_ACK,
    MsgType.DISCONNECT,
    MsgType.DISCONNECT_ACK,
})
