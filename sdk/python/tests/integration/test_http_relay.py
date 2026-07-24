"""Integration tests for the HTTP relay chain.

Tests the full pipeline: Core route-bound client -> standalone c3 relay
-> Core host -> Python CRM.

Also tests ``cc.connect(address='http://...')`` end-to-end through the native
relay-aware explicit HTTP projection.
"""
from __future__ import annotations

import os
import threading

import pytest

import httpx

import c_two as cc
from c_two.config.settings import settings
from c_two.config.ipc import _resolve_server_ipc_config
from c_two.error import ResourceNotFound
from c_two.crm.contract import crm_contract
from c_two.transport.registry import _ProcessRegistry

from tests.fixtures.hello import HelloImpl
from tests.fixtures.ihello import Hello
from tests.fixtures.counter import Counter, CounterImpl

DEFAULT_MAX_PAYLOAD_SIZE = int(_resolve_server_ipc_config()['max_payload_size'])


def _server_instance_id_for(
    registry: _ProcessRegistry,
    address: str,
    *,
    route_name: str = 'hello',
    crm_class: type = Hello,
) -> str:
    """Read the identity owned by the same Core runtime as the host."""
    del route_name, crm_class
    identity = registry._runtime_session.ensure_server()  # noqa: SLF001
    assert identity["ipc_address"] == address
    instance_id = identity["server_instance_id"]
    assert isinstance(instance_id, str)
    assert instance_id
    return instance_id


def _expected_contract_headers(crm_class: type = Hello) -> dict[str, str]:
    expected = crm_contract(crm_class)
    return {
        "x-c2-expected-crm-ns": expected.crm_ns,
        "x-c2-expected-crm-name": expected.crm_name,
        "x-c2-expected-crm-ver": expected.crm_ver,
        "x-c2-expected-abi-hash": expected.abi_hash,
        "x-c2-expected-signature-hash": expected.signature_hash,
    }


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
def relay_stack(start_c3_relay):
    """Start Server + standalone c3 relay and return (relay_url, ipc_address).

    Registers a Hello CRM as 'hello' and a Counter CRM as 'counter'. The relay
    starts empty; upstreams are added through the HTTP control plane.
    """
    # Register CRMs via SOTA API.
    cc.register(Hello, HelloImpl(), name='hello')
    cc.register(Counter, CounterImpl(), name='counter')
    ipc_addr = cc.server_address()
    server_id = cc.server_id()
    server_instance_id = _server_instance_id_for(_ProcessRegistry.get(), ipc_addr)

    relay = start_c3_relay()
    relay_url = relay.url

    with httpx.Client(trust_env=False, timeout=5.0) as http:
        for name in ('hello', 'counter'):
            resp = http.post(
                f'{relay_url}/_register',
                json={
                    'name': name,
                    'server_id': server_id,
                    'server_instance_id': server_instance_id,
                    'address': ipc_addr,
                    'max_payload_size': DEFAULT_MAX_PAYLOAD_SIZE,
                },
            )
            assert resp.status_code == 201, resp.text

    yield relay_url, ipc_addr


# ---------------------------------------------------------------------------
# Full-chain tests
# ---------------------------------------------------------------------------

