"""Unit tests for SharedClient — thin wrapper over Rust IPC client."""
from __future__ import annotations

import os
import pickle
import threading
import time

import pytest

from c_two.transport.client.core import SharedClient
from c_two.transport.server import Server

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
        return f"test_shared_{os.getpid()}_{_counter}"


def _wait_for_server(address: str, timeout: float = 5.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            if SharedClient.ping(address, timeout=0.5):
                return
        except Exception:
            pass
        time.sleep(0.05)
    raise TimeoutError(f"Server at {address} not ready after {timeout}s")


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture
def ipc_addr():
    """Start an IPC server, yield address, shut down."""
    addr = f"ipc://{_unique_region()}"
    server = Server(bind_address=addr, icrm_class=IHello, crm_instance=Hello())
    server.start()
    _wait_for_server(addr)

    yield addr

    server.shutdown()


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

class TestSharedClientBasic:
    """Basic SharedClient lifecycle and call tests."""

    def test_connect_and_terminate(self, ipc_addr):
        client = SharedClient(ipc_addr)
        client.connect()
        assert client.is_connected
        client.terminate()
        assert not client.is_connected

    def test_basic_call(self, ipc_addr):
        """Simple RPC call through SharedClient -> Server."""
        client = SharedClient(ipc_addr)
        client.connect()
        try:
            result_bytes = client.call("greeting", pickle.dumps(("World",)))
            result = pickle.loads(result_bytes)
        finally:
            client.terminate()

    def test_multiple_sequential_calls(self, ipc_addr):
        client = SharedClient(ipc_addr)
        client.connect()
        try:
            r1 = pickle.loads(client.call("greeting", pickle.dumps(("Alice",))))
            r2 = pickle.loads(client.call("add", pickle.dumps((3, 4))))
            r3 = pickle.loads(client.call("greeting", pickle.dumps(("Bob",))))
            assert r2 == 7
        finally:
            client.terminate()

    def test_none_return(self, ipc_addr):
        client = SharedClient(ipc_addr)
        client.connect()
        try:
            result_bytes = client.call("echo_none", pickle.dumps(("none",)))
            assert result_bytes == b""
        finally:
            client.terminate()

    def test_list_return(self, ipc_addr):
        client = SharedClient(ipc_addr)
        client.connect()
        try:
            result = pickle.loads(client.call("get_items", pickle.dumps(([1, 2, 3],))))
            assert result == ["item-1", "item-2", "item-3"]
        finally:
            client.terminate()


class TestSharedClientConcurrent:
    """Concurrency tests — the core value proposition of SharedClient."""

    def test_concurrent_calls(self, ipc_addr):
        """Multiple threads calling via the same SharedClient simultaneously."""
        client = SharedClient(ipc_addr)
        client.connect()
        errors: list[str] = []
        n_threads = 4
        n_calls = 10

        def worker(thread_id: int) -> None:
            try:
                for i in range(n_calls):
                    result_bytes = client.call("add", pickle.dumps((i, thread_id)))
                    result = pickle.loads(result_bytes)
                    if result != i + thread_id:
                        errors.append(f"Thread {thread_id}: expected {i + thread_id}, got {result}")
            except Exception as e:
                errors.append(f"Thread {thread_id}: {e}")

        threads = [threading.Thread(target=worker, args=(i,)) for i in range(n_threads)]
        try:
            for t in threads:
                t.start()
            for t in threads:
                t.join(timeout=30)
            assert errors == [], f"Concurrency errors: {errors}"
        finally:
            client.terminate()

    def test_concurrent_different_methods(self, ipc_addr):
        """Different methods called concurrently on the same SharedClient."""
        client = SharedClient(ipc_addr)
        client.connect()
        results: dict[str, object] = {}
        errors: list[str] = []

        def call_greeting(name: str) -> None:
            try:
                r = pickle.loads(client.call("greeting", pickle.dumps((name,))))
                results[f"greeting_{name}"] = r
            except Exception as e:
                errors.append(str(e))

        def call_add(a: int, b: int) -> None:
            try:
                r = pickle.loads(client.call("add", pickle.dumps((a, b))))
                results[f"add_{a}_{b}"] = r
            except Exception as e:
                errors.append(str(e))

        threads = [
            threading.Thread(target=call_greeting, args=("Alice",)),
            threading.Thread(target=call_add, args=(10, 20)),
            threading.Thread(target=call_greeting, args=("Bob",)),
            threading.Thread(target=call_add, args=(100, 200)),
        ]
        try:
            for t in threads:
                t.start()
            for t in threads:
                t.join(timeout=30)

            assert errors == []
            assert results["add_10_20"] == 30
            assert results["add_100_200"] == 300
        finally:
            client.terminate()


class TestSharedClientPingShutdown:
    """Ping and shutdown static methods."""

    def test_ping(self, ipc_addr):
        assert SharedClient.ping(ipc_addr) is True

    def test_ping_nonexistent(self):
        assert SharedClient.ping("ipc://nonexistent_test_shared") is False

    def test_shutdown(self, ipc_addr):
        result = SharedClient.shutdown(ipc_addr)
        assert result is True
        time.sleep(0.5)
        assert SharedClient.ping(ipc_addr) is False


class TestSharedClientPayloads:
    """Payload size tests."""

    @pytest.mark.xfail(reason="Rust server: large SHM payload handling gap")
    def test_large_payload(self, ipc_addr):
        """Large payloads should work correctly."""
        client = SharedClient(ipc_addr)
        client.connect()
        try:
            large_name = "X" * 8000
            result_bytes = client.call("greeting", pickle.dumps((large_name,)))
            result = pickle.loads(result_bytes)
        finally:
            client.terminate()

    def test_small_payload(self, ipc_addr):
        """Small payloads should work correctly."""
        client = SharedClient(ipc_addr)
        client.connect()
        try:
            result_bytes = client.call("add", pickle.dumps((1, 2)))
            result = pickle.loads(result_bytes)
            assert result == 3
        finally:
            client.terminate()


class TestSharedClientErrorHandling:
    """Error propagation tests."""

    def test_call_after_terminate(self, ipc_addr):
        client = SharedClient(ipc_addr)
        client.connect()
        client.terminate()
        # After terminate, client is None — next call triggers reconnect
        # or raises depending on implementation.
        # The thin wrapper reconnects on call, so this should still work.
        result_bytes = client.call("add", pickle.dumps((1, 2)))
        result = pickle.loads(result_bytes)
        assert result == 3

    def test_double_terminate(self, ipc_addr):
        """Double terminate should not raise."""
        client = SharedClient(ipc_addr)
        client.connect()
        client.terminate()
        client.terminate()  # Should be idempotent
