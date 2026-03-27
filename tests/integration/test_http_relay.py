"""Integration tests for the HTTP relay chain.

Tests the full pipeline:  HttpClient → RelayV2 → SharedClient → ServerV2 → CRM

Also tests ``cc.connect(address='http://...')`` end-to-end.
"""
from __future__ import annotations

import os
import threading
import time

import pytest

import c_two as cc
from c_two.rpc_v2.http_client import HttpClient
from c_two.rpc_v2.proxy import ICRMProxy
from c_two.rpc_v2.registry import _ProcessRegistry
from c_two.rpc_v2.relay import RelayV2

from tests.fixtures.hello import Hello
from tests.fixtures.ihello import IHello
from tests.fixtures.counter import ICounter, Counter


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

_counter = 0
_lock = threading.Lock()


def _next_id() -> int:
    global _counter
    with _lock:
        _counter += 1
        return _counter


def _wait_for_relay(url: str, timeout: float = 5.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if HttpClient.ping(url, timeout=0.5):
            return
        time.sleep(0.1)
    raise TimeoutError(f'Relay at {url} not ready after {timeout}s')


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture(autouse=True)
def _clean_registry():
    """Ensure a clean registry for every test."""
    _ProcessRegistry.reset()
    yield
    _ProcessRegistry.reset()


@pytest.fixture
def relay_stack():
    """Start ServerV2 + RelayV2 and return (relay_url, ipc_address).

    Registers a Hello CRM as 'hello' and optionally a Counter CRM as
    'counter'.
    """
    ipc_addr = f'ipc-v3://relay_test_{os.getpid()}_{_next_id()}'
    http_port = 19000 + _next_id()

    # Register CRMs via SOTA API.
    cc.set_address(ipc_addr)
    cc.register(IHello, Hello(), name='hello')
    cc.register(ICounter, Counter(), name='counter')

    # Start relay.
    relay = RelayV2(
        bind=f'0.0.0.0:{http_port}',
        upstream=ipc_addr,
    )
    relay.start(blocking=False)
    _wait_for_relay(relay.url)

    yield relay.url, ipc_addr

    relay.stop()


# ---------------------------------------------------------------------------
# Full-chain tests
# ---------------------------------------------------------------------------

class TestHttpRelayFullChain:
    """End-to-end: HttpClient → RelayV2 → ServerV2 → Hello CRM."""

    def test_hello_via_http(self, relay_stack):
        """Simple string call through HTTP relay."""
        relay_url, _ = relay_stack
        client = HttpClient(relay_url)
        try:
            icrm = IHello()
            icrm.client = ICRMProxy.http(client, 'hello')
            result = icrm.greeting('HTTP')
            assert result == 'Hello, HTTP!'
        finally:
            client.terminate()

    def test_add_via_http(self, relay_stack):
        """Numeric call through HTTP relay."""
        relay_url, _ = relay_stack
        client = HttpClient(relay_url)
        try:
            icrm = IHello()
            icrm.client = ICRMProxy.http(client, 'hello')
            result = icrm.add(42, 58)
            assert result == 100
        finally:
            client.terminate()

    def test_get_items_via_http(self, relay_stack):
        """List return through HTTP relay."""
        relay_url, _ = relay_stack
        client = HttpClient(relay_url)
        try:
            icrm = IHello()
            icrm.client = ICRMProxy.http(client, 'hello')
            result = icrm.get_items([10, 20, 30])
            assert result == ['item-10', 'item-20', 'item-30']
        finally:
            client.terminate()

    def test_get_data_transferable_via_http(self, relay_stack):
        """Custom transferable round-trip through HTTP relay."""
        relay_url, _ = relay_stack
        client = HttpClient(relay_url)
        try:
            icrm = IHello()
            icrm.client = ICRMProxy.http(client, 'hello')
            result = icrm.get_data(5)
            assert result.name == 'data-5'
            assert result.value == 50
        finally:
            client.terminate()

    def test_multi_crm_routing(self, relay_stack):
        """Route to different CRMs by name through HTTP relay."""
        relay_url, _ = relay_stack
        client = HttpClient(relay_url)
        try:
            # Hello CRM
            hello = IHello()
            hello.client = ICRMProxy.http(client, 'hello')
            assert hello.greeting('Route') == 'Hello, Route!'

            # Counter CRM
            counter = ICounter()
            counter.client = ICRMProxy.http(client, 'counter')
            counter.increment(1)
            counter.increment(1)
            assert counter.get() == 2
        finally:
            client.terminate()

    def test_health_endpoint(self, relay_stack):
        """GET /health returns JSON."""
        relay_url, _ = relay_stack
        client = HttpClient(relay_url)
        try:
            health = client.health()
            assert health['status'] == 'ok'
        finally:
            client.terminate()

    def test_concurrent_http_calls(self, relay_stack):
        """Multiple threads calling through HTTP relay."""
        relay_url, _ = relay_stack
        client = HttpClient(relay_url)
        errors: list[str] = []
        n_threads = 8
        n_calls = 5

        def worker(tid: int) -> None:
            try:
                icrm = IHello()
                icrm.client = ICRMProxy.http(client, 'hello')
                for i in range(n_calls):
                    result = icrm.add(tid, i)
                    if result != tid + i:
                        errors.append(f'T{tid}[{i}]: {result} != {tid + i}')
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


# ---------------------------------------------------------------------------
# cc.connect(address='http://...') tests
# ---------------------------------------------------------------------------

class TestCcConnectHttp:
    """Test the ``cc.connect(address='http://...')`` integration."""

    def test_connect_http_mode(self, relay_stack):
        """cc.connect with HTTP address returns http-mode proxy."""
        relay_url, _ = relay_stack
        icrm = cc.connect(IHello, name='hello', address=relay_url)
        try:
            assert icrm.client._mode == 'http'
            assert icrm.client.supports_direct_call is False
        finally:
            cc.close(icrm)

    def test_connect_http_call(self, relay_stack):
        """cc.connect with HTTP address can make real CRM calls."""
        relay_url, _ = relay_stack
        icrm = cc.connect(IHello, name='hello', address=relay_url)
        try:
            result = icrm.greeting('SOTA')
            assert result == 'Hello, SOTA!'
        finally:
            cc.close(icrm)

    def test_connect_http_multi_crm(self, relay_stack):
        """cc.connect to different CRMs via HTTP."""
        relay_url, _ = relay_stack

        hello = cc.connect(IHello, name='hello', address=relay_url)
        counter = cc.connect(ICounter, name='counter', address=relay_url)
        try:
            assert hello.greeting('Multi') == 'Hello, Multi!'
            counter.increment(1)
            assert counter.get() == 1
        finally:
            cc.close(hello)
            cc.close(counter)

    def test_connect_http_close_releases_pool(self, relay_stack):
        """cc.close releases the HTTP pool reference."""
        relay_url, _ = relay_stack
        registry = _ProcessRegistry.get()

        icrm = cc.connect(IHello, name='hello', address=relay_url)
        assert registry._http_pool.refcount(relay_url) == 1

        cc.close(icrm)
        assert registry._http_pool.refcount(relay_url) == 0
