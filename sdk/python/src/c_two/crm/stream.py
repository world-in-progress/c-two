"""Python iteration and codec glue around native server-stream call records."""
from __future__ import annotations

import contextvars
import inspect
from collections.abc import Generator, Iterator
from functools import wraps
from typing import Generic, TypeVar, TypedDict, get_args, get_origin

from .. import error
from .payload_plan import (
    PayloadPlanKind, _DeferredFastDbPlan, _fastdb_input_nbytes,
    _materialize_direct_plan, decode_stage_of,
)

T = TypeVar('T')


class StreamCloseReceipt(TypedDict):
    completed: bool
    error: error.CCBaseError | None
    complete_items: int | None


def stream_item_annotation(annotation):
    origin = get_origin(annotation)
    args = get_args(annotation)
    if origin is Iterator and len(args) == 1:
        return args[0]
    if origin is Generator and len(args) == 3 and args[1:] == (type(None), type(None)):
        return args[0]
    raise TypeError('a @cc.stream method must return Iterator[T] or Generator[T, None, None]')


def _native_call(operation, *args, **kwargs):
    from ..transport.client.proxy import _native_call as call
    return call(operation, *args, **kwargs)


def _resource_input_decode_error(exc: BaseException) -> error.SerializationError:
    code = (
        error.ErrorCode.ResourceInputFromBuffer
        if decode_stage_of(exc) == 'from_buffer'
        else error.ErrorCode.ResourceInputDeserializing
    )
    from .transferable import _cause_details, _safe_error_message
    return error.SerializationError(code, _safe_error_message(exc), details=_cause_details(exc))


def _resource_output_encode_error(exc: BaseException) -> error.SerializationError:
    from .transferable import _cause_details, _safe_error_message
    return error.SerializationError(
        error.ErrorCode.ResourceOutputSerializing,
        _safe_error_message(exc), details=_cause_details(exc),
    )


def _resource_execution_failure(exc: BaseException) -> error.CCError:
    """Project an exception escaping resource stream code for the dispatch.

    A business ``CCError`` (or a ``CoreError`` semantic payload) keeps its
    code, message and details on a fresh instance with inner dispatch phases
    cleared; anything else becomes ``ResourceExecutionError``.
    """
    from .transferable import _cause_details, _safe_error_message
    if issubclass(type(exc), error.CCBaseError):
        clean = error._callback_semantic_copy(exc)  # type: ignore[arg-type]
        if clean is not None:
            return clean
    return error.ResourceExecutionError(_safe_error_message(exc), details=_cause_details(exc))


