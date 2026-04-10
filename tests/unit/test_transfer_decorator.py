# tests/unit/test_transfer_decorator.py
import pickle
import pytest
from c_two.crm.transferable import (
    Transferable, transfer, _build_transfer_wrapper, auto_transfer,
)
import c_two as cc


@cc.transferable
class _FakeInput(Transferable):
    x: int
    def serialize(d: '_FakeInput') -> bytes:
        return pickle.dumps(d.x)
    def deserialize(b: bytes) -> '_FakeInput':
        return _FakeInput(x=pickle.loads(b))


@cc.transferable
class _FakeOutput(Transferable):
    y: str
    def serialize(d: '_FakeOutput') -> bytes:
        return pickle.dumps(d.y)
    def deserialize(b: bytes) -> '_FakeOutput':
        return _FakeOutput(y=pickle.loads(b))


class TestTransferDecorator:
    """@cc.transfer attaches __cc_transfer__ metadata without wrapping."""

    def test_attaches_metadata(self):
        @transfer(input=_FakeInput, output=_FakeOutput)
        def my_method(self, x: int) -> str:
            ...

        meta = getattr(my_method, '__cc_transfer__', None)
        assert meta is not None
        assert meta['input'] is _FakeInput
        assert meta['output'] is _FakeOutput
        assert meta['buffer'] == 'view'

    def test_buffer_hold(self):
        @transfer(buffer='hold')
        def my_method(self, data: bytes) -> bytes:
            ...

        meta = my_method.__cc_transfer__
        assert meta['buffer'] == 'hold'
        assert meta['input'] is None
        assert meta['output'] is None

    def test_does_not_wrap_function(self):
        """The decorated function should be the original — no wrapper layer."""
        def original(self, x: int) -> int:
            return x

        decorated = transfer(input=_FakeInput)(original)
        assert decorated is original

    def test_invalid_buffer_raises(self):
        with pytest.raises(ValueError, match='buffer'):
            @transfer(buffer='copy')
            def bad(self) -> None:
                ...


class TestBuildTransferWrapper:
    """_build_transfer_wrapper creates the actual com_to_crm/crm_to_com closure."""

    def test_wrapper_has_buffer_mode_attr(self):
        def my_func(self, x: int) -> int: ...
        wrapped = _build_transfer_wrapper(my_func, input=_FakeInput, output=_FakeOutput, buffer='hold')
        assert wrapped._input_buffer_mode == 'hold'

    def test_wrapper_default_buffer_view(self):
        def my_func(self, x: int) -> int: ...
        wrapped = _build_transfer_wrapper(my_func, input=_FakeInput, output=_FakeOutput)
        assert wrapped._input_buffer_mode == 'view'

    def test_crm_to_com_round_trip(self):
        """Server-side round-trip: serialized input → CRM method → serialized output."""
        def greet(self, name: str) -> str: ...

        wrapped = _build_transfer_wrapper(greet, input=_FakeInput, output=_FakeOutput)

        class MockCRM:
            def greet(self, data):
                return _FakeOutput(y=f'Hi {data.x}')
        class MockICRM:
            direction = '<-'
            crm = MockCRM()

        err_bytes, result_bytes = wrapped(MockICRM(), _FakeInput.serialize(_FakeInput(x=42)))
        from c_two.error import CCError
        assert CCError.deserialize(memoryview(err_bytes)) is None
        out = _FakeOutput.deserialize(result_bytes)
        assert out.y == 'Hi 42'


class TestAutoTransferWithConfig:
    """auto_transfer accepts input/output/buffer kwargs from @cc.transfer."""

    def test_explicit_input_override(self):
        wrapped = auto_transfer(
            lambda self, x: ...,
            input=_FakeInput,
        )
        assert callable(wrapped)

    def test_explicit_buffer_hold(self):
        def my_method(self, x: int) -> int: ...
        wrapped = auto_transfer(my_method, buffer='hold')
        assert wrapped._input_buffer_mode == 'hold'

    def test_explicit_output_override(self):
        def my_method(self, x: int) -> str: ...
        wrapped = auto_transfer(my_method, output=_FakeOutput)
        assert callable(wrapped)

    def test_default_buffer_is_view(self):
        def my_method(self, x: int) -> int: ...
        wrapped = auto_transfer(my_method)
        assert wrapped._input_buffer_mode == 'view'

    def test_kwargs_forwarded_to_build(self):
        """Explicit input/output skip auto-matching."""
        def my_method(self, x: int) -> str: ...
        wrapped = auto_transfer(my_method, input=_FakeInput, output=_FakeOutput, buffer='hold')
        assert wrapped._input_buffer_mode == 'hold'
