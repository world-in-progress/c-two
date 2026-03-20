import queue
import threading
import time

import pytest

import c_two as cc
from c_two.rpc import ConcurrencyConfig, ConcurrencyMode, ServerConfig
from c_two.rpc.server import _start

pytestmark = pytest.mark.timeout(20)

_counter = 0
_counter_lock = threading.Lock()


def _next_id() -> int:
    global _counter
    with _counter_lock:
        _counter += 1
        return _counter


@cc.icrm(namespace='test.concurrent', version='0.1.0')
class IConcurrencyProbe:
    @cc.read
    def read(self, name: str, delay: float) -> str:
        ...

    @cc.write
    def write(self, name: str, delay: float) -> str:
        ...


class ConcurrencyProbe:
    def __init__(self):
        self._lock = threading.Lock()
        self._markers: dict[str, threading.Event] = {}
        self.events: list[str] = []
        self.active_reads = 0
        self.max_active_reads = 0

    def _mark_locked(self, marker: str) -> None:
        self.events.append(marker)
        marker_event = self._markers.get(marker)
        if marker_event is None:
            marker_event = threading.Event()
            self._markers[marker] = marker_event
        marker_event.set()

    def wait_for_marker(self, marker: str, timeout: float = 2.0) -> bool:
        with self._lock:
            marker_event = self._markers.get(marker)
            if marker_event is None:
                marker_event = threading.Event()
                self._markers[marker] = marker_event
        return marker_event.wait(timeout)

    def read(self, name: str, delay: float) -> str:
        with self._lock:
            self.active_reads += 1
            self.max_active_reads = max(self.max_active_reads, self.active_reads)
            self._mark_locked(f'start:{name}')

        try:
            time.sleep(delay)
            return name
        finally:
            with self._lock:
                self._mark_locked(f'end:{name}')
                self.active_reads -= 1

    def write(self, name: str, delay: float) -> str:
        with self._lock:
            self._mark_locked(f'start:{name}')

        try:
            time.sleep(delay)
            return name
        finally:
            with self._lock:
                self._mark_locked(f'end:{name}')


@pytest.fixture
def concurrent_server():
    address = f'thread://concurrency_probe_{_next_id()}'
    crm = ConcurrencyProbe()
    server = cc.rpc.Server(ServerConfig(
        name='ConcurrencyProbe',
        crm=crm,
        icrm=IConcurrencyProbe,
        bind_address=address,
        concurrency=ConcurrencyConfig(mode=ConcurrencyMode.READ_PARALLEL, max_workers=4),
    ))

    _start(server._state)

    for _ in range(50):
        try:
            if cc.rpc.Client.ping(address, timeout=0.5):
                break
        except Exception:
            pass
        time.sleep(0.1)

    yield address, crm

    try:
        cc.rpc.Client.shutdown(address, timeout=2.0)
    except Exception:
        pass
    time.sleep(0.1)
    try:
        server.stop()
    except Exception:
        pass


def _read_call(address: str, name: str, delay: float) -> str:
    with cc.compo.runtime.connect_crm(address, IConcurrencyProbe) as crm:
        return crm.read(name, delay)


def _write_call(address: str, name: str, delay: float) -> str:
    with cc.compo.runtime.connect_crm(address, IConcurrencyProbe) as crm:
        return crm.write(name, delay)


