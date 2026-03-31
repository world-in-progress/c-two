"""Deprecated — use ``c_two.mem`` instead.

This module re-exports from ``c_two.mem`` for backward compatibility.
``BuddyPoolHandle`` is an alias for ``MemPool``.
"""
from __future__ import annotations

import warnings as _warnings

_warnings.warn(
    "c_two.buddy is deprecated — use c_two.mem instead",
    DeprecationWarning,
    stacklevel=2,
)

from c_two.mem import (  # noqa: E402
    BuddyPoolHandle,
    MemPool,
    PoolAlloc,
    PoolConfig,
    PoolStats,
    cleanup_stale_shm,
)

__all__ = [
    "BuddyPoolHandle",
    "MemPool",
    "PoolAlloc",
    "PoolConfig",
    "PoolStats",
    "cleanup_stale_shm",
]
