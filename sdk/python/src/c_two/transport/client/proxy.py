"""Unified client-compatible proxy for CRM consumers.

Implements the same interface as :class:`rpc.Client` — assign to
``crm.client`` and the existing ``auto_transfer`` machinery works
transparently.

Three modes:

- **thread-local**: ``supports_direct_call = True``.  Calls resource methods
  directly via ``call_direct(method_name, args)`` — zero serialization.
- **ipc**: ``supports_direct_call = False``.  Delegates to a route-bound Rust
  IPC client.
- **http**: ``supports_direct_call = False``.  Delegates to a route-bound
  relay-aware Rust HTTP call surface.

Usage::

    # Thread-local (same process, skip serialization):
    proxy = CRMProxy.thread_local(resource_instance)
    crm = Hello()
    crm.client = proxy
    crm.greeting('World')       # → resource_instance.greeting('World')

    # IPC (cross-process via a route-bound RustClient):
    proxy = CRMProxy.ipc(rust_client, 'hello')
    crm = Hello()
    crm.client = proxy
    crm.greeting('World')

    # HTTP (cross-node via a route-bound relay-aware client):
    proxy = CRMProxy.http(http_client, 'hello')
    crm = Hello()
    crm.client = proxy
    crm.greeting('World')
"""
from __future__ import annotations

import threading
from typing import Any, Callable, TYPE_CHECKING

if TYPE_CHECKING:
    from ..server.scheduler import Scheduler


def _require_explicit_route_name(name: str, transport: str) -> str:
    if not isinstance(name, str):
        raise TypeError(f'{transport} CRMProxy requires route name as a string')
    if not name:
        raise ValueError(
            f'{transport} CRMProxy requires an explicit non-empty route name',
        )
    return name


