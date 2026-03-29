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
    Bit 5: FLAG_DISK_SPILL     — reserved
    Bit 6: FLAG_BUDDY          — buddy-allocated SHM block
    Bit 7: FLAG_CALL           — call frame (control-plane routing)
    Bit 8: FLAG_REPLY          — reply frame (control-plane status)
    Bit 9: FLAG_CHUNKED        — frame is part of a chunked sequence
    Bit 10: FLAG_CHUNK_LAST    — last frame in the chunked sequence
    Bit 11: FLAG_SIGNAL        — frame carries a 1-byte signal (PING, SHUTDOWN, etc.)
"""
from __future__ import annotations

import struct
from dataclasses import dataclass, field

# ---------------------------------------------------------------------------
# Flag constants
# ---------------------------------------------------------------------------

FLAG_CALL  = 1 << 7   # 0x80  — call frame with control-plane routing
FLAG_REPLY = 1 << 8   # 0x100 — reply frame with control-plane status

# Chunked transfer flags
FLAG_CHUNKED    = 1 << 9   # 0x200 — frame is part of a chunked sequence
FLAG_CHUNK_LAST = 1 << 10  # 0x400 — last frame in the chunked sequence
FLAG_SIGNAL     = 1 << 11  # 0x800 — frame carries a 1-byte signal (PING, SHUTDOWN, etc.)

# ---------------------------------------------------------------------------
# Handshake
# ---------------------------------------------------------------------------

HANDSHAKE_VERSION = 5

# Capability flags (2 bytes, exchanged during handshake)
CAP_CALL    = 1 << 0  # Supports call/reply frames
CAP_METHOD_IDX = 1 << 1  # Supports method indexing (2-byte index vs UTF-8 name)
CAP_CHUNKED    = 1 << 2  # Supports chunked transfer for large payloads

# ---------------------------------------------------------------------------
# Reply status codes (1 byte, in reply control payload)
# ---------------------------------------------------------------------------

STATUS_SUCCESS = 0x00     # Result data follows (inline or in buddy SHM)
STATUS_ERROR   = 0x01     # Error data follows inline

# ---------------------------------------------------------------------------
# Handshake safety limits (prevent malicious payloads exhausting resources)
# ---------------------------------------------------------------------------

_MAX_HANDSHAKE_SEGMENTS = 16
_MAX_HANDSHAKE_ROUTES   = 64
_MAX_HANDSHAKE_METHODS  = 256

# ---------------------------------------------------------------------------
# Struct helpers
# ---------------------------------------------------------------------------

_U16 = struct.Struct('<H')
_U32 = struct.Struct('<I')


# ---------------------------------------------------------------------------
# Data classes
# ---------------------------------------------------------------------------

@dataclass
class MethodEntry:
    """A single method in a route's method table."""
    name: str
    index: int


@dataclass
class RouteInfo:
    """Routing name + method table, exchanged during handshake.

    The ``name`` field is the user-chosen CRM routing name (not the ICRM
    namespace from ``__tag__``).
    """
    name: str
    methods: list[MethodEntry] = field(default_factory=list)

    def method_by_name(self, name: str) -> int | None:
        for m in self.methods:
            if m.name == name:
                return m.index
        return None

    def method_by_index(self, idx: int) -> str | None:
        for m in self.methods:
            if m.index == idx:
                return m.name
        return None


@dataclass
class Handshake:
    """Parsed handshake payload."""
    segments: list[tuple[str, int]]   # [(shm_name, segment_size), ...]
    capability_flags: int = 0
    routes: list[RouteInfo] = field(default_factory=list)


# ---------------------------------------------------------------------------
# Handshake — Client → Server
# ---------------------------------------------------------------------------

def encode_client_handshake(
    segments: list[tuple[str, int]],
    capability_flags: int = CAP_CALL | CAP_METHOD_IDX,
) -> bytes:
    """Encode client→server handshake.

    Format::

        [1B version=5]
        [2B seg_count LE]
        [per-segment: [4B size LE][1B name_len][name UTF-8]]
        [2B capability_flags LE]
    """
    parts: list[bytes] = [bytes([HANDSHAKE_VERSION])]
    parts.append(_U16.pack(len(segments)))
    for name, size in segments:
        name_b = name.encode('utf-8')
        parts.append(_U32.pack(size))
        parts.append(bytes([len(name_b)]))
        parts.append(name_b)
    parts.append(_U16.pack(capability_flags))
    return b''.join(parts)


