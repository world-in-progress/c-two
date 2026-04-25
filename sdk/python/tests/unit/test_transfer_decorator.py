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


@cc.transferable
class _FakeInputWithFromBuffer(Transferable):
    x: int
    def serialize(d: '_FakeInputWithFromBuffer') -> bytes:
        return pickle.dumps(d.x)
    def deserialize(b: bytes) -> '_FakeInputWithFromBuffer':
        return _FakeInputWithFromBuffer(x=pickle.loads(b))
    def from_buffer(b: bytes) -> '_FakeInputWithFromBuffer':
        return _FakeInputWithFromBuffer(x=pickle.loads(b))


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
        assert meta['buffer'] is None

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
        class MockCRM:
            direction = '<-'
            resource = MockCRM()

        err_bytes, result_bytes = wrapped(MockCRM(), _FakeInput.serialize(_FakeInput(x=42)))
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
        def my_method(self, data: _FakeInputWithFromBuffer) -> int: ...
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
        wrapped = auto_transfer(my_method, input=_FakeInputWithFromBuffer, output=_FakeOutput, buffer='hold')
        assert wrapped._input_buffer_mode == 'hold'


class TestIcrmTransferIntegration:
    """@cc.transfer metadata is consumed by crm() via auto_transfer."""

    def test_icrm_picks_up_transfer_metadata(self):
        @cc.crm(namespace='test.transfer', version='0.1.0')
        class ITest:
            @transfer(input=_FakeInputWithFromBuffer, buffer='hold')
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

        @cc.crm(namespace='test.roundtrip', version='0.1.0')
        class IRoundTrip:
            @transfer(input=TrackedInput)
            def process(self, data: TrackedInput) -> int:
                ...

        class CRM:
            def process(self, data):
                return data.val * 2

        server = IRoundTrip()
        server.resource = CRM()
        server.direction = '<-'

        serialized = TrackedInput.serialize(TrackedInput(val=7))
        err_bytes, result_bytes = server.process(serialized)

        from c_two.error import CCError
        assert CCError.deserialize(memoryview(err_bytes)) is None
        assert 7 in deserialize_calls

    def test_plain_method_still_works(self):
        """Methods without @transfer still auto-bundle via pickle."""
        @cc.crm(namespace='test.plain', version='0.1.0')
        class IPlain:
            def greet(self, name: str) -> str:
                ...

        class CRM:
            def greet(self, name):
                return f'Hi {name}'

        server = IPlain()
        server.resource = CRM()
        server.direction = '<-'

        err_bytes, result_bytes = server.greet(pickle.dumps('World'))
        from c_two.error import CCError
        assert CCError.deserialize(memoryview(err_bytes)) is None
        assert pickle.loads(result_bytes) == 'Hi World'


class TestMethodLevelBufferBehavior:
    """Buffer lifecycle is controlled at method level, not type level."""

    def test_view_buffer_releases_after_deserialize(self):
        """With buffer='view' (default), _release_fn is invoked after CRM method returns."""
        @cc.transferable
        class ViewIn:
            def serialize(val: int) -> bytes:
                return pickle.dumps(val)
            def deserialize(data) -> int:
                return pickle.loads(data)

        def echo(self, x):
            return x

        wrapped = _build_transfer_wrapper(echo, input=ViewIn, output=None, buffer='view')

        class MockCRM:
            def echo(self, x):
                return x
        class MockCRM:
            direction = '<-'
            resource = MockCRM()
        crm = MockCRM()

        released = []
        result = wrapped(crm, pickle.dumps(42), _release_fn=lambda: released.append(True))
        assert released, '_release_fn should be called in view mode'

    def test_hold_buffer_declared_at_method(self):
        """buffer='hold' is set via @cc.transfer, not on the type."""
        @cc.crm(namespace='test.buf.hold', version='0.1.0')
        class IBufHold:
            @transfer(buffer='hold')
            def process(self, data: _FakeInputWithFromBuffer) -> str:
                ...

        method = getattr(IBufHold, 'process')
        assert getattr(method, '_input_buffer_mode', None) == 'hold'


