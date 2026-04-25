"""Unit tests for HoldRegistry — server-side hold-mode SHM tracking."""
import gc
import time
import threading
import weakref
import pytest

from c_two.transport.server.hold_registry import HoldEntry, HoldRegistry


class _FakeBuffer:
    """Simulates PyShmBuffer for testing. Supports weakrefs and has is_inline."""
    __slots__ = ('data_len', 'is_inline', '__weakref__')

    def __init__(self, data_len=1024, is_inline=False):
        self.data_len = data_len
        self.is_inline = is_inline

    def __len__(self):
        return self.data_len


class TestHoldRegistryTrack:
    def test_track_increases_count(self):
        reg = HoldRegistry(warn_threshold=60.0)
        buf = _FakeBuffer(1024)
        reg.track(buf, 'method_a', 'route_a')
        stats = reg.stats()
        assert stats['active_holds'] == 1
        assert stats['total_held_bytes'] == 1024

    def test_track_multiple(self):
        reg = HoldRegistry(warn_threshold=60.0)
        bufs = [_FakeBuffer(i * 100) for i in range(1, 4)]
        for i, buf in enumerate(bufs):
            reg.track(buf, f'method_{i}', 'route')
        stats = reg.stats()
        assert stats['active_holds'] == 3
        assert stats['total_held_bytes'] == 100 + 200 + 300

    def test_track_skips_inline(self):
        """Inline buffers (no SHM) should not be tracked."""
        reg = HoldRegistry(warn_threshold=60.0)
        buf = _FakeBuffer(64, is_inline=True)
        reg.track(buf, 'method', 'route')
        stats = reg.stats()
        assert stats['active_holds'] == 0


class TestHoldRegistryAutoCleanup:
    def test_weakref_cleanup_on_gc(self):
        """When buffer is GC'd, entry is automatically removed."""
        reg = HoldRegistry(warn_threshold=60.0)
        buf = _FakeBuffer(2048)
        reg.track(buf, 'method', 'route')
        assert reg.stats()['active_holds'] == 1

        del buf
        gc.collect()

        assert reg.stats()['active_holds'] == 0

    def test_weakref_cleanup_multiple(self):
        reg = HoldRegistry(warn_threshold=60.0)
        bufs = [_FakeBuffer(100) for _ in range(5)]
        for buf in bufs:
            reg.track(buf, 'method', 'route')
        assert reg.stats()['active_holds'] == 5

        # Delete first and third buffers
        del bufs[2]
        del bufs[0]
        gc.collect()

        stats = reg.stats()
        assert stats['active_holds'] == 3


class TestHoldRegistrySweep:
    def test_sweep_returns_stale(self):
        reg = HoldRegistry(warn_threshold=0.01)  # 10ms threshold
        buf = _FakeBuffer(512)
        reg.track(buf, 'stale_method', 'route_x')
        time.sleep(0.02)  # exceed threshold

        stale = reg.sweep()
        assert len(stale) == 1
        assert stale[0].method_name == 'stale_method'
        assert stale[0].route_name == 'route_x'

    def test_sweep_skips_fresh(self):
        reg = HoldRegistry(warn_threshold=10.0)  # 10s threshold
        buf = _FakeBuffer(512)
        reg.track(buf, 'fresh_method', 'route')

        stale = reg.sweep()
        assert len(stale) == 0

    def test_sweep_cleans_dead_refs(self):
        reg = HoldRegistry(warn_threshold=60.0)
        buf = _FakeBuffer(100)
        reg.track(buf, 'method', 'route')
        del buf
        gc.collect()

        stale = reg.sweep()
        assert len(stale) == 0
        assert reg.stats()['active_holds'] == 0


class TestHoldRegistryStats:
    def test_empty_stats(self):
        reg = HoldRegistry(warn_threshold=60.0)
        stats = reg.stats()
        assert stats == {
            'active_holds': 0,
            'total_held_bytes': 0,
            'oldest_hold_seconds': 0,
        }

    def test_oldest_hold_seconds(self):
        reg = HoldRegistry(warn_threshold=60.0)
        buf = _FakeBuffer(100)
        reg.track(buf, 'method', 'route')
        time.sleep(0.05)

        stats = reg.stats()
        assert stats['oldest_hold_seconds'] >= 0.04


class TestHoldRegistryThreadSafety:
    def test_concurrent_track_and_sweep(self):
        """Track and sweep from multiple threads without deadlock."""
        reg = HoldRegistry(warn_threshold=0.001)
        errors = []
        bufs = []  # keep references to prevent GC during test

        def tracker():
            try:
                for _ in range(50):
                    buf = _FakeBuffer(64)
                    bufs.append(buf)
                    reg.track(buf, 'method', 'route')
            except Exception as e:
                errors.append(e)

        def sweeper():
            try:
                for _ in range(50):
                    reg.sweep()
            except Exception as e:
                errors.append(e)

        threads = [
            threading.Thread(target=tracker),
            threading.Thread(target=tracker),
            threading.Thread(target=sweeper),
        ]
        for t in threads:
            t.start()
        for t in threads:
            t.join(timeout=5.0)

        assert not errors, f"Thread errors: {errors}"
