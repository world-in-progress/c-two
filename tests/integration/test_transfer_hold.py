"""IPC integration tests for buffer hold mode and HeldResult lifecycle."""
import pytest
import c_two as cc
from c_two.crm.transferable import HeldResult
from c_two.transport.registry import _ProcessRegistry

from tests.fixtures.ihello import Hello
from tests.fixtures.hello import HelloImpl


@pytest.fixture(autouse=True)
def _cleanup():
    yield
    cc.shutdown()
    _ProcessRegistry._instance = None


class TestIPCHoldMode:
    """Test cc.hold() through real IPC transport."""

    def test_hold_returns_held_result_ipc(self):
        """cc.hold() on IPC proxy returns HeldResult with valid data."""
        crm = HelloImpl()
        cc.register(Hello, crm, name='hold_test')

        try:
            proxy = cc.connect(Hello, name='hold_test')
            try:
                # Normal call
                result = proxy.greeting('World')
                assert result == 'Hello, World!'

                # Hold call
                held = cc.hold(proxy.greeting)('World')
                assert isinstance(held, HeldResult)
                assert held.value == 'Hello, World!'
                held.release()

                # After release, value is inaccessible
                with pytest.raises(RuntimeError):
                    _ = held.value
            finally:
                cc.close(proxy)
        finally:
            cc.unregister('hold_test')

    def test_hold_context_manager_ipc(self):
        """HeldResult works as context manager through IPC."""
        crm = HelloImpl()
        cc.register(Hello, crm, name='hold_ctx')

        try:
            proxy = cc.connect(Hello, name='hold_ctx')
            try:
                with cc.hold(proxy.greeting)('World') as held:
                    assert held.value == 'Hello, World!'
                # After context exit, released
                with pytest.raises(RuntimeError):
                    _ = held.value
            finally:
                cc.close(proxy)
        finally:
            cc.unregister('hold_ctx')

    def test_view_default_releases_after_call(self):
        """Default view mode: data is accessible as normal return, no HeldResult."""
        crm = HelloImpl()
        cc.register(Hello, crm, name='view_test')

        try:
            proxy = cc.connect(Hello, name='view_test')
            try:
                result = proxy.greeting('World')
                assert result == 'Hello, World!'
                assert not isinstance(result, HeldResult)
            finally:
                cc.close(proxy)
        finally:
            cc.unregister('view_test')
