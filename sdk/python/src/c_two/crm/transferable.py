from __future__ import annotations
import inspect
import sys
import warnings
from functools import wraps
from typing import get_type_hints, Any, Callable, Generic, ParamSpec, TypeVar, Type

from .. import error
from ._payload_abi import (
    MethodPayloadAbiShape,
    MethodParameterShape,
)
from .payload_plan import (
    DEFAULT_PICKLE_PROTOCOL,
    PayloadBinding,
    PayloadPlanKind,
    no_payload_binding,
    python_pickle_input_binding,
    python_pickle_output_binding,
)


R = TypeVar('R')
P = ParamSpec('P')


class HeldResult(Generic[R]):
    """Wraps a method return value with explicit SHM lifecycle control.

    Three-layer safety net:
    1. Explicit .release() — preferred
    2. Context manager (__enter__/__exit__) — recommended
    3. __del__ fallback — last resort with warning
    """

    __slots__ = ('_value', '_release_cb', '_invalidate_cb', '_released', '_buffer')

    def __init__(self, value, release_cb=None, buffer=None, invalidate_cb=None):
        self._value = value
        self._release_cb = release_cb
        self._invalidate_cb = invalidate_cb
        self._buffer = buffer
        self._released = False

    @property
    def value(self):
        if self._released:
            raise RuntimeError("SHM released — value no longer accessible")
        return self._value

    @property
    def buffer(self):
        return self.unsafe_buffer

    @property
    def unsafe_buffer(self):
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
            invalidate_cb = self._invalidate_cb
            self._invalidate_cb = None
            value = self._value
            if invalidate_cb is not None:
                try:
                    invalidate_cb(value)
                except Exception:
                    pass
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


Held = HeldResult


def _invalidate_fastdb_value(value: object) -> None:
    if value is None:
        return
    try:
        from fastdb4py import invalidate as fastdb_invalidate
    except Exception:
        return
    try:
        fastdb_invalidate(value)
    except Exception:
        pass


def hold(method: Callable[P, R]) -> Callable[P, HeldResult[R]]:
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


_VALID_TRANSFER_BUFFERS = frozenset(('view',))

def transfer(*, input=None, output=None, buffer=None):
    """Metadata-only buffer policy decorator for CRM methods."""
    if input is not None or output is not None:
        raise TypeError(
            '@cc.transfer input/output codec overrides were removed; '
            'use FDB annotations for portable CRM payloads or Python pickle '
            'fallback for Python-only methods.',
        )
    if buffer == 'hold':
        raise ValueError(
            "server-side borrowed input is controlled by "
            "cc.register(..., input_lifetime=...), not @cc.transfer(buffer='hold')",
        )
    if buffer is not None and buffer not in _VALID_TRANSFER_BUFFERS:
        raise ValueError(
            f"buffer must be None or one of {sorted(_VALID_TRANSFER_BUFFERS)}, got {buffer!r}"
        )

    def decorator(func):
        func.__cc_transfer__ = {
            'buffer': buffer,
        }
        return func  # NO wrapping
    return decorator


