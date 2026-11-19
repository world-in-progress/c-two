from __future__ import annotations

import inspect
import sys
import warnings
from functools import wraps
from collections.abc import Iterator
from typing import Any, Callable, Generic, ParamSpec, TYPE_CHECKING, TypeVar, get_type_hints, overload

from .. import error
from .payload_plan import (
    PayloadBinding,
    PayloadPlanKind,
    _payload_type,
    decode_stage_of,
    fastdb_payload_binding,
    no_payload_binding,
    python_pickle_input_binding,
    python_pickle_output_binding,
)

R = TypeVar("R")
P = ParamSpec("P")

if TYPE_CHECKING:
    from .stream import Stream


class HeldResult(Generic[R]):
    """A result whose local response copy must be released explicitly."""

    __slots__ = (
        "_value", "_release_cb", "_invalidate_cb", "_released", "_buffer",
        "_native_response", "_release_attempted",
    )

    def __init__(
        self,
        value: R,
        release_cb: Callable[[], None] | None = None,
        buffer: memoryview | None = None,
        invalidate_cb: Callable[[R], None] | None = None,
    ) -> None:
        self._value = value
        self._release_cb = release_cb
        self._invalidate_cb = invalidate_cb
        self._buffer = buffer
        self._released = False
        self._native_response: object | None = None
        self._release_attempted = False

    @property
    def value(self) -> R:
        if self._released:
            raise RuntimeError("Local copy released — value no longer accessible")
        return self._value

    @property
    def unsafe_buffer(self) -> memoryview:
        if self._released:
            raise RuntimeError("Local copy released — buffer no longer accessible")
        if self._buffer is None:
            raise RuntimeError("HeldResult has no retained buffer")
        return self._buffer

    def release(self) -> None:
        self._release_attempted = True
        if self._native_response is not None:
            self._release_native_response()
            return
        if self._released:
            return

        self._release_python_buffer()

    def _release_python_buffer(self) -> None:
        self._released = True
        release_cb = self._release_cb
        invalidate_cb = self._invalidate_cb
        value = self._value
        self._release_cb = None
        self._invalidate_cb = None
        self._value = None  # type: ignore[assignment]

        first_error: BaseException | None = None
        if invalidate_cb is not None:
            try:
                invalidate_cb(value)
            except BaseException as exc:
                first_error = exc
        view, self._buffer = self._buffer, None
        if view is not None:
            try:
                view.release()
            except BaseException as exc:
                if first_error is None:
                    first_error = exc
                else:
                    _attach_cleanup_error(first_error, exc)
        try:
            if release_cb is not None:
                release_cb()
        except BaseException as exc:
            if first_error is None:
                raise
            _attach_cleanup_error(first_error, exc)

        if first_error is not None:
            try:
                raise _client_output_error(first_error) from first_error
            finally:
                del first_error

    def _release_native_response(self) -> None:
        response = self._native_response
        assert response is not None
        first_error: BaseException | None = None
        if self._buffer is not None:
            try:
                self._buffer.release()
            except BaseException as exc:
                first_error = exc
            else:
                self._buffer = None

        try:
            if response.is_released or self._invalidate_cb is None:
                response.release()
            else:
                response.invalidate_then_release(self._invalidate_cb, self._value)
        except BaseException as exc:
            if first_error is None:
                first_error = exc
            else:
                _attach_cleanup_error(first_error, exc)
        finally:
            if response.is_released:
                self._released = True
                self._value = None  # type: ignore[assignment]
                self._invalidate_cb = None
                self._buffer = None
                self._native_response = None

        if first_error is not None:
            try:
                raise _client_output_error(first_error) from first_error
            finally:
                del first_error

    def __enter__(self) -> HeldResult[R]:
        return self

    def __exit__(self, exc_type, exc_value, traceback) -> None:
        try:
            self.release()
        except BaseException as cleanup_error:
            if exc_type is None:
                raise
            _attach_cleanup_error(exc_value, cleanup_error)

    def __del__(
        self,
        _is_finalizing=sys.is_finalizing,
        _warn=warnings.warn,
    ) -> None:
        response = getattr(self, "_native_response", None)
        if getattr(self, "_released", True) and response is None:
            return
        if not _is_finalizing():
            if not getattr(self, "_release_attempted", False):
                message = (
                    "HeldResult was garbage-collected without release() — "
                    "retained local copy. Use 'with cc.hold(...)' or call .release()."
                )
            else:
                message = "HeldResult release() did not complete before garbage collection"
            _warn(
                message,
                ResourceWarning,
                stacklevel=2,
            )
        try:
            self.release()
        except BaseException as cleanup_error:
            if not _is_finalizing():
                _warn(
                    f"HeldResult final release failed: {_safe_error_message(cleanup_error)}",
                    ResourceWarning,
                    stacklevel=2,
                )