class Stream(Iterator[T], Generic[T]):
    """Iterate complete items from one native call; close observes its cleanup."""

    @classmethod
    def _from_encoded(cls, native_stream, decoder, *, held=False, tracker=None,
                      route_name='', method_name='', invalidator=None,
                      decoder_classifies=False):
        stream = cls.__new__(cls)
        stream._decoder = decoder
        stream._held = held
        stream._tracker = tracker
        stream._route_name = route_name
        stream._method_name = method_name
        stream._invalidator = invalidator
        stream._decoder_classifies = decoder_classifies
        stream._native = native_stream
        return stream

    @classmethod
    def _from_objects(cls, native_stream, *, held=False):
        return cls._from_encoded(native_stream, None, held=held)

    def __iter__(self) -> Stream[T]:
        return self

    def __next__(self) -> T:
        from .transferable import (
            HeldResult, _cleanup_preserving_primary, _client_output_error,
            _invalidate_and_release_held_response, _is_native_response_owner,
            _release_decoded_response, _release_view_and_response,
        )
        item = _native_call(self._native.next_item)
        # Native returns a presence flag for object streams, since None is a value.
        if self._decoder is None:
            present, value = item
            if not present:
                raise StopIteration
            return HeldResult(value) if self._held else value
        if item is None:
            raise StopIteration
        view = None
        decoding = False
        decoded = False
        checking_deadline = False
        owner_exports = 0
        try:
            native_item = _is_native_response_owner(item)
            direct_native_decode = native_item and hasattr(item, 'export_count')
            if self._held or not direct_native_decode:
                view = memoryview(item)
            exports_before = item.export_count if direct_native_decode else 0
            decoding = True
            value = self._decoder(item if direct_native_decode else view)
            decoding = False
            decoded = True
            if direct_native_decode:
                owner_exports = max(0, item.export_count - exports_before)
            checking_deadline = True
            _native_call(self._native.check_deadline)
            checking_deadline = False
            if self._held:
                if self._tracker is not None and hasattr(item, 'track_retained'):
                    item.track_retained(self._tracker, self._route_name,
                                        self._method_name, 'client_response')
                result = HeldResult(value, buffer=view, invalidate_cb=self._invalidator)
                if native_item:
                    result._native_response = item
                    result._owner_exports = owner_exports
                elif callable(getattr(item, 'release', None)):
                    result._source_buffer = item
                return result
        except BaseException as exc:
            if not checking_deadline:
                _cleanup_preserving_primary(exc, lambda: _native_call(self._native.cancel))
            if decoded and self._invalidator is not None:
                _cleanup_preserving_primary(exc, lambda: _invalidate_and_release_held_response(
                    view, item, self._invalidator, value, owner_exports))
            else:
                _cleanup_preserving_primary(exc, lambda: _release_view_and_response(view, item))
            if checking_deadline:
                raise
            if (
                self._decoder_classifies
                and decoding
                and issubclass(type(exc), error.SerializationError)
                and exc.transport_phase is None  # type: ignore[attr-defined]
            ):
                raise
            raise _client_output_error(exc, decoding=decoding) from exc
        try:
            _release_decoded_response(view, item, self._invalidator, value, owner_exports)
        except BaseException as exc:
            _cleanup_preserving_primary(exc, lambda: _native_call(self._native.cancel))
            raise _client_output_error(exc) from exc
        return value

    def close(self, timeout: float | None = 30.0) -> StreamCloseReceipt:
        receipt = _native_call(self._native.close, timeout)
        failure = receipt['error']
        if failure is not None:
            projected = error._project_native_error(failure)
            if projected is not None:
                # Keep the native CoreError observable as the direct cause.
                projected.__cause__ = failure
                receipt['error'] = projected
        return receipt

    def __enter__(self) -> Stream[T]:
        return self

    def __exit__(self, exc_type, exc, traceback) -> None:
        _native_call(self._native.cancel)

    def __del__(self) -> None:
        native = getattr(self, '_native', None)
        if native is not None:
            native.cancel()


