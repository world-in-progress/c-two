from __future__ import annotations

import copy
from collections.abc import Mapping
from enum import IntEnum, unique
from typing import Literal

from c_two import _native

ErrorSide = Literal['client', 'resource']
ErrorStage = Literal['serialize', 'deserialize', 'from_buffer', 'execute', 'call']
TransportPhase = Literal['pre_dispatch', 'dispatch_uncertain']

_PHASE_VALUES = ('pre_dispatch', 'dispatch_uncertain')
_CALLBACK_DETAIL_KEYS = ('transport_phase', 'dispatch_phase', 'fallback_eligible', 'route_withdrawal')
_DEFAULT_MESSAGE = 'Error occurred when using C-Two.'
_UNSET = object()


def _validated_phase(value: object) -> TransportPhase | None:
    """Accept only the two canonical Core dispatch phases (or ``None``)."""
    if value is None or value in _PHASE_VALUES:
        return value  # type: ignore[return-value]
    raise ValueError(
        f'transport_phase must be one of {_PHASE_VALUES} or None, got {value!r}',
    )


def _phase_from_details(details: Mapping[str, str]) -> TransportPhase | None:
    """Adopt a phase only from an exact canonical details entry."""
    key = 'transport_phase' if 'transport_phase' in details else 'dispatch_phase'
    value = details.get(key)
    if value in _PHASE_VALUES:
        return value  # type: ignore[return-value]
    return None


def _load_error_code_members() -> (
    tuple[dict[str, int], dict[int, tuple[ErrorSide | None, ErrorStage | None]]]
):
    registry = _native.error_registry()
    members: dict[str, int] = {}
    meta: dict[int, tuple[ErrorSide | None, ErrorStage | None]] = {}
    for name, (code, side, stage) in registry.items():
        code = int(code)
        members[name] = code
        meta[code] = (side, stage)
    return members, meta


_ERROR_CODE_MEMBERS, _CODE_META = _load_error_code_members()

#: Standard error codes, generated from the native Rust registry so Python
#: and Rust share one numeric identity and member naming.
ErrorCode = unique(IntEnum('ErrorCode', _ERROR_CODE_MEMBERS))


class CCBaseError(Exception):
    """Base class for all C-Two-related errors."""

    @property
    def transport_phase(self) -> TransportPhase | None:
        """Dispatch phase established for this error, when Core knows one.

        Read-only; only ``'pre_dispatch'``, ``'dispatch_uncertain'`` and
        ``None`` are valid. Never inferred from arbitrary message text.
        """
        return getattr(self, '_transport_phase', None)

    def __reduce__(self):
        state = self.__dict__.copy()
        if isinstance(state.get('details'), Mapping):
            state['details'] = dict(state['details'])
        if isinstance(state.get('__notes__'), list):
            state['__notes__'] = list(state['__notes__'])
        slots: dict[str, object] = {}
        for base in type(self).__mro__:
            raw_slots = base.__dict__.get('__slots__', ())
            slot_names = (raw_slots,) if isinstance(raw_slots, str) else raw_slots
            for name in slot_names:
                if name not in ('__dict__', '__weakref__') and name not in slots and hasattr(self, name):
                    val = getattr(self, name)
                    if name == '__notes__' and isinstance(val, list):
                        val = list(val)
                    slots[name] = val
        return _rebuild_error, (type(self), self.args), (state, slots)

    def __setstate__(self, state):
        attrs, slots = state
        self.__dict__.update(attrs)
        for name, value in slots.items():
            setattr(self, name, value)


def _rebuild_error(target: type[CCBaseError], args: tuple) -> CCBaseError:
    """Pickle/copy reconstruction preserving the concrete exception class."""
    inst = Exception.__new__(target)
    inst.args = args
    return inst