@overload
def hold(method: Callable[P, Iterator[R]]) -> Callable[P, Stream[HeldResult[R]]]: ...


@overload
def hold(method: Callable[P, R]) -> Callable[P, HeldResult[R]]: ...


def hold(method):
    """Retain one local response copy until the returned owner is released."""

    if not callable(method):
        raise TypeError(
            f"cc.hold() requires a callable, got {type(method).__name__}",
        )
    self_obj = getattr(method, "__self__", None)
    name = getattr(method, "__name__", None)
    if self_obj is None or name is None:
        raise TypeError(
            "cc.hold() requires a bound CRM method, "
            "e.g. cc.hold(grid.compute)",
        )

    @wraps(method)
    def wrapper(*args, **kwargs):
        kwargs["_c2_buffer"] = "hold"
        return getattr(self_obj, name)(*args, **kwargs)

    return wrapper


def transfer(*, input=None, output=None):
    """Bind explicit nested FastDB specs to a portable CRM method."""

    def decorator(func):
        func.__cc_transfer__ = {
            "input": input,
            "output": output,
        }
        return func

    return decorator


def _build_transfer_wrapper(
    func,
    input: PayloadBinding | None = None,
    output: PayloadBinding | None = None,
):
    """Build the client/resource closure over opaque payload bytes."""

    if getattr(func, '__cc_stream__', False):
        from .stream import _build_stream_wrapper
        return _build_stream_wrapper(func, input, output)

    method_name = func.__name__
    method_signature = inspect.signature(func)

    def com_to_crm(*args, _c2_buffer=None, _c2_follow_budget=None):
        prepared = None
        primary = None
        dispatched = False
        input_serializer = (
            input.serialize
            if input is not None and input.kind is not PayloadPlanKind.NO_PAYLOAD
            else None
        )
        output_decoder = (
            output.deserialize
            if output is not None and output.kind is not PayloadPlanKind.NO_PAYLOAD
            else None
        )
        retained_owner = (
            _c2_buffer == "hold"
            and output is not None
            and output.supports_scoped_owner
        )

        if len(args) < 1:
            raise ValueError(
                "Instance method requires self, but no instance was provided.",
            )
        crm = args[0]
        client = crm.client
        if _c2_follow_budget is not None:
            client = client.resolve_for_call(_c2_follow_budget)
        else:
            following = getattr(client, '_following_local', None)
            if following is not None:
                target = following.target
                if target is not None:
                    client = client.snapshot_followed_target(target)
        budget = _c2_follow_budget

        request = args[1:] if len(args) > 1 else None
        if getattr(client, "supports_direct_call", False):
            from ..transport.client.proxy import _LocalRouteClosed
            try:
                result = client.call_direct(method_name, request or ())
            except _LocalRouteClosed:
                if budget is None:
                    budget = crm.client.start_call_budget()
                return com_to_crm(
                    *args, _c2_buffer=_c2_buffer, _c2_follow_budget=budget,
                )
            except error.CCBaseError:
                raise
            except Exception as exc:
                raise error.ResourceExecutionError(
                    _safe_error_message(exc), details=_cause_details(exc),
                ) from exc
            return HeldResult(result) if _c2_buffer == "hold" else result

        # Reserve before running Python serializer hooks; native preparation owns D.
        prepare_call = getattr(client, "prepare_call", None)
        if callable(prepare_call):
            prepared = prepare_call(method_name)
        try:
            if (
                prepared is not None
                and input is not None
                and input.kind is PayloadPlanKind.FASTDB
                and request is not None
                and len(request) == 1
            ):
                try:
                    nbytes = (
                        request[0].execution_report().used_bytes
                        if isinstance(request[0], _payload_type(required=True))
                        else None
                    )
                except BaseException as exc:
                    raise _client_input_error(exc) from exc
                if nbytes is not None:
                    prepared.charge_input(nbytes)
            try:
                serialized_args = (
                    input_serializer(*request)
                    if request is not None and input_serializer is not None
                    else None
                )
            except BaseException as exc:
                raise _client_input_error(exc) from exc

            call_client = client if prepared is None else prepared
            dispatched = True
            response = call_client.call(method_name, serialized_args)

            view = None
            decoded = False
            decoding = False
            try:
                native_response = hasattr(response, "release")
                if output_decoder is None:
                    _release_response(response)
                    return HeldResult(None) if _c2_buffer == "hold" else None
                if native_response or retained_owner:
                    view = memoryview(response)
                decoding = True
                result = output_decoder(view if view is not None else response)
                decoding = False
                decoded = True
                if retained_owner:
                    if hasattr(response, "track_retained"):
                        tracker = getattr(client, "local_copy_tracker", None)
                        if tracker is not None:
                            response.track_retained(
                                tracker, getattr(client, "route_name", ""),
                                method_name, "client_response",
                            )
                    held = HeldResult(result, buffer=view, invalidate_cb=output.invalidate)
                    if native_response:
                        held._native_response = response
                    return held
            except BaseException as exc:
                if decoded and output is not None and output.invalidate is not None:
                    _cleanup_preserving_primary(
                        exc, lambda: _invalidate_and_release_held_response(
                            view, response, output.invalidate, result,
                        ),
                    )
                elif output_decoder is not None:
                    _cleanup_preserving_primary(
                        exc, lambda: _release_view_and_response(view, response),
                    )
                raise _client_output_error(exc, decoding=decoding) from exc

            if native_response:
                try:
                    _release_view_and_response(view, response)
                except BaseException as exc:
                    if output is not None and output.invalidate is not None:
                        _cleanup_preserving_primary(exc, lambda: output.invalidate(result))
                    raise _client_output_error(exc) from exc
            return HeldResult(result) if _c2_buffer == "hold" else result
        except BaseException as exc:
            primary = exc
            raise
        finally:
            try:
                if prepared is not None:
                    _close_prepared_handle(prepared, primary, dispatched)
            finally:
                del primary

    def crm_to_com(
        *args,
        _release_fn=None,
        _c2_input_buffer_mode=None,
    ):
        input_buffer_mode = _c2_input_buffer_mode or "view"
        input_decoder = (
            input.deserialize
            if input is not None and input.kind is not PayloadPlanKind.NO_PAYLOAD
            else None
        )
        output_serializer = (
            output.serialize
            if output is not None and output.kind is not PayloadPlanKind.NO_PAYLOAD
            else None
        )
        if input_buffer_mode == "borrowed":
            if input is None or not input.supports_scoped_owner:
                def input_decoder(_request):
                    raise ValueError(
                        "borrowed input requires an explicit FastDB Payload binding",
                    )

        stage = "deserialize_input"
        deserialized_args: tuple[object, ...] = ()

        def release_input_buffer() -> None:
            nonlocal _release_fn, deserialized_args
            first_error: BaseException | None = None
            if (
                input_buffer_mode == "borrowed"
                and input is not None
                and input.invalidate is not None
            ):
                values, deserialized_args = deserialized_args, ()
                for value in values:
                    try:
                        input.invalidate(value)
                    except BaseException as exc:
                        if first_error is None:
                            first_error = exc
            if _release_fn is not None:
                release_fn = _release_fn
                _release_fn = None
                try:
                    release_fn()
                except BaseException as exc:
                    if first_error is None:
                        first_error = exc
                    else:
                        _attach_cleanup_error(first_error, exc)
            if first_error is not None:
                try:
                    raise first_error
                finally:
                    del first_error

        try:
            if len(args) < 1:
                raise ValueError(
                    "Instance method requires self, but no instance was provided.",
                )

            contract = args[0]
            resource = contract.resource
            request = args[1] if len(args) > 1 else None

            if input_decoder is not None:
                decoded = input_decoder(request)
                deserialized_args = decoded if isinstance(decoded, tuple) else (decoded,)
                if input_buffer_mode != "borrowed":
                    release_input_buffer()
            else:
                deserialized_args = ()
                release_input_buffer()

            resource_method = getattr(resource, method_name, None)
            if resource_method is None:
                raise ValueError(
                    f'Method "{method_name}" not found on resource class.',
                )

            stage = "execute_function"
            result = resource_method(*deserialized_args)
        except BaseException as exc:
            failure: BaseException = exc
            try:
                if stage == "deserialize_input":
                    code = (
                        error.ErrorCode.ResourceInputFromBuffer
                        if decode_stage_of(exc) == 'from_buffer'
                        else error.ErrorCode.ResourceInputDeserializing
                    )
                    failure = error.SerializationError(
                        code, _safe_error_message(exc), details=_cause_details(exc),
                    )
                else:
                    projected = (
                        error._callback_semantic_copy(exc)
                        if isinstance(exc, error.CCBaseError)
                        else None
                    )
                    failure = (
                        projected
                        if projected is not None
                        else error.ResourceExecutionError(
                            _safe_error_message(exc), details=_cause_details(exc),
                        )
                    )
                raise failure from exc
            finally:
                try:
                    _cleanup_preserving_primary(failure, release_input_buffer)
                finally:
                    del failure

        serialized_result = b""
        if output_serializer is not None:
            try:
                serialized_result = output_serializer(result)
            except BaseException as exc:
                failure: BaseException = exc
                try:
                    failure = error.SerializationError(
                        error.ErrorCode.ResourceOutputSerializing,
                        _safe_error_message(exc),
                        details=_cause_details(exc),
                    )
                    raise failure from exc
                finally:
                    try:
                        _cleanup_preserving_primary(failure, release_input_buffer)
                    finally:
                        del failure

        if _release_fn is not None or input_buffer_mode == "borrowed":
            # Borrowed inputs invalidate only after their output serialization
            # completed; this is their single release point.
            try:
                release_input_buffer()
            except BaseException as exc:
                raise error.SerializationError(
                    error.ErrorCode.ResourceInputFromBuffer,
                    _safe_error_message(exc),
                    details=_cause_details(exc),
                ) from exc

        return serialized_result

    @wraps(func)
    def transfer_wrapper(*args, **kwargs):
        if not args:
            raise ValueError("No arguments provided to determine direction.")

        crm = args[0]
        if not hasattr(crm, "direction"):
            raise AttributeError(
                'The CRM instance does not have a "direction" attribute.',
            )

        if crm.direction == "->":
            c2_buffer = kwargs.pop("_c2_buffer", None)
            bound = method_signature.bind(*args, **kwargs)
            bound.apply_defaults()
            if bound.kwargs:
                names = ", ".join(sorted(bound.kwargs))
                raise TypeError(
                    f"{method_name} cannot transport keyword-only arguments: "
                    f"{names}.",
                )
            return com_to_crm(
                *bound.args,
                _c2_buffer=c2_buffer,
            )
        if crm.direction == "<-":
            return crm_to_com(*args, **kwargs)
        raise ValueError(
            f"Invalid direction value: {crm.direction}. Expected '->' or '<-'.",
        )

    transfer_wrapper._input_payload_binding = input
    transfer_wrapper._output_payload_binding = output
    return transfer_wrapper


