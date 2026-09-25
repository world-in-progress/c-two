from __future__ import annotations

import inspect
import sys
import warnings
from functools import wraps
from typing import Any, Callable, Generic, ParamSpec, TypeVar, get_type_hints

from .. import error
from .payload_plan import (
    PayloadBinding,
    PayloadPlanKind,
    fastdb_payload_binding,
    no_payload_binding,
    python_pickle_input_binding,
    python_pickle_output_binding,
)

R = TypeVar("R")
P = ParamSpec("P")


class HeldResult(Generic[R]):
    """A result whose C-Two lease must be released explicitly."""

    __slots__ = ("_value", "_release_cb", "_invalidate_cb", "_released", "_buffer")

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

    @property
    def value(self) -> R:
        if self._released:
            raise RuntimeError("SHM released — value no longer accessible")
        return self._value

    @property
    def unsafe_buffer(self) -> memoryview:
        if self._released:
            raise RuntimeError("SHM released — buffer no longer accessible")
        if self._buffer is None:
            raise RuntimeError("HeldResult has no retained buffer")
        return self._buffer

    def release(self) -> None:
        if self._released:
            return

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
        try:
            if release_cb is not None:
                release_cb()
        except BaseException as exc:
            if first_error is None:
                first_error = exc
        finally:
            self._buffer = None

        if first_error is not None:
            raise first_error

    def __enter__(self) -> HeldResult[R]:
        return self

    def __exit__(self, exc_type, exc_value, traceback) -> None:
        try:
            self.release()
        except BaseException:
            if exc_type is None:
                raise

    def __del__(
        self,
        _is_finalizing=sys.is_finalizing,
        _warn=warnings.warn,
    ) -> None:
        if getattr(self, "_released", True):
            return
        if not _is_finalizing():
            _warn(
                "HeldResult was garbage-collected without release() — "
                "potential SHM leak. Use 'with cc.hold(...)' or call .release().",
                ResourceWarning,
                stacklevel=2,
            )
        try:
            self.release()
        except BaseException:
            pass


Held = HeldResult


def hold(method: Callable[P, R]) -> Callable[P, HeldResult[R]]:
    """Retain one remote response lease until the returned owner is released."""

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


_VALID_TRANSFER_BUFFERS = frozenset(("view",))


def transfer(*, input=None, output=None, buffer=None):
    """Bind explicit nested FastDB specs to a portable CRM method."""

    if buffer == "hold":
        raise ValueError(
            "server-side scoped input is controlled by "
            "cc.register(..., input_lifetime=...), not @cc.transfer(buffer='hold')",
        )
    if buffer is not None and buffer not in _VALID_TRANSFER_BUFFERS:
        raise ValueError(
            f"buffer must be None or one of {sorted(_VALID_TRANSFER_BUFFERS)}, "
            f"got {buffer!r}",
        )

    def decorator(func):
        func.__cc_transfer__ = {
            "input": input,
            "output": output,
            "buffer": buffer,
        }
        return func

    return decorator


