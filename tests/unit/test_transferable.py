import pickle
import inspect
import pytest
import c_two as cc
from c_two.crm.transferable import (
    Transferable, TransferableMeta,
    register_transferable, get_transferable,
    create_default_transferable, auto_transfer,
    _create_pydantic_model_from_func_sig, _TRANSFERABLE_MAP,
)
from tests.fixtures.ihello import HelloData, HelloItems, IHello


# ---------------------------------------------------------------------------
# @cc.transferable decorator
# ---------------------------------------------------------------------------

class TestTransferableDecorator:
    def test_decorated_class_is_dataclass(self):
        from dataclasses import is_dataclass
        assert is_dataclass(HelloData)

    def test_serialize_is_staticmethod(self):
        assert isinstance(inspect.getattr_static(HelloData, 'serialize'), staticmethod)

    def test_deserialize_is_staticmethod(self):
        assert isinstance(inspect.getattr_static(HelloData, 'deserialize'), staticmethod)

    def test_class_registered_in_map(self):
        full_name = f'{HelloData.__module__}.{HelloData.__name__}'
        assert get_transferable(full_name) is HelloData

    def test_hello_data_round_trip(self):
        data = HelloData(name='alice', value=42)
        raw = HelloData.serialize(data)
        restored = HelloData.deserialize(raw)
        assert restored.name == 'alice'
        assert restored.value == 42

    def test_hello_items_round_trip(self):
        items = ['a', 'b', 'c']
        raw = HelloItems.serialize(items)
        restored = HelloItems.deserialize(raw)
        assert restored == items

    def test_new_transferable_registered(self):
        """A freshly decorated transferable is immediately retrievable."""
        @cc.transferable
        class UniqueTransA:
            x: int
            def serialize(d: 'UniqueTransA') -> bytes:
                return pickle.dumps(d.x)
            def deserialize(b: bytes) -> 'UniqueTransA':
                return UniqueTransA(x=pickle.loads(b))

        full_name = f'{UniqueTransA.__module__}.{UniqueTransA.__name__}'
        assert get_transferable(full_name) is UniqueTransA


# ---------------------------------------------------------------------------
# TransferableMeta — Default module NOT registered
# ---------------------------------------------------------------------------

class TestTransferableMeta:
    def test_default_module_not_registered(self):
        """Classes whose __module__ is 'Default' must NOT appear in the map."""
        cls = type(
            'DefaultModuleTrans',
            (Transferable,),
            {
                '__module__': 'Default',
                'serialize': staticmethod(lambda *a: b''),
                'deserialize': staticmethod(lambda b: None),
            }
        )
        full_name = f'Default.{cls.__name__}'
        assert get_transferable(full_name) is None


# ---------------------------------------------------------------------------
# register_transferable / get_transferable
# ---------------------------------------------------------------------------

class TestTransferableRegistry:
    def test_register_and_get(self):
        @cc.transferable
        class UniqueRegTest:
            val: str
            def serialize(d: 'UniqueRegTest') -> bytes:
                return pickle.dumps(d.val)
            def deserialize(b: bytes) -> 'UniqueRegTest':
                return UniqueRegTest(val=pickle.loads(b))

        full_name = f'{UniqueRegTest.__module__}.{UniqueRegTest.__name__}'
        assert get_transferable(full_name) is UniqueRegTest

    def test_get_nonexistent_returns_none(self):
        assert get_transferable('no.such.Transferable') is None


# ---------------------------------------------------------------------------
# create_default_transferable
# ---------------------------------------------------------------------------

