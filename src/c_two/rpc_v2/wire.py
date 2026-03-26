"""Wire v2 — control-plane / data-plane separated frame codec.

In wire v1 (``rpc.util.wire``), the SHM buddy block contains the full wire
message: ``[type][method_len][method_name][payload]``.  Control metadata
(method name, error info) is mixed with data in the SHM block.

In wire v2, control and data are cleanly separated:

- **Control payload** travels in the UDS inline frame (small, always inline).
- **Data payload** sits at offset 0 in the SHM buddy block — pure serialized
  bytes, no headers.

This separation eliminates:
- Extra copies when concatenating method name + payload into a single buffer.
- 5-byte ``CRM_REPLY`` header overhead in every SHM block.
- Error bytes polluting SHM (errors are always inline in wire v2).

V2 Call Control (appended to buddy pointer, or prepended to inline data)::

    [1B ns_len][namespace UTF-8][2B method_idx LE]

    Total: 3 + ns_len bytes.  ns_len=0 → use connection default namespace.

V2 Reply Control::

    [1B status]
    if status == STATUS_SUCCESS (0x00):
        (data follows — inline or in buddy SHM)
    if status == STATUS_ERROR (0x01):
        [4B error_len LE][error_bytes]

V2 frame identification (per frame header flags):
- ``FLAG_CALL_V2`` (bit 7): request frame carries v2 call control.
- ``FLAG_REPLY_V2`` (bit 8): response frame carries v2 reply control.
"""
from __future__ import annotations

import struct

from ..rpc.ipc.ipc_protocol import (
    FRAME_STRUCT,
    FLAG_RESPONSE,
    encode_frame,
)
from ..rpc.ipc.ipc_v3_protocol import (
    FLAG_BUDDY,
    BUDDY_PAYLOAD_STRUCT,
    encode_buddy_payload,
)
from .protocol import (
    FLAG_CALL_V2,
    FLAG_REPLY_V2,
    STATUS_SUCCESS,
    STATUS_ERROR,
)

# ---------------------------------------------------------------------------
# Struct helpers
# ---------------------------------------------------------------------------

_U16 = struct.Struct('<H')
_U32 = struct.Struct('<I')

# ---------------------------------------------------------------------------
# V2 Call Control
# ---------------------------------------------------------------------------


def encode_call_control(namespace: str, method_idx: int) -> bytes:
    """Encode v2 call control payload.

    Format: ``[1B ns_len][namespace UTF-8][2B method_idx LE]``
    """
    if namespace:
        ns_b = namespace.encode('utf-8')
        buf = bytearray(1 + len(ns_b) + 2)
        buf[0] = len(ns_b)
        buf[1:1 + len(ns_b)] = ns_b
        _U16.pack_into(buf, 1 + len(ns_b), method_idx)
    else:
        buf = bytearray(3)
        buf[0] = 0
        _U16.pack_into(buf, 1, method_idx)
    return bytes(buf)


def decode_call_control(
    data: bytes | memoryview, offset: int = 0,
) -> tuple[str, int, int]:
    """Decode v2 call control payload.

    Returns ``(namespace, method_idx, bytes_consumed)``.
    """
    ns_len = data[offset]
    off = offset + 1
    if ns_len > 0:
        namespace = bytes(data[off:off + ns_len]).decode('utf-8')
        off += ns_len
    else:
        namespace = ''
    method_idx = _U16.unpack_from(data, off)[0]
    off += 2
    return namespace, method_idx, off - offset


# ---------------------------------------------------------------------------
# V2 Reply Control
# ---------------------------------------------------------------------------


def encode_reply_control(status: int, error_data: bytes | None = None) -> bytes:
    """Encode v2 reply control payload.

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
    """Decode v2 reply control payload.

    Returns ``(status, error_data_or_none, bytes_consumed)``.
    """
    status = data[offset]
    off = offset + 1
    if status == STATUS_ERROR:
        err_len = _U32.unpack_from(data, off)[0]; off += 4
        error_data = bytes(data[off:off + err_len]); off += err_len
        return status, error_data, off - offset
    return status, None, off - offset


# ---------------------------------------------------------------------------
# V2 Frame Builders — Call
# ---------------------------------------------------------------------------


def encode_v2_buddy_call_frame(
    request_id: int,
    seg_idx: int,
    offset: int,
    data_size: int,
    is_dedicated: bool,
    namespace: str,
    method_idx: int,
) -> bytes:
    """Build a complete v2 buddy call frame.

    Frame layout::

        [16B header (flags=FLAG_BUDDY|FLAG_CALL_V2)]
        [11B buddy_payload]
        [v2_call_control]
    """
    buddy = encode_buddy_payload(seg_idx, offset, data_size, is_dedicated)
    ctrl = encode_call_control(namespace, method_idx)
    payload = buddy + ctrl
    flags = FLAG_BUDDY | FLAG_CALL_V2
    return encode_frame(request_id, flags, payload)


def encode_v2_inline_call_frame(
    request_id: int,
    namespace: str,
    method_idx: int,
    data: bytes | memoryview,
) -> bytes:
    """Build a complete v2 inline call frame (small payloads, no buddy).

    Frame layout::

        [16B header (flags=FLAG_CALL_V2)]
        [v2_call_control]
        [inline_data]
    """
    ctrl = encode_call_control(namespace, method_idx)
    payload = ctrl + bytes(data) if data else ctrl
    return encode_frame(request_id, FLAG_CALL_V2, payload)


# ---------------------------------------------------------------------------
# V2 Frame Builders — Reply
# ---------------------------------------------------------------------------


def encode_v2_buddy_reply_frame(
    request_id: int,
    seg_idx: int,
    offset: int,
    data_size: int,
    is_dedicated: bool,
) -> bytes:
    """Build a v2 buddy success reply frame.

    Frame layout::

        [16B header (flags=FLAG_RESPONSE|FLAG_BUDDY|FLAG_REPLY_V2)]
        [11B buddy_payload]
        [1B status=SUCCESS]
    """
    buddy = encode_buddy_payload(seg_idx, offset, data_size, is_dedicated)
    ctrl = encode_reply_control(STATUS_SUCCESS)
    payload = buddy + ctrl
    flags = FLAG_RESPONSE | FLAG_BUDDY | FLAG_REPLY_V2
    return encode_frame(request_id, flags, payload)


def encode_v2_inline_reply_frame(
    request_id: int,
    result_data: bytes | memoryview | None = None,
) -> bytes:
    """Build a v2 inline success reply frame (small result).

    Frame layout::

        [16B header (flags=FLAG_RESPONSE|FLAG_REPLY_V2)]
        [1B status=SUCCESS]
        [inline_result_data]
    """
    ctrl = encode_reply_control(STATUS_SUCCESS)
    if result_data:
        payload = ctrl + bytes(result_data)
    else:
        payload = ctrl
    flags = FLAG_RESPONSE | FLAG_REPLY_V2
    return encode_frame(request_id, flags, payload)


def encode_v2_error_reply_frame(
    request_id: int,
    error_data: bytes,
) -> bytes:
    """Build a v2 error reply frame (always inline, no buddy SHM).

    Frame layout::

        [16B header (flags=FLAG_RESPONSE|FLAG_REPLY_V2)]
        [1B status=ERROR][4B error_len LE][error_bytes]
    """
    ctrl = encode_reply_control(STATUS_ERROR, error_data)
    flags = FLAG_RESPONSE | FLAG_REPLY_V2
    return encode_frame(request_id, flags, ctrl)


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