def _build_transfer_wrapper(
    func,
    input: PayloadBinding | None = None,
    output: PayloadBinding | None = None,
    buffer='view',
    payload_abi_context=None,
):
    """Build the com_to_crm / crm_to_com / transfer_wrapper closure.

    This is the runtime transfer closure. It consumes internal payload bindings,
    not user-authored custom codec classes.
    """
    method_name = func.__name__

    def com_to_crm(*args, _c2_buffer=None):
        stage = 'call_crm'
        input_serializer = (
            input.serialize
            if input is not None and input.kind is not PayloadPlanKind.NO_PAYLOAD
            else None
        )
        input_prepare_writer = (
            input.prepare_write
            if input is not None and input.kind is not PayloadPlanKind.NO_PAYLOAD
            else None
        )
        if output is not None and output.kind is not PayloadPlanKind.NO_PAYLOAD:
            if _c2_buffer == 'hold' and output.view_from_buffer is not None:
                output_fn = output.view_from_buffer
                output_hook = 'retained_view'
                output_retains_buffer = True
            else:
                output_fn = output.deserialize
                output_hook = 'deserialize'
                output_retains_buffer = False
        else:
            output_fn = None
            output_hook = None
            output_retains_buffer = False

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
            prepared_args = (
                input_prepare_writer(*request)
                if (
                    request is not None
                    and input_prepare_writer is not None
                    and callable(getattr(client, 'call_prepared', None))
                )
                else None
            )

            stage = 'call_crm'
            if prepared_args is not None:
                response = client.call_prepared(method_name, prepared_args)
            else:
                serialized_args = input_serializer(*request) if (request is not None and input_serializer is not None) else None
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
                if _c2_buffer == 'hold' and output_retains_buffer:
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
                        if output_hook == 'retained_view':
                            raise error.ClientOutputFromBuffer(str(exc)) from exc
                        raise

                    def release_cb():
                        mv.release()
                        try:
                            response.release()
                        except Exception:
                            pass
                    return HeldResult(
                        result,
                        release_cb,
                        buffer=mv,
                        invalidate_cb=_invalidate_fastdb_value,
                    )
                else:
                    try:
                        result = output_fn(mv)
                        if isinstance(result, memoryview):
                            result = bytes(result)
                    finally:
                        mv.release()
                        response.release()
                    if _c2_buffer == 'hold':
                        return HeldResult(result, None)
                    return result
            else:
                if _c2_buffer == 'hold' and output_retains_buffer:
                    mv = memoryview(response)
                    try:
                        result = output_fn(mv)
                    except Exception as exc:
                        mv.release()
                        if output_hook == 'retained_view':
                            raise error.ClientOutputFromBuffer(str(exc)) from exc
                        raise

                    def release_cb():
                        mv.release()

                    return HeldResult(
                        result,
                        release_cb,
                        buffer=mv,
                        invalidate_cb=_invalidate_fastdb_value,
                    )
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
            elif output_hook == 'retained_view':
                raise error.ClientOutputFromBuffer(str(e)) from e
            else:
                raise error.ClientDeserializeOutput(str(e)) from e

    def crm_to_com(*args, _release_fn=None, _c2_input_buffer_mode=None):
        input_buffer_mode = _c2_input_buffer_mode or buffer
        if input is not None and input.kind is not PayloadPlanKind.NO_PAYLOAD:
            if input_buffer_mode == 'borrowed':
                input_hook = 'retained_view'
                if input.kind is PayloadPlanKind.FDB and input.view_from_buffer is not None:
                    input_fn = input.view_from_buffer
                else:
                    def input_fn(_request):
                        raise ValueError(
                            'borrowed input requires a buffer-view FDB input payload',
                        )
            else:
                input_fn = input.deserialize
                input_hook = 'deserialize'
        else:
            input_fn = None
            input_hook = None
        output_serializer = (
            output.serialize
            if output is not None and output.kind is not PayloadPlanKind.NO_PAYLOAD
            else None
        )
        output_prepare_writer = (
            output.prepare_write
            if output is not None and output.kind is not PayloadPlanKind.NO_PAYLOAD
            else None
        )

        err = None
        result = None
        stage = 'deserialize_input'
        deserialized_args: tuple[object, ...] = tuple()

        def release_input_buffer() -> None:
            nonlocal _release_fn
            if input_buffer_mode == 'borrowed':
                _invalidate_fastdb_value(deserialized_args)
            if _release_fn is not None:
                try:
                    _release_fn()
                finally:
                    _release_fn = None

        try:
            if len(args) < 1:
                raise ValueError('Instance method requires self, but only get one argument.')

            contract = args[0]
            crm = contract.resource
            request = args[1] if len(args) > 1 else None

            if request is not None and input_fn is not None:
                if input_buffer_mode == 'view':
                    deserialized_args = input_fn(request)
                    release_input_buffer()
                else:  # borrowed
                    deserialized_args = input_fn(request)
            else:
                deserialized_args = tuple()
                release_input_buffer()

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
                and (
                    input_buffer_mode in ('view', 'borrowed')
                    or stage == 'deserialize_input'
                )
            )
            if should_release_input:
                try:
                    release_input_buffer()
                except Exception:
                    pass
            if stage == 'deserialize_input':
                if input_hook == 'retained_view':
                    err = error.ResourceInputFromBuffer(str(e))
                else:
                    err = error.ResourceDeserializeInput(str(e))
            elif stage == 'execute_function':
                err = error.ResourceExecuteFunction(str(e))
            else:
                err = error.ResourceSerializeOutput(str(e))

        serialized_result = b''
        if err is None and result is not None and (output_prepare_writer is not None or output_serializer is not None):
            try:
                if output_prepare_writer is not None:
                    serialized_result = (
                        output_prepare_writer(*result) if isinstance(result, tuple)
                        else output_prepare_writer(result)
                    )
                else:
                    serialized_result = (
                        output_serializer(*result) if isinstance(result, tuple)
                        else output_serializer(result)
                    )
            except Exception as e:
                err = error.ResourceSerializeOutput(str(e))
                serialized_result = b''

        if _release_fn is not None and input_buffer_mode == 'borrowed':
            try:
                release_input_buffer()
            except Exception:
                pass

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
    transfer_wrapper._input_payload_binding = input
    transfer_wrapper._output_payload_binding = output
    transfer_wrapper._payload_abi_context = dict(payload_abi_context or {})
    return transfer_wrapper

