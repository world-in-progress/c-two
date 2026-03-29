"""IPC shared protocol — constants, config, frame codec, and SHM utilities.

Extracted from ``ipc_server.py`` so both server and client can import
without a circular or tight-coupling dependency.  All symbols are public
(no underscore prefix) because they are genuinely shared across modules.
"""

import ctypes
import hashlib
import os
import struct
from dataclasses import dataclass
from enum import IntEnum
from multiprocessing import shared_memory

from ... import error

# ---------------------------------------------------------------------------
# Frame flag bits (uint32, little-endian)
# ---------------------------------------------------------------------------
FLAG_SHM = 1 << 0            # Payload references a per-request SharedMemory segment
FLAG_RESPONSE = 1 << 1       # Server→client direction marker
FLAG_HANDSHAKE = 1 << 2      # Pool SHM handshake message
FLAG_POOL = 1 << 3           # Payload references pre-allocated pool SharedMemory
FLAG_CTRL = 1 << 4           # Control message (segment announce, consumed signal)
FLAG_DISK_SPILL = 1 << 5     # Reserved for Phase 2 disk spillover

# ---------------------------------------------------------------------------
# Pre-compiled struct objects for hot-path encoding/decoding
# ---------------------------------------------------------------------------
FRAME_STRUCT = struct.Struct('<IQI')   # 16B frame header: total_len(4) + request_id(8) + flags(4)
U64_STRUCT = struct.Struct('<Q')       # 8B unsigned 64-bit (SHM data size)
U32_STRUCT = struct.Struct('<I')       # 4B unsigned 32-bit (error length, segment size)

# ---------------------------------------------------------------------------
# Performance tuning
# ---------------------------------------------------------------------------
FRAME_HEADER_SIZE = FRAME_STRUCT.size  # 16 bytes
POOL_PAYLOAD_HEADER_SIZE = 9           # 1B segment_index + 8B data_size

# ---------------------------------------------------------------------------
# Default configuration values
# ---------------------------------------------------------------------------
DEFAULT_SHM_THRESHOLD = 4_096                       # 4 KB — inline vs SHM cutover
DEFAULT_MAX_FRAME_SIZE = 16_777_216                  # 16 MB — max inline frame
DEFAULT_MAX_PAYLOAD_SIZE = 17_179_869_184            # 16 GB — max SHM payload
DEFAULT_MAX_PENDING_REQUESTS = 1024                  # per-server total
DEFAULT_POOL_SEGMENT_SIZE = 268_435_456              # 256 MB — pool SHM segment
DEFAULT_MAX_POOL_SEGMENTS = 4                        # max segments per pool
DEFAULT_MAX_POOL_MEMORY = DEFAULT_POOL_SEGMENT_SIZE * DEFAULT_MAX_POOL_SEGMENTS  # 1 GB per pool

# ---------------------------------------------------------------------------
# IPCConfig dataclass
# ---------------------------------------------------------------------------

@dataclass
class IPCConfig:
    """IPC transport configuration.

    Transport limits (affect correctness and compatibility):
        max_frame_size: Maximum inline frame size in bytes (default 16 MB).
        max_payload_size: Maximum SHM payload size (default 16 GB).
        max_pending_requests: Server-side concurrent request limit (default 1024).

    Performance tuning:
        shm_threshold: Payload size cutover from inline to SHM (default 4 KB).
        pool_enabled: Whether to use pre-allocated pool SHM (default True).
        pool_segment_size: Size of each pool SHM segment (default 256 MB).
        pool_decay_seconds: Idle seconds before pool teardown (default 60, 0=never).
        max_pool_memory: Memory budget per pool direction (default 1 GB).

    Heartbeat (server probes client liveness):
        heartbeat_interval: Seconds between PING probes (default 15; ≤0 disables).
        heartbeat_timeout: Seconds with no activity before declaring dead (default 30).
    """
    shm_threshold: int = DEFAULT_SHM_THRESHOLD
    max_frame_size: int = DEFAULT_MAX_FRAME_SIZE
    max_payload_size: int = DEFAULT_MAX_PAYLOAD_SIZE
    max_pending_requests: int = DEFAULT_MAX_PENDING_REQUESTS
    pool_segment_size: int = DEFAULT_POOL_SEGMENT_SIZE
    pool_enabled: bool = True
    pool_decay_seconds: float = 60.0
    max_pool_segments: int = DEFAULT_MAX_POOL_SEGMENTS
    max_pool_memory: int = DEFAULT_MAX_POOL_MEMORY
    heartbeat_interval: float = 15.0
    heartbeat_timeout: float = 30.0

    # Chunked transfer settings
    chunk_threshold_ratio: float = 0.9
    chunk_assembler_timeout: float = 60.0
    chunk_gc_interval: int = 100
    max_total_chunks: int = 512
    max_reassembly_bytes: int = 8 * (1 << 30)  # 8 GB

    def __post_init__(self) -> None:
        if self.pool_segment_size > 0xFFFFFFFF:
            raise ValueError(
                f'pool_segment_size {self.pool_segment_size} exceeds uint32 max '
                f'(handshake wire format is uint32)'
            )
        if self.pool_segment_size <= 0:
            raise ValueError('pool_segment_size must be > 0')
        if self.max_frame_size <= 16:
            raise ValueError('max_frame_size must be > 16 (header size)')
        if self.max_payload_size <= 0:
            raise ValueError('max_payload_size must be > 0')
        if self.shm_threshold > self.max_frame_size:
            raise ValueError(
                f'shm_threshold ({self.shm_threshold}) must not exceed '
                f'max_frame_size ({self.max_frame_size})'
            )
        if self.pool_segment_size > self.max_payload_size:
            self.pool_segment_size = self.max_payload_size
        if not (1 <= self.max_pool_segments <= 255):
            raise ValueError(
                f'max_pool_segments must be >= 1 and <= 255, got {self.max_pool_segments}'
            )
        if self.max_pool_memory < self.pool_segment_size:
            raise ValueError(
                f'max_pool_memory ({self.max_pool_memory}) must be >= '
                f'pool_segment_size ({self.pool_segment_size})'
            )
        if self.heartbeat_interval > 0 and self.heartbeat_timeout <= self.heartbeat_interval:
            raise ValueError(
                f'heartbeat_timeout ({self.heartbeat_timeout}) must be > '
                f'heartbeat_interval ({self.heartbeat_interval})'
            )
        if not (0.0 < self.chunk_threshold_ratio <= 1.0):
            raise ValueError(f'chunk_threshold_ratio must be in (0, 1], got {self.chunk_threshold_ratio}')
        if self.max_total_chunks < 1:
            raise ValueError(f'max_total_chunks must be >= 1, got {self.max_total_chunks}')
        if self.max_reassembly_bytes < 1:
            raise ValueError(f'max_reassembly_bytes must be >= 1, got {self.max_reassembly_bytes}')


