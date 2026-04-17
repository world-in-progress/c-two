"""Unit tests for thread-local proxy concurrency control (P0 §1.1)."""
from __future__ import annotations

import threading
import time

import pytest

from c_two.crm.meta import MethodAccess
from c_two.transport.client.proxy import CRMProxy
from c_two.transport.server.scheduler import Scheduler, ConcurrencyConfig, ConcurrencyMode


# ---------------------------------------------------------------------------
# Slow CRM for concurrency testing
# ---------------------------------------------------------------------------

class _SlowCRM:
    """CRM with methods that sleep to expose concurrency issues."""

    def __init__(self):
        self.call_log: list[tuple[str, float, float]] = []
        self._lock = threading.Lock()

    def read_op(self) -> str:
        start = time.monotonic()
        time.sleep(0.05)
        end = time.monotonic()
        with self._lock:
            self.call_log.append(('read', start, end))
        return 'read_result'

    def write_op(self, data: str = '') -> str:
        start = time.monotonic()
        time.sleep(0.05)
        end = time.monotonic()
        with self._lock:
            self.call_log.append(('write', start, end))
        return 'write_result'


_READ_WRITE_MAP = {
    'read_op': MethodAccess.READ,
    'write_op': MethodAccess.WRITE,
}


def _overlaps(a_start, a_end, b_start, b_end) -> bool:
    """Return True if time intervals [a_start, a_end] and [b_start, b_end] overlap."""
    return a_start < b_end and b_start < a_end


# ---------------------------------------------------------------------------
# Backward compatibility: no scheduler
# ---------------------------------------------------------------------------

class TestThreadLocalNoScheduler:

    def test_call_direct_no_scheduler(self):
        """Without scheduler, call_direct works as before (no locking)."""
        crm = _SlowCRM()
        proxy = CRMProxy.thread_local(crm)
        assert proxy.call_direct('read_op', ()) == 'read_result'
        assert proxy.call_direct('write_op', ('hello',)) == 'write_result'

    def test_supports_direct_call(self):
        proxy = CRMProxy.thread_local(_SlowCRM())
        assert proxy.supports_direct_call is True


# ---------------------------------------------------------------------------
# EXCLUSIVE mode: all calls serialized
# ---------------------------------------------------------------------------

class TestExclusiveMode:

    def test_writes_serialized(self):
        """Two concurrent writes should not overlap in EXCLUSIVE mode."""
        crm = _SlowCRM()
        sched = Scheduler(ConcurrencyConfig(mode=ConcurrencyMode.EXCLUSIVE))
        proxy = CRMProxy.thread_local(
            crm, scheduler=sched, access_map=_READ_WRITE_MAP,
        )

        results = [None, None]
        def do_write(idx):
            results[idx] = proxy.call_direct('write_op', (f'w{idx}',))

        t1 = threading.Thread(target=do_write, args=(0,))
        t2 = threading.Thread(target=do_write, args=(1,))
        t1.start(); t2.start()
        t1.join(); t2.join()

        assert results == ['write_result', 'write_result']
        assert len(crm.call_log) == 2
        # Calls should NOT overlap
        (_, s1, e1), (_, s2, e2) = crm.call_log
        assert not _overlaps(s1, e1, s2, e2), 'EXCLUSIVE writes must not overlap'

    def test_read_write_serialized(self):
        """Read and write should not overlap in EXCLUSIVE mode."""
        crm = _SlowCRM()
        sched = Scheduler(ConcurrencyConfig(mode=ConcurrencyMode.EXCLUSIVE))
        proxy = CRMProxy.thread_local(
            crm, scheduler=sched, access_map=_READ_WRITE_MAP,
        )

        def do_read():
            proxy.call_direct('read_op', ())

        def do_write():
            proxy.call_direct('write_op', ('',))

        t1 = threading.Thread(target=do_read)
        t2 = threading.Thread(target=do_write)
        t1.start(); t2.start()
        t1.join(); t2.join()

        assert len(crm.call_log) == 2
        (_, s1, e1), (_, s2, e2) = crm.call_log
        assert not _overlaps(s1, e1, s2, e2), 'EXCLUSIVE read+write must not overlap'


