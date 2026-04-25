"""Unit tests for the Rust memory pool Python bindings (c_two.mem)."""

import multiprocessing.shared_memory as shm
import random
import threading

import pytest

from c_two.mem import MemPool, PoolConfig, cleanup_stale_shm


# ------------------------------------------------------------------
# PoolConfig validation
# ------------------------------------------------------------------

class TestPoolConfig:
    def test_defaults(self):
        cfg = PoolConfig()
        assert cfg.segment_size == 256 * 1024 * 1024
        assert cfg.min_block_size == 4096
        assert cfg.max_segments == 8

    def test_custom_values(self):
        cfg = PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
            max_segments=2,
            max_dedicated_segments=1,
        )
        assert cfg.segment_size == 64 * 1024
        assert cfg.max_segments == 2

    def test_non_power_of_two_block_fails(self):
        with pytest.raises(ValueError, match='power of 2'):
            PoolConfig(min_block_size=3000)

    def test_segment_too_small_fails(self):
        with pytest.raises(ValueError, match='2x min_block_size'):
            PoolConfig(segment_size=4096, min_block_size=4096)


# ------------------------------------------------------------------
# Basic allocation
# ------------------------------------------------------------------

class TestBasicAllocation:
    @pytest.fixture
    def pool(self):
        p = MemPool(PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
            max_segments=4,
            max_dedicated_segments=2,
        ))
        yield p
        p.destroy()

    def test_alloc_and_free(self, pool):
        alloc = pool.alloc(4096)
        assert alloc.actual_size >= 4096
        assert not alloc.is_dedicated
        pool.free(alloc)
        stats = pool.stats()
        assert stats.alloc_count == 0

    def test_alloc_returns_disjoint_offsets(self, pool):
        a = pool.alloc(4096)
        b = pool.alloc(4096)
        assert a.offset != b.offset or a.seg_idx != b.seg_idx
        pool.free(a)
        pool.free(b)

    def test_alloc_rounds_up(self, pool):
        alloc = pool.alloc(5000)
        assert alloc.actual_size == 8192
        pool.free(alloc)

    def test_zero_alloc_fails(self, pool):
        with pytest.raises(RuntimeError):
            pool.alloc(0)


# ------------------------------------------------------------------
# Data read/write
# ------------------------------------------------------------------

class TestDataReadWrite:
    @pytest.fixture
    def pool(self):
        p = MemPool(PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
            max_segments=4,
        ))
        yield p
        p.destroy()

    def test_write_and_read(self, pool):
        alloc = pool.alloc(4096)
        data = b'hello world!' * 100
        pool.write(alloc, data)
        result = pool.read(alloc, len(data))
        assert result == data
        pool.free(alloc)

    def test_write_exceeds_size_fails(self, pool):
        alloc = pool.alloc(4096)
        with pytest.raises(ValueError, match='exceeds'):
            pool.write(alloc, b'\x00' * (alloc.actual_size + 1))
        pool.free(alloc)

    def test_read_at_and_free_at(self, pool):
        """Test the cross-process read/free methods."""
        alloc = pool.alloc(4096)
        data = b'cross-process test data'
        pool.write(alloc, data)

        # Read using read_at (simulates remote side).
        result = pool.read_at(alloc.seg_idx, alloc.offset, len(data), alloc.is_dedicated)
        assert result == data

        # Free using free_at (simulates remote side).
        pool.free_at(alloc.seg_idx, alloc.offset, 4096, alloc.is_dedicated)
        stats = pool.stats()
        assert stats.alloc_count == 0

    def test_large_data_roundtrip(self, pool):
        """Test with data that spans multiple blocks."""
        size = 32 * 1024  # 32KB
        alloc = pool.alloc(size)
        data = bytes(range(256)) * (size // 256)
        pool.write(alloc, data)
        result = pool.read(alloc, len(data))
        assert result == data
        pool.free(alloc)


# ------------------------------------------------------------------
# Segment management
# ------------------------------------------------------------------

class TestSegmentManagement:
    def test_lazy_segment_creation(self):
        pool = MemPool(PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
            max_segments=4,
        ))
        try:
            assert pool.segment_count() == 0
            alloc = pool.alloc(4096)
            assert pool.segment_count() == 1
            pool.free(alloc)
        finally:
            pool.destroy()

    def test_segment_name(self):
        pool = MemPool(PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
        ))
        try:
            alloc = pool.alloc(4096)
            name = pool.segment_name(0)
            assert name is not None
            assert name.startswith('/cc3b')
            pool.free(alloc)
        finally:
            pool.destroy()

    def test_open_segment(self):
        """Test that one pool can open another pool's segment."""
        pool1 = MemPool(PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
        ))
        pool2 = None
        try:
            alloc = pool1.alloc(4096)
            data = b'shared data across pools'
            pool1.write(alloc, data)
            seg_name = pool1.segment_name(0)

            # Open same segment from second pool.
            pool2 = MemPool(PoolConfig(
                segment_size=64 * 1024,
                min_block_size=4096,
            ))
            pool2.open_segment(seg_name, 64 * 1024)

            # Read from pool2 using read_at.
            result = pool2.read_at(0, alloc.offset, len(data), False)
            assert result == data

            # Free from pool2 (cross-process free).
            pool2.free_at(0, alloc.offset, 4096, False)
        finally:
            if pool2 is not None:
                pool2.destroy()
            pool1.destroy()