def _build_transfer_wrapper(
    func,
    input: PayloadBinding | None = None,
    output: PayloadBinding | None = None,
    buffer: str = "view",
):
    """Build the client/resource closure over opaque payload bytes."""

    method_name = func.__name__
    method_signature = inspect.signature(func)

    def com_to_crm(*args, _c2_buffer=None):
        stage = "call_crm"
        output_hook = "deserialize"
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

        try:
            if len(args) < 1:
                raise ValueError(
                    "Instance method requires self, but no instance was provided.",
                )

            crm = args[0]
            client = crm.client
            request = args[1:] if len(args) > 1 else None

            if getattr(client, "supports_direct_call", False):
                stage = "execute_direct"
                result = client.call_direct(method_name, request or ())
                if _c2_buffer == "hold":
                    return HeldResult(result)
                return result

            stage = "serialize_input"
            serialized_args = (
                input_serializer(*request)
                if request is not None and input_serializer is not None
                else None
            )

            stage = "call_crm"
            response = client.call(method_name, serialized_args)

            stage = "deserialize_output"
            if output_decoder is None:
                _release_response(response)
                if _c2_buffer == "hold":
                    return HeldResult(None)
                return None

            if hasattr(response, "release"):
                view = memoryview(response)
                if retained_owner:
                    output_hook = "scoped_owner"
                    result_ready = False
                    try:
                        result = output_decoder(view)
                        result_ready = True
                        if hasattr(response, "track_retained"):
                            tracker = getattr(client, "lease_tracker", None)
                            if tracker is not None:
                                response.track_retained(
                                    tracker,
                                    getattr(client, "route_name", ""),
                                    method_name,
                                    "client_response",
                                )
                    except BaseException as exc:
                        if (
                            result_ready
                            and output is not None
                            and output.invalidate is not None
                        ):
                            _cleanup_preserving_primary(
                                exc,
                                lambda: _invalidate_and_release_held_response(
                                    view,
                                    response,
                                    output.invalidate,
                                    result,
                                ),
                            )
                        else:
                            _cleanup_preserving_primary(
                                exc,
                                lambda: _release_view_and_response(view, response),
                            )
                        raise

                    def release_cb() -> None:
                        _invalidate_and_release_held_response(
                            view,
                            response,
                            output.invalidate,
                            result,
                        )

                    return HeldResult(
                        result,
                        release_cb,
                        buffer=view,
                    )

                try:
                    result = output_decoder(view)
                except BaseException as exc:
                    _cleanup_preserving_primary(
                        exc,
                        lambda: _release_view_and_response(view, response),
                    )
                    raise
                try:
                    _release_view_and_response(view, response)
                except BaseException as exc:
                    if output is not None and output.invalidate is not None:
                        _cleanup_preserving_primary(
                            exc,
                            lambda: output.invalidate(result),
                        )
                    raise
            elif retained_owner:
                output_hook = "scoped_owner"
                view = memoryview(response)
                try:
                    result = output_decoder(view)
                except BaseException as exc:
                    _cleanup_preserving_primary(exc, view.release)
                    raise

                return HeldResult(
                    result,
                    view.release,
                    buffer=view,
                    invalidate_cb=output.invalidate,
                )
            else:
                result = output_decoder(response)

            if _c2_buffer == "hold":
                return HeldResult(result)
            return result

        except error.CCBaseError:
            raise
        except Exception as exc:
            details = _cause_details(exc)
            if stage == "serialize_input":
                raise error.ClientSerializeInput(str(exc), details=details) from exc
            if stage == "call_crm":
                raise error.ClientCallResource(str(exc), details=details) from exc
            if stage == "execute_direct":
                raise error.ResourceExecuteFunction(str(exc), details=details) from exc
            if output_hook == "scoped_owner":
                raise error.ClientOutputFromBuffer(str(exc), details=details) from exc
            raise error.ClientDeserializeOutput(str(exc), details=details) from exc

    def crm_to_com(
        *args,
        _release_fn=None,
        _c2_input_buffer_mode=None,
        _c2_output_allocator=None,
    ):
        del _c2_output_allocator
        input_buffer_mode = _c2_input_buffer_mode or buffer
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
        input_hook = "deserialize"
        if input_buffer_mode == "borrowed":
            input_hook = "scoped_owner"
            if input is None or not input.supports_scoped_owner:
                def input_decoder(_request):
                    raise ValueError(
                        "borrowed input requires an explicit FastDB Payload binding",
                    )

        err = None
        result = None
        stage = "deserialize_input"
        deserialized_args: tuple[object, ...] = ()

        def release_input_buffer() -> None:
            nonlocal _release_fn
            first_error: BaseException | None = None
            if (
                input_buffer_mode == "borrowed"
                and input is not None
                and input.invalidate is not None
            ):
                for value in deserialized_args:
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
            if first_error is not None:
                raise first_error

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
        except Exception as exc:
            result = None
            if _release_fn is not None:
                try:
                    release_input_buffer()
                except Exception:
                    pass
            details = _cause_details(exc)
            if stage == "deserialize_input":
                if input_hook == "scoped_owner":
                    err = error.ResourceInputFromBuffer(
                        str(exc),
                        details=details,
                    )
                else:
                    err = error.ResourceDeserializeInput(
                        str(exc),
                        details=details,
                    )
            elif stage == "execute_function":
                err = error.ResourceExecuteFunction(str(exc), details=details)
            else:
                err = error.ResourceSerializeOutput(str(exc), details=details)

        serialized_result = b""
        if err is None and output_serializer is not None:
            try:
                stage = "serialize_output"
                serialized_result = output_serializer(result)
            except Exception as exc:
                err = error.ResourceSerializeOutput(
                    str(exc),
                    details=_cause_details(exc),
                )
                serialized_result = b""

        if _release_fn is not None:
            try:
                release_input_buffer()
            except Exception as exc:
                if err is None:
                    err = error.ResourceInputFromBuffer(
                        str(exc),
                        details=_cause_details(exc),
                    )
                    serialized_result = b""

        return error.CCError.serialize(err), serialized_result

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

    transfer_wrapper._input_buffer_mode = buffer
    transfer_wrapper._input_payload_binding = input
    transfer_wrapper._output_payload_binding = output
    return transfer_wrapper


