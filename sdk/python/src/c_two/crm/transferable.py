from __future__ import annotations
import pickle
import inspect
import logging
import sys
import threading
import warnings
from abc import ABCMeta
from functools import wraps
from dataclasses import dataclass, is_dataclass
from typing import get_type_hints, Any, Callable, TypeVar, Type

import struct as _struct

from .. import error
from ._payload_abi import (
    PayloadAbiBinding,
    PayloadAbiRef,
    MethodPayloadAbiShape,
    MethodParameterShape,
    normalize_payload_abi_ref,
)


R = TypeVar('R')
DEFAULT_PICKLE_PROTOCOL = 4


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
) -> tuple[str | None, str | None]:
    if abi_id is not None and not isinstance(abi_id, str):
        raise TypeError('abi_id must be a string.')
    if abi_schema is not None and not isinstance(abi_schema, str):
        raise TypeError('abi_schema must be a string.')
    if abi_id is not None and abi_schema is not None:
        raise ValueError('Declare either abi_id or abi_schema, not both.')
    if abi_id is not None and not abi_id.strip():
        raise ValueError('abi_id cannot be empty.')
    if abi_schema is not None and not abi_schema.strip():
        raise ValueError('abi_schema cannot be empty.')
    return abi_id, abi_schema


def transferable(
    cls=None,
    *,
    abi_id: str | None = None,
    abi_schema: str | None = None,
):
    """Decorator to make a class inherit from Transferable.

    Supports ``@cc.transferable``, ``@cc.transferable()``, and explicit ABI
    declarations for custom byte formats via ``abi_id`` or ``abi_schema``.
    """
    abi_id, abi_schema = _validate_transferable_abi(abi_id, abi_schema)
    return _make_transferable_decorator(cls, abi_id=abi_id, abi_schema=abi_schema)


def _payload_abi_ref_transferable(cls=None, *, payload_abi_ref: object):
    payload_abi_ref = normalize_payload_abi_ref(payload_abi_ref)
    return _make_transferable_decorator(cls, payload_abi_ref=payload_abi_ref)


def _make_transferable_decorator(
    cls=None,
    *,
    abi_id: str | None = None,
    abi_schema: str | None = None,
    payload_abi_ref: PayloadAbiRef | None = None,
):
    def wrap(cls):
        new_cls = type(cls.__name__, (cls, Transferable), dict(cls.__dict__))
        new_cls.__module__ = cls.__module__
        new_cls.__qualname__ = cls.__qualname__
        new_cls.__cc_transferable_abi__ = {
            'abi_id': abi_id,
            'abi_schema': abi_schema,
        }
        new_cls.__cc_payload_abi_ref__ = payload_abi_ref
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


def _build_transfer_wrapper(func, input=None, output=None, buffer='view', payload_abi_context=None):
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
    transfer_wrapper._payload_abi_context = dict(payload_abi_context or {})
    return transfer_wrapper