# ------------------------------------------------------------------
# Dedicated segment fallback
# ------------------------------------------------------------------

class TestDedicatedFallback:
    def test_oversized_alloc_uses_dedicated(self):
        pool = MemPool(PoolConfig(
            segment_size=32 * 1024,
            min_block_size=4096,
            max_segments=1,
            max_dedicated_segments=2,
        ))
        try:
            # Fill the buddy segment.
            a = pool.alloc(16 * 1024)
            # This should go to dedicated.
            b = pool.alloc(64 * 1024)
            assert b.is_dedicated
            pool.free(a)
            pool.free(b)
            pool.gc()
        finally:
            pool.destroy()

    def test_segment_exhaustion_degradation(self):
        """Test fallback chain: buddy → dedicated → error."""
        pool = MemPool(PoolConfig(
            segment_size=64 * 1024,   # 64KB buddy segment
            min_block_size=4096,      # 4KB min block
            max_segments=1,           # No buddy expansion
            max_dedicated_segments=2, # Only 2 dedicated segments
        ))
        try:
            # Phase 1: fill buddy segment with 4KB blocks
            buddy_allocs = []
            while True:
                a = pool.alloc(4096)
                if a.is_dedicated:
                    # Buddy exhausted — this allocation fell through to dedicated
                    buddy_allocs.append(a)
                    break
                buddy_allocs.append(a)
            assert any(a.is_dedicated for a in buddy_allocs), \
                'Expected at least one dedicated allocation after buddy exhaustion'

            # Phase 2: exhaust dedicated segments
            dedicated_allocs = []
            for _ in range(10):
                try:
                    d = pool.alloc(4096)
                    dedicated_allocs.append(d)
                except RuntimeError:
                    # Phase 3: all capacity exhausted — error raised
                    break
            else:
                pytest.fail('Expected RuntimeError when all segments exhausted')

            # Cleanup
            for a in buddy_allocs + dedicated_allocs:
                pool.free(a)
            pool.gc()
        finally:
            pool.destroy()


# ------------------------------------------------------------------
# Statistics
# ------------------------------------------------------------------

class TestPoolStats:
    def test_stats_tracking(self):
        pool = MemPool(PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
        ))
        try:
            a = pool.alloc(4096)
            stats = pool.stats()
            assert stats.total_segments == 1
            assert stats.alloc_count >= 1
            assert stats.total_bytes > 0
            pool.free(a)
            stats = pool.stats()
            assert stats.alloc_count == 0
        finally:
            pool.destroy()


# ------------------------------------------------------------------
# Thread safety
# ------------------------------------------------------------------

