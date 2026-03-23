"""IPC v2 shared protocol — constants, config, frame codec, and SHM utilities.

Extracted from ``ipc_server.py`` so both server and client can import
without a circular or tight-coupling dependency.  All symbols are public
(no underscore prefix) because they are genuinely shared across modules.
"""

import ctypes
import hashlib
import struct
from dataclasses import dataclass
from multiprocessing import shared_memory

from ... import error
from ..util.adaptive_buffer import AdaptiveBuffer

# ---------------------------------------------------------------------------
# Frame flag bits (uint32, little-endian)
# ---------------------------------------------------------------------------
FLAG_SHM = 1 << 0            # Payload references a per-request SharedMemory segment
FLAG_RESPONSE = 1 << 1       # Server→client direction marker
FLAG_HANDSHAKE = 1 << 2      # Pool SHM handshake message
FLAG_POOL = 1 << 3           # Payload references pre-allocated pool SharedMemory

# ---------------------------------------------------------------------------
# Pre-compiled struct objects for hot-path encoding/decoding
# ---------------------------------------------------------------------------
FRAME_STRUCT = struct.Struct('<IQI')   # 16B frame header: total_len(4) + request_id(8) + flags(4)
U64_STRUCT = struct.Struct('<Q')       # 8B unsigned 64-bit (SHM data size)
U32_STRUCT = struct.Struct('<I')       # 4B unsigned 32-bit (error length, segment size)

# ---------------------------------------------------------------------------
# Performance tuning
# ---------------------------------------------------------------------------
FAST_READ_THRESHOLD = 1_048_576        # 1 MB — use ctypes.memmove above this size (benchmarked)
FRAME_HEADER_SIZE = FRAME_STRUCT.size  # 16 bytes

# ---------------------------------------------------------------------------
# Default configuration values
# ---------------------------------------------------------------------------
DEFAULT_SHM_THRESHOLD = 4_096                       # 4 KB — inline vs SHM cutover
DEFAULT_MAX_FRAME_SIZE = 16_777_216                  # 16 MB — max inline frame
DEFAULT_MAX_PAYLOAD_SIZE = 17_179_869_184            # 16 GB — max SHM payload
DEFAULT_MAX_PENDING_REQUESTS = 1024                  # per-server total
DEFAULT_POOL_SEGMENT_SIZE = 268_435_456              # 256 MB — pool SHM segment

# ---------------------------------------------------------------------------
# SHM garbage collection tuning
# ---------------------------------------------------------------------------
SHM_GC_INTERVAL = 30.0   # seconds — scan interval for leaked SHM segments
SHM_MAX_AGE = 120.0       # seconds — max time before SHM segment is considered leaked


# ---------------------------------------------------------------------------
# IPCConfig dataclass
# ---------------------------------------------------------------------------

@dataclass
class IPCConfig:
    """IPC v2 transport configuration.

    Transport limits (affect correctness and compatibility):
        max_frame_size: Maximum inline frame size in bytes (default 16 MB).
        max_payload_size: Maximum SHM payload size (default 16 GB).
        max_pending_requests: Server-side concurrent request limit (default 1024).

    Performance tuning:
        shm_threshold: Payload size cutover from inline to SHM (default 4 KB).
        pool_enabled: Whether to use pre-allocated pool SHM (default True).
        pool_segment_size: Size of each pool SHM segment (default 256 MB).
        pool_decay_seconds: Idle seconds before pool teardown (default 60, 0=never).
    """
    shm_threshold: int = DEFAULT_SHM_THRESHOLD
    max_frame_size: int = DEFAULT_MAX_FRAME_SIZE
    max_payload_size: int = DEFAULT_MAX_PAYLOAD_SIZE
    max_pending_requests: int = DEFAULT_MAX_PENDING_REQUESTS
    pool_segment_size: int = DEFAULT_POOL_SEGMENT_SIZE
    pool_enabled: bool = True
    pool_decay_seconds: float = 60.0

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


# ---------------------------------------------------------------------------
# SHM naming
# ---------------------------------------------------------------------------

def shm_name(region_id: str, request_id: str, direction: str) -> str:
    """Generate a per-request SHM segment name.

    macOS limits POSIX SHM names to 31 chars (excluding leading ``/``).
    """
    raw = f'{region_id}_{request_id}_{direction}'.encode()
    h = hashlib.md5(raw).hexdigest()[:16]
    d = direction[0]
    return f'cc{d}_{h}'


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
    from ..event.msg_type import MsgType

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
    from ..event.msg_type import MsgType

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


# ---------------------------------------------------------------------------
# SHM read utilities
# ---------------------------------------------------------------------------

def fast_read_shm(
    name: str,
    size: int,
    adaptive_buf: AdaptiveBuffer | None = None,
) -> tuple[memoryview, AdaptiveBuffer]:
    """Read SHM using native memcpy into an :class:`AdaptiveBuffer`.

    For large payloads, uses ctypes.memmove instead of
    Python's ``bytes(shm.buf)`` to avoid per-call mmap overhead.

    The adaptive buffer grows when needed and shrinks when consecutive reads
    are significantly smaller than capacity (see :mod:`~c_two.rpc.util.adaptive_buffer`).

    Returns ``(data_view, adaptive_buf)`` — pass *adaptive_buf* back on the
    next call for reuse.
    """
    if adaptive_buf is None:
        adaptive_buf = AdaptiveBuffer()

    shm = shared_memory.SharedMemory(name=name, create=False, track=False)
    try:
        # Validate SHM actual size covers the declared size BEFORE allocating
        if shm.size < size:
            raise error.EventDeserializeError(
                f'SHM segment {name!r} actual size {shm.size} < declared size {size}'
            )

        buf = adaptive_buf.acquire(size)

        if size >= FAST_READ_THRESHOLD:
            ctypes.memmove(
                ctypes.addressof(ctypes.c_char.from_buffer(buf)),
                ctypes.addressof(ctypes.c_char.from_buffer(shm.buf)),
                size,
            )
        else:
            buf[:size] = shm.buf[:size]
    finally:
        shm.close()
        shm.unlink()
    return memoryview(buf)[:size], adaptive_buf


def read_from_pool_shm(
    shm_buf: memoryview,
    size: int,
    adaptive_buf: AdaptiveBuffer | None = None,
) -> tuple[memoryview, AdaptiveBuffer]:
    """Read from a pre-opened pool SHM buffer into an :class:`AdaptiveBuffer`.

    Unlike :func:`fast_read_shm`, does **not** open or unlink the SHM — the
    handle is assumed to be pre-opened and cached from the pool handshake.
    """
    if adaptive_buf is None:
        adaptive_buf = AdaptiveBuffer()

    if size > len(shm_buf):
        raise error.EventDeserializeError(
            f'Pool read size {size} exceeds SHM buffer length {len(shm_buf)}'
        )

    buf = adaptive_buf.acquire(size)

    if size >= FAST_READ_THRESHOLD:
        ctypes.memmove(
            ctypes.addressof(ctypes.c_char.from_buffer(buf)),
            ctypes.addressof(ctypes.c_char.from_buffer(shm_buf)),
            size,
        )
    else:
        buf[:size] = shm_buf[:size]

    return memoryview(buf)[:size], adaptive_buf
