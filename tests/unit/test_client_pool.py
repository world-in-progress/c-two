"""Unit tests for ClientPool — reference-counted SharedClient pool."""
from __future__ import annotations

import os
import pickle
import threading
import time

import pytest

from c_two.transport.client.pool import ClientPool
from c_two.transport.client.core import SharedClient
from c_two.transport.server.core import Server

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
        return f'test_pool_{os.getpid()}_{_counter}'


def _wait_for_server(address: str, timeout: float = 5.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if SharedClient.ping(address, timeout=0.5):
                return
        except Exception:
            pass
        time.sleep(0.05)
    raise TimeoutError(f'Server at {address} not ready after {timeout}s')


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture
def ipc_addr():
    addr = f'ipc://{_unique_region()}'
    server = Server(bind_address=addr, icrm_class=IHello, crm_instance=Hello())
    server.start()
    _wait_for_server(addr)

    yield addr

    server.shutdown()


@pytest.fixture
def pool():
    """Create a fresh ClientPool (not the singleton) with short grace period."""
    p = ClientPool(grace_seconds=0.5)
    yield p
    p.shutdown_all()


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

class TestPoolAcquireRelease:

    def test_acquire_creates_client(self, ipc_addr, pool):
        client = pool.acquire(ipc_addr)
        assert isinstance(client, SharedClient)
        assert pool.active_count() == 1
        assert pool.refcount(ipc_addr) == 1
        pool.release(ipc_addr)

    def test_acquire_reuses_client(self, ipc_addr, pool):
        c1 = pool.acquire(ipc_addr)
        c2 = pool.acquire(ipc_addr)
        assert c1 is c2
        assert pool.refcount(ipc_addr) == 2
        pool.release(ipc_addr)
        pool.release(ipc_addr)

    def test_release_grace_period(self, ipc_addr, pool):
        """After release, client stays alive during grace period."""
        pool.acquire(ipc_addr)
        pool.release(ipc_addr)
        # Still alive immediately after release.
        assert pool.has_client(ipc_addr)

    def test_release_destroys_after_grace(self, ipc_addr, pool):
        """After grace period, client is destroyed."""
        pool.acquire(ipc_addr)
        pool.release(ipc_addr)
        # Wait for grace period (0.5s) plus margin.
        time.sleep(1.0)
        assert not pool.has_client(ipc_addr)

    def test_reacquire_cancels_destroy(self, ipc_addr, pool):
        """Re-acquire during grace period cancels destruction."""
        c1 = pool.acquire(ipc_addr)
        pool.release(ipc_addr)
        time.sleep(0.1)  # Within grace period (0.5s).
        c2 = pool.acquire(ipc_addr)
        assert c1 is c2
        assert pool.refcount(ipc_addr) == 1
        time.sleep(0.7)  # Past original grace, but refcount > 0.
        assert pool.has_client(ipc_addr)
        pool.release(ipc_addr)


class TestPoolConcurrency:

    def test_concurrent_acquire_release(self, ipc_addr, pool):
        """Multiple threads acquiring/releasing the same address."""
        errors: list[str] = []

        def worker(tid: int) -> None:
            try:
                client = pool.acquire(ipc_addr)
                r = pickle.loads(client.call('add', pickle.dumps((tid, 1))))
                if r != tid + 1:
                    errors.append(f'Thread {tid}: expected {tid + 1}, got {r}')
                pool.release(ipc_addr)
            except Exception as e:
                errors.append(f'Thread {tid}: {e}')

        threads = [threading.Thread(target=worker, args=(i,)) for i in range(4)]
        for t in threads:
            t.start()
        for t in threads:
            t.join(timeout=30)

        assert errors == [], f'Concurrency errors: {errors}'


class TestPoolFunctionality:

    def test_call_through_pool(self, ipc_addr, pool):
        """Acquire client from pool and make RPC calls."""
        client = pool.acquire(ipc_addr)
        try:
            r = pickle.loads(client.call('greeting', pickle.dumps(('Pool',))))
            assert r == 'Hello, Pool!'
        finally:
            pool.release(ipc_addr)

    def test_shutdown_all(self, ipc_addr, pool):
        """shutdown_all terminates all clients immediately."""
        pool.acquire(ipc_addr)
        pool.release(ipc_addr)
        pool.shutdown_all()
        assert pool.active_count() == 0
