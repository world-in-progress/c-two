"""Deprecated — use ``c_two.transport.ipc.shm_frame`` instead.

This module re-exports everything from ``shm_frame`` for backward compatibility.
"""
from c_two.transport.ipc.shm_frame import *  # noqa: F401, F403
from c_two.transport.ipc.shm_frame import (  # explicit for type checkers
    FLAG_BUDDY,
    BUDDY_PAYLOAD_STRUCT,
    BUDDY_PAYLOAD_SIZE,
    BUDDY_REUSE_FLAG,
    BUDDY_REUSE_EXTRA,
    BUDDY_REUSE_EXTRA_SIZE,
    HANDSHAKE_VERSION,
    CTRL_BUDDY_ANNOUNCE,
    CTRL_BUDDY_FREE,
    encode_buddy_payload,
    decode_buddy_payload,
    encode_buddy_call_frame,
    encode_buddy_reply_frame,
    encode_buddy_reuse_reply_frame,
    encode_buddy_handshake,
    decode_buddy_handshake,
    encode_ctrl_buddy_announce,
    decode_ctrl_buddy_announce,
)