# ---------------------------------------------------------------------------
# SHM naming
# ---------------------------------------------------------------------------

def shm_name(region_id: str, request_id: str, direction: str) -> str:
    """Generate a per-request SHM segment name with embedded creator PID.

    Format: ``cc{d}{pid_hex}_{hash_hex}`` — always exactly 20 chars.
    macOS limits POSIX SHM names to 31 chars (excluding leading ``/``).
    """
    pid_hex = format(os.getpid(), 'x')
    raw = f'{region_id}_{request_id}_{direction}'.encode()
    hash_len = 16 - len(pid_hex)
    h = hashlib.md5(raw).hexdigest()[:hash_len]
    d = direction[0]
    return f'cc{d}{pid_hex}_{h}'


def extract_pid_from_shm_name(name: str) -> int | None:
    """Extract creator PID from a C-Two SHM name.

    Handles per-request (``cc{d}{pid}_{hash}``), legacy pool
    (``ccp{d}{pid}_{hash}``), and split pool
    (``ccpo{pid}_{hash}``, ``ccps{pid}_{hash}``) formats.
    Returns ``None`` if *name* does not match any pattern.
    """
    if name.startswith('ccpo') or name.startswith('ccps'):
        rest = name[4:]   # skip 'ccpo' / 'ccps'
    elif name.startswith('ccp'):
        rest = name[4:]   # skip 'ccp' + direction char
    elif name.startswith('cc'):
        rest = name[3:]   # skip 'cc' + direction char
    else:
        return None
    idx = rest.find('_')
    if idx <= 0:
        return None
    try:
        return int(rest[:idx], 16)
    except ValueError:
        return None


# ---------------------------------------------------------------------------
# Segment state (for future full-duplex readiness)
# ---------------------------------------------------------------------------

class SegmentState(IntEnum):
    """Per-segment state machine for pool SHM segments.

    Half-duplex: transitions are immediate (write → FREE since reader
    is blocked). Full-duplex (Phase 2): CONSUMED signal drives FREE.
    """
    FREE = 0
    IN_USE = 1


# ---------------------------------------------------------------------------
# Control message codec
# ---------------------------------------------------------------------------
# CTRL frame: [16B frame header (rid=0, flags=FLAG_CTRL)][1B ctrl_type][payload]

CTRL_SEGMENT_ANNOUNCE = 0x01   # Announce a new pool segment to peer
CTRL_CONSUMED = 0x02           # Signal that a segment has been consumed

# Pool direction byte values
POOL_DIR_OUTBOUND = 0   # Client → Server (request direction)
POOL_DIR_RESPONSE = 1   # Server → Client (response direction)

_CTRL_ANNOUNCE_HEADER = struct.Struct('<BBBI')  # ctrl_type(1) + direction(1) + index(1) + size(4)


def encode_ctrl_segment_announce(direction: int, index: int, size: int, name: str) -> bytes:
    """Encode a CTRL_SEGMENT_ANNOUNCE message.

    Format: ``[1B ctrl=0x01][1B direction][1B index][4B size LE][name UTF-8]``
    """
    name_bytes = name.encode('utf-8')
    buf = bytearray(7 + len(name_bytes))
    _CTRL_ANNOUNCE_HEADER.pack_into(buf, 0, CTRL_SEGMENT_ANNOUNCE, direction, index, size)
    buf[7:] = name_bytes
    return bytes(buf)


