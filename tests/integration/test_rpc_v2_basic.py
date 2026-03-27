"""Integration tests for rpc_v2 — SharedClient + ClientPool against standard IPCv3Server.

Verifies that SharedClient is a drop-in replacement for IPCv3Client at the
wire level: same server, same CRM, same results.
"""
from __future__ import annotations

import os
import pickle
import threading
import time

import pytest

import c_two as cc
from c_two.rpc.server import _start
from c_two.rpc_v2 import SharedClient, ClientPool

from tests.fixtures.hello import Hello
from tests.fixtures.ihello import IHello


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_counter = 0
_lock = threading.Lock()


def _unique_region() -> str:
    global _counter
    with _lock:
        _counter += 1
        return f'test_rpcv2_integ_{os.getpid()}_{_counter}'


def _wait_for_server(address: str, timeout: float = 5.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if cc.rpc.Client.ping(address, timeout=0.5):
                return
        except Exception:
            pass
        time.sleep(0.05)
    raise TimeoutError(f'Server at {address} not ready after {timeout}s')


def _wait_for_shutdown(address: str, timeout: float = 5.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if not cc.rpc.Client.ping(address, timeout=0.3):
                return
        except Exception:
            return
        time.sleep(0.05)


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture
def ipc_v3_addr():
    addr = f'ipc-v3://{_unique_region()}'
    config = cc.rpc.ServerConfig(
        name='IntegrationTest',
        crm=Hello(),
        icrm=IHello,
        bind_address=addr,
    )
    server = cc.rpc.Server(config)
    _start(server._state)
    _wait_for_server(addr)

    yield addr

    try:
        cc.rpc.Client.shutdown(addr, timeout=1.0)
    except Exception:
        pass
    _wait_for_shutdown(addr)
    try:
        server.stop()
    except Exception:
        pass


# ---------------------------------------------------------------------------
# Integration tests
# ---------------------------------------------------------------------------

class TestSharedClientMatchesIPCv3Client:
    """Verify SharedClient produces identical results to standard IPCv3Client."""

    def test_greeting_parity(self, ipc_v3_addr):
        """Same call, same result via both client types."""
        # Standard client via connect_crm.
        with cc.compo.runtime.connect_crm(ipc_v3_addr, IHello) as crm:
            expected = crm.greeting('Parity')

        # SharedClient at wire level.
        client = SharedClient(ipc_v3_addr)
        client.connect()
        try:
            result = pickle.loads(client.call('greeting', pickle.dumps(('Parity',))))
            assert result == expected
        finally:
            client.terminate()

    def test_add_parity(self, ipc_v3_addr):
        with cc.compo.runtime.connect_crm(ipc_v3_addr, IHello) as crm:
            expected = crm.add(42, 58)

        client = SharedClient(ipc_v3_addr)
        client.connect()
        try:
            result = pickle.loads(client.call('add', pickle.dumps((42, 58))))
            assert result == expected
        finally:
            client.terminate()

    def test_list_parity(self, ipc_v3_addr):
        with cc.compo.runtime.connect_crm(ipc_v3_addr, IHello) as crm:
            expected = crm.get_items([10, 20, 30])

        client = SharedClient(ipc_v3_addr)
        client.connect()
        try:
            result = pickle.loads(client.call('get_items', pickle.dumps(([10, 20, 30],))))
            assert result == expected
        finally:
            client.terminate()


class TestPoolIntegration:
    """ClientPool with real server — acquire, call, release."""

    def test_pool_acquire_call_release(self, ipc_v3_addr):
        pool = ClientPool(grace_seconds=1.0)
        try:
            client = pool.acquire(ipc_v3_addr)
            result = pickle.loads(client.call('greeting', pickle.dumps(('Pool',))))
            assert result == 'Hello, Pool!'
            pool.release(ipc_v3_addr)
        finally:
            pool.shutdown_all()

    def test_pool_shared_by_multiple_callers(self, ipc_v3_addr):
        """Multiple callers share one client via pool."""
        pool = ClientPool(grace_seconds=5.0)
        errors: list[str] = []

        def caller(tid: int) -> None:
            try:
                client = pool.acquire(ipc_v3_addr)
                try:
                    r = pickle.loads(client.call('add', pickle.dumps((tid, 100))))
                    if r != tid + 100:
                        errors.append(f'T{tid}: expected {tid+100}, got {r}')
                finally:
                    pool.release(ipc_v3_addr)
            except Exception as e:
                errors.append(f'T{tid}: {e}')

        threads = [threading.Thread(target=caller, args=(i,)) for i in range(6)]
        try:
            for t in threads:
                t.start()
            for t in threads:
                t.join(timeout=30)
            assert errors == [], f'Errors: {errors}'
            # All 6 callers shared one client.
            assert pool.active_count() == 1
        finally:
            pool.shutdown_all()


class TestSharedClientHighConcurrency:
    """Stress test: many concurrent calls on one SharedClient."""

    def test_50_concurrent_calls(self, ipc_v3_addr):
        client = SharedClient(ipc_v3_addr)
        client.connect()
        errors: list[str] = []
        n_threads = 10
        n_calls_per_thread = 5

        def worker(tid: int) -> None:
            try:
                for i in range(n_calls_per_thread):
                    a, b = tid * 100 + i, i
                    r = pickle.loads(client.call('add', pickle.dumps((a, b))))
                    if r != a + b:
                        errors.append(f'T{tid}[{i}]: expected {a+b}, got {r}')
            except Exception as e:
                errors.append(f'T{tid}: {e}')

        threads = [threading.Thread(target=worker, args=(i,)) for i in range(n_threads)]
        try:
            for t in threads:
                t.start()
            for t in threads:
                t.join(timeout=60)
            assert errors == [], f'Errors: {errors}'
        finally:
            client.terminate()
