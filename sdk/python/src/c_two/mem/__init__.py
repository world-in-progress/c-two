"""Unified memory pool for cross-process shared memory.

Projects the Rust ``c2-mem`` pool, providing dynamic allocation within
shared memory mappings for
the C-Two IPC transport.

Typical usage::

    from c_two.mem import MemPool, PoolConfig

    pool = MemPool(PoolConfig(
        segment_size=256 * 1024 * 1024,
        min_block_size=4096,
        max_segments=8,
    ))
    alloc = pool.alloc(65536)
    pool.write(alloc, b'hello')
    data = pool.read(alloc, 5)
    pool.free(alloc)
    pool.destroy()
"""
from __future__ import annotations

from c_two._native import (
    MemPool,
    PoolAlloc,
    PoolConfig,
    PoolStats,
    cleanup_stale_shm,
    MemHandle,
    ChunkAssembler,
)

__all__ = [
    "MemPool",
    "PoolAlloc",
    "PoolConfig",
    "PoolStats",
    "cleanup_stale_shm",
    "MemHandle",
    "ChunkAssembler",
]