class CoreError(CCBaseError, RuntimeError):
    """Structured failure populated by the native Core runtime.

    ``message`` retains the outer Core diagnostic. When the failure carries a
    canonical semantic error, the public fields expose its diagnostics and
    ``semantic`` projects a public exception from a separate immutable snapshot.
    Fields without a native value remain None.
    """

    category: Literal['semantic', 'contract', 'admission', 'transport', 'lifecycle', 'protocol_version_mismatch']
    message: str
    code: int | None
    name: str | None
    details: dict[str, str]
    semantic_message: str | None
    _semantic_snapshot: tuple[int, str, tuple[tuple[str, str], ...], TransportPhase | None] | None
    _transport_phase: TransportPhase | None
    transport_kind: Literal['ipc', 'http'] | None
    # Semantic errors preserve the peer's arbitrary diagnostic detail.
    fallback_eligible: bool | None
    route_withdrawal: bool | None
    io_operation: Literal['bind', 'connect'] | None
    io_kind: Literal[
        'permission_denied', 'not_found', 'connection_refused', 'address_in_use', 'other',
    ] | None
    raw_os_error: int | None
    endpoint_unverified_reason: Literal[
        'unsafe-directory', 'symlink', 'unexpected-object', 'foreign-owner',
        'missing-ownership', 'invalid-ownership', 'invalid-record',
        'record-mismatch', 'coordinator-missing', 'coordinator-replaced',
        'initialization-incomplete', 'unsupported-namespace-format',
    ] | None
    # Only protocol_version_mismatch supplies these nonzero u32 values.
    local_protocol_version: int | None
    peer_protocol_version: int | None
    lifecycle_kind: Literal[
        'invalid_server_id', 'config_frozen',
        'duplicate_route', 'missing_route', 'missing_relay_address',
        'register_failure',
        'held_response_release', 'response_copy',
        'configuration', 'server', 'startup_timeout', 'relay',
        'runtime_retired', 'shutdown_wait_timeout',
    ] | None
    startup_stage: Literal['readiness', 'responsiveness'] | None
    timeout_seconds: float | None
    route_name: str | None
    status_code: int | None
    relay_duplicate: bool | None
    failure_source: Literal[
        'relay_projection', 'relay_prepare', 'commit', 'relay_register', 'route_open',
    ] | None
    registration_failure: dict[str, object] | None
    rollback: dict[str, object] | None
    relay_cleanup_error: dict[str, object] | None

    def __init__(
        self,
        category: Literal['semantic', 'contract', 'admission', 'transport', 'lifecycle', 'protocol_version_mismatch'],
        message: str,
    ):
        super().__init__(message)
        self.category = category
        self.message = message
        self.code = None
        self.name = None
        self.details = {}
        self.semantic_message = None
        self._semantic_snapshot = None
        self._transport_phase = None
        self.transport_kind = None
        self.fallback_eligible = None
        self.route_withdrawal = None
        self.io_operation = None
        self.io_kind = None
        self.raw_os_error = None
        self.endpoint_unverified_reason = None
        self.local_protocol_version = None
        self.peer_protocol_version = None
        self.lifecycle_kind = None
        self.startup_stage = None
        self.timeout_seconds = None
        self.route_name = None
        self.status_code = None
        self.relay_duplicate = None
        self.failure_source = None
        self.registration_failure = None
        self.rollback = None
        self.relay_cleanup_error = None

    @property
    def semantic(self) -> CCError | None:
        """Independent projection of the canonical semantic error, if any.

        Each call constructs a fresh exception from the immutable snapshot
        (code, semantic message, details and this error's dispatch phase).
        The ``CoreError`` keeps no reference to the projected exception.
        """
        snapshot = self._semantic_snapshot
        if not isinstance(snapshot, tuple) or len(snapshot) != 4:
            return None
        code, message, details, phase = snapshot
        return CCError._from_parts(
            int(code),
            message,
            dict(details),
            transport_phase=phase,
        )


def _build_error(
    target: type[CCError],
    code: ErrorCode,
    message: str,
    details: dict[str, str],
    phase: TransportPhase | None,
) -> CCError:
    """Construct one exception instance without running ``__init__``."""
    error = Exception.__new__(target)
    error.code = code
    error.message = message
    error.details = details
    error._transport_phase = phase
    error.args = (code, message)
    return error


