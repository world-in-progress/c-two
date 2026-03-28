"""Integration tests for rpc_v2 Phase 2 — ServerV2 + SharedClient.

Tests both backward-compatible (v1 mode: SharedClient → IPCv3Server) and
v2 mode (SharedClient(try_v2=True) → ServerV2) paths.
"""
from __future__ import annotations

import os
import pickle
import threading
import time

import pytest

import c_two as cc
from c_two.transport.ipc.frame import IPCConfig
from c_two.transport import SharedClient, ClientPool, ServerV2

from tests.fixtures.hello import Hello
from tests.fixtures.ihello import IHello


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_counter = 0
_lock = threading.Lock()


def _unique_region(prefix: str = 'test_p2') -> str:
    global _counter
    with _lock:
        _counter += 1
        return f'{prefix}_{os.getpid()}_{_counter}'


def _wait_for_server(address: str, timeout: float = 5.0) -> None:
    """Poll until the server responds to ping."""
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
def server_v2_addr():
    """Start a ServerV2 hosting the Hello CRM + IHello ICRM."""
    addr = f'ipc-v3://{_unique_region()}'
    server = ServerV2(
        bind_address=addr,
        icrm_class=IHello,
        crm_instance=Hello(),
    )
    server.start()
    _wait_for_server(addr)
    yield addr
    server.shutdown()


@pytest.fixture
def server_v2_small_shm():
    """ServerV2 with small SHM threshold (forces inline path more often)."""
    addr = f'ipc-v3://{_unique_region("small")}'
    config = IPCConfig(shm_threshold=16)
    server = ServerV2(
        bind_address=addr,
        icrm_class=IHello,
        crm_instance=Hello(),
        ipc_config=config,
    )
    server.start()
    _wait_for_server(addr)
    yield addr
    server.shutdown()


# ---------------------------------------------------------------------------
# V1 mode: SharedClient (v4 handshake) → ServerV2
# ---------------------------------------------------------------------------

class TestServerV2BackwardCompat:
    """ServerV2 must handle v1 wire frames from a v4-handshake client."""

    def test_greeting_v1(self, server_v2_addr):
        client = SharedClient(server_v2_addr)
        client.connect()
        try:
            result = pickle.loads(client.call('greeting', pickle.dumps(('World',))))
            assert result == 'Hello, World!'
        finally:
            client.terminate()

    def test_add_v1(self, server_v2_addr):
        client = SharedClient(server_v2_addr)
        client.connect()
        try:
            result = pickle.loads(client.call('add', pickle.dumps((7, 8))))
            assert result == 15
        finally:
            client.terminate()

    def test_list_v1(self, server_v2_addr):
        client = SharedClient(server_v2_addr)
        client.connect()
        try:
            result = pickle.loads(
                client.call('get_items', pickle.dumps(([1, 2, 3],)))
            )
            assert result == ['item-1', 'item-2', 'item-3']
        finally:
            client.terminate()

    def test_ping_v1(self, server_v2_addr):
        assert SharedClient.ping(server_v2_addr)

    def test_shutdown_v1(self):
        addr = f'ipc-v3://{_unique_region("shut")}'
        server = ServerV2(
            bind_address=addr,
            icrm_class=IHello,
            crm_instance=Hello(),
        )
        server.start()
        _wait_for_server(addr)
        assert SharedClient.shutdown(addr)
        # Server should be shutting down; give it a moment.
        time.sleep(0.3)


# ---------------------------------------------------------------------------
# V2 mode: SharedClient(try_v2=True) → ServerV2 (handshake v5)
# ---------------------------------------------------------------------------