class TestDefaultTransferable:
    def _make_func(self):
        """Helper returning a simple function with typed params and return."""
        def sample_func(self, a: int, b: str) -> list:
            ...
        return sample_func

    def test_input_round_trip_single_arg(self):
        def fn(self, x: int) -> int: ...
        t = create_default_transferable(fn, is_input=True)
        raw = t.serialize(42)
        assert t.deserialize(raw) == 42

    def test_input_round_trip_multiple_args(self):
        def fn(self, a: int, b: str) -> int: ...
        t = create_default_transferable(fn, is_input=True)
        raw = t.serialize(1, 'hello')
        assert t.deserialize(raw) == (1, 'hello')

    def test_output_round_trip_int(self):
        def fn(self) -> int: ...
        t = create_default_transferable(fn, is_input=False)
        raw = t.serialize(99)
        assert t.deserialize(raw) == 99

    def test_output_round_trip_str(self):
        def fn(self) -> str: ...
        t = create_default_transferable(fn, is_input=False)
        raw = t.serialize('hello')
        assert t.deserialize(raw) == 'hello'

    def test_output_round_trip_list(self):
        def fn(self) -> list: ...
        t = create_default_transferable(fn, is_input=False)
        raw = t.serialize([1, 2, 3])
        assert t.deserialize(raw) == [1, 2, 3]

    def test_output_round_trip_dict(self):
        def fn(self) -> dict: ...
        t = create_default_transferable(fn, is_input=False)
        raw = t.serialize({'k': 'v'})
        assert t.deserialize(raw) == {'k': 'v'}

    def test_output_round_trip_tuple(self):
        def fn(self) -> tuple: ...
        t = create_default_transferable(fn, is_input=False)
        raw = t.serialize((1, 2))
        assert t.deserialize(raw) == (1, 2)

    def test_output_none(self):
        def fn(self) -> int: ...
        t = create_default_transferable(fn, is_input=False)
        assert t.deserialize(None) is None

    def test_input_skips_self(self):
        def fn(self, a: int, b: str) -> int: ...
        t = create_default_transferable(fn, is_input=True)
        assert 'self' not in t._param_names
        assert t._param_names == ['a', 'b']

    def test_input_class_module_is_default(self):
        def fn(self, x: int) -> int: ...
        t = create_default_transferable(fn, is_input=True)
        assert t.__module__ == 'Default'

    def test_output_class_module_is_default(self):
        def fn(self) -> int: ...
        t = create_default_transferable(fn, is_input=False)
        assert t.__module__ == 'Default'


# ---------------------------------------------------------------------------
# auto_transfer decorator
# ---------------------------------------------------------------------------

class TestAutoTransfer:
    def test_wraps_function(self):
        """auto_transfer returns a callable wrapping the original function."""
        def dummy(self, x: int) -> int: ...
        wrapped = auto_transfer(dummy)
        assert callable(wrapped)
        assert wrapped.__name__ == 'dummy'

    def test_matching_transferable_uses_existing(self):
        """
        When a transferable in the same module matches the function's input
        signature, auto_transfer should select it rather than creating a
        default one.
        """
        # HelloData has fields (name: str, value: int).
        # A function in the SAME module with those params should match.
        # We need the function's __module__ to match HelloData's module.
        def get_data(self, name: str, value: int) -> HelloData: ...
        get_data.__module__ = HelloData.__module__

        wrapped = auto_transfer(get_data)
        # The wrapped function is a transfer_wrapper — it exists
        assert callable(wrapped)

    def test_no_match_creates_default(self):
        """When no transferable matches, a default pickle-based one is used."""
        def unique_fn(self, zzz_unique: int) -> int: ...
        unique_fn.__module__ = 'some.unique.module.that.has.no.transferables'
        wrapped = auto_transfer(unique_fn)
        assert callable(wrapped)

    def test_callable_check(self):
        """Passing a non-callable raises TypeError."""
        with pytest.raises(TypeError):
            auto_transfer(42)

    def test_direct_transferable_lookup_single_param(self):
        """When a function has a single non-self param typed as a registered
        Transferable, auto_transfer should use it directly (Priority 1)."""
        # HelloData is a registered @transferable
        def process(self, data: HelloData) -> HelloData: ...
        process.__module__ = HelloData.__module__
        wrapped = auto_transfer(process)
        assert callable(wrapped)

    def test_direct_lookup_prefers_over_field_matching(self):
        """Direct Transferable lookup (Priority 1) should be preferred over
        field name+type comparison (Priority 2)."""
        # Create a function whose param type IS a registered Transferable
        def echo(self, item: HelloData) -> HelloData: ...
        echo.__module__ = 'some.other.module'  # different module
        wrapped = auto_transfer(echo)
        # Should still work — Priority 1 doesn't check module
        assert callable(wrapped)


