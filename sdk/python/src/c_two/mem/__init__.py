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

This module also carries the typed shape of :func:`c_two.memory_stats`.
Those counters describe C-Two-owned IPC backing and live reassembly
accounting. They are independent scopes, not process RSS, and must never be
summed and presented as physical memory usage.
"""
from __future__ import annotations

from typing import TypedDict

from c_two._native import (
    MemPool,
    PoolAlloc,
    PoolConfig,
    PoolStats,
    cleanup_stale_shm,
    MemHandle,
    ChunkAssembler,
)


class MemoryLimits(TypedDict):
    """Resolved finite limits of one memory scope, in bytes."""

    shm_backing_bytes: int
    file_backing_bytes: int
    live_reassembly_bytes: int


class MemoryCellStats(TypedDict):
    """One budget cell: limit, current usage, peak, and rejections."""

    limit_bytes: int
    used_bytes: int
    peak_bytes: int
    rejected_allocations: int
    rejected_bytes: int


class MemoryScopeStats(TypedDict):
    """One scope-labelled budget snapshot.

    ``role`` is ``"runtime_outgoing"`` for a Runtime's outgoing client domain
    or ``"server"`` for the server direction. ``state`` is ``"active"`` for
    the current session's scope or ``"retired"`` for a domain kept observable
    after ``cc.shutdown()`` while a real owner — an old proxy's native client,
    an in-flight response, or outstanding hold/charge guards — still keeps its
    accounting alive.
    """

    role: str
    state: str
    limits: MemoryLimits
    cells: dict[str, MemoryCellStats]


class RetiredScopeStats(MemoryScopeStats):
    """A retired scope report.

    A retired record stays listed for exactly as long as a real owner keeps
    its budget accounting or lease metadata alive; it detaches once the last
    owner is gone, so zero counters alone never end an observation and
    repeated empty session swaps never accumulate records.
    """


class MemoryStats(TypedDict):
    """Result shape of :func:`c_two.memory_stats`."""

    runtime_outgoing: MemoryScopeStats | None
    server: MemoryScopeStats | None
    retired: list[RetiredScopeStats]
    holds: dict
    budget_cells_note: str


__all__ = [
    "MemPool",
    "PoolAlloc",
    "PoolConfig",
    "PoolStats",
    "cleanup_stale_shm",
    "MemHandle",
    "ChunkAssembler",
    "MemoryLimits",
    "MemoryCellStats",
    "MemoryScopeStats",
    "RetiredScopeStats",
    "MemoryStats",
]