class TestServerV2WireV2:
    """SharedClient in v2 mode against ServerV2 — control-plane routing."""

    def test_greeting_v2(self, server_v2_addr):
        client = SharedClient(server_v2_addr, try_v2=True)
        client.connect()
        try:
            assert client._v2_mode, 'Expected v2 mode after v5 handshake'
            result = pickle.loads(client.call('greeting', pickle.dumps(('V2',))))
            assert result == 'Hello, V2!'
        finally:
            client.terminate()

    def test_add_v2(self, server_v2_addr):
        client = SharedClient(server_v2_addr, try_v2=True)
        client.connect()
        try:
            assert client._v2_mode
            result = pickle.loads(client.call('add', pickle.dumps((100, 200))))
            assert result == 300
        finally:
            client.terminate()

    def test_list_v2(self, server_v2_addr):
        client = SharedClient(server_v2_addr, try_v2=True)
        client.connect()
        try:
            assert client._v2_mode
            result = pickle.loads(
                client.call('get_items', pickle.dumps(([5, 10],)))
            )
            assert result == ['item-5', 'item-10']
        finally:
            client.terminate()

    def test_custom_type_v2(self, server_v2_addr):
        """Test transferable type round-trip in v2 mode."""
        client = SharedClient(server_v2_addr, try_v2=True)
        client.connect()
        try:
            assert client._v2_mode
            raw = client.call('get_data', pickle.dumps((42,)))
            # HelloData.serialize produces pickle.dumps(dict), so wire result
            # is a pickled dict.  Deserialize with the transferable's deserializer.
            from tests.fixtures.ihello import HelloData
            result = HelloData.deserialize(raw)
            assert result.name == 'data-42'
            assert result.value == 420
        finally:
            client.terminate()

    def test_v2_method_table_populated(self, server_v2_addr):
        """Verify that v5 handshake populates method table on client."""
        client = SharedClient(server_v2_addr, try_v2=True)
        client.connect()
        try:
            assert client._v2_mode
            assert client._method_table is not None
            assert client._method_table.has_name('greeting')
            assert client._method_table.has_name('add')
            assert client._method_table.has_name('get_items')
        finally:
            client.terminate()


# ---------------------------------------------------------------------------
# V2 mode: inline path (small SHM threshold)
# ---------------------------------------------------------------------------

class TestServerV2InlinePath:
    """Force the inline path by using a very small SHM threshold."""

    def test_greeting_inline_v1(self, server_v2_small_shm):
        client = SharedClient(
            server_v2_small_shm,
            ipc_config=IPCConfig(shm_threshold=16),
        )
        client.connect()
        try:
            result = pickle.loads(client.call('greeting', pickle.dumps(('Inline',))))
            assert result == 'Hello, Inline!'
        finally:
            client.terminate()


# ---------------------------------------------------------------------------
# Concurrent calls
# ---------------------------------------------------------------------------

class TestServerV2Concurrent:
    """Multiple concurrent calls to ServerV2."""

    def test_concurrent_v1(self, server_v2_addr):
        client = SharedClient(server_v2_addr)
        client.connect()
        errors: list[str] = []

        def worker(tid: int) -> None:
            try:
                for i in range(5):
                    a, b = tid * 100 + i, i
                    r = pickle.loads(client.call('add', pickle.dumps((a, b))))
                    if r != a + b:
                        errors.append(f'T{tid}[{i}]: expected {a + b}, got {r}')
            except Exception as e:
                errors.append(f'T{tid}: {e}')

        threads = [threading.Thread(target=worker, args=(i,)) for i in range(6)]
        try:
            for t in threads:
                t.start()
            for t in threads:
                t.join(timeout=30)
            assert errors == [], f'Errors: {errors}'
        finally:
            client.terminate()

    def test_concurrent_v2(self, server_v2_addr):
        client = SharedClient(server_v2_addr, try_v2=True)
        client.connect()
        errors: list[str] = []

        def worker(tid: int) -> None:
            try:
                for i in range(5):
                    a, b = tid * 100 + i, i
                    r = pickle.loads(client.call('add', pickle.dumps((a, b))))
                    if r != a + b:
                        errors.append(f'T{tid}[{i}]: expected {a + b}, got {r}')
            except Exception as e:
                errors.append(f'T{tid}: {e}')

        threads = [threading.Thread(target=worker, args=(i,)) for i in range(6)]
        try:
            assert client._v2_mode
            for t in threads:
                t.start()
            for t in threads:
                t.join(timeout=30)
            assert errors == [], f'Errors: {errors}'
        finally:
            client.terminate()

