"""IPC protocol — frame flag extensions and handshake.

Extends the IPC buddy protocol with:
- ``FLAG_CALL`` / ``FLAG_REPLY`` frame flags for control-plane routing
- Handshake with capability negotiation and method index exchange
- Reply status codes (success / error, no per-byte overhead in SHM)

Frame flag bit allocation (32-bit LE flags field in the 16-byte frame header):

    Bit 0: FLAG_SHM           — per-request SHM (legacy)
    Bit 1: FLAG_RESPONSE       — server→client direction
    Bit 2: FLAG_HANDSHAKE      — handshake message
    Bit 3: FLAG_POOL           — pool SHM (legacy)
    Bit 4: FLAG_CTRL           — control message
    Bit 6: FLAG_BUDDY          — buddy-allocated SHM block
    Bit 7: FLAG_CALL           — call frame (control-plane routing)
    Bit 8: FLAG_REPLY          — reply frame (control-plane status)
    Bit 9: FLAG_CHUNKED        — frame is part of a chunked sequence
    Bit 10: FLAG_CHUNK_LAST    — last frame in the chunked sequence
    Bit 11: FLAG_SIGNAL        — frame carries a 1-byte signal (PING, SHUTDOWN, etc.)

.. note::

   As of Phase 1, all codec logic is implemented in Rust (``c2-wire`` crate)
   and exposed via ``c_two._native``.  This module re-exports the Rust
   implementations to keep import paths stable.
"""
from __future__ import annotations

from c_two._native import (  # noqa: F401 — re-export
    # Flag constants
    FLAG_CALL,
    FLAG_REPLY,
    FLAG_CHUNKED,
    FLAG_CHUNK_LAST,
    FLAG_SIGNAL,
    # Handshake constants
    HANDSHAKE_VERSION,
    STATUS_SUCCESS,
    STATUS_ERROR,
    # Capability flags
    CAP_CALL,
    CAP_METHOD_IDX,
    CAP_CHUNKED,
    # Handshake dataclasses (Rust pyclasses)
    MethodEntry,
    RouteInfo,
    Handshake,
    # Core functions (private, wrapped below)
    encode_client_handshake as _encode_client_hs,
    encode_server_handshake as _encode_server_hs,
    decode_handshake as _decode_hs,
)

# ---------------------------------------------------------------------------
# Handshake safety limits (kept for API compat; enforced inside Rust codec)
# ---------------------------------------------------------------------------

_MAX_HANDSHAKE_SEGMENTS = 16
_MAX_HANDSHAKE_ROUTES = 64
_MAX_HANDSHAKE_METHODS = 256


# ---------------------------------------------------------------------------
# Handshake — wrappers with Python-compatible signatures
# ---------------------------------------------------------------------------

def encode_client_handshake(
    segments: list[tuple[str, int]],
    capability_flags: int = CAP_CALL | CAP_METHOD_IDX,
    *,
    prefix: str = '',
) -> bytes:
    """Encode client→server handshake.

    Format (v6)::

        [1B version=6]
        [1B prefix_len][prefix UTF-8]
        [2B seg_count LE]
        [per-segment: [4B size LE][1B name_len][name UTF-8]]
        [2B capability_flags LE]
    """
    return _encode_client_hs(segments, capability_flags, prefix)


def encode_server_handshake(
    segments: list[tuple[str, int]],
    capability_flags: int,
    routes: list[RouteInfo],
    *,
    prefix: str = '',
) -> bytes:
    """Encode server→client handshake ACK.

    Format (v6)::

        [1B version=6]
        [1B prefix_len][prefix UTF-8]
        [2B seg_count LE]
        [per-segment: [4B size LE][1B name_len][name UTF-8]]
        [2B capability_flags LE]
        [2B route_count LE]
        [per-route:
            [1B name_len][route_name UTF-8]
            [2B method_count LE]
            [per-method: [1B name_len][method_name UTF-8][2B method_idx LE]]
        ]
    """
    return _encode_server_hs(segments, capability_flags, routes, prefix)


def decode_handshake(
    payload: bytes | memoryview,
    *,
    max_segments: int = _MAX_HANDSHAKE_SEGMENTS,
    max_routes: int = _MAX_HANDSHAKE_ROUTES,
    max_methods: int = _MAX_HANDSHAKE_METHODS,
) -> Handshake:
    """Decode handshake from either direction.

    Client payloads have no route section (detected by exhausting bytes
    after capability_flags).

    Safety limits are enforced inside the Rust codec; the keyword
    arguments are kept for API compatibility but ignored.
    """
    return _decode_hs(payload)
