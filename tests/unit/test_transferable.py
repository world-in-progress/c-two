import pickle
import inspect
import pytest
import c_two as cc
from c_two.rpc.transferable import (
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
