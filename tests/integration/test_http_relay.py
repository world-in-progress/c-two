"""Integration tests for the HTTP relay chain.

Tests the full pipeline:  RustHttpClient → NativeRelay → RustClient → Server → CRM

Also tests ``cc.connect(address='http://...')`` end-to-end.
"""
from __future__ import annotations

import os
import threading
import time

import pytest

import httpx

import c_two as cc
from c_two._native import NativeRelay, RustHttpClientPool
from c_two.transport.client.proxy import ICRMProxy
from c_two.transport.registry import _ProcessRegistry

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
        try:
            resp = httpx.get(f'{url}/health', timeout=0.5)
            if resp.status_code == 200:
                return
        except Exception:
            pass
        time.sleep(0.1)
    raise TimeoutError(f'Relay at {url} not ready after {timeout}s')


def _acquire_http(url: str):
    """Acquire a RustHttpClient from the singleton pool."""
    return RustHttpClientPool.instance().acquire(url)


def _release_http(url: str):
    """Release a RustHttpClient reference back to the pool."""
    RustHttpClientPool.instance().release(url)


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
    """Start Server + NativeRelay and return (relay_url, ipc_address).

    Registers a Hello CRM as 'hello' and optionally a Counter CRM as
    'counter'.  The relay starts empty; upstreams are added
    programmatically via ``register_upstream()``.
    """
    ipc_addr = f'ipc://relay_test_{os.getpid()}_{_next_id()}'
    http_port = 19000 + _next_id()

    # Register CRMs via SOTA API.
    cc.set_address(ipc_addr)
    cc.register(IHello, Hello(), name='hello')
    cc.register(ICounter, Counter(), name='counter')

    # Start relay (empty — no upstream param).
    relay = NativeRelay(f'0.0.0.0:{http_port}')
    relay.start()
    relay_url = f'http://127.0.0.1:{http_port}'
    _wait_for_relay(relay_url)

    # Register upstreams programmatically.
    relay.register_upstream('hello', ipc_addr)
    relay.register_upstream('counter', ipc_addr)

    yield relay_url, ipc_addr

    relay.stop()


# ---------------------------------------------------------------------------
# Full-chain tests
# ---------------------------------------------------------------------------

class TestHttpRelayFullChain:
    """End-to-end: HttpClient → Relay → Server → Hello CRM."""

    def test_hello_via_http(self, relay_stack):
        """Simple string call through HTTP relay."""
        relay_url, _ = relay_stack
        client = _acquire_http(relay_url)
        try:
            icrm = IHello()
            icrm.client = ICRMProxy.http(client, 'hello')
            result = icrm.greeting('HTTP')
            assert result == 'Hello, HTTP!'
        finally:
            _release_http(relay_url)

    def test_add_via_http(self, relay_stack):
        """Numeric call through HTTP relay."""
        relay_url, _ = relay_stack
        client = _acquire_http(relay_url)
        try:
            icrm = IHello()
            icrm.client = ICRMProxy.http(client, 'hello')
            result = icrm.add(42, 58)
            assert result == 100
        finally:
            _release_http(relay_url)

    def test_get_items_via_http(self, relay_stack):
        """List return through HTTP relay."""
        relay_url, _ = relay_stack
        client = _acquire_http(relay_url)
        try:
            icrm = IHello()
            icrm.client = ICRMProxy.http(client, 'hello')
            result = icrm.get_items([10, 20, 30])
            assert result == ['item-10', 'item-20', 'item-30']
        finally:
            _release_http(relay_url)

    def test_get_data_transferable_via_http(self, relay_stack):
        """Custom transferable round-trip through HTTP relay."""
        relay_url, _ = relay_stack
        client = _acquire_http(relay_url)
        try:
            icrm = IHello()
            icrm.client = ICRMProxy.http(client, 'hello')
            result = icrm.get_data(5)
            assert result.name == 'data-5'
            assert result.value == 50
        finally:
            _release_http(relay_url)

    def test_multi_crm_routing(self, relay_stack):
        """Route to different CRMs by name through HTTP relay."""
        relay_url, _ = relay_stack
        client = _acquire_http(relay_url)
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
            _release_http(relay_url)

    def test_health_endpoint(self, relay_stack):
        """GET /health returns OK."""
        relay_url, _ = relay_stack
        client = _acquire_http(relay_url)
        try:
            health = client.health()
            assert health is True
        finally:
            _release_http(relay_url)

    def test_concurrent_http_calls(self, relay_stack):
        """Multiple threads calling through HTTP relay."""
        relay_url, _ = relay_stack
        client = _acquire_http(relay_url)
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
            _release_http(relay_url)


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


# ---------------------------------------------------------------------------
# Control-plane endpoint tests
# ---------------------------------------------------------------------------

