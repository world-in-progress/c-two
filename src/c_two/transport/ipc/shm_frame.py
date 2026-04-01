"""IPC protocol — SHM frame transport with buddy-allocated memory.

Extends the base IPC protocol with:
- Buddy pool allocation (zero-syscall dynamic SHM alloc/free)
- Full-duplex communication (concurrent requests and responses on shared buddy pool)
- Direct SHM memoryview access (zero-copy consumer reads)

Frame format is compatible with the base protocol (same 16-byte header) but uses a new
FLAG_BUDDY flag and extended pool payload format.
"""

import struct
from dataclasses import dataclass, field
from typing import Optional

from c_two._native import (
    encode_ctrl_buddy_announce,
    decode_ctrl_buddy_announce,
)

# Buddy payload encode: wrap Rust FFI to preserve is_dedicated default
from c_two._native import encode_buddy_payload as _ffi_encode_buddy_payload


def encode_buddy_payload(
    seg_idx: int, offset: int, data_size: int, is_dedicated: bool = False,
) -> bytes:
    """Encode a FLAG_BUDDY payload header (11 bytes)."""
    return _ffi_encode_buddy_payload(seg_idx, offset, data_size, is_dedicated)
from .frame import (
    FRAME_STRUCT,
    FRAME_HEADER_SIZE,
    FLAG_RESPONSE,
    FLAG_CTRL,
    U64_STRUCT,
    U32_STRUCT,
    IPCConfig,
    encode_frame,
    decode_frame,
    encode_inline_call_frame,
    encode_inline_reply_frame,
)

# ---------------------------------------------------------------------------
# Buddy flag bits (extend base flags)
# ---------------------------------------------------------------------------
FLAG_BUDDY = 1 << 6   # Payload references a buddy-allocated SHM block

# ---------------------------------------------------------------------------
# Buddy pool payload format
# ---------------------------------------------------------------------------
# FLAG_BUDDY payload: [2B seg_idx LE][4B offset LE][4B data_size LE][1B flags]
#   - seg_idx:   buddy segment index (u16)
#   - offset:    byte offset within the segment's data region (u32)
#   - data_size: actual data size written (u32, not the allocation size)
#   - flags:     bit 0 = is_dedicated, bit 1 = reuse (zero-copy response in request block)
BUDDY_PAYLOAD_STRUCT = struct.Struct('<HII B')
BUDDY_PAYLOAD_SIZE = BUDDY_PAYLOAD_STRUCT.size   # 11 bytes

# Zero-copy response reuse: when BUDDY_REUSE_FLAG is set, 8 extra bytes follow
# the standard payload with the original allocation coordinates for freeing.
BUDDY_REUSE_FLAG = 0x02
BUDDY_REUSE_EXTRA = struct.Struct('<II')    # free_offset(4B) + free_size(4B)
BUDDY_REUSE_EXTRA_SIZE = BUDDY_REUSE_EXTRA.size  # 8 bytes

# ---------------------------------------------------------------------------
# Handshake v4 (buddy pool)
# ---------------------------------------------------------------------------
HANDSHAKE_VERSION = 4

# Control messages (extend v2)
CTRL_BUDDY_ANNOUNCE = 0x03   # Announce a buddy segment to peer
CTRL_BUDDY_FREE = 0x04       # Notify peer of a freed buddy block (for concurrent tracking)


def decode_buddy_payload(payload: bytes | memoryview) -> tuple[int, int, int, bool, int, int]:
    """Decode a FLAG_BUDDY payload header.

    Returns (seg_idx, data_offset, data_size, is_dedicated, free_offset, free_size).
    For normal frames free_offset == data_offset and free_size == data_size.
    For reuse frames (BUDDY_REUSE_FLAG), free coordinates point to the original
    allocation so the consumer can free the correct buddy block.
    """
    if len(payload) < BUDDY_PAYLOAD_SIZE:
        raise ValueError(f'Buddy payload too short: {len(payload)} < {BUDDY_PAYLOAD_SIZE}')
    seg_idx, offset, data_size, flags = BUDDY_PAYLOAD_STRUCT.unpack_from(payload, 0)
    is_dedicated = bool(flags & 1)
    if flags & BUDDY_REUSE_FLAG:
        if len(payload) < BUDDY_PAYLOAD_SIZE + BUDDY_REUSE_EXTRA_SIZE:
            raise ValueError('Buddy reuse payload too short')
        free_offset, free_size = BUDDY_REUSE_EXTRA.unpack_from(payload, BUDDY_PAYLOAD_SIZE)
        return seg_idx, offset, data_size, is_dedicated, free_offset, free_size
    return seg_idx, offset, data_size, is_dedicated, offset, data_size


