from __future__ import annotations
import json
import math
import pickle
import inspect
import logging
import sys
import threading
import types
import warnings
from abc import ABCMeta
from functools import wraps
from dataclasses import dataclass, is_dataclass
from typing import get_args, get_origin, get_type_hints, Any, Callable, TypeVar, Type, Union

import struct as _struct

from .. import error
from .codec import CodecRef, normalize_codec_ref, resolve_codec


R = TypeVar('R')
DEFAULT_PICKLE_PROTOCOL = 4
CONTROL_JSON_SCHEMA = 'c-two.control.json.v1'
CONTROL_JSON_CODEC_REF = CodecRef(
    id='c-two.control.json',
    version='1',
    schema=CONTROL_JSON_SCHEMA,
    capabilities=('bytes',),
    media_type='application/json',
)


def _pickle_dumps_default(value: object) -> bytes:
    return pickle.dumps(value, protocol=DEFAULT_PICKLE_PROTOCOL)


class HeldResult:
    """Wraps a method return value with explicit SHM lifecycle control.

    Three-layer safety net:
    1. Explicit .release() — preferred
    2. Context manager (__enter__/__exit__) — recommended
    3. __del__ fallback — last resort with warning
    """

    __slots__ = ('_value', '_release_cb', '_released', '_buffer')

    def __init__(self, value, release_cb=None, buffer=None):
        self._value = value
        self._release_cb = release_cb
        self._buffer = buffer
        self._released = False

    @property
    def value(self):
        if self._released:
            raise RuntimeError("SHM released — value no longer accessible")
        return self._value

    @property
    def buffer(self):
        if self._released:
            raise RuntimeError("SHM released — buffer no longer accessible")
        if self._buffer is None:
            raise RuntimeError("HeldResult has no retained buffer")
        return self._buffer

    def release(self):
        if not self._released:
            self._released = True
            cb = self._release_cb
            self._release_cb = None
            self._value = None
            try:
                if cb is not None:
                    try:
                        cb()
                    except Exception:
                        pass
            finally:
                self._buffer = None

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.release()

    def __del__(self, _is_finalizing=sys.is_finalizing, _warn=warnings.warn):
        # NOTE: _is_finalizing and _warn are bound as default args at class
        # definition time so they survive late interpreter teardown.
        if getattr(self, '_released', True):
            return
        if _is_finalizing():
            self.release()
            return
        _warn(
            "HeldResult was garbage-collected without release() — "
            "potential SHM leak. Use 'with cc.hold(...)' or call .release().",
            ResourceWarning,
            stacklevel=2,
        )
        self.release()


def hold(method):
    """Wrap a CRM bound method to hold SHM on the response.

    Usage: ``cc.hold(proxy.method)(args)`` — single-shot pattern.
    Returns a callable that injects ``_c2_buffer='hold'`` into kwargs.
    """
    if not callable(method):
        raise TypeError(
            f"cc.hold() requires a callable, got {type(method).__name__}"
        )
    self_obj = getattr(method, '__self__', None)
    name = getattr(method, '__name__', None)
    if self_obj is None or name is None:
        raise TypeError(
            "cc.hold() requires a bound CRM method, "
            "e.g. cc.hold(grid.compute)"
        )

    @wraps(method)
    def wrapper(*args, **kwargs):
        kwargs['_c2_buffer'] = 'hold'
        return getattr(self_obj, name)(*args, **kwargs)

    return wrapper


def _add_length_prefix(message_bytes):
    length = len(message_bytes)
    prefix = _struct.pack('>Q', length)
    return prefix + message_bytes

T = TypeVar('T')
logger = logging.getLogger(__name__)

# Global Caches ###################################################################

_TRANSFERABLE_MAP: dict[str, Transferable] = {}
_TRANSFERABLE_LOCK = threading.Lock()

# Definition of Transferable ######################################################

class TransferableMeta(ABCMeta):
    """
    TransferableMeta
    --
    A metaclass for Transferable that:
    1. Automatically converts 'serialize', 'deserialize', and 'from_buffer' methods to static methods
    2. Ensures all subclasses of Transferable are dataclasses
    3. Registers the transferable class in the global transferable map and transferable information list
    """
    def __new__(mcs, name, bases, attrs, **kwargs):
        # Static
        if 'serialize' in attrs and not isinstance(attrs['serialize'], staticmethod):
            attrs['serialize'] = staticmethod(attrs['serialize'])
        if 'deserialize' in attrs and not isinstance(attrs['deserialize'], staticmethod):
            attrs['deserialize'] = staticmethod(attrs['deserialize'])
        if 'from_buffer' in attrs and not isinstance(attrs['from_buffer'], staticmethod):
            attrs['from_buffer'] = staticmethod(attrs['from_buffer'])
        
        # Create the class
        cls = super().__new__(mcs, name, bases, attrs, **kwargs)
        
        # Register
        if name != 'Transferable' and hasattr(cls, 'serialize') and hasattr(cls, 'deserialize'):
            if not is_dataclass(cls):
                cls = dataclass(cls)

            # Register the class except for classes in 'Default' module
            if cls.__module__ != 'Default':
                # Register the class in the global transferable map
                full_name = register_transferable(cls)

                logger.debug(f'Registered transferable: {full_name}')
        return cls