class CRMProxy:
    """Client-compatible proxy for CRM consumers.

    Do **not** instantiate directly — use the factory classmethods
    :meth:`thread_local` and :meth:`ipc`.

    Parameters
    ----------
    on_terminate:
        Optional callback invoked on :meth:`terminate` — for pool ref-count
        release or cleanup.
    """

    __slots__ = (
        '_mode', '_crm', '_client', '_name',
        '_closed', '_close_lock', '_on_terminate',
        '_scheduler', '_lease_tracker', '_call_options',
    )

    # ------------------------------------------------------------------
    # Factory classmethods
    # ------------------------------------------------------------------

    @classmethod
    def thread_local(
        cls,
        crm_instance: object,
        *,
        name: str | None = None,
        scheduler: Scheduler | None = None,
        on_terminate: Callable[[], None] | None = None,
        lease_tracker: Any = None,
    ) -> CRMProxy:
        """Create a thread-local proxy (same process, no serialization).

        ``supports_direct_call`` is ``True`` — the ``auto_transfer``
        wrapper will call :meth:`call_direct` instead of :meth:`call`.

        Parameters
        ----------
        scheduler:
            Optional :class:`Scheduler` for read/write concurrency control.
            When provided, ``call_direct`` wraps method execution in
            the scheduler's execution guard.
        """
        proxy = object.__new__(cls)
        proxy._mode = 'thread'
        proxy._crm = crm_instance
        proxy._client = None
        proxy._name = (
            _require_explicit_route_name(name, 'thread-local')
            if name is not None
            else ''
        )
        proxy._close_lock = threading.Lock()
        proxy._closed = False
        proxy._on_terminate = on_terminate
        proxy._scheduler = scheduler
        proxy._lease_tracker = lease_tracker
        proxy._call_options = None
        return proxy

    @classmethod
    def ipc(
        cls,
        client: Any,
        name: str,
        *,
        on_terminate: Callable[[], None] | None = None,
        lease_tracker: Any = None,
    ) -> CRMProxy:
        """Create an IPC proxy (cross-process via Rust IPC client).

        Calls are routed to the CRM registered under *name* on the
        remote server. The native client must already be route-bound by
        the runtime session.
        """
        route_name = _require_explicit_route_name(name, 'IPC')
        proxy = object.__new__(cls)
        proxy._mode = 'ipc'
        proxy._crm = None
        proxy._client = client
        proxy._name = route_name
        proxy._close_lock = threading.Lock()
        proxy._closed = False
        proxy._on_terminate = on_terminate
        proxy._scheduler = None
        proxy._lease_tracker = lease_tracker
        proxy._call_options = None
        return proxy

    @classmethod
    def http(
        cls,
        http_client: Any,
        name: str,
        *,
        on_terminate: Callable[[], None] | None = None,
        lease_tracker: Any = None,
    ) -> CRMProxy:
        """Create an HTTP proxy (cross-node via Rust HTTP client + relay).

        Calls are sent through the route-bound relay-aware native client.
        """
        route_name = _require_explicit_route_name(name, 'HTTP')
        proxy = object.__new__(cls)
        proxy._mode = 'http'
        proxy._crm = None
        proxy._client = http_client
        proxy._name = route_name
        proxy._close_lock = threading.Lock()
        proxy._closed = False
        proxy._on_terminate = on_terminate
        proxy._scheduler = None
        proxy._lease_tracker = lease_tracker
        proxy._call_options = None
        return proxy

    # ------------------------------------------------------------------
    # Client interface (compatible with rpc.Client)
    # ------------------------------------------------------------------

    def __getattr__(self, name: str) -> Any:
        with self._close_lock:
            if self._closed:
                raise RuntimeError('Proxy is closed')
        raise AttributeError(
            f'{type(self).__name__!r} object has no attribute {name!r}',
        )

    @property
    def supports_direct_call(self) -> bool:
        """``True`` for thread-local proxies (skip serialization)."""
        return self._mode == 'thread'

    @property
    def route_name(self) -> str:
        """Routing name associated with this proxy."""
        return self._name

    @property
    def observed_path(self) -> str:
        """Core-observed transport path for this remote proxy."""
        if self._mode == 'thread':
            return 'thread_local'
        return self._client.observed_path

    @property
    def route_uid(self) -> str:
        """Immutable route identity observed when this remote proxy connected."""
        if self._mode == 'thread':
            raise RuntimeError('thread-local proxy has no remote route token')
        return self._client.route_uid

    @property
    def route_revision(self) -> int:
        """Route revision observed when this remote proxy connected."""
        if self._mode == 'thread':
            raise RuntimeError('thread-local proxy has no remote route token')
        return self._client.route_revision

    @property
    def lease_tracker(self) -> Any:
        """Native retained-buffer lease tracker for this process session."""
        return self._lease_tracker

    def with_call_options(self, *, timeout=...) -> CRMProxy:
        """Create an independent connection view with Core-owned call options."""
        with self._close_lock:
            if self._closed:
                raise RuntimeError('Proxy is closed')
        from ..._native import NativeCallOptions

        view = object.__new__(type(self))
        for name in self.__slots__:
            setattr(view, name, getattr(self, name))
        view._close_lock = threading.Lock()
        view._closed = False
        if self._mode == 'thread':
            view._call_options = NativeCallOptions(timeout=timeout)
            view._on_terminate = None
        else:
            view._client = _native_call(self._client.with_call_options, timeout=timeout)
            view._on_terminate = view._client.close
        return view

    def begin_call(self, method_name: str):
        """Reserve the Core operation before Python input serialization."""
        with self._close_lock:
            if self._closed:
                raise RuntimeError('Proxy is closed')
        return _native_call(self._client.begin_call, method_name)

    def prepare_call(self, method_name: str):
        """Project one reserved native call into the transfer interface."""
        return _PreparedClient(self.begin_call(method_name))

    def call(self, method_name: str, data: bytes | bytearray | memoryview | None = None) -> bytes:
        """Send a serialized CRM call (IPC or HTTP mode).

        Raises :class:`NotImplementedError` in thread-local mode — use
        :meth:`call_direct` instead.
        """
        with self._close_lock:
            if self._closed:
                raise RuntimeError('Proxy is closed')
        if self._mode in ('ipc', 'http'):
            return _native_call(self._client.call, method_name, data if data is not None else b'')
        raise NotImplementedError(
            'call() not available in thread-local mode; use call_direct()',
        )

    def call_prepared(self, method_name: str, plan: object) -> bytes:
        """Send a prepared payload plan when the underlying transport supports it."""
        with self._close_lock:
            if self._closed:
                raise RuntimeError('Proxy is closed')
        if self._mode in ('ipc', 'http'):
            call_prepared = getattr(self._client, 'call_prepared', None)
            if callable(call_prepared):
                return _native_call(call_prepared, method_name, plan)
        if self._mode in ('ipc', 'http'):
            to_bytes = getattr(plan, 'to_bytes', None)
            if not callable(to_bytes):
                raise TypeError('prepared payload plan must define to_bytes() for fallback calls')
            return self.call(method_name, to_bytes())
        raise NotImplementedError(
            'call_prepared() not available in thread-local mode; use call_direct()',
        )

    def call_direct(self, method_name: str, args: tuple) -> Any:
        """Call CRM method directly with Python objects (thread-local mode).

        When a :class:`Scheduler` is attached, the call is wrapped in
        the scheduler's execution guard to enforce read/write isolation.

        Raises :class:`NotImplementedError` in IPC mode.
        """
        with self._close_lock:
            if self._closed:
                raise RuntimeError('Proxy is closed')
        if self._mode != 'thread':
            raise NotImplementedError(
                'call_direct() only available in thread-local mode',
            )
        from ..._native import NativeCallOptions
        _native_call((self._call_options or NativeCallOptions()).validate_thread)
        method = getattr(self._crm, method_name, None)
        if method is None:
            raise AttributeError(
                f'{type(self._crm).__name__} has no method {method_name!r}',
            )
        if self._scheduler is None:
            return method(*args)
        method_idx = self._scheduler.method_idx(method_name)
        try:
            guard = self._scheduler.execution_guard(method_idx)
        except RuntimeError as exc:
            error_bytes = getattr(exc, 'error_bytes', None)
            if error_bytes is not None:
                from ...error import CCError, ResourceClosed
                cc_err = CCError.deserialize(memoryview(error_bytes))
                if isinstance(cc_err, ResourceClosed) and self._name:
                    raise ResourceClosed(
                        f"route {self._name!r} is closed",
                        details={'route': self._name},
                    ) from exc
                if cc_err is not None:
                    raise cc_err from exc
            if str(exc) == 'route closed':
                from ...error import ResourceClosed
                details = {'route': self._name} if self._name else {}
                message = (
                    f"route {self._name!r} is closed"
                    if self._name
                    else 'route is closed'
                )
                raise ResourceClosed(message, details=details) from exc
            raise
        with guard:
            return method(*args)

    def terminate(self) -> None:
        """Release the proxy and invoke cleanup callback if set."""
        with self._close_lock:
            if self._closed:
                return
            self._closed = True
        if self._on_terminate is not None:
            self._on_terminate()


