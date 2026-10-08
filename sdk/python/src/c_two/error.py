from __future__ import annotations
from collections.abc import Mapping
from enum import IntEnum, unique

from c_two import _native

_NATIVE_TO_PY_ERROR_NAMES = {
    "Unknown": "ERROR_UNKNOWN",
    "ResourceInputDeserializing": "ERROR_AT_RESOURCE_INPUT_DESERIALIZING",
    "ResourceOutputSerializing": "ERROR_AT_RESOURCE_OUTPUT_SERIALIZING",
    "ResourceFunctionExecuting": "ERROR_AT_RESOURCE_FUNCTION_EXECUTING",
    "ResourceInputFromBuffer": "ERROR_AT_RESOURCE_INPUT_FROM_BUFFER",
    "ClientInputSerializing": "ERROR_AT_CLIENT_INPUT_SERIALIZING",
    "ClientOutputDeserializing": "ERROR_AT_CLIENT_OUTPUT_DESERIALIZING",
    "ClientCallingResource": "ERROR_AT_CLIENT_CALLING_RESOURCE",
    "ClientOutputFromBuffer": "ERROR_AT_CLIENT_OUTPUT_FROM_BUFFER",
    "ResourceNotFound": "ERROR_RESOURCE_NOT_FOUND",
    "ResourceUnavailable": "ERROR_RESOURCE_UNAVAILABLE",
    "ResourceAlreadyRegistered": "ERROR_RESOURCE_ALREADY_REGISTERED",
    "RouteStale": "ERROR_ROUTE_STALE",
    "RegistryUnavailable": "ERROR_REGISTRY_UNAVAILABLE",
    "WriteConflict": "ERROR_WRITE_CONFLICT",
    "ResourceClosed": "ERROR_RESOURCE_CLOSED",
    "ResourceRemoved": "ERROR_RESOURCE_REMOVED",
    "ContractMismatch": "ERROR_CONTRACT_MISMATCH",
    "IdentityMismatch": "ERROR_IDENTITY_MISMATCH",
    "RouteCatalogCompacted": "ERROR_ROUTE_CATALOG_COMPACTED",
    "RouteWatchUnavailable": "ERROR_ROUTE_WATCH_UNAVAILABLE",
    "ProtocolViolation": "ERROR_PROTOCOL_VIOLATION",
    "FallbackDenied": "ERROR_FALLBACK_DENIED",
    "CallDeadlineExceeded": "ERROR_CALL_DEADLINE_EXCEEDED",
    "UnsupportedCallMode": "ERROR_UNSUPPORTED_CALL_MODE",
    "CallCapacityExceeded": "ERROR_CALL_CAPACITY_EXCEEDED",
}


def _load_error_code_members() -> dict[str, int]:
    registry = _native.error_registry()
    missing = sorted(set(_NATIVE_TO_PY_ERROR_NAMES) - set(registry))
    extra = sorted(set(registry) - set(_NATIVE_TO_PY_ERROR_NAMES))
    if missing or extra:
        raise RuntimeError(
            "Rust error registry does not match Python facade mapping "
            f"(missing={missing}, extra={extra})"
        )
    return {
        py_name: int(registry[native_name])
        for native_name, py_name in _NATIVE_TO_PY_ERROR_NAMES.items()
    }


ERROR_Code = unique(IntEnum("ERROR_Code", _load_error_code_members()))

class CCBaseError(Exception):
    """Base class for all C-Two-related errors."""

class CCError(CCBaseError):
    """
    General error class for C-Two.
    
    Parameters:
        code (ERROR_Code): The error code representing the type of error.
        message (str | None): Optional custom error message. Defaults to a generic message.
        details (Mapping[str, str] | None): Optional machine-readable diagnostic context.
    """
    
    code: ERROR_Code
    message: str | None
    details: dict[str, str]

    def __init__(
        self,
        code: ERROR_Code = ERROR_Code.ERROR_UNKNOWN,
        message: str | None = None,
        details: Mapping[str, str] | None = None,
    ):
        self.code = code
        self.message = message or 'Error occurred when using C-Two.'
        self.details = dict(details or {})

    def __str__(self):
        return f'{self.code.name}: {self.message}'
    
    def __repr__(self):
        return f'CCError(code={self.code}, message={self.message}, details={self.details})'

    @staticmethod
    def serialize(err: 'CCError' | None) -> bytes:
        """
        Serialize the error to canonical C-Two error wire bytes.
        """
        if err is None:
            return b''

        message = err.message or 'Error occurred when using C-Two.'
        return _native.encode_error_wire(int(err.code), message, err.details)

    @staticmethod
    def deserialize(data) -> 'CCError' | None:
        """
        Deserialize canonical C-Two error wire bytes to a Python error object.
        """
        try:
            decoded = _native.decode_error_wire_parts(data)
        except ValueError as exc:
            return CCError(
                code=ERROR_Code.ERROR_UNKNOWN,
                message=f'Malformed error payload: {exc}',
                details={'decode_error': str(exc)},
            )

        if decoded is None:
            return None

        code_value, message, details = decoded
        try:
            code = ERROR_Code(code_value)
        except ValueError:
            code = ERROR_Code.ERROR_UNKNOWN
        subclass = _CODE_TO_CLASS.get(code, CCError)
        obj = Exception.__new__(subclass)
        obj.code = code
        obj.message = message or 'Error occurred when using C-Two.'
        obj.details = dict(details or {})
        return obj

