"""Compact binary wire codec for the C-Two RPC protocol.

Wire formats
------------

CRM_CALL request::

    [1B msg_type=0x03][2B method_name_len (LE)][method_name][payload ...]
    payload_len = total_size - 3 - method_name_len   (implicit)

CRM_REPLY response::

    [1B msg_type=0x04][4B error_len (LE)][error_bytes][result ...]
    result_len  = total_size - 5 - error_len          (implicit)

Signal messages (PING / PONG / SHUTDOWN_*)::

    [1B msg_type]

All multi-byte integers are **little-endian** to match iceoryx2 SHM
reference headers and the majority of target platforms.
"""
from __future__ import annotations

import struct
from collections.abc import Iterable
from ..event.msg_type import MsgType
from ..event.envelope import Envelope

# Fixed header sizes
CALL_HEADER_FIXED = 3    # 1B type + 2B method_len
REPLY_HEADER_FIXED = 5   # 1B type + 4B error_len
SIGNAL_SIZE = 1          # 1B type only

MAX_METHOD_NAME_LEN = 255

# Pre-encoded signal bytes (1 byte each)
PING_BYTES = bytes([MsgType.PING])
PONG_BYTES = bytes([MsgType.PONG])
SHUTDOWN_CLIENT_BYTES = bytes([MsgType.SHUTDOWN_CLIENT])
SHUTDOWN_SERVER_BYTES = bytes([MsgType.SHUTDOWN_SERVER])
SHUTDOWN_ACK_BYTES = bytes([MsgType.SHUTDOWN_ACK])

_SIGNAL_TYPES = frozenset({
    MsgType.PING,
    MsgType.PONG,
    MsgType.SHUTDOWN_CLIENT,
    MsgType.SHUTDOWN_SERVER,
    MsgType.SHUTDOWN_ACK,
})

_SIGNAL_BYTES_MAP = {
    MsgType.PING: PING_BYTES,
    MsgType.PONG: PONG_BYTES,
    MsgType.SHUTDOWN_CLIENT: SHUTDOWN_CLIENT_BYTES,
    MsgType.SHUTDOWN_SERVER: SHUTDOWN_SERVER_BYTES,
    MsgType.SHUTDOWN_ACK: SHUTDOWN_ACK_BYTES,
}

# ---------------------------------------------------------------------------
# Method-name pre-encoding cache
# ---------------------------------------------------------------------------
# Encode side: method_name str → pre-built call header bytes
#   header = [1B CRM_CALL][2B method_len LE][method_name UTF-8]
# Note: CRM_REPLY has no equivalent cache because its header contains
# error_len which varies per response — there is no fixed string to
# pre-encode (unlike the method name in CRM_CALL).
_call_header_cache: dict[str, bytes] = {}


def preregister_method(method_name: str) -> None:
    """Pre-encode a method name for fast wire encoding.

    Call at ICRM registration time to avoid repeated UTF-8 encoding
    and struct packing on every RPC call.
    """
    if method_name in _call_header_cache:
        return
    method_bytes = method_name.encode('utf-8')
    method_len = len(method_bytes)
    header = bytearray(CALL_HEADER_FIXED + method_len)
    header[0] = MsgType.CRM_CALL
    struct.pack_into('<H', header, 1, method_len)
    header[3:] = method_bytes
    _call_header_cache[method_name] = bytes(header)


def preregister_methods(method_names: Iterable[str]) -> None:
    """Pre-encode multiple method names. Convenience wrapper."""
    for name in method_names:
        preregister_method(name)


# ---------------------------------------------------------------------------
# Size helpers
# ---------------------------------------------------------------------------

def call_wire_size(method_name_len: int, payload_len: int) -> int:
    return CALL_HEADER_FIXED + method_name_len + payload_len


def reply_wire_size(error_len: int, result_len: int) -> int:
    return REPLY_HEADER_FIXED + error_len + result_len


# ---------------------------------------------------------------------------
# Encode — produce contiguous bytes (for stream transports)
# ---------------------------------------------------------------------------

def encode_signal(msg_type: MsgType) -> bytes:
    return _SIGNAL_BYTES_MAP[msg_type]


def encode_call(method_name: str, payload: bytes | memoryview | None = None) -> bytearray:
    """Encode a CRM_CALL into a single contiguous bytearray."""
    cached_header = _call_header_cache.get(method_name)
    if cached_header is not None:
        header_len = len(cached_header)
        payload_len = len(payload) if payload else 0
        buf = bytearray(header_len + payload_len)
        buf[:header_len] = cached_header
        if payload_len > 0:
            buf[header_len:] = payload
        return buf

    method_bytes = method_name.encode('utf-8')
    method_len = len(method_bytes)
    payload_len = len(payload) if payload else 0
    total = CALL_HEADER_FIXED + method_len + payload_len

    buf = bytearray(total)
    buf[0] = MsgType.CRM_CALL
    struct.pack_into('<H', buf, 1, method_len)
    buf[3:3 + method_len] = method_bytes
    if payload_len > 0:
        buf[3 + method_len:] = payload
    return buf


