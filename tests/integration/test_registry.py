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

import pytest

import c_two as cc
from c_two.transport.registry import _ProcessRegistry
from c_two.transport.client.proxy import ICRMProxy

# Re-use the existing test fixtures.
from tests.fixtures.ihello import IHello
from tests.fixtures.hello import Hello
from tests.fixtures.counter import ICounter, Counter


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
        n = cc.register(IHello, Hello(), name='hello')
        assert n == 'hello'

    def test_connect_thread_local(self):
        """Same-process connect returns thread-local proxy (zero serde)."""
        cc.register(IHello, Hello(), name='hello')
        icrm = cc.connect(IHello, name='hello')
        try:
            assert icrm.client.supports_direct_call is True
            assert icrm.client._mode == 'thread'
        finally:
            cc.close(icrm)

    def test_connect_thread_local_call(self):
        """Thread-local proxy actually invokes CRM methods."""
        cc.register(IHello, Hello(), name='hello')
        icrm = cc.connect(IHello, name='hello')
        try:
            result = icrm.greeting('World')
            assert result == 'Hello, World!'
        finally:
            cc.close(icrm)

    def test_connect_ipc(self):
        """IPC connect via explicit address returns ipc-mode proxy."""
        cc.register(IHello, Hello(), name='hello')
        addr = cc.server_address()
        assert addr is not None

        icrm = cc.connect(IHello, name='hello', address=addr)
        try:
            assert icrm.client.supports_direct_call is False
            assert icrm.client._mode == 'ipc'
            result = icrm.greeting('IPC')
            assert result == 'Hello, IPC!'
        finally:
            cc.close(icrm)

    def test_close_terminates_proxy(self):
        cc.register(IHello, Hello(), name='hello')
        icrm = cc.connect(IHello, name='hello')
        cc.close(icrm)
        assert icrm.client._closed is True

    def test_unregister(self):
        cc.register(IHello, Hello(), name='hello')
        cc.unregister('hello')
        assert 'hello' not in _ProcessRegistry.get().names

    def test_server_address_populated(self):
        assert cc.server_address() is None
        cc.register(IHello, Hello(), name='hello')
        addr = cc.server_address()
        assert addr is not None
        assert addr.startswith('ipc://')

    def test_shutdown_cleans_everything(self):
        cc.register(IHello, Hello(), name='hello')
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
        cc.register(IHello, Hello(), name='hello')
        cc.register(ICounter, Counter(), name='counter')
        reg = _ProcessRegistry.get()
        assert sorted(reg.names) == ['counter', 'hello']

    def test_same_icrm_different_names(self):
        """Same ICRM class can be registered under different names."""
        cc.register(ICounter, Counter(initial=10), name='counter_a')
        cc.register(ICounter, Counter(initial=20), name='counter_b')
        a = cc.connect(ICounter, name='counter_a')
        b = cc.connect(ICounter, name='counter_b')
        try:
            assert a.get() == 10
            assert b.get() == 20
        finally:
            cc.close(a)
            cc.close(b)

    def test_same_icrm_different_names_ipc(self):
        """Same ICRM class, different names, via IPC."""
        cc.register(ICounter, Counter(initial=10), name='counter_a')
        cc.register(ICounter, Counter(initial=20), name='counter_b')
        addr = cc.server_address()

        a = cc.connect(ICounter, name='counter_a', address=addr)
        b = cc.connect(ICounter, name='counter_b', address=addr)
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
        cc.register(IHello, Hello(), name='hello')
        cc.register(ICounter, Counter(), name='counter')

        hello = cc.connect(IHello, name='hello')
        counter = cc.connect(ICounter, name='counter')
        try:
            assert hello.greeting('A') == 'Hello, A!'
            assert counter.increment(1) == 1
            assert counter.increment(1) == 2
            assert counter.get() == 2
        finally:
            cc.close(hello)
            cc.close(counter)

    def test_connect_each_crm_ipc(self):
        cc.register(IHello, Hello(), name='hello')
        cc.register(ICounter, Counter(), name='counter')
        addr = cc.server_address()

        hello = cc.connect(IHello, name='hello', address=addr)
        counter = cc.connect(ICounter, name='counter', address=addr)
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
        crm = Hello()
        cc.register(IHello, crm, name='hello')
        icrm = cc.connect(IHello, name='hello')
        try:
            assert icrm.client._crm is crm
        finally:
            cc.close(icrm)

    def test_multiple_connects_get_independent_proxies(self):
        cc.register(IHello, Hello(), name='hello')
        a = cc.connect(IHello, name='hello')
        b = cc.connect(IHello, name='hello')
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
        cc.register(ICounter, Counter(), name='counter')
        n_threads = 8
        n_per_thread = 50
        errors: list[Exception] = []

        def worker():
            icrm = cc.connect(ICounter, name='counter')
            try:
                for _ in range(n_per_thread):
                    icrm.increment(1)
            except Exception as e:
                errors.append(e)
            finally:
                cc.close(icrm)

        threads = [threading.Thread(target=worker) for _ in range(n_threads)]
        for t in threads:
            t.start()
        for t in threads:
            t.join()

        assert errors == []
        # Counter is shared, so total increments = n_threads * n_per_thread.
        check = cc.connect(ICounter, name='counter')
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
        cc.register(IHello, Hello(), name='hello')
        with pytest.raises(ValueError, match='already registered'):
            cc.register(IHello, Hello(), name='hello')

    def test_connect_nonexistent_raises(self):
        with pytest.raises(LookupError, match='not registered'):
            cc.connect(IHello, name='nope')

    def test_unregister_nonexistent_raises(self):
        with pytest.raises(KeyError, match='not registered'):
            cc.unregister('nope')

    def test_close_idempotent(self):
        """Calling close twice doesn't raise."""
        cc.register(IHello, Hello(), name='hello')
        icrm = cc.connect(IHello, name='hello')
        cc.close(icrm)
        cc.close(icrm)  # should not raise

    def test_register_then_unregister_then_reregister(self):
        cc.register(IHello, Hello(), name='hello')
        cc.unregister('hello')
        cc.register(IHello, Hello(), name='hello')  # should succeed
        icrm = cc.connect(IHello, name='hello')
        try:
            assert icrm.greeting('Z') == 'Hello, Z!'
        finally:
            cc.close(icrm)
