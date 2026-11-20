from __future__ import annotations

import builtins
from collections.abc import Mapping
from enum import IntEnum, unique
from pathlib import PosixPath, PurePosixPath, PureWindowsPath, WindowsPath
from types import GetSetDescriptorType, MemberDescriptorType
from typing import Literal

from c_two import _native

_BASE_EXCEPTION_GROUP: type[BaseException] | None = getattr(builtins, 'BaseExceptionGroup', None)

ErrorSide = Literal['client', 'resource']
ErrorStage = Literal['serialize', 'deserialize', 'from_buffer', 'execute', 'call']
TransportPhase = Literal['pre_dispatch', 'dispatch_uncertain']

_PHASE_VALUES = ('pre_dispatch', 'dispatch_uncertain')
_CALLBACK_DETAIL_KEYS = ('transport_phase', 'dispatch_phase', 'fallback_eligible', 'route_withdrawal')
_SKIPPED_EXCEPTION_DESCRIPTORS = frozenset({
    '__dict__', '__weakref__', '__notes__', 'args', '__traceback__', '__context__', '__cause__', '__suppress_context__',
})
_BASE_EXCEPTION_DICT = vars(BaseException)['__dict__']
_DEFAULT_MESSAGE = 'Error occurred when using C-Two.'
_UNSET = object()


def _raw_instance_dict(exc: BaseException) -> dict[str, object]:
    try:
        raw = _BASE_EXCEPTION_DICT.__get__(exc, type(exc))  # type: ignore[union-attr]
    except BaseException:
        return {}
    return raw if issubclass(type(raw), dict) else {}


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
        return _raw_get_field(self, '_transport_phase')  # type: ignore[return-value]

    def __reduce__(self):
        cls = type(self)
        state = _raw_instance_dict(self).copy()
        if 'details' in state and isinstance(state['details'], Mapping):
            state['details'] = dict(state['details'])
        if '__notes__' in state and isinstance(state['__notes__'], list):
            state['__notes__'] = state['__notes__'].copy()
        slots = _extract_oserror_slots(self) if issubclass(cls, OSError) else {}
        for base in cls.__mro__:
            if base is object:
                break
            if base is OSError:
                # OSError C member pointers (NULL vs explicit None) are captured
                # above; still collect OSError getset descriptors such as characters_written.
                for name, member in vars(base).items():
                    if name in _SKIPPED_EXCEPTION_DESCRIPTORS or name in slots:
                        continue
                    if type(member) is GetSetDescriptorType:
                        try:
                            slot_val = member.__get__(self, cls)
                        except (AttributeError, ValueError):
                            continue
                        if slot_val is not None:
                            slots[name] = slot_val
                continue
            is_builtin = base.__module__ == 'builtins'
            for name, member in vars(base).items():
                if name in _SKIPPED_EXCEPTION_DESCRIPTORS or name in slots:
                    continue
                if type(member) is MemberDescriptorType or (
                    is_builtin and type(member) is GetSetDescriptorType
                ):
                    try:
                        slot_val = member.__get__(self, cls)
                    except (AttributeError, ValueError):
                        continue
                    if is_builtin and slot_val is None:
                        continue
                    if name == 'details' and isinstance(slot_val, Mapping):
                        slot_val = dict(slot_val)
                    slots[name] = slot_val
        raw_args = BaseException.args.__get__(self, cls)
        if _BASE_EXCEPTION_GROUP is not None and issubclass(cls, _BASE_EXCEPTION_GROUP):
            group_init = (
                _BASE_EXCEPTION_GROUP.message.__get__(self, cls),  # type: ignore[attr-defined]
                _BASE_EXCEPTION_GROUP.exceptions.__get__(self, cls),  # type: ignore[attr-defined]
            )
            return _rebuild_error, (cls, raw_args, group_init), (state, slots)
        return _rebuild_error, (cls, raw_args), (state, slots)

    def __setstate__(self, state):
        attributes, slots = state
        _raw_instance_dict(self).update(attributes)
        cls = type(self)
        for name, value in slots.items():
            descr = _raw_data_descriptor(cls, name)
            if descr is not None:
                try:
                    descr.__set__(self, value)
                    continue
                except (AttributeError, TypeError, ValueError):
                    continue
            try:
                setattr(self, name, value)
            except (AttributeError, TypeError, ValueError):
                continue


