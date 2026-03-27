"""Integration tests for ICRMProxy with real ICRM auto_transfer pipeline.

Tests that ICRMProxy.thread_local() and ICRMProxy.ipc() work as drop-in
replacements for rpc.Client when assigned to ``icrm.client``.
"""
from __future__ import annotations

import os
import time
import uuid

import pytest

from c_two.rpc_v2 import ServerV2, ICRMProxy
from c_two.rpc_v2.client import SharedClient

from tests.fixtures.ihello import IHello
from tests.fixtures.hello import Hello
from tests.fixtures.counter import ICounter, Counter


_IPC_SOCK_DIR = os.environ.get('CC_IPC_SOCK_DIR', '/tmp/c_two_ipc')


def _unique_region() -> str:
    return f'test_proxy_{uuid.uuid4().hex[:12]}'


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
# Thread-local proxy + real ICRM
# ---------------------------------------------------------------------------

class TestICRMProxyThreadLocal:
    """ICRMProxy.thread_local() with real ICRM auto_transfer pipeline."""

    def test_greeting_via_proxy(self):
        """Thread-local proxy routes through call_direct, skipping serde."""
        crm = Hello()
        proxy = ICRMProxy.thread_local(crm)
        icrm = IHello()
        icrm.client = proxy
        result = icrm.greeting('World')
        assert result == 'Hello, World!'

    def test_add_via_proxy(self):
        crm = Hello()
        proxy = ICRMProxy.thread_local(crm)
        icrm = IHello()
        icrm.client = proxy
        assert icrm.add(3, 4) == 7

    def test_counter_get_via_proxy(self):
        crm = Counter(initial=42)
        proxy = ICRMProxy.thread_local(crm)
        icrm = ICounter()
        icrm.client = proxy
        assert icrm.get() == 42

    def test_counter_increment_via_proxy(self):
        crm = Counter(initial=10)
        proxy = ICRMProxy.thread_local(crm)
        icrm = ICounter()
        icrm.client = proxy
        assert icrm.increment(5) == 15
        assert icrm.get() == 15

    def test_terminate_callback_invoked(self):
        released = []
        proxy = ICRMProxy.thread_local(Hello(), on_terminate=lambda: released.append(True))
        icrm = IHello()
        icrm.client = proxy
        assert icrm.greeting('X') == 'Hello, X!'
        proxy.terminate()
        assert released == [True]


# ---------------------------------------------------------------------------
# IPC proxy + real ICRM (end-to-end)
# ---------------------------------------------------------------------------

class TestICRMProxyIPC:
    """ICRMProxy.ipc() with real ServerV2 + SharedClient."""

    @pytest.fixture
    def server_addr(self):
        addr = f'ipc-v3://{_unique_region()}'
        server = ServerV2(bind_address=addr)
        server.register_crm(IHello, Hello(), name='hello')
        server.register_crm(ICounter, Counter(initial=50), name='counter')
        server.start()
        _wait_for_server(addr)
        yield addr
        server.shutdown()

    def test_hello_via_ipc_proxy(self, server_addr):
        client = SharedClient(server_addr, try_v2=True)
        client.connect()
        try:
            proxy = ICRMProxy.ipc(client, 'hello')
            icrm = IHello()
            icrm.client = proxy
            assert icrm.greeting('IPC') == 'Hello, IPC!'
            assert icrm.add(10, 20) == 30
        finally:
            client.terminate()

    def test_counter_via_ipc_proxy(self, server_addr):
        client = SharedClient(server_addr, try_v2=True)
        client.connect()
        try:
            proxy = ICRMProxy.ipc(client, 'counter')
            icrm = ICounter()
            icrm.client = proxy
            assert icrm.get() == 50
            assert icrm.increment(7) == 57
        finally:
            client.terminate()

    def test_two_proxies_same_client(self, server_addr):
        """Two different ICRM proxies share the same SharedClient."""
        client = SharedClient(server_addr, try_v2=True)
        client.connect()
        try:
            hello_proxy = ICRMProxy.ipc(client, 'hello')
            counter_proxy = ICRMProxy.ipc(client, 'counter')

            hello = IHello()
            hello.client = hello_proxy
            counter = ICounter()
            counter.client = counter_proxy

            assert hello.greeting('Shared') == 'Hello, Shared!'
            assert counter.get() == 50
            assert counter.increment(10) == 60
            assert hello.add(1, 2) == 3
        finally:
            client.terminate()