class _ResourceStream:
    """One Context and its iterator, entered serially by native resource steps."""

    def __init__(self, factory, *, output=None, input_binding=None, request=None,
                 release_input=None, detach_input=None, borrowed=False, args=(), input_arity=0, encoded=True):
        self._context = contextvars.copy_context()
        self._iterator = None
        self._factory = factory
        self._output = output
        self._input = input_binding
        self._input_arity = input_arity
        self._request = request
        self._release_input = release_input
        self._detach_input = detach_input
        self._borrowed = borrowed
        self._args = args
        self._encoded = encoded
        try:
            self._context.run(self._start)
        except BaseException as exc:
            from .transferable import _cleanup_preserving_primary
            _cleanup_preserving_primary(exc, lambda: self._context.run(self._cleanup))
            raise

    def _start(self):
        try:
            if self._input is not None and self._input.deserialize is not None:
                value = self._input.deserialize(self._request)
                if self._input_arity == 1:
                    self._args = (value,)
                elif not isinstance(value, tuple) or len(value) != self._input_arity:
                    raise TypeError('stream input does not match the contract argument list')
                else:
                    self._args = value
            if not self._borrowed:
                self._release_request(
                    detach=self._input is not None and self._input.supports_scoped_owner,
                )
        except BaseException as exc:
            raise _resource_input_decode_error(exc) from exc
        try:
            self._iterator = iter(self._factory(*self._args))
        except BaseException as exc:
            raise _resource_execution_failure(exc) from exc
        self._factory = None

    def step(self):
        return self._context.run(self._advance)

    def _advance(self):
        try:
            value = next(self._iterator)
        except StopIteration:
            return False, None
        except BaseException as exc:
            raise _resource_execution_failure(exc) from exc
        try:
            if self._encoded:
                value = self._output.serialize(value)
                if type(value) is not bytes and not (
                    self._output.supports_scoped_owner
                    and isinstance(value, (memoryview, _DeferredFastDbPlan))
                ):
                    raise TypeError('stream codec must produce immutable bytes')
            else:
                value = _materialize_direct_plan(value, self._output)
        except BaseException as exc:
            raise _resource_output_encode_error(exc) from exc
        return True, value

    def _release_request(self, *, detach=False):
        if self._release_input is not None:
            release = self._detach_input if detach and self._detach_input else self._release_input
            self._release_input = None
            self._detach_input = None
            self._request = None
            release()

    def close(self):
        return self._context.run(self._cleanup)

    def _cleanup(self):
        from .transferable import _attach_cleanup_error, _cause_details, _safe_error_message
        failure = None
        iterator = self._iterator
        self._iterator = None
        try:
            if iterator is not None:
                close = getattr(iterator, 'close', None)
                if close is not None:
                    close()
        except BaseException as exc:
            failure = _resource_execution_failure(exc)
        finally:
            del iterator
        try:
            if self._borrowed and self._input is not None:
                for value in self._args:
                    self._input.invalidate(value)
        except BaseException as exc:
            cleanup = error.SerializationError(
                error.ErrorCode.ResourceInputFromBuffer,
                _safe_error_message(exc),
                details=_cause_details(exc),
            )
            if failure is None:
                failure = cleanup
            else:
                _attach_cleanup_error(failure, cleanup)
            del cleanup
        try:
            self._release_request()
        except BaseException as exc:
            if failure is None:
                failure = error.SerializationError(
                    error.ErrorCode.ResourceInputFromBuffer,
                    _safe_error_message(exc),
                    details=_cause_details(exc),
                )
            else:
                _attach_cleanup_error(failure, exc)
        self._args = ()
        self._factory = None
        if failure is not None:
            try:
                raise failure
            finally:
                del failure


def _cancel_unclaimed_stream(native: object, primary: BaseException) -> None:
    from .transferable import _attach_cleanup_error
    try:
        cancel = getattr(native, 'cancel', None)
        if callable(cancel):
            _native_call(cancel)
    except BaseException as cancel_error:
        try:
            _attach_cleanup_error(primary, cancel_error)
        finally:
            cancel_error = None
    finally:
        primary = None