class TestThreadSafety:
    def test_concurrent_alloc_free(self):
        """Multiple threads allocating and freeing concurrently."""
        pool = MemPool(PoolConfig(
            segment_size=256 * 1024,
            min_block_size=4096,
            max_segments=4,
        ))
        errors = []

        def worker(thread_id):
            try:
                for i in range(50):
                    alloc = pool.alloc(4096)
                    data = f'thread-{thread_id}-iter-{i}'.encode()
                    pool.write(alloc, data)
                    result = pool.read(alloc, len(data))
                    if result != data:
                        errors.append(f'Data mismatch: {result!r} != {data!r}')
                    pool.free(alloc)
            except Exception as e:
                errors.append(str(e))

        try:
            threads = [threading.Thread(target=worker, args=(i,)) for i in range(4)]
            for t in threads:
                t.start()
            for t in threads:
                t.join()

            assert errors == [], f'Thread safety errors: {errors}'
        finally:
            pool.destroy()

    def test_concurrent_read_at_free_at(self):
        """Test cross-process-style read_at/free_at under true concurrency."""
        pool = MemPool(PoolConfig(
            segment_size=256 * 1024,
            min_block_size=4096,
            max_segments=4,
        ))
        errors = []

        def worker(thread_id):
            try:
                for i in range(50):
                    alloc = pool.alloc(4096)
                    data = f't{thread_id}-{i}'.encode().ljust(64, b'\x00')
                    pool.write(alloc, data)
                    result = pool.read_at(
                        alloc.seg_idx, alloc.offset, len(data), alloc.is_dedicated,
                    )
                    if result != data:
                        errors.append(
                            f'Thread {thread_id}: data mismatch at iter {i}'
                        )
                    pool.free_at(
                        alloc.seg_idx, alloc.offset, 4096, alloc.is_dedicated,
                    )
            except Exception as e:
                errors.append(f'Thread {thread_id}: {e}')

        try:
            threads = [
                threading.Thread(target=worker, args=(i,)) for i in range(4)
            ]
            for t in threads:
                t.start()
            for t in threads:
                t.join()

            assert errors == [], f'Concurrent read_at/free_at errors: {errors}'
            stats = pool.stats()
            assert stats.alloc_count == 0
        finally:
            pool.destroy()


# ------------------------------------------------------------------
# Buddy merge correctness
# ------------------------------------------------------------------

class TestBuddyMerge:
    def test_buddy_merge_correctness(self):
        """Stress test: alloc many small blocks, free in random order, verify full merge."""
        pool = MemPool(PoolConfig(
            segment_size=1 * 1024 * 1024,  # 1MB
            min_block_size=4096,
            max_segments=1,
            max_dedicated_segments=0,
        ))
        try:
            initial_alloc = pool.alloc(4096)
            initial_stats = pool.stats()
            pool.free(initial_alloc)
            initial_stats = pool.stats()

            # Allocate many small blocks
            allocs = []
            for _ in range(100):
                a = pool.alloc(4096)
                allocs.append(a)

            # Free in random order
            random.shuffle(allocs)
            for a in allocs:
                pool.free(a)

            # Verify stats match initial
            final_stats = pool.stats()
            assert final_stats.free_bytes == initial_stats.free_bytes
            assert final_stats.alloc_count == 0

            # Verify large alloc works (buddies merged back)
            big = pool.alloc(512 * 1024)  # 512KB should work
            pool.free(big)
        finally:
            pool.destroy()


# ------------------------------------------------------------------
# SHM leak detection
# ------------------------------------------------------------------

class TestSHMCleanup:
    def test_destroy_unlinks_shm(self):
        """Verify destroy() unlinks POSIX SHM segments."""
        pool = MemPool(PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
            max_segments=1,
            max_dedicated_segments=0,
        ))
        try:
            # Force segment creation via alloc
            a = pool.alloc(4096)
            seg_name = pool.segment_name(0)
            pool.free(a)
        finally:
            pool.destroy()

        # After destroy, the segment should not be openable
        try:
            s = shm.SharedMemory(name=seg_name.lstrip('/'), create=False)
            s.close()
            pytest.fail(f'SHM segment {seg_name} still exists after destroy()')
        except FileNotFoundError:
            pass  # Expected — segment was properly unlinked


# ------------------------------------------------------------------
# Safety tests (from op3-analysis.md audit)
# ------------------------------------------------------------------

