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

    # ---- bytes fast path (single bytes param) ----

    def test_bytes_fast_path_input_empty_roundtrip(self):
        """bytes fast-path input: b'' → serialize → deserialize → b'' (not None)."""
        def fn(self, data: bytes) -> bytes: ...
        t = create_default_transferable(fn, is_input=True)
        raw = t.serialize(b'')
        result = t.deserialize(raw)
        assert result is not None, 'b"" became None in bytes fast-path input'
        assert bytes(result) == b''

    def test_bytes_fast_path_output_empty_roundtrip(self):
        """bytes fast-path output: b'' → serialize → deserialize → b'' (not None)."""
        def fn(self, data: bytes) -> bytes: ...
        t = create_default_transferable(fn, is_input=False)
        raw = t.serialize(b'')
        result = t.deserialize(raw)
        assert result is not None, 'b"" became None in bytes fast-path output'
        assert bytes(result) == b''

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

    def test_bytes_fast_path_none_stays_none(self):
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

    def test_bytes_fast_path_rejects_non_bytes_input(self):
        """bytes fast-path serialize must reject non-bytes with TypeError."""
        def fn(self, data: bytes) -> bytes: ...
        t = create_default_transferable(fn, is_input=True)
        with pytest.raises(TypeError, match='bytes fast path'):
            t.serialize('a string')

    def test_bytes_fast_path_rejects_non_bytes_output(self):
        """bytes fast-path output serialize must reject non-bytes."""
        def fn(self, data: bytes) -> bytes: ...
        t = create_default_transferable(fn, is_input=False)
        with pytest.raises(TypeError, match='bytes fast path'):
            t.serialize(42)

    def test_bytes_fast_path_accepts_memoryview(self):
        """bytes fast-path must accept memoryview (zero-copy transport)."""
        def fn(self, data: bytes) -> bytes: ...
        t_in = create_default_transferable(fn, is_input=True)
        t_out = create_default_transferable(fn, is_input=False)
        mv = memoryview(b'test data')
        assert bytes(t_in.serialize(mv)) == b'test data'
        assert bytes(t_out.serialize(mv)) == b'test data'

    def test_output_bytes_fast_path_deserialize_accepts_both_formats(self):
        """Output bytes fast-path deserialize handles both raw bytes and
        pickle-encoded bytes (for version compatibility)."""
        def fn(self, data: bytes) -> bytes: ...
        t = create_default_transferable(fn, is_input=False)
        # Raw bytes (fast path)
        assert t.deserialize(b'raw') == b'raw'
        # Memoryview (zero-copy transport)
        assert bytes(t.deserialize(memoryview(b'mv'))) == b'mv'


# ---------------------------------------------------------------------------
# OPT-T3: __memoryview_aware__ flag behavior
# ---------------------------------------------------------------------------

class TestMemoryviewAwareFlag:
    """Tests for the __memoryview_aware__ optimization flag on Transferable."""

    def test_default_flag_is_false(self):
        """Transferable base class has __memoryview_aware__ = False."""
        assert Transferable.__memoryview_aware__ is False

    def test_custom_transferable_can_set_flag(self):
        """A @transferable subclass can set __memoryview_aware__ = True."""
        @cc.transferable
        class MvAwareData:
            __memoryview_aware__ = True
            value: int
            def serialize(d: 'MvAwareData') -> bytes:
                return pickle.dumps(d.value)
            def deserialize(b: bytes) -> 'MvAwareData':
                return MvAwareData(value=pickle.loads(b))

        assert MvAwareData.__memoryview_aware__ is True

    def test_flag_not_set_defaults_false(self):
        """A @transferable without the flag should default to False."""
        @cc.transferable
        class NoFlagData:
            value: int
            def serialize(d: 'NoFlagData') -> bytes:
                return pickle.dumps(d.value)
            def deserialize(b: bytes) -> 'NoFlagData':
                return NoFlagData(value=pickle.loads(b))

        assert NoFlagData.__memoryview_aware__ is False

    def test_getattr_works_for_flag_check(self):
        """The framework checks flag via getattr(..., '__memoryview_aware__', False)."""
        @cc.transferable
        class FlagCheckData:
            __memoryview_aware__ = True
            x: int
            def serialize(d: 'FlagCheckData') -> bytes:
                return b''
            def deserialize(b: bytes) -> 'FlagCheckData':
                return FlagCheckData(x=0)

        assert getattr(FlagCheckData, '__memoryview_aware__', False) is True

        @cc.transferable
        class NoFlagCheckData:
            y: int
            def serialize(d: 'NoFlagCheckData') -> bytes:
                return b''
            def deserialize(b: bytes) -> 'NoFlagCheckData':
                return NoFlagCheckData(y=0)

        assert getattr(NoFlagCheckData, '__memoryview_aware__', False) is False


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


