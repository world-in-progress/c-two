"""Remote IPC scheduler configuration is enforced by Rust c2-server."""
from __future__ import annotations

import threading
import time
from dataclasses import dataclass

import pytest

import c_two as cc
from c_two.config import settings
from c_two.transport.registry import _ProcessRegistry
from c_two.transport.server.scheduler import ConcurrencyConfig, ConcurrencyMode


@pytest.fixture(autouse=True)
def _clean_env_and_registry(monkeypatch):
    previous_override_relay = settings._relay_anchor_address  # noqa: SLF001
    previous_override_shm_threshold = settings._shm_threshold  # noqa: SLF001
    settings.relay_anchor_address = None
    monkeypatch.delenv('C2_RELAY_ANCHOR_ADDRESS', raising=False)
    yield
    try:
        cc.shutdown()
    except Exception:
        pass
    _ProcessRegistry._instance = None  # noqa: SLF001
    settings._relay_anchor_address = previous_override_relay  # noqa: SLF001
    settings._shm_threshold = previous_override_shm_threshold  # noqa: SLF001


def _direct_client(crm_type: type, name: str):
    address = cc.server_address()
    assert address is not None
    assert address.startswith('ipc://')
    return cc.connect(crm_type, name=name, address=address)


def _join_all(threads: list[threading.Thread]) -> None:
    for thread in threads:
        thread.join(timeout=5)
    alive = [thread.name for thread in threads if thread.is_alive()]
    assert not alive, f'threads did not finish: {alive}'


@dataclass
class Delay:
    value: float


@dataclass
class Window:
    start: float
    end: float


@dataclass
class Count:
    value: int


@cc.crm(namespace='test.remote.scheduler', version='0.1.0')
class RemoteSchedulerProbe:
    @cc.read
    def read_op(self, delay: Delay) -> Window: ...

    @cc.write
    def write_op(self, delay: Delay) -> Window: ...

    @cc.write
    def max_active(self) -> Count: ...


class RemoteSchedulerProbeImpl:
    def __init__(self):
        self.active = 0
        self.max_seen = 0
        self.lock = threading.Lock()

    def _run(self, delay: Delay) -> Window:
        start = time.monotonic()
        with self.lock:
            self.active += 1
            self.max_seen = max(self.max_seen, self.active)
        try:
            time.sleep(delay.value)
            return Window(start, time.monotonic())
        finally:
            with self.lock:
                self.active -= 1

    def read_op(self, delay: Delay) -> Window:
        return self._run(delay)

    def write_op(self, delay: Delay) -> Window:
        return self._run(delay)

    def max_active(self) -> Count:
        return Count(self.max_seen)


@cc.crm(namespace='test.remote.scheduler.shared', version='0.1.0')
class SharedLimitProbe:
    @cc.write
    def hold(self, delay: Delay) -> Window: ...

    @cc.write
    def probe(self) -> Count: ...


class SharedLimitProbeImpl:
    def __init__(self):
        self.hold_entered = threading.Event()
        self.release = threading.Event()
        self.probe_calls = 0
        self.lock = threading.Lock()

    def hold(self, delay: Delay) -> Window:
        start = time.monotonic()
        self.hold_entered.set()
        self.release.wait(timeout=2)
        time.sleep(delay.value)
        return Window(start, time.monotonic())

    def probe(self) -> Count:
        with self.lock:
            self.probe_calls += 1
            return Count(self.probe_calls)


def test_remote_ipc_exclusive_serializes_reads():
    cc.register(
        RemoteSchedulerProbe,
        RemoteSchedulerProbeImpl(),
        name='remote_exclusive',
        concurrency=ConcurrencyConfig(mode=ConcurrencyMode.EXCLUSIVE),
    )
    cc.serve(blocking=False)

    errors: list[BaseException] = []

    def worker():
        client = _direct_client(RemoteSchedulerProbe, 'remote_exclusive')
        try:
            client.read_op(Delay(0.05))
        except BaseException as exc:
            errors.append(exc)
        finally:
            cc.close(client)

    threads = [threading.Thread(target=worker) for _ in range(3)]
    for thread in threads:
        thread.start()
    _join_all(threads)

    assert not errors
    probe = _direct_client(RemoteSchedulerProbe, 'remote_exclusive')
    try:
        assert probe.max_active().value == 1
    finally:
        cc.close(probe)


