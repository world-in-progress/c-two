import pytest

import c_two as cc
from c_two.crm.meta import MethodAccess, get_method_access
from c_two.rpc import ConcurrencyConfig, ConcurrencyMode, ServerConfig
from tests.fixtures.hello import Hello
from tests.fixtures.ihello import IHello


class TestMethodAccessDecorators:
    def test_read_metadata_is_preserved_after_icrm_wrapping(self):
        @cc.icrm(namespace='test.concurrent', version='0.1.0')
        class IService:
            @cc.read
            def fetch(self, value: int) -> int:
                ...

            def mutate(self, value: int) -> int:
                ...

        assert get_method_access(IService.__dict__['fetch']) is MethodAccess.READ
        assert get_method_access(IService.__dict__['mutate']) is MethodAccess.WRITE

    def test_write_decorator_marks_method(self):
        @cc.icrm(namespace='test.concurrent', version='0.1.0')
        class IService:
            @cc.write
            def mutate(self, value: int) -> int:
                ...

        assert get_method_access(IService.__dict__['mutate']) is MethodAccess.WRITE


class TestConcurrencyConfig:
    def test_string_mode_is_coerced(self):
        config = ConcurrencyConfig(mode='read_parallel', max_workers=4)

        assert config.mode is ConcurrencyMode.READ_PARALLEL
        assert config.max_workers == 4

    def test_invalid_max_workers_raises(self):
        with pytest.raises(ValueError, match='max_workers'):
            ConcurrencyConfig(max_workers=0)

    def test_invalid_max_pending_raises(self):
        with pytest.raises(ValueError, match='max_pending'):
            ConcurrencyConfig(max_pending=0)

    def test_max_pending_accepts_positive(self):
        config = ConcurrencyConfig(max_pending=10)
        assert config.max_pending == 10

    def test_max_pending_none_means_unlimited(self):
        config = ConcurrencyConfig()
        assert config.max_pending is None


class TestProtocolGuard:
    def test_http_rejects_concurrent_scheduler_before_transport_hardening(self):
        config = ServerConfig(
            crm=Hello(),
            icrm=IHello,
            bind_address='http://127.0.0.1:19001',
            concurrency=ConcurrencyConfig(mode=ConcurrencyMode.READ_PARALLEL, max_workers=4),
        )

        with pytest.raises(ValueError, match='thread://.*memory://'):
            cc.rpc.Server(config)
