import pytest
import c_two as cc
from c_two.crm.meta import ICRMMeta, icrm
from tests.fixtures.ihello import IHello


class TestICRMMeta:
    """Tests for the ICRMMeta metaclass."""

    def test_direction_attribute_set(self):
        cls = ICRMMeta("Dummy", (), {})
        assert cls.direction == "->"

    def test_direction_on_subclass(self):
        Base = ICRMMeta("Base", (), {})
        Sub = ICRMMeta("Sub", (Base,), {})
        assert Sub.direction == "->"


class TestIcrmDecorator:
    """Tests for the @cc.icrm() decorator."""

    def test_direction_attribute(self):
        @cc.icrm(namespace="ns", version="1.0.0")
        class Svc:
            pass

        assert Svc.direction == "->"

    def test_tag_format(self):
        @cc.icrm(namespace="my.ns", version="2.3.4")
        class MyService:
            pass

        assert MyService.__tag__ == "my.ns/MyService/2.3.4"

    def test_methods_wrapped_by_auto_transfer(self):
        @cc.icrm(namespace="ns", version="0.1.0")
        class Svc:
            def do_work(self, x: int) -> int:
                ...

        # Wrapped method should not be the original bare function
        method = Svc.__dict__["do_work"]
        assert callable(method)
        assert hasattr(method, "__wrapped__")

    def test_private_methods_not_wrapped(self):
        @cc.icrm(namespace="ns", version="0.1.0")
        class Svc:
            def __init__(self):
                pass

        init_method = Svc.__dict__.get("__init__")
        if init_method is not None:
            assert not hasattr(init_method, "__wrapped__")

    def test_preserves_doc(self):
        @cc.icrm(namespace="ns", version="0.1.0")
        class Svc:
            """My service docstring."""

        assert Svc.__doc__ == "My service docstring."

    def test_preserves_module(self):
        @cc.icrm(namespace="ns", version="0.1.0")
        class Svc:
            pass

        assert Svc.__module__ == __name__

    def test_preserves_annotations(self):
        @cc.icrm(namespace="ns", version="0.1.0")
        class Svc:
            x: int
            y: str

        assert "x" in Svc.__annotations__
        assert "y" in Svc.__annotations__


class TestIcrmValidation:
    """Tests for namespace/version validation."""

    def test_empty_namespace_raises(self):
        with pytest.raises(ValueError, match="Namespace"):
            @cc.icrm(namespace="", version="1.0.0")
            class Svc:
                pass

    def test_empty_version_raises(self):
        with pytest.raises(ValueError, match="Version"):
            @cc.icrm(namespace="ns", version="")
            class Svc:
                pass

    def test_invalid_version_format_raises(self):
        with pytest.raises(ValueError, match="format"):
            @cc.icrm(namespace="ns", version="1.0")
            class Svc:
                pass


class TestIHelloFixture:
    """Tests using the IHello fixture from tests/fixtures."""

    def test_tag(self):
        assert IHello.__tag__ == "test.hello/IHello/0.1.0"

    def test_direction(self):
        assert IHello.direction == "->"

    def test_expected_methods_exist(self):
        expected = {"greeting", "add", "echo_none", "get_items", "get_data"}
        actual = {name for name in dir(IHello) if not name.startswith("_")}
        assert expected.issubset(actual)