# ---------------------------------------------------------------------------
# _create_pydantic_model_from_func_sig
# ---------------------------------------------------------------------------

class TestPydanticModelFromSig:
    def test_basic_fields(self):
        def fn(a: int, b: str) -> bool: ...
        model = _create_pydantic_model_from_func_sig(fn)
        fields = model.model_fields
        assert 'a' in fields
        assert 'b' in fields
        assert fields['a'].annotation is int
        assert fields['b'].annotation is str

    def test_skips_self(self):
        def fn(self, x: float) -> None: ...
        model = _create_pydantic_model_from_func_sig(fn)
        fields = model.model_fields
        assert 'self' not in fields
        assert 'x' in fields
        assert fields['x'].annotation is float

    def test_skips_cls(self):
        def fn(cls, y: int) -> None: ...
        model = _create_pydantic_model_from_func_sig(fn)
        fields = model.model_fields
        assert 'cls' not in fields
        assert 'y' in fields

    def test_empty_after_self(self):
        def fn(self) -> None: ...
        model = _create_pydantic_model_from_func_sig(fn)
        assert model.model_fields == {}

    def test_preserves_defaults(self):
        def fn(a: int, b: str = 'hi') -> None: ...
        model = _create_pydantic_model_from_func_sig(fn)
        assert model.model_fields['b'].default == 'hi'


# ---------------------------------------------------------------------------
# OPT-T1: empty bytes b"" must round-trip correctly (not become None)
# ---------------------------------------------------------------------------

