import time
import pytest
import c_two as cc
from c_two.compo.runtime_connect import get_current_client
from c_two.transport.registry import connect as transport_connect, close as transport_close
from tests.fixtures.hello import Hello
from tests.fixtures.ihello import IHello, HelloData

pytestmark = pytest.mark.timeout(15)


# Define component functions at module level for testing
@cc.runtime.connect
def my_greeting(crm: IHello, name: str) -> str:
    return crm.greeting(name)

@cc.runtime.connect
def my_add(crm: IHello, a: int, b: int) -> int:
    return crm.add(a, b)

@cc.runtime.connect
def my_get_data(crm: IHello, id: int) -> HelloData:
    return crm.get_data(id)


class TestConnectCrmWithIcrm:
    """Test connect_crm(address, IHello) context manager."""

    def test_yields_proxy(self, hello_server):
        with cc.compo.runtime.connect_crm(hello_server, IHello) as crm:
            assert crm is not None

    def test_methods_callable(self, hello_server):
        with cc.compo.runtime.connect_crm(hello_server, IHello) as crm:
            assert crm.greeting('Test') == 'Hello, Test!'

    def test_requires_icrm_class(self, hello_server):
        with pytest.raises(ValueError, match='icrm_class is required'):
            with cc.compo.runtime.connect_crm(hello_server) as _:
                pass


class TestRuntimeConnectDecorator:
    """Test @cc.runtime.connect decorated functions."""

    def test_call_with_crm_address(self, hello_server):
        result = my_greeting('World', crm_address=hello_server)
        assert result == 'Hello, World!'

    def test_call_with_context_manager(self, hello_server):
        with cc.compo.runtime.connect_crm(hello_server, IHello):
            result = my_greeting('Context')
            assert result == 'Hello, Context!'

    def test_call_with_crm_connection(self, hello_server):
        proxy = transport_connect(IHello, name='default', address=hello_server)
        try:
            result = my_greeting('Conn', crm_connection=proxy)
            assert result == 'Hello, Conn!'
        finally:
            transport_close(proxy)

    def test_multi_param_component(self, hello_server):
        result = my_add(3, 7, crm_address=hello_server)
        assert result == 10

    def test_transferable_return(self, hello_server):
        data = my_get_data(5, crm_address=hello_server)
        assert isinstance(data, HelloData)
        assert data.name == 'data-5'
        assert data.value == 50

    def test_no_client_raises(self):
        with pytest.raises(ValueError, match='No client available'):
            my_greeting('Test')


class TestContextNesting:
    """Test that nested connect_crm contexts work correctly."""

    def test_nested_contexts_restore_client(self, hello_server):
        with cc.compo.runtime.connect_crm(hello_server, IHello) as outer_proxy:
            outer = get_current_client()
            assert outer is not None

            with cc.compo.runtime.connect_crm(hello_server, IHello) as inner_proxy:
                inner = get_current_client()
                assert inner is not None

            # After inner context exits, outer proxy should be restored
            restored = get_current_client()
            assert restored is outer
