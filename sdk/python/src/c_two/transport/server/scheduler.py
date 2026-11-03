"""Concurrency configuration and thin native route-concurrency adapter."""
from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Literal

# Re-export access helpers for existing import sites. Scheduling authority lives
# in Rust; these are metadata helpers used while building the method index map.
from ...crm.meta import MethodAccess, get_method_access  # noqa: F401


@dataclass(frozen=True)
class ConcurrencyConfig:
    mode: Literal['parallel', 'exclusive', 'read_parallel'] = 'read_parallel'
    max_workers: int | None = None
    max_pending: int | None = None

    def __post_init__(self) -> None:
        if self.mode not in ('parallel', 'exclusive', 'read_parallel'):
            raise ValueError('mode must be parallel, exclusive, or read_parallel')
        if self.max_workers is not None and self.max_workers < 1:
            raise ValueError('max_workers must be at least 1 when provided.')
        if self.max_pending is not None and self.max_pending < 1:
            raise ValueError('max_pending must be at least 1 when provided.')


class Scheduler:
    """Thin adapter over Rust-owned route concurrency state.

    Python no longer owns locks, pending counters, executors, or drain state.
    This class exists so thread-local direct calls can enter the same native
    guard used by IPC dispatch while preserving the public SDK config types.
    """

    def __init__(self, native: Any, method_index: dict[str, int]) -> None:
        self._native = native
        self._method_index = dict(method_index)

    def method_idx(self, method_name: str) -> int:
        return self._method_index[method_name]

    @property
    def is_unconstrained(self) -> bool:
        return bool(self._native.is_unconstrained)

    def snapshot(self) -> Any:
        return self._native.snapshot()

    def execution_guard(self, method_idx: int):
        return self._native.execution_guard(method_idx)