class Transferable(metaclass=TransferableMeta):
    """
    Transferable
    --
    A base specification for classes that can be transferred between `client` and `resource`.  
    Transferable classes are automatically converted to `dataclasses` and should implement the methods:
    - serialize: convert runtime `args` to `bytes` message
    - deserialize: convert `bytes` message to runtime `args`

    """

    def serialize(*args: any) -> bytes:
        """
        serialize is a static method, and the decorator @staticmethod is added in the metaclass.  
        No Need To Add @staticmethod Here.
        """
        ...

    def deserialize(bytes: any) -> any:
        """
        deserialize is a static method, and the decorator @staticmethod is added in the metaclass.  
        No Need To Add @staticmethod Here.
        """
        ...

# Default Transferable Creation Factory ###########################################

def _default_serialize_func(*args):
    """Default serialization function for output transferables."""
    try:
        if len(args) == 1:
            return _pickle_dumps_default(args[0])
        else:
            return _pickle_dumps_default(args)
    except Exception as e:
        logger.error(f'Failed to serialize output data: {e}')
        raise

def _default_deserialize_func(data: memoryview | None):
    # b'' is used as a wire sentinel for None results (see crm_to_com:
    # serialized_result = b'' when result is None).  Must check both
    # None *and* empty bytes so the sentinel round-trips correctly,
    # while still allowing legitimate empty-bytes payloads when the
    # serializer actually produces them (pickle.dumps(b'') is non-empty).
    if data is None or len(data) == 0:
        return None
    return pickle.loads(data)


def create_control_json_transferable(func, is_input: bool):
    """Create a portable JSON control-plane transferable for JSON-safe signatures."""
    if is_input:
        param_specs = _extract_func_params(func)
        param_annotations = [annotation for _name, annotation, _default in param_specs]
        class_name = f'ControlJson{func.__name__.title()}InputTransferable'

        def serialize(*args) -> bytes:
            if len(args) > len(param_annotations):
                raise ValueError(
                    f'{func.__name__} expected at most {len(param_annotations)} '
                    f'control arguments, got {len(args)}',
                )
            return _control_json_dump(
                args,
                param_annotations[:len(args)],
                f'{func.__name__}.input',
            )

        def deserialize(data: memoryview | bytes):
            values = _control_json_load(data, f'{func.__name__}.input')
            if values is None:
                return tuple()
            if len(values) > len(param_annotations):
                raise ValueError(
                    f'{func.__name__} decoded too many control arguments: '
                    f'{len(values)} > {len(param_annotations)}',
                )
            restored = [
                _control_from_json_safe(value, annotation, f'{func.__name__}.input[{index}]')
                for index, (value, annotation) in enumerate(
                    zip(values, param_annotations, strict=False),
                )
            ]
            return tuple(restored)

        ControlJsonInputTransferable = type(
            class_name,
            (Transferable,),
            {
                '__module__': 'c_two.control',
                '__cc_codec_ref__': CONTROL_JSON_CODEC_REF,
                'serialize': serialize,
                'deserialize': deserialize,
            },
        )
        ControlJsonInputTransferable._is_input = True
        ControlJsonInputTransferable._original_func = func
        ControlJsonInputTransferable._param_names = [name for name, _annotation, _default in param_specs]
        ControlJsonInputTransferable._type_hints = _safe_type_hints(func)
        return ControlJsonInputTransferable

    type_hints = _safe_type_hints(func)
    return_annotation = type_hints.get('return')
    class_name = f'ControlJson{func.__name__.title()}OutputTransferable'

    def serialize(*args) -> bytes:
        annotations = _control_output_value_annotations(return_annotation, len(args), func.__name__)
        return _control_json_dump(args, annotations, f'{func.__name__}.output')

    def deserialize(data: memoryview | bytes):
        values = _control_json_load(data, f'{func.__name__}.output')
        if values is None:
            return None
        annotations = _control_output_value_annotations(
            return_annotation,
            len(values),
            func.__name__,
        )
        restored = [
            _control_from_json_safe(value, annotation, f'{func.__name__}.output[{index}]')
            for index, (value, annotation) in enumerate(zip(values, annotations, strict=False))
        ]
        if _is_tuple_annotation(return_annotation):
            return tuple(restored)
        if len(restored) != 1:
            raise ValueError(
                f'{func.__name__} decoded {len(restored)} values for non-tuple output.',
            )
        return restored[0]

    ControlJsonOutputTransferable = type(
        class_name,
        (Transferable,),
        {
            '__module__': 'c_two.control',
            '__cc_codec_ref__': CONTROL_JSON_CODEC_REF,
            'serialize': serialize,
            'deserialize': deserialize,
        },
    )
    ControlJsonOutputTransferable._is_input = False
    ControlJsonOutputTransferable._original_func = func
    ControlJsonOutputTransferable._return_type = return_annotation
    return ControlJsonOutputTransferable


