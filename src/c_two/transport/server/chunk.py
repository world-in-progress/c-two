"""Chunked-transfer reassembly for large payloads.

Extracted from ``server.core`` — fully self-contained with only stdlib
dependencies.  The ``ChunkAssembler`` uses anonymous ``mmap`` for the
reassembly buffer so the OS reclaims memory deterministically via
``munmap`` on ``close()``.
"""
from __future__ import annotations

import logging
import mmap
import time
from dataclasses import dataclass, field

logger = logging.getLogger(__name__)

# Chunked transfer constants.
CHUNK_THRESHOLD_RATIO = 0.9
CHUNK_ASSEMBLER_TIMEOUT = 60.0   # seconds
CHUNK_GC_INTERVAL = 100          # frames between GC sweeps
MAX_TOTAL_CHUNKS = 512           # 512 × 128 MB = 64 GB theoretical max
MAX_REASSEMBLY_BYTES = 8 * (1 << 30)  # 8 GB hard cap on reassembly buffer


@dataclass
class ChunkAssembler:
    """Reassembles a chunked request into contiguous bytes.

    Uses ``mmap.mmap(-1, size)`` (anonymous mmap) for the reassembly buffer
    to get deterministic OS-level memory release via ``close()``.
    """

    total_chunks: int
    chunk_size: int
    route_name: str
    method_idx: int
    received: int = 0
    _actual_total: int = 0
    created_at: float = field(default_factory=time.monotonic)
    _buf: mmap.mmap | None = field(default=None, init=False, repr=False)
    _received_flags: bytearray = field(default=None, init=False, repr=False)

    def __post_init__(self) -> None:
        if self.total_chunks <= 0 or self.total_chunks > MAX_TOTAL_CHUNKS:
            raise ValueError(
                f'total_chunks={self.total_chunks} out of range [1, {MAX_TOTAL_CHUNKS}]'
            )
        alloc_size = self.total_chunks * self.chunk_size
        if alloc_size > MAX_REASSEMBLY_BYTES:
            raise ValueError(
                f'Reassembly buffer {alloc_size} bytes exceeds '
                f'limit {MAX_REASSEMBLY_BYTES}'
            )
        self._buf = mmap.mmap(-1, alloc_size)
        self._received_flags = bytearray(self.total_chunks)

    def add(self, idx: int, data: bytes | memoryview) -> bool:
        """Write chunk data at the correct offset.  Returns True when complete."""
        if self._received_flags[idx]:
            return False
        offset = idx * self.chunk_size
        dlen = len(data)
        self._buf[offset:offset + dlen] = data
        self._received_flags[idx] = 1
        self._actual_total += dlen
        self.received += 1
        return self.received == self.total_chunks

    def assemble(self) -> bytes:
        """Return reassembled payload and release the mmap.

        .. note::

           ``buf.read()`` creates a ``bytes`` copy while the mmap is still
           alive, so peak RSS briefly doubles (mmap + bytes).  This is
           inherent to any copy-out scheme and acceptable given that the
           mmap is released immediately after via ``close()`` → ``munmap``.
        """
        buf = self._buf
        self._buf = None
        buf.seek(0)
        result = buf.read(self._actual_total)
        buf.close()
        return result

    def discard(self) -> None:
        """Release the mmap without assembling."""
        if self._buf is not None:
            self._buf.close()
            self._buf = None


def gc_chunk_assemblers(
    assemblers: dict[int, ChunkAssembler],
    conn_id: int,
) -> None:
    """Discard chunk assemblers that have been incomplete for too long."""
    now = time.monotonic()
    expired = [
        rid for rid, asm in assemblers.items()
        if now - asm.created_at > CHUNK_ASSEMBLER_TIMEOUT
    ]
    for rid in expired:
        asm = assemblers.pop(rid)
        logger.warning(
            'Conn %d: GC stale chunk assembler rid=%d '
            '(%d/%d chunks received)',
            conn_id, rid, asm.received, asm.total_chunks,
        )
        asm.discard()
