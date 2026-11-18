"""Unified memory pool for cross-process shared memory.

Projects the Rust ``c2-mem`` pool, providing dynamic allocation within
shared memory mappings for
the C-Two IPC transport.

Typical usage::

    from c_two.mem import MemPool, PoolConfig

    pool = MemPool(PoolConfig(
        segment_size=256 * 1024 * 1024,
        max_segments=8,
    ))
    alloc = pool.alloc(65536)
    pool.write(alloc, b'hello')
    data = pool.read(alloc, 5)
    pool.free(alloc)
    del pool

This module also carries the typed shape of :func:`c_two.observe.memory_stats`.
These counters describe role-domain budgets and local holds.
"""
from __future__ import annotations

from typing import Literal, TypedDict

from c_two._native import (
    MemPool,
    PoolAlloc,
    PoolConfig,
    PoolStats,
    cleanup_stale_shm,
)


class MemoryCellStats(TypedDict):
    """One budget cell: limit, current usage, peak, and rejections."""

    limit_bytes: int
    used_bytes: int
    peak_bytes: int
    rejected_allocations: int
    rejected_bytes: int


class RoleMemoryStats(TypedDict):
    """Memory accounting for one role. Cells are None until activation."""

    domain_id: int | None
    generation: int
    frozen: bool
    closed: bool
    memory: MemoryCellStats | None
    file_spill: MemoryCellStats | None


class RetiredRoleMemoryStats(RoleMemoryStats):
    role: Literal['client', 'server']


class HoldBucketStats(TypedDict):
    active_holds: int
    total_held_bytes: int


class HoldDirectionStats(TypedDict):
    client_response: HoldBucketStats
    resource_input: HoldBucketStats


class HoldStats(HoldBucketStats):
    oldest_hold_seconds: float | None
    by_direction: HoldDirectionStats


class MemoryStats(TypedDict):
    """Read-only role-domain counters and local holds across sessions."""

    client: RoleMemoryStats
    server: RoleMemoryStats
    retired: list[RetiredRoleMemoryStats]
    holds: HoldStats


__all__ = [
    "MemPool", "PoolAlloc", "PoolConfig", "PoolStats", "cleanup_stale_shm",
    "MemoryCellStats", "RoleMemoryStats", "RetiredRoleMemoryStats",
    "HoldBucketStats", "HoldDirectionStats", "HoldStats", "MemoryStats",
]
