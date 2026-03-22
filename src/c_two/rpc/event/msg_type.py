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
    SHUTDOWN_SERVER  = 0x06
    SHUTDOWN_ACK     = 0x07