# ---------------------------------------------------------------------------
# Handshake — Server → Client (ACK)
# ---------------------------------------------------------------------------

def encode_server_handshake(
    segments: list[tuple[str, int]],
    capability_flags: int,
    routes: list[RouteInfo],
) -> bytes:
    """Encode server→client handshake ACK.

    Format::

        [1B version=5]
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
    parts: list[bytes] = [bytes([HANDSHAKE_VERSION])]
    # Segments
    parts.append(_U16.pack(len(segments)))
    for name, size in segments:
        name_b = name.encode('utf-8')
        parts.append(_U32.pack(size))
        parts.append(bytes([len(name_b)]))
        parts.append(name_b)
    # Capabilities
    parts.append(_U16.pack(capability_flags))
    # Routes + method tables
    parts.append(_U16.pack(len(routes)))
    for route in routes:
        route_b = route.name.encode('utf-8')
        parts.append(bytes([len(route_b)]))
        parts.append(route_b)
        parts.append(_U16.pack(len(route.methods)))
        for m in route.methods:
            m_b = m.name.encode('utf-8')
            parts.append(bytes([len(m_b)]))
            parts.append(m_b)
            parts.append(_U16.pack(m.index))
    return b''.join(parts)


# ---------------------------------------------------------------------------
# Handshake — Decode (both directions)
# ---------------------------------------------------------------------------

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
    """
    buf = memoryview(payload) if not isinstance(payload, memoryview) else payload
    if len(buf) < 3:
        raise ValueError('Handshake payload too short')

    version = buf[0]
    if version != HANDSHAKE_VERSION:
        raise ValueError(f'Expected handshake (version={HANDSHAKE_VERSION}), got {version}')

    off = 1
    buf_len = len(buf)

    if off + 2 > buf_len:
        raise ValueError('Handshake truncated: missing segment count')
    seg_count = _U16.unpack_from(buf, off)[0]; off += 2
    if seg_count > max_segments:
        raise ValueError(f'Handshake segment count {seg_count} exceeds limit {max_segments}')

    segments: list[tuple[str, int]] = []
    for _ in range(seg_count):
        if off + 5 > buf_len:
            raise ValueError('Handshake truncated in segment entry')
        size = _U32.unpack_from(buf, off)[0]; off += 4
        name_len = buf[off]; off += 1
        if off + name_len > buf_len:
            raise ValueError('Handshake truncated in segment name')
        name = bytes(buf[off:off + name_len]).decode('utf-8'); off += name_len
        segments.append((name, size))

    if off + 2 > buf_len:
        raise ValueError('Handshake missing capability_flags')
    cap_flags = _U16.unpack_from(buf, off)[0]; off += 2

    # Route section (optional — only present in server→client ACK).
    routes: list[RouteInfo] = []
    if off + 2 <= buf_len:
        route_count = _U16.unpack_from(buf, off)[0]; off += 2
        if route_count > max_routes:
            raise ValueError(f'Handshake route count {route_count} exceeds limit {max_routes}')
        for _ in range(route_count):
            if off + 1 > buf_len:
                raise ValueError('Handshake truncated in route entry')
            r_len = buf[off]; off += 1
            if off + r_len > buf_len:
                raise ValueError('Handshake truncated in route name')
            r_name = bytes(buf[off:off + r_len]).decode('utf-8'); off += r_len
            if off + 2 > buf_len:
                raise ValueError('Handshake truncated: missing method count')
            m_count = _U16.unpack_from(buf, off)[0]; off += 2
            if m_count > max_methods:
                raise ValueError(f'Handshake method count {m_count} exceeds limit {max_methods}')
            methods: list[MethodEntry] = []
            for mi in range(m_count):
                if off + 1 > buf_len:
                    raise ValueError('Handshake truncated in method entry')
                m_len = buf[off]; off += 1
                if off + m_len > buf_len:
                    raise ValueError('Handshake truncated in method name')
                m_name = bytes(buf[off:off + m_len]).decode('utf-8'); off += m_len
                if off + 2 > buf_len:
                    raise ValueError('Handshake truncated: missing method index')
                m_idx = _U16.unpack_from(buf, off)[0]; off += 2
                methods.append(MethodEntry(name=m_name, index=m_idx))
            routes.append(RouteInfo(name=r_name, methods=methods))

    return Handshake(
        segments=segments,
        capability_flags=cap_flags,
        routes=routes,
    )
