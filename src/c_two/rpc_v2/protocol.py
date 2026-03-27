"""Protocol v3.1 extensions for rpc_v2.

Extends the IPC v3 buddy protocol with:
- ``FLAG_CALL_V2`` / ``FLAG_REPLY_V2`` frame flags for control-plane routing
- Handshake v5 with capability negotiation and method index exchange
- Reply status codes (success / error, no per-byte overhead in SHM)

Frame flag bit allocation (32-bit LE flags field in the 16-byte frame header):

    Bit 0: FLAG_SHM           — per-request SHM (ipc v2 legacy)
    Bit 1: FLAG_RESPONSE       — server→client direction
    Bit 2: FLAG_HANDSHAKE      — handshake message
    Bit 3: FLAG_POOL           — pool SHM (ipc v2 legacy)
    Bit 4: FLAG_CTRL           — control message
    Bit 5: FLAG_DISK_SPILL     — reserved
    Bit 6: FLAG_BUDDY          — buddy-allocated SHM block
    Bit 7: FLAG_CALL_V2        — v2 call frame (control-plane routing)
    Bit 8: FLAG_REPLY_V2       — v2 reply frame (control-plane status)
"""
from __future__ import annotations

import struct
from dataclasses import dataclass, field

# ---------------------------------------------------------------------------
# Flag constants
# ---------------------------------------------------------------------------

FLAG_CALL_V2  = 1 << 7   # 0x80  — v2 call frame with control-plane routing
FLAG_REPLY_V2 = 1 << 8   # 0x100 — v2 reply frame with control-plane status

# ---------------------------------------------------------------------------
# Handshake v5
# ---------------------------------------------------------------------------

HANDSHAKE_V5 = 5

# Capability flags (2 bytes, exchanged in handshake v5)
CAP_CALL_V2    = 1 << 0  # Supports v2 call/reply frames
CAP_METHOD_IDX = 1 << 1  # Supports method indexing (2-byte index vs UTF-8 name)

# ---------------------------------------------------------------------------
# Reply status codes (1 byte, in v2 reply control payload)
# ---------------------------------------------------------------------------

STATUS_SUCCESS = 0x00     # Result data follows (inline or in buddy SHM)
STATUS_ERROR   = 0x01     # Error data follows inline

# ---------------------------------------------------------------------------
# Struct helpers
# ---------------------------------------------------------------------------

_U8  = struct.Struct('<B')
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
    """Routing name + method table, exchanged during handshake v5.

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
class HandshakeV5:
    """Parsed handshake v5 payload."""
    segments: list[tuple[str, int]]   # [(shm_name, segment_size), ...]
    capability_flags: int = 0
    routes: list[RouteInfo] = field(default_factory=list)


# ---------------------------------------------------------------------------
# Handshake v5 — Client → Server
# ---------------------------------------------------------------------------

def encode_v5_client_handshake(
    segments: list[tuple[str, int]],
    capability_flags: int = CAP_CALL_V2 | CAP_METHOD_IDX,
) -> bytes:
    """Encode client→server handshake v5.

    Format::

        [1B version=5]
        [2B seg_count LE]
        [per-segment: [4B size LE][1B name_len][name UTF-8]]
        [2B capability_flags LE]
    """
    parts: list[bytes] = [bytes([HANDSHAKE_V5])]
    parts.append(_U16.pack(len(segments)))
    for name, size in segments:
        name_b = name.encode('utf-8')
        parts.append(_U32.pack(size))
        parts.append(bytes([len(name_b)]))
        parts.append(name_b)
    parts.append(_U16.pack(capability_flags))
    return b''.join(parts)


# ---------------------------------------------------------------------------
# Handshake v5 — Server → Client (ACK)
# ---------------------------------------------------------------------------

def encode_v5_server_handshake(
    segments: list[tuple[str, int]],
    capability_flags: int,
    routes: list[RouteInfo],
) -> bytes:
    """Encode server→client handshake v5 ACK.

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
    parts: list[bytes] = [bytes([HANDSHAKE_V5])]
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
# Handshake v5 — Decode (both directions)
# ---------------------------------------------------------------------------

def decode_v5_handshake(payload: bytes | memoryview) -> HandshakeV5:
    """Decode handshake v5 from either direction.

    Client payloads have no route section (detected by exhausting bytes
    after capability_flags).
    """
    buf = memoryview(payload) if not isinstance(payload, memoryview) else payload
    if len(buf) < 3:
        raise ValueError('Handshake v5 payload too short')

    version = buf[0]
    if version != HANDSHAKE_V5:
        raise ValueError(f'Expected handshake v5 (version={HANDSHAKE_V5}), got {version}')

    off = 1
    seg_count = _U16.unpack_from(buf, off)[0]; off += 2
    segments: list[tuple[str, int]] = []
    for _ in range(seg_count):
        size = _U32.unpack_from(buf, off)[0]; off += 4
        name_len = buf[off]; off += 1
        name = bytes(buf[off:off + name_len]).decode('utf-8'); off += name_len
        segments.append((name, size))

    if off + 2 > len(buf):
        raise ValueError('Handshake v5 missing capability_flags')
    cap_flags = _U16.unpack_from(buf, off)[0]; off += 2

    # Route section (optional — only present in server→client ACK).
    routes: list[RouteInfo] = []
    if off + 2 <= len(buf):
        route_count = _U16.unpack_from(buf, off)[0]; off += 2
        for _ in range(route_count):
            r_len = buf[off]; off += 1
            r_name = bytes(buf[off:off + r_len]).decode('utf-8'); off += r_len
            m_count = _U16.unpack_from(buf, off)[0]; off += 2
            methods: list[MethodEntry] = []
            for mi in range(m_count):
                m_len = buf[off]; off += 1
                m_name = bytes(buf[off:off + m_len]).decode('utf-8'); off += m_len
                m_idx = _U16.unpack_from(buf, off)[0]; off += 2
                methods.append(MethodEntry(name=m_name, index=m_idx))
            routes.append(RouteInfo(name=r_name, methods=methods))

    return HandshakeV5(
        segments=segments,
        capability_flags=cap_flags,
        routes=routes,
    )
