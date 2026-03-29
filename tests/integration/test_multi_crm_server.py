"""Integration tests for multi-CRM Server.

Tests that a single Server can host multiple CRM resources under
distinct routing names, routed via control-plane name field.
"""
from __future__ import annotations

import os
import pickle
import time
import uuid

import pytest

from c_two.transport import Server, ConcurrencyConfig, ConcurrencyMode
from c_two.transport.client.core import SharedClient
from c_two.transport.wire import MethodTable

from tests.fixtures.ihello import IHello
from tests.fixtures.hello import Hello
from tests.fixtures.counter import ICounter, Counter


_IPC_SOCK_DIR = os.environ.get('CC_IPC_SOCK_DIR', '/tmp/c_two_ipc')


def _unique_region() -> str:
    return f'test_multi_{uuid.uuid4().hex[:12]}'


def _wait_for_server(addr: str, timeout: float = 5.0) -> None:
    region_id = addr.replace('ipc://', '')
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
    """Start a Server hosting Hello + Counter CRMs."""
    addr = f'ipc://{_unique_region()}'
    server = Server(bind_address=addr)
    server.register_crm(IHello, Hello(), name='hello')
    server.register_crm(ICounter, Counter(initial=100), name='counter')
    server.start()
    _wait_for_server(addr)
    yield addr, server
    server.shutdown()


@pytest.fixture
def single_then_add_addr():
    """Start with one CRM, add second after start."""
    addr = f'ipc://{_unique_region()}'
    server = Server(
        bind_address=addr,
        icrm_class=IHello,
        crm_instance=Hello(),
        name='hello',
    )
    server.start()
    _wait_for_server(addr)
    yield addr, server
    server.shutdown()


# ---------------------------------------------------------------------------
# Tests — registration API
# ---------------------------------------------------------------------------

class TestRegistrationAPI:

    def test_register_returns_name(self):
        addr = f'ipc://{_unique_region()}'
        server = Server(bind_address=addr)
        result = server.register_crm(IHello, Hello(), name='hello')
        assert result == 'hello'
        server.shutdown()

    def test_duplicate_name_raises(self):
        addr = f'ipc://{_unique_region()}'
        server = Server(bind_address=addr)
        server.register_crm(IHello, Hello(), name='hello')
        with pytest.raises(ValueError, match='already registered'):
            server.register_crm(IHello, Hello(), name='hello')
        server.shutdown()

    def test_names_property(self):
        addr = f'ipc://{_unique_region()}'
        server = Server(bind_address=addr)
        server.register_crm(IHello, Hello(), name='hello')
        server.register_crm(ICounter, Counter(), name='counter')
        assert set(server.names) == {'hello', 'counter'}
        server.shutdown()

    def test_unregister(self):
        addr = f'ipc://{_unique_region()}'
        server = Server(bind_address=addr)
        server.register_crm(IHello, Hello(), name='hello')
        server.register_crm(ICounter, Counter(), name='counter')
        server.unregister_crm('counter')
        assert server.names == ['hello']
        server.shutdown()

    def test_unregister_unknown_raises(self):
        addr = f'ipc://{_unique_region()}'
        server = Server(bind_address=addr)
        with pytest.raises(KeyError):
            server.unregister_crm('nonexistent')
        server.shutdown()

    def test_no_crm_constructor(self):
        """Server can be created without any initial CRM."""
        addr = f'ipc://{_unique_region()}'
        server = Server(bind_address=addr)
        assert server.names == []
        server.shutdown()


# ---------------------------------------------------------------------------
# Tests — multi-CRM routing
# ---------------------------------------------------------------------------