class TestHttpRelayFullChain:
    """End-to-end: HttpClient → Relay → Server → Hello CRM."""

    def test_hello_via_http(self, relay_stack):
        """Simple string call through HTTP relay."""
        relay_url, _ = relay_stack
        crm = cc.connect(Hello, name='hello', address=relay_url)
        try:
            result = crm.greeting('HTTP')
            assert result == 'Hello, HTTP!'
        finally:
            cc.close(crm)

    def test_add_via_http(self, relay_stack):
        """Numeric call through HTTP relay."""
        relay_url, _ = relay_stack
        crm = cc.connect(Hello, name='hello', address=relay_url)
        try:
            result = crm.add(42, 58)
            assert result == 100
        finally:
            cc.close(crm)

    def test_get_items_via_http(self, relay_stack):
        """List return through HTTP relay."""
        relay_url, _ = relay_stack
        crm = cc.connect(Hello, name='hello', address=relay_url)
        try:
            result = crm.get_items([10, 20, 30])
            assert result == ['item-10', 'item-20', 'item-30']
        finally:
            cc.close(crm)

    def test_get_data_transferable_via_http(self, relay_stack):
        """Custom transferable round-trip through HTTP relay."""
        relay_url, _ = relay_stack
        crm = cc.connect(Hello, name='hello', address=relay_url)
        try:
            result = crm.get_data(5)
            assert result.name == 'data-5'
            assert result.value == 50
        finally:
            cc.close(crm)

    def test_multi_crm_routing(self, relay_stack):
        """Route to different CRMs by name through HTTP relay."""
        relay_url, _ = relay_stack
        hello = cc.connect(Hello, name='hello', address=relay_url)
        counter = cc.connect(Counter, name='counter', address=relay_url)
        try:
            assert hello.greeting('Route') == 'Hello, Route!'
            counter.increment(1)
            counter.increment(1)
            assert counter.get() == 2
        finally:
            cc.close(hello)
            cc.close(counter)

    def test_health_endpoint(self, relay_stack):
        """GET /health returns OK."""
        relay_url, _ = relay_stack
        with httpx.Client(trust_env=False, timeout=5.0) as client:
            response = client.get(f"{relay_url}/health")
        assert response.status_code == 200

    def test_concurrent_http_calls(self, relay_stack):
        """Multiple threads calling through HTTP relay."""
        relay_url, _ = relay_stack
        crm = cc.connect(Hello, name='hello', address=relay_url)
        errors: list[str] = []
        n_threads = 8
        n_calls = 5

        def worker(tid: int) -> None:
            try:
                for i in range(n_calls):
                    result = crm.add(tid, i)
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
            cc.close(crm)


# ---------------------------------------------------------------------------
# cc.connect(address='http://...') tests
# ---------------------------------------------------------------------------

