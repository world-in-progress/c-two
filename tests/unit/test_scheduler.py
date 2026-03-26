"""Unit tests for rpc_v2 Scheduler (read/write concurrency control)."""
from __future__ import annotations

import threading
import time

import pytest

from c_two.rpc_v2.scheduler import (
    Scheduler,
    ConcurrencyConfig,
    ConcurrencyMode,
    MethodAccess,
    _WriterPriorityReadWriteLock,
)


# ---------------------------------------------------------------------------
# ConcurrencyConfig
# ---------------------------------------------------------------------------

class TestConcurrencyConfig:

    def test_default_exclusive(self):
        cfg = ConcurrencyConfig()
        assert cfg.mode is ConcurrencyMode.EXCLUSIVE

    def test_from_string(self):
        cfg = ConcurrencyConfig(mode='read_parallel')
        assert cfg.mode is ConcurrencyMode.READ_PARALLEL

    def test_invalid_mode(self):
        with pytest.raises(ValueError):
            ConcurrencyConfig(mode='bogus')

    def test_bad_max_workers(self):
        with pytest.raises(ValueError, match='max_workers'):
            ConcurrencyConfig(max_workers=0)

    def test_bad_max_pending(self):
        with pytest.raises(ValueError, match='max_pending'):
            ConcurrencyConfig(max_pending=-1)


# ---------------------------------------------------------------------------
# WriterPriorityReadWriteLock
# ---------------------------------------------------------------------------

class TestRWLock:

    def test_concurrent_readers(self):
        lock = _WriterPriorityReadWriteLock()
        results = []
        barrier = threading.Barrier(3)

        def reader(tid: int) -> None:
            with lock.read_lock():
                barrier.wait(timeout=5)
                results.append(tid)

        threads = [threading.Thread(target=reader, args=(i,)) for i in range(3)]
        for t in threads:
            t.start()
        for t in threads:
            t.join(timeout=10)
        assert len(results) == 3

    def test_writer_exclusive(self):
        lock = _WriterPriorityReadWriteLock()
        log: list[str] = []
        log_lock = threading.Lock()

        def writer() -> None:
            with lock.write_lock():
                with log_lock:
                    log.append('w-enter')
                time.sleep(0.05)
                with log_lock:
                    log.append('w-exit')

        def reader() -> None:
            time.sleep(0.01)  # Ensure writer starts first.
            with lock.read_lock():
                with log_lock:
                    log.append('r')

        tw = threading.Thread(target=writer)
        tr = threading.Thread(target=reader)
        tw.start()
        tr.start()
        tw.join(timeout=5)
        tr.join(timeout=5)
        # Reader must wait until writer exits.
        assert log.index('w-exit') < log.index('r')


# ---------------------------------------------------------------------------
# Scheduler — EXCLUSIVE mode
# ---------------------------------------------------------------------------

class TestSchedulerExclusive:

    def test_basic_execute(self):
        sched = Scheduler(ConcurrencyConfig(mode='exclusive'))
        try:
            sched.begin()
            result = sched.execute(lambda x: x.upper(), b'hello', MethodAccess.WRITE)
            assert result == b'HELLO'
        finally:
            sched.shutdown()

    def test_pending_count(self):
        sched = Scheduler(ConcurrencyConfig(mode='exclusive'))
        try:
            sched.begin()
            assert sched.pending_count == 1
            sched.execute(lambda x: x, b'', MethodAccess.WRITE)
            assert sched.pending_count == 0
        finally:
            sched.shutdown()

    def test_max_pending_enforced(self):
        sched = Scheduler(ConcurrencyConfig(mode='exclusive', max_pending=1))
        try:
            sched.begin()
            with pytest.raises(RuntimeError, match='capacity'):
                sched.begin()
            # Execute to clear pending.
            sched.execute(lambda x: x, b'', MethodAccess.WRITE)
        finally:
            sched.shutdown()

    def test_serializes_writes(self):
        """In EXCLUSIVE mode, all calls are serialized."""
        sched = Scheduler(ConcurrencyConfig(mode='exclusive'))
        log: list[str] = []
        log_lock = threading.Lock()

        def slow_method(x: bytes) -> bytes:
            with log_lock:
                log.append('enter')
            time.sleep(0.03)
            with log_lock:
                log.append('exit')
            return x

        def worker() -> None:
            sched.begin()
            sched.execute(slow_method, b'', MethodAccess.WRITE)

        try:
            threads = [threading.Thread(target=worker) for _ in range(3)]
            for t in threads:
                t.start()
            for t in threads:
                t.join(timeout=10)
            # In exclusive mode: enter/exit must alternate (no overlap).
            for i in range(0, len(log), 2):
                assert log[i] == 'enter'
                assert log[i + 1] == 'exit'
        finally:
            sched.shutdown()