def _solid_exception_base(target: type[BaseException]) -> type[BaseException]:
    best: type[BaseException] = BaseException
    for base in target.__mro__:
        if base.__module__ == 'builtins' and issubclass(base, BaseException):
            if base.__basicsize__ > best.__basicsize__ or best is BaseException:
                best = base
    return best


def _rebuild_error(
    target: type[CCBaseError],
    args: tuple,
    group_init: tuple[object, object] | None = None,
) -> CCBaseError:
    solid = _solid_exception_base(target)
    if (
        group_init is not None
        and _BASE_EXCEPTION_GROUP is not None
        and issubclass(target, _BASE_EXCEPTION_GROUP)
    ):
        inst = _BASE_EXCEPTION_GROUP.__new__(target, *group_init)
    else:
        try:
            inst = solid.__new__(target)
        except TypeError:
            inst = solid.__new__(target, *args)
    BaseException.args.__set__(inst, args)
    return inst  # type: ignore[return-value]


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
        return _project_core_semantic_snapshot(self)


def _build_error(
    target: type[CCError],
    code: ErrorCode,
    message: str,
    details: dict[str, str],
    phase: TransportPhase | None,
) -> CCError:
    """Construct one exception instance without running ``__init__``."""
    error = Exception.__new__(target)
    _raw_instance_dict(error)['_cc_canonical_state'] = (code, message, dict(details))
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
        if transport_phase is _UNSET:
            transport_phase = _phase_from_details(resolved_details)
        resolved_phase = _validated_phase(transport_phase)
        _raw_instance_dict(self)['_cc_canonical_state'] = (
            resolved_code,
            resolved_message,
            dict(resolved_details),
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
        if transport_phase is _UNSET:
            phase = _phase_from_details(details)
        else:
            phase = transport_phase
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
}


def _strip_callback_phases(details: Mapping[str, str]) -> dict[str, str]:
    """Drop control details belonging to the inner call."""
    return {
        key: value
        for key, value in details.items()
        if key not in _CALLBACK_DETAIL_KEYS
    }


def _is_same_type_in(candidate: type, types: tuple[type, ...]) -> bool:
    for item in types:
        if candidate is item:
            return True
    return False


def _is_mapping_without_instance_hooks(value: object) -> bool:
    cls = type(value)
    if issubclass(cls, dict):
        return True
    try:
        return issubclass(cls, Mapping)
    except BaseException:
        return False


def _raw_data_descriptor(cls: type, name: str) -> MemberDescriptorType | GetSetDescriptorType | None:
    for base in cls.__mro__:
        if base is object:
            break
        member = vars(base).get(name)
        if type(member) is MemberDescriptorType:
            return member
        if base.__module__ == 'builtins' and type(member) is GetSetDescriptorType:
            return member
    return None


def _raw_slot_descriptor(cls: type, name: str) -> MemberDescriptorType | None:
    for base in cls.__mro__:
        if base is object:
            break
        member = vars(base).get(name)
        if type(member) is MemberDescriptorType:
            return member
    return None


def _raw_get_field(exc: BaseException, name: str, default: object = None) -> object:
    slot = _raw_slot_descriptor(type(exc), name)
    if slot is not None:
        try:
            return slot.__get__(exc, type(exc))
        except AttributeError:
            return default
    return _raw_instance_dict(exc).get(name, default)


def _raw_set_field(exc: BaseException, name: str, value: object) -> None:
    slot = _raw_slot_descriptor(type(exc), name)
    inst_dict = _raw_instance_dict(exc)
    if slot is not None:
        slot.__set__(exc, value)
        inst_dict.pop(name, None)
        return
    inst_dict[name] = value


def _clear_copied_phase_fields(copied: CCBaseError) -> None:
    _raw_set_field(copied, '_transport_phase', None)
    public_slot = _raw_slot_descriptor(type(copied), 'transport_phase')
    if public_slot is not None:
        public_slot.__set__(copied, None)
    _raw_instance_dict(copied).pop('transport_phase', None)


