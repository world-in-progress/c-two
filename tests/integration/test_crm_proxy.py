"""Integration tests for CRMProxy with real CRM auto_transfer pipeline.

Tests that CRMProxy.thread_local() and CRMProxy.ipc() work as drop-in
replacements for rpc.Client when assigned to ``crm.client``.
"""
from __future__ import annotations

import uuid

import pytest

import c_two as cc
from c_two.transport import Server, CRMProxy

from tests.fixtures.ihello import Hello
from tests.fixtures.hello import HelloImpl
from tests.fixtures.counter import Counter, CounterImpl


def _unique_region() -> str:
    return f'test_proxy_{uuid.uuid4().hex[:12]}'


# ---------------------------------------------------------------------------
# Thread-local proxy + real CRM
# ---------------------------------------------------------------------------

class TestCRMProxyThreadLocal:
    """CRMProxy.thread_local() with real CRM auto_transfer pipeline."""

    def test_greeting_via_proxy(self):
        """Thread-local proxy routes through call_direct, skipping serde."""
        crm = HelloImpl()
        proxy = CRMProxy.thread_local(crm)
        crm = Hello()
        crm.client = proxy
        result = crm.greeting('World')
        assert result == 'Hello, World!'

    def test_add_via_proxy(self):
        crm = HelloImpl()
        proxy = CRMProxy.thread_local(crm)
        crm = Hello()
        crm.client = proxy
        assert crm.add(3, 4) == 7

    def test_counter_get_via_proxy(self):
        crm = CounterImpl(initial=42)
        proxy = CRMProxy.thread_local(crm)
        crm = Counter()
        crm.client = proxy
        assert crm.get() == 42

    def test_counter_increment_via_proxy(self):
        crm = CounterImpl(initial=10)
        proxy = CRMProxy.thread_local(crm)
        crm = Counter()
        crm.client = proxy
        assert crm.increment(5) == 15
        assert crm.get() == 15

    def test_terminate_callback_invoked(self):
        released = []
        proxy = CRMProxy.thread_local(HelloImpl(), on_terminate=lambda: released.append(True))
        crm = Hello()
        crm.client = proxy
        assert crm.greeting('X') == 'Hello, X!'
        proxy.terminate()
        assert released == [True]


# ---------------------------------------------------------------------------
# IPC proxy via SOTA API (end-to-end)
# ---------------------------------------------------------------------------

class TestCRMProxyIPC:
    """CRMProxy.ipc() via SOTA cc.register/connect API."""

    @pytest.fixture
    def server_addr(self):
        addr = f'ipc://{_unique_region()}'
        server = Server(bind_address=addr)
        server.register_crm(Hello, HelloImpl(), name='hello')
        server.register_crm(Counter, CounterImpl(initial=50), name='counter')
        server.start()
        from c_two.transport.client.util import ping
        deadline = __import__('time').monotonic() + 5.0
        while __import__('time').monotonic() < deadline:
            if ping(addr, timeout=0.5):
                break
            __import__('time').sleep(0.05)
        yield addr
        server.shutdown()

    def test_hello_via_ipc_proxy(self, server_addr):
        proxy = cc.connect(Hello, name='hello', address=server_addr)
        try:
            assert proxy.greeting('IPC') == 'Hello, IPC!'
            assert proxy.add(10, 20) == 30
        finally:
            cc.close(proxy)

    def test_counter_via_ipc_proxy(self, server_addr):
        proxy = cc.connect(Counter, name='counter', address=server_addr)
        try:
            assert proxy.get() == 50
            assert proxy.increment(7) == 57
        finally:
            cc.close(proxy)

    def test_two_proxies_same_server(self, server_addr):
        """Two different CRM proxies connect to the same server."""
        hello = cc.connect(Hello, name='hello', address=server_addr)
        counter = cc.connect(Counter, name='counter', address=server_addr)
        try:
            assert hello.greeting('Shared') == 'Hello, Shared!'
            assert counter.get() == 50
            assert counter.increment(10) == 60
            assert hello.add(1, 2) == 3
        finally:
            cc.close(hello)
            cc.close(counter)