def _build_stream_wrapper(func, input_binding, output_binding):
    signature = inspect.signature(func)
    method_name = func.__name__
    # Binding hooks and the fixed arity are frozen once here; the call path
    # only reads them.
    input_serializer = input_binding.serialize
    output_decoder = output_binding.deserialize
    output_invalidate = output_binding.invalidate
    fastdb_input = input_binding.kind is PayloadPlanKind.FASTDB
    stream_input_arity = len(signature.parameters) - 1
    # A signature whose parameters are all plain positional ones admits an
    # exact-count fast path: binding and default application are no-ops when
    # the caller passed exactly that many positional values after self.
    exact_arg_count = (
        len(signature.parameters)
        if all(
            parameter.kind in (
                inspect.Parameter.POSITIONAL_ONLY,
                inspect.Parameter.POSITIONAL_OR_KEYWORD,
            )
            for parameter in signature.parameters.values()
        )
        else None
    )

    @wraps(func)
    def invoke(self, *args, _c2_buffer=None, _c2_follow_budget=None, **kwargs):
        if self.direction == '<-':
            return _ResourceStream(
                getattr(self.resource, method_name), output=output_binding,
                input_binding=input_binding, request=args[0] if args else None,
                input_arity=stream_input_arity,
                release_input=kwargs.pop('_release_fn', None),
                detach_input=kwargs.pop('_detach_input', None),
                borrowed=kwargs.pop('_c2_input_buffer_mode', None) == 'borrowed',
            )
        if (
            exact_arg_count is not None
            and not kwargs
            and len(args) + 1 == exact_arg_count
        ):
            values = args
        else:
            bound = signature.bind(self, *args, **kwargs)
            bound.apply_defaults()
            if bound.kwargs:
                raise TypeError('stream methods cannot transport keyword-only arguments')
            values = bound.args[1:]
        client = self.client
        if _c2_follow_budget is not None:
            client = client.resolve_for_call(_c2_follow_budget)
        else:
            following = getattr(client, '_following_local', None)
            if following is not None:
                target = following.target
                if target is not None:
                    client = client.snapshot_followed_target(target)
        budget = _c2_follow_budget
        if client.supports_direct_call:
            try:
                direct_values = values
                if fastdb_input:
                    direct_values = (_materialize_direct_plan(values[0], input_binding),)
                native = client.open_direct_stream(method_name, direct_values, output=output_binding)
            except Exception as exc:
                from ..transport.client.proxy import _LocalRouteClosed
                if not issubclass(type(exc), _LocalRouteClosed):
                    raise
                if budget is None:
                    budget = self.client.start_call_budget()
                return invoke(
                    self, *args, _c2_buffer=_c2_buffer,
                    _c2_follow_budget=budget, **kwargs,
                )
            return Stream._from_objects(native, held=_c2_buffer == 'hold')
        from .transferable import _client_input_error, _close_prepared_handle

        prepared = client.begin_stream(method_name)
        primary = None
        dispatched = False
        unclaimed = None
        try:
            try:
                if fastdb_input:
                    try:
                        nbytes = _fastdb_input_nbytes(values[0])
                    except BaseException as exc:
                        raise _client_input_error(exc) from exc
                    if nbytes is not None:
                        prepared.charge_input(nbytes)
                try:
                    data = input_serializer(*values) if input_serializer is not None else b''
                except BaseException as exc:
                    raise _client_input_error(exc) from exc
                dispatched = True
                try:
                    unclaimed = _native_call(prepared.stream, data)
                except error.CCBaseError:
                    raise
                except Exception as exc:
                    if isinstance(data, _DeferredFastDbPlan):
                        raise _client_input_error(exc) from exc
                    raise
                finally:
                    if fastdb_input and isinstance(data, memoryview):
                        data.release()
            except BaseException as exc:
                primary = exc
                raise
            finally:
                try:
                    _close_prepared_handle(prepared, primary, dispatched)
                except BaseException as close_exc:
                    if primary is None:
                        primary = close_exc
                    close_exc = None
                    raise
            try:
                stream_obj = Stream._from_encoded(
                    unclaimed, output_decoder, held=_c2_buffer == 'hold',
                    tracker=client.local_copy_tracker, route_name=client.route_name,
                    method_name=method_name, invalidator=output_invalidate,
                )
            except BaseException as wrap_exc:
                primary = wrap_exc
                clean = error._post_dispatch_cleanup_error(wrap_exc)
                primary = clean
                try:
                    if clean is not wrap_exc:
                        raise clean from wrap_exc
                    raise
                finally:
                    clean = None
                    wrap_exc = None
            unclaimed = None
            return stream_obj
        finally:
            try:
                if unclaimed is not None:
                    if primary is not None:
                        _cancel_unclaimed_stream(unclaimed, primary)
                    else:
                        cancel = getattr(unclaimed, 'cancel', None)
                        if callable(cancel):
                            cancel()
            finally:
                unclaimed = None
                primary = None

    invoke._input_payload_binding = input_binding
    invoke._output_payload_binding = output_binding
    return invoke