def test_remote_ipc_parallel_allows_concurrent_writes():
    cc.register(
        RemoteSchedulerProbe,
        RemoteSchedulerProbeImpl(),
        name='remote_parallel',
        concurrency=ConcurrencyConfig(mode=ConcurrencyMode.PARALLEL),
    )
    cc.serve(blocking=False)

    errors: list[BaseException] = []

    def worker():
        client = _direct_client(RemoteSchedulerProbe, 'remote_parallel')
        try:
            client.write_op(Delay(0.08))
        except BaseException as exc:
            errors.append(exc)
        finally:
            cc.close(client)

    threads = [threading.Thread(target=worker) for _ in range(3)]
    for thread in threads:
        thread.start()
    _join_all(threads)

    assert not errors
    probe = _direct_client(RemoteSchedulerProbe, 'remote_parallel')
    try:
        assert probe.max_active().value >= 2
    finally:
        cc.close(probe)


def test_remote_ipc_read_parallel_allows_concurrent_reads():
    cc.register(
        RemoteSchedulerProbe,
        RemoteSchedulerProbeImpl(),
        name='remote_read_parallel_reads',
        concurrency=ConcurrencyConfig(mode=ConcurrencyMode.READ_PARALLEL),
    )
    cc.serve(blocking=False)

    read_errors: list[BaseException] = []

    def read_worker():
        client = _direct_client(RemoteSchedulerProbe, 'remote_read_parallel_reads')
        try:
            client.read_op(Delay(0.05))
        except BaseException as exc:
            read_errors.append(exc)
        finally:
            cc.close(client)

    readers = [threading.Thread(target=read_worker) for _ in range(3)]
    for thread in readers:
        thread.start()
    _join_all(readers)

    assert not read_errors
    client = _direct_client(RemoteSchedulerProbe, 'remote_read_parallel_reads')
    try:
        assert client.max_active().value >= 2
    finally:
        cc.close(client)


def test_remote_ipc_read_parallel_serializes_writes():
    cc.register(
        RemoteSchedulerProbe,
        RemoteSchedulerProbeImpl(),
        name='remote_read_parallel_writes',
        concurrency=ConcurrencyConfig(mode=ConcurrencyMode.READ_PARALLEL),
    )

    write_errors: list[BaseException] = []

    def write_worker():
        client = _direct_client(RemoteSchedulerProbe, 'remote_read_parallel_writes')
        try:
            client.write_op(Delay(0.05))
        except BaseException as exc:
            write_errors.append(exc)
        finally:
            cc.close(client)

    writers = [threading.Thread(target=write_worker) for _ in range(3)]
    for thread in writers:
        thread.start()
    _join_all(writers)

    assert not write_errors
    client = _direct_client(RemoteSchedulerProbe, 'remote_read_parallel_writes')
    try:
        assert client.max_active().value == 1
    finally:
        cc.close(client)


@pytest.mark.parametrize(
    'mode',
    [ConcurrencyMode.PARALLEL, ConcurrencyMode.EXCLUSIVE, ConcurrencyMode.READ_PARALLEL],
)
def test_remote_ipc_registers_all_public_concurrency_modes(mode):
    name = f'remote_mode_{mode.value}'
    cc.register(
        RemoteSchedulerProbe,
        RemoteSchedulerProbeImpl(),
        name=name,
        concurrency=ConcurrencyConfig(mode=mode),
    )
    cc.serve(blocking=False)
    client = _direct_client(RemoteSchedulerProbe, name)
    try:
        assert client.max_active().value == 0
    finally:
        cc.close(client)


