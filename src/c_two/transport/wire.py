"""Wire codec — control-plane / data-plane frame separation.

Control and data planes are cleanly separated:

- **Control payload** travels in the UDS inline frame (small, always inline).
- **Data payload** sits at offset 0 in the SHM buddy block — pure serialized
  bytes, no headers.

Call Control (appended to buddy pointer, or prepended to inline data)::

    [1B name_len][route_name UTF-8][2B method_idx LE]

    Total: 3 + name_len bytes.  name_len=0 → use connection default route.

Reply Control::

    [1B status]
    if status == STATUS_SUCCESS (0x00):
        (data follows — inline or in buddy SHM)
    if status == STATUS_ERROR (0x01):
        [4B error_len LE][error_bytes]

Frame identification (per frame header flags):
- ``FLAG_CALL`` (bit 7): request frame carries call control.
- ``FLAG_REPLY`` (bit 8): response frame carries reply control.

.. note::

   As of Phase 1, primitive codecs delegate to Rust (``c2-wire`` via
   ``c_two._native``).  Composite frame builders remain in Python and
   call the Rust primitives.
"""
from __future__ import annotations

import struct

from c_two._native import (  # noqa: F401 — re-export
    encode_call_control,
    encode_chunk_header,
    CHUNK_HEADER_SIZE,
)

# Rust FFI primitives — imported with underscore prefix; public wrappers below
# preserve Python defaults (offset=0, error_data=None, is_dedicated=False).
from c_two._native import (
    decode_call_control as _ffi_decode_call_control,
    encode_reply_control as _ffi_encode_reply_control,
    decode_reply_control as _ffi_decode_reply_control,
    encode_buddy_payload as _ffi_encode_buddy_payload,
    decode_buddy_payload as _ffi_decode_buddy_payload,
    decode_chunk_header as _ffi_decode_chunk_header,
)

from .ipc.frame import (
    FRAME_STRUCT,
    FLAG_RESPONSE,
    encode_frame,
)
from .ipc.shm_frame import (
    FLAG_BUDDY,
    BUDDY_PAYLOAD_STRUCT,
)
from .protocol import (
    FLAG_CALL,
    FLAG_REPLY,
    FLAG_CHUNKED,
    FLAG_CHUNK_LAST,
    STATUS_SUCCESS,
    STATUS_ERROR,
)

# ---------------------------------------------------------------------------
# Struct helpers
# ---------------------------------------------------------------------------

_U16 = struct.Struct('<H')
_U32 = struct.Struct('<I')

# Public alias for modules that need fast inline method_idx unpacking.
U16_LE = _U16


def decode_call_control(
    data: bytes | memoryview, offset: int = 0,
) -> tuple[str, int, int]:
    """Decode call control → ``(route_name, method_idx, bytes_consumed)``."""
    return _ffi_decode_call_control(bytes(data), offset)


def encode_reply_control(status: int, error_data: bytes | None = None) -> bytes:
    """Encode reply control payload."""
    return _ffi_encode_reply_control(status, error_data)


def decode_reply_control(
    data: bytes | memoryview, offset: int = 0,
) -> tuple[int, bytes | None, int]:
    """Decode reply control → ``(status, error_data_or_none, bytes_consumed)``."""
    return _ffi_decode_reply_control(bytes(data), offset)


def encode_buddy_payload(
    seg_idx: int, offset: int, data_size: int, is_dedicated: bool = False,
) -> bytes:
    """Encode a FLAG_BUDDY payload header (11 bytes)."""
    return _ffi_encode_buddy_payload(seg_idx, offset, data_size, is_dedicated)


def decode_buddy_payload(payload: bytes | memoryview) -> tuple[int, int, int, bool]:
    """Decode a FLAG_BUDDY payload header → ``(seg_idx, offset, data_size, is_dedicated)``."""
    return _ffi_decode_buddy_payload(bytes(payload))


def decode_chunk_header(
    data: bytes | memoryview, offset: int = 0,
) -> tuple[int, int, int]:
    """Decode chunk header → ``(chunk_idx, total_chunks, bytes_consumed)``."""
    return _ffi_decode_chunk_header(bytes(data), offset)


def payload_total_size(payload) -> int:
    """Return total byte length of *payload*.

    *payload* may be ``bytes``, ``memoryview``, a ``tuple``/``list`` of
    such segments (scatter-write), or ``None``.
    """
    if payload is None:
        return 0
    if isinstance(payload, (list, tuple)):
        return sum(len(s) for s in payload)
    return len(payload)

# ---------------------------------------------------------------------------
# Call Control — pre-computed cache for default-route hot path
# ---------------------------------------------------------------------------

_DEFAULT_ROUTE_CACHE: list[bytes] = []
for _i in range(256):
    _DEFAULT_ROUTE_CACHE.append(encode_call_control('', _i))
del _i


# ---------------------------------------------------------------------------
# Frame Builders — Call
# ---------------------------------------------------------------------------