def _details_need_cleanup(raw_details: object) -> bool:
    cls = type(raw_details)
    if issubclass(cls, dict):
        if cls is not dict:
            return True
        return any(dict.__contains__(raw_details, key) for key in _CALLBACK_DETAIL_KEYS)  # type: ignore[arg-type]
    return _is_mapping_without_instance_hooks(raw_details)


_OSERROR_MEMBER_NAMES: tuple[str, ...] = (
    ('errno', 'strerror', 'filename', 'filename2', 'winerror')
    if hasattr(OSError, 'winerror')
    else ('errno', 'strerror', 'filename', 'filename2')
)
_SAFE_OSERROR_SCALAR_TYPES: tuple[type, ...] = (
    int,
    float,
    bool,
    str,
    bytes,
    bytearray,
    type(None),
    ErrorCode,
    PurePosixPath,
    PureWindowsPath,
    PosixPath,
    WindowsPath,
)
_SAFE_OSERROR_CONTAINER_TYPES: tuple[type, ...] = (tuple, list, set, frozenset)


def _is_safe_oserror_value(val: object, depth: int = 0) -> bool:
    cls = type(val)
    if _is_same_type_in(cls, _SAFE_OSERROR_SCALAR_TYPES):
        return True
    if depth >= 4:
        return False
    if _is_same_type_in(cls, _SAFE_OSERROR_CONTAINER_TYPES):
        return all(_is_safe_oserror_value(item, depth + 1) for item in val)  # type: ignore[attr-defined]
    if cls is dict:
        return all(
            _is_safe_oserror_value(k, depth + 1) and _is_safe_oserror_value(v, depth + 1)
            for k, v in dict.items(val)  # type: ignore[arg-type]
        )
    return False


def _extract_oserror_slots(exc: OSError) -> dict[str, object]:
    """Preserve C ``PyOSErrorObject`` ``NULL`` vs explicit ``None`` fields."""
    cls = type(exc)
    vals: dict[str, object] = {}
    for field in _OSERROR_MEMBER_NAMES:
        try:
            vals[field] = getattr(OSError, field).__get__(exc, cls)
        except AttributeError:
            continue
    out = {field: val for field, val in vals.items() if val is not None}
    if not any(val is None for val in vals.values()):
        return out
    raw_args = BaseException.args.__get__(exc, cls)
    if len(raw_args) == 2:
        try:
            reduced = OSError.__reduce__(exc)
        except BaseException:
            reduced = ()
        if type(reduced) is tuple and len(reduced) >= 2 and type(reduced[1]) is tuple:
            rargs = reduced[1]
            if len(rargs) >= 3:
                out['filename'] = rargs[2]
                if len(rargs) >= 5:
                    out['filename2'] = rargs[4]
    if not all(_is_safe_oserror_value(val) for val in vals.values()):
        return out
    args_are_safe = all(_is_safe_oserror_value(arg) for arg in raw_args)
    if 'filename' not in out and not args_are_safe:
        return out
    safe_args = raw_args if args_are_safe else ()
    try:
        actual_str = OSError.__str__(exc)
    except BaseException:
        return out
    core_fields = [f for f in ('errno', 'strerror', 'filename', 'filename2') if f in vals]
    for mask in range(1 << len(core_fields)):
        candidate = dict(out)
        for idx, field in enumerate(core_fields):
            if vals[field] is None and (mask & (1 << idx)):
                candidate[field] = None
        probe = OSError.__new__(OSError)
        BaseException.args.__set__(probe, safe_args)
        for key, val in candidate.items():
            try:
                getattr(OSError, key).__set__(probe, val)
            except (AttributeError, TypeError, ValueError):
                continue
        try:
            probe_str = OSError.__str__(probe)
        except BaseException:
            continue
        if probe_str == actual_str:
            return candidate
    return out


