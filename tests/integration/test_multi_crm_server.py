"""Integration tests for multi-CRM ServerV2.

Tests that a single ServerV2 can host multiple CRM resources under
distinct namespaces, routed via v2 control-plane namespace field.
"""
from __future__ import annotations

import os
import pickle
import time
import uuid

import pytest

from c_two.rpc_v2 import ServerV2, ConcurrencyConfig, ConcurrencyMode
from c_two.rpc_v2.client import SharedClient
from c_two.rpc_v2.wire import MethodTable
from c_two.rpc_v2.protocol import NamespaceInfo

from tests.fixtures.ihello import IHello
from tests.fixtures.hello import Hello
from tests.fixtures.counter import ICounter, Counter


_IPC_SOCK_DIR = os.environ.get('CC_IPC_SOCK_DIR', '/tmp/c_two_ipc')


def _unique_region() -> str:
    return f'test_multi_{uuid.uuid4().hex[:12]}'


def _wait_for_server(addr: str, timeout: float = 5.0) -> None:
    region_id = addr.replace('ipc-v3://', '')
    sock_path = os.path.join(_IPC_SOCK_DIR, f'{region_id}.sock')
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if os.path.exists(sock_path):
            return
        time.sleep(0.05)
    raise RuntimeError(f'Server at {sock_path} not ready')


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------

@pytest.fixture
def multi_crm_addr():
    """Start a ServerV2 hosting Hello + Counter CRMs."""
    addr = f'ipc-v3://{_unique_region()}'
    server = ServerV2(bind_address=addr)
    server.register_crm(IHello, Hello())
    server.register_crm(ICounter, Counter(initial=100))
    server.start()
    _wait_for_server(addr)
    yield addr, server
    server.shutdown()


@pytest.fixture
def single_then_add_addr():
    """Start with one CRM, add second after start."""
    addr = f'ipc-v3://{_unique_region()}'
    server = ServerV2(
        bind_address=addr,
        icrm_class=IHello,
        crm_instance=Hello(),
    )
    server.start()
    _wait_for_server(addr)
    yield addr, server
    server.shutdown()


# ---------------------------------------------------------------------------
# Tests — registration API
# ---------------------------------------------------------------------------

class TestRegistrationAPI:

    def test_register_returns_namespace(self):
        addr = f'ipc-v3://{_unique_region()}'
        server = ServerV2(bind_address=addr)
        ns = server.register_crm(IHello, Hello())
        assert ns == 'test.hello'
        server.shutdown()

    def test_duplicate_namespace_raises(self):
        addr = f'ipc-v3://{_unique_region()}'
        server = ServerV2(bind_address=addr)
        server.register_crm(IHello, Hello())
        with pytest.raises(ValueError, match='already registered'):
            server.register_crm(IHello, Hello())
        server.shutdown()

    def test_namespaces_property(self):
        addr = f'ipc-v3://{_unique_region()}'
        server = ServerV2(bind_address=addr)
        server.register_crm(IHello, Hello())
        server.register_crm(ICounter, Counter())
        assert set(server.namespaces) == {'test.hello', 'test.counter'}
        server.shutdown()

    def test_unregister(self):
        addr = f'ipc-v3://{_unique_region()}'
        server = ServerV2(bind_address=addr)
        server.register_crm(IHello, Hello())
        server.register_crm(ICounter, Counter())
        server.unregister_crm('test.counter')
        assert server.namespaces == ['test.hello']
        server.shutdown()

    def test_unregister_unknown_raises(self):
        addr = f'ipc-v3://{_unique_region()}'
        server = ServerV2(bind_address=addr)
        with pytest.raises(KeyError):
            server.unregister_crm('nonexistent')
        server.shutdown()

    def test_no_crm_constructor(self):
        """ServerV2 can be created without any initial CRM."""
        addr = f'ipc-v3://{_unique_region()}'
        server = ServerV2(bind_address=addr)
        assert server.namespaces == []
        server.shutdown()


# ---------------------------------------------------------------------------
# Tests — multi-CRM v2 routing
# ---------------------------------------------------------------------------