class TestReadParallelScheduler:
    def test_reads_can_overlap(self, concurrent_server):
        address, crm = concurrent_server
        start_barrier = threading.Barrier(3)
        results: queue.Queue[str] = queue.Queue()
        errors: queue.Queue[Exception] = queue.Queue()

        def worker(name: str):
            start_barrier.wait()
            try:
                results.put(_read_call(address, name, 0.2))
            except Exception as exc:
                errors.put(exc)

        threads = [
            threading.Thread(target=worker, args=('r1',)),
            threading.Thread(target=worker, args=('r2',)),
        ]

        for thread in threads:
            thread.start()

        start_barrier.wait()

        for thread in threads:
            thread.join()

        assert errors.empty()
        assert sorted([results.get_nowait(), results.get_nowait()]) == ['r1', 'r2']
        assert crm.max_active_reads >= 2

    def test_writer_priority_blocks_new_readers(self, concurrent_server):
        address, crm = concurrent_server
        errors: queue.Queue[Exception] = queue.Queue()
        write_call_started = threading.Event()

        def read_worker(name: str, delay: float):
            try:
                _read_call(address, name, delay)
            except Exception as exc:
                errors.put(exc)

        def write_worker():
            write_call_started.set()
            try:
                _write_call(address, 'w1', 0.05)
            except Exception as exc:
                errors.put(exc)

        t1 = threading.Thread(target=read_worker, args=('r1', 0.2))
        t2 = threading.Thread(target=write_worker)
        t3 = threading.Thread(target=read_worker, args=('r2', 0.01))

        t1.start()
        assert crm.wait_for_marker('start:r1')

        t2.start()
        assert write_call_started.wait(1.0)
        time.sleep(0.05)

        t3.start()

        for thread in (t1, t2, t3):
            thread.join()

        assert errors.empty()

        assert crm.events.index('start:r1') < crm.events.index('end:r1')
        assert crm.events.index('end:r1') < crm.events.index('start:w1')
        assert crm.events.index('start:w1') < crm.events.index('end:w1')
        assert crm.events.index('end:w1') < crm.events.index('start:r2')


class TestBackpressure:
    def test_rejects_when_at_capacity(self):
        """When max_pending is exceeded, new calls receive an error reply."""
        address = f'thread://backpressure_{_next_id()}'
        crm = ConcurrencyProbe()
        server = cc.rpc.Server(ServerConfig(
            name='BackpressureProbe',
            crm=crm,
            icrm=IConcurrencyProbe,
            bind_address=address,
            concurrency=ConcurrencyConfig(
                mode=ConcurrencyMode.READ_PARALLEL,
                max_workers=4,
                max_pending=2,
            ),
        ))
        _start(server._state)

        for _ in range(50):
            try:
                if cc.rpc.Client.ping(address, timeout=0.5):
                    break
            except Exception:
                pass
            time.sleep(0.1)

        results: queue.Queue[tuple[str, str | Exception]] = queue.Queue()

        def slow_read(name: str):
            try:
                result = _read_call(address, name, 0.5)
                results.put(('ok', result))
            except Exception as exc:
                results.put(('error', exc))

        t1 = threading.Thread(target=slow_read, args=('fill1',))
        t2 = threading.Thread(target=slow_read, args=('fill2',))
        t1.start()
        t2.start()

        # Wait for both reads to be actively executing
        assert crm.wait_for_marker('start:fill1', timeout=3.0)
        assert crm.wait_for_marker('start:fill2', timeout=3.0)

        # Third call should be rejected (2 pending, max_pending=2)
        try:
            overflow_result = _read_call(address, 'overflow', 0.01)
            got_error = False
        except Exception:
            got_error = True

        t1.join()
        t2.join()

        assert got_error, 'Expected backpressure rejection for overflow call'

        try:
            cc.rpc.Client.shutdown(address, timeout=2.0)
        except Exception:
            pass
        time.sleep(0.1)
        try:
            server.stop()
        except Exception:
            pass

    def test_accepts_after_drain(self):
        """After pending tasks complete, new calls are accepted again."""
        address = f'thread://backpressure_drain_{_next_id()}'
        crm = ConcurrencyProbe()
        server = cc.rpc.Server(ServerConfig(
            name='BackpressureDrain',
            crm=crm,
            icrm=IConcurrencyProbe,
            bind_address=address,
            concurrency=ConcurrencyConfig(
                mode=ConcurrencyMode.READ_PARALLEL,
                max_workers=4,
                max_pending=1,
            ),
        ))
        _start(server._state)

        for _ in range(50):
            try:
                if cc.rpc.Client.ping(address, timeout=0.5):
                    break
            except Exception:
                pass
            time.sleep(0.1)

        # Fill to capacity and let it complete
        result1 = _read_call(address, 'first', 0.01)
        assert result1 == 'first'

        # Should succeed because previous task drained
        result2 = _read_call(address, 'second', 0.01)
        assert result2 == 'second'

        try:
            cc.rpc.Client.shutdown(address, timeout=2.0)
        except Exception:
            pass
        time.sleep(0.1)
        try:
            server.stop()
        except Exception:
            pass
