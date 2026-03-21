import pytest
from c_two.rpc import ServerConfig
from tests.fixtures.hello import Hello
from tests.fixtures.ihello import IHello


class TestServerConfigValid:
    def test_valid_config(self):
        config = ServerConfig(
            crm=Hello(),
            icrm=IHello,
            bind_address='thread://test_config',
        )
        assert config.name == 'Hello'

    def test_custom_name(self):
        config = ServerConfig(
            name='MyServer',
            crm=Hello(),
            icrm=IHello,
            bind_address='thread://test_config',
        )
        assert config.name == 'MyServer'

    def test_default_on_shutdown(self):
        config = ServerConfig(
            crm=Hello(),
            icrm=IHello,
            bind_address='thread://test_config',
        )
        assert config.on_shutdown is not None
        # Default lambda should be callable and return None
        assert config.on_shutdown() is None

    def test_custom_on_shutdown(self):
        called = []
        callback = lambda: called.append(True)
        config = ServerConfig(
            crm=Hello(),
            icrm=IHello,
            bind_address='thread://test_config',
            on_shutdown=callback,
        )
        assert config.on_shutdown is callback
        config.on_shutdown()
        assert called == [True]


class TestServerConfigValidation:
    def test_missing_method_raises(self):
        class IncompleteCRM:
            def greeting(self, name: str) -> str:
                return name

        with pytest.raises(ValueError, match='missing'):
            ServerConfig(
                crm=IncompleteCRM(),
                icrm=IHello,
                bind_address='thread://test_config',
            )

    def test_missing_method_names_in_error(self):
        class IncompleteCRM:
            def greeting(self, name: str) -> str:
                return name

        with pytest.raises(ValueError) as exc_info:
            ServerConfig(
                crm=IncompleteCRM(),
                icrm=IHello,
                bind_address='thread://test_config',
            )

        message = str(exc_info.value)
        for method in ('add', 'echo_none', 'get_items', 'get_data'):
            assert method in message

    def test_empty_crm_raises(self):
        class EmptyCRM:
            pass

        with pytest.raises(ValueError, match='missing'):
            ServerConfig(
                crm=EmptyCRM(),
                icrm=IHello,
                bind_address='thread://test_config',
            )
