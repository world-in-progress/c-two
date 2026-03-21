"""
Adaptive buffer management for IPC data plane.

Provides a reusable ``bytearray`` that dynamically grows *and* shrinks based
on observed read sizes, inspired by Netty's ``AdaptiveRecvByteBufAllocator``
and high-water-mark time-decay strategies.

Key behaviours:
  - **Grow**: immediately when ``acquire(size)`` exceeds current capacity.
  - **Shrink**: after *N* consecutive reads use < ``shrink_threshold`` of the
    buffer, reallocate to the smallest SIZE_TABLE entry ≥ recent peak.
  - **Decay**: after ``decay_after_seconds`` of inactivity, drop to
    ``min_size`` (or release entirely).
"""

from __future__ import annotations

import bisect
import time
from dataclasses import dataclass, field

# Power-of-2 size table from 1 MB to 4 GB.
SIZE_TABLE: list[int] = [1 << e for e in range(20, 33)]  # 1MB … 4GB


def _ceil_size(size: int) -> int:
    """Return the smallest SIZE_TABLE entry ≥ *size*."""
    idx = bisect.bisect_left(SIZE_TABLE, size)
    if idx < len(SIZE_TABLE):
        return SIZE_TABLE[idx]
    return size  # beyond table — use exact size


@dataclass
class AdaptiveBufferConfig:
    """Tunable knobs for :class:`AdaptiveBuffer`."""

    min_size: int = 1_048_576          # 1 MB — matches SHM threshold
    max_size: int = 4_294_967_296      # 4 GB
    shrink_threshold: float = 0.25     # usage ratio counted as "underused"
    shrink_after_n: int = 3            # consecutive underused reads before shrink
    decay_after_seconds: float = 60.0  # idle time before aggressive release


_DEFAULT_CONFIG = AdaptiveBufferConfig()


class AdaptiveBuffer:
    """A reusable ``bytearray`` that grows and shrinks adaptively.

    Thread-safety: **not** required — each instance is owned by a single
    connection handler (server) or client instance, which serialises access.
    """

    __slots__ = (
        '_buf',
        '_cfg',
        '_consecutive_small',
        '_last_access',
        '_recent_peak',
    )

    def __init__(self, config: AdaptiveBufferConfig | None = None) -> None:
        self._cfg = config or _DEFAULT_CONFIG
        self._buf: bytearray | None = None
        self._consecutive_small: int = 0
        self._last_access: float = time.monotonic()
        self._recent_peak: int = 0

    # ------------------------------------------------------------------
    # Public API
    # ------------------------------------------------------------------

    def acquire(self, size: int) -> bytearray:
        """Ensure the internal buffer has at least *size* bytes and return it.

        The underlying ``bytearray`` may be reallocated (grow or shrink).
        The returned reference is valid until the next ``acquire`` call.
        """
        now = time.monotonic()
        self._last_access = now

        buf = self._buf

        # --- Grow (unavoidable) ---
        if buf is None or len(buf) < size:
            new_cap = max(_ceil_size(size), self._cfg.min_size)
            self._buf = bytearray(new_cap)
            self._consecutive_small = 0
            self._recent_peak = size
            return self._buf

        buf_len = len(buf)

        # --- Shrink detection ---
        if buf_len > self._cfg.min_size and size < buf_len * self._cfg.shrink_threshold:
            if self._consecutive_small == 0:
                # Entering a new "small reads" window — reset peak tracker
                self._recent_peak = size
            else:
                self._recent_peak = max(self._recent_peak, size)
            self._consecutive_small += 1
            if self._consecutive_small >= self._cfg.shrink_after_n:
                target = max(_ceil_size(self._recent_peak), self._cfg.min_size)
                if target < buf_len:
                    self._buf = bytearray(target)
                self._consecutive_small = 0
                self._recent_peak = size
        else:
            self._consecutive_small = 0
            self._recent_peak = max(self._recent_peak, size)

        return self._buf

    def maybe_decay(self) -> None:
        """Shrink or release the buffer if idle for too long.

        Should be called periodically (e.g. from a GC / timer loop).
        """
        if self._buf is None:
            return
        elapsed = time.monotonic() - self._last_access
        if elapsed < self._cfg.decay_after_seconds:
            return
        if len(self._buf) > self._cfg.min_size:
            self._buf = bytearray(self._cfg.min_size)
        else:
            self._buf = None
        self._consecutive_small = 0
        self._recent_peak = 0

    def release(self) -> None:
        """Drop the buffer entirely (connection close)."""
        self._buf = None
        self._consecutive_small = 0
        self._recent_peak = 0

    # ------------------------------------------------------------------
    # Diagnostics
    # ------------------------------------------------------------------

    @property
    def capacity(self) -> int:
        """Current buffer capacity in bytes (0 if released)."""
        return len(self._buf) if self._buf is not None else 0

    def stats(self) -> dict:
        """Return diagnostic snapshot."""
        return {
            'capacity': self.capacity,
            'recent_peak': self._recent_peak,
            'consecutive_small': self._consecutive_small,
            'idle_seconds': round(time.monotonic() - self._last_access, 2),
        }

    def __repr__(self) -> str:
        cap = self.capacity
        if cap == 0:
            return 'AdaptiveBuffer(released)'
        unit = 'MB' if cap < 1 << 30 else 'GB'
        val = cap / (1 << 20) if cap < 1 << 30 else cap / (1 << 30)
        return f'AdaptiveBuffer({val:.1f} {unit})'
