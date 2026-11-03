"""Unit tests for the Rust memory pool Python bindings (c_two.mem)."""

import multiprocessing.shared_memory as shm
import math
import random
import threading

import pytest

from c_two.mem import MemPool, PoolConfig, cleanup_stale_shm


def test_manual_assembly_exports_retired():
    from c_two import _native, mem

    for name in ("ChunkAssembler", "MemHandle"):
        assert name not in mem.__all__
        assert not hasattr(mem, name)
        assert not hasattr(_native, name)


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

    @pytest.mark.parametrize(
        'field',
        ['buddy_idle_decay_secs'],
    )
    @pytest.mark.parametrize('value', [math.inf, 1e100])
    def test_gc_delays_must_be_representable(self, field, value):
        with pytest.raises(ValueError, match=field):
            PoolConfig(**{field: value})


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
        del p

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
        del p

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
            del pool

    def test_segment_name(self):
        pool = MemPool(PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
        ))
        try:
            alloc = pool.alloc(4096)
            name = pool.segment_name(0)
            assert name is not None
            assert name == pool.derive_segment_name(0, alloc.generation)
            pool.free(alloc)
        finally:
            del pool


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
            del pool

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
            del pool


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
            assert stats.buddy_data_bytes > 0
            assert (
                stats.buddy_occupied_bytes + stats.buddy_idle_bytes
                == stats.buddy_data_bytes
            )
            assert stats.buddy_occupied_bytes > 0
            assert 0.0 < stats.utilization_ratio <= 1.0
            assert stats.buddy_expanded_allocs == 1
            assert stats.buddy_reused_allocs == 0
            assert stats.dedicated_mapped_bytes == 0
            assert stats.pressure_denied_backings == 0
            pool.free(a)
            stats = pool.stats()
            assert stats.alloc_count == 0
            assert stats.buddy_occupied_bytes == 0
            assert stats.buddy_idle_bytes == stats.buddy_data_bytes
            assert stats.utilization_ratio == 0.0
        finally:
            del pool

    def test_stats_dedicated_release_drops_backing(self):
        pool = MemPool(PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
            max_segments=1,
            max_dedicated_segments=2,
        ))
        try:
            alloc = pool.alloc(128 * 1024)  # > segment_size → dedicated
            stats = pool.stats()
            assert stats.dedicated_segments == 1
            assert stats.dedicated_active_count == 1
            assert stats.dedicated_active_bytes == stats.dedicated_mapped_bytes
            assert stats.dedicated_allocs == 1
            pool.free(alloc)
            # Dedicated release immediately drops its local mapping.
            stats = pool.stats()
            assert stats.dedicated_active_count == 0
            assert stats.dedicated_active_bytes == 0
            assert stats.dedicated_mapped_bytes == 0
            assert stats.dedicated_segments == 0
            assert stats.alloc_count == 0
            pool.gc()
            stats = pool.stats()
            assert stats.dedicated_mapped_bytes == 0
        finally:
            del pool

    def test_stats_utilization_is_not_fragmentation(self):
        """utilization_ratio is the occupied share of buddy data capacity."""
        pool = MemPool(PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
        ))
        try:
            assert pool.stats().utilization_ratio == 0.0
            a = pool.alloc(64 * 1024)
            assert pool.stats().utilization_ratio == 1.0
            pool.free(a)
            assert pool.stats().utilization_ratio == 0.0
        finally:
            del pool


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
            del pool


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
            assert final_stats.buddy_idle_bytes == initial_stats.buddy_idle_bytes
            assert final_stats.alloc_count == 0

            # Verify large alloc works (buddies merged back)
            big = pool.alloc(512 * 1024)  # 512KB should work
            pool.free(big)
        finally:
            del pool


# ------------------------------------------------------------------
# SHM leak detection
# ------------------------------------------------------------------

