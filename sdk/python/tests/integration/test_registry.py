"""Integration tests for the SOTA ``cc.register / cc.connect`` API.

Tests cover:
- Thread-local preference (same-process zero-serde)
- IPC connect (cross-process via SOTA API)
- Multi-CRM registration
- Lifecycle: register → connect → close → unregister → shutdown
- Error cases
"""
from __future__ import annotations

import threading
import time
from unittest.mock import patch

import pytest

import c_two as cc
from c_two._native import NativeRelay
from c_two.error import ResourceAlreadyRegistered
from c_two.transport.registry import _ProcessRegistry
from c_two.transport.client.proxy import CRMProxy

# Re-use the existing test fixtures.
from tests.fixtures.ihello import Hello
from tests.fixtures.hello import HelloImpl
from tests.fixtures.counter import Counter, CounterImpl


_port_counter = 0
_port_lock = threading.Lock()


def _next_relay_port() -> int:
    global _port_counter
    with _port_lock:
        _port_counter += 1
        return 19150 + _port_counter


@pytest.fixture(autouse=True)
def _clean_registry():
    """Ensure a clean registry for every test."""
    _ProcessRegistry.reset()
    yield
    _ProcessRegistry.reset()


# ------------------------------------------------------------------
# Basic lifecycle
# ------------------------------------------------------------------

class TestRegisterConnect:
    """Core register / connect / close / unregister cycle."""

    def test_register_returns_name(self):
        n = cc.register(Hello, HelloImpl(), name='hello')
        assert n == 'hello'

    def test_connect_thread_local(self):
        """Same-process connect returns thread-local proxy (zero serde)."""
        cc.register(Hello, HelloImpl(), name='hello')
        crm = cc.connect(Hello, name='hello')
        try:
            assert crm.client.supports_direct_call is True
            assert crm.client._mode == 'thread'
        finally:
            cc.close(crm)

    def test_connect_thread_local_call(self):
        """Thread-local proxy actually invokes CRM methods."""
        cc.register(Hello, HelloImpl(), name='hello')
        crm = cc.connect(Hello, name='hello')
        try:
            result = crm.greeting('World')
            assert result == 'Hello, World!'
        finally:
            cc.close(crm)

    def test_connect_ipc(self):
        """IPC connect via explicit address returns ipc-mode proxy."""
        cc.register(Hello, HelloImpl(), name='hello')
        addr = cc.server_address()
        assert addr is not None

        crm = cc.connect(Hello, name='hello', address=addr)
        try:
            assert crm.client.supports_direct_call is False
            assert crm.client._mode == 'ipc'
            result = crm.greeting('IPC')
            assert result == 'Hello, IPC!'
        finally:
            cc.close(crm)

    def test_close_terminates_proxy(self):
        cc.register(Hello, HelloImpl(), name='hello')
        crm = cc.connect(Hello, name='hello')
        cc.close(crm)
        assert crm.client._closed is True

    def test_unregister(self):
        cc.register(Hello, HelloImpl(), name='hello')
        cc.unregister('hello')
        assert 'hello' not in _ProcessRegistry.get().names

    def test_server_address_populated(self):
        assert cc.server_address() is None
        cc.register(Hello, HelloImpl(), name='hello')
        addr = cc.server_address()
        assert addr is not None
        assert addr.startswith('ipc://')

    def test_shutdown_cleans_everything(self):
        cc.register(Hello, HelloImpl(), name='hello')
        assert cc.server_address() is not None
        cc.shutdown()
        assert cc.server_address() is None
        assert _ProcessRegistry.get().names == []


# ------------------------------------------------------------------
# Multi-CRM
# ------------------------------------------------------------------

class TestMultiCRM:
    """Multiple CRMs registered in the same process."""

    def test_register_two_crms(self):
        cc.register(Hello, HelloImpl(), name='hello')
        cc.register(Counter, CounterImpl(), name='counter')
        reg = _ProcessRegistry.get()
        assert sorted(reg.names) == ['counter', 'hello']

    def test_same_icrm_different_names(self):
        """Same CRM class can be registered under different names."""
        cc.register(Counter, CounterImpl(initial=10), name='counter_a')
        cc.register(Counter, CounterImpl(initial=20), name='counter_b')
        a = cc.connect(Counter, name='counter_a')
        b = cc.connect(Counter, name='counter_b')
        try:
            assert a.get() == 10
            assert b.get() == 20
        finally:
            cc.close(a)
            cc.close(b)

    def test_same_icrm_different_names_ipc(self):
        """Same CRM class, different names, via IPC."""
        cc.register(Counter, CounterImpl(initial=10), name='counter_a')
        cc.register(Counter, CounterImpl(initial=20), name='counter_b')
        addr = cc.server_address()

        a = cc.connect(Counter, name='counter_a', address=addr)
        b = cc.connect(Counter, name='counter_b', address=addr)
        try:
            assert a.get() == 10
            assert b.get() == 20
            a.increment(5)
            assert a.get() == 15
            assert b.get() == 20  # independent
        finally:
            cc.close(a)
            cc.close(b)

    def test_connect_each_crm_thread_local(self):
        cc.register(Hello, HelloImpl(), name='hello')
        cc.register(Counter, CounterImpl(), name='counter')

        hello = cc.connect(Hello, name='hello')
        counter = cc.connect(Counter, name='counter')
        try:
            assert hello.greeting('A') == 'Hello, A!'
            assert counter.increment(1) == 1
            assert counter.increment(1) == 2
            assert counter.get() == 2
        finally:
            cc.close(hello)
            cc.close(counter)

    def test_connect_each_crm_ipc(self):
        cc.register(Hello, HelloImpl(), name='hello')
        cc.register(Counter, CounterImpl(), name='counter')
        addr = cc.server_address()

        hello = cc.connect(Hello, name='hello', address=addr)
        counter = cc.connect(Counter, name='counter', address=addr)
        try:
            assert hello.greeting('B') == 'Hello, B!'
            assert counter.increment(1) == 1
            assert counter.get() == 1
        finally:
            cc.close(hello)
            cc.close(counter)