class TestExports:
    def test_cc_transfer_importable(self):
        import c_two as cc
        assert hasattr(cc, 'transfer')
        assert callable(cc.transfer)

    def test_cc_hold_importable(self):
        import c_two as cc
        assert hasattr(cc, 'hold')
        assert callable(cc.hold)

    def test_cc_held_result_importable(self):
        import c_two as cc
        assert hasattr(cc, 'HeldResult')


class TestEndToEnd:
    """Full round-trip: CRM with @cc.transfer, proxy call, hold mode."""

    def test_auto_bundle_round_trip(self):
        """Methods without @cc.transfer auto-bundle via pickle."""
        import pickle
        import c_two as cc
        from c_two.error import CCError

        @cc.crm(namespace='test.e2e.autobundle', version='0.1.0')
        class IE2E:
            def greeting(self, name: str) -> str:
                ...

        class CRM:
            def greeting(self, name: str) -> str:
                return f'Hello, {name}!'

        server_icrm = IE2E()
        server_icrm.resource = CRM()
        server_icrm.direction = '<-'

        greet = getattr(server_icrm, 'greeting')
        serialized_input = pickle.dumps('World')
        err_bytes, result_bytes = greet(serialized_input)

        assert CCError.deserialize(memoryview(err_bytes)) is None
        assert pickle.loads(result_bytes) == 'Hello, World!'

    def test_hold_mode_thread_local(self):
        """cc.hold() on thread-local proxy returns HeldResult."""
        import c_two as cc
        from c_two.crm.transferable import HeldResult
        from c_two.transport.client.proxy import CRMProxy

        @cc.crm(namespace='test.hold.threadlocal', version='0.1.0')
        class IHoldTest:
            def compute(self, x: int) -> int:
                ...

        class FakeCRM:
            def compute(self, x: int) -> int:
                return x * 2

        proxy = CRMProxy.thread_local(FakeCRM())

        client_icrm = IHoldTest()
        client_icrm.client = proxy
        client_icrm.direction = '->'

        result = client_icrm.compute(5)
        assert result == 10

        held = cc.hold(client_icrm.compute)(5)
        assert isinstance(held, HeldResult)
        assert held.value == 10
        held.release()

    def test_c2_buffer_not_leaked_to_crm(self):
        """_c2_buffer is popped by transfer_wrapper, never reaches user CRM methods."""
        import pickle
        import c_two as cc

        received_kwargs = {}

        @cc.crm(namespace='test.noleak', version='0.1.0')
        class INoLeak:
            def process(self, x: int) -> int:
                ...

        class CRM:
            def process(self, x, **kwargs):
                received_kwargs.update(kwargs)
                return x

        server = INoLeak()
        server.resource = CRM()
        server.direction = '<-'

        err, result = server.process(pickle.dumps(42))
        assert '_c2_buffer' not in received_kwargs


class TestHoldVsViewSmoke:
    """Smoke test that hold vs view both produce correct results."""

    def test_view_and_hold_produce_same_value(self):
        """Both modes return the same data; hold wraps in HeldResult."""
        import c_two as cc
        from c_two.crm.transferable import HeldResult
        from c_two.transport.client.proxy import CRMProxy

        @cc.crm(namespace='test.holdview.smoke', version='0.1.0')
        class ISmoke:
            def echo(self, data: bytes) -> bytes: ...

        class SmokeEcho:
            def echo(self, data: bytes) -> bytes:
                return data

        proxy = CRMProxy.thread_local(SmokeEcho())
        crm = ISmoke()
        crm.client = proxy
        crm.direction = '->'

        payload = b'x' * 1024

        # View mode
        view_result = crm.echo(payload)
        assert view_result == payload

        # Hold mode
        held = cc.hold(crm.echo)(payload)
        assert isinstance(held, HeldResult)
        assert held.value == payload
        held.release()