class TestSHMCleanup:
    def test_last_pool_owner_unlinks_shm(self):
        """Dropping the last pool owner unlinks POSIX SHM segments."""
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
            del pool

        # After the last pool owner drops, the segment should not be openable
        try:
            s = shm.SharedMemory(name=seg_name.lstrip('/'), create=False)
            s.close()
            pytest.fail(f'SHM segment {seg_name} still exists after the last pool owner drops')
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
            del pool

    def test_negative_gc_delay_no_panic(self):
        """BUDDY-PANIC-2: negative buddy_idle_decay_secs must not cause Duration panic."""
        cfg = PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
            max_segments=1,
            max_dedicated_segments=1,
            buddy_idle_decay_secs=-1.0,
        )
        pool = MemPool(cfg)
        try:
            # An idle buddy backing can be collected with a nonpositive delay.
            alloc = pool.alloc(4096)
            pool.free(alloc)
            # If we get here without panic, the test passes
            stats = pool.stats()
            assert stats.alloc_count == 0
        finally:
            del pool

    def test_nan_gc_delay_rejected(self):
        """Config validation: NaN buddy_idle_decay_secs must be rejected."""
        with pytest.raises(ValueError, match='buddy_idle_decay_secs'):
            PoolConfig(buddy_idle_decay_secs=float('nan'))

    def test_non_power_of_two_segment_rejected(self):
        """Config validation: non-power-of-2 segment_size must be rejected."""
        with pytest.raises(ValueError, match='power of 2'):
            PoolConfig(segment_size=100_000)


class TestDoubleFreeSafety:
    """A Python allocation can be released only once by its owner pool."""

    def test_double_free_rejected_without_releasing_reused_block(self):
        pool = MemPool(PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
            max_segments=1,
        ))
        first = pool.alloc(4096)
        pool.free(first)
        reused = pool.alloc(4096)
        assert (reused.seg_idx, reused.offset) == (first.seg_idx, first.offset)
        before = pool.stats().alloc_count

        with pytest.raises(RuntimeError, match='already released'):
            pool.free(first)
        assert pool.stats().alloc_count == before

        next_alloc = pool.alloc(4096)
        assert (next_alloc.seg_idx, next_alloc.offset) != (reused.seg_idx, reused.offset)
        pool.write(reused, b'reused')
        pool.write(next_alloc, b'next')
        assert pool.read(reused, 6) == b'reused'
        assert pool.read(next_alloc, 4) == b'next'
        pool.free(reused)
        pool.free(next_alloc)
        assert pool.stats().alloc_count == 0

    def test_cross_pool_free_rejected_without_consuming_allocation(self):
        config = PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
            max_segments=1,
        )
        owner = MemPool(config)
        other = MemPool(config)
        alloc, _ = owner.alloc_ptr(4096)
        other_alloc = other.alloc(4096)
        assert (
            alloc.seg_idx, alloc.generation, alloc.offset, alloc.level, alloc.is_dedicated
        ) == (
            other_alloc.seg_idx, other_alloc.generation, other_alloc.offset,
            other_alloc.level, other_alloc.is_dedicated
        )
        owner_count = owner.stats().alloc_count
        other_count = other.stats().alloc_count

        with pytest.raises(RuntimeError, match='different memory pool'):
            other.free(alloc)
        assert owner.stats().alloc_count == owner_count
        assert other.stats().alloc_count == other_count

        next_alloc = other.alloc(4096)
        assert (next_alloc.seg_idx, next_alloc.offset) != (
            other_alloc.seg_idx, other_alloc.offset
        )
        other.write(other_alloc, b'other')
        other.write(next_alloc, b'next')
        assert other.read(other_alloc, 5) == b'other'
        assert other.read(next_alloc, 4) == b'next'
        owner.free(alloc)
        other.free(other_alloc)
        other.free(next_alloc)
        assert owner.stats().alloc_count == 0
        assert other.stats().alloc_count == 0

    @pytest.mark.parametrize('operation', ['read', 'write', 'write_from_buffer'])
    @pytest.mark.parametrize('invalid', ['released', 'foreign'])
    def test_invalid_allocation_access_preserves_live_block(self, operation, invalid):
        config = PoolConfig(segment_size=64 * 1024, max_segments=1)
        pool = MemPool(config)
        owner = pool if invalid == 'released' else MemPool(config)
        alloc = owner.alloc(4096)
        if invalid == 'released':
            pool.free(alloc)
        live = pool.alloc(4096)
        pool.write(live, b'unchanged')
        before = pool.stats().alloc_count
        message = 'already released' if invalid == 'released' else 'different memory pool'
        with pytest.raises(RuntimeError, match=message):
            if operation == 'read':
                pool.read(alloc, 9)
            elif operation == 'write':
                pool.write(alloc, b'corrupted')
            else:
                pool.write_from_buffer(b'corrupted', alloc)
        assert pool.stats().alloc_count == before
        assert pool.read(live, 9) == b'unchanged'
        pool.free(live)
        if invalid == 'foreign':
            owner.free(alloc)


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
            del pool

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
            del pool