class ResourceDeserializeInput(CCError):
    def __init__(self, message: str | None = None, details: Mapping[str, str] | None = None):
        message = 'Error occurred when deserializing input at resource' + (f':\n{message}' if message else '')
        super().__init__(code=ERROR_Code.ERROR_AT_RESOURCE_INPUT_DESERIALIZING, message=message, details=details)

class ResourceInputFromBuffer(CCError):
    def __init__(self, message: str | None = None, details: Mapping[str, str] | None = None):
        message = 'Error occurred when constructing resource input from buffer' + (f':\n{message}' if message else '')
        super().__init__(code=ERROR_Code.ERROR_AT_RESOURCE_INPUT_FROM_BUFFER, message=message, details=details)

class ResourceSerializeOutput(CCError):
    def __init__(self, message: str | None = None, details: Mapping[str, str] | None = None):
        message = 'Error occurred when serializing output at resource' + (f':\n{message}' if message else '')
        super().__init__(code=ERROR_Code.ERROR_AT_RESOURCE_OUTPUT_SERIALIZING, message=message, details=details)

class ResourceExecuteFunction(CCError):
    def __init__(self, message: str | None = None, details: Mapping[str, str] | None = None):
        message = 'Error occurred when executing function at resource' + (f':\n{message}' if message else '')
        super().__init__(code=ERROR_Code.ERROR_AT_RESOURCE_FUNCTION_EXECUTING, message=message, details=details)

class ClientSerializeInput(CCError):
    def __init__(self, message: str | None = None, details: Mapping[str, str] | None = None):
        message = 'Error occurred when serializing input at client' + (f':\n{message}' if message else '')
        super().__init__(code=ERROR_Code.ERROR_AT_CLIENT_INPUT_SERIALIZING, message=message, details=details)

class ClientDeserializeOutput(CCError):
    def __init__(self, message: str | None = None, details: Mapping[str, str] | None = None):
        message = 'Error occurred when deserializing output at client' + (f':\n{message}' if message else '')
        super().__init__(code=ERROR_Code.ERROR_AT_CLIENT_OUTPUT_DESERIALIZING, message=message, details=details)

class ClientOutputFromBuffer(CCError):
    def __init__(self, message: str | None = None, details: Mapping[str, str] | None = None):
        message = 'Error occurred when constructing client output from buffer' + (f':\n{message}' if message else '')
        super().__init__(code=ERROR_Code.ERROR_AT_CLIENT_OUTPUT_FROM_BUFFER, message=message, details=details)

class ClientCallResource(CCError):
    def __init__(self, message: str | None = None, details: Mapping[str, str] | None = None):
        message = 'Error occurred when calling resource from client' + (f':\n{message}' if message else '')
        super().__init__(code=ERROR_Code.ERROR_AT_CLIENT_CALLING_RESOURCE, message=message, details=details)

class ResourceNotFound(CCError):
    """Raised when a named resource cannot be resolved by any relay."""
    ERROR_CODE = 701

    def __init__(self, message: str | None = None, details: Mapping[str, str] | None = None):
        super().__init__(code=ERROR_Code.ERROR_RESOURCE_NOT_FOUND, message=message or 'Resource not found', details=details)

class ResourceUnavailable(CCError):
    """Raised when a resource exists but is not reachable."""
    ERROR_CODE = 702

    def __init__(
        self,
        message: str | None = None,
        detail: str | None = None,
        details: Mapping[str, str] | None = None,
    ):
        msg = message or 'Resource unavailable'
        if detail:
            msg = f'{msg}: {detail}'
        super().__init__(code=ERROR_Code.ERROR_RESOURCE_UNAVAILABLE, message=msg, details=details)

