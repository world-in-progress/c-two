"""Registration-scoped publishing and client topic subscriptions (ADR 0005).

The Runtime keeps every route registration alive; a dropped
:class:`Registration` never unregisters it. Publishers bind to one
registration instance and fail with ``ResourceClosed`` (707) once that
registration ended, never writing to a later registration of the same name.
Subscriptions span at most one registration and follow ADR 0004 on a later
``subscribe()`` call.
"""
from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass
from typing import Generic, TypedDict, TypeVar

from .. import error
from ..crm.payload_plan import (
    PayloadBinding,
    PayloadPlanKind,
    _DeferredFastDbPlan,
    decode_stage_of,
)
from ..crm.transferable import (
    HeldResult,
    _cause_details,
    _cleanup_preserving_primary,
    _invalidate_and_release_held_response,
    _is_native_response_owner,
    _release_decoded_response,
    _release_view_and_response,
    _safe_error_message,
)

T = TypeVar('T')


class TopicCloseReceipt(TypedDict):
    """Terminal observation returned by an explicit subscription close."""

    completed: bool
    error: error.CCBaseError | None
    complete_items: int | None


@dataclass(frozen=True, slots=True)
class Delivery(Generic[T]):
    """One immutable delivered frame: the value, its sequence number and the
    count of frames skipped before it."""

    value: T
    seq: int
    dropped: int


def _native_call(operation, *args, **kwargs):
    from .client.proxy import _native_call as call

    return call(operation, *args, **kwargs)


def _publish_encode_error(exc: BaseException) -> error.SerializationError:
    return error.SerializationError(
        error.ErrorCode.ResourceOutputSerializing,
        _safe_error_message(exc),
        details=_cause_details(exc),
        transport_phase='pre_dispatch',
    )


def _delivery_decode_error(exc: BaseException, *, decoding: bool) -> error.SerializationError:
    code = error.ErrorCode.ClientOutputFromBuffer
    if decoding and decode_stage_of(exc) != 'from_buffer':
        code = error.ErrorCode.ClientOutputDeserializing
    return error.SerializationError(
        code, _safe_error_message(exc), details=_cause_details(exc), transport_phase=None,
    )


class Publisher(Generic[T]):
    """Registration-scoped handle publishing immutable frames to one topic.

    ``publish`` serializes the value exactly once with the topic's output
    binding (so same-process deliveries see an immutable snapshot of mutable
    Python objects) and hands the bytes to the native publisher, which never
    blocks on slow subscribers. After the registration ends it reports
    ``ResourceClosed`` (707) and never writes to a later registration.
    """

    __slots__ = ('_native', '_binding')

    def __init__(self, native, binding: PayloadBinding) -> None:
        self._native = native
        self._binding = binding

    def publish(self, value: T) -> int:
        """Publish one value; returns its monotonic registration-scoped seq."""
        payload = self._encode(value)
        try:
            return _native_call(self._native.publish, payload)
        except error.CCBaseError:
            raise
        except Exception as exc:
            raise _publish_encode_error(exc) from exc
        finally:
            if self._binding.supports_scoped_owner and isinstance(payload, memoryview):
                payload.release()

    def _encode(self, value: T):
        try:
            return self._binding.serialize(value)
        except BaseException as exc:
            raise _publish_encode_error(exc) from exc

    @property
    def topic_name(self) -> str:
        return self._native.topic_name

    @property
    def is_closed(self) -> bool:
        return self._native.is_closed

    @property
    def head_seq(self) -> int:
        return self._native.head_seq


