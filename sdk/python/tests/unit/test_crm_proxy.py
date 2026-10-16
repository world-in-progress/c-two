"""Unit tests for CRMProxy."""
from __future__ import annotations

import pytest

from c_two.transport.client.proxy import CRMProxy


# ---------------------------------------------------------------------------
# Dummy CRM for thread-local tests
# ---------------------------------------------------------------------------

class _DummyCRM:
    def greet(self, name: str) -> str:
        return f'Hi, {name}'

    def add(self, a: int, b: int) -> int:
        return a + b

    def noop(self) -> None:
        return None


# ---------------------------------------------------------------------------
# Mock client for IPC tests
# ---------------------------------------------------------------------------

class _MockClient:
    """Fake client recording calls (Rust client API)."""

    def __init__(self):
        self.calls: list[tuple] = []

    def call(self, method_name: str, data: bytes = b'') -> bytes:
        self.calls.append((method_name, data))
        return b'result'


# ---------------------------------------------------------------------------
# Thread-local proxy
# ---------------------------------------------------------------------------

class TestThreadLocalProxy:

    def test_supports_direct_call(self):
        proxy = CRMProxy.thread_local(_DummyCRM())
        assert proxy.supports_direct_call is True

    def test_call_direct_basic(self):
        crm = _DummyCRM()
        proxy = CRMProxy.thread_local(crm)
        assert proxy.call_direct('greet', ('World',)) == 'Hi, World'
        assert proxy.call_direct('add', (3, 4)) == 7

    def test_call_direct_no_args(self):
        proxy = CRMProxy.thread_local(_DummyCRM())
        assert proxy.call_direct('noop', ()) is None

    def test_call_direct_missing_method(self):
        proxy = CRMProxy.thread_local(_DummyCRM())
        with pytest.raises(AttributeError, match='nonexistent'):
            proxy.call_direct('nonexistent', ())

    def test_call_raises_not_implemented(self):
        proxy = CRMProxy.thread_local(_DummyCRM())
        with pytest.raises(NotImplementedError, match='call_direct'):
            proxy.call('greet', b'data')

    def test_raw_relay_entrypoint_is_not_available(self):
        proxy = CRMProxy.thread_local(_DummyCRM())
        with pytest.raises(AttributeError, match='relay'):
            proxy.relay(b'data')

    def test_terminate(self):
        terminated = []
        proxy = CRMProxy.thread_local(_DummyCRM(), on_terminate=lambda: terminated.append(True))
        proxy.terminate()
        assert terminated == [True]

    def test_terminate_idempotent(self):
        count = []
        proxy = CRMProxy.thread_local(_DummyCRM(), on_terminate=lambda: count.append(1))
        proxy.terminate()
        proxy.terminate()
        assert len(count) == 1

    def test_call_after_terminate_raises(self):
        proxy = CRMProxy.thread_local(_DummyCRM())
        proxy.terminate()
        with pytest.raises(RuntimeError, match='closed'):
            proxy.call_direct('greet', ('x',))


# ---------------------------------------------------------------------------
# IPC proxy
# ---------------------------------------------------------------------------

class TestIPCProxy:

    def test_supports_direct_call_false(self):
        proxy = CRMProxy.ipc(_MockClient(), 'test.ns')
        assert proxy.supports_direct_call is False

    def test_requires_explicit_non_empty_route_name(self):
        with pytest.raises(ValueError, match='explicit non-empty route name'):
            CRMProxy.ipc(_MockClient(), '')

    def test_call_delegates(self):
        client = _MockClient()
        proxy = CRMProxy.ipc(client, 'test.hello')
        result = proxy.call('greet', b'payload')
        assert result == b'result'
        assert client.calls == [('greet', b'payload')]

    def test_call_none_data(self):
        client = _MockClient()
        proxy = CRMProxy.ipc(client, 'ns')
        proxy.call('method')
        assert client.calls == [('method', b'')]

    def test_raw_relay_entrypoint_is_not_available_on_route_bound_proxy(self):
        client = _MockClient()
        proxy = CRMProxy.ipc(client, 'ns')
        with pytest.raises(AttributeError, match='relay'):
            proxy.relay(b'wire')

    def test_call_direct_raises(self):
        proxy = CRMProxy.ipc(_MockClient(), 'ns')
        with pytest.raises(NotImplementedError, match='thread-local'):
            proxy.call_direct('greet', ('x',))

    def test_terminate_callback(self):
        released = []
        proxy = CRMProxy.ipc(
            _MockClient(), 'ns',
            on_terminate=lambda: released.append(True),
        )
        proxy.terminate()
        assert released == [True]

    def test_call_after_terminate_raises(self):
        proxy = CRMProxy.ipc(_MockClient(), 'ns')
        proxy.terminate()
        with pytest.raises(RuntimeError, match='closed'):
            proxy.call('method', b'data')
