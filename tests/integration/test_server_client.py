import time
import pytest
import threading
import c_two as cc
from c_two.rpc import ServerConfig
from c_two.rpc.server import _start
from tests.fixtures.hello import Hello
from tests.fixtures.ihello import IHello, HelloData

pytestmark = pytest.mark.timeout(15)


class TestServerClientLifecycle:
    """Test full server/client lifecycle across all protocols."""

    def test_ping(self, hello_server):
        assert cc.rpc.Client.ping(hello_server, timeout=2.0)

    def test_greeting(self, hello_server):
        with cc.compo.runtime.connect_crm(hello_server, IHello) as crm:
            result = crm.greeting('World')
            assert result == 'Hello, World!'

    def test_add(self, hello_server):
        with cc.compo.runtime.connect_crm(hello_server, IHello) as crm:
            assert crm.add(3, 4) == 7

    def test_echo_none_returns_none(self, hello_server):
        with cc.compo.runtime.connect_crm(hello_server, IHello) as crm:
            assert crm.echo_none('none') is None

    def test_echo_none_returns_value(self, hello_server):
        with cc.compo.runtime.connect_crm(hello_server, IHello) as crm:
            assert crm.echo_none('hi') == 'hi'

    def test_get_items(self, hello_server):
        with cc.compo.runtime.connect_crm(hello_server, IHello) as crm:
            result = crm.get_items([1, 2, 3])
            assert result == ['item-1', 'item-2', 'item-3']

    def test_get_data(self, hello_server):
        with cc.compo.runtime.connect_crm(hello_server, IHello) as crm:
            data = crm.get_data(5)
            assert isinstance(data, HelloData)
            assert data.name == 'data-5'
            assert data.value == 50

    def test_multiple_calls(self, hello_server):
        """Multiple sequential calls on the same connection."""
        with cc.compo.runtime.connect_crm(hello_server, IHello) as crm:
            assert crm.greeting('A') == 'Hello, A!'
            assert crm.add(10, 20) == 30
            assert crm.greeting('B') == 'Hello, B!'


class TestShutdown:
    """Test graceful server shutdown."""

    def test_client_shutdown(self, unique_thread_address):
        crm = Hello()
        config = ServerConfig(
            crm=crm,
            icrm=IHello,
            bind_address=unique_thread_address,
        )
        server = cc.rpc.Server(config)
        _start(server._state)

        # Wait for server to be ready
        for _ in range(50):
            if cc.rpc.Client.ping(unique_thread_address, timeout=0.5):
                break
            time.sleep(0.1)

        assert cc.rpc.Client.ping(unique_thread_address)
        cc.rpc.Client.shutdown(unique_thread_address, timeout=2.0)
        time.sleep(0.3)

        # Server should no longer respond
        assert not cc.rpc.Client.ping(unique_thread_address, timeout=0.5)
