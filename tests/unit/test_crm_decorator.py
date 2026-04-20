import pytest
import c_two as cc
from c_two.crm.meta import CRMMeta, crm
from tests.fixtures.ihello import Hello


class TestCRMMeta:
    """Tests for the CRMMeta metaclass."""

    def test_direction_attribute_set(self):
        cls = CRMMeta("Dummy", (), {})
        assert cls.direction == "->"

    def test_direction_on_subclass(self):
        Base = CRMMeta("Base", (), {})
        Sub = CRMMeta("Sub", (Base,), {})
        assert Sub.direction == "->"


class TestIcrmDecorator:
    """Tests for the @cc.crm() decorator."""

    def test_direction_attribute(self):
        @cc.crm(namespace="ns", version="1.0.0")
        class Svc:
            pass

        assert Svc.direction == "->"

    def test_tag_format(self):
        @cc.crm(namespace="my.ns", version="2.3.4")
        class MyService:
            pass

        assert MyService.__tag__ == "my.ns/MyService/2.3.4"

    def test_methods_wrapped_by_auto_transfer(self):
        @cc.crm(namespace="ns", version="0.1.0")
        class Svc:
            def do_work(self, x: int) -> int:
                ...

        # Wrapped method should not be the original bare function
        method = Svc.__dict__["do_work"]
        assert callable(method)
        assert hasattr(method, "__wrapped__")

    def test_private_methods_not_wrapped(self):
        @cc.crm(namespace="ns", version="0.1.0")
        class Svc:
            def __init__(self):
                pass

        init_method = Svc.__dict__.get("__init__")
        if init_method is not None:
            assert not hasattr(init_method, "__wrapped__")

    def test_preserves_doc(self):
        @cc.crm(namespace="ns", version="0.1.0")
        class Svc:
            """My service docstring."""

        assert Svc.__doc__ == "My service docstring."

    def test_preserves_module(self):
        @cc.crm(namespace="ns", version="0.1.0")
        class Svc:
            pass

        assert Svc.__module__ == __name__

    def test_preserves_annotations(self):
        @cc.crm(namespace="ns", version="0.1.0")
        class Svc:
            x: int
            y: str

        assert "x" in Svc.__annotations__
        assert "y" in Svc.__annotations__


class TestIcrmValidation:
    """Tests for namespace/version validation."""

    def test_empty_namespace_raises(self):
        with pytest.raises(ValueError, match="Namespace"):
            @cc.crm(namespace="", version="1.0.0")
            class Svc:
                pass

    def test_empty_version_raises(self):
        with pytest.raises(ValueError, match="Version"):
            @cc.crm(namespace="ns", version="")
            class Svc:
                pass

    def test_invalid_version_format_raises(self):
        with pytest.raises(ValueError, match="format"):
            @cc.crm(namespace="ns", version="1.0")
            class Svc:
                pass


class TestHelloFixture:
    """Tests using the Hello fixture from tests/fixtures."""

    def test_tag(self):
        assert Hello.__tag__ == "test.hello/Hello/0.1.0"

    def test_direction(self):
        assert Hello.direction == "->"

    def test_expected_methods_exist(self):
        expected = {"greeting", "add", "echo_none", "get_items", "get_data"}
        actual = {name for name in dir(Hello) if not name.startswith("_")}
        assert expected.issubset(actual)