def _native_call(operation, *args, **kwargs):
    """Keep canonical Core errors and their transport-phase observation."""
    try:
        return operation(*args, **kwargs)
    except Exception as exc:
        error_bytes = getattr(exc, 'error_bytes', None)
        if error_bytes is not None:
            from ...error import CCError
            cc_error = CCError.deserialize(memoryview(error_bytes))
            if cc_error is not None:
                phase = getattr(exc, 'transport_phase', None)
                if phase is not None:
                    cc_error.details['transport_phase'] = phase
                raise cc_error from exc
        raise


class _PreparedClient:
    """One caller-thread serialization followed by the reserved Core call."""

    __slots__ = ('_prepared',)

    def __init__(self, prepared):
        self._prepared = prepared

    def call(self, method_name, data=None):
        return _native_call(self._prepared.call, data if data is not None else b'')

    def charge_input(self, nbytes):
        return _native_call(self._prepared.charge_input, nbytes)

    def close(self):
        self._prepared.close()


def with_call_options(proxy, *, timeout=...):
    """Return a CRM connection view with a timeout in seconds.

    Omission inherits the current path policy; ``None`` selects unlimited.
    Business method keyword arguments, including ``timeout``, remain unchanged.
    """
    client = getattr(proxy, 'client', None)
    if not isinstance(client, CRMProxy):
        raise TypeError('cc.with_call_options() requires a connected CRM instance')
    view = object.__new__(type(proxy))
    view.__dict__.update(proxy.__dict__)
    view.client = client.with_call_options(timeout=timeout)
    return view
