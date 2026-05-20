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
from c_two.config.settings import settings
from c_two.error import ResourceAlreadyRegistered
from c_two.transport.registry import _ProcessRegistry
from c_two.transport.client.proxy import CRMProxy

# Re-use the existing test fixtures.
from tests.fixtures.ihello import Hello
from tests.fixtures.hello import HelloImpl
from tests.fixtures.counter import Counter, CounterImpl


@cc.crm(namespace='test.other_hello', version='0.1.0')
class OtherHello:
    def greeting(self, name: str) -> str:
        ...


@cc.crm(namespace='test.hello', version='0.1.0')
class RenamedHello:
    def greeting(self, name: str) -> str:
        ...


class UndecoratedHello:
    def greeting(self, name: str) -> str:
        ...


class MissingGreetingImpl:
    def other(self, name: str) -> str:
        return name


class BadGreetingAnnotationImpl:
    def greeting(self, name: bytes) -> str:
        return name.decode()

    def add(self, a: int, b: int) -> int:
        return a + b

    def echo_none(self, msg: str) -> str | None:
        return msg

    def get_items(self, ids: list[int]) -> list[str]:
        return []

    def get_data(self, id: int):
        return None


@cc.crm(namespace='test.direct_error', version='0.1.0')
class DirectError:
    def fail(self) -> str:
        ...


class DirectErrorImpl:
    def fail(self) -> str:
        raise RuntimeError('direct resource failed')


@cc.crm(namespace='test.bridge_text', version='0.1.0')
class BridgeText:
    def shout(self, text: str) -> str:
        ...


class BytesTextResource:
    def __init__(self) -> None:
        self.seen: list[bytes] = []

    def shout(self, payload: bytes) -> bytes:
        self.seen.append(payload)
        return payload.upper() + b'!'


