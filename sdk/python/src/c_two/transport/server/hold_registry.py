"""Server-level registry tracking hold-mode SHM buffers.

Uses Python weakrefs for automatic cleanup when CRM drops views.
Provides lazy sweep for stale hold detection and stats API.
"""
from __future__ import annotations

import logging
import threading
import time
import weakref

logger = logging.getLogger(__name__)


class HoldEntry:
    """Tracks a single hold-mode SHM buffer."""
    __slots__ = ('ref', 'created_at', 'method_name', 'data_len', 'route_name')

    def __init__(
        self,
        buf_ref: weakref.ref,
        method_name: str,
        data_len: int,
        route_name: str,
    ) -> None:
        self.ref = buf_ref
        self.created_at = time.monotonic()
        self.method_name = method_name
        self.data_len = data_len
        self.route_name = route_name


class HoldRegistry:
    """Server-level registry tracking hold-mode SHM buffers.

    Uses weakrefs for automatic cleanup when CRM drops views.
    Provides lazy sweep for stale hold detection.
    """

    def __init__(self, warn_threshold: float = 60.0) -> None:
        self._entries: dict[int, HoldEntry] = {}
        self._counter: int = 0
        self._warn_threshold = warn_threshold
        self._lock = threading.RLock()

    def track(self, request_buf: object, method_name: str, route_name: str) -> None:
        """Register a hold-mode buffer for tracking.

        Skips inline buffers (no SHM to leak).
        """
        if getattr(request_buf, 'is_inline', False):
            return

        with self._lock:
            self._counter += 1
            entry_id = self._counter

            def _on_gc(_ref: weakref.ref, _eid: int = entry_id) -> None:
                with self._lock:
                    self._entries.pop(_eid, None)

            ref = weakref.ref(request_buf, _on_gc)
            self._entries[entry_id] = HoldEntry(
                buf_ref=ref,
                method_name=method_name,
                data_len=len(request_buf),
                route_name=route_name,
            )

    def sweep(self) -> list[HoldEntry]:
        """Check for stale holds. Returns list of stale entries."""
        now = time.monotonic()
        stale: list[HoldEntry] = []
        dead: list[int] = []
        with self._lock:
            for eid, entry in self._entries.items():
                buf = entry.ref()
                if buf is None:
                    dead.append(eid)
                elif now - entry.created_at > self._warn_threshold:
                    stale.append(entry)
            for eid in dead:
                del self._entries[eid]
        return stale

    def stats(self) -> dict:
        """Return hold tracking statistics."""
        active = 0
        total_bytes = 0
        oldest_age = 0.0
        now = time.monotonic()
        with self._lock:
            for entry in self._entries.values():
                if entry.ref() is not None:
                    active += 1
                    total_bytes += entry.data_len
                    age = now - entry.created_at
                    if age > oldest_age:
                        oldest_age = age
        return {
            'active_holds': active,
            'total_held_bytes': total_bytes,
            'oldest_hold_seconds': round(oldest_age, 2) if oldest_age > 0 else 0,
        }