class ResourceAlreadyRegistered(CCError):
    """Raised when a relay rejects duplicate registration for a resource name."""
    ERROR_CODE = 703

    def __init__(self, message: str | None = None, details: Mapping[str, str] | None = None):
        super().__init__(
            code=ERROR_Code.ERROR_RESOURCE_ALREADY_REGISTERED,
            message=message or 'Resource already registered',
            details=details,
        )

class RouteStale(CCError):
    """Raised when an observed route token is older than current route state."""
    ERROR_CODE = 704

    def __init__(self, message: str | None = None, details: Mapping[str, str] | None = None):
        super().__init__(code=ERROR_Code.ERROR_ROUTE_STALE, message=message or 'Route stale', details=details)

class RegistryUnavailable(CCError):
    """Raised when no relay is available for name resolution."""
    ERROR_CODE = 705

    def __init__(self, message: str | None = None, details: Mapping[str, str] | None = None):
        super().__init__(code=ERROR_Code.ERROR_REGISTRY_UNAVAILABLE, message=message or 'Registry unavailable', details=details)

class WriteConflict(CCError):
    """Raised when a write cannot acquire the required resource coordination."""
    ERROR_CODE = 706

    def __init__(self, message: str | None = None, details: Mapping[str, str] | None = None):
        super().__init__(code=ERROR_Code.ERROR_WRITE_CONFLICT, message=message or 'Write conflict', details=details)

class ResourceClosed(CCError):
    """Raised when a route exists but no longer accepts new calls."""
    ERROR_CODE = 707

    def __init__(self, message: str | None = None, details: Mapping[str, str] | None = None):
        super().__init__(code=ERROR_Code.ERROR_RESOURCE_CLOSED, message=message or 'Resource closed', details=details)

class ResourceRemoved(CCError):
    """Raised when a previously observed route was removed."""
    ERROR_CODE = 708

    def __init__(self, message: str | None = None, details: Mapping[str, str] | None = None):
        super().__init__(code=ERROR_Code.ERROR_RESOURCE_REMOVED, message=message or 'Resource removed', details=details)

class ContractMismatch(CCError):
    """Raised when route contract metadata does not match the expected CRM."""
    ERROR_CODE = 709

    def __init__(self, message: str | None = None, details: Mapping[str, str] | None = None):
        super().__init__(code=ERROR_Code.ERROR_CONTRACT_MISMATCH, message=message or 'Contract mismatch', details=details)

class IdentityMismatch(CCError):
    """Raised when the actual server identity does not match the expected owner."""
    ERROR_CODE = 710

    def __init__(self, message: str | None = None, details: Mapping[str, str] | None = None):
        super().__init__(code=ERROR_Code.ERROR_IDENTITY_MISMATCH, message=message or 'Identity mismatch', details=details)

class RouteCatalogCompacted(CCError):
    """Raised when route watch history is no longer incrementally recoverable."""
    ERROR_CODE = 711

    def __init__(self, message: str | None = None, details: Mapping[str, str] | None = None):
        super().__init__(code=ERROR_Code.ERROR_ROUTE_CATALOG_COMPACTED, message=message or 'Route catalog compacted', details=details)

class RouteWatchUnavailable(CCError):
    """Raised when route state cannot be trusted because the watch is unavailable."""
    ERROR_CODE = 712

    def __init__(self, message: str | None = None, details: Mapping[str, str] | None = None):
        super().__init__(code=ERROR_Code.ERROR_ROUTE_WATCH_UNAVAILABLE, message=message or 'Route watch unavailable', details=details)

class ProtocolViolation(CCError):
    """Raised when a peer sends invalid C-Two protocol data."""
    ERROR_CODE = 713

    def __init__(self, message: str | None = None, details: Mapping[str, str] | None = None):
        super().__init__(code=ERROR_Code.ERROR_PROTOCOL_VIOLATION, message=message or 'Protocol violation', details=details)

class FallbackDenied(CCError):
    """Raised when a fallback route would retry the same failed local path."""
    ERROR_CODE = 714

    def __init__(self, message: str | None = None, details: Mapping[str, str] | None = None):
        super().__init__(code=ERROR_Code.ERROR_FALLBACK_DENIED, message=message or 'Fallback denied', details=details)