def _safe_type_hints(func) -> dict[str, Any]:
    try:
        return get_type_hints(func)
    except (NameError, ValueError, TypeError):
        return {}


def _control_output_value_annotations(
    return_annotation: Any,
    value_count: int,
    method_name: str,
) -> list[Any]:
    if _is_tuple_annotation(return_annotation):
        args = get_args(return_annotation)
        if len(args) == 2 and args[1] is Ellipsis:
            return [args[0]] * value_count
        if len(args) != value_count:
            raise ValueError(
                f'{method_name} expected {len(args)} tuple output values, got {value_count}',
            )
        return list(args)
    if value_count != 1:
        raise ValueError(f'{method_name} expected one output value, got {value_count}')
    return [return_annotation]


def _control_json_dump(values: tuple[Any, ...], annotations: list[Any], path: str) -> bytes:
    wire_values = [
        _control_to_json_safe(value, annotation, f'{path}[{index}]')
        for index, (value, annotation) in enumerate(zip(values, annotations, strict=False))
    ]
    payload = {
        'schema': CONTROL_JSON_SCHEMA,
        'values': wire_values,
    }
    return json.dumps(payload, sort_keys=True, separators=(',', ':')).encode()


def _control_json_load(data: memoryview | bytes | None, path: str) -> list[Any] | None:
    if data is None or len(data) == 0:
        return None
    try:
        payload = json.loads(bytes(data))
    except Exception as exc:
        raise ValueError(f'{path} is not valid JSON control data: {exc}') from exc
    if not isinstance(payload, dict):
        raise ValueError(f'{path} control payload must be an object.')
    if payload.get('schema') != CONTROL_JSON_SCHEMA:
        raise ValueError(f'{path} control payload schema must be {CONTROL_JSON_SCHEMA}.')
    values = payload.get('values')
    if not isinstance(values, list):
        raise ValueError(f'{path} control payload values must be a list.')
    return values


def _is_control_json_annotation(annotation: Any) -> bool:
    if annotation is inspect.Signature.empty or annotation is Any:
        return False
    if annotation is None or annotation is type(None):
        return True
    if annotation in {bool, int, float, str}:
        return True
    if annotation in {bytes, memoryview, bytearray}:
        return False
    origin = get_origin(annotation)
    args = get_args(annotation)
    if origin in {Union, types.UnionType}:
        return bool(args) and all(_is_control_json_annotation(arg) for arg in args)
    if origin is list:
        return len(args) == 1 and _is_control_json_annotation(args[0])
    if origin is dict:
        return len(args) == 2 and args[0] is str and _is_control_json_annotation(args[1])
    if origin is tuple:
        if not args:
            return False
        if len(args) == 2 and args[1] is Ellipsis:
            return _is_control_json_annotation(args[0])
        return all(_is_control_json_annotation(arg) for arg in args)
    return False


def _is_tuple_annotation(annotation: Any) -> bool:
    return get_origin(annotation) is tuple