def auto_transfer(func=None, *, input=None, output=None, buffer=None):
    """Wrap a CRM method with explicit FastDB, pickle, or no-payload bindings."""

    if buffer == "hold":
        raise ValueError(
            "server-side scoped input is controlled by "
            "cc.register(..., input_lifetime=...), not buffer='hold'",
        )
    if buffer is not None and buffer not in _VALID_TRANSFER_BUFFERS:
        raise ValueError(
            f"buffer must be None or one of {sorted(_VALID_TRANSFER_BUFFERS)}, "
            f"got {buffer!r}",
        )

    def create_wrapper(target):
        parameters = _rpc_parameters(target)
        hints = _resolved_hints(target)
        payload_type = _payload_type()

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
            if any(hints.get(parameter.name) is payload_type for parameter in parameters):
                raise TypeError(
                    f"{target.__name__} uses fastdb4py.payload.Payload input "
                    "without @cc.transfer(input=...).",
                )
            input_binding = python_pickle_input_binding(target)

        return_annotation = hints.get("return", inspect.Signature.empty)
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
        elif return_annotation in (None, type(None), inspect.Signature.empty):
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
            buffer=buffer or "view",
        )

    if func is None:
        return create_wrapper
    if not callable(func):
        raise TypeError("@auto_transfer requires a callable or parentheses.")
    return create_wrapper(func)


def _rpc_parameters(func: Callable[..., object]) -> list[inspect.Parameter]:
    try:
        parameters = list(inspect.signature(func).parameters.values())
    except (TypeError, ValueError):
        return []
    if parameters and parameters[0].name in {"self", "cls"}:
        parameters = parameters[1:]
    return parameters


def _resolved_hints(func: Callable[..., object]) -> dict[str, Any]:
    try:
        return get_type_hints(func, include_extras=True)
    except (NameError, TypeError, ValueError):
        return dict(getattr(func, "__annotations__", {}))


def _payload_type():
    from fastdb4py.payload import Payload

    return Payload


def _release_response(response: object) -> None:
    release = getattr(response, "release", None)
    if callable(release):
        release()


def _release_view_and_response(view: memoryview, response: object) -> None:
    first_error: BaseException | None = None
    try:
        view.release()
    except BaseException as exc:
        first_error = exc
    try:
        _release_response(response)
    except BaseException as exc:
        if first_error is None:
            first_error = exc
    if first_error is not None:
        raise first_error


def _invalidate_and_release_held_response(
    view: memoryview,
    response: object,
    invalidator: Callable[[object], None],
    value: object,
) -> None:
    first_error: BaseException | None = None
    try:
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
        try:
            invalidator(value)
        except BaseException as exc:
            if first_error is None:
                first_error = exc
        try:
            _release_response(response)
        except BaseException as exc:
            if first_error is None:
                first_error = exc

    if first_error is not None:
        raise first_error


def _cleanup_preserving_primary(
    primary: BaseException,
    cleanup: Callable[[], None],
) -> None:
    try:
        cleanup()
    except BaseException as cleanup_error:
        add_note = getattr(primary, "add_note", None)
        if callable(add_note):
            add_note(
                "C-Two cleanup also failed with "
                f"{type(cleanup_error).__name__}: {cleanup_error}",
            )


def _cause_details(exc: BaseException) -> dict[str, str]:
    from fastdb4py.payload import PayloadError

    if not isinstance(exc, PayloadError):
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
            fields[target] = str(value)
    if fields:
        fields["cause_owner"] = "fastdb"
    return fields