def encode_reply(error_data: bytes | memoryview | None = None,
                 result_data: bytes | memoryview | None = None) -> bytearray:
    """Encode a CRM_REPLY into a single contiguous bytearray."""
    error_len = len(error_data) if error_data else 0
    result_len = len(result_data) if result_data else 0
    total = REPLY_HEADER_FIXED + error_len + result_len

    buf = bytearray(total)
    buf[0] = MsgType.CRM_REPLY
    struct.pack_into('<I', buf, 1, error_len)
    if error_len > 0:
        buf[5:5 + error_len] = error_data
    if result_len > 0:
        buf[5 + error_len:] = result_data
    return buf


# ---------------------------------------------------------------------------
# Write-into — scatter directly into a pre-allocated buffer (SHM / mmap)
# ---------------------------------------------------------------------------

def write_call_into(buf, offset: int,
                    method_name: str,
                    payload: bytes | memoryview | None = None) -> int:
    """Write a CRM_CALL message directly into *buf* starting at *offset*.

    *buf* must support ``struct.pack_into`` and slice assignment
    (``bytearray``, ``mmap``, ``SharedMemory.buf``, etc.).

    Returns the total number of bytes written.
    """
    cached_header = _call_header_cache.get(method_name)
    if cached_header is not None:
        header_len = len(cached_header)
        payload_len = len(payload) if payload else 0
        buf[offset:offset + header_len] = cached_header
        if payload_len > 0:
            buf[offset + header_len:offset + header_len + payload_len] = payload
        return header_len + payload_len

    method_bytes = method_name.encode('utf-8')
    method_len = len(method_bytes)
    payload_len = len(payload) if payload else 0

    buf[offset] = MsgType.CRM_CALL
    struct.pack_into('<H', buf, offset + 1, method_len)
    buf[offset + 3:offset + 3 + method_len] = method_bytes
    if payload_len > 0:
        buf[offset + 3 + method_len:offset + 3 + method_len + payload_len] = payload

    return CALL_HEADER_FIXED + method_len + payload_len


def write_reply_into(buf, offset: int,
                     error_data: bytes | memoryview | None = None,
                     result_data: bytes | memoryview | None = None) -> int:
    """Write a CRM_REPLY message directly into *buf* starting at *offset*.

    Returns the total number of bytes written.
    """
    error_len = len(error_data) if error_data else 0
    result_len = len(result_data) if result_data else 0

    buf[offset] = MsgType.CRM_REPLY
    struct.pack_into('<I', buf, offset + 1, error_len)
    if error_len > 0:
        buf[offset + 5:offset + 5 + error_len] = error_data
    if result_len > 0:
        buf[offset + 5 + error_len:offset + 5 + error_len + result_len] = result_data

    return REPLY_HEADER_FIXED + error_len + result_len


def write_signal_into(buf, offset: int, msg_type: MsgType) -> int:
    """Write a 1-byte signal message into *buf*. Returns 1."""
    buf[offset] = msg_type
    return SIGNAL_SIZE


# ---------------------------------------------------------------------------
# Decode — parse wire bytes into an Envelope (zero-copy via memoryview)
# ---------------------------------------------------------------------------

def decode(buf: bytes | memoryview, total_size: int | None = None) -> Envelope:
    """Decode wire bytes into an :class:`Envelope`.

    When *buf* is a ``memoryview``, the returned ``Envelope.payload``
    and ``Envelope.error`` are memoryview slices — **zero copy**.

    Parameters
    ----------
    buf : bytes | memoryview
        The raw wire message.
    total_size : int, optional
        Length of the valid region in *buf*.  Defaults to ``len(buf)``.
    """
    if not isinstance(buf, memoryview):
        buf = memoryview(buf)

    if total_size is None:
        total_size = len(buf)

    if total_size < 1:
        raise ValueError('Empty wire message')

    msg_type = MsgType(buf[0])

    if msg_type in _SIGNAL_TYPES:
        return Envelope(msg_type=msg_type)

    if msg_type == MsgType.CRM_CALL:
        if total_size < CALL_HEADER_FIXED:
            raise ValueError(f'CRM_CALL too short ({total_size} < {CALL_HEADER_FIXED})')
        method_len = struct.unpack_from('<H', buf, 1)[0]
        if method_len > MAX_METHOD_NAME_LEN:
            raise ValueError(f'method_name too long: {method_len} > {MAX_METHOD_NAME_LEN}')
        header_end = CALL_HEADER_FIXED + method_len
        if total_size < header_end:
            raise ValueError(
                f'CRM_CALL truncated: need {header_end} bytes for header, got {total_size}'
            )
        method_name = bytes(buf[3:header_end]).decode('utf-8')
        payload = buf[header_end:total_size] if header_end < total_size else None
        return Envelope(msg_type=msg_type, method_name=method_name, payload=payload)

    if msg_type == MsgType.CRM_REPLY:
        if total_size < REPLY_HEADER_FIXED:
            raise ValueError(f'CRM_REPLY too short ({total_size} < {REPLY_HEADER_FIXED})')
        error_len = struct.unpack_from('<I', buf, 1)[0]
        error_end = REPLY_HEADER_FIXED + error_len
        if total_size < error_end:
            raise ValueError(
                f'CRM_REPLY truncated: need {error_end} bytes for error, got {total_size}'
            )
        error_data = buf[5:error_end] if error_len > 0 else None
        payload = buf[error_end:total_size] if error_end < total_size else None
        return Envelope(msg_type=msg_type, error=error_data, payload=payload)

    raise ValueError(f'Unknown MsgType: 0x{int(msg_type):02X}')