def encode_buddy_call_frame(
    request_id: int,
    seg_idx: int,
    offset: int,
    data_size: int,
    is_dedicated: bool = False,
) -> bytes:
    """Encode a complete frame for a buddy-backed CRM_CALL.

    The wire data is in the buddy block; the frame only carries the reference.
    """
    payload = encode_buddy_payload(seg_idx, offset, data_size, is_dedicated)
    return encode_frame(request_id, FLAG_BUDDY, payload)


def encode_buddy_reply_frame(
    request_id: int,
    seg_idx: int,
    offset: int,
    data_size: int,
    is_dedicated: bool = False,
) -> bytes:
    """Encode a complete frame for a buddy-backed CRM_REPLY."""
    payload = encode_buddy_payload(seg_idx, offset, data_size, is_dedicated)
    return encode_frame(request_id, FLAG_BUDDY | FLAG_RESPONSE, payload)


def encode_buddy_reuse_reply_frame(
    request_id: int,
    seg_idx: int,
    data_offset: int,
    data_size: int,
    free_offset: int,
    free_size: int,
) -> bytes:
    """Encode a response that reuses the request's buddy block (zero-copy).

    The data coordinates point to the CRM_REPLY header written in-place.
    The free coordinates point to the original allocation for buddy freeing.
    """
    flags = BUDDY_REUSE_FLAG
    buddy = BUDDY_PAYLOAD_STRUCT.pack(seg_idx, data_offset, data_size, flags)
    extra = BUDDY_REUSE_EXTRA.pack(free_offset, free_size)
    return encode_frame(request_id, FLAG_BUDDY | FLAG_RESPONSE, buddy + extra)


# ---------------------------------------------------------------------------
# Buddy handshake codec
# ---------------------------------------------------------------------------

def encode_buddy_handshake(segments: list[tuple[str, int]]) -> bytes:
    """Encode a v4 buddy handshake message.

    Args:
        segments: list of (shm_name, segment_size) for each buddy segment.

    Format: [1B version=4][2B seg_count LE][per-segment: [4B size LE][1B name_len][name UTF-8]]
    """
    parts = [struct.pack('<BH', HANDSHAKE_VERSION, len(segments))]
    for name, size in segments:
        name_bytes = name.encode('utf-8')
        parts.append(struct.pack('<IB', size, len(name_bytes)))
        parts.append(name_bytes)
    return b''.join(parts)


def decode_buddy_handshake(payload: bytes | memoryview) -> list[tuple[str, int]]:
    """Decode a v4 buddy handshake message.

    Returns list of (shm_name, segment_size).
    """
    if len(payload) < 3:
        raise ValueError(f'Buddy handshake too short: {len(payload)}')
    version = payload[0]
    if version != HANDSHAKE_VERSION:
        raise ValueError(f'Expected handshake version {HANDSHAKE_VERSION}, got {version}')
    seg_count = struct.unpack_from('<H', payload, 1)[0]
    offset = 3
    segments = []
    for _ in range(seg_count):
        if offset + 5 > len(payload):
            raise ValueError('Buddy handshake truncated')
        size, name_len = struct.unpack_from('<IB', payload, offset)
        offset += 5
        if offset + name_len > len(payload):
            raise ValueError('Buddy handshake name truncated')
        name = bytes(payload[offset:offset + name_len]).decode('utf-8')
        offset += name_len
        segments.append((name, size))
    return segments


# ---------------------------------------------------------------------------
# Buddy control messages — imported from Rust FFI
# ---------------------------------------------------------------------------
# encode_ctrl_buddy_announce, decode_ctrl_buddy_announce are imported at
# module top from c_two._native.  Re-exported for backward compatibility.