def _control_to_json_safe(value: Any, annotation: Any, path: str) -> Any:
    if annotation is None or annotation is type(None):
        if value is not None:
            raise ValueError(f'{path} must be None.')
        return None
    origin = get_origin(annotation)
    args = get_args(annotation)
    if origin in {Union, types.UnionType}:
        if value is None and type(None) in args:
            return None
        failures = []
        for arg in args:
            if arg is type(None):
                continue
            try:
                return _control_to_json_safe(value, arg, path)
            except (TypeError, ValueError) as exc:
                failures.append(str(exc))
        raise ValueError(f'{path} does not match any union item: {"; ".join(failures)}')
    if annotation is bool:
        if not isinstance(value, bool):
            raise TypeError(f'{path} must be bool.')
        return value
    if annotation is int:
        if not isinstance(value, int) or isinstance(value, bool):
            raise TypeError(f'{path} must be int.')
        return value
    if annotation is float:
        if not isinstance(value, (int, float)) or isinstance(value, bool):
            raise TypeError(f'{path} must be float.')
        converted = float(value)
        if not math.isfinite(converted):
            raise ValueError(f'{path} must be finite.')
        return converted
    if annotation is str:
        if not isinstance(value, str):
            raise TypeError(f'{path} must be str.')
        return value
    if origin is list:
        if not isinstance(value, list):
            raise TypeError(f'{path} must be list.')
        item_annotation = args[0]
        return [
            _control_to_json_safe(item, item_annotation, f'{path}[{index}]')
            for index, item in enumerate(value)
        ]
    if origin is dict:
        if args[0] is not str:
            raise TypeError(f'{path} only supports dict[str, T].')
        if not isinstance(value, dict):
            raise TypeError(f'{path} must be dict.')
        value_annotation = args[1]
        result = {}
        for key, item in value.items():
            if not isinstance(key, str):
                raise TypeError(f'{path} keys must be str.')
            result[key] = _control_to_json_safe(item, value_annotation, f'{path}.{key}')
        return result
    if origin is tuple:
        if not isinstance(value, tuple):
            raise TypeError(f'{path} must be tuple.')
        if len(args) == 2 and args[1] is Ellipsis:
            return [
                _control_to_json_safe(item, args[0], f'{path}[{index}]')
                for index, item in enumerate(value)
            ]
        if len(value) != len(args):
            raise ValueError(f'{path} expected tuple length {len(args)}, got {len(value)}.')
        return [
            _control_to_json_safe(item, item_annotation, f'{path}[{index}]')
            for index, (item, item_annotation) in enumerate(zip(value, args, strict=True))
        ]
    raise TypeError(f'{path} has unsupported control annotation {annotation!r}.')


def _control_from_json_safe(value: Any, annotation: Any, path: str) -> Any:
    if annotation is None or annotation is type(None):
        if value is not None:
            raise ValueError(f'{path} must be None.')
        return None
    origin = get_origin(annotation)
    args = get_args(annotation)
    if origin in {Union, types.UnionType}:
        if value is None and type(None) in args:
            return None
        failures = []
        for arg in args:
            if arg is type(None):
                continue
            try:
                return _control_from_json_safe(value, arg, path)
            except (TypeError, ValueError) as exc:
                failures.append(str(exc))
        raise ValueError(f'{path} does not match any union item: {"; ".join(failures)}')
    if annotation is bool:
        if not isinstance(value, bool):
            raise TypeError(f'{path} must be bool.')
        return value
    if annotation is int:
        if not isinstance(value, int) or isinstance(value, bool):
            raise TypeError(f'{path} must be int.')
        return value
    if annotation is float:
        if not isinstance(value, (int, float)) or isinstance(value, bool):
            raise TypeError(f'{path} must be float.')
        converted = float(value)
        if not math.isfinite(converted):
            raise ValueError(f'{path} must be finite.')
        return converted
    if annotation is str:
        if not isinstance(value, str):
            raise TypeError(f'{path} must be str.')
        return value
    if origin is list:
        if not isinstance(value, list):
            raise TypeError(f'{path} must be list.')
        item_annotation = args[0]
        return [
            _control_from_json_safe(item, item_annotation, f'{path}[{index}]')
            for index, item in enumerate(value)
        ]
    if origin is dict:
        if args[0] is not str:
            raise TypeError(f'{path} only supports dict[str, T].')
        if not isinstance(value, dict):
            raise TypeError(f'{path} must be dict.')
        value_annotation = args[1]
        return {
            key: _control_from_json_safe(item, value_annotation, f'{path}.{key}')
            for key, item in value.items()
        }
    if origin is tuple:
        if not isinstance(value, list):
            raise TypeError(f'{path} tuple wire value must be list.')
        if len(args) == 2 and args[1] is Ellipsis:
            return tuple(
                _control_from_json_safe(item, args[0], f'{path}[{index}]')
                for index, item in enumerate(value)
            )
        if len(value) != len(args):
            raise ValueError(f'{path} expected tuple length {len(args)}, got {len(value)}.')
        return tuple(
            _control_from_json_safe(item, item_annotation, f'{path}[{index}]')
            for index, (item, item_annotation) in enumerate(zip(value, args, strict=True))
        )
    raise TypeError(f'{path} has unsupported control annotation {annotation!r}.')


