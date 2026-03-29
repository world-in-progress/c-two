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
"""
from __future__ import annotations

import struct

from .ipc.frame import (
    FRAME_STRUCT,
    FLAG_RESPONSE,
    encode_frame,
)
from .ipc.buddy import (
    FLAG_BUDDY,
    BUDDY_PAYLOAD_STRUCT,
    encode_buddy_payload,
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
# Call Control
# ---------------------------------------------------------------------------


# Pre-computed default-route (name='') call control payloads for method indices
# 0–255.  Eliminates bytearray allocation + pack + bytes() on the hot path.
_DEFAULT_ROUTE_CACHE: list[bytes] = []
for _i in range(256):
    _b = bytearray(3)
    _b[0] = 0
    _U16.pack_into(_b, 1, _i)
    _DEFAULT_ROUTE_CACHE.append(bytes(_b))
del _i, _b


def encode_call_control(name: str, method_idx: int) -> bytes:
    """Encode call control payload.

    Format: ``[1B name_len][route_name UTF-8][2B method_idx LE]``
    """
    if name:
        name_b = name.encode('utf-8')
        buf = bytearray(1 + len(name_b) + 2)
        buf[0] = len(name_b)
        buf[1:1 + len(name_b)] = name_b
        _U16.pack_into(buf, 1 + len(name_b), method_idx)
        return bytes(buf)
    if method_idx < 256:
        return _DEFAULT_ROUTE_CACHE[method_idx]
    buf = bytearray(3)
    buf[0] = 0
    _U16.pack_into(buf, 1, method_idx)
    return bytes(buf)


def decode_call_control(
    data: bytes | memoryview, offset: int = 0,
) -> tuple[str, int, int]:
    """Decode call control payload.

    Returns ``(route_name, method_idx, bytes_consumed)``.

    Raises ``ValueError`` if the buffer is too short.
    """
    dlen = len(data)
    if offset >= dlen:
        raise ValueError('call control: offset beyond buffer')
    name_len = data[offset]
    # Minimum remaining: 1B name_len + name_len + 2B method_idx
    if offset + 1 + name_len + 2 > dlen:
        raise ValueError(
            f'call control: buffer too short for name_len={name_len} '
            f'(need {1 + name_len + 2}, have {dlen - offset})'
        )
    off = offset + 1
    if name_len > 0:
        name = bytes(data[off:off + name_len]).decode('utf-8')
        off += name_len
    else:
        name = ''
    method_idx = _U16.unpack_from(data, off)[0]
    off += 2
    return name, method_idx, off - offset


# ---------------------------------------------------------------------------
# Reply Control
# ---------------------------------------------------------------------------


def encode_reply_control(status: int, error_data: bytes | None = None) -> bytes:
    """Encode reply control payload.

    Format::

        [1B status]
        if STATUS_ERROR: [4B error_len LE][error_bytes]
    """
    if status == STATUS_ERROR and error_data:
        buf = bytearray(1 + 4 + len(error_data))
        buf[0] = STATUS_ERROR
        _U32.pack_into(buf, 1, len(error_data))
        buf[5:] = error_data
        return bytes(buf)
    return bytes([status])


def decode_reply_control(
    data: bytes | memoryview, offset: int = 0,
) -> tuple[int, bytes | None, int]:
    """Decode reply control payload.

    Returns ``(status, error_data_or_none, bytes_consumed)``.

    Raises ``ValueError`` if the buffer is too short.
    """
    dlen = len(data)
    if offset >= dlen:
        raise ValueError('reply control: offset beyond buffer')
    status = data[offset]
    off = offset + 1
    if status == STATUS_ERROR:
        if off + 4 > dlen:
            raise ValueError('reply control: buffer too short for error length')
        err_len = _U32.unpack_from(data, off)[0]; off += 4
        if off + err_len > dlen:
            raise ValueError(
                f'reply control: error data claims {err_len} bytes '
                f'but only {dlen - off} available'
            )
        error_data = bytes(data[off:off + err_len]); off += err_len
        return status, error_data, off - offset
    return status, None, off - offset


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
# Chunk Header Codec
# ---------------------------------------------------------------------------

_CHUNK_HDR = struct.Struct('<HH')  # 4 bytes: chunk_idx(u16) + total_chunks(u16)
CHUNK_HEADER_SIZE = _CHUNK_HDR.size


def encode_chunk_header(chunk_idx: int, total_chunks: int) -> bytes:
    """Encode chunk header: ``[2B chunk_idx LE][2B total_chunks LE]``."""
    return _CHUNK_HDR.pack(chunk_idx, total_chunks)


def decode_chunk_header(
    data: bytes | memoryview, offset: int = 0,
) -> tuple[int, int, int]:
    """Decode chunk header.

    Returns ``(chunk_idx, total_chunks, bytes_consumed)``.
    """
    if offset + CHUNK_HEADER_SIZE > len(data):
        raise ValueError(
            f'chunk header: need {CHUNK_HEADER_SIZE} bytes at offset {offset}, '
            f'have {len(data) - offset}'
        )
    chunk_idx, total_chunks = _CHUNK_HDR.unpack_from(data, offset)
    return chunk_idx, total_chunks, CHUNK_HEADER_SIZE


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
    chunk_hdr = _CHUNK_HDR.pack(chunk_idx, total_chunks)
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
    chunk_hdr = _CHUNK_HDR.pack(chunk_idx, total_chunks)
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
    chunk_hdr = _CHUNK_HDR.pack(chunk_idx, total_chunks)
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
    chunk_hdr = _CHUNK_HDR.pack(chunk_idx, total_chunks)
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

    Built during handshake v5 and used per-request to replace method name
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
