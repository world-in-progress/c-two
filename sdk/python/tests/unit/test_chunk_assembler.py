"""Unit tests for RustChunkAssembler.

All chunk assembly is now handled by the Rust-backed ChunkAssembler with MemPool.
"""

from __future__ import annotations

import ctypes
import random

import pytest


# ---------------------------------------------------------------------------
# Import assembler from Rust-backed module
# ---------------------------------------------------------------------------

from c_two.mem import ChunkAssembler as RustChunkAssembler, MemPool, PoolConfig


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _make_chunk_data(chunk_idx: int, size: int) -> bytes:
    """Create deterministic test data for a given chunk index."""
    pattern = bytes([chunk_idx & 0xFF]) * size
    return pattern


def _make_payload(n_chunks: int, chunk_size: int, last_chunk_size: int | None = None) -> tuple[bytes, list[bytes]]:
    """Create a full payload and its chunk decomposition.

    Returns (full_payload, list_of_chunks).
    """
    chunks = []
    for i in range(n_chunks):
        if i == n_chunks - 1 and last_chunk_size is not None:
            sz = last_chunk_size
        else:
            sz = chunk_size
        chunks.append(_make_chunk_data(i, sz))
    full = b''.join(chunks)
    return full, chunks


@pytest.fixture
def pool():
    """Create a MemPool for RustChunkAssembler tests."""
    p = MemPool(PoolConfig(
        segment_size=4 * 1024 * 1024,
        min_block_size=4096,
        max_segments=2,
    ))
    yield p
    p.destroy()


def _finish_to_bytes(asm: RustChunkAssembler) -> bytes:
    """Finish a RustChunkAssembler and extract reassembled bytes."""
    mem_handle = asm.finish()
    try:
        addr, length = mem_handle.buffer_info()
        mv = memoryview((ctypes.c_char * length).from_address(addr)).cast('B')
        return bytes(mv)
    finally:
        mem_handle.release()


# ===========================================================================
# Tests for RustChunkAssembler
# ===========================================================================

class TestChunkAssembler:
    """Tests for the Rust-backed ChunkAssembler."""

    def test_sequential_add(self, pool):
        """Add chunks in order 0, 1, 2 — should complete on last."""
        chunk_size = 1024
        n_chunks = 3
        last_size = 512
        full, chunks = _make_payload(n_chunks, chunk_size, last_size)

        asm = RustChunkAssembler(pool, n_chunks, chunk_size)

        assert not asm.feed_chunk(0, chunks[0])
        assert not asm.feed_chunk(1, chunks[1])
        assert asm.feed_chunk(2, chunks[2])  # last chunk → complete

        result = _finish_to_bytes(asm)
        assert result == full

    def test_random_order(self, pool):
        """Add chunks in random order — should still reassemble correctly."""
        chunk_size = 2048
        n_chunks = 5
        last_size = 1000
        full, chunks = _make_payload(n_chunks, chunk_size, last_size)

        asm = RustChunkAssembler(pool, n_chunks, chunk_size)

        indices = list(range(n_chunks))
        random.seed(42)
        random.shuffle(indices)

        for idx in indices:
            asm.feed_chunk(idx, chunks[idx])

        result = _finish_to_bytes(asm)
        assert result == full

    def test_duplicate_chunk_raises(self, pool):
        """Duplicate chunk raises RuntimeError."""
        asm = RustChunkAssembler(pool, 2, 512)
        data0 = b'\x00' * 512
        data1 = b'\x01' * 512

        assert not asm.feed_chunk(0, data0)
        with pytest.raises(RuntimeError, match='duplicate'):
            asm.feed_chunk(0, data0)
        assert asm.received == 1
        assert asm.feed_chunk(1, data1)

    def test_single_chunk(self, pool):
        """Single-chunk transfer (degenerate case)."""
        chunk_size = 4096
        data = b'\xAB' * 1500

        asm = RustChunkAssembler(pool, 1, chunk_size)

        assert asm.feed_chunk(0, data)
        result = _finish_to_bytes(asm)
        assert result == data

    def test_assemble_returns_exact_bytes(self, pool):
        """Assembled result has exact total size (not padded to chunk boundary)."""
        chunk_size = 1024
        n_chunks = 3
        last_size = 100  # much smaller than chunk_size
        full, chunks = _make_payload(n_chunks, chunk_size, last_size)

        asm = RustChunkAssembler(pool, n_chunks, chunk_size)
        for i, c in enumerate(chunks):
            asm.feed_chunk(i, c)

        result = _finish_to_bytes(asm)
        assert len(result) == len(full)
        assert result == full

    def test_abort_releases_resources(self, pool):
        """abort() releases resources; subsequent abort is idempotent."""
        asm = RustChunkAssembler(pool, 2, 1024)
        asm.feed_chunk(0, b'\x00' * 512)
        asm.abort()
        # Idempotent
        asm.abort()

    def test_finish_releases_handle(self, pool):
        """finish() returns a MemHandle that can be released."""
        asm = RustChunkAssembler(pool, 1, 1024)
        asm.feed_chunk(0, b'\x00' * 500)
        result = _finish_to_bytes(asm)
        assert len(result) == 500

    def test_memoryview_input(self, pool):
        """feed_chunk() accepts bytes (memoryview must be converted)."""
        chunk_size = 256
        data = b'\x01\x02\x03' * 80  # 240 bytes

        asm = RustChunkAssembler(pool, 1, chunk_size)
        assert asm.feed_chunk(0, bytes(memoryview(data)))
        assert _finish_to_bytes(asm) == data

    def test_large_chunk_count(self, pool):
        """Stress test with many small chunks."""
        n_chunks = 100
        chunk_size = 64
        last_size = 30
        full, chunks = _make_payload(n_chunks, chunk_size, last_size)

        asm = RustChunkAssembler(pool, n_chunks, chunk_size)

        for i in range(n_chunks):
            asm.feed_chunk(i, chunks[i])

        assert _finish_to_bytes(asm) == full