def auto_transfer(func=None, *, input=None, output=None):
    """Wrap a CRM method with explicit FastDB, pickle, or no-payload bindings."""

    def create_wrapper(target):
        parameters = _rpc_parameters(target)
        hints = _resolved_hints(target)
        payload_type = _payload_type(required=input is not None or output is not None)

        if input is not None:
            if len(parameters) != 1:
                raise TypeError(
                    f"{target.__name__} input binding requires exactly one "
                    "fastdb4py.payload.Payload parameter.",
                )
            parameter = parameters[0]
            if parameter.kind not in {
                inspect.Parameter.POSITIONAL_ONLY,
                inspect.Parameter.POSITIONAL_OR_KEYWORD,
            }:
                raise TypeError(
                    f"{target.__name__}.{parameter.name} must be positional "
                    "for a portable Payload binding.",
                )
            if hints.get(parameter.name) is not payload_type:
                raise TypeError(
                    f"{target.__name__}.{parameter.name} must be annotated as "
                    "fastdb4py.payload.Payload for an explicit input binding.",
                )
            if parameter.default is not inspect.Parameter.empty:
                raise TypeError(
                    f"{target.__name__}.{parameter.name} cannot define a default "
                    "for a portable Payload binding.",
                )
            input_binding = fastdb_payload_binding(
                input,
                label=f"{target.__name__}.input",
            )
        elif not parameters:
            input_binding = no_payload_binding()
        else:
            if payload_type is not None and any(
                hints.get(parameter.name) is payload_type for parameter in parameters
            ):
                raise TypeError(
                    f"{target.__name__} uses fastdb4py.payload.Payload input "
                    "without @cc.transfer(input=...).",
                )
            input_binding = python_pickle_input_binding(target)

        return_annotation = hints.get("return", inspect.Signature.empty)
        is_stream = getattr(target, '__cc_stream__', False)
        if is_stream:
            from .stream import stream_item_annotation
            return_annotation = stream_item_annotation(return_annotation)
        if output is not None:
            if return_annotation is not payload_type:
                raise TypeError(
                    f"{target.__name__} must return "
                    "fastdb4py.payload.Payload for an explicit output binding.",
                )
            output_binding = fastdb_payload_binding(
                output,
                label=f"{target.__name__}.output",
            )
        elif not is_stream and return_annotation in (None, type(None), inspect.Signature.empty):
            output_binding = no_payload_binding()
        else:
            if return_annotation is payload_type:
                raise TypeError(
                    f"{target.__name__} returns fastdb4py.payload.Payload "
                    "without @cc.transfer(output=...).",
                )
            output_binding = python_pickle_output_binding(target)

        return _build_transfer_wrapper(
            target,
            input=input_binding,
            output=output_binding,
        )

    if func is None:
        return create_wrapper
    if not callable(func):
        raise TypeError("@auto_transfer requires a callable or parentheses.")
    return create_wrapper(func)