def _clone_exception_without_user_hooks(exc: CCBaseError) -> CCBaseError:
    """Allocate a fresh exception copy without invoking subclass hooks.

    User ``__copy__``, ``__reduce__``, ``__getattr__`` and ``__setattr__``
    overrides must never run while Core/SDK sanitizes an error escaping an
    already-dispatched operation, or a subclass hook could substitute a
    ``pre_dispatch`` exception or return ``self``.
    """
    cls = type(exc)
    raw_args = BaseException.args.__get__(exc, cls)
    solid = _solid_exception_base(cls)
    if _BASE_EXCEPTION_GROUP is not None and issubclass(cls, _BASE_EXCEPTION_GROUP):
        grp_msg = _BASE_EXCEPTION_GROUP.message.__get__(exc, cls)  # type: ignore[attr-defined]
        grp_excs = _BASE_EXCEPTION_GROUP.exceptions.__get__(exc, cls)  # type: ignore[attr-defined]
        copied = _BASE_EXCEPTION_GROUP.__new__(cls, grp_msg, grp_excs)
    else:
        try:
            copied = solid.__new__(cls)
        except TypeError:
            copied = solid.__new__(cls, *raw_args)
    BaseException.args.__set__(copied, raw_args)
    src_dict = _raw_instance_dict(exc)
    dst_dict = _raw_instance_dict(copied)
    dst_dict.update(src_dict)
    raw_notes = dst_dict.get('__notes__')
    if type(raw_notes) is list:
        dst_dict['__notes__'] = raw_notes.copy()
    if issubclass(cls, OSError):
        for key, val in _extract_oserror_slots(exc).items():
            try:
                getattr(OSError, key).__set__(copied, val)
            except (AttributeError, TypeError, ValueError):
                continue
    for base in cls.__mro__:
        if base is object:
            break
        if base is OSError:
            for name, member in vars(base).items():
                if name in _SKIPPED_EXCEPTION_DESCRIPTORS:
                    continue
                if type(member) is GetSetDescriptorType:
                    try:
                        slot_val = member.__get__(exc, cls)
                    except (AttributeError, ValueError):
                        continue
                    if slot_val is not None:
                        try:
                            member.__set__(copied, slot_val)
                        except (AttributeError, TypeError, ValueError):
                            continue
            continue
        is_builtin = base.__module__ == 'builtins'
        for name, member in vars(base).items():
            if name in _SKIPPED_EXCEPTION_DESCRIPTORS:
                continue
            if type(member) is MemberDescriptorType or (
                is_builtin and type(member) is GetSetDescriptorType
            ):
                try:
                    slot_val = member.__get__(exc, cls)
                except (AttributeError, ValueError):
                    continue
                if is_builtin and slot_val is None:
                    try:
                        if member.__get__(copied, cls) is None:
                            continue
                    except (AttributeError, ValueError):
                        continue
                try:
                    member.__set__(copied, slot_val)
                except (AttributeError, TypeError, ValueError):
                    continue
    return copied  # type: ignore[return-value]


_STANDARD_ERROR_CLASSES_TUPLE: tuple[type, ...] = tuple(
    dict.fromkeys([
        CCBaseError,
        CoreError,
        CCError,
        _FixedCodeError,
        *_CODE_TO_CLASS.values(),
    ]),
)
_BASE_DATA_CONTROL_NAMES: frozenset[str] = frozenset({
    '_transport_phase',
    'details',
})
_CORE_DATA_CONTROL_NAMES: frozenset[str] = _BASE_DATA_CONTROL_NAMES | frozenset({
    'fallback_eligible',
    'route_withdrawal',
    '_semantic_snapshot',
})


def _extract_canonical_details(exc: CCBaseError) -> tuple[object, bool]:
    raw = _raw_get_field(exc, 'details', _UNSET)
    if raw is not _UNSET:
        return raw, True
    state = _raw_instance_dict(exc).get('_cc_canonical_state')
    if type(state) is tuple and len(state) == 3:
        return state[2], True
    for base in type(exc).__mro__:
        if base is object:
            break
        if _is_same_type_in(base, _STANDARD_ERROR_CLASSES_TUPLE) or base.__module__ == 'builtins':
            continue
        member = vars(base).get('details', _UNSET)
        if member is not _UNSET and type(member) is dict:
            return member, True
    return None, False