def _text_bridge():
    return {
        'shout': cc.bridge(
            input=lambda text: (text.encode(),),
            output=lambda payload: payload.decode(),
        ),
    }


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

    def test_register_rejects_undecorated_contract_before_server_creation(self):
        with pytest.raises(ValueError, match='decorate it with @cc.crm'):
            cc.register(UndecoratedHello, HelloImpl(), name='hello')

        assert cc.server_address() is None

    def test_register_rejects_resource_missing_crm_method_before_server_creation(self):
        with pytest.raises(TypeError, match='missing method'):
            cc.register(Hello, MissingGreetingImpl(), name='hello')

        assert cc.server_address() is None

    def test_register_rejects_resource_annotation_mismatch_before_server_creation(self):
        with pytest.raises(TypeError, match='greeting.name.*str.*bytes'):
            cc.register(Hello, BadGreetingAnnotationImpl(), name='hello')

        assert cc.server_address() is None

    def test_register_accepts_bridged_resource_annotation_mismatch(self):
        resource = BytesTextResource()
        cc.register(BridgeText, resource, name='bridge-text', bridge=_text_bridge())

        crm = cc.connect(BridgeText, name='bridge-text')
        try:
            assert crm.client._mode == 'thread'  # noqa: SLF001
            assert crm.shout('hello') == 'HELLO!'
            assert resource.seen == [b'hello']
        finally:
            cc.close(crm)

    def test_direct_ipc_calls_apply_resource_bridge(self):
        resource = BytesTextResource()
        cc.register(BridgeText, resource, name='bridge-text-ipc', bridge=_text_bridge())
        address = cc.server_address()
        assert address is not None

        crm = cc.connect(BridgeText, name='bridge-text-ipc', address=address)
        try:
            assert crm.client._mode == 'ipc'  # noqa: SLF001
            assert crm.shout('remote') == 'REMOTE!'
            assert resource.seen == [b'remote']
        finally:
            cc.close(crm)

    def test_explicit_http_relay_calls_apply_resource_bridge(self, start_c3_relay):
        previous_relay = settings.relay_anchor_address
        relay = start_c3_relay()
        resource = BytesTextResource()
        try:
            cc.set_relay_anchor(relay.url)
            cc.register(BridgeText, resource, name='bridge-text-relay', bridge=_text_bridge())

            crm = cc.connect(BridgeText, name='bridge-text-relay', address=relay.url)
            try:
                assert crm.client._mode == 'http'  # noqa: SLF001
                assert crm.shout('relay') == 'RELAY!'
                assert resource.seen == [b'relay']
            finally:
                cc.close(crm)
        finally:
            settings.relay_anchor_address = previous_relay

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

    def test_connect_thread_local_resource_error_is_classified(self):
        """Thread-local resource errors should not be masked by client wrapper state."""
        cc.register(DirectError, DirectErrorImpl(), name='direct-error')
        crm = cc.connect(DirectError, name='direct-error')
        try:
            with pytest.raises(cc.error.ResourceExecuteFunction, match='direct resource failed'):
                crm.fail()
        finally:
            cc.close(crm)

    def test_connect_thread_local_rejects_crm_contract_mismatch(self):
        cc.register(Hello, HelloImpl(), name='hello')

        with pytest.raises(TypeError, match='CRM contract mismatch'):
            cc.connect(OtherHello, name='hello')

    def test_connect_thread_local_rejects_crm_name_mismatch(self):
        cc.register(Hello, HelloImpl(), name='hello')

        with pytest.raises(TypeError, match='CRM contract mismatch'):
            cc.connect(RenamedHello, name='hello')

    def test_connect_rejects_undecorated_contract_before_transport(self):
        cc.register(Hello, HelloImpl(), name='hello')

        with pytest.raises(ValueError, match='decorate it with @cc.crm'):
            cc.connect(UndecoratedHello, name='hello')

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

    def test_connect_ipc_rejects_crm_contract_mismatch(self):
        cc.register(Hello, HelloImpl(), name='hello')
        addr = cc.server_address()
        assert addr is not None

        with pytest.raises(RuntimeError, match='CRM contract mismatch'):
            cc.connect(OtherHello, name='hello', address=addr)

    def test_connect_ipc_rejects_crm_name_mismatch(self):
        cc.register(Hello, HelloImpl(), name='hello')
        addr = cc.server_address()
        assert addr is not None

        with pytest.raises(RuntimeError, match='CRM contract mismatch'):
            cc.connect(RenamedHello, name='hello', address=addr)

    def test_validated_ipc_client_is_bound_to_connected_route(self):
        cc.register(Hello, HelloImpl(), name='hello')
        cc.register(Counter, CounterImpl(), name='counter')
        addr = cc.server_address()
        assert addr is not None

        crm = cc.connect(Hello, name='hello', address=addr)
        try:
            assert crm.client._client.route_name == 'hello'  # noqa: SLF001
        finally:
            cc.close(crm)

    def test_raw_ipc_client_rejects_unbound_crm_call(self):
        from c_two._native import RustClientPool

        cc.register(Hello, HelloImpl(), name='hello')
        addr = cc.server_address()
        assert addr is not None

        pool = RustClientPool.instance()
        client = pool.acquire(addr)
        try:
            assert 'hello' in client.route_names()
            with pytest.raises(RuntimeError, match='route-bound client'):
                client.call('greeting', b'')
        finally:
            pool.release(addr)

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

    def test_unreachable_relay_registration_rolls_back_local_registration(self):
        registry = _ProcessRegistry.get()
        previous_relay = settings.relay_anchor_address
        try:
            registry.set_relay_anchor('http://127.0.0.1:9')
            with pytest.raises(Exception, match='relay_prepare'):
                registry.register(Hello, HelloImpl(), name='hello')

            assert 'hello' not in registry.names
            assert registry.get_server_address() is None
            assert registry.get_server_id() is None
        finally:
            settings.relay_anchor_address = previous_relay

    def test_failed_relay_registration_leaves_existing_route_usable(self):
        registry = _ProcessRegistry.get()
        previous_relay = settings.relay_anchor_address
        try:
            cc.register(Hello, HelloImpl(), name='hello')
            registry.set_relay_anchor('http://127.0.0.1:9')
            with pytest.raises(Exception, match='relay_prepare'):
                registry.register(Counter, CounterImpl(), name='counter')

            assert registry.names == ['hello']
            crm = cc.connect(Hello, name='hello')
            try:
                assert crm.greeting('Z') == 'Hello, Z!'
            finally:
                cc.close(crm)
        finally:
            settings.relay_anchor_address = previous_relay

    def test_set_relay_anchor_updates_native_relay_projection_without_python_cache(self):
        registry = _ProcessRegistry()
        previous_relay = settings.relay_anchor_address

        try:
            registry.set_relay_anchor('http://relay-b.test/')

            assert registry._runtime_session.relay_anchor_address_override == 'http://relay-b.test'  # noqa: SLF001
            assert not hasattr(registry, '_relay_control_client')
            assert not hasattr(registry, '_relay_control_address')
        finally:
            registry.shutdown()
            settings.relay_anchor_address = previous_relay

    def test_relay_rejects_duplicate_name_from_second_registry(self, start_c3_relay):
        relay = start_c3_relay()
        relay_url = relay.url

        first = _ProcessRegistry()
        second = _ProcessRegistry()
        previous_relay = settings.relay_anchor_address
        try:
            first.set_relay_anchor(relay_url)
            second.set_relay_anchor(relay_url)

            first.register(Hello, HelloImpl(), name='hello')

            with pytest.raises(ResourceAlreadyRegistered, match='already registered'):
                second.register(Hello, HelloImpl(), name='hello')

            assert second.names == []
            assert second.get_server_address() is None
        finally:
            second.shutdown()
            first.shutdown()
            settings.relay_anchor_address = previous_relay