def create_default_transferable(func, is_input: bool):
    """
    Factory function to dynamically create a Transferable class for a given function.

    This factory analyzes the function signature and creates a transferable class
    that uses pickle-based serialization as a fallback.

    Args:
        func: The function to create a transferable for
        is_input: True for input parameters, False for return values

    Returns:
        A dynamically created Transferable class
    """
    if is_input:
        # Create dynamic class name for input
        class_name = f'Default{func.__name__.title()}InputTransferable'
        
        # Create transferable for function input parameters
        sig = inspect.signature(func)
        param_names = list(sig.parameters.keys())
        try:
            type_hints = get_type_hints(func)
        except (NameError, ValueError, TypeError):
            type_hints = {}
        
        # Filter out 'self' or 'cls' if they are the first parameter
        filtered_params = []
        for i, name in enumerate(param_names):
            if i == 0 and name in ('self', 'cls'):
                continue
            filtered_params.append(name)

        # All input types use pickle-based serialization.
        # pickle.loads() accepts memoryview since Python 3.8, so the
        # deserializer is memoryview-aware for zero-copy transport.
        class DynamicInputTransferable(Transferable):
            __module__ = 'Default'

            def serialize(*args) -> bytes:
                """Default serialization: pickle args directly as tuple."""
                try:
                    if len(args) == 1:
                        return _pickle_dumps_default(args[0])
                    return _pickle_dumps_default(args)
                except Exception as e:
                    logger.error(f'Failed to serialize input data: {e}')
                    raise

            def deserialize(data: memoryview | bytes):
                """Default deserialization: unpickle and return value or tuple."""
                try:
                    if data is None or len(data) == 0:
                        return None
                    unpickled = pickle.loads(data)
                    return unpickled
                except Exception as e:
                    raise ValueError(f"Failed to deserialize input data for {func.__name__}: {e}")
        
        # Set the dynamic class properties
        DynamicInputTransferable._is_input = True
        DynamicInputTransferable._original_func = func
        DynamicInputTransferable.__name__ = class_name
        DynamicInputTransferable._type_hints = type_hints
        DynamicInputTransferable.__qualname__ = class_name
        DynamicInputTransferable._param_names = filtered_params
        return DynamicInputTransferable
    
    else:
        # Create dynamic class name for output
        class_name = f'Default{func.__name__.title()}OutputTransferable'
        
        # Create transferable for function return value
        try:
            type_hints = get_type_hints(func)
        except (NameError, ValueError, TypeError):
            type_hints = {}
        return_type = type_hints.get('return')

        serialize_func = staticmethod(_default_serialize_func)
        deserialize_func = staticmethod(_default_deserialize_func)

        # Define the dynamic transferable class for output
        attrs = {
            '__module__': 'Default',
            'serialize': serialize_func,
            'deserialize': deserialize_func,
        }
        if return_type is bytes:
            pass  # already True above
        DynamicOutputTransferable = type(
            class_name,
            (Transferable,),
            attrs,
        )
        
        # Set the dynamic class properties
        DynamicOutputTransferable._is_input = False
        DynamicOutputTransferable.__name__ = class_name
        DynamicOutputTransferable._original_func = func
        DynamicOutputTransferable.__qualname__ = class_name
        DynamicOutputTransferable._return_type = return_type
        return DynamicOutputTransferable

# Transferable-related interfaces #################################################

def register_transferable(transferable: Transferable) -> str:
    full_name = f'{transferable.__module__}.{transferable.__name__}' if transferable.__module__ else transferable.__name__
    with _TRANSFERABLE_LOCK:
        _TRANSFERABLE_MAP[full_name] = transferable
    return full_name

def get_transferable(full_name: str) -> Transferable | None:
    with _TRANSFERABLE_LOCK:
        return _TRANSFERABLE_MAP.get(full_name)

# Transferable-related decorators #################################################

def _validate_transferable_abi(
    abi_id: str | None,
    abi_schema: str | None,
    codec_ref: object | None,
) -> tuple[str | None, str | None, object | None]:
    if abi_id is not None and not isinstance(abi_id, str):
        raise TypeError('abi_id must be a string.')
    if abi_schema is not None and not isinstance(abi_schema, str):
        raise TypeError('abi_schema must be a string.')
    if abi_id is not None and abi_schema is not None:
        raise ValueError('Declare either abi_id or abi_schema, not both.')
    if codec_ref is not None and (abi_id is not None or abi_schema is not None):
        raise ValueError('Declare either codec_ref or legacy abi_id/abi_schema, not both.')
    if abi_id is not None and not abi_id.strip():
        raise ValueError('abi_id cannot be empty.')
    if abi_schema is not None and not abi_schema.strip():
        raise ValueError('abi_schema cannot be empty.')
    normalized_codec_ref = normalize_codec_ref(codec_ref) if codec_ref is not None else None
    return abi_id, abi_schema, normalized_codec_ref


def transferable(
    cls=None,
    *,
    abi_id: str | None = None,
    abi_schema: str | None = None,
    codec_ref: object | None = None,
):
    """Decorator to make a class inherit from Transferable.

    Supports ``@cc.transferable``, ``@cc.transferable()``, and explicit ABI
    declarations for custom byte formats via ``abi_id``, ``abi_schema``, or
    ``codec_ref``.
    """
    abi_id, abi_schema, codec_ref = _validate_transferable_abi(abi_id, abi_schema, codec_ref)

    def wrap(cls):
        new_cls = type(cls.__name__, (cls, Transferable), dict(cls.__dict__))
        new_cls.__module__ = cls.__module__
        new_cls.__qualname__ = cls.__qualname__
        new_cls.__cc_transferable_abi__ = {
            'abi_id': abi_id,
            'abi_schema': abi_schema,
        }
        new_cls.__cc_codec_ref__ = codec_ref
        return new_cls

    if cls is not None:
        if not isinstance(cls, type):
            raise TypeError('@cc.transferable must decorate a class.')
        return wrap(cls)
    return wrap

