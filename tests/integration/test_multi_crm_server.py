"""Integration tests for multi-CRM Server.

Tests that a single Server can host multiple CRM resources under
distinct routing names, routed via control-plane name field.
"""
from __future__ import annotations

import os
import time
import uuid

import pytest

import c_two as cc
from c_two.transport import Server, ConcurrencyConfig, ConcurrencyMode
from c_two.transport.client.util import ping

from tests.fixtures.ihello import IHello
from tests.fixtures.hello import Hello
from tests.fixtures.counter import ICounter, Counter


def _unique_region() -> str:
    return f'test_multi_{uuid.uuid4().hex[:12]}'


def _wait_for_server(addr: str, timeout: float = 5.0) -> None:
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if ping(addr, timeout=0.5):
            return
        time.sleep(0.05)
    raise RuntimeError(f'Server at {addr} not ready')


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
# Tests — multi-CRM routing via SOTA API
# ---------------------------------------------------------------------------

class TestMultiCRMRouting:

    def test_hello_greeting(self, multi_crm_addr):
        addr, _server = multi_crm_addr
        proxy = cc.connect(IHello, name='hello', address=addr)
        try:
            assert proxy.greeting('World') == 'Hello, World!'
        finally:
            cc.close(proxy)

    def test_counter_get(self, multi_crm_addr):
        addr, _server = multi_crm_addr
        proxy = cc.connect(ICounter, name='counter', address=addr)
        try:
            assert proxy.get() == 100  # initial=100 in fixture
        finally:
            cc.close(proxy)

    def test_both_names_same_server(self, multi_crm_addr):
        """A single server can serve calls to both CRMs."""
        addr, _server = multi_crm_addr
        hello = cc.connect(IHello, name='hello', address=addr)
        counter = cc.connect(ICounter, name='counter', address=addr)
        try:
            assert hello.greeting('Multi') == 'Hello, Multi!'
            assert counter.get() == 100
        finally:
            cc.close(hello)
            cc.close(counter)

    def test_counter_increment(self, multi_crm_addr):
        """Stateful CRM — increment counter and read back."""
        addr, _server = multi_crm_addr
        proxy = cc.connect(ICounter, name='counter', address=addr)
        try:
            assert proxy.increment(5) == 105  # 100 + 5
            assert proxy.get() == 105
        finally:
            cc.close(proxy)

    def test_unknown_name_returns_error(self, multi_crm_addr):
        """Connecting to a non-existent route name raises an error."""
        addr, _server = multi_crm_addr
        with pytest.raises(Exception):
            proxy = cc.connect(IHello, name='nonexistent', address=addr)
            try:
                proxy.greeting('x')
            finally:
                cc.close(proxy)


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
        proxy = cc.connect(ICounter, name='counter', address=addr)
        try:
            assert proxy.get() == 42
        finally:
            cc.close(proxy)

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