class TestCcConnectHttp:
    """Test the ``cc.connect(address='http://...')`` integration."""

    def test_connect_http_mode(self, relay_stack):
        """cc.connect with HTTP address returns http-mode proxy."""
        relay_url, _ = relay_stack
        crm = cc.connect(Hello, name='hello', address=relay_url)
        try:
            assert crm.client._mode == 'http'
            assert crm.client.supports_direct_call is False
        finally:
            cc.close(crm)

    def test_connect_http_call(self, relay_stack):
        """cc.connect with HTTP address can make real CRM calls."""
        relay_url, _ = relay_stack
        crm = cc.connect(Hello, name='hello', address=relay_url)
        try:
            result = crm.greeting('SOTA')
            assert result == 'Hello, SOTA!'
        finally:
            cc.close(crm)

    def test_connect_http_multi_crm(self, relay_stack):
        """cc.connect to different CRMs via HTTP."""
        relay_url, _ = relay_stack

        hello = cc.connect(Hello, name='hello', address=relay_url)
        counter = cc.connect(Counter, name='counter', address=relay_url)
        try:
            assert hello.greeting('Multi') == 'Hello, Multi!'
            counter.increment(1)
            assert counter.get() == 1
        finally:
            cc.close(hello)
            cc.close(counter)

    def test_connect_http_rejects_crm_contract_mismatch_before_call(self, relay_stack):
        relay_url, _ = relay_stack

        with pytest.raises(ResourceNotFound, match="route not found: hello"):
            cc.connect(Counter, name='hello', address=relay_url)

    def test_connect_http_call_route_not_found_raises_cc_error(self, relay_stack):
        relay_url, _ = relay_stack
        server_id = cc.server_id()

        crm = cc.connect(Hello, name='hello', address=relay_url)
        try:
            with httpx.Client(trust_env=False, timeout=5.0) as http:
                resp = http.post(
                    f'{relay_url}/_unregister',
                    json={'name': 'hello', 'server_id': server_id},
                )
                assert resp.status_code == 200, resp.text

            with pytest.raises(ResourceNotFound, match='route not found: hello'):
                crm.greeting('stale')
        finally:
            cc.close(crm)

    def test_relay_call_rejects_invalid_expected_crm_header(self, start_c3_relay):
        relay = start_c3_relay()
        with httpx.Client(trust_env=False, timeout=5.0) as http:
            resp = http.post(
                f"{relay.url}/grid/get",
                content=b"",
                headers={
                    "x-c2-expected-crm-ns": "test/grid",
                    "x-c2-expected-crm-name": "Grid",
                    "x-c2-expected-crm-ver": "0.1.0",
                },
            )

        assert resp.status_code == 400
        assert resp.json()["error"] == "InvalidCrmTag"

    def test_connect_http_close_closes_relay_aware_client(self, relay_stack):
        """cc.close closes the relay-aware explicit HTTP client."""
        relay_url, _ = relay_stack

        crm = cc.connect(Hello, name='hello', address=relay_url)
        native_client = crm.client._client  # noqa: SLF001
        assert native_client.mode == 'http'

        cc.close(crm)
        with pytest.raises(Exception, match='closed'):
            native_client.call('greeting', b'')

    def test_connect_http_with_slash_in_name(self, start_c3_relay):
        """CRM names containing '/' (toodle-style resource paths) work over HTTP relay.

        Regression: axum's single-segment ``Path<String>`` extractor would
        404 on a raw ``/_resolve/a/b`` and split ``/{name}/{method}``
        wrong. Both the Python registry and the Rust HTTP client now
        percent-encode ``/`` as ``%2F``.
        """
        slashed_name = 'toodle/grid/0'
        cc.register(Hello, HelloImpl(), name=slashed_name)
        ipc_addr = cc.server_address()
        server_id = cc.server_id()
        server_instance_id = _server_instance_id_for(
            _ProcessRegistry.get(),
            ipc_addr,
            route_name=slashed_name,
        )

        relay = start_c3_relay()
        relay_url = relay.url
        with httpx.Client(trust_env=False, timeout=5.0) as http:
            resp = http.post(
                f'{relay_url}/_register',
                json={
                    'name': slashed_name,
                    'server_id': server_id,
                    'server_instance_id': server_instance_id,
                    'address': ipc_addr,
                    'max_payload_size': DEFAULT_MAX_PAYLOAD_SIZE,
                },
            )
            assert resp.status_code == 201, resp.text

        # Forced HTTP mode (address explicitly set).
        crm = cc.connect(Hello, name=slashed_name, address=relay_url)
        try:
            assert crm.greeting('Slash') == 'Hello, Slash!'
        finally:
            cc.close(crm)

    def test_no_address_connect_uses_env_relay_projection(self, start_c3_relay, monkeypatch):
        name = 'runtime/session/env-relay-connect'
        relay = start_c3_relay()
        relay_url = relay.url
        previous_relay = settings.relay_anchor_address
        registrar = _ProcessRegistry()
        resolver = _ProcessRegistry()
        crm = None
        try:
            monkeypatch.setenv('C2_RELAY_ANCHOR_ADDRESS', relay_url)
            settings.relay_anchor_address = None
            registrar.register(Hello, HelloImpl(), name=name)
            assert registrar.get_server_address() is not None

            crm = resolver.connect(Hello, name=name)
            assert crm.client._mode == 'ipc'
            assert crm.greeting('EnvRelay') == 'Hello, EnvRelay!'
        finally:
            if crm is not None:
                resolver.close(crm)
            resolver.shutdown()
            settings.relay_anchor_address = previous_relay
            registrar.shutdown()
            settings.relay_anchor_address = previous_relay

    def test_no_address_connect_uses_runtime_session_relay_projection(self, start_c3_relay):
        name = 'runtime/session/relay-connect'
        relay = start_c3_relay()
        relay_url = relay.url
        previous_relay = settings.relay_anchor_address
        registrar = _ProcessRegistry()
        resolver = _ProcessRegistry()
        crm = None
        try:
            registrar.set_relay_anchor(relay_url)
            registrar.register(Hello, HelloImpl(), name=name)
            assert registrar.get_server_address() is not None

            resolver.set_relay_anchor(relay_url)
            crm = resolver.connect(Hello, name=name)
            assert crm.client._mode == 'ipc'
            assert crm.greeting('RuntimeRelay') == 'Hello, RuntimeRelay!'
        finally:
            if crm is not None:
                resolver.close(crm)
            resolver.shutdown()
            settings.relay_anchor_address = previous_relay
            registrar.shutdown()
            settings.relay_anchor_address = previous_relay

    def test_name_only_connect_uses_verified_ipc_for_local_anchor(self, start_c3_relay):
        name = 'identity/verified/local-ipc'
        relay = start_c3_relay()
        relay_url = relay.url
        previous_relay = settings.relay_anchor_address
        registrar = _ProcessRegistry()
        resolver = _ProcessRegistry()
        crm = None
        try:
            registrar.set_relay_anchor(relay_url)
            registrar.register(Hello, HelloImpl(), name=name)
            resolver.set_relay_anchor(relay_url)

            crm = resolver.connect(Hello, name=name)

            assert crm.client._mode == 'ipc'  # noqa: SLF001
            assert crm.greeting('Identity') == 'Hello, Identity!'
        finally:
            if crm is not None:
                resolver.close(crm)
            resolver.shutdown()
            settings.relay_anchor_address = previous_relay
            registrar.shutdown()
            settings.relay_anchor_address = previous_relay

    def test_relay_local_ipc_contract_mismatch_does_not_fallback_to_http(self, start_c3_relay):
        name = 'identity/local-ipc-contract-mismatch'
        relay = start_c3_relay()
        relay_url = relay.url
        previous_relay = settings.relay_anchor_address
        registrar = _ProcessRegistry()
        resolver = _ProcessRegistry()
        try:
            registrar.register(Hello, HelloImpl(), name=name)
            ipc_addr = registrar.get_server_address()
            server_id = registrar.get_server_id()
            assert ipc_addr is not None
            assert server_id is not None
            server_instance_id = _server_instance_id_for(
                registrar,
                ipc_addr,
                route_name=name,
            )

            wrong_contract = crm_contract(Counter)
            with httpx.Client(trust_env=False, timeout=5.0) as http:
                resp = http.post(
                    f'{relay_url}/_register',
                    json={
                        'name': name,
                        'server_id': server_id,
                        'server_instance_id': server_instance_id,
                        'address': ipc_addr,
                        'crm_ns': wrong_contract.crm_ns,
                        'crm_name': wrong_contract.crm_name,
                        'crm_ver': wrong_contract.crm_ver,
                        'abi_hash': wrong_contract.abi_hash,
                        'signature_hash': wrong_contract.signature_hash,
                        'max_payload_size': DEFAULT_MAX_PAYLOAD_SIZE,
                    },
                )
                assert resp.status_code == 400
                assert 'CRM contract mismatch' in resp.text
        finally:
            resolver.shutdown()
            settings.relay_anchor_address = previous_relay
            registrar.shutdown()
            settings.relay_anchor_address = previous_relay

    def test_no_address_relay_connect_maps_missing_route_to_resource_not_found(self, start_c3_relay):
        relay = start_c3_relay()
        previous_relay = settings.relay_anchor_address
        registry = _ProcessRegistry()
        try:
            registry.set_relay_anchor(relay.url)
            with pytest.raises(ResourceNotFound, match="route not found: missing-route"):
                registry.connect(Hello, name='missing-route')
        finally:
            registry.shutdown()
            settings.relay_anchor_address = previous_relay

    def test_relay_traffic_bypasses_system_proxy(self, monkeypatch, start_c3_relay):
        """All relay HTTP traffic must ignore HTTP_PROXY by default.

        Regression: when a user sets HTTP_PROXY/HTTPS_PROXY (e.g. a corporate
        forward proxy or a docker host proxy), relay control traffic must not
        flow through it by default. Proxies are known to normalize
        percent-encoded ``%2F`` in URL paths to ``/`` — which then breaks
        resource names containing ``/``. This test points HTTP_PROXY at a
        black-hole address and verifies that name resolution + a CRM call
        still succeed (i.e., proxy was bypassed).
        """
        # Black-hole proxy: connecting to TEST-NET-1 (RFC 5737) is guaranteed
        # to time out / fail. If the request actually goes through the proxy,
        # the test will fail loudly.
        for var in ('http_proxy', 'https_proxy', 'HTTP_PROXY', 'HTTPS_PROXY'):
            monkeypatch.setenv(var, 'http://192.0.2.1:9')
        for var in ('no_proxy', 'NO_PROXY'):
            monkeypatch.delenv(var, raising=False)

        name = 'proxy/bypass/test'
        relay = start_c3_relay()
        relay_url = relay.url
        previous_relay = settings.relay_anchor_address
        registrar = _ProcessRegistry()
        resolver = _ProcessRegistry()
        crm = None
        try:
            registrar.set_relay_anchor(relay_url)
            # Registration uses the Rust relay control client and must bypass proxies.
            registrar.register(Hello, HelloImpl(), name=name)
            resolver.set_relay_anchor(relay_url)

            # No-address connect resolves through the Rust relay control client.
            crm = resolver.connect(Hello, name=name)
            # reqwest call must succeed (Rust HttpClient builder bypasses proxy).
            assert crm.greeting('NoProxy') == 'Hello, NoProxy!'
        finally:
            if crm is not None:
                resolver.close(crm)
            resolver.shutdown()
            settings.relay_anchor_address = previous_relay
            registrar.shutdown()
            settings.relay_anchor_address = previous_relay


