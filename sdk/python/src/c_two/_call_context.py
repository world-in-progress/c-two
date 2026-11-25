"""Host admission and resource call context for the Python SDK."""
from __future__ import annotations

from collections.abc import Mapping
from contextvars import ContextVar
from dataclasses import dataclass
from types import MappingProxyType
from typing import Any, Literal


class _Unset:
    pass


_UNSET = _Unset()


@dataclass(frozen=True, slots=True)
class AdmissionContext:
    """The Host's read-only view of an incoming operation."""

    route_name: str
    crm_ns: str
    crm_name: str
    crm_ver: str
    target: str | None
    access: Literal['read', 'write'] | None
    operation: Literal['unary', 'server_stream', 'subscribe', 'recover', 'query_outcome']
    transport: Literal['thread_local', 'ipc']
    call_id: str | None
    metadata: MappingProxyType[str, str]

    def __post_init__(self):
        object.__setattr__(self, 'metadata', MappingProxyType(dict(self.metadata)))


@dataclass(frozen=True, slots=True)
class AdmissionDecision:
    """Allow an operation or deny it before resource dispatch."""

    _allowed: bool
    _extension: Any = None
    _message: str = 'admission denied'
    _details: Mapping[str, str] | None = None

    @staticmethod
    def allow(*, extension: Any = None) -> AdmissionDecision:
        return AdmissionDecision(True, _extension=extension)

    @staticmethod
    def deny(
        message: str = 'admission denied', *, details: Mapping[str, str] | None = None,
    ) -> AdmissionDecision:
        if not isinstance(message, str):
            raise TypeError('admission denial message must be a str')
        if details is None:
            return AdmissionDecision(False, _message=message)
        if not isinstance(details, Mapping):
            raise TypeError('admission denial details must be a mapping')
        snapshot = dict(details)
        if any(not isinstance(key, str) or not isinstance(value, str)
               for key, value in snapshot.items()):
            raise TypeError('admission denial details must have str keys and values')
        return AdmissionDecision(False, _message=message, _details=MappingProxyType(snapshot))


@dataclass(frozen=True, slots=True)
class CallContext:
    """The context of the resource invocation currently executing."""

    route_name: str
    target: str
    operation: Literal['unary', 'server_stream']
    call_id: str | None
    metadata: MappingProxyType[str, str]
    extension: Any = None

    def __post_init__(self):
        object.__setattr__(self, 'metadata', MappingProxyType(dict(self.metadata)))


_current: ContextVar[CallContext | None] = ContextVar('c_two_call_context', default=None)


def current_call_context() -> CallContext | None:
    """Return the current resource call context, if any."""
    return _current.get()


def _run_in_call_context(context, callback, args):
    token = _current.set(context)
    del context
    try:
        return callback(*args)
    finally:
        _current.reset(token)