def _rpc_parameters(func: Callable[..., object]) -> list[inspect.Parameter]:
    parameters = list(inspect.signature(func).parameters.values())
    if parameters and parameters[0].name in {"self", "cls"}:
        parameters = parameters[1:]
    return parameters


def _resolved_hints(func: Callable[..., object]) -> dict[str, Any]:
    try:
        return get_type_hints(func, include_extras=True)
    except (NameError, TypeError, ValueError):
        return dict(getattr(func, "__annotations__", {}))


def _release_response(response: object) -> None:
    release = getattr(response, "release", None)
    if callable(release):
        release()


def _release_view_and_response(view: memoryview | None, response: object) -> None:
    first_error: BaseException | None = None
    try:
        if view is not None:
            view.release()
    except BaseException as exc:
        first_error = exc
    try:
        _release_response(response)
    except BaseException as exc:
        if first_error is None:
            first_error = exc
        else:
            _attach_cleanup_error(first_error, exc)
    if first_error is not None:
        try:
            raise first_error
        finally:
            del first_error


def _invalidate_and_release_held_response(
    view: memoryview | None,
    response: object,
    invalidator: Callable[[object], None],
    value: object,
) -> None:
    first_error: BaseException | None = None
    try:
        if view is not None:
            view.release()
    except BaseException as exc:
        first_error = exc

    core_release = getattr(response, "invalidate_then_release", None)
    if callable(core_release):
        try:
            core_release(invalidator, value)
        except BaseException as exc:
            if first_error is None:
                first_error = exc
            else:
                _attach_cleanup_error(first_error, exc)
    else:
        try:
            invalidator(value)
        except BaseException as exc:
            if first_error is None:
                first_error = exc
            else:
                _attach_cleanup_error(first_error, exc)
        try:
            _release_response(response)
        except BaseException as exc:
            if first_error is None:
                first_error = exc
            else:
                _attach_cleanup_error(first_error, exc)

    if first_error is not None:
        try:
            raise first_error
        finally:
            del first_error