# ---------------------------------------------------------------------------
# READ_PARALLEL mode: reads concurrent, writes exclusive
# ---------------------------------------------------------------------------

class TestReadParallelMode:

    def test_reads_concurrent(self):
        """Two reads should overlap in READ_PARALLEL mode."""
        crm = _SlowCRM()
        sched = Scheduler(ConcurrencyConfig(mode=ConcurrencyMode.READ_PARALLEL))
        proxy = CRMProxy.thread_local(
            crm, scheduler=sched, access_map=_READ_WRITE_MAP,
        )

        def do_read():
            proxy.call_direct('read_op', ())

        t1 = threading.Thread(target=do_read)
        t2 = threading.Thread(target=do_read)
        t1.start(); t2.start()
        t1.join(); t2.join()

        assert len(crm.call_log) == 2
        (_, s1, e1), (_, s2, e2) = crm.call_log
        assert _overlaps(s1, e1, s2, e2), 'READ_PARALLEL reads should overlap'

    def test_write_exclusive(self):
        """Write blocks reads in READ_PARALLEL mode."""
        crm = _SlowCRM()
        sched = Scheduler(ConcurrencyConfig(mode=ConcurrencyMode.READ_PARALLEL))
        proxy = CRMProxy.thread_local(
            crm, scheduler=sched, access_map=_READ_WRITE_MAP,
        )

        def do_write():
            proxy.call_direct('write_op', ('',))

        t1 = threading.Thread(target=do_write)
        t2 = threading.Thread(target=do_write)
        t1.start(); t2.start()
        t1.join(); t2.join()

        assert len(crm.call_log) == 2
        (_, s1, e1), (_, s2, e2) = crm.call_log
        assert not _overlaps(s1, e1, s2, e2), 'READ_PARALLEL writes must not overlap'


# ---------------------------------------------------------------------------
# PARALLEL mode: no locking
# ---------------------------------------------------------------------------

class TestParallelMode:

    def test_all_concurrent(self):
        """All calls should overlap in PARALLEL mode."""
        crm = _SlowCRM()
        sched = Scheduler(ConcurrencyConfig(mode=ConcurrencyMode.PARALLEL))
        proxy = CRMProxy.thread_local(
            crm, scheduler=sched, access_map=_READ_WRITE_MAP,
        )

        def do_write():
            proxy.call_direct('write_op', ('',))

        t1 = threading.Thread(target=do_write)
        t2 = threading.Thread(target=do_write)
        t1.start(); t2.start()
        t1.join(); t2.join()

        assert len(crm.call_log) == 2
        (_, s1, e1), (_, s2, e2) = crm.call_log
        assert _overlaps(s1, e1, s2, e2), 'PARALLEL writes should overlap'


# ---------------------------------------------------------------------------
# Default access (no access_map entry → WRITE)
# ---------------------------------------------------------------------------

class TestDefaultAccess:

    def test_unknown_method_defaults_to_write(self):
        """Methods not in access_map default to WRITE (EXCLUSIVE locks)."""
        crm = _SlowCRM()
        sched = Scheduler(ConcurrencyConfig(mode=ConcurrencyMode.EXCLUSIVE))
        # Empty access_map — all methods default to WRITE
        proxy = CRMProxy.thread_local(crm, scheduler=sched, access_map={})

        results = []
        def do_write(idx):
            results.append(proxy.call_direct('write_op', (f'w{idx}',)))

        t1 = threading.Thread(target=do_write, args=(0,))
        t2 = threading.Thread(target=do_write, args=(1,))
        t1.start(); t2.start()
        t1.join(); t2.join()

        # Both should succeed and be serialized
        assert len(results) == 2
        (_, s1, e1), (_, s2, e2) = crm.call_log
        assert not _overlaps(s1, e1, s2, e2)