class Subscription(Generic[T]):
    """One live client attachment to a route's topic.

    Iterating yields :class:`Delivery` records (or ``Delivery[HeldResult[T]]``
    when created with ``hold=True``, where ``.value`` owns the retained local
    copy). The subscription spans at most one registration: unregistering the
    route or stopping the host ends it with ``CallCancelled`` (718) naming the
    cause, a lagging ``error`` policy ends it with ``SubscriptionLagged``
    (720), and a later ``subscribe()`` follows the current registration.
    """

    __slots__ = (
        '_native', '_binding', '_held', '_local', '_tracker',
        '_route_name',
    )

    def __init__(
        self,
        native,
        binding: PayloadBinding,
        *,
        held: bool = False,
        tracker=None,
        route_name: str = '',
        local: bool = False,
    ) -> None:
        self._native = native
        self._binding = binding
        self._held = held
        self._local = local
        self._tracker = tracker
        self._route_name = route_name

    @property
    def topic_name(self) -> str:
        return self._native.topic_name

    @property
    def start_seq(self) -> int:
        return self._native.start_seq

    @property
    def route_name(self) -> str:
        return self._route_name

    def __iter__(self) -> Subscription[T]:
        return self

    def __next__(self) -> Delivery[T] | Delivery[HeldResult[T]]:
        try:
            outcome = _native_call(self._native.next_delivery)
        except error.CCBaseError as exc:
            if self._is_consumer_cancel(exc):
                raise StopIteration from exc
            raise
        seq, dropped, payload = outcome
        return self._decode_delivery(seq, dropped, payload)

    def _is_consumer_cancel(self, exc: BaseException) -> bool:
        return (
            isinstance(exc, error.CallCancelled)
            and exc.details.get('cause') == 'consumer_cancelled'
        )

    def _decode_delivery(self, seq: int, dropped: int, payload):
        if self._local:
            # The frame was already copied out of the shared ring into
            # Python-owned bytes; decoding it yields a fresh object per
            # delivery, so readers can never observe each other's mutations.
            try:
                value = self._binding.deserialize(payload)
            except BaseException as exc:
                raise _delivery_decode_error(exc, decoding=True) from exc
            if self._held:
                invalidator = (
                    self._binding.invalidate
                    if self._binding.kind is PayloadPlanKind.FASTDB
                    else None
                )
                return Delivery(HeldResult(value, invalidate_cb=invalidator), seq, dropped)
            return Delivery(value, seq, dropped)
        return self._decode_remote_delivery(seq, dropped, payload)

    def _decode_remote_delivery(self, seq: int, dropped: int, payload):
        view = None
        value = None
        decoded = False
        checking_deadline = False
        decoding = False
        owner_exports = 0
        invalidator = (
            self._binding.invalidate
            if self._binding.kind is PayloadPlanKind.FASTDB
            else None
        )
        try:
            native_payload = _is_native_response_owner(payload)
            direct_native_decode = native_payload and hasattr(payload, 'export_count')
            if self._held or not direct_native_decode:
                view = memoryview(payload)
            exports_before = payload.export_count if direct_native_decode else 0
            decoding = True
            value = self._binding.deserialize(payload if direct_native_decode else view)
            decoding = False
            decoded = True
            if direct_native_decode:
                owner_exports = max(0, payload.export_count - exports_before)
            checking_deadline = True
            _native_call(self._native.check_deadline)
            checking_deadline = False
            if self._held:
                if self._tracker is not None and hasattr(payload, 'track_retained'):
                    payload.track_retained(
                        self._tracker, self._route_name,
                        self._native.topic_name, 'client_response',
                    )
                held = HeldResult(value, buffer=view, invalidate_cb=invalidator)
                if native_payload:
                    held._native_response = payload
                    held._owner_exports = owner_exports
                return Delivery(held, seq, dropped)
        except BaseException as exc:
            if decoded and invalidator is not None:
                _cleanup_preserving_primary(exc, lambda: _invalidate_and_release_held_response(
                    view, payload, invalidator, value, owner_exports))
            else:
                _cleanup_preserving_primary(exc, lambda: _release_view_and_response(view, payload))
            if checking_deadline:
                value = None
                raise
            raise _delivery_decode_error(exc, decoding=decoding) from exc
        try:
            _release_decoded_response(view, payload, invalidator, value, owner_exports)
        except BaseException as exc:
            raise _delivery_decode_error(exc, decoding=False) from exc
        return Delivery(value, seq, dropped)

    def cancel(self) -> None:
        """Submit consumer cancellation without blocking."""
        _native_call(self._native.cancel)

    def close(self, timeout: float | None = 30.0) -> TopicCloseReceipt:
        """Cancel and observe the real terminal outcome within *timeout*."""
        if self._local:
            receipt = dict(_native_call(self._native.close))
        else:
            receipt = dict(_native_call(self._native.close, timeout))
        failure = receipt.get('error')
        if failure is not None:
            projected = error._project_native_error(failure)
            if projected is not None:
                # Keep the native CoreError observable as the direct cause.
                projected.__cause__ = failure
                receipt['error'] = projected
        return receipt

    def __enter__(self) -> Subscription[T]:
        return self

    def __exit__(self, exc_type, exc, traceback) -> None:
        self.close()

    def __del__(self, _native_cancel=lambda native: native.cancel()) -> None:
        native = getattr(self, '_native', None)
        if native is not None:
            try:
                _native_cancel(native)
            except BaseException:
                pass


class Registration:
    """One live route registration, kept alive by its Runtime.

    Dropping or garbage-collecting this object never unregisters the route;
    the Runtime owns the registration's lifetime. The publisher handles it
    hands out are bound to this registration instance.
    """

    __slots__ = ('_native', '_topics')

    def __init__(self, native, topics: Mapping[str, PayloadBinding]) -> None:
        self._native = native
        self._topics = dict(topics)

    @property
    def name(self) -> str:
        return self._native.route_name

    @property
    def is_closed(self) -> bool:
        return self._native.is_closed

    @property
    def active_subscribers(self) -> int:
        """Live topic subscribers currently attached to this registration."""
        return self._native.active_subscribers

    def publisher(self, topic: str) -> Publisher:
        """Obtain the registration-scoped publisher for a declared topic."""
        binding = self._topics.get(topic)
        if binding is None:
            raise error.ProtocolViolation(
                f'topic {topic!r} is not declared on the contract '
                f'of route {self.name!r}',
            )
        native = _native_call(self._native.topic_publisher, topic)
        return Publisher(native, binding)


def encode_topic_initial_value(binding: PayloadBinding, value) -> bytes:
    """Serialize one initial topic frame with the topic's publish codec."""
    try:
        payload = binding.serialize(value)
        if isinstance(payload, _DeferredFastDbPlan):
            return payload.to_bytes()
        if isinstance(payload, memoryview) and binding.supports_scoped_owner:
            with payload:
                return bytes(payload)
        if not isinstance(payload, (bytes, bytearray, memoryview)):
            raise TypeError(
                f'topic payload serializer returned {type(payload).__name__}, expected bytes-like data',
            )
        return bytes(payload)
    except BaseException as exc:
        raise _publish_encode_error(exc) from exc


__all__ = [
    'Delivery',
    'Publisher',
    'Registration',
    'Subscription',
    'TopicCloseReceipt',
]