def _checked_error_code(code: ErrorCode | int) -> ErrorCode:
    if not isinstance(code, ErrorCode) and type(code) is not int:
        raise TypeError(f'code must be an ErrorCode or int, got {type(code).__name__}')
    return ErrorCode(code)


class CCError(CCBaseError):
    """
    General error class for C-Two.

    Parameters:
        code (ErrorCode): The error code representing the type of error.
        message (str | None): Optional custom error message; an explicit
            empty string is kept, the default ``None`` uses the generic text.
        details (Mapping[str, str] | None): Optional machine-readable
            diagnostic context.
        transport_phase (TransportPhase | None): The Core-established
            dispatch phase. When not passed, it is adopted only from an exact
            canonical ``transport_phase``/``dispatch_phase`` details entry;
            later edits to ``details`` never change ``transport_phase``.
    """

    code: ErrorCode
    message: str
    details: dict[str, str]

    def __init__(
        self,
        code: ErrorCode | int = ErrorCode.Unknown,
        message: str | None = None,
        details: Mapping[str, str] | None = None,
        *,
        transport_phase: TransportPhase | None | object = _UNSET,
    ):
        resolved_code = _checked_error_code(code)
        resolved_message = _DEFAULT_MESSAGE if message is None else message
        resolved_details = dict(details or {})
        resolved_phase = (
            _validated_phase(_phase_from_details(resolved_details))
            if transport_phase is _UNSET
            else _validated_phase(transport_phase)
        )
        self.code = resolved_code
        self.message = resolved_message
        self.details = resolved_details
        self._transport_phase = resolved_phase
        self.args = (resolved_code, resolved_message)

    @property
    def side(self) -> ErrorSide | None:
        """Adapter side from the native error registry, when classified."""
        return _CODE_META.get(int(self.code), (None, None))[0]

    @property
    def stage(self) -> ErrorStage | None:
        """Adapter stage from the native error registry, when classified."""
        return _CODE_META.get(int(self.code), (None, None))[1]

    @classmethod
    def _from_parts(
        cls,
        code_int: int,
        message: str | None = None,
        details: Mapping[str, str] | None = None,
        *,
        transport_phase: TransportPhase | None | object = _UNSET,
    ) -> CCError:
        """Build the exception for one canonical semantic snapshot.

        The class is selected by the registry mapping; an unknown code
        degrades to ``ErrorCode.Unknown`` and records the raw code in
        ``details['unknown_code']``.
        """
        details = dict(details or {})
        try:
            code = ErrorCode(int(code_int))
        except ValueError:
            code = ErrorCode.Unknown
            details['unknown_code'] = str(int(code_int))
        if message is None:
            message = _DEFAULT_MESSAGE
        phase = _phase_from_details(details) if transport_phase is _UNSET else transport_phase
        target = _CODE_TO_CLASS.get(int(code), CCError)
        return _build_error(target, code, message, details, _validated_phase(phase))

    def __str__(self):
        return f'{self.code.name}: {self.message}'

    def __repr__(self):
        return f'CCError(code={int(self.code)}, message={self.message}, details={self.details})'


class _FixedCodeError(CCError):
    """A public subclass bound to one registry code and default message."""

    _code: ErrorCode
    _default_message: str

    def __init__(
        self,
        message: str | None = None,
        details: Mapping[str, str] | None = None,
        *,
        transport_phase: TransportPhase | None | object = _UNSET,
    ):
        super().__init__(
            self._code,
            message if message is not None else self._default_message,
            details,
            transport_phase=transport_phase,
        )


class ResourceExecutionError(_FixedCodeError):
    """A resource method, stream generator or resource cleanup raised.

    Fixed to ``ErrorCode.ResourceFunctionExecuting`` (code 3,
    ``side='resource'``, ``stage='execute'``).
    """

    _code = ErrorCode.ResourceFunctionExecuting
    _default_message = 'Error occurred when executing function at resource'