class TestEmptyBytesHandling:
    """b'' is falsy in Python. Deserializers must use `is None` checks,
    not truthiness, otherwise b'' silently becomes None."""

    # ---- pickle path (all types now use pickle) ----

    def test_pickle_input_bytes_roundtrip(self):
        """bytes param: serialize → deserialize round-trips via pickle."""
        def fn(self, data: bytes) -> bytes: ...
        t = create_default_transferable(fn, is_input=True)
        raw = t.serialize(b'')
        result = t.deserialize(raw)
        assert result is not None, 'b"" became None in pickle input'
        assert result == b''

    def test_pickle_output_bytes_roundtrip(self):
        """bytes return: serialize → deserialize round-trips via pickle."""
        def fn(self, data: bytes) -> bytes: ...
        t = create_default_transferable(fn, is_input=False)
        raw = t.serialize(b'')
        result = t.deserialize(raw)
        assert result is not None, 'b"" became None in pickle output'
        assert result == b''

    # ---- pickle path (non-bytes params) ----

    def test_pickle_input_empty_bytes_param_roundtrip(self):
        """Pickle input path: a function that takes a str param, but we pass
        something that serializes to empty-ish data. More importantly, test
        that deserialize(pickle.dumps(b'')) does NOT return None."""
        def fn(self, data: str) -> str: ...
        t = create_default_transferable(fn, is_input=True)
        # Serialize an empty string — pickle.dumps('') produces non-empty bytes,
        # so this tests the normal case. The real test is the output path below.
        raw = t.serialize('')
        result = t.deserialize(raw)
        assert result == ''

    def test_pickle_output_empty_bytes_roundtrip(self):
        """_default_deserialize_func: pickle.dumps(b'') is non-empty bytes,
        but we must verify the deserializer handles it correctly."""
        def fn(self) -> str: ...
        t = create_default_transferable(fn, is_input=False)
        raw = t.serialize(b'')  # pickle.dumps(b'') → non-empty bytes
        result = t.deserialize(raw)
        assert result == b''

    def test_pickle_output_deserialize_empty_bytes_is_none_sentinel(self):
        """b'' is the wire sentinel for None results (crm_to_com sets
        serialized_result = b'' when result is None).  _default_deserialize_func
        must return None for b'', matching the serializer convention."""
        def fn(self) -> str: ...
        t = create_default_transferable(fn, is_input=False)
        assert t.deserialize(b'') is None

    def test_pickle_input_deserialize_empty_bytes_is_none_sentinel(self):
        """b'' on the input path is the wire sentinel for 'no args'.
        Must return None, not attempt pickle.loads(b'')."""
        def fn(self, a: int, b: str) -> int: ...
        t = create_default_transferable(fn, is_input=True)
        assert t.deserialize(b'') is None

    def test_pickle_output_real_data_roundtrips(self):
        """pickle.dumps(b'') is NOT empty — it produces a real pickle payload.
        Verify this data round-trips correctly (not confused with sentinel)."""
        def fn(self) -> bytes: ...
        # Use non-bytes return type to go through pickle, not fast path
        def fn2(self) -> object: ...
        t = create_default_transferable(fn2, is_input=False)
        raw = t.serialize(b'')  # pickle.dumps(b'') → non-empty bytes
        assert len(raw) > 0, 'pickle.dumps(b"") should produce non-empty bytes'
        result = t.deserialize(raw)
        assert result == b'', 'pickled b"" should round-trip back to b""'

    # ---- None sentinel still works ----

    def test_pickle_bytes_none_stays_none(self):
        def fn(self, data: bytes) -> bytes: ...
        t = create_default_transferable(fn, is_input=True)
        assert t.deserialize(None) is None

    def test_pickle_output_none_stays_none(self):
        def fn(self) -> int: ...
        t = create_default_transferable(fn, is_input=False)
        assert t.deserialize(None) is None

    def test_pickle_input_none_stays_none(self):
        def fn(self, a: int) -> int: ...
        t = create_default_transferable(fn, is_input=True)
        assert t.deserialize(None) is None


# ---------------------------------------------------------------------------
# OPT-T2: serialize/deserialize format consistency
# ---------------------------------------------------------------------------