def _safe_strip_callback_details(exc: CCBaseError) -> dict[str, str]:
    raw, has_details = _extract_canonical_details(exc)
    if not has_details:
        return {}
    if issubclass(type(raw), dict):
        return {
            key: value
            for key, value in dict.items(raw)  # type: ignore[arg-type]
            if key not in _CALLBACK_DETAIL_KEYS
        }
    if _is_mapping_without_instance_hooks(raw):
        try:
            return _strip_callback_phases(raw)  # type: ignore[arg-type]
        except BaseException:
            return {}
    return {}


def _has_custom_control_descriptor(cls: type, *, check_core_flags: bool = False) -> bool:
    """True when any class in the full MRO overrides control attributes or attribute lookup."""
    data_names = _CORE_DATA_CONTROL_NAMES if check_core_flags else _BASE_DATA_CONTROL_NAMES
    for base in cls.__mro__:
        if base is object:
            break
        if _is_same_type_in(base, _STANDARD_ERROR_CLASSES_TUPLE) or base.__module__ == 'builtins':
            continue
        namespace = vars(base)
        if '__getattribute__' in namespace or '__getattr__' in namespace:
            return True
        if 'transport_phase' in namespace:
            return True
        if check_core_flags and 'semantic' in namespace:
            return True
        for name in data_names:
            if name in namespace and type(namespace[name]) is not MemberDescriptorType:
                return True
    return False


def _has_instance_phase_value(cls: type, exc: BaseException) -> bool:
    public_slot = _raw_slot_descriptor(cls, 'transport_phase')
    return (
        (public_slot is not None and _raw_get_field(exc, 'transport_phase') is not None)
        or 'transport_phase' in _raw_instance_dict(exc)
    )


def _semantic_snapshot_needs_cleanup(core: CoreError) -> bool:
    snapshot = _raw_get_field(core, '_semantic_snapshot')
    if type(snapshot) is not tuple or len(snapshot) != 4:
        return False
    _, _, entries, phase = snapshot
    if phase is not None:
        return True
    if type(entries) is tuple:
        for entry in entries:
            if type(entry) is tuple and len(entry) == 2 and entry[0] in _CALLBACK_DETAIL_KEYS:
                return True
    return False


def _project_core_semantic_snapshot(core: CoreError) -> CCError | None:
    snapshot = _raw_get_field(core, '_semantic_snapshot')
    if type(snapshot) is not tuple or len(snapshot) != 4:
        return None
    code, message, details, phase = snapshot
    if not _is_same_type_in(type(code), (int, ErrorCode)) or not issubclass(type(message), str):
        return None
    try:
        detail_map = dict(details)  # type: ignore[arg-type]
    except BaseException:
        detail_map = {}
    return CCError._from_parts(int(code), str(message), detail_map, transport_phase=phase)  # type: ignore[arg-type]


def _safe_exception_message(exc: BaseException) -> str:
    raw_msg = _raw_get_field(exc, 'message', _UNSET)
    if raw_msg is not _UNSET and issubclass(type(raw_msg), str):
        return str(raw_msg)
    state = _raw_instance_dict(exc).get('_cc_canonical_state')
    if type(state) is tuple and len(state) == 3 and issubclass(type(state[1]), str):
        return str(state[1])
    try:
        return BaseException.__str__(exc)
    except BaseException:
        return _DEFAULT_MESSAGE


def _rebuild_standard_cc_error(
    exc: CCError,
    stripped_details: dict[str, str],
) -> CCError:
    raw_code = _raw_get_field(exc, 'code', _UNSET)
    code: ErrorCode | None = None
    if type(raw_code) is ErrorCode:
        code = raw_code
    elif type(raw_code) is int:
        try:
            code = ErrorCode(raw_code)
        except ValueError:
            code = ErrorCode.Unknown
    if code is None:
        state = _raw_instance_dict(exc).get('_cc_canonical_state')
        if type(state) is tuple and len(state) == 3 and type(state[0]) is ErrorCode:
            code = state[0]
    if code is None:
        for base in type(exc).__mro__:
            if base is object:
                break
            cand = vars(base).get('_code')
            if type(cand) is ErrorCode:
                code = cand
                break
            if type(cand) is int:
                try:
                    code = ErrorCode(cand)
                except ValueError:
                    code = ErrorCode.Unknown
                break
    if code is None:
        code = ErrorCode.Unknown
    message = _safe_exception_message(exc)
    target = _CODE_TO_CLASS.get(int(code), CCError)
    rebuilt = _build_error(target, code, message, stripped_details, None)
    rebuilt.__cause__ = exc
    return rebuilt