class SerializationError(CCError):
    """Codec or payload buffer-view failure at one serialization stage.

    Accepts only the six codec/buffer codes — 1 ``ResourceInputDeserializing``,
    2 ``ResourceOutputSerializing``, 4 ``ResourceInputFromBuffer``,
    5 ``ClientInputSerializing``, 6 ``ClientOutputDeserializing`` and
    8 ``ClientOutputFromBuffer`` — and rejects any other code with
    ``ValueError``. ``side`` and ``stage`` distinguish the failing stage.
    """

    _CODEC_CODES: frozenset[int] = frozenset({
        int(ErrorCode.ResourceInputDeserializing),
        int(ErrorCode.ResourceOutputSerializing),
        int(ErrorCode.ResourceInputFromBuffer),
        int(ErrorCode.ClientInputSerializing),
        int(ErrorCode.ClientOutputDeserializing),
        int(ErrorCode.ClientOutputFromBuffer),
    })

    def __init__(
        self,
        code: ErrorCode | int = ErrorCode.ClientInputSerializing,
        message: str | None = None,
        details: Mapping[str, str] | None = None,
        *,
        transport_phase: TransportPhase | None | object = _UNSET,
    ):
        code = _checked_error_code(code)
        if code not in self._CODEC_CODES:
            raise ValueError(
                f'SerializationError requires one of the codec codes '
                f'{sorted(self._CODEC_CODES)}, got {int(code)}',
            )
        super().__init__(code, message, details, transport_phase=transport_phase)


class ResourceNotFound(_FixedCodeError):
    """Raised when a named resource cannot be resolved by any relay."""

    _code = ErrorCode.ResourceNotFound
    _default_message = 'Resource not found'


class ResourceUnavailable(_FixedCodeError):
    """Raised when a resource exists but is not reachable."""

    _code = ErrorCode.ResourceUnavailable
    _default_message = 'Resource unavailable'

    def __init__(
        self,
        message: str | None = None,
        detail: str | None = None,
        details: Mapping[str, str] | None = None,
        *,
        transport_phase: TransportPhase | None | object = _UNSET,
    ):
        resolved = message if message is not None else self._default_message
        if detail:
            resolved = f'{resolved}: {detail}'
        super().__init__(resolved, details, transport_phase=transport_phase)


class ResourceAlreadyRegistered(_FixedCodeError):
    """Raised when a relay rejects duplicate registration for a route name."""

    _code = ErrorCode.ResourceAlreadyRegistered
    _default_message = 'Resource already registered'


class ResourceClosed(_FixedCodeError):
    """Raised when a route exists but no longer accepts new calls."""

    _code = ErrorCode.ResourceClosed
    _default_message = 'Resource closed'


class ResourceRemoved(_FixedCodeError):
    """Raised when a previously observed route was removed."""

    _code = ErrorCode.ResourceRemoved
    _default_message = 'Resource removed'


class ContractMismatch(_FixedCodeError):
    """Raised when route contract metadata does not match the expected CRM."""

    _code = ErrorCode.ContractMismatch
    _default_message = 'Contract mismatch'


class IdentityMismatch(_FixedCodeError):
    """Raised when the actual server identity does not match the expected owner."""

    _code = ErrorCode.IdentityMismatch
    _default_message = 'Identity mismatch'


class ProtocolViolation(_FixedCodeError):
    """Raised when a peer sends invalid C-Two protocol data."""

    _code = ErrorCode.ProtocolViolation
    _default_message = 'Protocol violation'


class FallbackDenied(_FixedCodeError):
    """Raised when a fallback route would retry the same failed local path."""

    _code = ErrorCode.FallbackDenied
    _default_message = 'Fallback denied'


class CallDeadlineExceeded(_FixedCodeError):
    """Raised when a bounded call deadline elapsed without a definitive outcome.

    The peer Core reports the observed dispatch phase through
    ``transport_phase`` (``pre_dispatch`` or ``dispatch_uncertain``). This
    class only projects the received detail; it never infers a phase from
    message text and never rewrites received details.
    """

    _code = ErrorCode.CallDeadlineExceeded
    _default_message = 'Call deadline exceeded'