class TestMultiCRMRouting:

    def test_hello_greeting(self, multi_crm_addr):
        addr, _server = multi_crm_addr
        client = SharedClient(addr)
        client.connect()
        try:
            result = pickle.loads(client.call(
                'greeting', pickle.dumps(('World',)), name='hello',
            ))
            assert result == 'Hello, World!'
        finally:
            client.terminate()

    def test_counter_get(self, multi_crm_addr):
        addr, _server = multi_crm_addr
        client = SharedClient(addr)
        client.connect()
        try:
            result = pickle.loads(client.call(
                'get', pickle.dumps(()), name='counter',
            ))
            assert result == 100  # initial=100 in fixture
        finally:
            client.terminate()

    def test_both_names_same_client(self, multi_crm_addr):
        """A single client can call methods on both CRMs."""
        addr, _server = multi_crm_addr
        client = SharedClient(addr)
        client.connect()
        try:
            r1 = pickle.loads(client.call(
                'greeting', pickle.dumps(('Multi',)), name='hello',
            ))
            r2 = pickle.loads(client.call(
                'get', pickle.dumps(()), name='counter',
            ))
            assert r1 == 'Hello, Multi!'
            assert r2 == 100
        finally:
            client.terminate()

    def test_counter_increment(self, multi_crm_addr):
        """Stateful CRM — increment counter and read back."""
        addr, _server = multi_crm_addr
        client = SharedClient(addr)
        client.connect()
        try:
            r1 = pickle.loads(client.call(
                'increment', pickle.dumps((5,)), name='counter',
            ))
            assert r1 == 105  # 100 + 5
            r2 = pickle.loads(client.call(
                'get', pickle.dumps(()), name='counter',
            ))
            assert r2 == 105
        finally:
            client.terminate()

    def test_handshake_returns_all_routes(self, multi_crm_addr):
        addr, _server = multi_crm_addr
        client = SharedClient(addr)
        client.connect()
        try:
            # The method tables should contain methods from both routes.
            name_tables = client._name_tables
            assert 'hello' in name_tables
            assert 'counter' in name_tables
            # Verify method names.
            hello_methods = name_tables['hello']
            assert hello_methods.has_name('greeting')
            assert hello_methods.has_name('add')
            counter_methods = name_tables['counter']
            assert counter_methods.has_name('get')
            assert counter_methods.has_name('increment')
            assert counter_methods.has_name('reset')
        finally:
            client.terminate()

    def test_unknown_name_returns_error(self, multi_crm_addr):
        """Calling a non-existent route name returns an error."""
        addr, _server = multi_crm_addr
        client = SharedClient(addr)
        client.connect()
        try:
            # The name doesn't exist in the server's routing tables,
            # and won't exist in the client's local tables — KeyError.
            with pytest.raises((KeyError, Exception)):
                client.call('greeting', pickle.dumps(('x',)), name='nonexistent')
        finally:
            client.terminate()


# ---------------------------------------------------------------------------
# Tests — v1 backward compat with multi-CRM
# ---------------------------------------------------------------------------

class TestMultiCRMV1Compat:

    def test_v1_uses_default_name(self, multi_crm_addr):
        """V1 calls (no name in wire) route to the first registered CRM."""
        addr, _server = multi_crm_addr
        client = SharedClient(addr)
        client.connect()
        try:
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
        assert server.names == ['hello']

        # Add counter CRM dynamically.
        server.register_crm(ICounter, Counter(initial=42), name='counter')
        assert set(server.names) == {'hello', 'counter'}

        # Connect and call the new CRM.
        client = SharedClient(addr)
        client.connect()
        try:
            result = pickle.loads(client.call(
                'get', pickle.dumps(()), name='counter',
            ))
            assert result == 42
        finally:
            client.terminate()

    def test_unregister_default_shifts(self):
        """Unregistering the default route shifts to the next one."""
        addr = f'ipc://{_unique_region()}'
        server = Server(bind_address=addr)
        server.register_crm(IHello, Hello(), name='hello')
        server.register_crm(ICounter, Counter(), name='counter')

        server.unregister_crm('hello')
        # Default should shift to counter.
        assert server.names == ['counter']
        server.shutdown()