def encode_buddy_call_frame(
    request_id: int,
    seg_idx: int,
    offset: int,
    data_size: int,
    is_dedicated: bool,
    name: str,
    method_idx: int,
) -> bytes:
    """Build a complete buddy call frame.

    Frame layout::

        [16B header (flags=FLAG_BUDDY|FLAG_CALL)]
        [11B buddy_payload]
        [call_control]
    """
    buddy = encode_buddy_payload(seg_idx, offset, data_size, is_dedicated)
    ctrl = encode_call_control(name, method_idx)
    payload = buddy + ctrl
    flags = FLAG_BUDDY | FLAG_CALL
    return encode_frame(request_id, flags, payload)


def encode_inline_call_frame(
    request_id: int,
    name: str,
    method_idx: int,
    data: bytes | memoryview,
) -> bytes:
    """Build a complete inline call frame (small payloads, no buddy).

    Frame layout::

        [16B header (flags=FLAG_CALL)]
        [call_control]
        [inline_data]
    """
    ctrl = encode_call_control(name, method_idx)
    payload = ctrl + bytes(data) if data else ctrl
    return encode_frame(request_id, FLAG_CALL, payload)


# ---------------------------------------------------------------------------
# Frame Builders — Reply
# ---------------------------------------------------------------------------


_BUDDY_REPLY_FLAGS = FLAG_RESPONSE | FLAG_BUDDY | FLAG_REPLY
_INLINE_REPLY_FLAGS = FLAG_RESPONSE | FLAG_REPLY
_BUDDY_REPLY_PAYLOAD_LEN = BUDDY_PAYLOAD_STRUCT.size + 1  # 11B buddy + 1B status


def encode_buddy_reply_frame(
    request_id: int,
    seg_idx: int,
    offset: int,
    data_size: int,
    is_dedicated: bool,
) -> bytearray:
    """Build a buddy success reply frame (single allocation, no copy)."""
    total_len = 12 + _BUDDY_REPLY_PAYLOAD_LEN
    buf = bytearray(4 + total_len)
    FRAME_STRUCT.pack_into(buf, 0, total_len, request_id, _BUDDY_REPLY_FLAGS)
    BUDDY_PAYLOAD_STRUCT.pack_into(
        buf, 16, seg_idx, offset, data_size, 1 if is_dedicated else 0,
    )
    buf[16 + BUDDY_PAYLOAD_STRUCT.size] = STATUS_SUCCESS
    return buf


def encode_inline_reply_frame(
    request_id: int,
    result_data: bytes | memoryview | None = None,
) -> bytearray:
    """Build an inline success reply frame (single allocation, no copy)."""
    data_len = len(result_data) if result_data else 0
    total_len = 12 + 1 + data_len
    buf = bytearray(4 + total_len)
    FRAME_STRUCT.pack_into(buf, 0, total_len, request_id, _INLINE_REPLY_FLAGS)
    buf[16] = STATUS_SUCCESS
    if data_len > 0:
        buf[17:17 + data_len] = result_data
    return buf


def encode_error_reply_frame(
    request_id: int,
    error_data: bytes,
) -> bytearray:
    """Build an error reply frame (single allocation, no copy)."""
    err_len = len(error_data)
    total_len = 12 + 1 + 4 + err_len
    buf = bytearray(4 + total_len)
    FRAME_STRUCT.pack_into(buf, 0, total_len, request_id, _INLINE_REPLY_FLAGS)
    buf[16] = STATUS_ERROR
    _U32.pack_into(buf, 17, err_len)
    buf[21:21 + err_len] = error_data
    return buf


# ---------------------------------------------------------------------------
# Chunk Header — imported from Rust FFI (re-exported at module top)
# ---------------------------------------------------------------------------

# encode_chunk_header, decode_chunk_header, CHUNK_HEADER_SIZE are imported
# from c_two._native at the top of this module.


# ---------------------------------------------------------------------------
# Chunked Frame Builders — Call
# ---------------------------------------------------------------------------


def encode_buddy_chunked_call_frame(
    request_id: int,
    seg_idx: int,
    offset: int,
    data_size: int,
    is_dedicated: bool,
    chunk_idx: int,
    total_chunks: int,
    name: str = '',
    method_idx: int = 0,
) -> bytes:
    """Build a buddy chunked call frame.

    Frame layout::

        [16B header (flags=FLAG_BUDDY|FLAG_CALL|FLAG_CHUNKED[|FLAG_CHUNK_LAST])]
        [11B buddy_payload]
        [4B chunk_header]
        [call_control]    ← only when chunk_idx==0

    ``name`` and ``method_idx`` are ignored when ``chunk_idx > 0``.
    """
    buddy = encode_buddy_payload(seg_idx, offset, data_size, is_dedicated)
    chunk_hdr = encode_chunk_header(chunk_idx, total_chunks)
    flags = FLAG_BUDDY | FLAG_CALL | FLAG_CHUNKED
    if chunk_idx == total_chunks - 1:
        flags |= FLAG_CHUNK_LAST
    if chunk_idx == 0:
        ctrl = encode_call_control(name, method_idx)
        payload = buddy + chunk_hdr + ctrl
    else:
        payload = buddy + chunk_hdr
    return encode_frame(request_id, flags, payload)