def auto_transfer(func=None, *, input=None, output=None, buffer=None, payload_abi_context=None):
    """Auto-wrap a CRM method with FDB/pickle/no-payload planning."""
    if input is not None or output is not None:
        raise TypeError(
            '@cc.transfer input/output codec overrides were removed; '
            'use FDB annotations for portable CRM payloads or rely on '
            'Python pickle fallback for Python-only methods.',
        )
    if buffer == 'hold':
        raise ValueError(
            "server-side borrowed input is controlled by "
            "cc.register(..., input_lifetime=...), not buffer='hold'",
        )
    if buffer is not None and buffer not in _VALID_TRANSFER_BUFFERS:
        raise ValueError(
            f"buffer must be None or one of {sorted(_VALID_TRANSFER_BUFFERS)}, got {buffer!r}",
        )

    def create_wrapper(func):
        base_payload_abi_context = dict(payload_abi_context or {})
        base_payload_abi_context.setdefault('method_name', func.__name__)

        func_params = _extract_func_params(func)
        is_empty_input = len(func_params) == 0
        input_binding: PayloadBinding = no_payload_binding()
        input_method_payload_abi_aggregate = False
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
            resolved_input = _resolve_fastdb_method_payload_abi(shape, resolution_context)
            if resolved_input is not None:
                input_binding = resolved_input
                input_method_payload_abi_aggregate = True
            else:
                input_binding = python_pickle_input_binding(func)

        try:
            type_hints = get_type_hints(func)
        except (NameError, ValueError, TypeError):
            type_hints = {}

        output_binding: PayloadBinding = no_payload_binding()
        output_method_payload_abi_aggregate = False
        if 'return' in type_hints:
            return_type = type_hints['return']
            if return_type is None or return_type is type(None):
                output_binding = no_payload_binding()
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
                resolved_output = _resolve_fastdb_method_payload_abi(shape, resolution_context)
                if resolved_output is not None:
                    output_binding = resolved_output
                    output_method_payload_abi_aggregate = True
                else:
                    output_binding = python_pickle_output_binding(func)

        effective_buffer = buffer or 'view'

        wrapped_func = _build_transfer_wrapper(
            func,
            input=input_binding,
            output=output_binding,
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
) -> PayloadBinding | None:
    try:
        from c_two.fastdb.call_db import resolve_method_payload_abi
    except ImportError:
        return None
    binding = resolve_method_payload_abi(shape, context)
    if binding is None:
        return None
    if not isinstance(binding, PayloadBinding):
        raise TypeError('FastDB method payload ABI resolver returned a non-PayloadBinding value.')
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
