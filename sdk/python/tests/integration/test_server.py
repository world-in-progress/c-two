"""Integration tests for Server via SOTA API.

Tests Server functionality through the cc.connect CRM proxy.
"""
from __future__ import annotations

import os
import threading
import time

import pytest

import c_two as cc
from c_two.transport import Server
from c_two.transport.client.util import ping, shutdown

from tests.fixtures.hello import HelloImpl
from tests.fixtures.ihello import Hello


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
            if ping(address, timeout=0.5):
                return
        except Exception:
            pass
        time.sleep(0.05)
    raise TimeoutError(f'Server at {address} not ready after {timeout}s')


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture
def server_addr():
    """Start a Server hosting the Hello CRM + Hello CRM."""
    addr = f'ipc://{_unique_region()}'
    server = Server(
        bind_address=addr,
        crm_class=Hello,
        crm_instance=HelloImpl(),
        name='hello',
    )
    server.start()
    _wait_for_server(addr)
    yield addr
    server.shutdown()


@pytest.fixture
def server_small_shm():
    """Server with small SHM threshold (forces inline path more often)."""
    addr = f'ipc://{_unique_region("small")}'
    server = Server(
        bind_address=addr,
        crm_class=Hello,
        crm_instance=HelloImpl(),
        name='hello',
        shm_threshold=16,
    )
    server.start()
    _wait_for_server(addr)
    yield addr
    server.shutdown()


# ---------------------------------------------------------------------------
# Basic server functionality via SOTA API
# ---------------------------------------------------------------------------

class TestServerBasic:
    """Server must handle standard RPC calls via SOTA API."""

    def test_greeting(self, server_addr):
        proxy = cc.connect(Hello, name='hello', address=server_addr)
        try:
            result = proxy.greeting('World')
            assert result == 'Hello, World!'
        finally:
            cc.close(proxy)

    def test_add(self, server_addr):
        proxy = cc.connect(Hello, name='hello', address=server_addr)
        try:
            result = proxy.add(7, 8)
            assert result == 15
        finally:
            cc.close(proxy)

    def test_list(self, server_addr):
        proxy = cc.connect(Hello, name='hello', address=server_addr)
        try:
            result = proxy.get_items([1, 2, 3])
            assert result == ['item-1', 'item-2', 'item-3']
        finally:
            cc.close(proxy)

    def test_ping(self, server_addr):
        assert ping(server_addr)

    def test_shutdown(self):
        addr = f'ipc://{_unique_region("shut")}'
        server = Server(
            bind_address=addr,
            crm_class=Hello,
            crm_instance=HelloImpl(),
            name='hello',
        )
        server.start()
        _wait_for_server(addr)
        assert shutdown(addr)
        # Server should be shutting down; give it a moment.
        time.sleep(0.3)


# ---------------------------------------------------------------------------
# CRM method routing
# ---------------------------------------------------------------------------

class TestServerRouting:
    """Server routes CRM method calls correctly."""

    def test_greeting_routed(self, server_addr):
        proxy = cc.connect(Hello, name='hello', address=server_addr)
        try:
            result = proxy.greeting('V2')
            assert result == 'Hello, V2!'
        finally:
            cc.close(proxy)

    def test_add_routed(self, server_addr):
        proxy = cc.connect(Hello, name='hello', address=server_addr)
        try:
            result = proxy.add(100, 200)
            assert result == 300
        finally:
            cc.close(proxy)

    def test_list_routed(self, server_addr):
        proxy = cc.connect(Hello, name='hello', address=server_addr)
        try:
            result = proxy.get_items([5, 10])
            assert result == ['item-5', 'item-10']
        finally:
            cc.close(proxy)

    def test_custom_type_routed(self, server_addr):
        """Test transferable type round-trip via SOTA API."""
        proxy = cc.connect(Hello, name='hello', address=server_addr)
        try:
            from tests.fixtures.ihello import HelloData
            result = proxy.get_data(42)
            assert result.name == 'data-42'
            assert result.value == 420
        finally:
            cc.close(proxy)


# ---------------------------------------------------------------------------
# Inline path (small SHM threshold)
# ---------------------------------------------------------------------------

class TestServerInlinePath:
    """Force the inline path by using a very small SHM threshold."""

    def test_greeting_inline(self, server_small_shm):
        proxy = cc.connect(Hello, name='hello', address=server_small_shm)
        try:
            result = proxy.greeting('Inline')
            assert result == 'Hello, Inline!'
        finally:
            cc.close(proxy)


# ---------------------------------------------------------------------------
# Concurrent calls
# ---------------------------------------------------------------------------

class TestServerConcurrent:
    """Multiple concurrent calls to Server."""

    def test_concurrent_calls(self, server_addr):
        proxy = cc.connect(Hello, name='hello', address=server_addr)
        errors: list[str] = []

        def worker(tid: int) -> None:
            try:
                for i in range(5):
                    a, b = tid * 100 + i, i
                    r = proxy.add(a, b)
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
            cc.close(proxy)
