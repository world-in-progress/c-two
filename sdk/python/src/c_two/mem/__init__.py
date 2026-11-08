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
    del pool

This module also carries the typed shape of :func:`c_two.observe.memory_stats`.
Those counters describe C-Two-owned payload buffers, IPC backing and live
reassembly accounting. They are independent scopes, not process RSS, and must never be
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
    an in-flight call, or outstanding backing charge guards — still keeps its
    accounting alive.
    """

    role: str
    state: str
    limits: MemoryLimits
    cells: dict[str, MemoryCellStats]


class RetiredScopeStats(MemoryScopeStats):
    """A retired scope report.

    A retired scope stays listed while a real owner keeps its backing
    accounting alive. Local-copy metadata contributes to ``holds`` separately
    and never retains an otherwise ownerless backing scope.
    """


class MemoryStats(TypedDict):
    """Result shape of :func:`c_two.observe.memory_stats`."""

    runtime_outgoing: MemoryScopeStats | None
    server: MemoryScopeStats | None
    retired: list[RetiredScopeStats]
    payload_buffer: MemoryCellStats | None
    retired_payload_buffers: list[MemoryCellStats]
    holds: dict


__all__ = [
    "MemPool",
    "PoolAlloc",
    "PoolConfig",
    "PoolStats",
    "cleanup_stale_shm",
    "MemoryLimits",
    "MemoryCellStats",
    "MemoryScopeStats",
    "RetiredScopeStats",
    "MemoryStats",
]