_VALID_TRANSFER_BUFFERS = frozenset(('view', 'hold'))

def transfer(*, input=None, output=None, buffer=None):
    """Metadata-only decorator for CRM methods.

    Attaches ``__cc_transfer__`` dict to the function. Does NOT wrap it.
    Consumed by ``crm()`` → ``auto_transfer()`` at class decoration time.

    Parameters
    ----------
    input : type[Transferable] | None
        Custom input transferable. None = auto-bundle via pickle.
    output : type[Transferable] | None
        Custom output transferable. None = auto-bundle via pickle.
    buffer : 'view' | 'hold' | None
        Input buffer mode (resource-side). None = auto-detect from input
        transferable's ``from_buffer`` availability. Default None.
    """
    if buffer is not None and buffer not in _VALID_TRANSFER_BUFFERS:
        raise ValueError(
            f"buffer must be None or one of {sorted(_VALID_TRANSFER_BUFFERS)}, got {buffer!r}"
        )

    def decorator(func):
        func.__cc_transfer__ = {
            'input': input,
            'output': output,
            'buffer': buffer,
        }
        return func  # NO wrapping
    return decorator


def _build_transfer_wrapper(func, input=None, output=None, buffer='view', codec_context=None):
    """Build the com_to_crm / crm_to_com / transfer_wrapper closure.

    This is the internal implementation that was previously inside transfer().
    Called by auto_transfer() after resolving input/output transferables.
    """
    method_name = func.__name__

    def com_to_crm(*args, _c2_buffer=None):
        stage = 'call_crm'
        input_transferable = input.serialize if input else None
        # Output construction hook: from_buffer when hold mode and available
        if output is not None:
            if _c2_buffer == 'hold' and hasattr(output, 'from_buffer') and callable(output.from_buffer):
                output_fn = output.from_buffer
                output_hook = 'from_buffer'
            else:
                output_fn = output.deserialize
                output_hook = 'deserialize'
        else:
            output_fn = None
            output_hook = None

        try:
            if len(args) < 1:
                raise ValueError('Instance method requires self, but only get one argument.')

            crm = args[0]
            client = crm.client
            request = args[1:] if len(args) > 1 else None
            # Thread fast path — skip all serialization/deserialization
            if getattr(client, 'supports_direct_call', False):
                stage = 'execute_direct'
                result = client.call_direct(method_name, request or ())
                if _c2_buffer == 'hold':
                    return HeldResult(result, None)
                return result

            # Standard cross-process path
            stage = 'serialize_input'
            serialized_args = input_transferable(*request) if (request is not None and input_transferable is not None) else None

            stage = 'call_crm'
            response = client.call(method_name, serialized_args)

            stage = 'deserialize_output'
            if not output_fn:
                if hasattr(response, 'release'):
                    response.release()
                if _c2_buffer == 'hold':
                    return HeldResult(None, None)
                return None

            if hasattr(response, 'release'):
                mv = memoryview(response)
                if _c2_buffer == 'hold':
                    try:
                        result = output_fn(mv)
                        if hasattr(response, 'track_retained'):
                            tracker = getattr(client, 'lease_tracker', None)
                            if tracker is not None:
                                route_name = getattr(client, 'route_name', '')
                                response.track_retained(
                                    tracker,
                                    route_name,
                                    method_name,
                                    'client_response',
                                )
                    except Exception as exc:
                        mv.release()
                        try:
                            response.release()
                        except Exception:
                            pass
                        if output_hook == 'from_buffer':
                            raise error.ClientOutputFromBuffer(str(exc)) from exc
                        raise

                    def release_cb():
                        mv.release()
                        try:
                            response.release()
                        except Exception:
                            pass
                    return HeldResult(result, release_cb, buffer=mv)
                else:
                    # view (default)
                    try:
                        result = output_fn(mv)
                        if isinstance(result, memoryview):
                            result = bytes(result)
                    finally:
                        mv.release()
                        response.release()
                    return result
            else:
                if _c2_buffer == 'hold':
                    mv = memoryview(response)
                    try:
                        result = output_fn(mv) if output_hook == 'from_buffer' else output_fn(response)
                    except Exception as exc:
                        mv.release()
                        if output_hook == 'from_buffer':
                            raise error.ClientOutputFromBuffer(str(exc)) from exc
                        raise

                    def release_cb():
                        mv.release()

                    return HeldResult(result, release_cb, buffer=mv)
                result = output_fn(response)
                if _c2_buffer == 'hold':
                    return HeldResult(result, None)
                return result

        except error.CCBaseError:
            raise
        except Exception as e:
            if stage == 'serialize_input':
                raise error.ClientSerializeInput(str(e)) from e
            elif stage == 'call_crm':
                raise error.ClientCallResource(str(e)) from e
            elif stage == 'execute_direct':
                raise error.ResourceExecuteFunction(str(e)) from e
            elif output_hook == 'from_buffer':
                raise error.ClientOutputFromBuffer(str(e)) from e
            else:
                raise error.ClientDeserializeOutput(str(e)) from e

    def crm_to_com(*args, _release_fn=None):
        # Select input construction hook based on buffer mode.
        if input is not None:
            if buffer == 'hold' and hasattr(input, 'from_buffer') and callable(input.from_buffer):
                input_fn = input.from_buffer
                input_hook = 'from_buffer'
            else:
                input_fn = input.deserialize
                input_hook = 'deserialize'
        else:
            input_fn = None
            input_hook = None
        output_transferable = output.serialize if output else None
        input_buffer_mode = buffer

        err = None
        result = None
        stage = 'deserialize_input'

        try:
            if len(args) < 1:
                raise ValueError('Instance method requires self, but only get one argument.')

            contract = args[0]
            crm = contract.resource
            request = args[1] if len(args) > 1 else None

            if request is not None and input_fn is not None:
                if input_buffer_mode == 'view':
                    deserialized_args = input_fn(request)
                    if _release_fn is not None:
                        _release_fn()
                        _release_fn = None
                else:  # hold
                    deserialized_args = input_fn(request)
            else:
                deserialized_args = tuple()
                if _release_fn is not None:
                    _release_fn()
                    _release_fn = None

            if not isinstance(deserialized_args, tuple):
                deserialized_args = (deserialized_args,)

            crm_method = getattr(crm, method_name, None)
            if crm_method is None:
                raise ValueError(f'Method "{method_name}" not found on resource class.')

            stage = 'execute_function'
            result = crm_method(*deserialized_args)
            err = None

        except Exception as e:
            result = None
            should_release_input = (
                _release_fn is not None
                and (input_buffer_mode == 'view' or stage == 'deserialize_input')
            )
            if should_release_input:
                try:
                    _release_fn()
                    _release_fn = None
                except Exception:
                    pass
            if stage == 'deserialize_input':
                if input_hook == 'from_buffer':
                    err = error.ResourceInputFromBuffer(str(e))
                else:
                    err = error.ResourceDeserializeInput(str(e))
            elif stage == 'execute_function':
                err = error.ResourceExecuteFunction(str(e))
            else:
                err = error.ResourceSerializeOutput(str(e))

        serialized_result = b''
        if err is None and output_transferable is not None and result is not None:
            try:
                serialized_result = (
                    output_transferable(*result) if isinstance(result, tuple)
                    else output_transferable(result)
                )
            except Exception as e:
                err = error.ResourceSerializeOutput(str(e))
                serialized_result = b''

        serialized_error = error.CCError.serialize(err)
        return (serialized_error, serialized_result)

    @wraps(func)
    def transfer_wrapper(*args, **kwargs):
        if not args:
            raise ValueError('No arguments provided to determine direction.')

        crm = args[0]
        if not hasattr(crm, 'direction'):
            raise AttributeError('The CRM instance does not have a "direction" attribute.')

        if crm.direction == '->':
            _c2_buffer = kwargs.pop('_c2_buffer', None)
            return com_to_crm(*args, _c2_buffer=_c2_buffer)
        elif crm.direction == '<-':
            return crm_to_com(*args, **kwargs)
        else:
            raise ValueError(f'Invalid direction value: {crm.direction}. Expected "->" or "<-".')

    transfer_wrapper._input_buffer_mode = buffer
    transfer_wrapper._input_transferable = input
    transfer_wrapper._output_transferable = output
    transfer_wrapper._codec_context = dict(codec_context or {})
    return transfer_wrapper

