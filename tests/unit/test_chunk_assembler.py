"""Unit tests for _ChunkAssembler (server) and _ReplyChunkAssembler (client).

Both classes use anonymous ``mmap.mmap(-1, size)`` for reassembly buffers with
deterministic OS-level release via ``close()``.
"""

from __future__ import annotations

import mmap
import os
import random

import pytest


# ---------------------------------------------------------------------------
# Import private assembler classes from rpc_v2 internals
# ---------------------------------------------------------------------------

from c_two.rpc_v2.server import _ChunkAssembler
from c_two.rpc_v2.client import _ReplyChunkAssembler


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _make_chunk_data(chunk_idx: int, size: int) -> bytes:
    """Create deterministic test data for a given chunk index."""
    # Repeating pattern so we can verify reassembly correctness
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


# ===========================================================================
# Tests for _ChunkAssembler (server-side)
# ===========================================================================

class TestChunkAssembler:
    """Tests for the server-side _ChunkAssembler."""

    def test_sequential_add(self):
        """Add chunks in order 0, 1, 2 — should complete on last."""
        chunk_size = 1024
        n_chunks = 3
        last_size = 512
        full, chunks = _make_payload(n_chunks, chunk_size, last_size)

        asm = _ChunkAssembler(
            total_chunks=n_chunks,
            chunk_size=chunk_size,
            route_name='test',
            method_idx=0,
        )

        assert not asm.add(0, chunks[0])
        assert not asm.add(1, chunks[1])
        assert asm.add(2, chunks[2])  # last chunk → complete

        result = asm.assemble()
        assert result == full

    def test_random_order(self):
        """Add chunks in random order — should still reassemble correctly."""
        chunk_size = 2048
        n_chunks = 5
        last_size = 1000
        full, chunks = _make_payload(n_chunks, chunk_size, last_size)

        asm = _ChunkAssembler(
            total_chunks=n_chunks,
            chunk_size=chunk_size,
            route_name='test',
            method_idx=1,
        )

        indices = list(range(n_chunks))
        random.seed(42)
        random.shuffle(indices)

        for i, idx in enumerate(indices):
            is_complete = asm.add(idx, chunks[idx])
            if i < n_chunks - 1:
                assert not is_complete
            else:
                assert is_complete

        result = asm.assemble()
        assert result == full

    def test_duplicate_chunk_ignored(self):
        """Duplicate chunk idx is silently ignored (returns False)."""
        chunk_size = 512
        n_chunks = 2
        full, chunks = _make_payload(n_chunks, chunk_size)

        asm = _ChunkAssembler(
            total_chunks=n_chunks,
            chunk_size=chunk_size,
            route_name='test',
            method_idx=0,
        )

        assert not asm.add(0, chunks[0])
        # Duplicate of chunk 0
        result = asm.add(0, chunks[0])
        assert result is False
        assert asm.received == 1

        assert asm.add(1, chunks[1])
        assert asm.received == 2

    def test_single_chunk(self):
        """Single-chunk transfer (degenerate case)."""
        chunk_size = 4096
        data = b'\xAB' * 1500

        asm = _ChunkAssembler(
            total_chunks=1,
            chunk_size=chunk_size,
            route_name='single',
            method_idx=0,
        )

        assert asm.add(0, data)
        result = asm.assemble()
        assert result == data

    def test_assemble_returns_exact_bytes(self):
        """Assembled result has exact total size (not padded to chunk boundary)."""
        chunk_size = 1024
        n_chunks = 3
        last_size = 100  # much smaller than chunk_size
        full, chunks = _make_payload(n_chunks, chunk_size, last_size)

        asm = _ChunkAssembler(
            total_chunks=n_chunks,
            chunk_size=chunk_size,
            route_name='test',
            method_idx=0,
        )
        for i, c in enumerate(chunks):
            asm.add(i, c)

        result = asm.assemble()
        assert len(result) == len(full)
        assert result == full

    def test_discard_releases_mmap(self):
        """discard() closes the mmap; subsequent discard is idempotent."""
        asm = _ChunkAssembler(
            total_chunks=2,
            chunk_size=1024,
            route_name='test',
            method_idx=0,
        )
        assert asm._buf is not None
        asm.discard()
        assert asm._buf is None
        # Idempotent
        asm.discard()
        assert asm._buf is None

    def test_assemble_releases_mmap(self):
        """assemble() closes the internal mmap — buf is set to None."""
        asm = _ChunkAssembler(
            total_chunks=1,
            chunk_size=1024,
            route_name='test',
            method_idx=0,
        )
        asm.add(0, b'\x00' * 500)
        result = asm.assemble()
        assert asm._buf is None
        assert len(result) == 500

    def test_memoryview_input(self):
        """add() accepts memoryview as well as bytes."""
        chunk_size = 256
        data = b'\x01\x02\x03' * 80  # 240 bytes
        mv = memoryview(data)

        asm = _ChunkAssembler(
            total_chunks=1,
            chunk_size=chunk_size,
            route_name='test',
            method_idx=0,
        )
        assert asm.add(0, mv)
        assert asm.assemble() == data

    def test_large_chunk_count(self):
        """Stress test with many small chunks."""
        n_chunks = 100
        chunk_size = 64
        last_size = 30
        full, chunks = _make_payload(n_chunks, chunk_size, last_size)

        asm = _ChunkAssembler(
            total_chunks=n_chunks,
            chunk_size=chunk_size,
            route_name='stress',
            method_idx=0,
        )

        for i in range(n_chunks):
            asm.add(i, chunks[i])

        assert asm.assemble() == full

    def test_metadata_preserved(self):
        """route_name, method_idx, created_at are accessible after construction."""
        asm = _ChunkAssembler(
            total_chunks=2,
            chunk_size=1024,
            route_name='my_route',
            method_idx=42,
        )
        assert asm.route_name == 'my_route'
        assert asm.method_idx == 42
        assert asm.created_at > 0
        asm.discard()


