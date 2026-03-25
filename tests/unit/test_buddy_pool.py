"""Unit tests for the Rust buddy allocator Python bindings (c2_buddy)."""

import pytest
import threading

import c2_buddy


# ------------------------------------------------------------------
# PoolConfig validation
# ------------------------------------------------------------------

class TestPoolConfig:
    def test_defaults(self):
        cfg = c2_buddy.PoolConfig()
        assert cfg.segment_size == 256 * 1024 * 1024
        assert cfg.min_block_size == 4096
        assert cfg.max_segments == 8

    def test_custom_values(self):
        cfg = c2_buddy.PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
            max_segments=2,
            max_dedicated_segments=1,
        )
        assert cfg.segment_size == 64 * 1024
        assert cfg.max_segments == 2

    def test_non_power_of_two_block_fails(self):
        with pytest.raises(ValueError, match='power of 2'):
            c2_buddy.PoolConfig(min_block_size=3000)

    def test_segment_too_small_fails(self):
        with pytest.raises(ValueError, match='2x min_block_size'):
            c2_buddy.PoolConfig(segment_size=4096, min_block_size=4096)


# ------------------------------------------------------------------
# Basic allocation
# ------------------------------------------------------------------

class TestBasicAllocation:
    @pytest.fixture
    def pool(self):
        p = c2_buddy.BuddyPoolHandle(c2_buddy.PoolConfig(
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
        p = c2_buddy.BuddyPoolHandle(c2_buddy.PoolConfig(
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
        pool = c2_buddy.BuddyPoolHandle(c2_buddy.PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
            max_segments=4,
        ))
        assert pool.segment_count() == 0
        alloc = pool.alloc(4096)
        assert pool.segment_count() == 1
        pool.free(alloc)
        pool.destroy()

    def test_segment_name(self):
        pool = c2_buddy.BuddyPoolHandle(c2_buddy.PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
        ))
        alloc = pool.alloc(4096)
        name = pool.segment_name(0)
        assert name is not None
        assert name.startswith('/cc3b')
        pool.free(alloc)
        pool.destroy()

    def test_open_segment(self):
        """Test that one pool can open another pool's segment."""
        pool1 = c2_buddy.BuddyPoolHandle(c2_buddy.PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
        ))
        alloc = pool1.alloc(4096)
        data = b'shared data across pools'
        pool1.write(alloc, data)
        seg_name = pool1.segment_name(0)

        # Open same segment from second pool.
        pool2 = c2_buddy.BuddyPoolHandle(c2_buddy.PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
        ))
        pool2.open_segment(seg_name, 64 * 1024)

        # Read from pool2 using read_at.
        result = pool2.read_at(0, alloc.offset, len(data), False)
        assert result == data

        # Free from pool2 (cross-process free).
        pool2.free_at(0, alloc.offset, 4096, False)

        pool2.destroy()
        pool1.destroy()


# ------------------------------------------------------------------
# Dedicated segment fallback
# ------------------------------------------------------------------

class TestDedicatedFallback:
    def test_oversized_alloc_uses_dedicated(self):
        pool = c2_buddy.BuddyPoolHandle(c2_buddy.PoolConfig(
            segment_size=32 * 1024,
            min_block_size=4096,
            max_segments=1,
            max_dedicated_segments=2,
        ))
        # Fill the buddy segment.
        a = pool.alloc(16 * 1024)
        # This should go to dedicated.
        b = pool.alloc(64 * 1024)
        assert b.is_dedicated
        pool.free(a)
        pool.free(b)
        pool.gc()
        pool.destroy()


# ------------------------------------------------------------------
# Statistics
# ------------------------------------------------------------------

class TestPoolStats:
    def test_stats_tracking(self):
        pool = c2_buddy.BuddyPoolHandle(c2_buddy.PoolConfig(
            segment_size=64 * 1024,
            min_block_size=4096,
        ))
        a = pool.alloc(4096)
        stats = pool.stats()
        assert stats.total_segments == 1
        assert stats.alloc_count >= 1
        assert stats.total_bytes > 0
        pool.free(a)
        stats = pool.stats()
        assert stats.alloc_count == 0
        pool.destroy()


# ------------------------------------------------------------------
# Thread safety
# ------------------------------------------------------------------

class TestThreadSafety:
    def test_concurrent_alloc_free(self):
        """Multiple threads allocating and freeing concurrently."""
        pool = c2_buddy.BuddyPoolHandle(c2_buddy.PoolConfig(
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

        threads = [threading.Thread(target=worker, args=(i,)) for i in range(4)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        assert errors == [], f'Thread safety errors: {errors}'
        pool.destroy()

    def test_concurrent_read_at_free_at(self):
        """Test cross-process-style read_at/free_at under concurrency."""
        pool = c2_buddy.BuddyPoolHandle(c2_buddy.PoolConfig(
            segment_size=256 * 1024,
            min_block_size=4096,
            max_segments=4,
        ))
        errors = []

        def writer(allocs_out, count):
            try:
                for i in range(count):
                    alloc = pool.alloc(4096)
                    data = f'msg-{i}'.encode().ljust(64, b'\x00')
                    pool.write(alloc, data)
                    allocs_out.append((alloc.seg_idx, alloc.offset, len(data), alloc.is_dedicated))
            except Exception as e:
                errors.append(f'writer: {e}')

        def reader(allocs_in):
            try:
                for seg_idx, offset, size, is_ded in allocs_in:
                    data = pool.read_at(seg_idx, offset, size, is_ded)
                    pool.free_at(seg_idx, offset, 4096, is_ded)
            except Exception as e:
                errors.append(f'reader: {e}')

        allocs = []
        count = 50
        writer(allocs, count)
        assert len(allocs) == count

        # Read and free from another "thread" (simulating cross-process).
        reader(allocs)

        assert errors == []
        stats = pool.stats()
        assert stats.alloc_count == 0
        pool.destroy()