# ---------------------------------------------------------------------------
# Control-plane endpoint tests
# ---------------------------------------------------------------------------

class TestRelayControlPlane:
    """Test the relay control-plane endpoints (/_register, /_unregister, /_routes)."""

    def test_native_control_client_does_not_expose_name_only_resolve(self, start_c3_relay):
        from c_two import _native

        start_c3_relay()
        assert not hasattr(_native, 'RustRelayControlClient')

    def test_register_via_http_control(self, start_c3_relay):
        """POST /_register adds an upstream and allows calls."""
        cc.register(Hello, HelloImpl(), name='hello')
        ipc_addr = cc.server_address()
        server_id = cc.server_id()
        server_instance_id = _server_instance_id_for(_ProcessRegistry.get(), ipc_addr)

        relay = start_c3_relay()
        relay_url = relay.url

        with httpx.Client(trust_env=False, timeout=5.0) as http:
            # Register via control endpoint.
            resp = http.post(
                f'{relay_url}/_register',
                json={
                    'name': 'hello',
                    'server_id': server_id,
                    'server_instance_id': server_instance_id,
                    'address': ipc_addr,
                    'max_payload_size': DEFAULT_MAX_PAYLOAD_SIZE,
                },
            )
            assert resp.status_code == 201, resp.text
            assert resp.json()['registered'] == 'hello'

            # Verify route appears in /_routes.
            resp = http.get(f'{relay_url}/_routes')
            assert resp.status_code == 200
            routes = resp.json()['routes']
            assert any(r['name'] == 'hello' for r in routes)
            route = next(r for r in routes if r['name'] == 'hello')
            assert 'server_id' not in route
            assert 'server_instance_id' not in route
            assert 'ipc_address' not in route

        # Verify route-bound data-plane call works.
        crm = cc.connect(Hello, name='hello', address=relay_url)
        try:
            assert crm.greeting('Control') == 'Hello, Control!'
        finally:
            cc.close(crm)

    def test_register_duplicate_upsert(self, start_c3_relay):
        """POST /_register with same name upserts (returns 201)."""
        cc.register(Hello, HelloImpl(), name='hello')
        ipc_addr = cc.server_address()
        server_id = cc.server_id()
        server_instance_id = _server_instance_id_for(_ProcessRegistry.get(), ipc_addr)

        relay = start_c3_relay()
        relay_url = relay.url

        with httpx.Client(trust_env=False, timeout=5.0) as http:
            resp = http.post(
                f'{relay_url}/_register',
                json={
                    'name': 'hello',
                    'server_id': server_id,
                    'server_instance_id': server_instance_id,
                    'address': ipc_addr,
                    'max_payload_size': DEFAULT_MAX_PAYLOAD_SIZE,
                },
            )
            assert resp.status_code == 201, resp.text

            # Same-relay duplicate registration uses upsert semantics.
            resp = http.post(
                f'{relay_url}/_register',
                json={
                    'name': 'hello',
                    'server_id': server_id,
                    'server_instance_id': server_instance_id,
                    'address': ipc_addr,
                    'max_payload_size': DEFAULT_MAX_PAYLOAD_SIZE,
                },
            )
            assert resp.status_code == 200

    def test_unregister_removes_route(self, start_c3_relay):
        """POST /_unregister removes the route; calls return 404."""
        cc.register(Hello, HelloImpl(), name='hello')
        ipc_addr = cc.server_address()
        server_id = cc.server_id()
        server_instance_id = _server_instance_id_for(_ProcessRegistry.get(), ipc_addr)

        relay = start_c3_relay()
        relay_url = relay.url

        with httpx.Client(trust_env=False, timeout=5.0) as http:
            # Register, then unregister.
            http.post(
                f'{relay_url}/_register',
                json={
                    'name': 'hello',
                    'server_id': server_id,
                    'server_instance_id': server_instance_id,
                    'address': ipc_addr,
                    'max_payload_size': DEFAULT_MAX_PAYLOAD_SIZE,
                },
            )
            resp = http.post(
                f'{relay_url}/_unregister',
                json={'name': 'hello', 'server_id': server_id},
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
                headers=_expected_contract_headers(),
            )
            assert resp.status_code == 404

    def test_unregister_missing_404(self, start_c3_relay):
        """POST /_unregister for unknown name returns 404."""
        relay = start_c3_relay()
        relay_url = relay.url

        with httpx.Client(trust_env=False, timeout=5.0) as http:
            resp = http.post(
                f'{relay_url}/_unregister',
                json={'name': 'nonexistent', 'server_id': 'missing-server'},
            )
            assert resp.status_code == 404

    def test_health_shows_registered_routes(self, start_c3_relay):
        """GET /health lists all registered route names."""
        cc.register(Hello, HelloImpl(), name='hello')
        cc.register(Counter, CounterImpl(), name='counter')
        ipc_addr = cc.server_address()
        server_id = cc.server_id()
        server_instance_id = _server_instance_id_for(_ProcessRegistry.get(), ipc_addr)

        relay = start_c3_relay()
        relay_url = relay.url

        with httpx.Client(trust_env=False, timeout=5.0) as http:
            for name in ('hello', 'counter'):
                resp = http.post(
                    f'{relay_url}/_register',
                    json={
                        'name': name,
                        'server_id': server_id,
                        'server_instance_id': server_instance_id,
                        'address': ipc_addr,
                        'max_payload_size': DEFAULT_MAX_PAYLOAD_SIZE,
                    },
                )
                assert resp.status_code == 201, resp.text

            resp = http.get(f'{relay_url}/health')
            health = resp.json()
            assert health['status'] == 'ok'
            assert set(health['routes']) == {'hello', 'counter'}

    def test_call_unknown_route_404(self, start_c3_relay):
        """POST /{route}/{method} for unregistered route returns 404."""
        relay = start_c3_relay()
        relay_url = relay.url

        with httpx.Client(trust_env=False, timeout=5.0) as http:
            resp = http.post(
                f'{relay_url}/nonexistent/method',
                content=b'data',
                headers=_expected_contract_headers(),
            )
            assert resp.status_code == 404
            data = resp.json()
            assert data['version'] == 1
            assert data['code'] == 701
            assert data['name'] == 'ResourceNotFound'
            assert data['details']['route'] == 'nonexistent'
