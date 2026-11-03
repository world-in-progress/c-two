"""Concurrency safety and security integration tests.

Tests:
- Thread-safe concurrent calls via SOTA API
- Server double-shutdown safety
- Concurrent connect/close lifecycle
"""
from __future__ import annotations

import os
import time
import threading

import pytest

from tests.fixtures import local_server

import c_two as cc
from c_two.transport.server import Server
from c_two.transport.client.util import ping
from c_two.transport.registry import _get_default_runtime
from c_two.transport.server.scheduler import ConcurrencyConfig


# ---------------------------------------------------------------------------
# Test fixtures
# ---------------------------------------------------------------------------

@cc.crm(namespace='cc.test.concurrency', version='0.1.0')
class IEcho:
    def echo(self, data: bytes) -> bytes: ...
    def add(self, a: int, b: int) -> int: ...

class EchoImpl:
    def echo(self, data: bytes) -> bytes:
        return data
    def add(self, a: int, b: int) -> int:
        return a + b


@cc.crm(namespace='cc.test.concurrency.drain', version='0.1.0')
class IDrain:
    @cc.write
    def hold(self, delay: float) -> float: ...

    @cc.write
    def probe(self) -> int: ...


class DrainImpl:
    def __init__(self):
        self.entered = threading.Event()
        self.release = threading.Event()
        self.calls = 0
        self.lock = threading.Lock()

    def hold(self, delay: float) -> float:
        self.entered.set()
        self.release.wait(timeout=2)
        time.sleep(delay)
        return delay

    def probe(self) -> int:
        with self.lock:
            self.calls += 1
            return self.calls


_counter = 0

def _next_addr():
    global _counter
    _counter += 1
    return f'ipc://test_conc_{os.getpid()}_{_counter}'