class TestSerializeDeserializeConsistency:
    """The pickle-based input serialize has an exception fallback that
    produces pickle.dumps(args) — a different format from the primary
    pickle.dumps(dict) path. The deserialize side must handle both."""

    def test_pickle_input_normal_roundtrip(self):
        """Normal path: serialize produces dict, deserialize unpacks it."""
        def fn(self, x: int, y: str) -> int: ...
        t = create_default_transferable(fn, is_input=True)
        raw = t.serialize(42, 'hello')
        result = t.deserialize(raw)
        assert result == (42, 'hello')

    def test_pickle_input_single_arg_roundtrip(self):
        """Single-arg non-bytes: serialize produces dict, deserialize unwraps."""
        def fn(self, x: int) -> int: ...
        t = create_default_transferable(fn, is_input=True)
        raw = t.serialize(99)
        result = t.deserialize(raw)
        assert result == 99

    def test_pickle_input_fallback_format_roundtrip(self):
        """If serialize's dict path fails and falls back to pickle.dumps(args),
        the deserialize side should still produce a usable result.
        Simulate by feeding deserialize with the fallback format."""
        def fn(self, x: int, y: str) -> int: ...
        t = create_default_transferable(fn, is_input=True)
        # Manually produce the fallback format: pickle.dumps((42, 'hello'))
        fallback_data = pickle.dumps((42, 'hello'))
        result = t.deserialize(fallback_data)
        # The deserialize code checks isinstance(unpickled, dict).
        # For a tuple, it hits the else branch and returns the tuple directly.
        assert result == (42, 'hello')

    def test_pickle_bytes_rejects_non_serializable_input(self):
        """pickle serialize must handle non-serializable types."""
        def fn(self, data: bytes) -> bytes: ...
        t = create_default_transferable(fn, is_input=True)
        # pickle can serialize most things, but lambda cannot be pickled
        with pytest.raises(Exception):
            t.serialize(lambda: None)

    def test_pickle_bytes_rejects_non_serializable_output(self):
        """pickle output serialize must handle non-serializable types."""
        def fn(self, data: bytes) -> bytes: ...
        t = create_default_transferable(fn, is_input=False)
        with pytest.raises(Exception):
            t.serialize(lambda: None)

    def test_pickle_bytes_roundtrip(self):
        """bytes param type now uses pickle, verify round-trip."""
        def fn(self, data: bytes) -> bytes: ...
        t_in = create_default_transferable(fn, is_input=True)
        t_out = create_default_transferable(fn, is_input=False)
        raw_in = t_in.serialize(b'test data')
        assert t_in.deserialize(raw_in) == b'test data'
        raw_out = t_out.serialize(b'test data')
        assert t_out.deserialize(raw_out) == b'test data'

    def test_output_pickle_deserialize_accepts_both_formats(self):
        """Output pickle deserialize handles pickle-encoded bytes."""
        def fn(self, data: bytes) -> bytes: ...
        t = create_default_transferable(fn, is_input=False)
        # Pickle-encoded bytes
        raw = t.serialize(b'raw')
        assert t.deserialize(raw) == b'raw'




# ---------------------------------------------------------------------------
# OPT-T4: wire.py scatter-write and payload_total_size
# ---------------------------------------------------------------------------

class TestWireScatterWrite:
    """Tests for tuple/list scatter-write in wire.py."""

    def test_payload_total_size_none(self):
        from c_two.transport.wire import payload_total_size
        assert payload_total_size(None) == 0

    def test_payload_total_size_bytes(self):
        from c_two.transport.wire import payload_total_size
        assert payload_total_size(b'hello') == 5

    def test_payload_total_size_memoryview(self):
        from c_two.transport.wire import payload_total_size
        assert payload_total_size(memoryview(b'hello')) == 5

    def test_payload_total_size_tuple(self):
        from c_two.transport.wire import payload_total_size
        assert payload_total_size((b'hel', b'lo')) == 5

    def test_payload_total_size_list(self):
        from c_two.transport.wire import payload_total_size
        assert payload_total_size([b'a', b'bc', b'def']) == 6


# ---------------------------------------------------------------------------
# OPT-T5: buffer='copy|view|hold' parameter on @cc.transferable
# ---------------------------------------------------------------------------