# ===========================================================================
# Tests for _ReplyChunkAssembler (client-side)
# ===========================================================================

class TestReplyChunkAssembler:
    """Tests for the client-side _ReplyChunkAssembler."""

    def test_sequential_add(self):
        """Add reply chunks in order."""
        chunk_size = 1024
        n_chunks = 3
        last_size = 512
        full, chunks = _make_payload(n_chunks, chunk_size, last_size)

        asm = _ReplyChunkAssembler(total_chunks=n_chunks, chunk_size=chunk_size)

        assert not asm.add(0, chunks[0])
        assert not asm.add(1, chunks[1])
        assert asm.add(2, chunks[2])

        result = asm.assemble()
        assert result == full

    def test_random_order(self):
        """Add reply chunks in shuffled order."""
        chunk_size = 2048
        n_chunks = 4
        last_size = 800
        full, chunks = _make_payload(n_chunks, chunk_size, last_size)

        asm = _ReplyChunkAssembler(total_chunks=n_chunks, chunk_size=chunk_size)

        indices = list(range(n_chunks))
        random.seed(99)
        random.shuffle(indices)

        for idx in indices:
            asm.add(idx, chunks[idx])

        assert asm.assemble() == full

    def test_duplicate_ignored(self):
        """Duplicate chunk is silently ignored."""
        asm = _ReplyChunkAssembler(total_chunks=2, chunk_size=512)
        data0 = b'\x00' * 512
        data1 = b'\x01' * 512

        assert not asm.add(0, data0)
        assert not asm.add(0, data0)  # duplicate → False, no increment
        assert asm.received == 1
        assert asm.add(1, data1)

    def test_single_chunk(self):
        """Single-chunk reply reassembly."""
        data = b'\xCD' * 200
        asm = _ReplyChunkAssembler(total_chunks=1, chunk_size=4096)
        assert asm.add(0, data)
        assert asm.assemble() == data

    def test_discard(self):
        """discard() releases the mmap."""
        asm = _ReplyChunkAssembler(total_chunks=2, chunk_size=1024)
        assert asm._buf is not None
        asm.discard()
        assert asm._buf is None
        asm.discard()  # idempotent

    def test_assemble_releases_mmap(self):
        """assemble() sets internal buffer to None."""
        asm = _ReplyChunkAssembler(total_chunks=1, chunk_size=1024)
        asm.add(0, b'\xFF' * 100)
        result = asm.assemble()
        assert asm._buf is None
        assert len(result) == 100

    def test_memoryview_input(self):
        """add() accepts memoryview."""
        data = b'\x42' * 300
        asm = _ReplyChunkAssembler(total_chunks=1, chunk_size=512)
        assert asm.add(0, memoryview(data))
        assert asm.assemble() == data

    def test_large_reassembly(self):
        """Reassemble 50 chunks of 1KB each + smaller last chunk."""
        n_chunks = 50
        chunk_size = 1024
        last_size = 700
        full, chunks = _make_payload(n_chunks, chunk_size, last_size)

        asm = _ReplyChunkAssembler(total_chunks=n_chunks, chunk_size=chunk_size)
        for i, c in enumerate(chunks):
            asm.add(i, c)

        assert asm.assemble() == full