def decode_ctrl_segment_announce(payload: bytes | memoryview) -> tuple[int, int, int, str]:
    """Decode a CTRL_SEGMENT_ANNOUNCE message.

    Returns ``(direction, segment_index, segment_size, shm_name)``.
    """
    if len(payload) < 7:
        raise ValueError(f'CTRL_SEGMENT_ANNOUNCE too short: {len(payload)}')
    _, direction, index, size = _CTRL_ANNOUNCE_HEADER.unpack_from(payload, 0)
    name = bytes(payload[7:]).decode('utf-8')
    return direction, index, size, name


def encode_ctrl_consumed(direction: int, index: int) -> bytes:
    """Encode a CTRL_CONSUMED message.

    Format: ``[1B ctrl=0x02][1B direction][1B index]``
    """
    return bytes((CTRL_CONSUMED, direction, index))


def decode_ctrl_consumed(payload: bytes | memoryview) -> tuple[int, int]:
    """Decode a CTRL_CONSUMED message.

    Returns ``(direction, segment_index)``.
    """
    if len(payload) < 3:
        raise ValueError(f'CTRL_CONSUMED too short: {len(payload)}')
    return int(payload[1]), int(payload[2])


# ---------------------------------------------------------------------------
# Frame encoding / decoding
# ---------------------------------------------------------------------------

def encode_frame(request_id: int, flags: int, payload: bytes | bytearray | memoryview) -> bytes:
    """Encode a 16-byte-header frame: ``[total_len][request_id][flags][payload]``."""
    payload_len = len(payload)
    total_len = 12 + payload_len  # 8B rid + 4B flags + payload
    if total_len > 0xFFFFFFFF:
        raise OverflowError(
            f'Frame total_len {total_len} exceeds uint32 max; '
            f'use SHM transport for payloads > {0xFFFFFFFF - 12} bytes'
        )
    buf = bytearray(4 + total_len)
    FRAME_STRUCT.pack_into(buf, 0, total_len, request_id, flags)
    buf[16:16 + payload_len] = payload
    return bytes(buf)


def encode_inline_call_frame(
    request_id: int,
    method_name: str,
    args: bytes | memoryview,
    call_header_cache: dict[str, bytes],
) -> bytes:
    """Single-allocation frame for inline CRM_CALL (eliminates encode_call + encode_frame double copy)."""
    from .msg_type import MsgType

    cached_header = call_header_cache.get(method_name)
    method_bytes: bytes | None = None
    if cached_header is not None:
        call_header_len = len(cached_header)
    else:
        method_bytes = method_name.encode('utf-8')
        call_header_len = 3 + len(method_bytes)

    args_len = len(args) if args else 0
    wire_len = call_header_len + args_len
    total_len = 12 + wire_len
    buf = bytearray(4 + total_len)

    # Frame header (16 bytes)
    FRAME_STRUCT.pack_into(buf, 0, total_len, request_id, 0)

    # Wire call message — directly into frame buffer
    off = 16
    if cached_header is not None:
        buf[off:off + call_header_len] = cached_header
    else:
        buf[off] = MsgType.CRM_CALL
        struct.pack_into('<H', buf, off + 1, len(method_bytes))
        buf[off + 3:off + 3 + len(method_bytes)] = method_bytes

    if args_len > 0:
        buf[off + call_header_len:off + call_header_len + args_len] = args

    return bytes(buf)


def encode_inline_reply_frame(
    request_id: int,
    flags: int,
    err_bytes: bytes | memoryview,
    result_bytes: bytes | memoryview,
) -> bytes:
    """Single-allocation frame for inline CRM_REPLY (eliminates encode_reply + encode_frame double copy)."""
    from .msg_type import MsgType

    err_len = len(err_bytes) if err_bytes else 0
    result_len = len(result_bytes) if result_bytes else 0
    wire_len = 5 + err_len + result_len  # 1B type + 4B error_len + error + result
    total_len = 12 + wire_len
    buf = bytearray(4 + total_len)

    # Frame header
    FRAME_STRUCT.pack_into(buf, 0, total_len, request_id, flags)

    # Wire reply message — directly into frame buffer
    off = 16
    buf[off] = MsgType.CRM_REPLY
    U32_STRUCT.pack_into(buf, off + 1, err_len)
    if err_len > 0:
        buf[off + 5:off + 5 + err_len] = err_bytes
    if result_len > 0:
        buf[off + 5 + err_len:off + 5 + err_len + result_len] = result_bytes

    return bytes(buf)


def decode_frame(body: bytes) -> tuple[int, int, bytes]:
    """Decode a frame body (without the leading 4-byte total_len) into ``(request_id, flags, payload)``."""
    body_len = len(body)
    if body_len < 12:
        raise error.EventDeserializeError(f'Frame body too small: {body_len} < 12')
    request_id, flags = U64_STRUCT.unpack_from(body, 0)[0], U32_STRUCT.unpack_from(body, 8)[0]
    payload = body[12:]
    return request_id, flags, payload