class UnsupportedCallMode(_FixedCodeError):
    """Raised when a call mode cannot honor the requested semantics, such as a
    finite deadline that does not support same-process synchronous dispatch."""

    _code = ErrorCode.UnsupportedCallMode
    _default_message = 'Unsupported call mode'


class CallCapacityExceeded(_FixedCodeError):
    """Raised when a bounded continuation budget is exhausted, such as
    continuation slots or byte capacity."""

    _code = ErrorCode.CallCapacityExceeded
    _default_message = 'Call capacity exceeded'


class CallCancelled(_FixedCodeError):
    """Raised when a server stream's cancellation has completed."""

    _code = ErrorCode.CallCancelled
    _default_message = 'Call cancelled'


class RouteConflict(_FixedCodeError):
    """Raised when multiple online registrations hold one route name."""

    _code = ErrorCode.RouteConflict
    _default_message = 'Route conflict'


class SubscriptionLagged(_FixedCodeError):
    """Raised when a strict topic subscription misses evicted history."""

    _code = ErrorCode.SubscriptionLagged
    _default_message = 'Subscription lagged'


_CODE_TO_CLASS: dict[int, type[CCError]] = {
    int(ErrorCode.ResourceInputDeserializing): SerializationError,
    int(ErrorCode.ResourceOutputSerializing): SerializationError,
    int(ErrorCode.ResourceInputFromBuffer): SerializationError,
    int(ErrorCode.ResourceFunctionExecuting): ResourceExecutionError,
    int(ErrorCode.ClientInputSerializing): SerializationError,
    int(ErrorCode.ClientOutputDeserializing): SerializationError,
    int(ErrorCode.ClientOutputFromBuffer): SerializationError,
    int(ErrorCode.ResourceNotFound): ResourceNotFound,
    int(ErrorCode.ResourceUnavailable): ResourceUnavailable,
    int(ErrorCode.ResourceAlreadyRegistered): ResourceAlreadyRegistered,
    int(ErrorCode.ResourceClosed): ResourceClosed,
    int(ErrorCode.ResourceRemoved): ResourceRemoved,
    int(ErrorCode.ContractMismatch): ContractMismatch,
    int(ErrorCode.IdentityMismatch): IdentityMismatch,
    int(ErrorCode.ProtocolViolation): ProtocolViolation,
    int(ErrorCode.FallbackDenied): FallbackDenied,
    int(ErrorCode.CallDeadlineExceeded): CallDeadlineExceeded,
    int(ErrorCode.UnsupportedCallMode): UnsupportedCallMode,
    int(ErrorCode.CallCapacityExceeded): CallCapacityExceeded,
    int(ErrorCode.CallCancelled): CallCancelled,
    int(ErrorCode.RouteConflict): RouteConflict,
    int(ErrorCode.SubscriptionLagged): SubscriptionLagged,
}


def _strip_callback_phases(details: object) -> dict[str, str]:
    """Drop inner call-control details that never describe the outer call."""
    if not isinstance(details, Mapping):
        return {}
    return {
        key: value
        for key, value in details.items()
        if key not in _CALLBACK_DETAIL_KEYS
    }


def _has_callback_detail_keys(details: object) -> bool:
    if not isinstance(details, Mapping):
        return False
    return any(key in details for key in _CALLBACK_DETAIL_KEYS)


def _strip_snapshot_controls(
    snapshot: tuple[int, str, tuple[tuple[str, str], ...], TransportPhase | None] | None,
) -> tuple[int, str, tuple[tuple[str, str], ...], TransportPhase | None] | None:
    if not isinstance(snapshot, tuple) or len(snapshot) != 4:
        return None
    code, message, entries, _ = snapshot
    cleaned = tuple(
        (key, value)
        for key, value in entries
        if key not in _CALLBACK_DETAIL_KEYS
    )
    return (code, message, cleaned, None)


def _snapshot_has_controls(
    snapshot: tuple[int, str, tuple[tuple[str, str], ...], TransportPhase | None] | None,
) -> bool:
    if not isinstance(snapshot, tuple) or len(snapshot) != 4:
        return False
    _, _, entries, phase = snapshot
    if phase is not None:
        return True
    return any(key in _CALLBACK_DETAIL_KEYS for key, _ in entries)