# ---------------------------------------------------------------------------
# Scheduler — READ_PARALLEL mode
# ---------------------------------------------------------------------------

class TestSchedulerReadParallel:

    def test_readers_concurrent(self):
        sched = Scheduler(ConcurrencyConfig(mode='read_parallel', max_workers=4))
        barrier = threading.Barrier(3, timeout=5)
        results = []

        def reader(x: bytes) -> bytes:
            barrier.wait()
            results.append(1)
            return x

        def worker() -> None:
            sched.begin()
            sched.execute(reader, b'', MethodAccess.READ)

        try:
            threads = [threading.Thread(target=worker) for _ in range(3)]
            for t in threads:
                t.start()
            for t in threads:
                t.join(timeout=10)
            assert len(results) == 3
        finally:
            sched.shutdown()

    def test_writer_blocks_readers(self):
        sched = Scheduler(ConcurrencyConfig(mode='read_parallel', max_workers=4))
        log: list[str] = []
        log_lock = threading.Lock()

        def writer(x: bytes) -> bytes:
            with log_lock:
                log.append('w-enter')
            time.sleep(0.05)
            with log_lock:
                log.append('w-exit')
            return x

        def reader(x: bytes) -> bytes:
            time.sleep(0.01)  # Ensure writer starts first.
            with log_lock:
                log.append('r')
            return x

        try:
            sched.begin()
            tw = threading.Thread(
                target=sched.execute, args=(writer, b'', MethodAccess.WRITE),
            )
            tw.start()
            time.sleep(0.005)  # Let writer grab lock.

            sched.begin()
            tr = threading.Thread(
                target=sched.execute, args=(reader, b'', MethodAccess.READ),
            )
            tr.start()

            tw.join(timeout=5)
            tr.join(timeout=5)
            assert log.index('w-exit') < log.index('r')
        finally:
            sched.shutdown()


# ---------------------------------------------------------------------------
# Scheduler — PARALLEL mode
# ---------------------------------------------------------------------------

class TestSchedulerParallel:

    def test_no_locking(self):
        sched = Scheduler(ConcurrencyConfig(mode='parallel', max_workers=4))
        barrier = threading.Barrier(4, timeout=5)
        results = []

        def method(x: bytes) -> bytes:
            barrier.wait()
            results.append(1)
            return x

        def worker() -> None:
            sched.begin()
            sched.execute(method, b'', MethodAccess.WRITE)

        try:
            threads = [threading.Thread(target=worker) for _ in range(4)]
            for t in threads:
                t.start()
            for t in threads:
                t.join(timeout=10)
            assert len(results) == 4
        finally:
            sched.shutdown()


# ---------------------------------------------------------------------------
# Scheduler shutdown
# ---------------------------------------------------------------------------

class TestSchedulerShutdown:

    def test_shutdown_after_begin_raises(self):
        sched = Scheduler()
        sched.shutdown()
        with pytest.raises(RuntimeError, match='shut down'):
            sched.begin()

    def test_shutdown_drains_pending(self):
        sched = Scheduler(ConcurrencyConfig(mode='parallel', max_workers=2))
        done = threading.Event()

        def slow(x: bytes) -> bytes:
            done.wait(timeout=5)
            return x

        sched.begin()
        t = threading.Thread(target=sched.execute, args=(slow, b'', MethodAccess.WRITE))
        t.start()

        assert sched.pending_count == 1
        done.set()
        t.join(timeout=5)
        sched.shutdown()
        assert sched.pending_count == 0
