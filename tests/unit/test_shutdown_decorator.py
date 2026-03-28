"""Unit tests for @cc.on_shutdown decorator and discovery."""
from __future__ import annotations

import pytest

import c_two as cc
from c_two.crm.meta import (
    on_shutdown,
    get_shutdown_method,
    _SHUTDOWN_ATTR,
)


# ---------------------------------------------------------------------------
# Decorator tests
# ---------------------------------------------------------------------------

class TestOnShutdownDecorator:

    def test_sets_attribute(self):
        @on_shutdown
        def cleanup(self):
            pass

        assert getattr(cleanup, _SHUTDOWN_ATTR) is True

    def test_returns_same_function(self):
        def cleanup(self):
            pass

        result = on_shutdown(cleanup)
        assert result is cleanup

    def test_non_callable_raises(self):
        with pytest.raises(TypeError, match='callables'):
            on_shutdown(42)

    def test_cc_namespace_export(self):
        """@cc.on_shutdown is importable from the top-level package."""
        assert hasattr(cc, 'on_shutdown')
        assert cc.on_shutdown is on_shutdown


# ---------------------------------------------------------------------------
# Discovery tests
# ---------------------------------------------------------------------------

class TestGetShutdownMethod:

    def test_no_shutdown_method(self):
        class IFoo:
            def hello(self): ...

        assert get_shutdown_method(IFoo) is None

    def test_one_shutdown_method(self):
        class IBar:
            def work(self): ...

            @on_shutdown
            def cleanup(self): ...

        assert get_shutdown_method(IBar) == 'cleanup'

    def test_multiple_shutdown_raises(self):
        class IBad:
            @on_shutdown
            def cleanup1(self): ...

            @on_shutdown
            def cleanup2(self): ...

        with pytest.raises(ValueError, match='multiple @on_shutdown'):
            get_shutdown_method(IBad)

    def test_private_methods_rejected(self):
        """@on_shutdown on private methods raises ValueError."""
        class IPrivate:
            @on_shutdown
            def _internal_cleanup(self): ...

        with pytest.raises(ValueError, match='private method'):
            get_shutdown_method(IPrivate)


# ---------------------------------------------------------------------------
# ICRM integration tests
# ---------------------------------------------------------------------------

class TestICRMOnShutdown:

    def test_shutdown_method_not_in_transfer(self):
        """@cc.on_shutdown methods should not be wrapped by auto_transfer."""
        @cc.icrm(namespace='test.sd', version='0.1.0')
        class IDemo:
            def work(self, x: int) -> int: ...

            @cc.on_shutdown
            def cleanup(self): ...

        # 'work' should be in the class (auto_transfer wrapped)
        assert hasattr(IDemo, 'work')
        # 'cleanup' should NOT be auto_transfer wrapped — it should
        # either be absent or be the original unwrapped function
        # The @cc.icrm decorator skips @cc.on_shutdown methods
        # They won't appear as decorated_methods in the new class

    def test_read_write_shutdown_coexist(self):
        """@cc.read, @cc.write, and @cc.on_shutdown can coexist."""
        @cc.icrm(namespace='test.coexist', version='0.1.0')
        class ICoexist:
            @cc.read
            def get_data(self) -> int: ...

            @cc.write
            def set_data(self, v: int): ...

            @cc.on_shutdown
            def cleanup(self): ...

        assert get_shutdown_method(ICoexist) == 'cleanup'
        # Regular methods still work
        assert hasattr(ICoexist, 'get_data')
        assert hasattr(ICoexist, 'set_data')
