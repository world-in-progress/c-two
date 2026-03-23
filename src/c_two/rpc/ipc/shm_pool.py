"""Pre-allocated SharedMemory management for IPC v2 transport.

Eliminates per-RPC shm_open/ftruncate/mmap/munmap/shm_unlink syscalls
by pre-allocating a single SHM segment during connection handshake and
reusing it for the lifetime of the connection.

Each connection gets **one** shared SHM segment (unified bidirectional):
- Client creates and owns (unlinks on disconnect)
- Server opens the same segment with track=False
- Synchronous RPC guarantees requests and responses never coexist

If payload exceeds the segment size, falls back to per-request SHM.
"""

from __future__ import annotations

import hashlib
import logging
import struct
from multiprocessing import shared_memory

logger = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# Handshake wire format
# ---------------------------------------------------------------------------
# [1B version][4B segment_size LE][shm_name UTF-8]

_HANDSHAKE_VERSION = 1
_HANDSHAKE_HEADER_SIZE = 5  # 1B version + 4B segment_size


def encode_handshake(shm_name: str, segment_size: int) -> bytes:
    """Encode a pool handshake payload."""
    name_bytes = shm_name.encode('utf-8')
    buf = bytearray(_HANDSHAKE_HEADER_SIZE + len(name_bytes))
    buf[0] = _HANDSHAKE_VERSION
    struct.pack_into('<I', buf, 1, segment_size)
    buf[_HANDSHAKE_HEADER_SIZE:] = name_bytes
    return bytes(buf)


def decode_handshake(payload: bytes | memoryview) -> tuple[str, int]:
    """Decode a pool handshake payload.

    Returns (shm_name, segment_size).
    """
    if len(payload) < _HANDSHAKE_HEADER_SIZE:
        raise ValueError(f'Handshake payload too short: {len(payload)}')
    version = payload[0] if isinstance(payload, (bytes, bytearray)) else int(payload[0])
    if version != _HANDSHAKE_VERSION:
        raise ValueError(f'Unsupported handshake version: {version}')
    segment_size = struct.unpack_from('<I', payload, 1)[0]
    shm_name = bytes(payload[_HANDSHAKE_HEADER_SIZE:]).decode('utf-8')
    return shm_name, segment_size


# ---------------------------------------------------------------------------
# SHM naming
# ---------------------------------------------------------------------------

def pool_shm_name(region_id: str, conn_id: int, direction: str) -> str:
    """Generate a deterministic SHM name for a per-connection pool segment.

    Format: ``ccp{d}_{12_hex}`` where d is the direction initial.
    macOS POSIX SHM names are limited to 31 chars; this produces 16 chars.
    """
    raw = f'{region_id}_c{conn_id}_{direction}'.encode()
    h = hashlib.md5(raw).hexdigest()[:12]
    return f'ccp{direction[0]}_{h}'


# ---------------------------------------------------------------------------
# SHM lifecycle helpers
# ---------------------------------------------------------------------------

def cleanup_stale_shm(name: str) -> None:
    """Remove a stale SHM segment if it exists (e.g. from a previous crash)."""
    try:
        stale = shared_memory.SharedMemory(name=name, create=False, track=False)
        stale.close()
        stale.unlink()
        logger.debug('Cleaned up stale pool SHM segment: %s', name)
    except FileNotFoundError:
        pass


def create_pool_shm(name: str, size: int) -> shared_memory.SharedMemory:
    """Create a new pool SHM segment, cleaning up any stale segment first."""
    cleanup_stale_shm(name)
    return shared_memory.SharedMemory(name=name, create=True, size=size)


def close_pool_shm(shm: shared_memory.SharedMemory | None, *, unlink: bool = False) -> None:
    """Close (and optionally unlink) a pool SHM segment safely."""
    if shm is None:
        return
    try:
        shm.close()
    except Exception:
        pass
    if unlink:
        try:
            shm.unlink()
        except Exception:
            pass