_CORE_REBUILD_EXCLUDED_KEYS: frozenset[str] = frozenset({
    'category',
    'message',
    'details',
    'transport_phase',
    '_transport_phase',
    'fallback_eligible',
    'route_withdrawal',
    '_semantic_snapshot',
    'semantic',
    '_cc_canonical_state',
})


def _rebuild_standard_core_error(
    core: CoreError,
    stripped_details: dict[str, str],
    cleaned_snapshot: tuple[int, str, tuple[tuple[str, str], ...], TransportPhase | None] | None,
) -> CoreError:
    cls = type(core)
    raw_cat = _raw_get_field(core, 'category', 'transport')
    rebuilt = CoreError(
        str(raw_cat) if issubclass(type(raw_cat), str) else 'transport',  # type: ignore[arg-type]
        _safe_exception_message(core),
    )
    inst_dict = _raw_instance_dict(rebuilt)
    seen_slots: set[str] = set()
    for base in cls.__mro__:
        if base is object:
            break
        for name, member in vars(base).items():
            if (
                name in seen_slots
                or name in _SKIPPED_EXCEPTION_DESCRIPTORS
                or name in _CORE_REBUILD_EXCLUDED_KEYS
            ):
                continue
            if type(member) is MemberDescriptorType:
                seen_slots.add(name)
                try:
                    inst_dict[name] = member.__get__(core, cls)
                except (AttributeError, ValueError):
                    continue
    for key, val in dict.items(_raw_instance_dict(core)):
        if key not in _CORE_REBUILD_EXCLUDED_KEYS and key not in seen_slots:
            if key == '__notes__' and type(val) is list:
                inst_dict[key] = val.copy()
            else:
                inst_dict[key] = val
    inst_dict['details'] = stripped_details
    inst_dict['_transport_phase'] = None
    inst_dict['fallback_eligible'] = None
    inst_dict['route_withdrawal'] = None
    inst_dict['_semantic_snapshot'] = cleaned_snapshot
    rebuilt.__cause__ = core
    return rebuilt


def _callback_error_copy(exc: CCError) -> CCError:
    """Fresh copy of an error escaping a resource callback.

    Keeps the subclass instance state (attributes and custom slots), code,
    message and business details; clears inner call-control details and phase
    without running user copy hooks. If an exotic custom layout refuses raw
    allocation or overrides control attributes via a descriptor, preserves the
    canonical semantic code, message and details on the standard public
    exception class rather than downgrading to code 3 or leaking ``pre_dispatch``.
    """
    stripped_details = _safe_strip_callback_details(exc)
    if not _has_custom_control_descriptor(type(exc)):
        try:
            copied = _clone_exception_without_user_hooks(exc)
            _raw_set_field(copied, 'details', stripped_details)
            copied_dict = _raw_instance_dict(copied)
            state = copied_dict.get('_cc_canonical_state')
            if type(state) is tuple and len(state) == 3:
                copied_dict['_cc_canonical_state'] = (state[0], state[1], dict(stripped_details))
            _clear_copied_phase_fields(copied)
            return copied  # type: ignore[return-value]
        except BaseException:
            pass
    return _rebuild_standard_cc_error(exc, stripped_details)