class TestMultiCRMRouting:

    def test_v2_hello_greeting(self, multi_crm_addr):
        addr, _server = multi_crm_addr
        client = SharedClient(addr, try_v2=True)
        client.connect()
        try:
            assert client._v2_mode
            result = pickle.loads(client.call(
                'greeting', pickle.dumps(('World',)), namespace='test.hello',
            ))
            assert result == 'Hello, World!'
        finally:
            client.terminate()

    def test_v2_counter_get(self, multi_crm_addr):
        addr, _server = multi_crm_addr
        client = SharedClient(addr, try_v2=True)
        client.connect()
        try:
            assert client._v2_mode
            result = pickle.loads(client.call(
                'get', pickle.dumps(()), namespace='test.counter',
            ))
            assert result == 100  # initial=100 in fixture
        finally:
            client.terminate()

    def test_v2_both_namespaces_same_client(self, multi_crm_addr):
        """A single client can call methods on both CRMs."""
        addr, _server = multi_crm_addr
        client = SharedClient(addr, try_v2=True)
        client.connect()
        try:
            r1 = pickle.loads(client.call(
                'greeting', pickle.dumps(('Multi',)), namespace='test.hello',
            ))
            r2 = pickle.loads(client.call(
                'get', pickle.dumps(()), namespace='test.counter',
            ))
            assert r1 == 'Hello, Multi!'
            assert r2 == 100
        finally:
            client.terminate()

    def test_v2_counter_increment(self, multi_crm_addr):
        """Stateful CRM — increment counter and read back."""
        addr, _server = multi_crm_addr
        client = SharedClient(addr, try_v2=True)
        client.connect()
        try:
            r1 = pickle.loads(client.call(
                'increment', pickle.dumps((5,)), namespace='test.counter',
            ))
            assert r1 == 105  # 100 + 5
            r2 = pickle.loads(client.call(
                'get', pickle.dumps(()), namespace='test.counter',
            ))
            assert r2 == 105
        finally:
            client.terminate()

    def test_v2_handshake_returns_all_namespaces(self, multi_crm_addr):
        addr, _server = multi_crm_addr
        client = SharedClient(addr, try_v2=True)
        client.connect()
        try:
            assert client._v2_mode
            # The method tables should contain methods from both namespaces.
            ns_tables = client._namespace_tables
            assert 'test.hello' in ns_tables
            assert 'test.counter' in ns_tables
            # Verify method names.
            hello_methods = ns_tables['test.hello']
            assert hello_methods.has_name('greeting')
            assert hello_methods.has_name('add')
            counter_methods = ns_tables['test.counter']
            assert counter_methods.has_name('get')
            assert counter_methods.has_name('increment')
            assert counter_methods.has_name('reset')
        finally:
            client.terminate()

    def test_v2_unknown_namespace_returns_error(self, multi_crm_addr):
        """Calling a non-existent namespace returns an error."""
        addr, _server = multi_crm_addr
        client = SharedClient(addr, try_v2=True)
        client.connect()
        try:
            # The namespace doesn't exist in the server's namespace tables,
            # but also won't exist in the client's local tables — KeyError.
            with pytest.raises((KeyError, Exception)):
                client.call('greeting', pickle.dumps(('x',)), namespace='nonexistent.ns')
        finally:
            client.terminate()


# ---------------------------------------------------------------------------
# Tests — v1 backward compat with multi-CRM
# ---------------------------------------------------------------------------

class TestMultiCRMV1Compat:

    def test_v1_uses_default_namespace(self, multi_crm_addr):
        """V1 calls (no namespace in wire) route to the first registered CRM."""
        addr, _server = multi_crm_addr
        client = SharedClient(addr, try_v2=False)
        client.connect()
        try:
            assert not client._v2_mode
            result = pickle.loads(client.call(
                'greeting', pickle.dumps(('V1Client',)),
            ))
            assert result == 'Hello, V1Client!'
        finally:
            client.terminate()


# ---------------------------------------------------------------------------
# Tests — dynamic registration
# ---------------------------------------------------------------------------

class TestDynamicRegistration:

    def test_add_crm_after_start(self, single_then_add_addr):
        """Register a second CRM after the server is already running."""
        addr, server = single_then_add_addr
        assert server.namespaces == ['test.hello']

        # Add counter CRM dynamically.
        server.register_crm(ICounter, Counter(initial=42))
        assert set(server.namespaces) == {'test.hello', 'test.counter'}

        # Connect with v2 and call the new CRM.
        client = SharedClient(addr, try_v2=True)
        client.connect()
        try:
            assert client._v2_mode
            result = pickle.loads(client.call(
                'get', pickle.dumps(()), namespace='test.counter',
            ))
            assert result == 42
        finally:
            client.terminate()

    def test_unregister_default_shifts(self):
        """Unregistering the default namespace shifts to the next one."""
        addr = f'ipc-v3://{_unique_region()}'
        server = ServerV2(bind_address=addr)
        server.register_crm(IHello, Hello())
        server.register_crm(ICounter, Counter())

        server.unregister_crm('test.hello')
        # Default should shift to test.counter.
        assert server.namespaces == ['test.counter']
        server.shutdown()