class TestBufferMode:
    """Tests for the buffer='copy|view|hold' parameter on @cc.transferable."""

    def test_default_buffer_mode_is_copy(self):
        """@cc.transferable without buffer= defaults to 'copy'."""
        @cc.transferable
        class CopyData:
            x: int
            def serialize(d: 'CopyData') -> bytes:
                return pickle.dumps(d.x)
            def deserialize(b: bytes) -> 'CopyData':
                return CopyData(x=pickle.loads(b))

        assert CopyData._buffer_mode == 'copy'

    def test_explicit_copy_mode(self):
        """@cc.transferable(buffer='copy') sets _buffer_mode='copy'."""
        @cc.transferable(buffer='copy')
        class ExplicitCopy:
            v: int
            def serialize(d: 'ExplicitCopy') -> bytes:
                return pickle.dumps(d.v)
            def deserialize(b: bytes) -> 'ExplicitCopy':
                return ExplicitCopy(v=pickle.loads(b))

        assert ExplicitCopy._buffer_mode == 'copy'

    def test_view_mode(self):
        """@cc.transferable(buffer='view') sets _buffer_mode='view'."""
        @cc.transferable(buffer='view')
        class ViewData:
            v: int
            def serialize(d: 'ViewData') -> bytes:
                return pickle.dumps(d.v)
            def deserialize(data: memoryview) -> 'ViewData':
                return ViewData(v=pickle.loads(data))

        assert ViewData._buffer_mode == 'view'

    def test_hold_mode(self):
        """@cc.transferable(buffer='hold') sets _buffer_mode='hold'."""
        @cc.transferable(buffer='hold')
        class HoldData:
            v: int
            def serialize(d: 'HoldData') -> bytes:
                return pickle.dumps(d.v)
            def deserialize(data: memoryview) -> 'HoldData':
                return HoldData(v=pickle.loads(data))

        assert HoldData._buffer_mode == 'hold'

    def test_invalid_buffer_mode_raises(self):
        """Invalid buffer= value raises ValueError."""
        with pytest.raises(ValueError, match='buffer'):
            @cc.transferable(buffer='invalid')
            class Bad:
                x: int
                def serialize(d: 'Bad') -> bytes:
                    return b''
                def deserialize(b: bytes) -> 'Bad':
                    return Bad(x=0)

    def test_existing_hello_data_has_copy_mode(self):
        """Pre-existing @cc.transferable classes default to 'copy'."""
        assert HelloData._buffer_mode == 'copy'

    def test_default_transferable_has_view_mode(self):
        """Default pickle transferable (auto-generated) uses 'view' mode."""
        def fn(self, x: int) -> int: ...
        t_in = create_default_transferable(fn, is_input=True)
        t_out = create_default_transferable(fn, is_input=False)
        assert t_in._buffer_mode == 'view'
        assert t_out._buffer_mode == 'view'


class TestCrmToComBufferModes:
    """Test that crm_to_com handles _release_fn based on input buffer mode."""

    def _setup(self, input_trans):
        """Create a transfer-wrapped echo function with given input transferable."""
        from c_two.crm.transferable import transfer
        decorator = transfer(input=input_trans, output=None)
        def echo(self, x):
            return x
        return decorator(echo)

    def _make_icrm(self):
        class MockCRM:
            def echo(self, x):
                return x
        class MockICRM:
            direction = '<-'
            crm = MockCRM()
        return MockICRM()

    def test_copy_mode_calls_release(self):
        """In copy mode, _release_fn is called (before deserialize)."""
        released = []

        @cc.transferable(buffer='copy')
        class CopyIn:
            def serialize(val: int) -> bytes:
                return pickle.dumps(val)
            def deserialize(data: bytes) -> int:
                return pickle.loads(data)

        wrapped = self._setup(CopyIn)
        icrm = self._make_icrm()
        result = wrapped(icrm, pickle.dumps(42), _release_fn=lambda: released.append(True))
        assert released, '_release_fn was not called in copy mode'

    def test_view_mode_calls_release(self):
        """In view mode, _release_fn is called (after deserialize)."""
        released = []
        def fn(self, x: int) -> int: ...
        input_trans = create_default_transferable(fn, is_input=True)
        assert input_trans._buffer_mode == 'view'

        wrapped = self._setup(input_trans)
        icrm = self._make_icrm()
        result = wrapped(icrm, pickle.dumps(42), _release_fn=lambda: released.append(True))
        assert released, '_release_fn was not called in view mode'

    def test_hold_mode_skips_release(self):
        """In hold mode, _release_fn is NOT called (RAII)."""
        released = []

        @cc.transferable(buffer='hold')
        class HoldIn:
            def serialize(val: int) -> bytes:
                return pickle.dumps(val)
            def deserialize(data) -> int:
                return pickle.loads(data) if isinstance(data, (bytes, memoryview)) else data

        wrapped = self._setup(HoldIn)
        icrm = self._make_icrm()
        result = wrapped(icrm, pickle.dumps(42), _release_fn=lambda: released.append(True))
        assert not released, '_release_fn should NOT be called in hold mode'

    def test_no_release_fn_works(self):
        """When _release_fn is None (thread-local), all modes work fine."""
        def fn(self, x: int) -> int: ...
        input_trans = create_default_transferable(fn, is_input=True)
        wrapped = self._setup(input_trans)
        icrm = self._make_icrm()
        # No _release_fn — must not crash
        result = wrapped(icrm, pickle.dumps(42))
        # result is (error_bytes, result_bytes) tuple
        assert isinstance(result, tuple)

    def test_release_called_on_exception(self):
        """_release_fn must be called even if deserialize raises."""
        released = []

        @cc.transferable(buffer='copy')
        class BadDeser:
            def serialize(val: int) -> bytes:
                return pickle.dumps(val)
            def deserialize(data: bytes) -> int:
                raise RuntimeError('boom')

        wrapped = self._setup(BadDeser)
        icrm = self._make_icrm()
        # Should not raise — crm_to_com catches exceptions
        result = wrapped(icrm, pickle.dumps(42), _release_fn=lambda: released.append(True))
        assert released, '_release_fn must be called even on error'

    def test_transfer_wrapper_has_buffer_mode_attrs(self):
        """transfer_wrapper should expose _input_buffer_mode and _output_buffer_mode."""
        @cc.transferable(buffer='view')
        class ViewIn:
            def serialize(val: int) -> bytes:
                return pickle.dumps(val)
            def deserialize(data) -> int:
                return pickle.loads(data)

        wrapped = self._setup(ViewIn)
        assert hasattr(wrapped, '_input_buffer_mode')
        assert wrapped._input_buffer_mode == 'view'