def _safe_error_message(value: object) -> str:
    try:
        return str(value)
    except BaseException:
        return f"<{type(value).__qualname__}: unprintable value>"


def _close_prepared_handle(
    prepared: Any,
    primary: BaseException | None,
    dispatched: bool,
) -> None:
    if primary is not None:
        _cleanup_preserving_primary(primary, prepared.close)
        return
    try:
        prepared.close()
    except BaseException as cleanup_error:
        if dispatched:
            cleaned = error._post_dispatch_cleanup_error(cleanup_error)
            if cleaned is not cleanup_error:
                try:
                    raise cleaned from cleanup_error
                finally:
                    del cleaned, cleanup_error
        raise


def _cleanup_preserving_primary(
    primary: BaseException,
    cleanup: Callable[[], None],
) -> None:
    try:
        cleanup()
    except BaseException as cleanup_error:
        _attach_cleanup_error(primary, cleanup_error)
    finally:
        del primary


def _attach_cleanup_error(primary: BaseException, cleanup: BaseException) -> None:
    if cleanup is primary:
        return
    previous = primary.__context__
    cursor = cleanup
    seen = {id(cleanup)}
    while cursor.__context__ is not None:
        following = cursor.__context__
        if following is primary:
            cursor.__context__ = previous if id(previous) not in seen else None
            break
        if id(following) in seen:
            break
        seen.add(id(following))
        cursor = following
    else:
        if previous is not None and id(previous) not in seen:
            cursor.__context__ = previous
    primary.__context__ = cleanup
    if sys.version_info >= (3, 11):
        BaseException.add_note(
            primary,
            f"C-Two cleanup also failed: {type(cleanup).__name__}: {_safe_error_message(cleanup)}",
        )