def encode_inline_chunked_call_frame(
    request_id: int,
    chunk_idx: int,
    total_chunks: int,
    data: bytes | memoryview,
    name: str = '',
    method_idx: int = 0,
) -> bytes:
    """Build a inline chunked call frame (no buddy).

    Frame layout::

        [16B header (flags=FLAG_CALL|FLAG_CHUNKED[|FLAG_CHUNK_LAST])]
        [4B chunk_header]
        [call_control]    ← only when chunk_idx==0
        [inline_data]

    ``name`` and ``method_idx`` are ignored when ``chunk_idx > 0``.
    """
    chunk_hdr = encode_chunk_header(chunk_idx, total_chunks)
    flags = FLAG_CALL | FLAG_CHUNKED
    if chunk_idx == total_chunks - 1:
        flags |= FLAG_CHUNK_LAST
    if chunk_idx == 0:
        ctrl = encode_call_control(name, method_idx)
        payload = chunk_hdr + ctrl + bytes(data)
    else:
        payload = chunk_hdr + bytes(data)
    return encode_frame(request_id, flags, payload)


# ---------------------------------------------------------------------------
# Chunked Frame Builders — Reply
# ---------------------------------------------------------------------------


_BUDDY_CHUNKED_REPLY_BASE_FLAGS = FLAG_RESPONSE | FLAG_BUDDY | FLAG_REPLY | FLAG_CHUNKED
_INLINE_CHUNKED_REPLY_BASE_FLAGS = FLAG_RESPONSE | FLAG_REPLY | FLAG_CHUNKED


def encode_buddy_chunked_reply_frame(
    request_id: int,
    seg_idx: int,
    offset: int,
    data_size: int,
    is_dedicated: bool,
    chunk_idx: int,
    total_chunks: int,
) -> bytes:
    """Build a buddy chunked reply frame.

    Frame layout::

        [16B header (flags=RESPONSE|BUDDY|REPLY|CHUNKED[|CHUNK_LAST])]
        [11B buddy_payload]
        [4B chunk_header]
        [1B status]          ← only when chunk_idx==0 (always STATUS_SUCCESS)
    """
    buddy = encode_buddy_payload(seg_idx, offset, data_size, is_dedicated)
    chunk_hdr = encode_chunk_header(chunk_idx, total_chunks)
    flags = _BUDDY_CHUNKED_REPLY_BASE_FLAGS
    if chunk_idx == total_chunks - 1:
        flags |= FLAG_CHUNK_LAST
    if chunk_idx == 0:
        payload = buddy + chunk_hdr + bytes([STATUS_SUCCESS])
    else:
        payload = buddy + chunk_hdr
    return encode_frame(request_id, flags, payload)


def encode_inline_chunked_reply_frame(
    request_id: int,
    chunk_idx: int,
    total_chunks: int,
    data: bytes | memoryview,
) -> bytes:
    """Build a inline chunked reply frame.

    Frame layout::

        [16B header (flags=RESPONSE|REPLY|CHUNKED[|CHUNK_LAST])]
        [4B chunk_header]
        [1B status]          ← only when chunk_idx==0 (always STATUS_SUCCESS)
        [inline_data]
    """
    chunk_hdr = encode_chunk_header(chunk_idx, total_chunks)
    flags = _INLINE_CHUNKED_REPLY_BASE_FLAGS
    if chunk_idx == total_chunks - 1:
        flags |= FLAG_CHUNK_LAST
    if chunk_idx == 0:
        payload = chunk_hdr + bytes([STATUS_SUCCESS]) + bytes(data)
    else:
        payload = chunk_hdr + bytes(data)
    return encode_frame(request_id, flags, payload)


# ---------------------------------------------------------------------------
# MethodTable — bidirectional name ↔ index mapping
# ---------------------------------------------------------------------------


class MethodTable:
    """Maps method names to 2-byte indices (and back).

    Built during handshake and used per-request to replace method name
    strings with compact indices.
    """

    def __init__(self) -> None:
        self._name_to_idx: dict[str, int] = {}
        self._idx_to_name: dict[int, str] = {}

    @classmethod
    def from_methods(cls, method_names: list[str]) -> MethodTable:
        """Create a table assigning sequential indices to method names."""
        table = cls()
        for i, name in enumerate(method_names):
            table._name_to_idx[name] = i
            table._idx_to_name[i] = name
        return table

    def add(self, name: str, idx: int) -> None:
        self._name_to_idx[name] = idx
        self._idx_to_name[idx] = name

    def index_of(self, name: str) -> int:
        return self._name_to_idx[name]

    def name_of(self, idx: int) -> str:
        return self._idx_to_name[idx]

    def has_name(self, name: str) -> bool:
        return name in self._name_to_idx

    def has_index(self, idx: int) -> bool:
        return idx in self._idx_to_name

    def __len__(self) -> int:
        return len(self._name_to_idx)

    def names(self) -> list[str]:
        return list(self._name_to_idx.keys())
