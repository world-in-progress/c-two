"""Unit tests for CRM routing name collision prevention.

Verifies that:
- Duplicate names are rejected at both Server and registry level.
- Same CRM class can be registered under different names.
- Different CRM classes can co-exist with unique names.
"""
from __future__ import annotations

import os
import uuid

import pytest

from c_two.transport import Server, ConcurrencyConfig, ConcurrencyMode
from c_two.transport.registry import _ProcessRegistry

from tests.fixtures.ihello import Hello
from tests.fixtures.hello import HelloImpl
from tests.fixtures.counter import Counter, CounterImpl


_IPC_SOCK_DIR = os.environ.get('CC_IPC_SOCK_DIR', '/tmp/c_two_ipc')


def _unique_addr() -> str:
    return f'ipc://test_collision_{uuid.uuid4().hex[:12]}'


# ---------------------------------------------------------------------------
# Server-level name collision
# ---------------------------------------------------------------------------

class TestServerNameCollision:

    def test_duplicate_name_raises(self):
        """Registering two CRMs under the same name raises ValueError."""
        server = Server(bind_address=_unique_addr())
        server.register_crm(Hello, HelloImpl(), name='my_hello')
        with pytest.raises(ValueError, match='already registered'):
            server.register_crm(Hello, HelloImpl(), name='my_hello')
        server.shutdown()

    def test_duplicate_name_different_icrm_raises(self):
        """Different CRM classes under the same name also raises."""
        server = Server(bind_address=_unique_addr())
        server.register_crm(Hello, HelloImpl(), name='shared_name')
        with pytest.raises(ValueError, match='already registered'):
            server.register_crm(Counter, CounterImpl(), name='shared_name')
        server.shutdown()

    def test_same_icrm_different_names_ok(self):
        """Same CRM class can be registered under distinct names."""
        server = Server(bind_address=_unique_addr())
        server.register_crm(Hello, HelloImpl(), name='hello_a')
        server.register_crm(Hello, HelloImpl(), name='hello_b')
        assert set(server.names) == {'hello_a', 'hello_b'}
        server.shutdown()

    def test_different_icrm_unique_names_ok(self):
        """Different CRM classes with unique names coexist."""
        server = Server(bind_address=_unique_addr())
        server.register_crm(Hello, HelloImpl(), name='hello')
        server.register_crm(Counter, CounterImpl(), name='counter')
        assert set(server.names) == {'hello', 'counter'}
        server.shutdown()

    def test_reregister_after_unregister(self):
        """After unregistering, the same name can be reused."""
        server = Server(bind_address=_unique_addr())
        server.register_crm(Hello, HelloImpl(), name='reuse_me')
        server.unregister_crm('reuse_me')
        # Should succeed now
        server.register_crm(Counter, CounterImpl(), name='reuse_me')
        assert server.names == ['reuse_me']
        server.shutdown()

    def test_unregister_nonexistent_raises(self):
        """Unregistering a name that was never registered raises KeyError."""
        server = Server(bind_address=_unique_addr())
        with pytest.raises(KeyError):
            server.unregister_crm('does_not_exist')
        server.shutdown()


# ---------------------------------------------------------------------------
# Registry-level name collision
# ---------------------------------------------------------------------------

class TestRegistryNameCollision:

    @pytest.fixture(autouse=True)
    def _clean_registry(self):
        """Reset the global registry before/after each test."""
        _ProcessRegistry.reset()
        yield
        _ProcessRegistry.reset()

    def test_duplicate_name_raises(self):
        registry = _ProcessRegistry.get()
        registry.register(Hello, HelloImpl(), name='hello')
        with pytest.raises(ValueError, match='already registered'):
            registry.register(Hello, HelloImpl(), name='hello')

    def test_duplicate_name_different_icrm_raises(self):
        registry = _ProcessRegistry.get()
        registry.register(Hello, HelloImpl(), name='shared')
        with pytest.raises(ValueError, match='already registered'):
            registry.register(Counter, CounterImpl(), name='shared')

    def test_same_icrm_different_names_ok(self):
        registry = _ProcessRegistry.get()
        registry.register(Hello, HelloImpl(), name='hello_1')
        registry.register(Hello, HelloImpl(), name='hello_2')
        assert set(registry.names) == {'hello_1', 'hello_2'}

    def test_reregister_after_unregister(self):
        registry = _ProcessRegistry.get()
        registry.register(Hello, HelloImpl(), name='temp')
        registry.unregister('temp')
        registry.register(Counter, CounterImpl(), name='temp')
        assert registry.names == ['temp']

    def test_unregister_nonexistent_raises(self):
        registry = _ProcessRegistry.get()
        with pytest.raises(KeyError):
            registry.unregister('nope')