def auto_transfer(func=None, *, input=None, output=None, buffer=None, payload_abi_context=None):
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
        base_payload_abi_context = dict(payload_abi_context or {})
        base_payload_abi_context.setdefault('method_name', func.__name__)

        # --- Input Matching ---
        input_transferable = input
        input_method_payload_abi_aggregate = False

        if input_transferable is None:
            func_params = _extract_func_params(func)
            is_empty_input = len(func_params) == 0

            if not is_empty_input:
                shape = _method_payload_abi_shape(
                    base_payload_abi_context,
                    func,
                    'input',
                    parameters=func_params,
                )
                resolution_context = _payload_abi_resolution_context(
                    base_payload_abi_context,
                    func,
                    'input',
                )
                binding = _resolve_fastdb_method_payload_abi(shape, resolution_context)
                if binding is not None:
                    input_transferable = binding.transferable
                    input_method_payload_abi_aggregate = True

            if not is_empty_input and len(func_params) == 1:
                _, param_type, _ = func_params[0]
                param_module = getattr(param_type, '__module__', None)
                param_name = getattr(param_type, '__name__', str(param_type))
                full_name = f'{param_module}.{param_name}' if param_module else param_name
                if input_transferable is None:
                    input_transferable = get_transferable(full_name)

            if input_transferable is None and not is_empty_input:
                input_transferable = create_default_transferable(func, is_input=True)

        # --- Output Matching ---
        output_transferable = output
        output_method_payload_abi_aggregate = False

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
                    shape = _method_payload_abi_shape(
                        base_payload_abi_context,
                        func,
                        'output',
                        return_annotation=return_type,
                    )
                    resolution_context = _payload_abi_resolution_context(
                        base_payload_abi_context,
                        func,
                        'output',
                    )
                    binding = _resolve_fastdb_method_payload_abi(shape, resolution_context)
                    if binding is not None:
                        output_transferable = binding.transferable
                        output_method_payload_abi_aggregate = True
                    if output_transferable is None:
                        return_type_name = getattr(return_type, '__name__', str(return_type))
                        return_type_module = getattr(return_type, '__module__', None)
                        return_type_full_name = f'{return_type_module}.{return_type_name}' if return_type_module else return_type_name
                        output_transferable = get_transferable(return_type_full_name)
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
            payload_abi_context=base_payload_abi_context,
        )
        wrapped_func._input_method_payload_abi_aggregate = input_method_payload_abi_aggregate
        wrapped_func._output_method_payload_abi_aggregate = output_method_payload_abi_aggregate
        return wrapped_func

    if func is None:
        return create_wrapper
    else:
        if not callable(func):
            raise TypeError("@auto_transfer requires a callable function or parentheses.")
        return create_wrapper(func)

# Helpers #########################################################################

def _resolve_fastdb_method_payload_abi(
    shape: MethodPayloadAbiShape,
    context: dict[str, Any],
) -> PayloadAbiBinding | None:
    try:
        from c_two.fastdb.call_db import resolve_method_payload_abi
    except ImportError:
        return None
    binding = resolve_method_payload_abi(shape, context)
    if binding is None:
        return None
    if not isinstance(binding, PayloadAbiBinding):
        raise TypeError('FastDB method payload ABI resolver returned a non-PayloadAbiBinding value.')
    return binding


def _payload_abi_resolution_context(
    base_context: dict[str, Any],
    func: Callable,
    position: str,
) -> dict[str, Any]:
    context = dict(base_context)
    context['function'] = func
    context['position'] = position
    context.setdefault('method_name', func.__name__)
    return context


def _method_payload_abi_shape(
    base_context: dict[str, Any],
    func: Callable,
    direction: str,
    *,
    parameters: list[tuple[str, type, Any]] | None = None,
    return_annotation: object | None = None,
) -> MethodPayloadAbiShape:
    return MethodPayloadAbiShape(
        method_name=str(base_context.get('method_name') or func.__name__),
        direction=direction,
        crm_namespace=base_context.get('crm_namespace'),
        crm_name=base_context.get('crm_name'),
        crm_version=base_context.get('crm_version'),
        parameters=(
            _extract_method_parameter_shapes(func)
            if parameters is not None
            else ()
        ),
        return_annotation=return_annotation,
    )


def _extract_method_parameter_shapes(
    func: Callable,
) -> tuple[MethodParameterShape, ...]:
    try:
        sig = inspect.signature(func)
    except (ValueError, TypeError):
        return ()
    try:
        type_hints = get_type_hints(func)
    except (NameError, ValueError, TypeError):
        type_hints = {}

    shapes = []
    for index, (name, param) in enumerate(sig.parameters.items()):
        if index == 0 and name in ('self', 'cls'):
            continue
        annotation = type_hints.get(name, param.annotation)
        if annotation is inspect.Parameter.empty:
            annotation = Any
        shapes.append(
            MethodParameterShape(
                name=name,
                annotation=annotation,
                default=param.default,
                kind=param.kind.name,
            )
        )
    return tuple(shapes)


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