def auto_transfer(func=None, *, input=None, output=None, buffer=None, codec_context=None):
    """Auto-wrap a function with transfer logic.

    When called without kwargs, performs auto-matching:
    - Single-param direct match → registered @transferable
    - Return type direct match → registered @transferable
    - Fallback → DynamicInput/OutputTransferable (pickle)

    Buffer mode auto-detection:
    - buffer=None (default): 'hold' if input transferable has from_buffer, else 'view'
    - buffer='view'/'hold': explicit override
    """
    def create_wrapper(func):
        base_codec_context = dict(codec_context or {})
        base_codec_context.setdefault('method_name', func.__name__)

        # --- Input Matching ---
        input_transferable = input

        if input_transferable is None:
            func_params = _extract_func_params(func)
            is_empty_input = len(func_params) == 0

            if not is_empty_input and len(func_params) == 1:
                _, param_type, _ = func_params[0]
                param_module = getattr(param_type, '__module__', None)
                param_name = getattr(param_type, '__name__', str(param_type))
                full_name = f'{param_module}.{param_name}' if param_module else param_name
                input_transferable = get_transferable(full_name)
                if input_transferable is None:
                    binding = resolve_codec(
                        param_type,
                        _codec_resolution_context(base_codec_context, func, 'input'),
                    )
                    if binding is not None:
                        input_transferable = binding.transferable

            if (
                input_transferable is None
                and not is_empty_input
                and all(_is_control_json_annotation(param_type) for _name, param_type, _default in func_params)
            ):
                input_transferable = create_control_json_transferable(func, is_input=True)

            if input_transferable is None and not is_empty_input:
                input_transferable = create_default_transferable(func, is_input=True)

        # --- Output Matching ---
        output_transferable = output

        if output_transferable is None:
            try:
                type_hints = get_type_hints(func)
            except (NameError, ValueError, TypeError):
                type_hints = {}

            if 'return' in type_hints:
                return_type = type_hints['return']
                if return_type is None or return_type is type(None):
                    output_transferable = None
                else:
                    return_type_name = getattr(return_type, '__name__', str(return_type))
                    return_type_module = getattr(return_type, '__module__', None)
                    return_type_full_name = f'{return_type_module}.{return_type_name}' if return_type_module else return_type_name
                    output_transferable = get_transferable(return_type_full_name)
                    if output_transferable is None:
                        binding = resolve_codec(
                            return_type,
                            _codec_resolution_context(base_codec_context, func, 'output'),
                        )
                        if binding is not None:
                            output_transferable = binding.transferable
                    if output_transferable is None and _is_control_json_annotation(return_type):
                        output_transferable = create_control_json_transferable(func, is_input=False)
                    if output_transferable is None and not (return_type is None or return_type is type(None)):
                        output_transferable = create_default_transferable(func, is_input=False)

        # --- Buffer Mode Resolution (NEW) ---
        effective_buffer = buffer
        if effective_buffer is None:
            # Auto-detect: hold if input transferable has from_buffer
            has_from_buffer = (
                input_transferable is not None
                and hasattr(input_transferable, 'from_buffer')
                and callable(input_transferable.from_buffer)
                and getattr(input_transferable, '_c2_auto_input_hold', True)
            )
            effective_buffer = 'hold' if has_from_buffer else 'view'
            
            # Log auto-detection
            if effective_buffer == 'hold':
                logger.debug(
                    'Auto-detected hold mode for %s (input type has from_buffer)',
                    func.__name__,
                )

        # Validate: hold requires from_buffer on input transferable
        if effective_buffer == 'hold' and input_transferable is not None:
            has_fb = (
                hasattr(input_transferable, 'from_buffer')
                and callable(input_transferable.from_buffer)
            )
            if not has_fb:
                raise TypeError(
                    f"buffer='hold' on {func.__name__} requires "
                    f"{getattr(input_transferable, '__name__', input_transferable)} "
                    f"to implement from_buffer()"
                )

        # --- Wrapping ---
        wrapped_func = _build_transfer_wrapper(
            func,
            input=input_transferable,
            output=output_transferable,
            buffer=effective_buffer,
            codec_context=base_codec_context,
        )
        return wrapped_func

    if func is None:
        return create_wrapper
    else:
        if not callable(func):
            raise TypeError("@auto_transfer requires a callable function or parentheses.")
        return create_wrapper(func)

# Helpers #########################################################################

def _codec_resolution_context(
    base_context: dict[str, Any],
    func: Callable,
    position: str,
) -> dict[str, Any]:
    context = dict(base_context)
    context['function'] = func
    context['position'] = position
    context.setdefault('method_name', func.__name__)
    return context


def _extract_func_params(func: Callable) -> list[tuple[str, type, Any]]:
    """
    Extract input parameters from a function signature using pure inspect.

    Returns a list of (name, annotation, default) tuples, skipping 'self'/'cls'.
    Returns an empty list if the function has no parameters (beyond self/cls)
    or if the signature cannot be determined.
    """
    try:
        sig = inspect.signature(func)
    except (ValueError, TypeError):
        return []
    try:
        type_hints = get_type_hints(func)
    except (NameError, ValueError, TypeError):
        type_hints = {}

    params = []
    for i, (name, param) in enumerate(sig.parameters.items()):
        if i == 0 and name in ('self', 'cls'):
            continue
        annotation = type_hints.get(name, param.annotation)
        if annotation is inspect.Parameter.empty:
            annotation = Any
        default = param.default if param.default is not inspect.Parameter.empty else ...
        params.append((name, annotation, default))
    return params
