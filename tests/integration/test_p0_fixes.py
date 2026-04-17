"""Integration tests for P0 fixes: thread-local concurrency + @cc.on_shutdown."""
from __future__ import annotations

import threading
import time

import pytest

import c_two as cc
from c_two.transport.server.scheduler import ConcurrencyConfig, ConcurrencyMode


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture(autouse=True)
def _cleanup():
    """Ensure clean registry state before and after each test."""
    yield
    try:
        cc.shutdown()
    except Exception:
        pass


# ---------------------------------------------------------------------------
# CRM + CRM definitions
# ---------------------------------------------------------------------------

@cc.crm(namespace='test.p0', version='0.1.0')
class Counter:
    @cc.read
    def get_value(self) -> int: ...

    @cc.write
    def increment(self) -> int: ...

    @cc.on_shutdown
    def cleanup(self): ...


class CounterImpl:
    def __init__(self):
        self.value = 0
        self.cleaned_up = False

    def get_value(self) -> int:
        return self.value

    def increment(self) -> int:
        self.value += 1
        return self.value

    def cleanup(self):
        self.cleaned_up = True


@cc.crm(namespace='test.p0.noclean', version='0.1.0')
class Simple:
    @cc.read
    def read_data(self) -> str: ...


class SimpleImpl:
    def read_data(self) -> str:
        return 'data'


# ---------------------------------------------------------------------------
# §1.1 Thread-local concurrency control
# ---------------------------------------------------------------------------

class TestThreadLocalConcurrency:

    def test_connect_returns_guarded_proxy(self):
        """cc.connect() thread-local path should return a proxy with scheduler."""
        crm = CounterImpl()
        cc.register(Counter, crm, name='ctr')
        crm = cc.connect(Counter, name='ctr')

        # The proxy should have scheduler set
        assert crm.client._scheduler is not None
        assert crm.client._access_map is not None
        assert 'get_value' in crm.client._access_map
        assert 'increment' in crm.client._access_map

        cc.close(crm)

    def test_thread_local_rw_isolation(self):
        """Concurrent reads and writes should be properly isolated."""
        crm = CounterImpl()
        cc.register(
            Counter, crm, name='ctr_iso',
            concurrency=ConcurrencyConfig(mode=ConcurrencyMode.EXCLUSIVE),
        )

        errors = []
        def do_increments(n):
            try:
                crm = cc.connect(Counter, name='ctr_iso')
                for _ in range(n):
                    crm.increment()
                cc.close(crm)
            except Exception as e:
                errors.append(e)

        threads = [threading.Thread(target=do_increments, args=(100,)) for _ in range(4)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        assert not errors, f'Errors: {errors}'
        # With EXCLUSIVE, all increments are serialized → counter = 400
        assert crm.value == 400

    def test_read_parallel_allows_concurrent_reads(self):
        """READ_PARALLEL mode allows concurrent reads."""
        @cc.crm(namespace='test.p0.slow', version='0.1.0')
        class ISlowReader:
            @cc.read
            def slow_read(self) -> float: ...

        class SlowReader:
            def slow_read(self) -> float:
                start = time.monotonic()
                time.sleep(0.05)
                return start

        crm = SlowReader()
        cc.register(
            ISlowReader, crm, name='slow',
            concurrency=ConcurrencyConfig(mode=ConcurrencyMode.READ_PARALLEL),
        )

        results = [None, None]
        def do_read(idx):
            crm = cc.connect(ISlowReader, name='slow')
            results[idx] = crm.slow_read()
            cc.close(crm)

        t1 = threading.Thread(target=do_read, args=(0,))
        t2 = threading.Thread(target=do_read, args=(1,))
        t1.start(); t2.start()
        t1.join(); t2.join()

        # Both reads started at roughly the same time (overlapping)
        assert results[0] is not None
        assert results[1] is not None
        # Time difference between start times should be small (< 0.04s)
        assert abs(results[0] - results[1]) < 0.04, (
            'Concurrent reads should overlap in READ_PARALLEL mode'
        )


# ---------------------------------------------------------------------------
# §1.2 @cc.on_shutdown lifecycle
# ---------------------------------------------------------------------------

class TestOnShutdownLifecycle:

    def test_unregister_invokes_shutdown(self):
        """cc.unregister() should call @cc.on_shutdown method."""
        crm = CounterImpl()
        cc.register(Counter, crm, name='ctr_sd')

        assert not crm.cleaned_up
        cc.unregister('ctr_sd')
        assert crm.cleaned_up

    def test_shutdown_invokes_all(self):
        """cc.shutdown() should call @cc.on_shutdown on all CRMs."""
        crm1 = CounterImpl()
        crm2 = CounterImpl()
        cc.register(Counter, crm1, name='ctr1')
        cc.register(Counter, crm2, name='ctr2')

        assert not crm1.cleaned_up
        assert not crm2.cleaned_up
        cc.shutdown()
        assert crm1.cleaned_up
        assert crm2.cleaned_up

    def test_no_shutdown_method_still_works(self):
        """CRM without @cc.on_shutdown should register/unregister fine."""
        crm = SimpleImpl()
        cc.register(Simple, crm, name='simple')
        cc.unregister('simple')

    def test_shutdown_exception_does_not_crash(self):
        """Exception in @cc.on_shutdown should be caught."""
        @cc.crm(namespace='test.p0.err', version='0.1.0')
        class IFailing:
            @cc.on_shutdown
            def cleanup(self): ...

        class Failing:
            def cleanup(self):
                raise RuntimeError('Cleanup failed!')

        crm = Failing()
        cc.register(IFailing, crm, name='failing')
        # Should not raise — exception is caught and logged
        cc.unregister('failing')

    def test_shutdown_method_not_rpc_callable(self):
        """@cc.on_shutdown methods should not appear in dispatch table."""
        crm = CounterImpl()
        cc.register(Counter, crm, name='ctr_rpc')

        from c_two.transport.registry import _ProcessRegistry
        reg = _ProcessRegistry._instance
        slot = reg._server._slots.get('ctr_rpc')
        assert slot is not None
        # cleanup should NOT be in the dispatch table
        assert 'cleanup' not in slot._dispatch_table
        # But regular methods should be
        assert 'get_value' in slot._dispatch_table
        assert 'increment' in slot._dispatch_table