def _callback_error_copy(exc: CCError) -> CCError:
    """Fresh copy of a ``CCError`` escaping a resource callback.

    Keeps the subclass, code, message and business details; clears inner
    dispatch phases and call-control details without mutating the original.
    """
    stripped_details = _strip_callback_phases(getattr(exc, 'details', {}))
    copied = copy.copy(exc)
    copied.details = stripped_details
    copied._transport_phase = None
    if isinstance(copied.__dict__.get('__notes__'), list):
        copied.__dict__['__notes__'] = list(copied.__dict__['__notes__'])
    return copied


def _callback_core_copy(core: CoreError) -> CoreError:
    """Fresh ``CoreError`` copy for a same-process resource callback re-raise.

    Keeps the structured native facts, drops the outer-inapplicable inner
    dispatch phase, fallback eligibility, route withdrawal and phase details.
    """
    copied = copy.copy(core)
    copied.details = _strip_callback_phases(getattr(core, 'details', {}))
    copied._transport_phase = None
    copied.fallback_eligible = None
    copied.route_withdrawal = None
    copied._semantic_snapshot = _strip_snapshot_controls(core._semantic_snapshot)
    if isinstance(copied.__dict__.get('__notes__'), list):
        copied.__dict__['__notes__'] = list(copied.__dict__['__notes__'])
    return copied


def _post_dispatch_cleanup_error(exc: BaseException) -> BaseException:
    """Strip inner call-control phases from a cleanup failure after dispatch."""
    if isinstance(exc, CoreError):
        if (
            exc.transport_phase is not None
            or exc.fallback_eligible is not None
            or exc.route_withdrawal is not None
            or _has_callback_detail_keys(exc.details)
            or _snapshot_has_controls(exc._semantic_snapshot)
        ):
            return _callback_core_copy(exc)
        return exc
    if isinstance(exc, CCError):
        if exc.transport_phase is not None or _has_callback_detail_keys(exc.details):
            return _callback_error_copy(exc)
        return exc
    if isinstance(exc, CCBaseError):
        details = getattr(exc, 'details', None)
        if exc.transport_phase is not None or _has_callback_detail_keys(details):
            copied = copy.copy(exc)
            if _has_callback_detail_keys(details):
                copied.details = _strip_callback_phases(details)  # type: ignore[attr-defined]
            copied._transport_phase = None  # type: ignore[attr-defined]
            return copied
    return exc


def _callback_semantic_copy(exc: CCBaseError) -> CCError | None:
    """Semantic projection of a callback error, or ``None``.

    A ``CoreError`` projects its semantic snapshot first; a plain ``CCError``
    is copied with inner dispatch phases cleared.
    """
    if isinstance(exc, CoreError):
        projected = exc.semantic
        if projected is not None:
            return _callback_error_copy(projected)
        return None
    if isinstance(exc, CCError):
        return _callback_error_copy(exc)
    return None


def _project_native_error(exc: BaseException) -> CCError | None:
    """The unified native semantic projection shared by every SDK call site.

    A native ``CoreError`` carrying a canonical semantic error projects a
    fresh typed exception (with the Core-established dispatch phase); a
    ``CoreError`` without one, and any other exception, projects ``None`` so
    callers re-raise the original.
    """
    if isinstance(exc, CoreError):
        return exc.semantic
    return None


__all__ = [
    'ErrorCode',
    'CCBaseError',
    'CoreError',
    'CCError',
    'ResourceExecutionError',
    'SerializationError',
    'ResourceNotFound',
    'ResourceUnavailable',
    'ResourceAlreadyRegistered',
    'ResourceClosed',
    'ResourceRemoved',
    'ContractMismatch',
    'IdentityMismatch',
    'ProtocolViolation',
    'FallbackDenied',
    'CallDeadlineExceeded',
    'UnsupportedCallMode',
    'CallCapacityExceeded',
    'CallCancelled',
    'RouteConflict',
    'SubscriptionLagged',
]