# ===========================================================================
# Edge case & lifecycle tests
# ===========================================================================

class TestAssemblerEdgeCases:
    """Edge cases for the Rust-backed ChunkAssembler."""

    def test_zero_length_chunk(self, pool):
        """Zero-length chunk data in assembler."""
        asm = RustChunkAssembler(pool, 1, 1024)
        assert asm.feed_chunk(0, b'')
        assert _finish_to_bytes(asm) == b''

    def test_very_small_chunk_size(self, pool):
        """chunk_size=1 with many chunks."""
        data = b'ABCDE'
        asm = RustChunkAssembler(pool, 5, 1)
        for i in range(5):
            asm.feed_chunk(i, data[i:i + 1])
        assert _finish_to_bytes(asm) == data

    def test_abort_after_partial(self, pool):
        """Abort assembler after partial receipt."""
        asm = RustChunkAssembler(pool, 4, 512)
        asm.feed_chunk(0, b'\x00' * 200)
        asm.abort()

    def test_reverse_order(self, pool):
        """Chunks arriving in exact reverse order."""
        chunk_size = 256
        n_chunks = 5
        last_size = 100
        full, chunks = _make_payload(n_chunks, chunk_size, last_size)

        asm = RustChunkAssembler(pool, n_chunks, chunk_size)
        for i in reversed(range(n_chunks)):
            asm.feed_chunk(i, chunks[i])
        assert _finish_to_bytes(asm) == full

    def test_last_chunk_much_smaller(self, pool):
        """Last chunk is 1 byte — total assembled payload is exact."""
        chunk_size = 4096
        asm = RustChunkAssembler(pool, 3, chunk_size)
        asm.feed_chunk(0, b'\xAA' * chunk_size)
        asm.feed_chunk(1, b'\xBB' * chunk_size)
        asm.feed_chunk(2, b'\xCC')  # 1 byte
        result = _finish_to_bytes(asm)
        assert len(result) == chunk_size * 2 + 1
        assert result[-1:] == b'\xCC'

    def test_concurrent_assemblers_independent(self, pool):
        """Two independent assemblers don't interfere."""
        asm1 = RustChunkAssembler(pool, 2, 512)
        asm2 = RustChunkAssembler(pool, 2, 512)

        asm1.feed_chunk(0, b'\x01' * 512)
        asm2.feed_chunk(0, b'\x02' * 512)
        asm1.feed_chunk(1, b'\x03' * 100)
        asm2.feed_chunk(1, b'\x04' * 200)

        r1 = _finish_to_bytes(asm1)
        r2 = _finish_to_bytes(asm2)
        assert len(r1) == 612
        assert len(r2) == 712
        assert r1[:512] == b'\x01' * 512
        assert r2[:512] == b'\x02' * 512