class TestBuddyPanicSafety:
    """Tests for BUDDY-PANIC-1 and BUDDY-PANIC-2 from op3-analysis.md."""

    def test_oversized_dedicated_returns_error_not_panic(self):
        """BUDDY-PANIC-1: >4GB dedicated alloc must return error, not assert-crash."""
        pool = MemPool(PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
            max_segments=1,
            max_dedicated_segments=2,
        ))
        try:
            # Request way beyond what's reasonable — must raise, not panic
            with pytest.raises(RuntimeError):
                pool.alloc(5 * 1024 * 1024 * 1024)  # 5GB
        finally:
            pool.destroy()

    def test_negative_gc_delay_no_panic(self):
        """BUDDY-PANIC-2: negative gc_delay_secs must not cause Duration panic."""
        cfg = PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
            max_segments=1,
            max_dedicated_segments=1,
            dedicated_crash_timeout_secs=-1.0,
        )
        pool = MemPool(cfg)
        try:
            # Allocate a dedicated segment, then GC should clamp delay to zero
            alloc = pool.alloc(128 * 1024)  # > segment_size → dedicated
            pool.free(alloc)
            # If we get here without panic, the test passes
            stats = pool.stats()
            assert stats.alloc_count == 0
        finally:
            pool.destroy()

    def test_nan_gc_delay_rejected(self):
        """Config validation: NaN gc_delay_secs must be rejected."""
        with pytest.raises(ValueError, match='NaN'):
            PoolConfig(dedicated_crash_timeout_secs=float('nan'))

    def test_non_power_of_two_segment_rejected(self):
        """Config validation: non-power-of-2 segment_size must be rejected."""
        with pytest.raises(ValueError, match='power of 2'):
            PoolConfig(segment_size=100_000)


class TestDoubleFreeSafety:
    """Test that pool handles double-free and invalid-free gracefully."""

    def test_double_free_does_not_corrupt(self):
        """Double-free on the same block must raise or be safely no-op."""
        pool = MemPool(PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
            max_segments=1,
        ))
        try:
            alloc = pool.alloc(4096)
            pool.free_at(alloc.seg_idx, alloc.offset, alloc.actual_size, False)
            # Second free — should raise or be safely handled
            try:
                pool.free_at(alloc.seg_idx, alloc.offset, alloc.actual_size, False)
            except RuntimeError:
                pass  # Expected: double-free detected
            # Either way, pool stats should not underflow
            stats = pool.stats()
            assert stats.alloc_count >= 0
        finally:
            pool.destroy()

    def test_alloc_after_double_free_still_works(self):
        """Pool must remain usable after a double-free attempt."""
        pool = MemPool(PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
            max_segments=1,
        ))
        try:
            alloc = pool.alloc(4096)
            pool.free(alloc)
            try:
                pool.free(alloc)
            except RuntimeError:
                pass
            # Pool must still be functional
            alloc2 = pool.alloc(4096)
            data = b'after-double-free'
            pool.write(alloc2, data)
            result = pool.read(alloc2, len(data))
            assert result == data
            pool.free(alloc2)
        finally:
            pool.destroy()


class TestSegmentExhaustion:
    """Test graceful degradation when pool is fully exhausted."""

    def test_exhaustion_returns_error(self):
        """When both buddy and dedicated segments are exhausted, alloc must raise."""
        pool = MemPool(PoolConfig(
            segment_size=64 * 1024,  # 64KB buddy
            min_block_size=4096,
            max_segments=1,
            max_dedicated_segments=0,  # No dedicated fallback
        ))
        try:
            # Fill buddy pool completely (64KB / 4KB = 16 blocks)
            allocs = []
            for _ in range(16):
                allocs.append(pool.alloc(4096))

            # Next alloc should fail
            with pytest.raises(RuntimeError):
                pool.alloc(4096)

            # Cleanup
            for a in allocs:
                pool.free(a)
        finally:
            pool.destroy()

    def test_dedicated_exhaustion_returns_error(self):
        """When max_dedicated_segments is reached, oversized alloc must raise."""
        pool = MemPool(PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
            max_segments=1,
            max_dedicated_segments=1,
        ))
        try:
            # First oversized → dedicated
            a1 = pool.alloc(128 * 1024)
            assert a1.is_dedicated
            # Second oversized → should fail (max_dedicated=1)
            with pytest.raises(RuntimeError):
                pool.alloc(128 * 1024)
            pool.free(a1)
        finally:
            pool.destroy()
