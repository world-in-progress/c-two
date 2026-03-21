"""Unit tests for :mod:`c_two.rpc.util.adaptive_buffer`."""

import time
import pytest
from unittest.mock import patch

from c_two.rpc.util.adaptive_buffer import (
    AdaptiveBuffer,
    AdaptiveBufferConfig,
    SIZE_TABLE,
    _ceil_size,
)


# ------------------------------------------------------------------
# SIZE_TABLE / _ceil_size
# ------------------------------------------------------------------

class TestSizeTable:

    def test_power_of_two(self):
        for s in SIZE_TABLE:
            assert s & (s - 1) == 0, f'{s} is not a power of 2'

    def test_starts_at_1mb(self):
        assert SIZE_TABLE[0] == 1 << 20

    def test_ends_at_4gb(self):
        assert SIZE_TABLE[-1] == 1 << 32

    def test_ceil_exact_match(self):
        assert _ceil_size(1 << 20) == 1 << 20

    def test_ceil_rounds_up(self):
        assert _ceil_size((1 << 20) + 1) == 1 << 21

    def test_ceil_beyond_table(self):
        huge = (1 << 32) + 1
        assert _ceil_size(huge) == huge


# ------------------------------------------------------------------
# Grow behaviour
# ------------------------------------------------------------------

class TestGrow:

    def test_first_acquire_creates_buffer(self):
        ab = AdaptiveBuffer()
        view = ab.acquire(100)
        assert len(view) == 100
        assert ab.capacity >= 100

    def test_grow_on_larger_request(self):
        ab = AdaptiveBuffer()
        ab.acquire(1 << 20)  # 1 MB
        cap1 = ab.capacity
        ab.acquire(1 << 22)  # 4 MB
        cap2 = ab.capacity
        assert cap2 >= 1 << 22
        assert cap2 > cap1

    def test_no_realloc_when_fits(self):
        ab = AdaptiveBuffer()
        ab.acquire(1 << 21)  # 2 MB
        buf_id = id(ab.raw_buffer)
        ab.acquire(1 << 20)  # 1 MB — still fits
        assert id(ab.raw_buffer) == buf_id

    def test_min_size_floor(self):
        cfg = AdaptiveBufferConfig(min_size=4 << 20)  # 4 MB
        ab = AdaptiveBuffer(config=cfg)
        ab.acquire(100)
        assert ab.capacity >= 4 << 20


# ------------------------------------------------------------------
# Shrink behaviour
# ------------------------------------------------------------------

class TestShrink:

    def test_no_shrink_before_threshold(self):
        cfg = AdaptiveBufferConfig(shrink_after_n=3, shrink_threshold=0.25)
        ab = AdaptiveBuffer(config=cfg)
        ab.acquire(8 << 20)  # 8 MB → allocates 8 MB
        big_cap = ab.capacity

        # Only 2 small reads — not enough to trigger shrink
        ab.acquire(100)
        ab.acquire(100)
        assert ab.capacity == big_cap

    def test_shrink_after_n_small_reads(self):
        cfg = AdaptiveBufferConfig(shrink_after_n=3, shrink_threshold=0.25, min_size=1 << 20)
        ab = AdaptiveBuffer(config=cfg)
        ab.acquire(64 << 20)  # 64 MB
        big_cap = ab.capacity

        # 3 consecutive small reads
        for _ in range(3):
            ab.acquire(100)

        assert ab.capacity < big_cap

    def test_large_read_resets_counter(self):
        cfg = AdaptiveBufferConfig(shrink_after_n=3, shrink_threshold=0.25)
        ab = AdaptiveBuffer(config=cfg)
        ab.acquire(8 << 20)
        big_cap = ab.capacity

        ab.acquire(100)  # small #1
        ab.acquire(100)  # small #2
        ab.acquire(8 << 20)  # large — resets counter
        ab.acquire(100)  # small #1 again
        ab.acquire(100)  # small #2

        assert ab.capacity == big_cap  # no shrink yet

    def test_shrink_respects_min_size(self):
        min_sz = 2 << 20
        cfg = AdaptiveBufferConfig(shrink_after_n=2, shrink_threshold=0.25, min_size=min_sz)
        ab = AdaptiveBuffer(config=cfg)
        ab.acquire(32 << 20)

        for _ in range(2):
            ab.acquire(10)

        assert ab.capacity >= min_sz


# ------------------------------------------------------------------
# Time-decay behaviour
# ------------------------------------------------------------------

class TestDecay:

    def test_no_decay_when_active(self):
        ab = AdaptiveBuffer()
        ab.acquire(8 << 20)
        cap = ab.capacity
        ab.maybe_decay()
        assert ab.capacity == cap

    def test_decay_after_idle(self):
        cfg = AdaptiveBufferConfig(decay_after_seconds=0.1, min_size=1 << 20)
        ab = AdaptiveBuffer(config=cfg)
        ab.acquire(64 << 20)
        big_cap = ab.capacity

        time.sleep(0.15)
        ab.maybe_decay()

        assert ab.capacity < big_cap
        assert ab.capacity <= cfg.min_size

    def test_decay_releases_when_at_min(self):
        cfg = AdaptiveBufferConfig(decay_after_seconds=0.1, min_size=1 << 20)
        ab = AdaptiveBuffer(config=cfg)
        ab.acquire(1 << 20)  # exactly min_size

        time.sleep(0.15)
        ab.maybe_decay()

        # At min_size, decay releases entirely
        assert ab.capacity == 0

    def test_decay_noop_when_released(self):
        ab = AdaptiveBuffer()
        ab.release()
        ab.maybe_decay()  # should not raise
        assert ab.capacity == 0


# ------------------------------------------------------------------
# Release
# ------------------------------------------------------------------

class TestRelease:

    def test_release_drops_buffer(self):
        ab = AdaptiveBuffer()
        ab.acquire(4 << 20)
        assert ab.capacity > 0
        ab.release()
        assert ab.capacity == 0
        assert ab.raw_buffer is None

    def test_acquire_after_release(self):
        ab = AdaptiveBuffer()
        ab.acquire(4 << 20)
        ab.release()
        view = ab.acquire(1 << 20)
        assert len(view) == 1 << 20
        assert ab.capacity >= 1 << 20


# ------------------------------------------------------------------
# Diagnostics
# ------------------------------------------------------------------

class TestDiagnostics:

    def test_stats_keys(self):
        ab = AdaptiveBuffer()
        ab.acquire(1 << 20)
        s = ab.stats()
        assert 'capacity' in s
        assert 'recent_peak' in s
        assert 'consecutive_small' in s
        assert 'idle_seconds' in s

    def test_repr_released(self):
        ab = AdaptiveBuffer()
        assert 'released' in repr(ab)

    def test_repr_mb(self):
        ab = AdaptiveBuffer()
        ab.acquire(4 << 20)
        assert 'MB' in repr(ab)