def _callback_core_copy(core: CoreError) -> CoreError:
    """Fresh ``CoreError`` copy for a same-process resource callback re-raise.

    Keeps the structured native facts, drops the outer-inapplicable inner
    dispatch phase, fallback eligibility and inner phase details without
    running user copy hooks.
    """
    stripped_details = _safe_strip_callback_details(core)
    snapshot = _raw_get_field(core, '_semantic_snapshot')
    cleaned_snapshot = None
    if type(snapshot) is tuple and len(snapshot) == 4:
        code, message, entries, _ = snapshot
        cleaned_entries = (
            tuple(
                (key, value)
                for entry in entries
                if type(entry) is tuple and len(entry) == 2
                for key, value in (entry,)
                if key not in _CALLBACK_DETAIL_KEYS
            )
            if type(entries) is tuple
            else ()
        )
        cleaned_snapshot = (code, message, cleaned_entries, None)
    if not _has_custom_control_descriptor(type(core), check_core_flags=True):
        try:
            copied = _clone_exception_without_user_hooks(core)
            _raw_set_field(copied, 'details', stripped_details)
            _clear_copied_phase_fields(copied)
            _raw_set_field(copied, 'fallback_eligible', None)
            _raw_set_field(copied, 'route_withdrawal', None)
            _raw_set_field(copied, '_semantic_snapshot', cleaned_snapshot)
            return copied  # type: ignore[return-value]
        except BaseException:
            pass
    return _rebuild_standard_core_error(core, stripped_details, cleaned_snapshot)


def _post_dispatch_cleanup_error(exc: BaseException) -> BaseException:
    """Sanitize a cleanup failure raised after call/stream dispatch succeeded."""
    cls = type(exc)
    if issubclass(cls, CoreError):
        raw_details, _ = _extract_canonical_details(exc)
        if (
            _raw_get_field(exc, '_transport_phase') is not None
            or _has_instance_phase_value(cls, exc)
            or _has_custom_control_descriptor(cls, check_core_flags=True)
            or _raw_get_field(exc, 'fallback_eligible') is not None
            or _raw_get_field(exc, 'route_withdrawal') is not None
            or _semantic_snapshot_needs_cleanup(exc)
            or _details_need_cleanup(raw_details)
        ):
            return _callback_core_copy(exc)  # type: ignore[arg-type]
        return exc
    if issubclass(cls, CCError):
        raw_details, _ = _extract_canonical_details(exc)
        if (
            _raw_get_field(exc, '_transport_phase') is not None
            or _has_instance_phase_value(cls, exc)
            or _has_custom_control_descriptor(cls)
            or _details_need_cleanup(raw_details)
        ):
            return _callback_error_copy(exc)  # type: ignore[arg-type]
        return exc
    if issubclass(cls, CCBaseError):
        raw_details, has_details = _extract_canonical_details(exc)
        has_custom_desc = _has_custom_control_descriptor(cls)
        if (
            _raw_get_field(exc, '_transport_phase') is not None
            or _has_instance_phase_value(cls, exc)
            or has_custom_desc
            or _details_need_cleanup(raw_details)
        ):
            stripped_details = _safe_strip_callback_details(exc)  # type: ignore[arg-type]
            if not has_custom_desc:
                try:
                    copied = _clone_exception_without_user_hooks(exc)  # type: ignore[arg-type]
                    if _details_need_cleanup(raw_details):
                        _raw_set_field(copied, 'details', stripped_details)
                    _clear_copied_phase_fields(copied)
                    return copied
                except BaseException:
                    pass
            fallback = CCBaseError(_safe_exception_message(exc))
            if has_details:
                _raw_instance_dict(fallback)['details'] = stripped_details
            fallback.__cause__ = exc
            return fallback
    return exc


def _callback_semantic_copy(exc: CCBaseError) -> CCError | None:
    """Semantic projection of a callback error, or ``None``.

    A ``CoreError`` projects its semantic snapshot first; a plain ``CCError``
    is copied with inner dispatch phases cleared.
    """
    cls = type(exc)
    if issubclass(cls, CoreError):
        projected = _project_core_semantic_snapshot(exc)  # type: ignore[arg-type]
        if projected is not None:
            return _callback_error_copy(projected)
        return None
    if issubclass(cls, CCError):
        return _callback_error_copy(exc)  # type: ignore[arg-type]
    return None


def _project_native_error(exc: BaseException) -> CCError | None:
    """The unified native semantic projection shared by every SDK call site.

    A native ``CoreError`` carrying a canonical semantic error projects a
    fresh typed exception (with the Core-established dispatch phase); a
    ``CoreError`` without one, and any other exception, projects ``None`` so
    callers re-raise the original.
    """
    if issubclass(type(exc), CoreError):
        return _project_core_semantic_snapshot(exc)  # type: ignore[arg-type]
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
]