# ===========================================================================
# Edge case & lifecycle tests for both assemblers
# ===========================================================================

class TestAssemblerEdgeCases:
    """Edge cases shared between server and client assemblers."""

    def test_server_zero_length_chunk(self):
        """Zero-length chunk data does not crash assembler."""
        asm = _ChunkAssembler(
            total_chunks=2,
            chunk_size=1024,
            route_name='test',
            method_idx=0,
        )
        assert not asm.add(0, b'')  # zero-length first chunk
        assert asm.add(1, b'')     # zero-length second chunk
        result = asm.assemble()
        assert result == b''

    def test_client_zero_length_chunk(self):
        """Zero-length chunk data in reply assembler."""
        asm = _ReplyChunkAssembler(total_chunks=1, chunk_size=1024)
        assert asm.add(0, b'')
        assert asm.assemble() == b''

    def test_server_very_small_chunk_size(self):
        """chunk_size=1 with many chunks."""
        n_chunks = 10
        data = b'0123456789'
        asm = _ChunkAssembler(
            total_chunks=n_chunks,
            chunk_size=1,
            route_name='test',
            method_idx=0,
        )
        for i in range(n_chunks):
            asm.add(i, data[i:i + 1])
        assert asm.assemble() == data

    def test_client_very_small_chunk_size(self):
        """Reply assembler with chunk_size=1."""
        data = b'ABCDE'
        asm = _ReplyChunkAssembler(total_chunks=5, chunk_size=1)
        for i in range(5):
            asm.add(i, data[i:i + 1])
        assert asm.assemble() == data

    def test_server_discard_after_partial(self):
        """Discard after receiving some (but not all) chunks."""
        asm = _ChunkAssembler(
            total_chunks=3,
            chunk_size=1024,
            route_name='test',
            method_idx=0,
        )
        asm.add(0, b'\x00' * 512)
        asm.add(1, b'\x01' * 512)
        # Discard before receiving chunk 2 (simulates GC timeout).
        asm.discard()
        assert asm._buf is None

    def test_client_discard_after_partial(self):
        """Discard reply assembler after partial receipt."""
        asm = _ReplyChunkAssembler(total_chunks=4, chunk_size=512)
        asm.add(0, b'\x00' * 200)
        asm.discard()
        assert asm._buf is None

    def test_server_reverse_order(self):
        """Chunks arriving in exact reverse order."""
        chunk_size = 256
        n_chunks = 5
        last_size = 100
        full, chunks = _make_payload(n_chunks, chunk_size, last_size)

        asm = _ChunkAssembler(
            total_chunks=n_chunks,
            chunk_size=chunk_size,
            route_name='rev',
            method_idx=0,
        )
        for i in reversed(range(n_chunks)):
            asm.add(i, chunks[i])
        assert asm.assemble() == full

    def test_server_last_chunk_much_smaller(self):
        """Last chunk is 1 byte — total assembled payload is exact."""
        chunk_size = 4096
        asm = _ChunkAssembler(
            total_chunks=3,
            chunk_size=chunk_size,
            route_name='test',
            method_idx=0,
        )
        asm.add(0, b'\xAA' * chunk_size)
        asm.add(1, b'\xBB' * chunk_size)
        asm.add(2, b'\xCC')  # 1 byte
        result = asm.assemble()
        assert len(result) == chunk_size * 2 + 1
        assert result[-1:] == b'\xCC'

    def test_client_concurrent_assemblers_independent(self):
        """Two independent reply assemblers don't interfere."""
        asm1 = _ReplyChunkAssembler(total_chunks=2, chunk_size=512)
        asm2 = _ReplyChunkAssembler(total_chunks=2, chunk_size=512)

        asm1.add(0, b'\x01' * 512)
        asm2.add(0, b'\x02' * 512)
        asm1.add(1, b'\x03' * 100)
        asm2.add(1, b'\x04' * 200)

        r1 = asm1.assemble()
        r2 = asm2.assemble()
        assert len(r1) == 612
        assert len(r2) == 712
        assert r1[:512] == b'\x01' * 512
        assert r2[:512] == b'\x02' * 512