def _wait_for_server(addr: str, timeout: float = 3.0):
    """Poll until server responds to ping."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if ping(addr, timeout=0.5):
            return
        time.sleep(0.02)
    raise TimeoutError(f'Server at {addr} not ready within {timeout}s')


# ---------------------------------------------------------------------------
# Concurrent calls stress test via SOTA API
# ---------------------------------------------------------------------------

class TestConcurrentCallSafety:
    """Stress-test concurrent call safety via SOTA API."""

    def test_32_threads_concurrent_echo(self):
        """32 threads making concurrent echo calls via a shared proxy."""
        addr = _next_addr()
        ipc_overrides = {
            'pool_segment_size': 1 << 20,  # 1MB
            'max_pool_segments': 2,
        }
        server = local_server(addr, ipc_overrides=ipc_overrides)
        server.register_crm(IEcho, EchoImpl(), name='echo')
        server.start()
        _wait_for_server(addr)

        try:
            proxy = cc.connect(IEcho, name='echo', address=addr)

            errors = []
            results = {}

            def worker(thread_id: int, calls: int):
                try:
                    for i in range(calls):
                        tag = f'{thread_id}:{i}'.encode()
                        r = proxy.echo(tag)
                        assert r == tag, f'Thread {thread_id} call {i}: expected {tag!r}, got {r!r}'
                        results[(thread_id, i)] = True
                except Exception as e:
                    errors.append((thread_id, e))

            threads = []
            n_threads = 32
            calls_per_thread = 10
            for tid in range(n_threads):
                t = threading.Thread(target=worker, args=(tid, calls_per_thread))
                threads.append(t)
                t.start()
            for t in threads:
                t.join(timeout=15)

            assert not errors, f'Errors in threads: {errors}'
            assert len(results) == n_threads * calls_per_thread
            cc.close(proxy)
        finally:
            server.shutdown()

    def test_concurrent_calls_different_sizes(self):
        """Concurrent calls with varied payload sizes (inline + buddy paths)."""
        addr = _next_addr()
        ipc_overrides = {
            'pool_segment_size': 1 << 20,
            'max_pool_segments': 2,
        }
        server = local_server(addr, ipc_overrides=ipc_overrides)
        server.register_crm(IEcho, EchoImpl(), name='echo')
        server.start()
        _wait_for_server(addr)

        try:
            proxy = cc.connect(IEcho, name='echo', address=addr)

            errors = []
            sizes = [64, 256, 1024, 4096, 16384, 65536]

            def worker(thread_id: int, payload_size: int):
                try:
                    data = bytes(range(256)) * (payload_size // 256 + 1)
                    data = data[:payload_size]
                    for _ in range(5):
                        r = proxy.echo(data)
                        assert r == data
                except Exception as e:
                    errors.append((thread_id, payload_size, e))

            threads = []
            for tid, sz in enumerate(sizes * 2):  # 12 threads
                t = threading.Thread(target=worker, args=(tid, sz))
                threads.append(t)
                t.start()
            for t in threads:
                t.join(timeout=15)

            assert not errors, f'Errors: {errors}'
            cc.close(proxy)
        finally:
            server.shutdown()


# ---------------------------------------------------------------------------
# Double-shutdown safety
# ---------------------------------------------------------------------------

class TestDoubleShutdown:
    """Ensure calling shutdown/close multiple times is safe."""

    def test_proxy_double_close(self):
        addr = _next_addr()
        server = local_server(addr)
        server.register_crm(IEcho, EchoImpl(), name='echo')
        server.start()
        _wait_for_server(addr)

        try:
            proxy = cc.connect(IEcho, name='echo', address=addr)
            r = proxy.echo(b'hello')
            assert r == b'hello'

            # Close twice — should not raise.
            cc.close(proxy)
            cc.close(proxy)
        finally:
            server.shutdown()

    def test_server_double_shutdown(self):
        addr = _next_addr()
        ipc_overrides = {
            'pool_segment_size': 65536,
            'max_pool_segments': 1,
        }
        server = local_server(addr, ipc_overrides=ipc_overrides)
        server.register_crm(IEcho, EchoImpl(), name='echo')
        server.start()
        _wait_for_server(addr)

        # Shutdown twice — should not raise.
        server.shutdown()
        server.shutdown()


class TestSOTAAPIConcurrency:
    """Test the high-level cc.register/connect/close API under concurrency."""

    def test_concurrent_connect_close_cycles(self):
        """Multiple threads rapidly connect/use/close the same CRM."""
        cc.set_server(ipc_overrides={
            'pool_segment_size': 1 << 20,
            'max_pool_segments': 2,
        })
        cc.register(IEcho, EchoImpl(), name='echo_conc')

        try:
            errors = []

            def worker(tid: int):
                try:
                    for _ in range(5):
                        crm = cc.connect(IEcho, name='echo_conc')
                        r = crm.echo(f'tid={tid}'.encode())
                        assert r == f'tid={tid}'.encode()
                        cc.close(crm)
                except Exception as e:
                    errors.append((tid, e))

            threads = []
            for tid in range(8):
                t = threading.Thread(target=worker, args=(tid,))
                threads.append(t)
                t.start()
            for t in threads:
                t.join(timeout=15)

            assert not errors, f'Errors: {errors}'
        finally:
            cc.shutdown()

    def test_connect_unknown_name_raises(self):
        """cc.connect with unknown name raises LookupError."""
        cc.register(IEcho, EchoImpl(), name='echo_lookup')

        try:
            with pytest.raises(LookupError, match='not registered'):
                cc.connect(IEcho, name='nonexistent')
        finally:
            cc.shutdown()

    def test_double_register_same_name_raises(self):
        """Registering the same name twice raises ValueError."""
        cc.register(IEcho, EchoImpl(), name='echo_dup')

        try:
            with pytest.raises(ValueError, match='already registered'):
                cc.register(IEcho, EchoImpl(), name='echo_dup')
        finally:
            cc.shutdown()


class TestUnregisterDrain:
    """Unregister should let in-flight calls drain but reject new ones."""

    def test_unregister_closes_local_and_remote_new_calls(self):
        cc.register(
            IDrain,
            DrainImpl(),
            name='drain_route',
            concurrency=ConcurrencyConfig(
                mode='parallel',
                max_workers=2,
            ),
        )
        addr = cc.server_address()
        assert addr is not None
        local = cc.connect(IDrain, name='drain_route')
        remote = cc.connect(IDrain, name='drain_route', address=addr)
        impl = _get_default_runtime()._server._slots['drain_route'].crm_instance.resource  # noqa: SLF001

        errors: list[BaseException] = []

        def local_hold():
            try:
                assert local.hold(0.01) == 0.01
            except BaseException as exc:
                errors.append(exc)

        t = threading.Thread(target=local_hold)
        t.start()
        assert impl.entered.wait(timeout=1)

        unregister_done = threading.Event()
        unregister_errors: list[BaseException] = []

        def unregister_route():
            try:
                cc.unregister('drain_route')
            except BaseException as exc:
                unregister_errors.append(exc)
            finally:
                unregister_done.set()

        def wait_until_rejected(call):
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                try:
                    call()
                except Exception:
                    return
                time.sleep(0.01)
            pytest.fail('route continued accepting calls after unregister started')

        unregister_thread = threading.Thread(target=unregister_route)
        unregister_thread.start()
        try:
            wait_until_rejected(local.probe)
            wait_until_rejected(remote.probe)
            assert not unregister_done.is_set()

            impl.release.set()
            t.join(timeout=5)
            unregister_thread.join(timeout=5)

            assert not t.is_alive()
            assert not unregister_thread.is_alive()
            assert not errors
            assert not unregister_errors
        finally:
            impl.release.set()
            t.join(timeout=5)
            unregister_thread.join(timeout=5)