class TestRelayControlPlane:
    """Test the relay control-plane endpoints (/_register, /_unregister, /_routes)."""

    def test_register_via_http_control(self):
        """POST /_register adds an upstream and allows calls."""
        ipc_addr = f'ipc://ctrl_test_{os.getpid()}_{_next_id()}'
        http_port = 19000 + _next_id()

        cc.set_address(ipc_addr)
        cc.register(IHello, Hello(), name='hello')

        relay = NativeRelay(f'0.0.0.0:{http_port}')
        relay.start()
        relay_url = f'http://127.0.0.1:{http_port}'
        _wait_for_relay(relay_url)

        try:
            transport = httpx.HTTPTransport()
            with httpx.Client(transport=transport) as http:
                # Register via control endpoint.
                resp = http.post(
                    f'{relay_url}/_register',
                    json={'name': 'hello', 'address': ipc_addr},
                )
                assert resp.status_code == 201
                assert resp.json()['registered'] == 'hello'

                # Verify route appears in /_routes.
                resp = http.get(f'{relay_url}/_routes')
                assert resp.status_code == 200
                routes = resp.json()['routes']
                assert any(r['name'] == 'hello' for r in routes)

            # Verify data-plane call works.
            client = _acquire_http(relay_url)
            try:
                icrm = IHello()
                icrm.client = ICRMProxy.http(client, 'hello')
                assert icrm.greeting('Control') == 'Hello, Control!'
            finally:
                _release_http(relay_url)
        finally:
            relay.stop()

    def test_register_duplicate_409(self):
        """POST /_register with duplicate name returns 409."""
        ipc_addr = f'ipc://dup_test_{os.getpid()}_{_next_id()}'
        http_port = 19000 + _next_id()

        cc.set_address(ipc_addr)
        cc.register(IHello, Hello(), name='hello')

        relay = NativeRelay(f'0.0.0.0:{http_port}')
        relay.start()
        relay_url = f'http://127.0.0.1:{http_port}'
        _wait_for_relay(relay_url)

        try:
            transport = httpx.HTTPTransport()
            with httpx.Client(transport=transport) as http:
                resp = http.post(
                    f'{relay_url}/_register',
                    json={'name': 'hello', 'address': ipc_addr},
                )
                assert resp.status_code == 201

                # Duplicate registration.
                resp = http.post(
                    f'{relay_url}/_register',
                    json={'name': 'hello', 'address': ipc_addr},
                )
                assert resp.status_code == 409
        finally:
            relay.stop()

    def test_unregister_removes_route(self):
        """POST /_unregister removes the route; calls return 404."""
        ipc_addr = f'ipc://unreg_test_{os.getpid()}_{_next_id()}'
        http_port = 19000 + _next_id()

        cc.set_address(ipc_addr)
        cc.register(IHello, Hello(), name='hello')

        relay = NativeRelay(f'0.0.0.0:{http_port}')
        relay.start()
        relay_url = f'http://127.0.0.1:{http_port}'
        _wait_for_relay(relay_url)

        try:
            transport = httpx.HTTPTransport()
            with httpx.Client(transport=transport) as http:
                # Register, then unregister.
                http.post(
                    f'{relay_url}/_register',
                    json={'name': 'hello', 'address': ipc_addr},
                )
                resp = http.post(
                    f'{relay_url}/_unregister',
                    json={'name': 'hello'},
                )
                assert resp.status_code == 200

                # Verify route is gone.
                resp = http.get(f'{relay_url}/_routes')
                routes = resp.json()['routes']
                assert not any(r['name'] == 'hello' for r in routes)

                # Data-plane call should return 404.
                resp = http.post(
                    f'{relay_url}/hello/greeting',
                    content=b'test',
                )
                assert resp.status_code == 404
        finally:
            relay.stop()

    def test_unregister_missing_404(self):
        """POST /_unregister for unknown name returns 404."""
        http_port = 19000 + _next_id()

        relay = NativeRelay(f'0.0.0.0:{http_port}')
        relay.start()
        relay_url = f'http://127.0.0.1:{http_port}'
        _wait_for_relay(relay_url)

        try:
            transport = httpx.HTTPTransport()
            with httpx.Client(transport=transport) as http:
                resp = http.post(
                    f'{relay_url}/_unregister',
                    json={'name': 'nonexistent'},
                )
                assert resp.status_code == 404
        finally:
            relay.stop()

    def test_health_shows_registered_routes(self):
        """GET /health lists all registered route names."""
        ipc_addr = f'ipc://health_test_{os.getpid()}_{_next_id()}'
        http_port = 19000 + _next_id()

        cc.set_address(ipc_addr)
        cc.register(IHello, Hello(), name='hello')
        cc.register(ICounter, Counter(), name='counter')

        relay = NativeRelay(f'0.0.0.0:{http_port}')
        relay.start()
        relay_url = f'http://127.0.0.1:{http_port}'
        _wait_for_relay(relay_url)

        try:
            relay.register_upstream('hello', ipc_addr)
            relay.register_upstream('counter', ipc_addr)

            transport = httpx.HTTPTransport()
            with httpx.Client(transport=transport) as http:
                resp = http.get(f'{relay_url}/health')
                health = resp.json()
                assert health['status'] == 'ok'
                assert set(health['routes']) == {'hello', 'counter'}
        finally:
            relay.stop()

    def test_call_unknown_route_404(self):
        """POST /{route}/{method} for unregistered route returns 404."""
        http_port = 19000 + _next_id()

        relay = NativeRelay(f'0.0.0.0:{http_port}')
        relay.start()
        relay_url = f'http://127.0.0.1:{http_port}'
        _wait_for_relay(relay_url)

        try:
            transport = httpx.HTTPTransport()
            with httpx.Client(transport=transport) as http:
                resp = http.post(
                    f'{relay_url}/nonexistent/method',
                    content=b'data',
                )
                assert resp.status_code == 404
                assert 'nonexistent' in resp.json()['error']
        finally:
            relay.stop()