# ------------------------------------------------------------------
# Thread preference verification
# ------------------------------------------------------------------

class TestThreadPreference:
    """Verify thread-local optimization avoids serialization."""

    def test_thread_local_returns_same_crm_instance(self):
        """Thread-local proxy delegates to the exact CRM instance."""
        impl = HelloImpl()
        cc.register(Hello, impl, name='hello')
        crm = cc.connect(Hello, name='hello')
        try:
            assert crm.client._crm is impl
        finally:
            cc.close(crm)

    def test_multiple_connects_get_independent_proxies(self):
        cc.register(Hello, HelloImpl(), name='hello')
        a = cc.connect(Hello, name='hello')
        b = cc.connect(Hello, name='hello')
        try:
            assert a is not b
            assert a.client is not b.client
            # Both work independently.
            assert a.greeting('X') == 'Hello, X!'
            assert b.greeting('Y') == 'Hello, Y!'
        finally:
            cc.close(a)
            cc.close(b)


# ------------------------------------------------------------------
# Concurrent access
# ------------------------------------------------------------------

class TestConcurrency:
    """Concurrent register / connect safety."""

    def test_concurrent_thread_local_calls(self):
        cc.register(Counter, CounterImpl(), name='counter')
        n_threads = 8
        n_per_thread = 50
        errors: list[Exception] = []

        def worker():
            crm = cc.connect(Counter, name='counter')
            try:
                for _ in range(n_per_thread):
                    crm.increment(1)
            except Exception as e:
                errors.append(e)
            finally:
                cc.close(crm)

        threads = [threading.Thread(target=worker) for _ in range(n_threads)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        assert errors == []
        # Counter is shared, so total increments = n_threads * n_per_thread.
        check = cc.connect(Counter, name='counter')
        try:
            assert check.get() == n_threads * n_per_thread
        finally:
            cc.close(check)


# ------------------------------------------------------------------
# Error cases
# ------------------------------------------------------------------

class TestErrors:
    """Error handling and edge cases."""

    def test_register_duplicate_name_raises(self):
        cc.register(Hello, HelloImpl(), name='hello')
        with pytest.raises(ValueError, match='already registered'):
            cc.register(Hello, HelloImpl(), name='hello')

    def test_connect_nonexistent_raises(self):
        with pytest.raises(LookupError, match='not registered'):
            cc.connect(Hello, name='nope')

    def test_unregister_nonexistent_raises(self):
        with pytest.raises(KeyError, match='not registered'):
            cc.unregister('nope')

    def test_close_idempotent(self):
        """Calling close twice doesn't raise."""
        cc.register(Hello, HelloImpl(), name='hello')
        crm = cc.connect(Hello, name='hello')
        cc.close(crm)
        cc.close(crm)  # should not raise

    def test_register_then_unregister_then_reregister(self):
        cc.register(Hello, HelloImpl(), name='hello')
        cc.unregister('hello')
        cc.register(Hello, HelloImpl(), name='hello')  # should succeed
        crm = cc.connect(Hello, name='hello')
        try:
            assert crm.greeting('Z') == 'Hello, Z!'
        finally:
            cc.close(crm)

    def test_relay_registration_failure_rolls_back_local_registration(self):
        registry = _ProcessRegistry.get()
        with patch.object(
            registry,
            '_relay_register',
            side_effect=ResourceAlreadyRegistered("Route name already registered: 'hello'"),
        ):
            with pytest.raises(ResourceAlreadyRegistered, match='already registered'):
                cc.register(Hello, HelloImpl(), name='hello')

        assert 'hello' not in registry.names
        assert cc.server_address() is None

    def test_relay_rejects_duplicate_name_from_second_registry(self):
        relay = NativeRelay(f'127.0.0.1:{_next_relay_port()}')
        relay.start()
        relay_url = f'http://{relay.bind_address}'

        first = _ProcessRegistry()
        second = _ProcessRegistry()
        try:
            first.set_relay(relay_url)
            second.set_relay(relay_url)

            first.register(Hello, HelloImpl(), name='hello')

            with pytest.raises(ResourceAlreadyRegistered, match='already registered'):
                second.register(Hello, HelloImpl(), name='hello')

            assert second.names == []
            assert second.get_server_address() is None
        finally:
            second.shutdown()
            first.shutdown()
            relay.stop()