class CallDeadlineExceeded(CCError):
    """Raised when a bounded call deadline elapsed without a definitive outcome.

    The peer Core reports the observed ``transport_phase`` detail, either
    ``pre_dispatch`` or ``dispatch_uncertain``. This class only projects the
    received detail; it never infers a phase from the message text and never
    rewrites received details.
    """
    ERROR_CODE = 715

    def __init__(self, message: str | None = None, details: Mapping[str, str] | None = None):
        super().__init__(
            code=ERROR_Code.ERROR_CALL_DEADLINE_EXCEEDED,
            message=message or 'Call deadline exceeded',
            details=details,
        )

    @property
    def transport_phase(self) -> str | None:
        """Read-only view of ``details['transport_phase']``; ``None`` when absent."""
        return self.details.get('transport_phase')

class UnsupportedCallMode(CCError):
    """Raised when a call mode cannot honor the requested semantics, such as a
    finite deadline that does not support same-process synchronous dispatch."""
    ERROR_CODE = 716

    def __init__(self, message: str | None = None, details: Mapping[str, str] | None = None):
        super().__init__(
            code=ERROR_Code.ERROR_UNSUPPORTED_CALL_MODE,
            message=message or 'Unsupported call mode',
            details=details,
        )

class CallCapacityExceeded(CCError):
    """Raised when a bounded continuation budget is exhausted, such as
    continuation slots or byte capacity."""
    ERROR_CODE = 717

    def __init__(self, message: str | None = None, details: Mapping[str, str] | None = None):
        super().__init__(
            code=ERROR_Code.ERROR_CALL_CAPACITY_EXCEEDED,
            message=message or 'Call capacity exceeded',
            details=details,
        )

_CODE_TO_CLASS: dict[int, type] = {
    ERROR_Code.ERROR_AT_RESOURCE_INPUT_DESERIALIZING: ResourceDeserializeInput,
    ERROR_Code.ERROR_AT_RESOURCE_INPUT_FROM_BUFFER:    ResourceInputFromBuffer,
    ERROR_Code.ERROR_AT_RESOURCE_OUTPUT_SERIALIZING:  ResourceSerializeOutput,
    ERROR_Code.ERROR_AT_RESOURCE_FUNCTION_EXECUTING:  ResourceExecuteFunction,
    ERROR_Code.ERROR_AT_CLIENT_INPUT_SERIALIZING:     ClientSerializeInput,
    ERROR_Code.ERROR_AT_CLIENT_OUTPUT_DESERIALIZING:  ClientDeserializeOutput,
    ERROR_Code.ERROR_AT_CLIENT_OUTPUT_FROM_BUFFER:    ClientOutputFromBuffer,
    ERROR_Code.ERROR_AT_CLIENT_CALLING_RESOURCE:      ClientCallResource,
    ERROR_Code.ERROR_RESOURCE_NOT_FOUND:               ResourceNotFound,
    ERROR_Code.ERROR_RESOURCE_UNAVAILABLE:             ResourceUnavailable,
    ERROR_Code.ERROR_RESOURCE_ALREADY_REGISTERED:      ResourceAlreadyRegistered,
    ERROR_Code.ERROR_ROUTE_STALE:                      RouteStale,
    ERROR_Code.ERROR_REGISTRY_UNAVAILABLE:             RegistryUnavailable,
    ERROR_Code.ERROR_WRITE_CONFLICT:                   WriteConflict,
    ERROR_Code.ERROR_RESOURCE_CLOSED:                  ResourceClosed,
    ERROR_Code.ERROR_RESOURCE_REMOVED:                 ResourceRemoved,
    ERROR_Code.ERROR_CONTRACT_MISMATCH:                ContractMismatch,
    ERROR_Code.ERROR_IDENTITY_MISMATCH:                IdentityMismatch,
    ERROR_Code.ERROR_ROUTE_CATALOG_COMPACTED:          RouteCatalogCompacted,
    ERROR_Code.ERROR_ROUTE_WATCH_UNAVAILABLE:          RouteWatchUnavailable,
    ERROR_Code.ERROR_PROTOCOL_VIOLATION:               ProtocolViolation,
    ERROR_Code.ERROR_FALLBACK_DENIED:                  FallbackDenied,
    ERROR_Code.ERROR_CALL_DEADLINE_EXCEEDED:           CallDeadlineExceeded,
    ERROR_Code.ERROR_UNSUPPORTED_CALL_MODE:            UnsupportedCallMode,
    ERROR_Code.ERROR_CALL_CAPACITY_EXCEEDED:           CallCapacityExceeded,
}