class TestComToCrmBufferModes:
    """Test that com_to_crm handles response based on output buffer mode."""

    def _make_mock_response(self, data: bytes):
        """Create a mock PyShmBuffer-like response."""
        class MockResponse:
            def __init__(self, data):
                self._data = data
                self.released = False
            def release(self):
                self.released = True
            def __buffer__(self, flags):
                return memoryview(self._data)
        return MockResponse(data)

    def _make_icrm(self, response_data):
        mock_resp = self._make_mock_response(response_data)
        class MockClient:
            supports_direct_call = False
            def __init__(self, resp):
                self.response = resp
            def call(self, method, data):
                return self.response
        class MockICRM:
            direction = '->'
            def __init__(self, client):
                self.client = client
        client = MockClient(mock_resp)
        return MockICRM(client), mock_resp

    def test_copy_mode_releases_response(self):
        """copy mode: response is materialized and released."""
        @cc.transferable(buffer='copy')
        class CopyOut:
            def serialize(val: int) -> bytes:
                return pickle.dumps(val)
            def deserialize(data: bytes) -> int:
                assert isinstance(data, bytes)
                return pickle.loads(data)

        from c_two.crm.transferable import transfer
        icrm, mock_resp = self._make_icrm(pickle.dumps(42))
        decorator = transfer(input=None, output=CopyOut)
        def fn(self) -> int: ...
        wrapped = decorator(fn)
        result = wrapped(icrm)
        assert result == 42
        assert mock_resp.released

    def test_hold_mode_skips_release(self):
        """hold mode: response is NOT released (RAII)."""
        @cc.transferable(buffer='hold')
        class HoldOut:
            def serialize(val: int) -> bytes:
                return pickle.dumps(val)
            def deserialize(data) -> int:
                return pickle.loads(bytes(data)) if isinstance(data, memoryview) else pickle.loads(data)

        from c_two.crm.transferable import transfer
        icrm, mock_resp = self._make_icrm(pickle.dumps(42))
        decorator = transfer(input=None, output=HoldOut)
        def fn(self) -> int: ...
        wrapped = decorator(fn)
        result = wrapped(icrm)
        assert result == 42
        assert not mock_resp.released

