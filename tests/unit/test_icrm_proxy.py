"""Unit tests for ICRMProxy."""
from __future__ import annotations

import pytest

from c_two.transport.client.proxy import ICRMProxy


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
        self.relays: list[bytes] = []

    def call(self, route_name: str, method_name: str, data: bytes = b'') -> bytes:
        self.calls.append((route_name, method_name, data))
        return b'result'

    def relay(self, event_bytes: bytes) -> bytes:
        self.relays.append(event_bytes)
        return b'relay_result'


# ---------------------------------------------------------------------------
# Thread-local proxy
# ---------------------------------------------------------------------------

class TestThreadLocalProxy:

    def test_supports_direct_call(self):
        proxy = ICRMProxy.thread_local(_DummyCRM())
        assert proxy.supports_direct_call is True

    def test_call_direct_basic(self):
        crm = _DummyCRM()
        proxy = ICRMProxy.thread_local(crm)
        assert proxy.call_direct('greet', ('World',)) == 'Hi, World'
        assert proxy.call_direct('add', (3, 4)) == 7

    def test_call_direct_no_args(self):
        proxy = ICRMProxy.thread_local(_DummyCRM())
        assert proxy.call_direct('noop', ()) is None

    def test_call_direct_missing_method(self):
        proxy = ICRMProxy.thread_local(_DummyCRM())
        with pytest.raises(AttributeError, match='nonexistent'):
            proxy.call_direct('nonexistent', ())

    def test_call_raises_not_implemented(self):
        proxy = ICRMProxy.thread_local(_DummyCRM())
        with pytest.raises(NotImplementedError, match='call_direct'):
            proxy.call('greet', b'data')

    def test_relay_raises_not_implemented(self):
        proxy = ICRMProxy.thread_local(_DummyCRM())
        with pytest.raises(NotImplementedError):
            proxy.relay(b'data')

    def test_terminate(self):
        terminated = []
        proxy = ICRMProxy.thread_local(_DummyCRM(), on_terminate=lambda: terminated.append(True))
        proxy.terminate()
        assert terminated == [True]

    def test_terminate_idempotent(self):
        count = []
        proxy = ICRMProxy.thread_local(_DummyCRM(), on_terminate=lambda: count.append(1))
        proxy.terminate()
        proxy.terminate()
        assert len(count) == 1

    def test_call_after_terminate_raises(self):
        proxy = ICRMProxy.thread_local(_DummyCRM())
        proxy.terminate()
        with pytest.raises(RuntimeError, match='closed'):
            proxy.call_direct('greet', ('x',))


# ---------------------------------------------------------------------------
# IPC proxy
# ---------------------------------------------------------------------------

class TestIPCProxy:

    def test_supports_direct_call_false(self):
        proxy = ICRMProxy.ipc(_MockClient(), 'test.ns')
        assert proxy.supports_direct_call is False

    def test_call_delegates(self):
        client = _MockClient()
        proxy = ICRMProxy.ipc(client, 'test.hello')
        result = proxy.call('greet', b'payload')
        assert result == b'result'
        assert client.calls == [('test.hello', 'greet', b'payload')]

    def test_call_none_data(self):
        client = _MockClient()
        proxy = ICRMProxy.ipc(client, 'ns')
        proxy.call('method')
        assert client.calls == [('ns', 'method', b'')]

    def test_relay_delegates(self):
        client = _MockClient()
        proxy = ICRMProxy.ipc(client, 'ns')
        result = proxy.relay(b'wire')
        assert result == b'relay_result'
        assert client.relays == [b'wire']

    def test_call_direct_raises(self):
        proxy = ICRMProxy.ipc(_MockClient(), 'ns')
        with pytest.raises(NotImplementedError, match='thread-local'):
            proxy.call_direct('greet', ('x',))

    def test_terminate_callback(self):
        released = []
        proxy = ICRMProxy.ipc(
            _MockClient(), 'ns',
            on_terminate=lambda: released.append(True),
        )
        proxy.terminate()
        assert released == [True]

    def test_call_after_terminate_raises(self):
        proxy = ICRMProxy.ipc(_MockClient(), 'ns')
        proxy.terminate()
        with pytest.raises(RuntimeError, match='closed'):
            proxy.call('method', b'data')