def test_remote_ipc_scheduler_direct_address_ignores_bad_relay(monkeypatch):
    cc.register(
        RemoteSchedulerProbe,
        RemoteSchedulerProbeImpl(),
        name='direct_no_relay_scheduler',
        concurrency=ConcurrencyConfig(mode=ConcurrencyMode.EXCLUSIVE),
    )
    cc.serve(blocking=False)
    direct_address = cc.server_address()
    assert direct_address is not None
    assert direct_address.startswith('ipc://')

    monkeypatch.setenv('C2_RELAY_ANCHOR_ADDRESS', 'http://127.0.0.1:9')
    settings.relay_anchor_address = None

    client = cc.connect(
        RemoteSchedulerProbe,
        name='direct_no_relay_scheduler',
        address=direct_address,
    )
    try:
        client.write_op(Delay(0.001))
        assert client.max_active().value == 1
    finally:
        cc.close(client)


def test_remote_ipc_scheduler_max_workers_shares_with_local_thread_calls():
    cc.register(
        SharedLimitProbe,
        SharedLimitProbeImpl(),
        name='mixed_limit_workers',
        concurrency=ConcurrencyConfig(
            mode=ConcurrencyMode.PARALLEL,
            max_workers=1,
        ),
    )
    cc.serve(blocking=False)
    direct_address = cc.server_address()
    assert direct_address is not None

    resource = _ProcessRegistry.get()._server._slots['mixed_limit_workers'].crm_instance.resource  # noqa: SLF001
    local = cc.connect(SharedLimitProbe, name='mixed_limit_workers')
    remote = cc.connect(
        SharedLimitProbe,
        name='mixed_limit_workers',
        address=direct_address,
    )
    try:
        errors: list[BaseException] = []
        local_result: list[Window] = []
        remote_result: list[Count] = []

        def local_worker():
            try:
                local_result.append(local.hold(Delay(0.01)))
            except BaseException as exc:
                errors.append(exc)

        def remote_worker():
            try:
                remote_result.append(remote.probe())
            except BaseException as exc:
                errors.append(exc)

        t1 = threading.Thread(target=local_worker)
        t1.start()
        assert resource.hold_entered.wait(timeout=1)

        t2 = threading.Thread(target=remote_worker)
        t2.start()
        time.sleep(0.1)

        assert t2.is_alive(), 'remote call should wait for shared worker capacity'
        assert resource.probe_calls == 0
        assert not errors
        resource.release.set()
        _join_all([t1, t2])
        assert local_result and local_result[0].end >= local_result[0].start
        assert remote_result and remote_result[0].value == 1
    finally:
        cc.close(local)
        cc.close(remote)


def test_remote_ipc_scheduler_max_pending_shares_with_local_thread_calls():
    cc.register(
        SharedLimitProbe,
        SharedLimitProbeImpl(),
        name='mixed_limit_pending',
        concurrency=ConcurrencyConfig(
            mode=ConcurrencyMode.PARALLEL,
            max_pending=1,
        ),
    )
    cc.serve(blocking=False)
    direct_address = cc.server_address()
    assert direct_address is not None

    resource = _ProcessRegistry.get()._server._slots['mixed_limit_pending'].crm_instance.resource  # noqa: SLF001
    local = cc.connect(SharedLimitProbe, name='mixed_limit_pending')
    remote = cc.connect(
        SharedLimitProbe,
        name='mixed_limit_pending',
        address=direct_address,
    )
    try:
        errors: list[BaseException] = []

        def local_worker():
            try:
                local.hold(Delay(0.01))
            except BaseException as exc:
                errors.append(exc)

        t1 = threading.Thread(target=local_worker)
        t1.start()
        assert resource.hold_entered.wait(timeout=1)

        with pytest.raises(Exception, match='max_pending=1'):
            remote.probe()

        assert resource.probe_calls == 0
        assert not errors
        resource.release.set()
        _join_all([t1])
    finally:
        cc.close(local)
        cc.close(remote)
