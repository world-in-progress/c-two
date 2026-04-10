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


class TestIcrmTransferIntegration:
    """@cc.transfer metadata is consumed by icrm() via auto_transfer."""

    def test_icrm_picks_up_transfer_metadata(self):
        @cc.icrm(namespace='test.transfer', version='0.1.0')
        class ITest:
            @transfer(input=_FakeInput, buffer='hold')
            def process(self, x: int) -> str:
                ...

            def plain(self, y: str) -> int:
                ...

        process_method = getattr(ITest, 'process', None)
        assert process_method is not None
        assert getattr(process_method, '_input_buffer_mode', None) == 'hold'

        plain_method = getattr(ITest, 'plain', None)
        assert plain_method is not None
        assert getattr(plain_method, '_input_buffer_mode', None) == 'view'

    def test_transfer_metadata_round_trip(self):
        """Round-trip: explicit @transfer(input=...) uses the custom codec, not pickle fallback."""
        serialize_calls = []
        deserialize_calls = []

        @cc.transferable
        class TrackedInput(Transferable):
            val: int
            def serialize(d: 'TrackedInput') -> bytes:
                serialize_calls.append(d.val)
                return pickle.dumps(d.val)
            def deserialize(b: bytes) -> 'TrackedInput':
                v = pickle.loads(b)
                deserialize_calls.append(v)
                return TrackedInput(val=v)

        @cc.icrm(namespace='test.roundtrip', version='0.1.0')
        class IRoundTrip:
            @transfer(input=TrackedInput)
            def process(self, data: TrackedInput) -> int:
                ...

        class CRM:
            def process(self, data):
                return data.val * 2

        server = IRoundTrip()
        server.crm = CRM()
        server.direction = '<-'

        serialized = TrackedInput.serialize(TrackedInput(val=7))
        err_bytes, result_bytes = server.process(serialized)

        from c_two.error import CCError
        assert CCError.deserialize(memoryview(err_bytes)) is None
        assert 7 in deserialize_calls

    def test_plain_method_still_works(self):
        """Methods without @transfer still auto-bundle via pickle."""
        @cc.icrm(namespace='test.plain', version='0.1.0')
        class IPlain:
            def greet(self, name: str) -> str:
                ...

        class CRM:
            def greet(self, name):
                return f'Hi {name}'

        server = IPlain()
        server.crm = CRM()
        server.direction = '<-'

        err_bytes, result_bytes = server.greet(pickle.dumps('World'))
        from c_two.error import CCError
        assert CCError.deserialize(memoryview(err_bytes)) is None
        assert pickle.loads(result_bytes) == 'Hi World'