def _client_input_error(exc: BaseException) -> error.SerializationError:
    return error.SerializationError(
        error.ErrorCode.ClientInputSerializing,
        _safe_error_message(exc),
        details=_cause_details(exc),
        transport_phase='pre_dispatch',
    )


def _client_output_error(
    exc: BaseException, *, decoding: bool = False,
) -> error.SerializationError:
    code = error.ErrorCode.ClientOutputFromBuffer
    if decoding and decode_stage_of(exc) != 'from_buffer':
        code = error.ErrorCode.ClientOutputDeserializing
    return error.SerializationError(
        code, _safe_error_message(exc), details=_cause_details(exc), transport_phase=None,
    )


def _cause_details(exc: BaseException) -> dict[str, str]:
    payload_error = getattr(sys.modules.get("fastdb4py.payload"), "PayloadError", None)
    if payload_error is None or not isinstance(exc, payload_error):
        return {}

    fields: dict[str, str] = {}
    for source, target in (
        ("code", "fastdb_code"),
        ("symbol", "fastdb_symbol"),
        ("path", "fastdb_path"),
        ("message", "fastdb_message"),
        ("details_json", "fastdb_details_json"),
    ):
        value = getattr(exc, source, None)
        if value is not None:
            fields[target] = _safe_error_message(value)
    if fields:
        fields["cause_owner"] = "fastdb"
    return fields
